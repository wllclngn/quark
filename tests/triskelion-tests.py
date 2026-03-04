#!/usr/bin/env python3
"""
triskelion integration test.

Validates that triskelion can serve as wineserver for real games.
This is the bridge between unit-level handler work and
performance benchmarking.

Subcommands:
    test-deploy     Deploy triskelion binary to Steam compat tools
    test-launch     Launch game through triskelion, validate startup
    test-survive    Launch + sustained survival check (longer timeout)
    test-profile    Launch + capture opcode profile for analysis
    test-iterate    Deploy + launch + on failure, report missing handlers
    test-compare    Profile multiple games sequentially, diff timing + opcodes
    test-all        Full pipeline: deploy, launch, survive, profile

Usage:
    ./tests/triskelion-tests.py test-deploy
    ./tests/triskelion-tests.py test-launch --app-id 2218750
    ./tests/triskelion-tests.py test-launch                   # defaults to Halls of Torment
    ./tests/triskelion-tests.py test-survive --timeout 120
    ./tests/triskelion-tests.py test-profile
    ./tests/triskelion-tests.py test-compare --game hot --game hades --game gw2
    ./tests/triskelion-tests.py test-iterate
    ./tests/triskelion-tests.py test-all
"""

import argparse
import io
import json
import os
import re
import signal
import subprocess
import sys
import time
from datetime import datetime
from pathlib import Path

SCRIPT_DIR = Path(__file__).parent.resolve()
PROJECT_DIR = SCRIPT_DIR.parent
RUST_DIR = PROJECT_DIR / "amphetamine"

STEAM_ROOT = Path.home() / ".steam" / "root"
STEAMAPPS = STEAM_ROOT / "steamapps"
COMPAT_DIR = STEAM_ROOT / "compatibilitytools.d" / "amphetamine"
BIN_DIR = COMPAT_DIR / "files" / "bin"

TRISKELION_BIN = PROJECT_DIR / "target" / "release" / "triskelion"
TMP_DIR = Path("/tmp/amphetamine")
STATS_FILE = TMP_DIR / "triskelion_opcode_stats.txt"
LOG_DIR = Path.home() / ".cache" / "amphetamine"
LOG_FILE = TMP_DIR / "triskelion-tests.log"
RESULTS_DIR = TMP_DIR / "triskelion-results"

# Default test game
DEFAULT_APP_ID = "2218750"
DEFAULT_GAME = "Halls of Torment"

# Known games for test-compare
KNOWN_GAMES = {
    "2218750": "Halls of Torment",
    "1145360": "Hades",
    "1284210": "Guild Wars 2",
}

GAME_ALIASES = {
    "hot": "2218750",
    "halls": "2218750",
    "hades": "1145360",
    "gw2": "1284210",
    "guildwars": "1284210",
}


def resolve_game(arg):
    """Resolve a game argument to (app_id, name). Accepts app ID, alias, or name."""
    # Direct app ID
    if arg in KNOWN_GAMES:
        return arg, KNOWN_GAMES[arg]
    # Alias lookup (case-insensitive)
    key = arg.lower().replace(" ", "").replace("-", "")
    if key in GAME_ALIASES:
        app_id = GAME_ALIASES[key]
        return app_id, KNOWN_GAMES[app_id]
    # Assume it's an unknown app ID
    return arg, f"game_{arg}"

# ---- test infra (matches project pattern) ----

passed = 0
failed = 0
skipped = 0
errors = []
verbose = False
_log_fd = None


def _ensure_log():
    """Open /tmp log file for real-time append."""
    global _log_fd
    if _log_fd is None:
        TMP_DIR.mkdir(parents=True, exist_ok=True)
        _log_fd = open(LOG_FILE, "a")
        ts = datetime.now().strftime("%Y-%m-%d %H:%M:%S")
        _log_fd.write(f"\ntriskelion tests -- {ts}\n\n")
        _log_fd.flush()


def log(msg):
    ts = datetime.now().strftime("%H:%M:%S")
    line = f"[{ts}] {msg}"
    print(line)
    _ensure_log()
    _log_fd.write(line + "\n")
    _log_fd.flush()


def ok(name, detail=""):
    global passed
    passed += 1
    extra = f"  ({detail})" if detail else ""
    log(f"  PASS  {name}{extra}")


def fail(name, reason):
    global failed
    failed += 1
    errors.append((name, reason))
    log(f"  FAIL  {name}: {reason}")


def skip(name, reason):
    global skipped
    skipped += 1
    log(f"  SKIP  {name}: {reason}")


def summary(label):
    total = passed + failed
    log("")
    log(f"  {label}: {passed}/{total} passed, {failed} failed, {skipped} skipped")
    if errors:
        log("")
        for name, reason in errors:
            log(f"  FAIL  {name}: {reason}")
    log("")


def flush_log():
    if _log_fd:
        _log_fd.flush()
    log(f"  Log: {LOG_FILE}")


def reset_counters():
    global passed, failed, skipped, errors
    passed = 0
    failed = 0
    skipped = 0
    errors = []


# ---- process utilities ----

def find_wine_pid(app_id, diagnostic=False):
    """Find a wine process whose WINEPREFIX contains this app_id's compatdata.
    Only returns a PID with a confirmed compatdata match -- no fallback."""
    try:
        for proc_name in ["wine64-preloader", "wine64", "wine"]:
            result = subprocess.run(["pgrep", "-a", proc_name],
                                    capture_output=True, text=True, timeout=5)
            if result.returncode != 0:
                if diagnostic:
                    log(f"    [diag] pgrep {proc_name}: no matches")
                continue
            lines = result.stdout.strip().splitlines()
            if diagnostic:
                log(f"    [diag] pgrep {proc_name}: {len(lines)} processes")
                for line in lines[:5]:
                    log(f"    [diag]   {line[:120]}")
            for line in lines:
                pid_str = line.split()[0]
                pid = int(pid_str)
                try:
                    environ = Path(f"/proc/{pid}/environ").read_bytes()
                    if f"compatdata/{app_id}".encode() in environ:
                        if diagnostic:
                            log(f"    [diag] matched pid={pid} for app_id={app_id}")
                        return pid
                    elif diagnostic:
                        # Show what prefix this process IS using
                        for chunk in environ.split(b'\x00'):
                            if chunk.startswith(b'STEAM_COMPAT_DATA_PATH=') or chunk.startswith(b'WINEPREFIX='):
                                log(f"    [diag]   pid={pid}: {chunk.decode(errors='replace')[:100]}")
                                break
                except (OSError, PermissionError):
                    continue
        return None
    except (subprocess.TimeoutExpired, ValueError, IndexError):
        return None


def find_triskelion_pid():
    """Find a running triskelion process."""
    try:
        result = subprocess.run(["pgrep", "-f", "triskelion.*server|wineserver"],
                                capture_output=True, text=True, timeout=5)
        if result.returncode == 0 and result.stdout.strip():
            return int(result.stdout.strip().splitlines()[0].split()[0])
    except (subprocess.TimeoutExpired, ValueError):
        pass
    return None


def _find_all_game_pids(app_id):
    """Find all wine/wine64/wine64-preloader PIDs belonging to an app_id."""
    pids = []
    try:
        for proc_name in ["wine64-preloader", "wine64", "wine"]:
            result = subprocess.run(["pgrep", "-a", proc_name],
                                    capture_output=True, text=True, timeout=5)
            if result.returncode != 0:
                continue
            for line in result.stdout.strip().splitlines():
                pid = int(line.split()[0])
                if pid in [p for p, _ in pids]:
                    continue
                try:
                    environ = Path(f"/proc/{pid}/environ").read_bytes()
                    if f"compatdata/{app_id}".encode() in environ:
                        pids.append((pid, proc_name))
                except (OSError, PermissionError):
                    continue
    except subprocess.TimeoutExpired:
        pass
    return pids


def kill_game(app_id):
    """Kill all wine processes for an app_id. Returns True if clean exit."""
    pfx = STEAMAPPS / "compatdata" / app_id / "pfx"

    # Step 1: wineserver -k (graceful shutdown, tells all wine processes to exit)
    ws = BIN_DIR / "wineserver"
    if ws.exists() and pfx.exists():
        env = dict(os.environ)
        env["WINEPREFIX"] = str(pfx)
        subprocess.run([str(ws), "-k"], env=env, capture_output=True, timeout=10)

    # Step 2: SIGTERM any remaining game processes
    game_pids = _find_all_game_pids(app_id)
    for pid, proc_name in game_pids:
        try:
            os.kill(pid, signal.SIGTERM)
        except (ProcessLookupError, PermissionError):
            pass

    # Step 3: Wait up to 10s for all game processes to die
    for _ in range(10):
        time.sleep(1)
        remaining = _find_all_game_pids(app_id)
        if not remaining:
            return True

    return False


# ---- opcode stats parsing ----

def parse_opcode_stats(path=None):
    """Parse triskelion's opcode stats dump. Returns (total, [(name, count)])."""
    path = path or STATS_FILE
    if not path.exists():
        return 0, []
    try:
        text = path.read_text()
    except OSError:
        return 0, []

    total = 0
    entries = []
    for line in text.splitlines():
        # Header: "triskelion opcode stats (N total)"
        m = re.match(r"triskelion opcode stats \((\d+) total\)", line)
        if m:
            total = int(m.group(1))
            continue
        # Entry: "   12345  opcode_name"
        m = re.match(r"\s*(\d+)\s+(\S+)", line)
        if m:
            entries.append((m.group(2), int(m.group(1))))
    return total, entries


def parse_triskelion_stderr(stderr_text):
    """Parse triskelion stderr for handler logs and errors."""
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
            # Try to extract opcode name
            m2 = re.search(r"opcode (\w+)", line)
            if m2:
                not_implemented.add(m2.group(1))
        # Panics / crashes
        if "panic" in line.lower() or "SIGSEGV" in line or "fatal" in line.lower():
            crashes.append(line.strip())

    return handlers_hit, not_implemented, crashes


TIMING_FILE = TMP_DIR / "launcher_timing.json"


def parse_launcher_timing():
    """Parse launcher phase timing JSON. Returns dict or None."""
    if not TIMING_FILE.exists():
        return None
    try:
        return json.loads(TIMING_FILE.read_text())
    except (json.JSONDecodeError, OSError):
        return None


def log_launcher_timing(timing):
    """Log launcher phase timing breakdown."""
    if not timing:
        return
    total = timing.get("total_setup_ms", 0)
    log(f"  Launcher timing ({total}ms total):")
    for key, label in [
        ("discover_ms", "discover wine/steam"),
        ("prefix_ms", "prefix setup"),
        ("dxvk_ms", "DXVK/VKD3D deploy"),
        ("steam_ms", "Steam client deploy"),
    ]:
        ms = timing.get(key, 0)
        pct = ms / total * 100 if total > 0 else 0
        log(f"    {ms:>6}ms  {pct:>5.1f}%  {label}")


def profile_game(app_id, name, duration, launch_timeout=60):
    """Launch a game, collect timing + opcode data, kill it. Returns result dict."""
    TMP_DIR.mkdir(parents=True, exist_ok=True)
    if STATS_FILE.exists():
        STATS_FILE.unlink()
    if TIMING_FILE.exists():
        TIMING_FILE.unlink()

    # Launch
    log(f"  Launching {name} via steam://rungameid/{app_id}")
    subprocess.Popen(["steam", f"steam://rungameid/{app_id}"],
                     stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)

    # Wait for wine process
    t_launch_start = time.monotonic()
    wine_pid = None
    checks = launch_timeout // 2
    for i in range(checks):
        time.sleep(2)
        elapsed = int(time.monotonic() - t_launch_start)
        # Diagnostic on first check and every 10s
        diag = (i == 0) or (elapsed % 10 == 0)
        if diag:
            log(f"  [{elapsed}s] scanning for wine process (app_id={app_id})...")
        wine_pid = find_wine_pid(app_id, diagnostic=diag)
        if wine_pid:
            break

    t_wine_up = time.monotonic() - t_launch_start if wine_pid else None

    if not wine_pid:
        log(f"  No wine process found after {launch_timeout}s")
        # Dump what IS running for diagnosis
        try:
            ps = subprocess.run(["pgrep", "-a", "-f", "wine|proton|triskelion"],
                                capture_output=True, text=True, timeout=5)
            if ps.stdout.strip():
                log(f"  Related processes running:")
                for line in ps.stdout.strip().splitlines()[:10]:
                    log(f"    {line[:150]}")
            else:
                log(f"  No wine/proton/triskelion processes found at all")
        except subprocess.TimeoutExpired:
            pass

        # Check Steam's recent log for errors
        steam_log = Path.home() / ".steam" / "steam" / "logs" / "console-linux.txt"
        if steam_log.exists():
            try:
                text = steam_log.read_text(errors="replace")
                lines = text.splitlines()
                # Last 20 lines
                recent = lines[-20:] if len(lines) > 20 else lines
                log(f"  Steam console (last {len(recent)} lines):")
                for line in recent:
                    log(f"    {line[:150]}")
            except OSError:
                pass

        return {
            "app_id": app_id, "name": name,
            "launched": False, "wine_appear_s": None,
            "survived": False, "clean_exit": True, "timing": parse_launcher_timing(),
            "total_requests": 0, "opcodes": [],
        }

    # Let it run
    alive = True
    for _ in range(duration // 5):
        time.sleep(5)
        try:
            os.kill(wine_pid, 0)
        except ProcessLookupError:
            alive = False
            break

    # Collect data before killing
    timing = parse_launcher_timing()

    # Clean shutdown with verification
    log(f"  Closing {name}...")
    clean = kill_game(app_id)
    if clean:
        log(f"  {name}: clean exit")
    else:
        log(f"  {name}: processes lingered after SIGTERM, sending SIGKILL")
        try:
            result = subprocess.run(["pgrep", "-a", "wine"],
                                    capture_output=True, text=True, timeout=5)
            if result.returncode == 0:
                for line in result.stdout.strip().splitlines():
                    pid_str = line.split()[0]
                    pid = int(pid_str)
                    try:
                        environ = Path(f"/proc/{pid}/environ").read_bytes()
                        if f"compatdata/{app_id}".encode() in environ:
                            os.kill(pid, signal.SIGKILL)
                    except (OSError, PermissionError, ProcessLookupError):
                        continue
        except subprocess.TimeoutExpired:
            pass
        time.sleep(3)

    # Final verification
    remaining = find_wine_pid(app_id)
    if remaining:
        log(f"  WARNING: wine pid={remaining} still alive for {name}")
    else:
        log(f"  {name}: all processes terminated")

    total, entries = parse_opcode_stats()

    return {
        "app_id": app_id, "name": name,
        "launched": True, "wine_appear_s": round(t_wine_up, 1) if t_wine_up else None,
        "survived": alive, "timing": timing,
        "clean_exit": clean and not remaining,
        "total_requests": total, "opcodes": entries,
    }


# ---- test commands ----

def cmd_test_deploy(args):
    """Verify triskelion binary is built and deploy to Steam compat tools."""
    log("")
    log("  test-deploy: deploying triskelion as wineserver")
    log("")

    # 1. Check binary exists
    if not TRISKELION_BIN.exists():
        log("  Binary not found, building...")
        ret = subprocess.run(["cargo", "build", "--release"],
                             cwd=PROJECT_DIR, capture_output=not verbose).returncode
        if ret != 0:
            fail("build", "cargo build --release failed")
            summary("test-deploy")
            flush_log()
            return 1

    if TRISKELION_BIN.exists():
        size_kb = TRISKELION_BIN.stat().st_size // 1024
        ok("binary exists", f"{size_kb} KB")
    else:
        fail("binary exists", str(TRISKELION_BIN))
        summary("test-deploy")
        flush_log()
        return 1

    # 2. Check Steam compat dir
    if not COMPAT_DIR.exists():
        fail("compat dir", f"{COMPAT_DIR} does not exist. Run 'triskelion package <wine_dir>' first.")
        summary("test-deploy")
        flush_log()
        return 1
    ok("compat dir exists")

    # 3. Deploy as wineserver
    BIN_DIR.mkdir(parents=True, exist_ok=True)
    ws_dest = BIN_DIR / "wineserver"
    import shutil
    shutil.copy2(TRISKELION_BIN, ws_dest)
    os.chmod(ws_dest, 0o755)

    if ws_dest.exists():
        ok("deploy wineserver", str(ws_dest))
    else:
        fail("deploy wineserver", "copy failed")

    # 4. Also deploy as proton launcher
    proton_dest = COMPAT_DIR / "proton"
    shutil.copy2(TRISKELION_BIN, proton_dest)
    os.chmod(proton_dest, 0o755)
    if proton_dest.exists():
        ok("deploy proton", str(proton_dest))
    else:
        fail("deploy proton", "copy failed")

    # 5. Verify wine64 exists (needed for actual game launch)
    wine64 = BIN_DIR / "wine64"
    if wine64.exists():
        ok("wine64 present")
    else:
        skip("wine64 present", "not found -- game launch will fail without Wine binaries")

    summary("test-deploy")
    flush_log()
    return 0 if failed == 0 else 1


def cmd_test_launch(args):
    """Launch a game through triskelion and validate startup."""
    app_id = args.app_id or DEFAULT_APP_ID
    name = args.name or DEFAULT_GAME
    launch_timeout = getattr(args, "launch_timeout", 30)

    log("")
    log(f"  test-launch: {name} (app_id={app_id})")
    log("")

    # Prechecks
    if not (BIN_DIR / "wineserver").exists():
        fail("precondition", "triskelion not deployed as wineserver. Run test-deploy first.")
        summary("test-launch")
        flush_log()
        return 1

    pfx = STEAMAPPS / "compatdata" / app_id
    if not pfx.exists():
        fail("precondition", f"no compatdata for {app_id}. Has this game been run with Proton before?")
        summary("test-launch")
        flush_log()
        return 1
    ok("compatdata exists")

    steam_check = subprocess.run(["pgrep", "-x", "steam"], capture_output=True)
    if steam_check.returncode != 0:
        fail("precondition", "Steam is not running")
        summary("test-launch")
        flush_log()
        return 1
    ok("steam running")

    # Clean any stale stats
    TMP_DIR.mkdir(parents=True, exist_ok=True)
    if STATS_FILE.exists():
        STATS_FILE.unlink()

    # Snapshot shm
    shm_before = set(f for f in os.listdir("/dev/shm") if f.startswith("triskelion-"))

    # Launch
    log(f"  Launching via steam://rungameid/{app_id}")
    subprocess.Popen(["steam", f"steam://rungameid/{app_id}"],
                     stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)

    # Wait for wine process
    wine_pid = None
    for i in range(launch_timeout // 2):
        time.sleep(2)
        wine_pid = find_wine_pid(app_id)
        if wine_pid:
            break
        if verbose:
            log(f"  ... waiting for wine process ({(i+1)*2}s)")

    if not wine_pid:
        fail("wine process", f"no wine64-preloader after {launch_timeout}s")

        # Check if triskelion left stats (crashed early)
        if STATS_FILE.exists():
            total, entries = parse_opcode_stats()
            log(f"  triskelion processed {total} requests before dying")
            if entries:
                log(f"  Last opcodes handled:")
                for name_op, count in entries[:10]:
                    log(f"    {count:>8}  {name_op}")

        # Try to find triskelion stderr in journal
        _dump_triskelion_journal()

        summary("test-launch")
        flush_log()
        kill_game(app_id)
        return 1

    ok("wine process", f"pid={wine_pid}")

    # Check triskelion is running
    tris_pid = find_triskelion_pid()
    if tris_pid:
        ok("triskelion alive", f"pid={tris_pid}")
    else:
        fail("triskelion alive", "not found (may have crashed)")

    # Quick survival check (10s)
    alive = True
    for _ in range(5):
        time.sleep(2)
        try:
            os.kill(wine_pid, 0)
        except ProcessLookupError:
            alive = False
            break

    if alive:
        ok("10s survival")
    else:
        fail("10s survival", "wine process died during startup")

    # SHM check
    shm_after = set(f for f in os.listdir("/dev/shm") if f.startswith("triskelion-"))
    new_shm = shm_after - shm_before
    if new_shm:
        ok("shm segments", f"{len(new_shm)} new triskelion-* segments")
    else:
        skip("shm segments", "no new segments (bypass may not be active)")

    # Save result
    result = {
        "test": "launch",
        "app_id": app_id,
        "name": name,
        "date": datetime.now().strftime("%Y-%m-%d %H:%M:%S"),
        "wine_pid": wine_pid,
        "triskelion_pid": tris_pid,
        "launched": wine_pid is not None,
        "survived_10s": alive,
        "shm_count": len(new_shm),
    }
    _save_result(result)

    # Cleanup
    log(f"  Killing game...")
    kill_game(app_id)

    # Launcher timing
    timing = parse_launcher_timing()
    if timing:
        log_launcher_timing(timing)
        result["timing"] = timing

    # Grab opcode stats after shutdown
    time.sleep(2)
    if STATS_FILE.exists():
        total, entries = parse_opcode_stats()
        log(f"  triskelion opcode stats: {total} total requests")
        for name_op, count in entries[:15]:
            pct = count / total * 100 if total > 0 else 0
            log(f"    {count:>8}  {pct:>5.1f}%  {name_op}")
        result["total_requests"] = total
        result["top_opcodes"] = entries[:20]
        _save_result(result)

    summary("test-launch")
    flush_log()
    return 0 if failed == 0 else 1


def cmd_test_survive(args):
    """Launch and run sustained survival check."""
    app_id = args.app_id or DEFAULT_APP_ID
    name = args.name or DEFAULT_GAME
    timeout = args.timeout

    log("")
    log(f"  test-survive: {name} for {timeout}s")
    log("")

    # Prechecks (same as launch)
    for check, path in [("wineserver deployed", BIN_DIR / "wineserver"),
                        ("compatdata", STEAMAPPS / "compatdata" / app_id)]:
        if not path.exists():
            fail(check, str(path))
            summary("test-survive")
            flush_log()
            return 1

    steam_check = subprocess.run(["pgrep", "-x", "steam"], capture_output=True)
    if steam_check.returncode != 0:
        fail("steam running", "start Steam first")
        summary("test-survive")
        flush_log()
        return 1

    # Clean stats
    TMP_DIR.mkdir(parents=True, exist_ok=True)
    if STATS_FILE.exists():
        STATS_FILE.unlink()

    # Launch
    log(f"  Launching via steam://rungameid/{app_id}")
    subprocess.Popen(["steam", f"steam://rungameid/{app_id}"],
                     stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)

    # Wait for wine
    wine_pid = None
    for _ in range(15):
        time.sleep(2)
        wine_pid = find_wine_pid(app_id)
        if wine_pid:
            break

    if not wine_pid:
        fail("launch", f"no wine process after 30s")
        _dump_triskelion_journal()
        summary("test-survive")
        flush_log()
        return 1

    ok("launch", f"wine pid={wine_pid}")

    # Sustained survival
    start = time.monotonic()
    alive = True
    last_check = 0
    while time.monotonic() - start < timeout:
        time.sleep(5)
        elapsed = int(time.monotonic() - start)
        try:
            os.kill(wine_pid, 0)
        except ProcessLookupError:
            alive = False
            fail("survival", f"died after {elapsed}s")
            break

        # Periodic status
        if elapsed - last_check >= 30:
            last_check = elapsed
            log(f"  ... alive at {elapsed}s")
            # Check triskelion still running
            tris_pid = find_triskelion_pid()
            if not tris_pid:
                fail("triskelion", f"wineserver process gone at {elapsed}s")
                alive = False
                break

    if alive:
        elapsed = int(time.monotonic() - start)
        ok("survival", f"alive for {elapsed}s")

    # Cleanup
    log(f"  Killing game...")
    clean = kill_game(app_id)
    if clean:
        ok("clean exit")
    else:
        fail("clean exit", "processes lingered")

    # Opcode stats
    time.sleep(2)
    if STATS_FILE.exists():
        total, entries = parse_opcode_stats()
        log(f"  triskelion opcode stats: {total} total requests over {timeout}s")
        rps = total / timeout if timeout > 0 else 0
        log(f"  Throughput: {rps:.0f} requests/sec")
        for name_op, count in entries[:20]:
            pct = count / total * 100 if total > 0 else 0
            log(f"    {count:>8}  {pct:>5.1f}%  {name_op}")

    result = {
        "test": "survive",
        "app_id": app_id,
        "name": name,
        "date": datetime.now().strftime("%Y-%m-%d %H:%M:%S"),
        "timeout": timeout,
        "survived": alive,
        "survival_seconds": int(time.monotonic() - start) if not alive else timeout,
        "clean_exit": clean,
    }
    _save_result(result)

    summary("test-survive")
    flush_log()
    return 0 if failed == 0 else 1


def cmd_test_profile(args):
    """Launch game, capture opcode profile + launcher timing."""
    app_id = args.app_id or DEFAULT_APP_ID
    name = args.name or DEFAULT_GAME
    duration = args.duration

    log("")
    log(f"  test-profile: {name} for {duration}s")
    log("")

    # Prechecks
    steam_check = subprocess.run(["pgrep", "-x", "steam"], capture_output=True)
    if steam_check.returncode != 0:
        fail("steam", "start Steam first")
        summary("test-profile")
        flush_log()
        return 1

    result = profile_game(app_id, name, duration)

    if not result["launched"]:
        fail("launch", "no wine process")
        timing = result.get("timing")
        if timing:
            log_launcher_timing(timing)
        _dump_triskelion_journal()
        summary("test-profile")
        flush_log()
        return 1

    ok("launch", f"wine appeared in {result['wine_appear_s']}s")

    if result["survived"]:
        ok("survival", f"alive for {duration}s")
    else:
        fail("survival", "died during profiling")

    # Launcher timing
    timing = result.get("timing")
    if timing:
        log_launcher_timing(timing)
        ok("launcher timing", f"{timing.get('total_setup_ms', 0)}ms setup")
    else:
        skip("launcher timing", "no timing data")

    # Opcode stats
    total = result["total_requests"]
    entries = result["opcodes"]

    if total > 0:
        rps = total / duration if duration > 0 else 0
        ok("opcode stats", f"{total} requests, {len(entries)} unique opcodes, {rps:.0f} req/s")
        log("")
        log(f"  {'Count':>8}  {'Pct':>6}  Opcode")
        for name_op, count in entries[:30]:
            pct = count / total * 100 if total > 0 else 0
            log(f"  {count:>8}  {pct:>5.1f}%  {name_op}")
    else:
        fail("opcode stats", "no stats captured")

    save_result = {
        "test": "profile",
        "app_id": app_id,
        "name": name,
        "date": datetime.now().strftime("%Y-%m-%d %H:%M:%S"),
        "duration": duration,
        "wine_appear_s": result["wine_appear_s"],
        "survived": result["survived"],
        "total_requests": total,
        "requests_per_sec": round(total / duration, 1) if duration > 0 else 0,
        "unique_opcodes": len(entries),
        "top_opcodes": entries[:30],
        "timing": timing,
    }
    _save_result(save_result)

    summary("test-profile")
    flush_log()
    return 0 if failed == 0 else 1


def cmd_test_compare(args):
    """Profile multiple games sequentially, diff their timing + opcode patterns."""
    # Resolve game list
    games = []
    if hasattr(args, "games") and args.games:
        for g in args.games:
            games.append(resolve_game(g))
    elif hasattr(args, "app_a") and args.app_a:
        # Backward compat with old --app-a / --app-b
        games.append(resolve_game(args.app_a))
        if hasattr(args, "app_b") and args.app_b:
            games.append(resolve_game(args.app_b))

    if len(games) < 2:
        log("  test-compare requires at least 2 games")
        log("  Usage: test-compare --game hot --game hades --game gw2")
        log(f"  Known: {', '.join(f'{v} ({k})' for k, v in KNOWN_GAMES.items())}")
        return 1

    duration = args.duration
    names = [name for _, name in games]

    log("")
    log(f"  test-compare: {' vs '.join(names)}")
    log(f"  Duration: {duration}s per game, {len(games)} games")
    log("")

    # Prechecks
    steam_check = subprocess.run(["pgrep", "-x", "steam"], capture_output=True)
    if steam_check.returncode != 0:
        fail("steam", "start Steam first")
        summary("test-compare")
        flush_log()
        return 1

    # Profile each game sequentially with clean exit between
    results = []
    for i, (app_id, name) in enumerate(games):
        log(f"  [{i+1}/{len(games)}] Profiling: {name} ({app_id})")
        result = profile_game(app_id, name, duration)

        if result["launched"]:
            ok(f"{name}: launch", f"{result['wine_appear_s']}s")
        else:
            fail(f"{name}: launch", "no wine process")

        clean = result.get("clean_exit", False)
        if clean:
            ok(f"{name}: clean exit")
        else:
            fail(f"{name}: clean exit", "processes may have lingered")

        results.append(result)

        # Wait between games (skip after last)
        if i < len(games) - 1:
            log(f"  Waiting 10s before next game...")
            time.sleep(10)

    # Comparison report
    log("")
    log("  Comparison")
    log("")

    # Dynamic column widths
    col_w = max(14, max(len(n) for n in names) + 2)

    # Header
    header = f"  {'Metric':<30}"
    for name in names:
        header += f" {name:<{col_w}}"
    log(header)
    log("")

    # Wine appear time
    row = f"  {'Wine appeared':<30}"
    for r in results:
        wa = r.get("wine_appear_s")
        row += f" {f'{wa}s' if wa else 'FAIL':<{col_w}}"
    log(row)

    # Launcher timing phases
    for key, label in [
        ("total_setup_ms", "Launcher setup (ms)"),
        ("discover_ms", "  discover wine/steam"),
        ("prefix_ms", "  prefix setup"),
        ("dxvk_ms", "  DXVK/VKD3D deploy"),
        ("steam_ms", "  Steam client deploy"),
    ]:
        row = f"  {label:<30}"
        for r in results:
            t = (r.get("timing") or {}).get(key, 0)
            row += f" {t:<{col_w}}"
        log(row)

    log("")
    row = f"  {'Total requests':<30}"
    for r in results:
        row += f" {r['total_requests']:<{col_w}}"
    log(row)

    row = f"  {'Unique opcodes':<30}"
    for r in results:
        row += f" {len(r['opcodes']):<{col_w}}"
    log(row)

    row = f"  {'Survived':<30}"
    for r in results:
        row += f" {str(r['survived']):<{col_w}}"
    log(row)

    row = f"  {'Clean exit':<30}"
    for r in results:
        row += f" {str(r.get('clean_exit', False)):<{col_w}}"
    log(row)

    # Opcode table across all games
    all_op_dicts = [dict(r["opcodes"]) for r in results]
    all_op_names = set()
    for d in all_op_dicts:
        all_op_names.update(d.keys())
    all_op_names = sorted(all_op_names,
                          key=lambda k: max(d.get(k, 0) for d in all_op_dicts),
                          reverse=True)

    if all_op_names:
        log("")
        header = f"  {'Opcode':<30}"
        for name in names:
            header += f" {name:<{col_w}}"
        log(header)
        log("")
        for op in all_op_names[:25]:
            row = f"  {op:<30}"
            for d in all_op_dicts:
                row += f" {d.get(op, 0):<{col_w}}"
            log(row)

    # Per-game unique opcodes
    for i, (_, name) in enumerate(games):
        others = set()
        for j, d in enumerate(all_op_dicts):
            if j != i:
                others.update(d.keys())
        only = set(all_op_dicts[i].keys()) - others
        if only:
            log("")
            log(f"  Opcodes only in {name}: {', '.join(sorted(only))}")

    # Save
    save = {
        "test": "compare",
        "date": datetime.now().strftime("%Y-%m-%d %H:%M:%S"),
        "games": results,
    }
    _save_result(save)

    summary("test-compare")
    flush_log()
    return 0 if failed == 0 else 1


def cmd_test_iterate(args):
    """Deploy, launch, and on failure report exactly what's missing."""
    app_id = args.app_id or DEFAULT_APP_ID
    name = args.name or DEFAULT_GAME

    log("")
    log(f"  test-iterate: deploy + launch + diagnose")
    log(f"  Game: {name} ({app_id})")
    log("")

    # Step 1: Build
    log("  Step 1: Building triskelion")
    ret = subprocess.run(["cargo", "build", "--release"],
                         cwd=PROJECT_DIR, capture_output=not verbose).returncode
    if ret != 0:
        fail("build", "cargo build failed")
        summary("test-iterate")
        flush_log()
        return 1
    ok("build")

    # Step 2: Deploy
    if not COMPAT_DIR.exists():
        fail("deploy", f"compat dir missing: {COMPAT_DIR}")
        log("  Run 'triskelion package <wine_dir>' to create it")
        summary("test-iterate")
        flush_log()
        return 1

    import shutil
    BIN_DIR.mkdir(parents=True, exist_ok=True)
    ws_dest = BIN_DIR / "wineserver"
    shutil.copy2(TRISKELION_BIN, ws_dest)
    os.chmod(ws_dest, 0o755)
    ok("deploy", str(ws_dest))

    # Step 3: Clean state
    TMP_DIR.mkdir(parents=True, exist_ok=True)
    if STATS_FILE.exists():
        STATS_FILE.unlink()

    # Step 4: Launch
    steam_check = subprocess.run(["pgrep", "-x", "steam"], capture_output=True)
    if steam_check.returncode != 0:
        fail("steam", "not running")
        summary("test-iterate")
        flush_log()
        return 1

    log(f"  Step 2: Launching {name}")
    subprocess.Popen(["steam", f"steam://rungameid/{app_id}"],
                     stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)

    wine_pid = None
    for _ in range(15):
        time.sleep(2)
        wine_pid = find_wine_pid(app_id)
        if wine_pid:
            break

    if wine_pid:
        ok("launch", f"pid={wine_pid}")

        # Wait 15s to see if it survives initial startup
        alive = True
        for _ in range(3):
            time.sleep(5)
            try:
                os.kill(wine_pid, 0)
            except ProcessLookupError:
                alive = False
                break

        if alive:
            ok("15s survival")
        else:
            fail("15s survival", "died during startup")
    else:
        fail("launch", "no wine process after 30s")
        alive = False

    # Step 5: Diagnose
    log("")
    log("  Step 3: Diagnosis")

    kill_game(app_id)
    time.sleep(2)

    if STATS_FILE.exists():
        total, entries = parse_opcode_stats()
        log(f"  triskelion handled {total} requests across {len(entries)} opcodes")

        if entries:
            log("")
            log(f"  Top opcodes:")
            for name_op, count in entries[:15]:
                pct = count / total * 100 if total > 0 else 0
                log(f"    {count:>8}  {pct:>5.1f}%  {name_op}")
    else:
        log("  No opcode stats (triskelion may not have started or crashed immediately)")

    # Check journal for crash info
    _dump_triskelion_journal()

    if not alive and wine_pid:
        log("")
        log("  DIAGNOSIS: Game started but crashed during startup.")
        log("  Most likely cause: an opcode returned STATUS_NOT_IMPLEMENTED")
        log("  that Wine treats as fatal. Check the opcode list above --")
        log("  any opcode with just 1-2 hits near the bottom is a candidate.")
        log("")
        log("  Action: add a stub handler for the failing opcode in event_loop.rs")
    elif not wine_pid:
        log("")
        log("  DIAGNOSIS: Wine process never appeared.")
        log("  Possible causes:")
        log("    1. triskelion crashed before accepting connections")
        log("    2. Wine binaries missing from compat tool directory")
        log("    3. Steam not configured to use amphetamine for this game")

    result = {
        "test": "iterate",
        "app_id": app_id,
        "name": name,
        "date": datetime.now().strftime("%Y-%m-%d %H:%M:%S"),
        "launched": wine_pid is not None,
        "survived": alive,
    }
    _save_result(result)

    summary("test-iterate")
    flush_log()
    return 0 if failed == 0 else 1


def cmd_test_all(args):
    """Full pipeline: deploy, launch, survive, profile."""
    log("")
    log("  test-all: full triskelion integration pipeline")
    log("")

    # Deploy
    reset_counters()
    ret = cmd_test_deploy(args)
    if ret != 0:
        log("  Deploy failed -- stopping pipeline")
        return ret

    # Launch
    reset_counters()
    ret = cmd_test_launch(args)
    if ret != 0:
        log("  Launch failed -- run test-iterate for diagnosis")
        return ret

    # Survive
    reset_counters()
    ret = cmd_test_survive(args)
    if ret != 0:
        log("  Survival failed -- game crashes under sustained load")
        return ret

    # Profile
    reset_counters()
    ret = cmd_test_profile(args)

    log("")
    log("  Pipeline complete.")
    if ret == 0:
        log("  triskelion is serving games. Ready for performance testing.")
    return ret


# ---- helpers ----

def _save_result(result):
    RESULTS_DIR.mkdir(parents=True, exist_ok=True)
    ts = datetime.now().strftime("%Y%m%d_%H%M%S")
    name = result.get("test", "unknown")
    path = RESULTS_DIR / f"{name}_{ts}.json"
    with open(path, "w") as f:
        json.dump(result, f, indent=2)
    if verbose:
        log(f"  Result saved: {path}")


def _dump_triskelion_journal():
    """Try to extract recent triskelion log lines from journald or dmesg."""
    try:
        result = subprocess.run(
            ["journalctl", "--user", "-n", "50", "--no-pager",
             "-g", "triskelion", "--since", "5 min ago"],
            capture_output=True, text=True, timeout=5)
        if result.returncode == 0 and result.stdout.strip():
            log("  Recent triskelion journal entries:")
            for line in result.stdout.strip().splitlines()[-10:]:
                log(f"    {line}")
    except (subprocess.TimeoutExpired, FileNotFoundError):
        pass


# ---- argparse ----

def main():
    global verbose

    parser = argparse.ArgumentParser(
        description="triskelion integration tests",
        formatter_class=argparse.RawDescriptionHelpFormatter,
    )
    sub = parser.add_subparsers(dest="command")

    # test-deploy
    p_deploy = sub.add_parser("test-deploy", help="Deploy triskelion as wineserver")
    p_deploy.add_argument("--verbose", action="store_true")

    # test-launch
    p_launch = sub.add_parser("test-launch", help="Launch game through triskelion")
    p_launch.add_argument("--app-id", type=str, default=None)
    p_launch.add_argument("--name", type=str, default=None)
    p_launch.add_argument("--launch-timeout", type=int, default=30)
    p_launch.add_argument("--verbose", action="store_true")

    # test-survive
    p_survive = sub.add_parser("test-survive", help="Sustained survival check")
    p_survive.add_argument("--app-id", type=str, default=None)
    p_survive.add_argument("--name", type=str, default=None)
    p_survive.add_argument("--timeout", type=int, default=60)
    p_survive.add_argument("--verbose", action="store_true")

    # test-profile
    p_profile = sub.add_parser("test-profile", help="Capture opcode profile")
    p_profile.add_argument("--app-id", type=str, default=None)
    p_profile.add_argument("--name", type=str, default=None)
    p_profile.add_argument("--duration", type=int, default=60)
    p_profile.add_argument("--verbose", action="store_true")

    # test-compare
    p_cmp = sub.add_parser("test-compare", help="Profile multiple games, diff timing + opcodes")
    p_cmp.add_argument("--game", dest="games", action="append",
                        help="Game app ID or alias (hot, hades, gw2). Repeat for each game.")
    p_cmp.add_argument("--duration", type=int, default=30, help="Profile duration per game (seconds)")
    p_cmp.add_argument("--verbose", action="store_true")

    # test-iterate
    p_iter = sub.add_parser("test-iterate", help="Deploy + launch + diagnose failures")
    p_iter.add_argument("--app-id", type=str, default=None)
    p_iter.add_argument("--name", type=str, default=None)
    p_iter.add_argument("--verbose", action="store_true")

    # test-all
    p_all = sub.add_parser("test-all", help="Full pipeline: deploy, launch, survive, profile")
    p_all.add_argument("--app-id", type=str, default=None)
    p_all.add_argument("--name", type=str, default=None)
    p_all.add_argument("--timeout", type=int, default=60)
    p_all.add_argument("--duration", type=int, default=60)
    p_all.add_argument("--verbose", action="store_true")

    args = parser.parse_args()
    verbose = getattr(args, "verbose", False)

    if not args.command:
        parser.print_help()
        return 1

    dispatch = {
        "test-deploy": cmd_test_deploy,
        "test-launch": cmd_test_launch,
        "test-survive": cmd_test_survive,
        "test-profile": cmd_test_profile,
        "test-compare": cmd_test_compare,
        "test-iterate": cmd_test_iterate,
        "test-all": cmd_test_all,
    }

    return dispatch[args.command](args)


if __name__ == "__main__":
    sys.exit(main())
