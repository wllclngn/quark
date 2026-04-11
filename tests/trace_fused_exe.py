#!/usr/bin/env python3
"""Trace LÖVE fused-exe detection: stock wineserver vs triskelion.

Uses montauk v5.1.0 --trace to capture eBPF syscall + I/O traces for both
runs, then diffs the file I/O on Balatro.exe to find where PhysFS fails.

Captures: open fds, read/write byte counts, lseek offsets + whence,
fstat return values, pread64 positioned reads — all correlated to filenames.

REQUIRES: sudo (montauk --trace needs CAP_SYS_ADMIN for eBPF)

Usage:
    sudo python3 tests/trace_fused_exe.py
    sudo python3 tests/trace_fused_exe.py --skip-stock   # Only run triskelion
    sudo python3 tests/trace_fused_exe.py --skip-quark     # Only run stock
    sudo python3 tests/trace_fused_exe.py --timeout 30   # Longer capture
"""

import os, subprocess, sys, time, signal, re, shutil
from datetime import datetime
from pathlib import Path
from collections import defaultdict


def _ts() -> str:
    return datetime.now().strftime("[%H:%M:%S]")

def log_info(msg: str) -> None:
    print(f"{_ts()} [INFO]   {msg}", flush=True)

def log_warn(msg: str) -> None:
    print(f"{_ts()} [WARN]   {msg}", flush=True)

def log_error(msg: str) -> None:
    print(f"{_ts()} [ERROR]  {msg}", flush=True)

# Resolve real user home even under sudo
_USER_HOME = Path(f"/home/{os.environ.get('SUDO_USER', os.environ.get('USER', 'mod'))}")

MONTAUK = _USER_HOME / "personal/PROGRAMMING/SYSTEM PROGRAMS/LINUX/montauk/build/montauk"
STEAM_ROOT = _USER_HOME / ".local/share/Steam"
QUARK_DIR = STEAM_ROOT / "compatibilitytools.d/quark"
GAME_EXE = STEAM_ROOT / "steamapps/common/Balatro/Balatro.exe"
COMPAT_DATA = STEAM_ROOT / "steamapps/compatdata/2379780"
PFX = COMPAT_DATA / "pfx"

# Stock Proton for comparison — use Proton's own wine (NOT system wine)
PROTON_DIR = STEAM_ROOT / "steamapps/common/Proton 10.0"
PROTON_BIN = PROTON_DIR / "proton"

TRACE_DIR = Path("/tmp/quark/fused_exe_trace")
STOCK_TRACE = TRACE_DIR / "stock"
QUARK_TRACE = TRACE_DIR / "quark"

TIMEOUT = int(sys.argv[sys.argv.index("--timeout") + 1]) if "--timeout" in sys.argv else 25

# Files we care about for the fused-exe diagnosis
INTERESTING_FILES = ["balatro", "love", ".exe", ".dll", ".love"]


def kill_all():
    """Kill quark/triskelion test processes only — never touch stock Proton.

    CRITICAL: pkill -f 'wine' would kill ALL wine processes system-wide,
    including Proton's wineserver for other running games. SIGKILL mid-operation
    leaves Proton prefixes in an inconsistent state (partial registry writes),
    which makes Proton appear 'broken' until the prefix is repaired.

    We only kill: our triskelion daemon, montauk, and wine processes spawned
    from quark's compat tool directory."""
    # Kill triskelion daemon and montauk tracer — safe, these are ours
    for pat in ["triskelion", "montauk"]:
        subprocess.run(["pkill", "-9", "-f", pat], capture_output=True)
    # Kill wine processes from quark's directory ONLY (not system/Proton wine)
    quark_bin = str(_USER_HOME / ".local/share/Steam/compatibilitytools.d/quark")
    subprocess.run(["pkill", "-9", "-f", quark_bin], capture_output=True)
    # Kill any wineserver in /tmp dirs that triskelion created (but not Proton's)
    subprocess.run(["pkill", "-9", "-x", "triskelion"], capture_output=True)
    time.sleep(2)


def run_trace(label, trace_dir, proton_bin, use_stock_proton=False):
    """Launch game + montauk --trace, capture syscall + I/O trace.

    Stock: uses Proton 10.0's own proton launcher (correct ABI, matching DLLs).
    Amp: uses quark's proton launcher (triskelion).
    Both run as the real user (sudo -u), montauk runs as root for BPF."""
    kill_all()
    if trace_dir.exists():
        shutil.rmtree(trace_dir)
    trace_dir.mkdir(parents=True, exist_ok=True)

    env = os.environ.copy()
    env["STEAM_COMPAT_DATA_PATH"] = str(COMPAT_DATA)
    env["STEAM_COMPAT_CLIENT_INSTALL_PATH"] = str(STEAM_ROOT)
    env["SteamAppId"] = "2379780"
    env["SteamGameId"] = "2379780"
    env["WINEDEBUG"] = "-all"
    env["HOME"] = str(_USER_HOME)

    if use_stock_proton:
        # Use Proton 10.0's own launcher — correct Wine + DLLs for this prefix
        game_cmd = ["python3", str(PROTON_BIN), "run", str(GAME_EXE)]
    else:
        game_cmd = [str(proton_bin / "proton"), "run", str(GAME_EXE)]

    # Drop to real user for Wine — sudo runs as root which breaks prefix permissions
    real_user = os.environ.get("SUDO_USER", "")
    if real_user and os.geteuid() == 0:
        game_cmd = ["sudo", "-u", real_user, "--preserve-env=STEAM_COMPAT_DATA_PATH,STEAM_COMPAT_CLIENT_INSTALL_PATH,SteamAppId,SteamGameId,WINEDEBUG,HOME"] + game_cmd

    # Start montauk --trace BEFORE the game so it catches the exec.
    # Pattern "wine" matches wine-preloader, wine64, wineserver, and children.
    trace_pattern = "wine"
    log_info(f"[{label}] starting montauk --trace {trace_pattern}")
    montauk_log = trace_dir / "montauk.log"
    montauk_proc = subprocess.Popen(
        [str(MONTAUK), "--trace", trace_pattern, "--log", str(trace_dir),
         "--log-interval-ms", "500"],
        stdout=open(montauk_log, "w"),
        stderr=subprocess.STDOUT,
    )
    time.sleep(2)  # let eBPF attach and settle

    # Launch the game
    log_info(f"[{label}] launching game")
    stderr_log = trace_dir / "wine_stderr.log"
    game_proc = subprocess.Popen(
        game_cmd, env=env, cwd="/tmp",
        stdout=subprocess.PIPE,
        stderr=open(stderr_log, "w"),
        start_new_session=True,  # own process group — killpg cleans entire tree
    )

    # Wait for game to run — check progress
    for i in range(TIMEOUT):
        time.sleep(1)
        if game_proc.poll() is not None:
            log_info(f"[{label}] game exited at {i+1}s")
            # Keep montauk running a bit to capture final state
            time.sleep(2)
            break
        if i % 5 == 4:
            prom_files = list(trace_dir.glob("montauk_*.prom"))
            log_info(f"[{label}] tick={i+1}s snapshots={len(prom_files)}")

    # Kill the ENTIRE process tree spawned by the game launcher.
    # start_new_session=True puts all children (wineserver, wine-preloader,
    # services.exe, winedevice.exe) in one process group. killpg catches
    # them ALL — even after the launcher exits, children keep the PGID.
    # Without this, orphaned services.exe / winedevice.exe survive and
    # corrupt the prefix on the next run.
    try:
        pgid = os.getpgid(game_proc.pid)
        os.killpg(pgid, signal.SIGTERM)
        time.sleep(1)
        os.killpg(pgid, signal.SIGKILL)
    except (ProcessLookupError, PermissionError, OSError):
        pass
    if game_proc.poll() is None:
        game_proc.kill()
    game_proc.wait()
    time.sleep(2)

    # Stop montauk gracefully
    montauk_proc.send_signal(signal.SIGINT)
    try:
        montauk_proc.wait(timeout=5)
    except subprocess.TimeoutExpired:
        montauk_proc.kill()

    kill_all()

    # Find ALL .prom snapshots and use the LATEST one with actual trace data
    prom_files = sorted(trace_dir.glob("montauk_*.prom"), key=lambda f: f.stat().st_mtime)
    best = None
    for pf in reversed(prom_files):
        text = pf.read_text(errors="replace")
        if "montauk_trace_process_info" in text and "group_size 0" not in text:
            best = pf
            break
    if best:
        shutil.copy2(best, trace_dir / "final.prom")
        log_info(f"[{label}] snapshot={best.name} size={best.stat().st_size // 1024}K")
    elif prom_files:
        shutil.copy2(prom_files[-1], trace_dir / "final.prom")
        log_warn(f"[{label}] using last snapshot (no active trace data found)")
    else:
        log_warn(f"[{label}] no trace snapshots captured")

    log_info(f"[{label}] total snapshots={len(prom_files)}")
    return trace_dir


def parse_trace_prom(trace_dir):
    """Parse montauk v5.1.0 trace .prom files for processes, threads, fds, and I/O."""
    final = trace_dir / "final.prom"
    if not final.exists():
        return {"processes": {}, "threads": {}, "fds": {}, "io": []}

    text = final.read_text(errors="replace")
    data = {"processes": {}, "threads": {}, "fds": {}, "io": []}

    for line in text.splitlines():
        if line.startswith("#") or not line.strip():
            continue

        # montauk_trace_process_info{pid="X",ppid="Y",cmd="Z",root="R"} 1
        m = re.match(r'montauk_trace_process_info\{.*pid="(\d+)".*ppid="(\d+)".*cmd="([^"]*)".*root="([^"]*)"', line)
        if m:
            pid, ppid, cmd, root = m.groups()
            data["processes"][pid] = {"ppid": ppid, "cmd": cmd, "root": root}
            continue

        # montauk_trace_thread_syscall{pid="X",tid="Y",comm="Z",syscall="S",wchan="W"} N
        m = re.match(r'montauk_trace_thread_syscall\{.*pid="(\d+)".*tid="(\d+)".*comm="([^"]*)".*syscall="([^"]*)".*wchan="([^"]*)"', line)
        if m:
            pid, tid, comm, syscall, wchan = m.groups()
            data["threads"][f"{pid}/{tid}"] = {"pid": pid, "comm": comm, "syscall": syscall, "wchan": wchan}
            continue

        # montauk_trace_fd_target{pid="X",fd="Y",target="Z"} 1
        m = re.match(r'montauk_trace_fd_target\{.*pid="(\d+)".*fd="(\d+)".*target="([^"]*)"', line)
        if m:
            pid, fd, target = m.groups()
            data["fds"][f"{pid}/{fd}"] = target
            continue

        # montauk_trace_thread_io{pid="X",tid="Y",comm="Z",syscall="S",fd="F",count="C",result="R",whence="W"} TS
        m = re.match(r'montauk_trace_thread_io\{.*pid="(\d+)".*tid="(\d+)".*comm="([^"]*)".*syscall="([^"]*)".*fd="([^"]*)".*count="([^"]*)".*result="([^"]*)".*whence="([^"]*)"', line)
        if m:
            pid, tid, comm, syscall, fd, count, result, whence = m.groups()
            data["io"].append({
                "pid": pid, "tid": tid, "comm": comm, "syscall": syscall,
                "fd": fd, "count": count, "result": result, "whence": whence,
            })
            continue

    return data


def fd_is_interesting(target):
    """Check if an fd target is relevant to our diagnosis."""
    lower = target.lower()
    return any(f in lower for f in INTERESTING_FILES)


def analyze_fds(data, label):
    """Show file descriptors related to Balatro/LÖVE."""
    log_info(f"[{label}] interesting fds:")
    found = {}
    for key, target in sorted(data["fds"].items()):
        if fd_is_interesting(target):
            log_info(f"  {key}: {target}")
            found[key] = target
    if not found:
        log_info("  (none found)")
    log_info(f"[{label}] total tracked fds={len(data['fds'])}")
    return found


def analyze_io(data, label):
    """Show file I/O operations — the core of the diagnosis."""
    log_info(f"[{label}] file I/O operations ({len(data['io'])} captured)")

    if not data["io"]:
        log_info("  (no I/O captured)")
        return

    # Build fd → filename map
    fd_names = {}
    for key, target in data["fds"].items():
        pid, fd = key.split("/")
        fd_names[(pid, fd)] = target

    # Group I/O by fd, show interesting ones
    io_by_fd = defaultdict(list)
    for op in data["io"]:
        fd_key = (op["pid"], op["fd"])
        io_by_fd[fd_key].append(op)

    # Show I/O on interesting fds
    interesting_io = False
    for fd_key, ops in sorted(io_by_fd.items()):
        pid, fd = fd_key
        target = fd_names.get(fd_key, "(unknown)")
        if fd_is_interesting(target) or any(fd_is_interesting(fd_names.get((o["pid"], o["fd"]), "")) for o in ops):
            interesting_io = True
            log_info(f"  fd={fd} pid={pid} target={target}")
            for op in ops:
                sc = op["syscall"]
                count = op["count"]
                result = op["result"]
                whence = op["whence"]
                if sc == "lseek":
                    whence_name = {0: "SEEK_SET", 1: "SEEK_CUR", 2: "SEEK_END"}.get(int(whence) if whence else -1, f"whence={whence}")
                    log_info(f"    {sc} fd={fd} offset={count} {whence_name} -> {result}")
                elif sc in ("read", "pread64"):
                    offset_str = f" offset={whence}" if sc == "pread64" and whence != "0" else ""
                    log_info(f"    {sc} fd={fd} count={count}{offset_str} -> {result} bytes")
                elif sc == "write":
                    log_info(f"    {sc} fd={fd} count={count} -> {result} bytes")
                elif sc in ("newfstat", "fstat"):
                    log_info(f"    fstat fd={fd} -> result={result} st_size={count}")
                else:
                    log_info(f"    {sc} fd={fd} count={count} result={result}")

    if not interesting_io:
        log_info("  no Balatro-specific I/O found, showing all operations:")
        for op in data["io"][:20]:
            target = fd_names.get((op["pid"], op["fd"]), "?")
            short_target = target.split("/")[-1] if "/" in target else target
            log_info(f"    pid={op['pid']} tid={op['tid']} {op['syscall']} fd={op['fd']} target={short_target} count={op['count']} result={op['result']}")
        if len(data["io"]) > 20:
            log_info(f"    ... and {len(data['io']) - 20} more")


def analyze_threads(data, label):
    """Show thread states for Wine/game processes."""
    log_info(f"[{label}] traced processes={len(data['processes'])}")
    for pid, info in sorted(data["processes"].items()):
        log_info(f"  pid={pid} ppid={info['ppid']} cmd={info['cmd']} root={info['root']}")

    log_info(f"[{label}] thread states ({len(data['threads'])} threads)")
    for key, info in sorted(data["threads"].items()):
        comm = info["comm"]
        if comm in ("montauk", "montauk-bpf"):
            continue
        log_info(f"  {key} comm={comm:20s} syscall={info['syscall']:30s} wchan={info['wchan']}")


def diff_io(stock_data, quark_data):
    """Compare I/O operations between stock and quark — the money shot."""
    log_info("I/O diff: stock vs triskelion")

    # Build fd → filename maps
    def fd_map(data):
        m = {}
        for key, target in data["fds"].items():
            pid, fd = key.split("/")
            m[(pid, fd)] = target
        return m

    stock_fds = fd_map(stock_data)
    quark_fds = fd_map(quark_data)

    stock_bal_fds = {k: v for k, v in stock_fds.items() if "balatro" in v.lower()}
    quark_bal_fds = {k: v for k, v in quark_fds.items() if "balatro" in v.lower()}

    log_info(f"Balatro.exe open fds: stock={len(stock_bal_fds)} quark={len(quark_bal_fds)}")
    for k, v in stock_bal_fds.items():
        log_info(f"  stock pid={k[0]} fd={k[1]} target={v}")
    for k, v in quark_bal_fds.items():
        log_info(f"  quark  pid={k[0]} fd={k[1]} target={v}")

    def io_on_file(data, fd_map, filename_match):
        ops = []
        for op in data["io"]:
            target = fd_map.get((op["pid"], op["fd"]), "")
            if filename_match in target.lower():
                ops.append(op)
        return ops

    stock_bal_io = io_on_file(stock_data, stock_fds, "balatro")
    quark_bal_io = io_on_file(quark_data, quark_fds, "balatro")

    def fmt_op(op):
        sc = op["syscall"]
        if sc == "lseek":
            wn = {0: "SET", 1: "CUR", 2: "END"}.get(int(op["whence"]) if op["whence"] else -1, op["whence"])
            return f"{sc} offset={op['count']} {wn} -> {op['result']}"
        if sc in ("read", "pread64"):
            return f"{sc} count={op['count']} -> {op['result']} bytes"
        if sc in ("newfstat", "fstat"):
            return f"fstat -> result={op['result']} st_size={op['count']}"
        return f"{sc} count={op['count']} -> {op['result']}"

    log_info(f"Balatro.exe I/O ops: stock={len(stock_bal_io)} quark={len(quark_bal_io)}")
    for op in stock_bal_io[:15]:
        log_info(f"  stock {fmt_op(op)}")
    for op in quark_bal_io[:15]:
        log_info(f"  quark  {fmt_op(op)}")

    # The verdict
    if stock_bal_io and not quark_bal_io:
        log_warn("verdict: stock has I/O on Balatro.exe, quark has NONE")
        log_warn("  PhysFS never reads the exe under triskelion - file not opened or fd not tracked")
    elif not stock_bal_io and not quark_bal_io:
        log_warn("verdict: neither has I/O on Balatro.exe in this snapshot")
        log_warn("  the I/O may have completed before the snapshot. Try --timeout 30.")
    elif stock_bal_io and quark_bal_io:
        stock_results = [(o["syscall"], o["count"], o["result"]) for o in stock_bal_io]
        quark_results = [(o["syscall"], o["count"], o["result"]) for o in quark_bal_io]
        if stock_results == quark_results:
            log_info("verdict: I/O sequences MATCH. PhysFS does the same thing in both runs.")
            log_info("  bug is elsewhere (not in file I/O on the exe).")
        else:
            log_warn("verdict: I/O sequences DIFFER. This is the bug.")
            log_warn("  compare the operations above to find the divergence.")


def main():
    if os.geteuid() != 0:
        log_error("this script requires sudo (montauk --trace needs CAP_SYS_ADMIN)")
        log_error("usage: sudo python3 tests/trace_fused_exe.py")
        sys.exit(1)

    if not MONTAUK.exists():
        log_error(f"montauk not found at {MONTAUK}")
        sys.exit(1)

    if not GAME_EXE.exists():
        log_error(f"Balatro not found at {GAME_EXE}")
        sys.exit(1)

    skip_stock = "--skip-stock" in sys.argv
    skip_quark = "--skip-quark" in sys.argv

    if TRACE_DIR.exists():
        shutil.rmtree(TRACE_DIR)
    TRACE_DIR.mkdir(parents=True)

    log_info("LÖVE fused-exe trace: stock proton vs triskelion")
    log_info(f"  game={GAME_EXE.name} size={GAME_EXE.stat().st_size / 1024 / 1024:.1f}MB")
    log_info(f"  prefix={PFX}")
    log_info(f"  timeout={TIMEOUT}s per run")
    log_info(f"  montauk={MONTAUK}")

    stock_data = {"processes": {}, "threads": {}, "fds": {}, "io": []}
    quark_data = {"processes": {}, "threads": {}, "fds": {}, "io": []}

    if not skip_stock:
        log_info("phase: stock wineserver")
        stock_dir = run_trace("stock", STOCK_TRACE, None, use_stock_proton=True)
        stock_data = parse_trace_prom(stock_dir)
        analyze_fds(stock_data, "stock")
        analyze_io(stock_data, "stock")
        analyze_threads(stock_data, "stock")

    if not skip_quark:
        log_info("phase: triskelion (quark)")
        quark_dir = run_trace("quark", QUARK_TRACE, QUARK_DIR, use_stock_proton=False)
        quark_data = parse_trace_prom(quark_dir)
        analyze_fds(quark_data, "quark")
        analyze_io(quark_data, "quark")
        analyze_threads(quark_data, "quark")

    if not skip_stock and not skip_quark:
        diff_io(stock_data, quark_data)

    log_info("trace files:")
    for label, d in [("stock", STOCK_TRACE), ("quark", QUARK_TRACE)]:
        if d.exists():
            log_info(f"  {label}: {d}/")
            for f in sorted(d.glob("*")):
                log_info(f"    {f.name} ({f.stat().st_size // 1024}K)")


if __name__ == "__main__":
    main()
