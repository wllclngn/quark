#!/usr/bin/env python3
"""Build → install → launch → monitor → analyze cycle for triskelion development.

Usage:
    python3 tests/iterate.py                  # Full cycle with Balatro
    python3 tests/iterate.py --skip-build     # Skip cargo build
    python3 tests/iterate.py --timeout 60     # Longer timeout
    python3 tests/iterate.py --appid 2218750  # Different game
"""

import argparse
import glob
import os
import pathlib
import re
import signal
import shutil
import subprocess
import sys
import threading
import time
from collections import Counter, OrderedDict
from dataclasses import dataclass, field
from pathlib import Path

# ── Paths ─────────────────────────────────────────────────────────────

REPO_ROOT = Path(__file__).resolve().parent.parent
RUST_DIR = REPO_ROOT / "rust"
CARGO_TARGET = REPO_ROOT / "target/release/triskelion"
CARGO_QUARK = REPO_ROOT / "target/release/quark"
CARGO_PARALLAX = REPO_ROOT / "target/release/parallax"
REPO_BINARY = REPO_ROOT / "triskelion"
_USER_HOME = Path(f"/home/{os.environ.get('SUDO_USER', os.environ.get('USER', 'mod'))}")
STEAM_ROOT = _USER_HOME / ".local/share/Steam"
STEAMAPPS = STEAM_ROOT / "steamapps"
COMPAT_DIR = STEAM_ROOT / "compatibilitytools.d/quark"
PROTON_BIN = COMPAT_DIR / "proton"

def clock_raw_ns() -> int:
    """CLOCK_MONOTONIC_RAW in nanoseconds — hardware TSC, no NTP slew."""
    return time.clock_gettime_ns(time.CLOCK_MONOTONIC_RAW)

def ns_to_ms(ns: int) -> float:
    return ns / 1_000_000

# LOGGING
from datetime import datetime

def _timestamp() -> str:
    return datetime.now().strftime("[%H:%M:%S]")

def log_info(msg: str) -> None:
    print(f"{_timestamp()} [INFO]   {msg}", flush=True)

def log_warn(msg: str) -> None:
    print(f"{_timestamp()} [WARN]   {msg}", flush=True)

def log_error(msg: str) -> None:
    print(f"{_timestamp()} [ERROR]  {msg}", flush=True)

DAEMON_LOG = Path("/tmp/quark/daemon.log")
OPCODE_STATS = Path("/tmp/quark/triskelion_opcode_stats.txt")
SHM_GLOB = "/dev/shm/triskelion-*"
WINE_INIT_GLOB = "/tmp/quark/wine_init_*.log"

DEFAULT_APPID = "2379780"  # Balatro
DEFAULT_TIMEOUT = 30

# ── Opcode implementation status ─────────────────────────────────────
# Auto-derived from rust/src/triskelion/event_loop/*.rs at module load time.
# Anything iterate.py sees that's NOT in this set is a real coverage gap.
#
# The previous version was a hand-maintained set that drifted out of sync
# with the actual code (handlers were added but the set wasn't updated),
# making the gap analysis lie. Reading the source directly kills the drift
# permanently — no human ever needs to remember to update this list.

def _scan_implemented_opcodes() -> set:
    """Find every `pub(crate) fn handle_<opcode>` in the event_loop modules.

    The convention in triskelion: every real wineserver opcode handler is
    a function named `handle_<opcode>` in some event_loop/*.rs file.
    build.rs uses the same scan to wire up dispatch.
    """
    import re
    impl = set()
    el_dir = REPO_ROOT / "rust" / "src" / "triskelion" / "event_loop"
    if not el_dir.exists():
        return impl
    pat = re.compile(r"pub\s*\(\s*crate\s*\)\s+fn\s+handle_([a-z_][a-z0-9_]*)\s*\(")
    for rs in el_dir.glob("*.rs"):
        try:
            for m in pat.finditer(rs.read_text(errors="replace")):
                impl.add(m.group(1))
        except OSError:
            pass
    return impl


IMPLEMENTED_OPCODES = _scan_implemented_opcodes()

# x86_64 syscall numbers → names
SYSCALL_NAMES = {
    0: "read", 1: "write", 3: "close", 7: "poll", 8: "lseek",
    16: "ioctl", 23: "select", 35: "nanosleep", 44: "sendto",
    45: "recvfrom", 46: "sendmsg", 47: "recvmsg", 56: "clone",
    59: "execve", 61: "wait4", 202: "futex", 228: "clock_gettime",
    230: "clock_nanosleep", 232: "epoll_wait", 270: "ppoll",
    281: "epoll_pwait", 435: "epoll_pwait2",
}

EXE_BLACKLIST = {
    "unitycrashhandler64.exe", "unitycrashhandler32.exe",
    "crashreport.exe", "crashhandler.exe", "crashpad_handler.exe",
    "ue4prereqsetup_x64.exe", "installermessage.exe",
    "dxsetup.exe", "vcredist_x64.exe", "vcredist_x86.exe",
    "dukeworkshopuploader.exe",
    "dotnetfx35.exe", "dotnetfx35setup.exe",
    "beservice.exe", "beservice_x64.exe",
    "easyanticheat_setup.exe", "easyanticheat.exe",
}

# ── Log parsing regexes ───────────────────────────────────────────────

RE_REQUEST = re.compile(r'\[trace\] #(\d+) (\w+) fd=(\d+)')
RE_ERROR = re.compile(r'\[triskelion\] !! (\w+) err=(0x[0-9a-f]+)')
RE_DISCONNECT = re.compile(r'\[triskelion\] client disconnected: fd=(\d+) pid=(\d+) tid=(\d+)')
RE_PANIC = re.compile(r"panicked at .+: (.+)")
RE_ZERO_READ = re.compile(r'n=0 inflight=0')

# ── Dynamic opcode tracing (ported from triskelion-tests.py + Cortex patterns) ──

def parse_triskelion_stderr(stderr_text: str) -> tuple:
    """Parse triskelion stderr for handler hits, NOT_IMPLEMENTED opcodes, crashes.
    Returns (handlers_hit: set, not_implemented: set, crashes: list)."""
    handlers_hit = set()
    not_implemented = set()
    crashes = []

    for line in stderr_text.splitlines():
        if "[triskelion]" not in line:
            continue
        # Handler hit: "[triskelion] init_first_thread: pid=1 ..."
        m = re.match(r".*\[triskelion\]\s+(\w+):", line)
        if m:
            handlers_hit.add(m.group(1))
        # STATUS_NOT_IMPLEMENTED shows up as error 0xC00000BB
        if "0xC00000BB" in line or "NOT_IMPLEMENTED" in line:
            m2 = re.search(r"opcode (\w+)", line)
            if m2:
                not_implemented.add(m2.group(1))
        # Panics / crashes
        if "panic" in line.lower() or "SIGSEGV" in line or "fatal" in line.lower():
            crashes.append(line.strip())

    return handlers_hit, not_implemented, crashes


class OpcodeTracer:
    """Real-time opcode stream tracker. Tails daemon.log, categorizes opcodes
    as they appear, tracks timing of first appearance, and identifies
    stubbed opcodes the game needs."""

    def __init__(self):
        self.seen_opcodes: OrderedDict = OrderedDict()  # opcode -> first_seen_time
        self.opcode_counts: Counter = Counter()
        self.new_since_last: list = []  # opcodes discovered since last report
        self.log_offset: int = 0  # file position for incremental reads
        self.start_time: float = time.monotonic()
        self.total_requests: int = 0

    def scan(self):
        """Incrementally read daemon.log from last position, extract new opcodes."""
        if not DAEMON_LOG.exists():
            return

        try:
            with open(DAEMON_LOG, "r", errors="replace") as f:
                f.seek(self.log_offset)
                new_data = f.read()
                self.log_offset = f.tell()
        except OSError:
            return

        if not new_data:
            return

        for line in new_data.splitlines():
            m = RE_REQUEST.search(line)
            if m:
                opcode = m.group(2)
                self.total_requests += 1
                self.opcode_counts[opcode] += 1
                if opcode not in self.seen_opcodes:
                    elapsed = time.monotonic() - self.start_time
                    self.seen_opcodes[opcode] = elapsed
                    self.new_since_last.append(opcode)

    def drain_new(self) -> list:
        """Return and clear list of newly discovered opcodes since last drain."""
        new = self.new_since_last[:]
        self.new_since_last.clear()
        return new

    def format_live_status(self) -> str:
        """One-line status: total requests, unique opcodes, impl/stub split."""
        impl = sum(1 for op in self.seen_opcodes if op in IMPLEMENTED_OPCODES)
        stub = sum(1 for op in self.seen_opcodes if op not in IMPLEMENTED_OPCODES)
        return (f"requests={self.total_requests} opcodes={len(self.seen_opcodes)} "
                f"(impl={impl} stub={stub})")

    def format_discovery_report(self, new_opcodes: list) -> str:
        """Format newly discovered opcodes with impl/stub classification."""
        if not new_opcodes:
            return ""
        lines = []
        for op in new_opcodes:
            t = self.seen_opcodes[op]
            status = "IMPL" if op in IMPLEMENTED_OPCODES else "STUB"
            lines.append(f"    +{t:.1f}s [{status}] {op}")
        return "\n".join(lines)

    def format_final_report(self) -> str:
        """Full opcode trace report: timeline, coverage, priority list."""
        if not self.seen_opcodes:
            return "  No opcodes traced."

        lines = []
        impl_count = sum(1 for op in self.seen_opcodes if op in IMPLEMENTED_OPCODES)
        stub_count = sum(1 for op in self.seen_opcodes if op not in IMPLEMENTED_OPCODES)
        total_unique = impl_count + stub_count
        coverage = impl_count / total_unique * 100 if total_unique else 0

        impl_req = sum(self.opcode_counts[op] for op in self.seen_opcodes if op in IMPLEMENTED_OPCODES)
        stub_req = sum(self.opcode_counts[op] for op in self.seen_opcodes if op not in IMPLEMENTED_OPCODES)
        req_coverage = impl_req / self.total_requests * 100 if self.total_requests else 0

        lines.append(f"  Coverage: {impl_count}/{total_unique} opcodes ({coverage:.0f}%), "
                     f"{impl_req}/{self.total_requests} requests ({req_coverage:.1f}%)")

        # Timeline of opcode discovery
        lines.append(f"\n  Discovery timeline ({total_unique} opcodes):")
        for op, t in self.seen_opcodes.items():
            status = "IMPL" if op in IMPLEMENTED_OPCODES else "STUB"
            count = self.opcode_counts[op]
            lines.append(f"    +{t:>6.1f}s  {count:>6}x  [{status}]  {op}")

        # Stubbed opcodes the game needs — priority list
        stubbed = [(op, self.opcode_counts[op]) for op in self.seen_opcodes
                   if op not in IMPLEMENTED_OPCODES]
        if stubbed:
            stubbed.sort(key=lambda x: -x[1])
            lines.append(f"\n  STUBBED — implement next ({len(stubbed)} opcodes, {stub_req} requests):")
            for op, count in stubbed:
                pct = count / self.total_requests * 100 if self.total_requests else 0
                lines.append(f"    {count:>8}  {pct:>5.1f}%  {op}")

        return "\n".join(lines)


def check_daemon_health() -> tuple:
    """Check if the triskelion daemon is still alive. Returns (alive, pid_or_none)."""
    try:
        result = subprocess.run(["pgrep", "-f", "triskelion.*server"],
                                capture_output=True, text=True, timeout=3)
        if result.returncode == 0 and result.stdout.strip():
            pid = int(result.stdout.strip().splitlines()[0])
            return True, pid
    except (subprocess.TimeoutExpired, ValueError):
        pass

    # Also check /tmp/quark for socket liveness
    for sock_dir in Path("/tmp").glob(".wine-*/server-*"):
        sock = sock_dir / "socket"
        if sock.exists():
            return True, None

    return False, None


# ── Data types ────────────────────────────────────────────────────────

@dataclass
class ProcSnapshot:
    pid: int
    comm: str = ""
    state: str = ""
    wchan: str = ""
    syscall_nr: int = -1
    syscall_name: str = "?"
    syscall_args: str = ""
    fd_count: int = 0
    key_fds: list = field(default_factory=list)
    cmdline: str = ""

@dataclass
class DaemonReport:
    total_requests: int = 0
    opcode_counts: dict = field(default_factory=dict)
    lifecycle: dict = field(default_factory=dict)
    errors: list = field(default_factory=list)
    error_summary: dict = field(default_factory=dict)
    disconnects: list = field(default_factory=list)
    panics: list = field(default_factory=list)
    last_requests: list = field(default_factory=list)
    last_request_per_fd: dict = field(default_factory=dict)

# ── Game discovery (from test_launch.py) ──────────────────────────────

def parse_appmanifest(path: Path) -> dict:
    text = path.read_text(errors="replace")
    result = {}
    for key in ("appid", "name", "installdir"):
        m = re.search(rf'"{key}"\s+"([^"]+)"', text)
        if m:
            result[key] = m.group(1)
    return result


def find_game_exe(install_dir: Path) -> str:
    if not install_dir.exists():
        return ""
    candidates = []
    for exe in install_dir.rglob("*.exe"):
        try:
            rel = exe.relative_to(install_dir)
            if len(rel.parts) > 3:
                continue
        except ValueError:
            continue
        if exe.name.lower() in EXE_BLACKLIST:
            continue
        parts_lower = [p.lower() for p in rel.parts]
        if any(skip in parts_lower for skip in
               ["easyanticheat", "battleye", "_commonredist", "directx", "redist"]):
            continue
        candidates.append((exe.stat().st_size, exe))
    if not candidates:
        return ""
    candidates.sort(key=lambda x: -x[0])
    return str(candidates[0][1])


def find_game(app_id: str) -> tuple:
    """Returns (name, exe_path) for an app ID."""
    for manifest in STEAMAPPS.glob("appmanifest_*.acf"):
        info = parse_appmanifest(manifest)
        if info.get("appid") == app_id:
            install_dir = STEAMAPPS / "common" / info.get("installdir", "")
            exe = find_game_exe(install_dir)
            return (info.get("name", "?"), exe)
    return ("?", "")

# ── Process management ────────────────────────────────────────────────

def cleanup_processes():
    """Kill quark/triskelion test processes and their Wine children.

    Wine child processes (services.exe, wineboot.exe, Balatro.exe, etc.) run as
    system Wine binaries — their cmdline shows Windows exe paths, not the quark
    directory. We must also kill these or they persist as zombies across test runs.

    Safe: we identify children by checking if their parent chain includes a process
    from the quark compat tool directory, OR if they're orphaned Wine processes
    from our prefix (compatdata/2379780)."""
    compat_dir = str(COMPAT_DIR)
    uid = os.getuid()

    # Phase 1: Graceful shutdown for triskelion
    subprocess.run(["pkill", "-TERM", "-f", "triskelion"],
                   capture_output=True, timeout=5)
    time.sleep(0.3)

    # Phase 2: Kill quark-spawned processes (scoped to our directory)
    for pattern in [f"{compat_dir}/proton",
                     f"{compat_dir}/lib/wine",
                     f"{compat_dir}/bin/wine",
                     "triskelion"]:
        subprocess.run(["pkill", "-9", "-f", pattern],
                       capture_output=True, timeout=5)
    time.sleep(0.5)

    # Phase 3: Kill orphaned Wine child processes (.exe processes, wineserver)
    # These don't match the quark directory but are from our test session.
    # Identify by: running as .exe (Wine PE), or wineserver with our prefix inode.
    try:
        result = subprocess.run(["ps", "-u", str(uid), "-o", "pid,args", "--no-headers"],
                                capture_output=True, text=True, timeout=5)
        if result.returncode == 0:
            wine_exe_patterns = [".exe", "wineserver", "wine64-preloader", "wine-preloader"]
            for line in result.stdout.strip().splitlines():
                parts = line.split(None, 1)
                if len(parts) < 2:
                    continue
                pid_str, cmdline = parts
                # Skip if it looks like a stock Proton process (contains Proton path but NOT quark)
                if "Proton" in cmdline and compat_dir not in cmdline:
                    continue
                if any(pat in cmdline for pat in wine_exe_patterns):
                    try:
                        os.kill(int(pid_str), signal.SIGKILL)
                    except (ValueError, ProcessLookupError, PermissionError):
                        pass
    except subprocess.TimeoutExpired:
        pass
    time.sleep(0.5)

    # Phase 4: Final kill pass — daemon runs as "triskelion", not matched by Phase 2's patterns.
    # Also catch anything from our compat dir that survived earlier phases.
    subprocess.run(["pkill", "-9", "-f", "triskelion"], capture_output=True, timeout=5)
    subprocess.run(["pkill", "-9", "-f", f"{compat_dir}"], capture_output=True, timeout=5)
    time.sleep(1.0)

    # Phase 5: Clean stale sockets. Must come AFTER all kills + sleep so
    # the daemon has fully exited and released the socket bind.
    # Only remove the socket file, not the directory — Wine expects the
    # directory to exist and recreating it races with the daemon's bind().
    wine_tmp = pathlib.Path(f"/tmp/.wine-{uid}")
    if wine_tmp.exists():
        for entry in wine_tmp.iterdir():
            if entry.is_dir() and entry.name.startswith("server-"):
                sock = entry / "socket"
                if sock.exists():
                    sock.unlink(missing_ok=True)
                lock = entry / "lock"
                if lock.exists():
                    lock.unlink(missing_ok=True)


def find_wine_pids() -> list:
    pids = []
    for pattern in ["wine64-preloader", "wine64", "wine-preloader", "wine",
                     r"\.exe", "wineserver"]:
        try:
            result = subprocess.run(["pgrep", "-a", "-f", pattern],
                                    capture_output=True, text=True, timeout=5)
            if result.returncode == 0:
                for line in result.stdout.strip().splitlines():
                    pid = int(line.split()[0])
                    if pid not in pids:
                        pids.append(pid)
        except (subprocess.TimeoutExpired, ValueError):
            pass
    return pids

# ── /proc inspection ──────────────────────────────────────────────────

def read_proc(pid: int, entry: str) -> str:
    try:
        return Path(f"/proc/{pid}/{entry}").read_text(errors="replace").strip()
    except (OSError, PermissionError):
        return ""


def snapshot_process(pid: int) -> ProcSnapshot:
    snap = ProcSnapshot(pid=pid)
    snap.comm = read_proc(pid, "comm")
    if not snap.comm:
        return snap

    status = read_proc(pid, "status")
    for line in status.splitlines():
        if line.startswith("State:"):
            snap.state = line.split()[1]
            break

    snap.wchan = read_proc(pid, "wchan")

    sc = read_proc(pid, "syscall")
    if sc and sc != "running":
        parts = sc.split()
        try:
            snap.syscall_nr = int(parts[0])
            snap.syscall_name = SYSCALL_NAMES.get(snap.syscall_nr, f"sys_{snap.syscall_nr}")
            snap.syscall_args = " ".join(parts[1:7])
        except (ValueError, IndexError):
            pass

    fd_dir = Path(f"/proc/{pid}/fd")
    try:
        fds = list(fd_dir.iterdir())
        snap.fd_count = len(fds)
        for fd_link in sorted(fds, key=lambda f: int(f.name))[:20]:
            try:
                target = os.readlink(str(fd_link))
                snap.key_fds.append(f"{fd_link.name}->{target}")
            except OSError:
                pass
    except OSError:
        pass

    try:
        raw = Path(f"/proc/{pid}/cmdline").read_bytes()
        snap.cmdline = raw.replace(b"\x00", b" ").decode(errors="replace").strip()
    except OSError:
        pass

    return snap


def analyze_hang(snapshots: list) -> str:
    if not snapshots:
        return "No processes to analyze"
    lines = []
    for snap in snapshots:
        if not snap.comm:
            continue
        if snap.state in ("S", "D"):
            fd_info = ""
            if snap.syscall_name in ("read", "recvmsg", "write", "sendmsg", "poll", "ppoll", "ioctl"):
                parts = snap.syscall_args.split()
                if parts:
                    try:
                        fd_num = int(parts[0], 16)
                        for fd_entry in snap.key_fds:
                            if fd_entry.startswith(f"{fd_num}->"):
                                fd_info = fd_entry
                                break
                    except ValueError:
                        pass
            # Always try to extract fd from first syscall arg
            if not fd_info and snap.syscall_args:
                parts = snap.syscall_args.split()
                if parts:
                    try:
                        fd_num = int(parts[0], 16)
                        for fd_entry in snap.key_fds:
                            if fd_entry.startswith(f"{fd_num}->"):
                                fd_info = fd_entry
                                break
                        if not fd_info:
                            fd_info = f"fd={fd_num}"
                    except ValueError:
                        pass
            analysis = f"PID {snap.pid} ({snap.comm}) blocked in {snap.syscall_name}()"
            if snap.wchan and snap.wchan != "0":
                analysis += f" @ {snap.wchan}"
            if fd_info:
                analysis += f" on {fd_info}"
            if snap.syscall_args:
                analysis += f" args=[{snap.syscall_args}]"
            lines.append(analysis)
    return "\n  ".join(lines) if lines else "All processes in running state"

# ── Build & Install ──────────────────────────────────────────────────

def build(skip: bool = False) -> bool:
    if skip:
        log_info("build: skipped (--skip-build)")
        return True

    log_info("build: cargo build --release")
    t0 = time.monotonic()
    result = subprocess.run(
        ["cargo", "build", "--release"],
        cwd=str(REPO_ROOT),
        capture_output=True, text=True, timeout=300,
    )
    elapsed = time.monotonic() - t0

    if result.returncode != 0:
        log_error(f"build: failed ({elapsed:.1f}s)")
        for line in result.stderr.splitlines()[-20:]:
            print(f"  {line}")
        return False

    warnings = sum(1 for line in result.stderr.splitlines() if "warning" in line.lower())
    log_info(f"build: ok ({elapsed:.1f}s, {warnings} warnings)")
    return True


def install() -> bool:
    # Kill stale processes holding the binary
    killed = 0
    for binary in [str(PROTON_BIN), str(REPO_BINARY)]:
        try:
            result = subprocess.run(["fuser", binary],
                                    capture_output=True, text=True, timeout=5)
            if result.stdout.strip():
                pids = result.stdout.strip().split()
                for pid in pids:
                    try:
                        os.kill(int(pid.strip()), signal.SIGKILL)
                        killed += 1
                    except (ValueError, ProcessLookupError, PermissionError):
                        pass
        except (subprocess.TimeoutExpired, FileNotFoundError):
            pass

    if killed:
        log_warn(f"install: killed {killed} stale process(es)")
        time.sleep(1)

    if not CARGO_TARGET.exists():
        log_error(f"install: {CARGO_TARGET} not found")
        return False

    try:
        # Deploy all three binaries using atomic rename to avoid ETXTBSY.
        # If the old binary is still mapped by a running process, copy+rename
        # succeeds because rename replaces the directory entry while the old
        # inode stays alive until the last process closes it.
        for name, src in [("triskelion", CARGO_TARGET), ("quark", CARGO_QUARK), ("parallax", CARGO_PARALLAX)]:
            if src.exists():
                for dst_dir in [REPO_ROOT, COMPAT_DIR]:
                    dst = dst_dir / name
                    tmp = dst_dir / f".{name}.tmp"
                    shutil.copy2(str(src), str(tmp))
                    os.rename(str(tmp), str(dst))
        # proton symlink -> quark (the launcher)
        proton_link = COMPAT_DIR / "proton"
        if proton_link.exists() or proton_link.is_symlink():
            proton_link.unlink()
        proton_link.symlink_to("quark")
    except (OSError, shutil.SameFileError) as e:
        log_error(f"install: {e}")
        return False

    # Clean logs, opcode stats, and shm
    for p in [str(DAEMON_LOG), str(OPCODE_STATS)] + glob.glob(SHM_GLOB) + glob.glob(WINE_INIT_GLOB):
        try:
            os.unlink(p)
        except OSError:
            pass
    os.makedirs("/tmp/quark", exist_ok=True)
    log_info(f"install: deployed to {PROTON_BIN}")
    return True

# ── Daemon log analysis ──────────────────────────────────────────────

def parse_daemon_log() -> DaemonReport:
    report = DaemonReport()

    if not DAEMON_LOG.exists():
        return report

    text = DAEMON_LOG.read_text(errors="replace")
    lines = text.splitlines()

    opcode_counts = Counter()
    all_requests = []  # (seq, opcode, fd)
    last_per_fd = {}  # fd -> (seq, opcode)

    lifecycle_keys = [
        "InitFirstThread", "InitThread", "NewProcess", "NewThread",
        "InitProcessDone", "GetStartupInfo", "TerminateProcess", "TerminateThread",
    ]
    lifecycle = {k: 0 for k in lifecycle_keys}

    for line in lines:
        # Requests
        m = RE_REQUEST.search(line)
        if m:
            seq, opcode, fd = int(m.group(1)), m.group(2), m.group(3)
            opcode_counts[opcode] += 1
            all_requests.append((seq, opcode, fd))
            last_per_fd[fd] = (seq, opcode)
            if opcode in lifecycle:
                lifecycle[opcode] += 1
            continue

        # Errors
        m = RE_ERROR.search(line)
        if m:
            report.errors.append((m.group(1), m.group(2)))
            continue

        # Disconnects
        m = RE_DISCONNECT.search(line)
        if m:
            fd, pid, tid = m.group(1), m.group(2), m.group(3)
            last_req = last_per_fd.get(fd, (0, "?"))
            report.disconnects.append({
                "fd": fd, "pid": pid, "tid": tid,
                "after_seq": last_req[0], "after_op": last_req[1],
            })
            continue

        # Panics
        m = RE_PANIC.search(line)
        if m:
            report.panics.append(m.group(1))

    report.total_requests = len(all_requests)
    report.opcode_counts = dict(opcode_counts.most_common())
    report.lifecycle = lifecycle
    report.last_requests = all_requests[-10:]
    report.last_request_per_fd = {fd: (seq, op) for fd, (seq, op) in last_per_fd.items()}

    # Summarize errors
    error_counter = Counter()
    for opcode, status in report.errors:
        error_counter[f"{opcode} err={status}"] += 1
    report.error_summary = dict(error_counter.most_common())

    return report

# ── Stderr analysis ──────────────────────────────────────────────────

WINE_STDERR_LOG = Path("/tmp/quark/wine_stderr.log")
WINE_STDOUT_LOG = Path("/tmp/quark/wine_stdout.log")

# ── Screenshots ──────────────────────────────────────────────────────
#
# Capturing the screen at fixed checkpoints lets us see what the user sees
# without having to type "white screen" / "title showing" / etc. The
# screenshot tool is detected once at module load and the result is
# cached. Failures are silent — a missing tool must never abort a run.

SCREENSHOTS_DIR = Path("/tmp/quark/screenshots")
SCREENSHOT_CHECKPOINTS_S = (5, 15, 30, 45)  # seconds after launch

def _detect_screenshot_cmd():
    """Return a callable that takes (path) and writes a PNG, or None.

    Probe order is biased toward Wayland-native tools because the typical
    setup runs games through XWayland, and ImageMagick's `import` cannot
    grab the XWayland root window on most compositors (returns
    "missing an image filename"). Wayland-native captures of the full
    output work everywhere.

      1. KDE `spectacle -b -n -f -o`  — Plasma/Wayland, no notification
      2. `grim`                       — wlroots compositors (Sway, Hyprland)
      3. `gnome-screenshot -f`        — GNOME Mutter
      4. ImageMagick `import`         — pure-X11 fallback (last resort)
    """
    candidates = [
        (["spectacle", "-b", "-n", "-f", "-o"], lambda c, p: c + [str(p)]),
        (["grim"],                              lambda c, p: c + [str(p)]),
        (["gnome-screenshot", "-f"],            lambda c, p: c + [str(p)]),
        (["import", "-window", "root"],         lambda c, p: c + [str(p)]),
    ]
    for cmd, build in candidates:
        if shutil.which(cmd[0]):
            return (cmd, build)
    return None

_SCREENSHOT_CMD = _detect_screenshot_cmd()


def take_screenshot(appid: str, ts_prefix: str, elapsed_s: int) -> Path | None:
    """Grab a full-screen PNG into SCREENSHOTS_DIR.

    Returns the written path on success, None on any failure (missing tool,
    no display, command error). Never raises — screenshot capture is
    best-effort and must not break a run.
    """
    if _SCREENSHOT_CMD is None:
        return None
    SCREENSHOTS_DIR.mkdir(parents=True, exist_ok=True)
    out_path = SCREENSHOTS_DIR / f"{appid}_{ts_prefix}_t{elapsed_s:02d}s.png"
    cmd, build = _SCREENSHOT_CMD
    try:
        full = build(cmd, out_path)
        result = subprocess.run(full, capture_output=True, timeout=5)
        if result.returncode == 0 and out_path.exists() and out_path.stat().st_size > 0:
            return out_path
    except (subprocess.TimeoutExpired, OSError):
        pass
    # Clean up zero-byte file if tool failed mid-write
    if out_path.exists() and out_path.stat().st_size == 0:
        try: out_path.unlink()
        except OSError: pass
    return None

def _collect_stderr_analysis() -> str:
    """Parse daemon stderr + wine stderr for NOT_IMPLEMENTED opcodes and crashes."""
    lines = []

    # Check daemon.log for NOT_IMPLEMENTED errors
    if DAEMON_LOG.exists():
        try:
            text = DAEMON_LOG.read_text(errors="replace")
            handlers, not_impl, crashes = parse_triskelion_stderr(text)
            if not_impl:
                lines.append(f"  NOT_IMPLEMENTED opcodes (from daemon): {', '.join(sorted(not_impl))}")
            if crashes:
                lines.append(f"  CRASHES ({len(crashes)}):")
                for c in crashes[:5]:
                    lines.append(f"    {c}")
        except OSError:
            pass

    # Check wine stderr for additional clues
    if WINE_STDERR_LOG.exists():
        try:
            text = WINE_STDERR_LOG.read_text(errors="replace")
            # Look for error messages, missing DLLs, assertion failures
            for line in text.splitlines():
                if any(s in line.lower() for s in ["error", "abort", "assertion", "unhandled exception"]):
                    lines.append(f"  wine: {line.strip()[:200]}")
        except OSError:
            pass

    return "\n".join(lines) if lines else ""


# ── Launch & Monitor ─────────────────────────────────────────────────

_LAST_LAUNCH_STAMP: str = ""


def launch_and_monitor(app_id: str, game_name: str, exe_path: str,
                       timeout: int) -> tuple:
    """Launch game, monitor, return (verdict, elapsed, report, snapshots)."""
    global _LAST_LAUNCH_STAMP
    _LAST_LAUNCH_STAMP = datetime.now().strftime("%Y%m%d-%H%M%S")
    log_info(f"launch: {game_name} ({app_id})")
    log_info(f"  exe: {exe_path}")

    cleanup_processes()

    env = os.environ.copy()
    env["STEAM_COMPAT_DATA_PATH"] = str(STEAMAPPS / "compatdata" / app_id)
    env["STEAM_COMPAT_CLIENT_INSTALL_PATH"] = str(STEAM_ROOT)
    env["SteamAppId"] = app_id
    env["SteamGameId"] = app_id
    env["WINEDEBUG"] = os.environ.get("QUARK_WINEDEBUG", "+driver,+loaddll,+wgl,+x11drv,+system,err")
    #env["LD_DEBUG"] = "libs"
    # Ensure display vars are passed through
    for var in ("DISPLAY", "WAYLAND_DISPLAY", "XDG_RUNTIME_DIR", "XAUTHORITY"):
        if var in os.environ:
            env[var] = os.environ[var]
    # Don't override EGL vendor — let Wine use GLX via UseEGL=N registry key
    env.pop("__EGL_VENDOR_LIBRARY_FILENAMES", None)

    t0 = clock_raw_ns()

    try:
        # Capture both stdout and stderr to files. LOVE2D writes Lua errors
        # to stdout via lua's print() and via callbacks.lua's error handler,
        # so capturing stdout is essential for diagnosing fused-exe issues.
        proc = subprocess.Popen(
            [str(PROTON_BIN), "run", exe_path],
            env=env,
            cwd="/tmp",
            stdout=open("/tmp/quark/wine_stdout.log", "w"),
            stderr=open("/tmp/quark/wine_stderr.log", "w"),
        )
    except OSError as e:
        return ("launch_fail", 0, DaemonReport(), [], f"Failed to start: {e}", None)

    # ── Dynamic opcode tracing ──────────────────────────────────────
    tracer = OpcodeTracer()
    prev_requests = 0
    stall_ticks = 0
    poll_interval = 2
    ticks = timeout // poll_interval

    for tick in range(ticks):
        time.sleep(poll_interval)

        # Scan daemon.log for new opcodes
        tracer.scan()
        new_ops = tracer.drain_new()

        # Check if proton launcher exited — but DON'T stop monitoring.
        # The launcher exits quickly after spawning wine64. The game runs
        # as a background Wine process. Keep monitoring until timeout or
        # until all Wine processes exit.
        ret = proc.poll()
        if ret is not None and not hasattr(proc, '_launcher_exited'):
            proc._launcher_exited = True  # mark but don't stop

        # Live opcode discovery reporting
        if new_ops:
            disc = tracer.format_discovery_report(new_ops)
            if disc:
                log_info(f"new opcodes: {disc.strip()}")

        requests = tracer.total_requests

        if requests == prev_requests and requests > 0:
            stall_ticks += 1
        else:
            stall_ticks = 0
        prev_requests = requests

        elapsed = ns_to_ms(clock_raw_ns() - t0) / 1000
        pids = find_wine_pids()
        pid_str = f" pids={[p for p in pids]}" if pids else ""
        stall_str = f" STALL={stall_ticks}" if stall_ticks else ""
        live = tracer.format_live_status()

        # Screenshot checkpoints — capture full screen at fixed elapsed times
        # so we can see what was actually rendered without the user typing it.
        # Files land in /tmp/quark/screenshots/ with the same {appid}_{stamp}
        # prefix as the archived daemon.log/wine_stderr.log.
        for cp in SCREENSHOT_CHECKPOINTS_S:
            if cp <= elapsed < cp + poll_interval:
                shot = take_screenshot(app_id, _LAST_LAUNCH_STAMP, cp)
                if shot:
                    log_info(f"screenshot: {shot.name}")
                break

        # Check wine_stderr + daemon log for milestones.
        #
        # IMPORTANT: every milestone here must be a TRUE positive — substring
        # matches that fire on error screens, helper windows, or unrelated
        # codepaths are misleading and have caused multiple "looks playable"
        # hallucinations. The bar:
        #   - "RENDERING" must mean a real top-level visible window received
        #     a real WM_PAINT, NOT just that wglSwapBuffers appeared (which
        #     fires for SDL2 helper windows AND for the LOVE2D error screen)
        #   - "STEAM_API" requires the bridge to actually answer, not just
        #     for the string "steam_api64" to appear in a load message
        #   - "CRASH" requires an actual exit/SIGSEGV, not "Unhandled" which
        #     appears in stub fixme lines that don't crash anything
        milestones = []
        try:
            stderr_text = WINE_STDERR_LOG.read_text(errors="replace") if WINE_STDERR_LOG.exists() else ""
            daemon_text = DAEMON_LOG.read_text(errors="replace") if DAEMON_LOG.exists() else ""
            if "winex11.drv" in stderr_text:
                milestones.append("x11drv")
            if "XRandR" in stderr_text:
                milestones.append("XRandR")
            if "desktop_ready" in daemon_text:
                milestones.append("DESKTOP")
            if "NO GAME" in stderr_text or "love" in stderr_text.lower():
                milestones.append("LÖVE")

            # RENDERING: a real top-level visible window must have received a
            # WM_PAINT delivery from the daemon. Helper windows (style=0,
            # parent=desktop_msg_window 0x22) and the LOVE2D error screen do
            # not count. Daemon logs WM_PAINT deliveries via:
            #   "get_message: tid=... win=0x.... msg=WM_PAINT"
            # Cross-reference with create_window logs to filter out windows
            # whose parent is the desktop msg window (those are SDL2 helpers).
            if "msg=WM_PAINT" in daemon_text:
                # Walk the daemon log: collect parent_msg_window helper handles,
                # find WM_PAINT deliveries to handles NOT in that set.
                helper_wins = set()
                for line in daemon_text.splitlines():
                    if "create_window:" in line and "parent=0x0022" in line:
                        m = re.search(r"create_window: handle=(0x[0-9a-fA-F]+)", line)
                        if m:
                            helper_wins.add(m.group(1).lower())
                real_paint = False
                for line in daemon_text.splitlines():
                    if "msg=WM_PAINT" not in line:
                        continue
                    m = re.search(r"win=(0x[0-9a-fA-F]+) msg=WM_PAINT", line)
                    if m and m.group(1).lower() not in helper_wins:
                        real_paint = True
                        break
                if real_paint:
                    milestones.append("RENDERING")

            # CRASH: a real exit from the launcher with non-zero status, OR
            # an actual SIGSEGV trace. "Unhandled" alone is too noisy.
            if re.search(r'\[quark\] exit: [1-9]', stderr_text) or "SIGSEGV" in stderr_text:
                milestones.append("CRASH")
            # LOVE error screen specifically — different from a quark crash
            if 'love "callbacks.lua"' in stderr_text or "module 'engine/object'" in stderr_text:
                milestones.append("LOVE_ERROR")

            if "steam_api64" in stderr_text:
                milestones.append("STEAM_API")
            if any(f"err:service" in l for l in stderr_text.splitlines()[-5:]):
                milestones.append("SVC_ERR")
        except Exception:
            pass
        ms_str = f" | {' '.join(milestones)}" if milestones else ""

        log_info(f"[{elapsed:.0f}s] {live}{stall_str}{pid_str}{ms_str}")

        # Check daemon health
        daemon_alive, daemon_pid = check_daemon_health()
        if not daemon_alive and requests > 0:
            log_warn("daemon appears dead")

        # Stall: 15 consecutive polls with no growth (30s at 2s interval)
        # Games like LÖVE may idle in message loop for a while before rendering
        if stall_ticks >= 15 and requests > 0:
            log_warn(f"stall detected at {requests} requests after {elapsed:.0f}s")
            # Per-THREAD state dump — the key diagnostic for ntsync deadlocks
            for p in pids:
                try:
                    comm = Path(f"/proc/{p}/comm").read_text().strip()
                except Exception:
                    comm = "?"
                # Build fd->target map for cross-referencing syscall args
                fd_map = {}
                try:
                    fd_dir = Path(f"/proc/{p}/fd")
                    for fd_link in sorted(fd_dir.iterdir(), key=lambda f: int(f.name)):
                        try:
                            target = os.readlink(str(fd_link))
                            fd_map[int(fd_link.name)] = target
                        except (OSError, ValueError):
                            pass
                except OSError:
                    pass
                fd_summary = " ".join(f"{n}->{t}" for n, t in sorted(fd_map.items()))
                print(f"  PID {p} ({comm}) — {len(fd_map)} fds")
                print(f"  fds: {fd_summary}")

                # Per-thread inspection
                task_dir = Path(f"/proc/{p}/task")
                try:
                    tids = sorted(int(t.name) for t in task_dir.iterdir()
                                  if t.name.isdigit())
                except OSError:
                    tids = []

                for tid in tids:
                    tbase = f"/proc/{p}/task/{tid}"
                    try:
                        tcomm = Path(f"{tbase}/comm").read_text().strip()
                    except Exception:
                        tcomm = "?"
                    try:
                        sc = Path(f"{tbase}/syscall").read_text().strip()
                    except Exception:
                        sc = "?"
                    try:
                        wchan = Path(f"{tbase}/wchan").read_text().strip()
                    except Exception:
                        wchan = "?"

                    # Parse syscall line to resolve fd
                    fd_info = ""
                    sc_fd_num = -1
                    is_ioctl = False
                    if sc and sc != "running":
                        parts = sc.split()
                        try:
                            sc_nr = int(parts[0])
                            sc_name = SYSCALL_NAMES.get(sc_nr, f"sys_{sc_nr}")
                            if sc_name in ("ioctl", "read", "write", "recvmsg",
                                           "sendmsg", "poll", "ppoll"):
                                sc_fd_num = int(parts[1], 16)
                                target = fd_map.get(sc_fd_num, "?")
                                fd_info = f" fd={sc_fd_num}->{target}"
                                if sc_name == "ioctl":
                                    is_ioctl = True
                                    if len(parts) > 2:
                                        fd_info += f" cmd={parts[2]}"
                            sc = f"{sc_name} [{' '.join(parts[1:7])}]"
                        except (ValueError, IndexError):
                            pass

                    print(f"    TID {tid:>7} {tcomm:<20} syscall={sc}{fd_info}  wchan={wchan}")

                    # Show kernel stack for blocked threads (ntsync ioctl OR futex)
                    show_stack = (is_ioctl and "ntsync" in fd_map.get(sc_fd_num, ""))
                    show_stack = show_stack or ("futex" in wchan)
                    show_stack = show_stack or (is_ioctl)
                    if show_stack:
                        try:
                            stack = Path(f"{tbase}/stack").read_text().strip()
                            for sline in stack.splitlines()[:10]:
                                print(f"      {sline.strip()}")
                        except Exception:
                            pass
            # Resolve instruction pointers to library names via /proc maps
            for p in pids:
                try:
                    comm = Path(f"/proc/{p}/comm").read_text().strip()
                except Exception:
                    comm = "?"
                if comm in ("proton", "wineserver"):
                    continue
                # Load memory maps for IP resolution
                maps = []
                try:
                    for line in Path(f"/proc/{p}/maps").read_text().splitlines():
                        parts = line.split()
                        if len(parts) >= 6:
                            addrs = parts[0].split("-")
                            if len(addrs) == 2:
                                start = int(addrs[0], 16)
                                end = int(addrs[1], 16)
                                lib = parts[5] if len(parts) >= 6 else ""
                                maps.append((start, end, lib))
                except Exception:
                    pass

                task_dir = Path(f"/proc/{p}/task")
                try:
                    tids = sorted(int(t.name) for t in task_dir.iterdir()
                                  if t.name.isdigit())
                except OSError:
                    continue

                log_info(f"ip resolve: PID {p} ({comm})")
                for tid in tids:
                    tbase = f"/proc/{p}/task/{tid}"
                    try:
                        tcomm = Path(f"{tbase}/comm").read_text().strip()
                    except Exception:
                        tcomm = "?"
                    try:
                        sc = Path(f"{tbase}/syscall").read_text().strip()
                    except Exception:
                        continue
                    # syscall line: nr arg0 arg1 arg2 arg3 arg4 arg5 SP IP
                    parts = sc.split()
                    if len(parts) >= 9:
                        try:
                            ip = int(parts[8], 16)
                            sp = int(parts[7], 16)
                            lib = "?"
                            offset = 0
                            for start, end, name in maps:
                                if start <= ip < end:
                                    lib = name.split("/")[-1] if "/" in name else name
                                    offset = ip - start
                                    break
                            print(f"    TID {tid:>7} {tcomm:<20} IP={ip:#x} → {lib}+{offset:#x}  SP={sp:#x}")
                        except (ValueError, IndexError):
                            pass

            report = parse_daemon_log()
            snaps = [snapshot_process(p) for p in find_wine_pids()]
            stderr_report = _collect_stderr_analysis()
            if stderr_report:
                print(stderr_report)
            proc.kill()
            proc.wait()
            cleanup_processes()
            return ("hung", elapsed, report, snaps, "", tracer)

    # Timeout
    elapsed_ns = clock_raw_ns() - t0
    elapsed = ns_to_ms(elapsed_ns) / 1000
    tracer.scan()  # final sweep
    report = parse_daemon_log()
    snaps = [snapshot_process(p) for p in find_wine_pids()]
    stderr_report = _collect_stderr_analysis()
    if stderr_report:
        print(stderr_report)
    proc.kill()
    proc.wait()
    cleanup_processes()
    return ("timeout", elapsed, report, snaps, "", tracer)

# ── Opcode analysis ──────────────────────────────────────────────────

def parse_opcode_stats() -> list:
    """Read the daemon's opcode stats file. Returns [(name, count), ...] sorted by count desc."""
    if not OPCODE_STATS.exists():
        return []
    entries = []
    for line in OPCODE_STATS.read_text(errors="replace").splitlines()[1:]:  # skip header
        parts = line.strip().split(None, 1)
        if len(parts) == 2:
            try:
                count = int(parts[0])
                name = parts[1]
                entries.append((name, count))
            except ValueError:
                pass
    return entries


def print_opcode_analysis(opcode_stats: list, total: int):
    """The money shot: which opcodes does the game need that we haven't implemented?"""
    if not opcode_stats:
        return

    implemented_hits = []
    stubbed_hits = []

    for name, count in opcode_stats:
        if name in IMPLEMENTED_OPCODES:
            implemented_hits.append((name, count))
        else:
            stubbed_hits.append((name, count))

    impl_count = len(implemented_hits)
    stub_count = len(stubbed_hits)
    total_unique = impl_count + stub_count
    coverage = impl_count / total_unique * 100 if total_unique else 0

    impl_req = sum(c for _, c in implemented_hits)
    stub_req = sum(c for _, c in stubbed_hits)
    req_coverage = impl_req / total * 100 if total else 0

    log_info(f"opcodes: {impl_count}/{total_unique} implemented ({coverage:.0f}%), "
             f"{impl_req}/{total} requests ({req_coverage:.1f}%)")

    if stubbed_hits:
        log_info("stubbed opcodes (implement next):")
        for name, count in stubbed_hits:
            pct = count / total * 100 if total else 0
            print(f"  {count:>8}  {pct:>5.1f}%  {name}")

    if implemented_hits:
        log_info(f"implemented ({impl_count} opcodes, {impl_req} requests):")
        for name, count in implemented_hits[:15]:
            pct = count / total * 100 if total else 0
            print(f"  {count:>8}  {pct:>5.1f}%  {name}")
        if len(implemented_hits) > 15:
            print(f"  ... and {len(implemented_hits) - 15} more")


# ── Report ───────────────────────────────────────────────────────────

def print_report(verdict: str, elapsed: float, report: DaemonReport,
                 snapshots: list, tracer: 'OpcodeTracer | None' = None):
    log_info(f"verdict: {verdict} after {report.total_requests} requests ({elapsed:.1f}s)")

    # Lifecycle
    for key, count in report.lifecycle.items():
        if count > 0 or key in ("InitFirstThread", "NewProcess", "InitProcessDone", "GetStartupInfo"):
            log_info(f"  {key}: {count}")

    # Errors
    for err, count in report.error_summary.items():
        log_warn(f"  {err}: {count}")

    # Panics
    for p in report.panics:
        log_error(f"  panic: {p}")

    # Dynamic opcode trace report (from live tracing)
    if tracer and tracer.seen_opcodes:
        print(tracer.format_final_report())

    # Opcode analysis
    opcode_stats = parse_opcode_stats()
    if not opcode_stats and report.opcode_counts:
        opcode_stats = [(op, count) for op, count in report.opcode_counts.items()]
    print_opcode_analysis(opcode_stats, report.total_requests)

    # Display driver
    daemon_text = DAEMON_LOG.read_text() if DAEMON_LOG.exists() else ""
    stderr_text = WINE_STDERR_LOG.read_text() if WINE_STDERR_LOG.exists() else ""
    n_queue_fd = daemon_text.count("set_queue_fd")
    driver = ""
    for line in daemon_text.split("\n"):
        if "display device:" in line and "driver=" in line:
            driver = line.split("driver=")[-1].strip()
            break
    x11_loaded = "winex11.drv" in stderr_text or "x11drv" in stderr_text
    log_info(f"display: driver='{driver}' x11={x11_loaded} queue_fd={n_queue_fd}")

    # Hang analysis
    if snapshots:
        analysis = analyze_hang(snapshots)
        if analysis:
            log_info(f"hang analysis:")
            for line in analysis.split("\n"):
                if line.strip():
                    print(f"  {line.strip()}")

# ── Main ─────────────────────────────────────────────────────────────

def discover_all_games() -> list[tuple[str, str, str]]:
    """Find all playable Steam games. Returns [(appid, name, exe_path), ...]."""
    _skip_appids = {
        "228980", "1493710", "1887720", "1826330", "2180100",
        "961940", "1391110", "1580130", "2348590",
    }
    _skip_names = {"proton", "steamworks", "steam linux runtime", "redistribut", "sdk"}
    games = []
    seen = set()
    for manifest in sorted(STEAMAPPS.glob("appmanifest_*.acf")):
        info = parse_appmanifest(manifest)
        appid = info.get("appid", "")
        name = info.get("name", "")
        installdir = info.get("installdir", "")
        if not appid or not name or not installdir:
            continue
        if appid in seen or appid in _skip_appids:
            continue
        if any(s in name.lower() for s in _skip_names):
            continue
        install_dir = STEAMAPPS / "common" / installdir
        if not install_dir.exists():
            continue
        exe = find_game_exe(install_dir)
        if not exe:
            continue
        seen.add(appid)
        games.append((appid, name, exe))
    games.sort(key=lambda g: g[1])
    return games


def run_single_game(appid, game_name, exe_path, timeout, skip_build):
    """Run one game through iterate cycle. Returns result dict."""
    if not build(skip=skip_build):
        return {"appid": appid, "name": game_name, "verdict": "build_fail"}

    if not install():
        return {"appid": appid, "name": game_name, "verdict": "install_fail"}

    result = launch_and_monitor(appid, game_name, exe_path, timeout)
    verdict, elapsed, report, snapshots, err = result[:5]
    tracer = result[5] if len(result) > 5 else None

    # Check wine_stderr for key errors
    stderr_text = ""
    if WINE_STDERR_LOG.exists():
        stderr_text = WINE_STDERR_LOG.read_text(errors="replace")
    steam_auth_ok = "Failed to find steamclient_init_registry" not in stderr_text
    has_crash = "Unhandled page fault" in stderr_text or "page fault" in stderr_text

    # Archive — reuse the launch stamp so archived logs and screenshots
    # captured during the run share the same {appid}_{stamp} prefix.
    stamp = _LAST_LAUNCH_STAMP or datetime.now().strftime("%Y%m%d-%H%M%S")
    archive_dir = Path("/tmp/quark/runs")
    archive_dir.mkdir(parents=True, exist_ok=True)
    prefix = f"{appid}_{stamp}"
    for src, suffix in [(DAEMON_LOG, "daemon.log"), (WINE_STDERR_LOG, "wine_stderr.log"), (WINE_STDOUT_LOG, "wine_stdout.log")]:
        if src.exists() and src.stat().st_size > 0:
            dst = archive_dir / f"{prefix}_{suffix}"
            try:
                shutil.copy2(src, dst)
            except OSError:
                pass

    return {
        "appid": appid,
        "name": game_name,
        "verdict": verdict,
        "elapsed": elapsed,
        "requests": report.total_requests,
        "steam_auth": steam_auth_ok,
        "crash": has_crash,
        "tracer": tracer,
    }


def run_all_games(timeout, skip_build):
    """Discover and test all Steam games. Print summary table."""
    games = discover_all_games()
    if not games:
        log_error("no games found")
        return

    log_info(f"found {len(games)} games to test")
    for appid, name, _ in games:
        log_info(f"  {appid:>8}  {name}")
    print()

    # Build once
    if not build(skip=skip_build):
        log_error("build failed")
        return
    if not install():
        log_error("install failed")
        return

    # Verify patches first
    log_info("running patch verification...")
    r = subprocess.run([sys.executable, str(REPO_ROOT / "tests" / "verify_patches.py")],
                       capture_output=True, text=True)
    for line in r.stdout.splitlines():
        if "[FAIL]" in line or "RESULTS" in line:
            print(f"  {line.strip()}")
    if r.returncode != 0:
        log_warn("patch verification had failures (continuing anyway for dev testing)")
    else:
        log_info("patch verification passed")
    print()

    results = []
    for i, (appid, name, exe) in enumerate(games):
        log_info(f"GAME {i+1}/{len(games)}: {name} ({appid})")
        res = run_single_game(appid, name, exe, timeout, skip_build=True)
        results.append(res)
        print()

    # Summary table
    print(f"  {'Game':<40} {'Verdict':<10} {'Reqs':>8} {'Steam':>6} {'Crash':>6}")
    for r in results:
        name = r["name"][:39]
        verdict = r["verdict"]
        reqs = r.get("requests", 0)
        steam = "OK" if r.get("steam_auth") else "FAIL"
        crash = "YES" if r.get("crash") else "no"
        print(f"  {name:<40} {verdict:<10} {reqs:>8} {steam:>6} {crash:>6}")

    ok = sum(1 for r in results if r["verdict"] == "timeout" and not r.get("crash"))
    crashed = sum(1 for r in results if r.get("crash"))
    print(f"\n  {len(results)} games | {ok} running | {crashed} crashed")


def main():
    parser = argparse.ArgumentParser(description="Triskelion build-test-analyze cycle")
    parser.add_argument("--appid", default=DEFAULT_APPID, help=f"Steam app ID (default: {DEFAULT_APPID})")
    parser.add_argument("--timeout", type=int, default=DEFAULT_TIMEOUT, help=f"Timeout in seconds (default: {DEFAULT_TIMEOUT})")
    parser.add_argument("--skip-build", action="store_true", help="Skip cargo build")
    parser.add_argument("--exe", type=str, default=None, help="Override game exe path")
    parser.add_argument("--all", action="store_true", help="Test ALL installed Steam games")
    args = parser.parse_args()

    if args.all:
        run_all_games(args.timeout, args.skip_build)
        return

    # Find game
    game_name, exe_path = find_game(args.appid)
    if args.exe:
        exe_path = args.exe
    if not exe_path:
        log_error(f"could not find game exe for app {args.appid}")
        sys.exit(1)

    # Build
    if not build(skip=args.skip_build):
        sys.exit(1)

    # Install
    if not install():
        sys.exit(1)

    # Launch & monitor
    result = launch_and_monitor(args.appid, game_name, exe_path, args.timeout)
    verdict, elapsed, report, snapshots, err = result[:5]
    tracer = result[5] if len(result) > 5 else None

    if err:
        log_error(err)
        sys.exit(1)

    # Report
    print_report(verdict, elapsed, report, snapshots, tracer)

    # Archive logs — reuse the launch stamp so archived logs and screenshots
    # captured during the run share the same {appid}_{stamp} prefix.
    stamp = _LAST_LAUNCH_STAMP or datetime.now().strftime("%Y%m%d-%H%M%S")
    archive_dir = Path("/tmp/quark/runs")
    archive_dir.mkdir(parents=True, exist_ok=True)
    prefix = f"{args.appid}_{stamp}"
    for src, suffix in [(DAEMON_LOG, "daemon.log"), (WINE_STDERR_LOG, "wine_stderr.log"), (WINE_STDOUT_LOG, "wine_stdout.log")]:
        if src.exists() and src.stat().st_size > 0:
            dst = archive_dir / f"{prefix}_{suffix}"
            try:
                shutil.copy2(src, dst)
            except OSError:
                pass
    log_info(f"logs archived: /tmp/quark/runs/{prefix}_*")


if __name__ == "__main__":
    main()
