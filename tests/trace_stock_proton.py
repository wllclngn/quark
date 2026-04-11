#!/usr/bin/env python3
"""Trace stock Proton launches for EVERY installed Steam game.

For each game, captures `WINEDEBUG=+server` output (every wineserver
request/reply pair) using Proton 10.0 — the same Wine source quark targets.
This is the ground-truth dataset of what each game actually asks the
wineserver to do, so we can diff against triskelion's coverage and find
exactly what's missing per-game.

Output layout:
    /tmp/quark/trace/all/
        master_summary.txt           (cross-game request frequency totals)
        master_index.txt             (one line per game: appid, name, status, requests)
        <appid>_<slug>/
            wine_server_trace.log    (full +server trace stderr)
            summary.txt              (per-game request frequency)
            stdout.log               (game stdout)

Usage:
    python3 tests/trace_stock_proton.py                  # All games, 30s each
    python3 tests/trace_stock_proton.py --timeout 45     # Longer per-game
    python3 tests/trace_stock_proton.py --appid 2379780  # Single game (legacy)
    python3 tests/trace_stock_proton.py --games 2379780,2218750  # Subset

NOTE: Stock Proton uses its OWN bundled Wine, NOT system Wine. The traces
captured here are the reference for "what a working Wine-based stack does"
when the system Wine has regressions. Compare against quark's daemon.log
to find ABI / opcode / lifecycle drift.
"""

import argparse
import os
import re
import signal
import subprocess
import sys
import time
from collections import Counter
from datetime import datetime
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from util import _USER_HOME, STEAM_ROOT, get_game_exe


def _ts() -> str:
    return datetime.now().strftime("[%H:%M:%S]")

def log_info(msg: str) -> None:
    print(f"{_ts()} [INFO]   {msg}", flush=True)

def log_warn(msg: str) -> None:
    print(f"{_ts()} [WARN]   {msg}", flush=True)

def log_error(msg: str) -> None:
    print(f"{_ts()} [ERROR]  {msg}", flush=True)

STEAMAPPS = STEAM_ROOT / "steamapps"
# Proton 10.0 — matches install.py's PROTON_GIT_TAG (proton-10.0-4)
PROTON_DIR = STEAMAPPS / "common" / "Proton 10.0"
PROTON_BIN = PROTON_DIR / "proton"
TRACE_ROOT = Path("/tmp/quark/trace/all")
INDEX_FILE = TRACE_ROOT / "master_index.txt"
MASTER_SUMMARY = TRACE_ROOT / "master_summary.txt"


# Non-game filters: skip Steam internals, runtimes, Proton itself

NON_GAME_NAMES = {
    "steam linux runtime", "proton", "steamworks common redistributables",
    "proton easyanticheat runtime", "proton hotfix", "proton experimental",
    "proton 9.0", "proton 10.0",
}

NON_GAME_APPIDS = {
    "228980", "1070560", "1391110", "1493710", "1628350",
    "1826330", "2180100", "2348590", "2805730", "3658110",
    "1580130", "1887720",
}


def discover_games():
    """Find every playable installed Steam game.

    Returns [(appid, name, install_dir, exe_path), ...] sorted by appid.
    Filters out runtimes, Proton versions, and games whose exe can't be
    located via get_game_exe.
    """
    games = []
    seen = set()
    for manifest in sorted(STEAMAPPS.glob("appmanifest_*.acf")):
        try:
            text = manifest.read_text(errors="replace")
        except OSError:
            continue
        info = {}
        for key in ("appid", "name", "installdir"):
            m = re.search(rf'"{key}"\s+"([^"]+)"', text)
            if m:
                info[key] = m.group(1)
        appid = info.get("appid", "")
        name = info.get("name", "")
        installdir = info.get("installdir", "")
        if not appid or not name or not installdir:
            continue
        if appid in seen or appid in NON_GAME_APPIDS:
            continue
        if any(skip in name.lower() for skip in NON_GAME_NAMES):
            continue
        install_path = STEAMAPPS / "common" / installdir
        if not install_path.exists():
            continue
        exe = get_game_exe(appid)
        if not exe:
            continue
        seen.add(appid)
        games.append((appid, name, install_path, exe))
    games.sort(key=lambda g: int(g[0]) if g[0].isdigit() else 0)
    return games


def slugify(name):
    """Lowercase, dash-separated, alphanumeric-only — safe for dir names."""
    s = re.sub(r"[^a-zA-Z0-9]+", "_", name).strip("_").lower()
    return s or "unknown"


def kill_pgroup(proc):
    """Kill the entire process group spawned by proc.

    Stock Proton's children (wineserver, wine-preloader, services.exe,
    winedevice.exe) all inherit the process group. SIGTERM the group,
    wait briefly, then SIGKILL. Without this, orphaned services.exe
    survives and corrupts the prefix.
    """
    try:
        pgid = os.getpgid(proc.pid)
        os.killpg(pgid, signal.SIGTERM)
        time.sleep(1.5)
        os.killpg(pgid, signal.SIGKILL)
    except (ProcessLookupError, PermissionError, OSError):
        pass
    if proc.poll() is None:
        proc.kill()
    proc.wait()


def trace_one_game(appid, name, exe, timeout, debug_flags):
    """Launch one game through stock Proton, capture +server trace.

    Returns (status, request_count, dir_path) where status is one of
    "ok", "launch_fail", "timeout", "exit_<code>".
    """
    slug = slugify(name)
    out_dir = TRACE_ROOT / f"{appid}_{slug}"
    out_dir.mkdir(parents=True, exist_ok=True)
    trace_log = out_dir / "wine_server_trace.log"
    stdout_log = out_dir / "stdout.log"
    summary_path = out_dir / "summary.txt"

    compat_data = STEAMAPPS / "compatdata" / appid
    env = os.environ.copy()
    env["STEAM_COMPAT_DATA_PATH"] = str(compat_data)
    env["STEAM_COMPAT_CLIENT_INSTALL_PATH"] = str(STEAM_ROOT)
    env["SteamAppId"] = appid
    env["SteamGameId"] = appid
    env["WINEDEBUG"] = debug_flags
    env["HOME"] = str(_USER_HOME)
    for var in ("DISPLAY", "WAYLAND_DISPLAY", "XDG_RUNTIME_DIR", "XAUTHORITY"):
        if var in os.environ:
            env[var] = os.environ[var]

    log_info(f"trace start: appid={appid} name=\"{name}\"")
    log_info(f"  exe={exe}")
    log_info(f"  out={out_dir}")

    t0 = time.monotonic()
    stderr_file = open(trace_log, "w")
    stdout_file = open(stdout_log, "w")
    try:
        proc = subprocess.Popen(
            [str(PROTON_BIN), "run", str(exe)],
            env=env,
            cwd="/tmp",
            stdout=stdout_file,
            stderr=stderr_file,
            start_new_session=True,
        )
    except OSError as e:
        stderr_file.close()
        stdout_file.close()
        log_error(f"launch failed: {e}")
        return ("launch_fail", 0, out_dir)

    status = "timeout"
    exit_code = None
    try:
        for tick in range(timeout):
            time.sleep(1)
            ret = proc.poll()
            if ret is not None:
                exit_code = ret
                status = f"exit_{ret}" if ret != 0 else "exit_0"
                break
            if tick > 0 and tick % 10 == 0:
                size_kb = trace_log.stat().st_size // 1024 if trace_log.exists() else 0
                log_info(f"tick={tick}s trace={size_kb}KB")
    except KeyboardInterrupt:
        log_warn("interrupted, killing process group")
        kill_pgroup(proc)
        stderr_file.close()
        stdout_file.close()
        raise

    if proc.poll() is None:
        kill_pgroup(proc)
    stderr_file.close()
    stdout_file.close()

    # Post-process for request frequency
    counts = Counter()
    line_count = 0
    try:
        with open(trace_log, "r", errors="replace") as f:
            for line in f:
                line_count += 1
                if ": " in line and "(" in line:
                    parts = line.split(": ", 1)
                    if len(parts) == 2:
                        req_part = parts[1].strip()
                        paren = req_part.find("(")
                        if paren > 0:
                            req = req_part[:paren].strip()
                            if req.replace("_", "").isalpha() and len(req) < 40:
                                counts[req] += 1
    except OSError:
        pass

    total_requests = sum(counts.values())
    elapsed = time.monotonic() - t0

    # Per-game summary file: key=value metadata block, blank line, then
    # `count opcode` data rows. The blank line is the section delimiter
    # (no decorative separators per project convention).
    with open(summary_path, "w") as f:
        f.write(f"appid={appid}\n")
        f.write(f"name={name}\n")
        f.write(f"exe={exe}\n")
        f.write(f"status={status}\n")
        f.write(f"elapsed_s={elapsed:.1f}\n")
        f.write(f"trace_lines={line_count}\n")
        f.write(f"total_requests={total_requests}\n")
        f.write(f"unique_requests={len(counts)}\n")
        f.write("\n")
        for req, n in counts.most_common():
            f.write(f"{n:8d}  {req}\n")

    trace_kb = trace_log.stat().st_size // 1024 if trace_log.exists() else 0
    log_info(f"trace done: status={status} requests={total_requests} "
             f"unique={len(counts)} trace={trace_kb}KB elapsed={elapsed:.0f}s")
    return (status, total_requests, out_dir)


def write_master_summary(results, debug_flags):
    """Aggregate per-game data into one cross-game summary."""
    TRACE_ROOT.mkdir(parents=True, exist_ok=True)
    totals = Counter()
    games_using = Counter()
    for appid, name, status, requests, out_dir in results:
        sumfile = out_dir / "summary.txt"
        if not sumfile.exists():
            continue
        # Per-game summary uses a blank-line delimiter between metadata and
        # the opcode data block. Skip until the blank line, then parse rows.
        in_data = False
        for line in sumfile.read_text(errors="replace").splitlines():
            if not in_data:
                if line == "":
                    in_data = True
                continue
            parts = line.split(maxsplit=1)
            if len(parts) == 2 and parts[0].isdigit():
                req = parts[1].strip()
                totals[req] += int(parts[0])
                games_using[req] += 1

    with open(INDEX_FILE, "w") as f:
        f.write(f"# Stock Proton trace index. WINEDEBUG={debug_flags}\n")
        f.write(f"# {'appid':<10} {'status':<14} {'requests':>10}  name\n")
        for appid, name, status, requests, _ in results:
            f.write(f"  {appid:<10} {status:<14} {requests:>10}  {name}\n")

    with open(MASTER_SUMMARY, "w") as f:
        f.write(f"# Cross-game wineserver request frequency (stock Proton 10.0).\n")
        f.write(f"# WINEDEBUG={debug_flags}, {len(results)} games traced.\n")
        f.write(f"# {'count':>10}  {'games':>5}  request\n")
        for req, count in totals.most_common():
            f.write(f"  {count:>10}  {games_using[req]:>5}  {req}\n")


def main():
    parser = argparse.ArgumentParser(description="Trace stock Proton on every installed game")
    parser.add_argument("--timeout", type=int, default=30, help="Per-game timeout in seconds")
    parser.add_argument("--appid", type=str, default=None, help="Single game appid (legacy)")
    parser.add_argument("--games", type=str, default=None, help="Comma-separated list of appids")
    parser.add_argument("--full-debug", action="store_true",
                        help="Use +server,+module,+loaddll,+process (very verbose)")
    args = parser.parse_args()

    if not PROTON_BIN.exists():
        log_error(f"Proton 10.0 not found at {PROTON_BIN}")
        log_error("install via Steam: Library > Tools > 'Proton 10.0'")
        sys.exit(1)

    debug_flags = "+server,+module,+loaddll,+process" if args.full_debug else "+server"
    TRACE_ROOT.mkdir(parents=True, exist_ok=True)

    all_games = discover_games()
    if args.appid:
        all_games = [g for g in all_games if g[0] == args.appid]
    elif args.games:
        wanted = set(args.games.split(","))
        all_games = [g for g in all_games if g[0] in wanted]

    if not all_games:
        log_error("no games to trace")
        sys.exit(1)

    log_info(f"trace plan: {len(all_games)} games via {PROTON_DIR}")
    log_info(f"  WINEDEBUG={debug_flags} timeout={args.timeout}s output={TRACE_ROOT}")
    for appid, name, _, _ in all_games:
        log_info(f"  {appid:<10} {name}")

    results = []
    for appid, name, install_dir, exe in all_games:
        try:
            status, requests, out_dir = trace_one_game(appid, name, exe, args.timeout, debug_flags)
            results.append((appid, name, status, requests, out_dir))
        except KeyboardInterrupt:
            log_warn("interrupted, writing partial summary")
            break

    write_master_summary(results, debug_flags)

    # Final per-game summary table — column-aligned data, no banners.
    log_info(f"summary ({len(results)} games)")
    log_info(f"  {'appid':<10} {'status':<14} {'requests':>10}  name")
    for appid, name, status, requests, _ in results:
        log_info(f"  {appid:<10} {status:<14} {requests:>10}  {name}")
    log_info(f"index={INDEX_FILE}")
    log_info(f"totals={MASTER_SUMMARY}")
    log_info(f"per_game={TRACE_ROOT}/<appid>_<slug>/")


if __name__ == "__main__":
    main()
