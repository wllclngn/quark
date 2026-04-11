#!/usr/bin/env python3
"""Side-by-side montauk comparison: stock Proton vs quark.

Uses montauk v5.2.0 --trace --log to capture Prometheus-format snapshots
with per-thread state, syscall, and I/O details. Parses .prom files for
a structured diff of process trees, thread states, syscall profiles, fd
tables, and file I/O patterns.

Runs as user. Only montauk needs sudo (for BPF) -- prompted automatically.
Stock phase launches through Proton 10.0 (not bare wine).

Usage:
    python3 tests/montauk_compare.py                    # Both runs, 30s each
    python3 tests/montauk_compare.py --stock-only       # Stock Proton only
    python3 tests/montauk_compare.py --quark-only        # Quark only
    python3 tests/montauk_compare.py --timeout 45       # Longer capture
    python3 tests/montauk_compare.py --appid 2320       # Different game
    python3 tests/montauk_compare.py --all              # ALL games, quark-only, Prometheus output
    python3 tests/montauk_compare.py --all --timeout 30 # ALL games, 30s each
"""

import argparse
import os
import re
import subprocess
import sys
import time
from collections import Counter, defaultdict
from pathlib import Path

from datetime import datetime

sys.path.insert(0, str(Path(__file__).resolve().parent))
from util import (kill_quark_processes, get_game_exe, STEAM_ROOT, _USER_HOME)


# Non-game filters for --all mode

_NON_GAME_NAMES = {
    "steam linux runtime", "proton", "steamworks common redistributables",
    "proton easyanticheat runtime", "proton hotfix", "proton experimental",
}

_NON_GAME_APPIDS = {
    "228980", "1070560", "1391110", "1493710", "1628350",
    "1826330", "2180100", "2348590", "2805730", "3658110",
    "1580130", "1887720",
}


def discover_all_games() -> list[tuple[str, str]]:
    """Find all playable Steam games. Returns [(appid, name), ...] sorted by name."""
    steamapps = STEAM_ROOT / "steamapps"
    games: list[tuple[str, str]] = []
    seen: set[str] = set()

    for manifest in sorted(steamapps.glob("appmanifest_*.acf")):
        try:
            text = manifest.read_text(errors="replace")
        except OSError:
            continue
        info: dict[str, str] = {}
        for key in ("appid", "name", "installdir"):
            m = re.search(rf'"{key}"\s+"([^"]+)"', text)
            if m:
                info[key] = m.group(1)

        appid = info.get("appid", "")
        name = info.get("name", "")
        installdir = info.get("installdir", "")
        if not appid or not name or not installdir:
            continue
        if appid in seen or appid in _NON_GAME_APPIDS:
            continue
        if any(skip in name.lower() for skip in _NON_GAME_NAMES):
            continue
        install_path = steamapps / "common" / installdir
        if not install_path.exists():
            continue
        exe = get_game_exe(appid)
        if not exe:
            continue
        seen.add(appid)
        games.append((appid, name))

    games.sort(key=lambda g: g[1])
    return games

def _timestamp() -> str:
    return datetime.now().strftime("[%H:%M:%S]")

def log_info(msg: str) -> None:
    print(f"{_timestamp()} [INFO]   {msg}", flush=True)

def log_warn(msg: str) -> None:
    print(f"{_timestamp()} [WARN]   {msg}", flush=True)

def log_error(msg: str) -> None:
    print(f"{_timestamp()} [ERROR]  {msg}", flush=True)

STEAMAPPS = STEAM_ROOT / "steamapps"
COMPAT_DIR = STEAM_ROOT / "compatibilitytools.d/quark"
PROTON_DIR = STEAMAPPS / "common" / "Proton 10.0"
PROTON_BIN = PROTON_DIR / "proton"
ITERATE_PY = Path(__file__).resolve().parent / "iterate.py"

OUT_DIR = Path("/tmp/quark/montauk_compare")

# Prometheus metric names we parse from .prom log files
METRIC_PROCESS  = "montauk_trace_process_info"
METRIC_STATE    = "montauk_trace_thread_state"
METRIC_SYSCALL  = "montauk_trace_thread_syscall"
METRIC_IO       = "montauk_trace_thread_io"
METRIC_FD       = "montauk_trace_fd_target"
METRIC_GROUP    = "montauk_trace_group_size"
METRIC_THREADS  = "montauk_trace_thread_total"


# Prometheus line parser

def parse_labels(label_str):
    """Parse Prometheus label set: key="val",key2="val2" -> dict."""
    labels = {}
    i = 0
    n = len(label_str)
    while i < n:
        eq = label_str.find("=", i)
        if eq < 0:
            break
        key = label_str[i:eq]
        # value starts after ="
        if eq + 1 < n and label_str[eq + 1] == '"':
            # find closing quote (handle escaped quotes)
            j = eq + 2
            val_parts = []
            while j < n:
                c = label_str[j]
                if c == '\\' and j + 1 < n:
                    val_parts.append(label_str[j + 1])
                    j += 2
                elif c == '"':
                    break
                else:
                    val_parts.append(c)
                    j += 1
            labels[key] = "".join(val_parts)
            # skip past closing quote and comma
            i = j + 1
            if i < n and label_str[i] == ',':
                i += 1
        else:
            i = eq + 1
    return labels


def parse_prom_line(line):
    """Parse a single Prometheus exposition line.

    Returns (metric_name, labels_dict, value_str) or None for comments/blanks.
    """
    line = line.strip()
    if not line or line.startswith("#"):
        return None

    # metric{labels} value
    brace = line.find("{")
    if brace >= 0:
        name = line[:brace]
        close = line.rfind("}")
        if close < 0:
            return None
        label_str = line[brace + 1:close]
        value = line[close + 1:].strip()
        return (name, parse_labels(label_str), value)

    # metric value (no labels)
    parts = line.split(None, 1)
    if len(parts) == 2:
        return (parts[0], {}, parts[1])
    return None


# Scrape block parser: each block starts with "# montauk_scrape_timestamp_ms"

def parse_prom_file(filepath):
    """Parse a .prom log file into a list of scrape blocks.

    Each block is a dict: {timestamp_ms: int, lines: [(name, labels, value)]}
    """
    blocks = []
    current = None
    try:
        with open(filepath) as f:
            for line in f:
                if line.startswith("# montauk_scrape_timestamp_ms "):
                    if current:
                        blocks.append(current)
                    ts = int(line.split()[-1])
                    current = {"timestamp_ms": ts, "lines": []}
                elif current is not None:
                    parsed = parse_prom_line(line)
                    if parsed:
                        current["lines"].append(parsed)
        if current:
            blocks.append(current)
    except OSError:
        pass
    return blocks


# Extract structured trace data from scrape blocks

class TraceSnapshot:
    """One point-in-time trace observation."""
    __slots__ = ("timestamp_ms", "procs", "threads", "syscalls", "io_events", "fds",
                 "group_size", "thread_total")

    def __init__(self, timestamp_ms=0):
        self.timestamp_ms = timestamp_ms
        self.procs = []      # [{pid, ppid, cmd, root}]
        self.threads = []    # [{pid, tid, comm, state}]
        self.syscalls = []   # [{pid, tid, comm, syscall, wchan, nr}]
        self.io_events = []  # [{pid, tid, comm, syscall, fd, count, result, whence, ts_ns}]
        self.fds = []        # [{pid, fd, target}]
        self.group_size = 0
        self.thread_total = 0


def extract_snapshots(blocks):
    """Convert parsed .prom blocks into TraceSnapshot objects."""
    snapshots = []
    for block in blocks:
        snap = TraceSnapshot(block["timestamp_ms"])
        for name, labels, value in block["lines"]:
            if name == METRIC_PROCESS:
                snap.procs.append({
                    "pid": labels.get("pid", ""),
                    "ppid": labels.get("ppid", ""),
                    "cmd": labels.get("cmd", ""),
                    "root": labels.get("root", "0"),
                })
            elif name == METRIC_STATE:
                snap.threads.append({
                    "pid": labels.get("pid", ""),
                    "tid": labels.get("tid", ""),
                    "comm": labels.get("comm", ""),
                    "state": labels.get("state", "?"),
                })
            elif name == METRIC_SYSCALL:
                snap.syscalls.append({
                    "pid": labels.get("pid", ""),
                    "tid": labels.get("tid", ""),
                    "comm": labels.get("comm", ""),
                    "syscall": labels.get("syscall", ""),
                    "wchan": labels.get("wchan", ""),
                    "nr": value,
                })
            elif name == METRIC_IO:
                snap.io_events.append({
                    "pid": labels.get("pid", ""),
                    "tid": labels.get("tid", ""),
                    "comm": labels.get("comm", ""),
                    "syscall": labels.get("syscall", ""),
                    "fd": labels.get("fd", ""),
                    "count": labels.get("count", ""),
                    "result": labels.get("result", ""),
                    "whence": labels.get("whence", ""),
                    "ts_ns": value,
                })
            elif name == METRIC_FD:
                snap.fds.append({
                    "pid": labels.get("pid", ""),
                    "fd": labels.get("fd", ""),
                    "target": labels.get("target", ""),
                })
            elif name == METRIC_GROUP:
                snap.group_size = int(value)
            elif name == METRIC_THREADS:
                snap.thread_total = int(value)
        if snap.group_size > 0:
            snapshots.append(snap)
    return snapshots


# Launch helpers

def kill_all():
    """Kill quark processes only. Never touch Proton or stock Wine state."""
    kill_quark_processes()
    # Only clean triskelion SHM -- never wine-* (that's Proton's)
    subprocess.run("rm -f /dev/shm/triskelion-* 2>/dev/null",
                   shell=True, capture_output=True)


def kill_stale_wineserver(appid):
    """Kill orphaned wineserver for a specific prefix and clean stale lock.

    Only targets the wineserver bound to this appid's prefix socket dir.
    Safe: checks by prefix inode, not by process name.
    """
    compat_data = STEAMAPPS / "compatdata" / str(appid)
    pfx = compat_data / "pfx"
    if not pfx.exists():
        return
    try:
        st = pfx.stat()
        server_dir = Path(f"/tmp/.wine-{os.getuid()}/server-{st.st_dev:x}-{st.st_ino:x}")
        lock_file = server_dir / "lock"
        if not lock_file.exists():
            return
        # Check if a process holds the lock
        result = subprocess.run(["fuser", str(lock_file)],
                                capture_output=True, text=True, timeout=3)
        if result.returncode == 0 and result.stdout.strip():
            # Process holds the lock -- kill it
            for pid_str in result.stdout.strip().split():
                try:
                    pid = int(pid_str.strip().rstrip('e').rstrip('f'))
                    os.kill(pid, 9)
                except (ValueError, ProcessLookupError, PermissionError):
                    pass
            time.sleep(1)
        # Remove stale lock and socket
        if lock_file.exists():
            result = subprocess.run(["fuser", str(lock_file)],
                                    capture_output=True, timeout=3)
            if result.returncode != 0:
                lock_file.unlink(missing_ok=True)
                socket_file = server_dir / "socket"
                socket_file.unlink(missing_ok=True)
    except (OSError, subprocess.TimeoutExpired):
        pass


def find_prom_files(log_dir):
    """Find all .prom files in a directory, sorted by name."""
    d = Path(log_dir)
    if not d.exists():
        return []
    return sorted(d.glob("*.prom"))


def run_stock_wine(timeout, appid, trace_pattern):
    """Launch game via stock Proton (stock wineserver) with montauk tracing.

    Script runs as user. Only montauk needs sudo (for BPF).
    Game launches through Proton -- handles prefix, display, env.
    """
    log_info(f"stock proton [{trace_pattern}]")

    kill_all()
    log_dir = OUT_DIR / "stock_logs"
    log_dir.mkdir(parents=True, exist_ok=True)
    for f in log_dir.glob("*.prom"):
        f.unlink()

    if not PROTON_BIN.exists():
        log_error(f"proton not found at {PROTON_BIN}")
        return log_dir

    game_exe = get_game_exe(appid)
    if not game_exe:
        log_error(f"no exe found for appid {appid}")
        return log_dir

    stderr_log = OUT_DIR / "stock_montauk_stderr.txt"

    # Start montauk with sudo (needs BPF). Script itself runs as user.
    montauk_cmd = ["sudo", "montauk",
                   "--trace", trace_pattern,
                   "--log", str(log_dir),
                   "--log-interval-ms", "400"]
    log_info(f"  montauk: {' '.join(montauk_cmd)}")
    montauk_proc = subprocess.Popen(
        montauk_cmd,
        stdout=subprocess.DEVNULL,
        stderr=open(stderr_log, "w"),
    )
    time.sleep(1)

    # Launch through Proton -- same pattern as stock_compare.py
    compat_data = STEAMAPPS / "compatdata" / str(appid)
    env = os.environ.copy()
    env["STEAM_COMPAT_DATA_PATH"] = str(compat_data)
    env["STEAM_COMPAT_CLIENT_INSTALL_PATH"] = str(STEAM_ROOT)
    env["SteamAppId"] = str(appid)
    env["SteamGameId"] = str(appid)
    env["WINEDEBUG"] = "-all"
    # Ensure stock wineserver -- remove any quark overrides
    env.pop("WINESERVER", None)
    env.pop("QUARK_VERBOSE", None)
    # Display vars
    for var in ("DISPLAY", "WAYLAND_DISPLAY", "XDG_RUNTIME_DIR", "XAUTHORITY"):
        if var in os.environ:
            env[var] = os.environ[var]

    wine_log = OUT_DIR / "stock_wine_stderr.txt"
    game_cmd = [str(PROTON_BIN), "run", str(game_exe)]
    log_info(f"  proton run: {game_exe.name}")
    game_proc = subprocess.Popen(
        game_cmd, env=env, cwd="/tmp",
        stdout=subprocess.DEVNULL,
        stderr=open(wine_log, "w"),
    )

    for i in range(timeout):
        time.sleep(1)
        if game_proc.poll() is not None:
            log_info(f"game exited after {i+1}s (code {game_proc.returncode})")
            break
        if i > 0 and i % 10 == 0:
            proms = find_prom_files(log_dir)
            total_kb = sum(f.stat().st_size for f in proms) // 1024
            log_info(f"  [{i}s] {len(proms)} .prom files, {total_kb}KB")
    else:
        log_warn(f"timeout ({timeout}s), killing...")

    game_proc.terminate()
    try:
        game_proc.wait(timeout=3)
    except subprocess.TimeoutExpired:
        game_proc.kill()

    # Shut down the stock wineserver Proton spawned. Without this it stays
    # alive, holds the prefix lock, and blocks the next Steam launch.
    proton_wineserver = PROTON_DIR / "files/bin/wineserver"
    if proton_wineserver.exists():
        subprocess.run([str(proton_wineserver), "-k"],
                       env=env, capture_output=True, timeout=5)
    time.sleep(2)

    # Clean stale wineserver lock for this prefix. wineserver -k should
    # handle this, but if the process was killed hard the lock file remains
    # and blocks the next Steam launch.
    try:
        import stat as stat_mod
        pfx = compat_data / "pfx"
        if pfx.exists():
            st = pfx.stat()
            server_dir = Path(f"/tmp/.wine-{os.getuid()}/server-{st.st_dev:x}-{st.st_ino:x}")
            lock_file = server_dir / "lock"
            if lock_file.exists():
                # Only remove if no process holds it
                result = subprocess.run(["fuser", str(lock_file)],
                                        capture_output=True, timeout=3)
                if result.returncode != 0:
                    lock_file.unlink()
    except (OSError, subprocess.TimeoutExpired):
        pass

    montauk_proc.terminate()
    try:
        montauk_proc.wait(timeout=3)
    except subprocess.TimeoutExpired:
        montauk_proc.kill()

    proms = find_prom_files(log_dir)
    log_info(f"captured {len(proms)} scrape files in {log_dir}")
    log_info(f"  stderr: {stderr_log}")
    return log_dir


def run_quark(timeout, appid, trace_pattern):
    """Launch game via iterate.py with montauk tracing.

    Script runs as user. Only montauk needs sudo (for BPF).
    """
    log_info(f"quark [{trace_pattern}]")

    kill_all()
    log_dir = OUT_DIR / "quark_logs"
    log_dir.mkdir(parents=True, exist_ok=True)
    for f in log_dir.glob("*.prom"):
        f.unlink()

    stderr_log = OUT_DIR / "quark_montauk_stderr.txt"

    montauk_cmd = ["sudo", "montauk",
                   "--trace", trace_pattern,
                   "--log", str(log_dir),
                   "--log-interval-ms", "400"]
    log_info(f"  montauk: {' '.join(montauk_cmd)}")
    montauk_proc = subprocess.Popen(
        montauk_cmd,
        stdout=subprocess.DEVNULL,
        stderr=open(stderr_log, "w"),
    )
    time.sleep(1)

    iter_log = OUT_DIR / "iterate_output.txt"
    iter_cmd = [sys.executable, str(ITERATE_PY),
                "--skip-build", f"--timeout={timeout}", f"--appid={appid}"]
    log_info(f"  iterate: {' '.join(iter_cmd)}")
    iter_proc = subprocess.Popen(
        iter_cmd,
        stdout=open(iter_log, "w"),
        stderr=subprocess.STDOUT,
        cwd=str(ITERATE_PY.parent.parent),
    )

    for i in range(timeout + 15):
        time.sleep(1)
        if iter_proc.poll() is not None:
            log_info(f"iterate.py exited after {i+1}s (code {iter_proc.returncode})")
            break
        if i > 0 and i % 10 == 0:
            proms = find_prom_files(log_dir)
            total_kb = sum(f.stat().st_size for f in proms) // 1024
            log_info(f"  [{i}s] {len(proms)} .prom files, {total_kb}KB")
    else:
        log_warn("timeout, killing...")
        iter_proc.kill()

    try:
        iter_proc.wait(timeout=5)
    except subprocess.TimeoutExpired:
        iter_proc.kill()

    time.sleep(2)
    montauk_proc.terminate()
    try:
        montauk_proc.wait(timeout=3)
    except subprocess.TimeoutExpired:
        montauk_proc.kill()

    kill_all()
    kill_stale_wineserver(appid)

    proms = find_prom_files(log_dir)
    log_info(f"captured {len(proms)} scrape files in {log_dir}")
    log_info(f"  stderr: {stderr_log}")
    log_info(f"  iterate log: {iter_log}")
    return log_dir


# Analysis

def load_trace(log_dir):
    """Load all .prom files from a log directory into TraceSnapshots."""
    snapshots = []
    for prom_file in find_prom_files(log_dir):
        blocks = parse_prom_file(prom_file)
        snapshots.extend(extract_snapshots(blocks))
    return snapshots


def process_names(snapshots):
    """Unique process command names across all snapshots."""
    names = set()
    for snap in snapshots:
        for p in snap.procs:
            cmd = p["cmd"].strip()
            if cmd:
                names.add(cmd)
    return names


def thread_state_summary(snapshots):
    """Per-comm summary: sample count, dominant state, dominant syscall."""
    by_comm = defaultdict(lambda: {"samples": 0, "states": Counter(), "syscalls": Counter()})
    for snap in snapshots:
        seen = set()
        for th in snap.threads:
            comm = th["comm"].strip() or "?"
            if comm in seen:
                continue
            seen.add(comm)
            entry = by_comm[comm]
            entry["samples"] += 1
            entry["states"][th["state"]] += 1
        for sc in snap.syscalls:
            comm = sc["comm"].strip() or "?"
            syscall = sc["syscall"].strip() or "?"
            by_comm[comm]["syscalls"][syscall] += 1
    return by_comm


def io_summary(snapshots):
    """Per-comm I/O summary: fd usage, read/write/seek patterns."""
    by_comm = defaultdict(lambda: {"events": 0, "fds": Counter(), "syscalls": Counter(),
                                    "total_bytes": 0, "samples": []})
    for snap in snapshots:
        for io in snap.io_events:
            comm = io["comm"].strip() or "?"
            entry = by_comm[comm]
            entry["events"] += 1
            entry["fds"][io["fd"]] += 1
            entry["syscalls"][io["syscall"]] += 1
            try:
                result = int(io["result"])
                if result > 0:
                    entry["total_bytes"] += result
            except (ValueError, TypeError):
                pass
            entry["samples"].append(io)
    return by_comm


def fd_summary(snapshots):
    """Per-process fd targets, aggregated across snapshots."""
    targets = defaultdict(lambda: defaultdict(set))
    for snap in snapshots:
        for fd in snap.fds:
            pid = fd["pid"]
            targets[pid][fd["fd"]].add(fd["target"])
    return targets


def print_thread_table(label, summary):
    """Print thread state table for one run."""
    log_info(f"thread summary ({label})")
    if not summary:
        print("  (no threads)")
        return
    print(f"  {'COMM':<24s} {'SAMPLES':>7s}  {'STATE':>5s}  TOP SYSCALL")
    for comm in sorted(summary, key=lambda c: summary[c]["samples"], reverse=True):
        entry = summary[comm]
        top_state = entry["states"].most_common(1)
        top_sc = entry["syscalls"].most_common(1)
        state_str = top_state[0][0] if top_state else "?"
        sc_str = f"{top_sc[0][0]}({top_sc[0][1]})" if top_sc else "-"
        print(f"  {comm:<24s} {entry['samples']:>7d}  {state_str:>5s}  {sc_str}")


def print_io_table(label, summary):
    """Print I/O summary for one run."""
    log_info(f"i/o summary ({label})")
    if not summary:
        print("  (no I/O captured)")
        return
    print(f"  {'COMM':<24s} {'EVENTS':>7s}  {'BYTES':>10s}  FDS              TOP SYSCALL")
    for comm in sorted(summary, key=lambda c: summary[c]["events"], reverse=True):
        entry = summary[comm]
        top_fds = entry["fds"].most_common(3)
        fd_str = ",".join(f"fd{f}({n})" for f, n in top_fds)
        top_sc = entry["syscalls"].most_common(1)
        sc_str = f"{top_sc[0][0]}({top_sc[0][1]})" if top_sc else "-"
        print(f"  {comm:<24s} {entry['events']:>7d}  {entry['total_bytes']:>10d}  {fd_str:<17s}{sc_str}")

    # Detail: last I/O sample per comm (most recent snapshot activity)
    log_info(f"i/o detail (last observation per thread)")
    print(f"  {'COMM':<20s} {'SYSCALL':<10s} {'FD':>4s} {'COUNT':>10s} {'RESULT':>10s} {'WHENCE':>6s}")
    for comm in sorted(summary, key=lambda c: summary[c]["events"], reverse=True)[:15]:
        samples = summary[comm]["samples"]
        if not samples:
            continue
        s = samples[-1]
        whence_names = {"0": "SET", "1": "CUR", "2": "END"}
        w = whence_names.get(s["whence"], s["whence"])
        print(f"  {comm:<20s} {s['syscall']:<10s} {s['fd']:>4s} {s['count']:>10s} {s['result']:>10s} {w:>6s}")


def print_fd_table(label, fd_map):
    """Print fd target summary for one run."""
    log_info(f"fd targets ({label})")
    if not fd_map:
        print("  (no fd data)")
        return
    print(f"  {'PID':<8s} {'FD':>4s}  TARGET")
    count = 0
    for pid in sorted(fd_map):
        for fd_num in sorted(fd_map[pid]):
            for target in sorted(fd_map[pid][fd_num]):
                if count >= 40:
                    remaining = sum(len(fds) for fds in fd_map[pid].values()) - count
                    if remaining > 0:
                        print(f"  ... +{remaining} more")
                    return
                print(f"  {pid:<8s} {fd_num:>4s}  {target}")
                count += 1


def diff_traces(stock_dir, quark_dir):
    """Full structured comparison between stock and quark traces."""
    log_info("comparison: stock wine vs quark")

    stock_snaps = load_trace(stock_dir)
    quark_snaps = load_trace(quark_dir)

    log_info(f"stock: {len(stock_snaps)} snapshots, quark: {len(quark_snaps)} snapshots")

    # Process names
    stock_procs = process_names(stock_snaps)
    quark_procs = process_names(quark_snaps)

    log_info("process names")
    print(f"  stock only:       {sorted(stock_procs - quark_procs) or '(none)'}")
    print(f"  quark only: {sorted(quark_procs - stock_procs) or '(none)'}")
    print(f"  both:             {sorted(stock_procs & quark_procs) or '(none)'}")

    # Thread state
    stock_threads = thread_state_summary(stock_snaps)
    quark_threads = thread_state_summary(quark_snaps)
    print_thread_table("Stock Wine", stock_threads)
    print_thread_table("Quark", quark_threads)

    # Thread differences
    stock_comms = set(stock_threads.keys())
    quark_comms = set(quark_threads.keys())
    log_info("thread differences")
    print(f"  stock only:       {sorted(stock_comms - quark_comms) or '(none)'}")
    print(f"  quark only: {sorted(quark_comms - stock_comms) or '(none)'}")

    # I/O comparison
    stock_io = io_summary(stock_snaps)
    quark_io = io_summary(quark_snaps)
    print_io_table("Stock Wine", stock_io)
    print_io_table("Quark", quark_io)

    # I/O differences: comms that do I/O in one but not the other
    stock_io_comms = set(stock_io.keys())
    quark_io_comms = set(quark_io.keys())
    if stock_io_comms != quark_io_comms:
        log_info("i/o thread differences")
        diff_stock = stock_io_comms - quark_io_comms
        diff_quark = quark_io_comms - stock_io_comms
        if diff_stock:
            print(f"  i/o in stock only:       {sorted(diff_stock)}")
        if diff_quark:
            print(f"  i/o in quark only: {sorted(diff_quark)}")

    # FD tables
    stock_fds = fd_summary(stock_snaps)
    quark_fds = fd_summary(quark_snaps)
    print_fd_table("Stock Wine", stock_fds)
    print_fd_table("Quark", quark_fds)

    # Peak snapshot (most threads)
    for label, snaps in [("Stock Wine", stock_snaps), ("Quark", quark_snaps)]:
        if snaps:
            peak = max(snaps, key=lambda s: len(s.threads))
            log_info(f"peak snapshot: {label} ({len(peak.threads)} threads, "
                     f"{len(peak.procs)} procs, {len(peak.io_events)} I/O events)")
            for p in peak.procs[:10]:
                root_tag = " [root]" if p["root"] == "1" else ""
                print(f"  PID {p['pid']:<8s} ppid={p['ppid']:<8s} {p['cmd']}{root_tag}")


def print_single(label, log_dir):
    """Print analysis for a single run (no comparison)."""
    snaps = load_trace(log_dir)
    log_info(f"{label}: {len(snaps)} snapshots")
    threads = thread_state_summary(snaps)
    print_thread_table(label, threads)
    io = io_summary(snaps)
    print_io_table(label, io)
    fds = fd_summary(snaps)
    print_fd_table(label, fds)


def run_all_games(timeout, prom_path):
    """Run quark trace for ALL installed games and write Prometheus output."""
    games = discover_all_games()
    if not games:
        log_error("no playable games found")
        return

    log_info(f"found {len(games)} games to trace")
    for appid, name in games:
        log_info(f"  {appid:>8}  {name}")
    print()

    all_results: list[dict] = []
    ts = int(time.time() * 1000)

    for i, (appid, name) in enumerate(games, 1):
        print()
        log_info(f"GAME {i}/{len(games)}: {name} ({appid})")

        kill_all()
        kill_stale_wineserver(appid)

        game_dir = OUT_DIR / f"all_{appid}"
        game_dir.mkdir(parents=True, exist_ok=True)

        quark_dir = run_quark(timeout, appid, "wine")
        snaps = load_trace(quark_dir)

        # Collect results
        result = {
            "appid": appid, "name": name,
            "snapshots": len(snaps),
            "peak_threads": max((len(s.threads) for s in snaps), default=0),
            "peak_procs": max((len(s.procs) for s in snaps), default=0),
            "total_io": sum(len(s.io_events) for s in snaps),
            "comms": set(),
            "syscalls": Counter(),
        }
        for snap in snaps:
            for th in snap.threads:
                result["comms"].add(th["comm"])
            for sc in snap.syscalls:
                result["syscalls"][sc["syscall"]] += 1
        all_results.append(result)

        # Per-game .prom in game_dir
        _write_game_prom(game_dir / f"{appid}.prom", result, snaps, ts)

        log_info(f"  {len(snaps)} snapshots, peak {result['peak_threads']} threads, "
                 f"{result['total_io']} I/O events")

        # Also read iterate.py opcode stats if available
        opcode_stats = Path("/tmp/quark/triskelion_opcode_stats.txt")
        if opcode_stats.exists():
            import shutil
            shutil.copy2(opcode_stats, game_dir / "opcode_stats.txt")

        daemon_log = Path("/tmp/quark/daemon.log")
        if daemon_log.exists():
            import shutil
            shutil.copy2(daemon_log, game_dir / "daemon.log")

    # Write combined Prometheus output
    _write_combined_prom(prom_path, all_results, ts)

    # Summary table
    print()
    
    print("  quark montauk diagnostic — all games")
    
    print(f"  {'Game':<35} {'Snaps':>6} {'Threads':>8} {'Procs':>6} {'I/O':>7} {'Comms'}")
    
    for r in all_results:
        name = r["name"][:33] + ".." if len(r["name"]) > 35 else r["name"]
        comms = ", ".join(sorted(r["comms"])[:5])
        if len(r["comms"]) > 5:
            comms += f" +{len(r['comms']) - 5}"
        print(f"  {name:<35} {r['snapshots']:>6} {r['peak_threads']:>8} "
              f"{r['peak_procs']:>6} {r['total_io']:>7} {comms}")
    
    print(f"  {len(all_results)} games | Prometheus: {prom_path}")
    
    print()


def _escape_prom(s: str) -> str:
    return s.replace("\\", "\\\\").replace('"', '\\"').replace("\n", "\\n")


def _write_game_prom(path, result, snaps, ts):
    """Write per-game Prometheus metrics."""
    lines = []
    appid = result["appid"]
    name = _escape_prom(result["name"])

    lines.append(f'# game: {result["name"]} ({appid})')
    lines.append(f'quark_trace_snapshots{{appid="{appid}",game="{name}"}} {result["snapshots"]} {ts}')
    lines.append(f'quark_trace_peak_threads{{appid="{appid}",game="{name}"}} {result["peak_threads"]} {ts}')
    lines.append(f'quark_trace_peak_procs{{appid="{appid}",game="{name}"}} {result["peak_procs"]} {ts}')
    lines.append(f'quark_trace_io_events{{appid="{appid}",game="{name}"}} {result["total_io"]} {ts}')

    for syscall, count in result["syscalls"].most_common(20):
        lines.append(f'quark_trace_syscall{{appid="{appid}",game="{name}",syscall="{syscall}"}} {count} {ts}')

    path.write_text("\n".join(lines) + "\n")


def _write_combined_prom(path, all_results, ts):
    """Write combined Prometheus metrics for all games."""
    lines = []
    lines.append("# HELP quark_trace_snapshots Montauk trace snapshots captured")
    lines.append("# TYPE quark_trace_snapshots gauge")
    for r in all_results:
        name = _escape_prom(r["name"])
        lines.append(f'quark_trace_snapshots{{appid="{r["appid"]}",game="{name}"}} {r["snapshots"]} {ts}')

    lines.append("")
    lines.append("# HELP quark_trace_peak_threads Peak thread count observed")
    lines.append("# TYPE quark_trace_peak_threads gauge")
    for r in all_results:
        name = _escape_prom(r["name"])
        lines.append(f'quark_trace_peak_threads{{appid="{r["appid"]}",game="{name}"}} {r["peak_threads"]} {ts}')

    lines.append("")
    lines.append("# HELP quark_trace_peak_procs Peak process count observed")
    lines.append("# TYPE quark_trace_peak_procs gauge")
    for r in all_results:
        name = _escape_prom(r["name"])
        lines.append(f'quark_trace_peak_procs{{appid="{r["appid"]}",game="{name}"}} {r["peak_procs"]} {ts}')

    lines.append("")
    lines.append("# HELP quark_trace_io_events Total I/O events captured")
    lines.append("# TYPE quark_trace_io_events gauge")
    for r in all_results:
        name = _escape_prom(r["name"])
        lines.append(f'quark_trace_io_events{{appid="{r["appid"]}",game="{name}"}} {r["total_io"]} {ts}')

    lines.append("")
    lines.append("# HELP quark_trace_syscall Per-game syscall counts")
    lines.append("# TYPE quark_trace_syscall gauge")
    for r in all_results:
        name = _escape_prom(r["name"])
        for syscall, count in r["syscalls"].most_common(20):
            lines.append(f'quark_trace_syscall{{appid="{r["appid"]}",game="{name}",syscall="{syscall}"}} {count} {ts}')

    path.write_text("\n".join(lines) + "\n")


def main():
    parser = argparse.ArgumentParser(description="montauk eBPF trace comparison (v5.2.0)")
    parser.add_argument("--timeout", type=int, default=30)
    parser.add_argument("--stock-only", action="store_true")
    parser.add_argument("--quark-only", action="store_true")
    parser.add_argument("--all", action="store_true",
                        help="Trace ALL installed games (quark only, Prometheus output)")
    parser.add_argument("--appid", default="2379780", help="Steam app ID (default: Balatro)")
    parser.add_argument("--pattern", default=None,
                        help="montauk --trace pattern (default: auto from appid)")
    args = parser.parse_args()

    if subprocess.run(["which", "montauk"], capture_output=True).returncode != 0:
        log_error("montauk not found in PATH")
        sys.exit(1)

    log_info("authenticating sudo (needed for montauk eBPF)")
    if subprocess.run(["sudo", "-v"]).returncode != 0:
        log_error("sudo authentication failed")
        sys.exit(1)

    OUT_DIR.mkdir(parents=True, exist_ok=True)

    # --all mode: trace every game, quark only, Prometheus output
    if args.all:
        prom_path = OUT_DIR / f"quark_all_{datetime.now().strftime('%Y%m%d_%H%M%S')}.prom"
        run_all_games(args.timeout, prom_path)
        return

    # Single-game mode (original behavior)
    trace_pattern = args.pattern
    if not trace_pattern:
        game_exe = get_game_exe(args.appid)
        if game_exe:
            trace_pattern = game_exe.stem.lower()
        else:
            trace_pattern = "wine"

    kill_all()
    kill_stale_wineserver(args.appid)

    stock_dir = None
    quark_dir = None

    if not args.quark_only:
        stock_dir = run_stock_wine(args.timeout, args.appid, "wine")

    if not args.stock_only:
        quark_dir = run_quark(args.timeout, args.appid, "wine")

    if stock_dir and quark_dir:
        diff_traces(stock_dir, quark_dir)
    elif stock_dir:
        print_single("Stock Wine", stock_dir)
    elif quark_dir:
        print_single("Quark", quark_dir)

    log_info("files:")
    if stock_dir:
        print(f"  stock logs:  {stock_dir}")
    if quark_dir:
        print(f"  amph logs:   {quark_dir}")
    print(f"  output dir:  {OUT_DIR}")


if __name__ == "__main__":
    main()
