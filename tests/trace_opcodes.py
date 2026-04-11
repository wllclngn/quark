#!/usr/bin/env python3
"""
Automated opcode trace: launches a game through REAL Proton (not quark),
captures the full WINEDEBUG=+server protocol trace, parses it, and reports
exactly which opcodes triskelion needs to implement.

Usage:
  python3 tests/trace_opcodes.py                  # Trace Balatro (default)
  python3 tests/trace_opcodes.py --appid 1145360  # Trace Hades
  python3 tests/trace_opcodes.py --analyze /path   # Just parse existing trace
  python3 tests/trace_opcodes.py --timeout 30      # Custom timeout (default 20s)

What it does (fully automated):
  1. Finds the game exe from Steam's appmanifest
  2. Finds a real Proton installation (Proton Experimental or Proton 10.0)
  3. Kills any stale wineserver/wine processes
  4. Launches the game with WINEDEBUG=+server, capturing stderr
  5. Waits for startup (configurable timeout)
  6. Kills the game and wineserver
  7. Parses the trace
  8. Reports: handled vs missing opcodes, priority order, coverage
"""

import os
import re
import signal
import subprocess
import sys
import time
from collections import Counter, OrderedDict
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from util import kill_quark_processes, STEAM_ROOT, COMPAT_DIR_STR

# ── Paths ────────────────────────────────────────────────────────────────────

SCRIPT_DIR = Path(__file__).parent.resolve()
PROJECT_DIR = SCRIPT_DIR.parent
EVENT_LOOP_RS = PROJECT_DIR / "rust" / "src" / "event_loop.rs"

STEAMAPPS = STEAM_ROOT / "steamapps"
COMPAT_TOOLS = STEAM_ROOT / "compatibilitytools.d"

TRACE_FILE = Path("/tmp/triskelion-opcode-trace.log")

DEFAULT_APPID = "2379780"  # Balatro


# ── Steam / Proton discovery ────────────────────────────────────────────────

def find_game_exe(app_id):
    """Find the main .exe for a Steam game from its appmanifest."""
    manifest = STEAMAPPS / f"appmanifest_{app_id}.acf"
    if not manifest.exists():
        die(f"No appmanifest for {app_id} at {manifest}")

    install_dir = None
    name = None
    for line in manifest.read_text().splitlines():
        line = line.strip()
        if line.startswith('"installdir"'):
            install_dir = line.split('"')[3]
        if line.startswith('"name"'):
            name = line.split('"')[3]

    if not install_dir:
        die(f"No installdir in {manifest}")

    game_dir = STEAMAPPS / "common" / install_dir
    if not game_dir.exists():
        die(f"Game dir not found: {game_dir}")

    # Find exe files, prefer ones matching the game name
    exes = sorted(game_dir.glob("*.exe"))
    if not exes:
        # Try one level deeper
        exes = sorted(game_dir.glob("*/*.exe"))
    if not exes:
        die(f"No .exe found in {game_dir}")

    # Prefer exact name match
    for exe in exes:
        if install_dir.lower() in exe.stem.lower():
            return exe, name or install_dir

    return exes[0], name or install_dir


def find_proton():
    """Find a real Proton installation (not quark)."""
    candidates = [
        STEAMAPPS / "common" / "Proton - Experimental",
        STEAMAPPS / "common" / "Proton 10.0",
        STEAMAPPS / "common" / "Proton 9.0-4",
        STEAMAPPS / "common" / "Proton 9.0-3",
    ]
    # Also check compatibilitytools.d for GE-Proton
    if COMPAT_TOOLS.exists():
        for d in sorted(COMPAT_TOOLS.iterdir(), reverse=True):
            if d.is_dir() and "proton" in d.name.lower() and "quark" not in d.name.lower():
                candidates.insert(0, d)

    for p in candidates:
        proton_script = p / "proton"
        if proton_script.exists():
            return p, proton_script

    die("No Proton installation found. Install Proton Experimental from Steam.")


def find_implemented_handlers():
    """Grep event_loop.rs for fn handle_xxx overrides."""
    handlers = set()
    if not EVENT_LOOP_RS.exists():
        return handlers
    text = EVENT_LOOP_RS.read_text()
    for m in re.finditer(r'fn handle_(\w+)\(&mut self, _?client_fd: i32', text):
        handlers.add(m.group(1))
    return handlers


# ── Process management ───────────────────────────────────────────────────────

def kill_wine_processes():
    """Kill all wine/wineserver processes. Returns after they're dead."""
    kill_quark_processes()


def find_wine_pids():
    """Find wine PIDs scoped to quark's compat tool directory.

    Read-only monitoring -- never used for kill decisions.
    """
    pids = []
    try:
        r = subprocess.run(["pgrep", "-a", "-f", COMPAT_DIR_STR],
                           capture_output=True, text=True, timeout=5)
        for line in r.stdout.strip().splitlines():
            if line.strip():
                pids.append(int(line.split()[0]))
    except (subprocess.TimeoutExpired, ValueError):
        pass
    return pids


# ── Launch + capture ─────────────────────────────────────────────────────────

def launch_and_trace(app_id, timeout_secs):
    """Launch game through real Proton with WINEDEBUG, capture trace, kill."""
    game_exe, game_name = find_game_exe(app_id)
    proton_dir, proton_script = find_proton()

    log("INFO", f"Game:   {game_name} ({app_id})")
    log("INFO", f"Exe:    {game_exe}")
    log("INFO", f"Proton: {proton_dir.name}")
    log("INFO", f"Trace:  {TRACE_FILE}")
    log("INFO", f"Timeout: {timeout_secs}s")
    print()

    # Kill any stale wine processes
    log("INFO", "Killing stale wine processes...")
    kill_wine_processes()

    # Clean stale sockets
    uid = os.getuid()
    socket_dir = Path(f"/tmp/.wine-{uid}")
    if socket_dir.exists():
        for sock in socket_dir.glob("server-*/socket"):
            try:
                sock.unlink()
            except OSError:
                pass

    # Remove old trace
    if TRACE_FILE.exists():
        TRACE_FILE.unlink()

    # Build environment for real Proton launch
    compat_data = STEAMAPPS / "compatdata" / app_id
    if not compat_data.exists():
        compat_data.mkdir(parents=True)

    env = os.environ.copy()
    env["STEAM_COMPAT_DATA_PATH"] = str(compat_data)
    env["STEAM_COMPAT_CLIENT_INSTALL_PATH"] = str(STEAM_ROOT)
    env["SteamAppId"] = app_id
    env["SteamGameId"] = app_id
    env["WINEDEBUG"] = "+server"
    # Disable DXVK/VKD3D noise
    env["DXVK_LOG_LEVEL"] = "none"
    env["VKD3D_DEBUG"] = "none"
    env["WINEESYNC"] = "0"  # Avoid esync noise
    env["WINEFSYNC"] = "0"  # Avoid fsync noise
    env["PROTON_USE_NTSYNC"] = "1"  # Use ntsync kernel module
    env["WINENTSYNC"] = "1"         # Enable ntsync in Wine

    # Launch Proton with stderr captured to trace file
    log("INFO", f"Launching {game_name}...")
    trace_fd = open(TRACE_FILE, "w")
    proc = subprocess.Popen(
        [sys.executable, str(proton_script), "run", str(game_exe)],
        env=env,
        stdout=subprocess.DEVNULL,
        stderr=trace_fd,
        cwd=str(game_exe.parent),
    )

    # Wait for game to start and generate trace data
    start = time.time()
    last_size = 0
    stable_count = 0
    log("INFO", f"Waiting up to {timeout_secs}s for startup trace...")

    while time.time() - start < timeout_secs:
        time.sleep(2)
        elapsed = int(time.time() - start)

        # Check trace file growth
        try:
            size = TRACE_FILE.stat().st_size
        except FileNotFoundError:
            size = 0

        wine_pids = find_wine_pids()
        growth = size - last_size

        if elapsed % 4 == 0 or growth == 0:
            log("INFO", f"  [{elapsed}s] trace={size//1024}KB  +{growth//1024}KB  "
                        f"wine_pids={len(wine_pids)}")

        # If trace stopped growing for 3 checks (6s), startup is likely done
        if size > 0 and growth == 0:
            stable_count += 1
            if stable_count >= 3:
                log("INFO", f"  Trace stable for 6s — startup complete")
                break
        else:
            stable_count = 0

        last_size = size

        # If the process died and no wine processes remain, we're done
        if proc.poll() is not None and not wine_pids:
            log("INFO", f"  Process exited (code={proc.returncode})")
            break

    # Kill everything
    log("INFO", "Killing game and wineserver...")
    proc.terminate()
    try:
        proc.wait(timeout=5)
    except subprocess.TimeoutExpired:
        proc.kill()
    trace_fd.close()

    kill_wine_processes()

    # Check result
    if not TRACE_FILE.exists() or TRACE_FILE.stat().st_size == 0:
        die("No trace data captured. Proton may have failed to start.")

    size = TRACE_FILE.stat().st_size
    log("OK", f"Trace captured: {size//1024}KB")
    return TRACE_FILE


# ── Parse WINEDEBUG=+server trace ───────────────────────────────────────────

# Wine's +server trace format (WINEDEBUG=+server):
#   0024: init_first_thread( unix_pid=12345, ... )
#   0024: init_first_thread() = 0 { pid=0020, ... }
#   0024: open_key() = OBJECT_NAME_NOT_FOUND { ... }
# Call line: thread hex, colon, space, opcode, open paren with args
# Return line: thread hex, colon, space, opcode, () = status
CALL_RE = re.compile(r'^([0-9a-f]+): (\w+)\(')
RET_RE  = re.compile(r'^[0-9a-f]+: (\w+)\(\) = (\S+)')

# Skip non-opcode lines (fd passing, internal messages)
SKIP_RE = re.compile(r'^[0-9a-f]+: \*fd\*')


def parse_trace(path):
    """Parse trace file → (calls, counts, errors, first_seen, thread_set)."""
    calls = []
    counts = Counter()
    errors = {}       # opcode -> Counter of non-zero statuses
    first_seen = OrderedDict()
    threads = set()

    with open(path, errors='replace') as f:
        for line in f:
            if SKIP_RE.match(line):
                continue

            m = CALL_RE.match(line)
            if m:
                tid, op = m.group(1), m.group(2)
                calls.append((op, tid))
                counts[op] += 1
                threads.add(tid)
                if op not in first_seen:
                    first_seen[op] = len(calls)
                continue

            m = RET_RE.match(line)
            if m:
                op, status = m.group(1), m.group(2)
                if status != '0':
                    errors.setdefault(op, Counter())[status] += 1

    return calls, counts, errors, first_seen, threads


# ── Report ───────────────────────────────────────────────────────────────────

def report(calls, counts, errors, first_seen, threads, implemented):
    total = len(calls)
    unique = len(counts)

    print()
    print("=" * 76)
    print(f"  WINE SERVER OPCODE TRACE")
    print(f"  {total} calls, {unique} unique opcodes, {len(threads)} threads")
    print("=" * 76)

    handled = {op: n for op, n in counts.items() if op in implemented}
    missing = {op: n for op, n in counts.items() if op not in implemented}

    # ── Handled ──
    print(f"\n  HANDLED by triskelion ({len(handled)}/{unique} opcodes, "
          f"{sum(handled.values())}/{total} calls):")
    for op, n in sorted(handled.items(), key=lambda x: -x[1]):
        errs = errors.get(op, {})
        err_str = ""
        if errs:
            top = sorted(errs.items(), key=lambda x: -x[1])[:3]
            err_str = "  errs=" + ",".join(f"{v}x{k}" for k, v in top)
        print(f"    {op:<42} {n:>6}x{err_str}")

    # ── Missing — sorted by first-seen ──
    print(f"\n  MISSING — need implementation ({len(missing)} opcodes, "
          f"{sum(missing.values())} calls):")
    print(f"  {'#':>4}  {'opcode':<42} {'count':>6}  {'first':>6}")
    print(f"  {'─'*4}  {'─'*42} {'─'*6}  {'─'*6}")

    missing_sorted = sorted(missing.items(),
                            key=lambda x: first_seen.get(x[0], 99999))
    for i, (op, n) in enumerate(missing_sorted, 1):
        fs = first_seen.get(op, '?')
        print(f"  {i:>4}. {op:<42} {n:>6}x  #{fs:<6}")

    # ── Critical early ops ──
    early_missing = []
    seen = set()
    for op, _tid in calls[:100]:
        if op not in seen and op not in implemented:
            early_missing.append(op)
            seen.add(op)

    if early_missing:
        print(f"\n  CRITICAL — unimplemented ops in first 100 calls (Wine dies here):")
        for op in early_missing:
            n = counts[op]
            fs = first_seen[op]
            print(f"    {op:<42} {n:>6}x total, first at #{fs}")

    # ── Top 20 ──
    print(f"\n  TOP 20 by volume:")
    for op, n in counts.most_common(20):
        status = "✓" if op in implemented else "✗"
        pct = n / total * 100
        print(f"    {status} {op:<42} {n:>6}x  {pct:>5.1f}%")

    # ── Summary ──
    print()
    print("─" * 76)
    cov_ops = len(handled)
    cov_calls = sum(handled.values())
    print(f"  Opcode coverage:  {cov_ops}/{unique} "
          f"({100*cov_ops//unique if unique else 0}%)")
    print(f"  Call coverage:    {cov_calls}/{total} "
          f"({100*cov_calls//total if total else 0}%)")
    print(f"  Missing opcodes:  {len(missing)}")
    print(f"  Missing calls:    {sum(missing.values())}")
    print("─" * 76)
    print()


# ── Utilities ────────────────────────────────────────────────────────────────

def log(level, msg):
    colors = {"INFO": "\033[36m", "OK": "\033[32m", "WARN": "\033[33m",
              "ERR": "\033[31m"}
    reset = "\033[0m"
    c = colors.get(level, "")
    print(f"{c}[{level}]{reset} {msg}")


def die(msg):
    log("ERR", msg)
    sys.exit(1)


# ── Main ─────────────────────────────────────────────────────────────────────

def main():
    import argparse
    p = argparse.ArgumentParser(description=__doc__,
                                formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("--appid", default=DEFAULT_APPID, help="Steam app ID (default: Balatro)")
    p.add_argument("--timeout", type=int, default=20, help="Seconds to wait for startup (default: 20)")
    p.add_argument("--analyze", metavar="FILE", help="Just parse an existing trace file")
    args = p.parse_args()

    implemented = find_implemented_handlers()
    print(f"triskelion handlers: {len(implemented)} implemented")

    if args.analyze:
        trace = Path(args.analyze)
        if not trace.exists():
            die(f"File not found: {trace}")
    else:
        trace = launch_and_trace(args.appid, args.timeout)

    log("INFO", f"Parsing {trace.name} ({trace.stat().st_size//1024}KB)...")
    calls, counts, errors, first_seen, threads = parse_trace(trace)

    if not calls:
        die(f"No server calls found in {trace}. Check WINEDEBUG=+server is set.")

    report(calls, counts, errors, first_seen, threads, implemented)


if __name__ == "__main__":
    main()
