#!/usr/bin/env python3
"""Run iterate.py against ALL installed Steam games and produce a Prometheus diagnostic.

Discovers every playable game in Steam libraries, launches each through quark/triskelion,
captures opcode coverage, and writes a comprehensive .prom file.

Usage:
    python3 tests/diagnose_all.py                  # All games, 30s each
    python3 tests/diagnose_all.py --timeout 45     # Longer per-game
    python3 tests/diagnose_all.py --skip-build     # Skip cargo build (use existing binaries)
    python3 tests/diagnose_all.py --games 2379780,570940  # Specific games only
"""

import argparse
import os
import re
import subprocess
import sys
import time
from dataclasses import dataclass, field
from datetime import datetime
from pathlib import Path

SCRIPT_DIR = Path(__file__).resolve().parent
REPO_ROOT = SCRIPT_DIR.parent
ITERATE_PY = SCRIPT_DIR / "iterate.py"
_USER_HOME = Path(f"/home/{os.environ.get('SUDO_USER', os.environ.get('USER', 'mod'))}")
STEAM_ROOT = _USER_HOME / ".local/share/Steam"
STEAMAPPS = STEAM_ROOT / "steamapps"
OUT_DIR = Path("/tmp/quark/diagnose_all")
DAEMON_LOG = Path("/tmp/quark/daemon.log")
OPCODE_STATS = Path("/tmp/quark/triskelion_opcode_stats.txt")


def _timestamp() -> str:
    return datetime.now().strftime("[%H:%M:%S]")

def log_info(msg: str) -> None:
    print(f"{_timestamp()} [INFO]   {msg}", flush=True)

def log_warn(msg: str) -> None:
    print(f"{_timestamp()} [WARN]   {msg}", flush=True)

def log_error(msg: str) -> None:
    print(f"{_timestamp()} [ERROR]  {msg}", flush=True)


# Non-game filters

NON_GAME_NAMES = {
    "steam linux runtime", "proton", "steamworks common redistributables",
    "proton easyanticheat runtime", "proton hotfix", "proton experimental",
}

NON_GAME_APPIDS = {
    "228980", "1070560", "1391110", "1493710", "1628350",
    "1826330", "2180100", "2348590", "2805730", "3658110",
    "1580130", "1887720",
}


# Game discovery

@dataclass
class GameInfo:
    appid: str
    name: str
    install_dir: Path
    exe: Path | None = None


def discover_games() -> list[GameInfo]:
    """Find all playable Steam games from appmanifest files."""
    games: list[GameInfo] = []
    seen: set[str] = set()

    # Discover all Steam library folders
    library_dirs = [STEAMAPPS]
    vdf = STEAMAPPS / "libraryfolders.vdf"
    if vdf.exists():
        try:
            text = vdf.read_text(errors="replace")
            for m in re.finditer(r'"path"\s+"([^"]+)"', text):
                lib = Path(m.group(1)) / "steamapps"
                if lib.exists() and lib.resolve() != STEAMAPPS.resolve():
                    library_dirs.append(lib)
        except OSError:
            pass

    for steamapps in library_dirs:
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
            if appid in seen or appid in NON_GAME_APPIDS:
                continue
            if any(skip in name.lower() for skip in NON_GAME_NAMES):
                continue

            install_path = steamapps / "common" / installdir
            if not install_path.exists():
                continue

            # Find largest .exe (same heuristic as iterate.py)
            exe = _find_exe(install_path)
            seen.add(appid)
            games.append(GameInfo(appid=appid, name=name, install_dir=install_path, exe=exe))

    games.sort(key=lambda g: g.name)
    return games


def _find_exe(install_dir: Path) -> Path | None:
    """Find the primary game executable (largest .exe, depth <= 3)."""
    blacklist = {"crashhandler", "crashreporter", "unins", "redist", "vcredist",
                 "dxsetup", "dxwebsetup", "easyanticheat", "battleye", "dotnet",
                 "vc_redist", "installer", "setup", "launcher_crash"}
    best: Path | None = None
    best_size = 0
    try:
        for depth, dirs, files in _walk_depth(install_dir, max_depth=3):
            for f in files:
                if not f.lower().endswith(".exe"):
                    continue
                if any(b in f.lower() for b in blacklist):
                    continue
                fp = depth / f
                try:
                    sz = fp.stat().st_size
                    if sz > best_size:
                        best = fp
                        best_size = sz
                except OSError:
                    pass
    except OSError:
        pass
    return best


def _walk_depth(root: Path, max_depth: int = 3):
    """os.walk with depth limit."""
    for dirpath, dirnames, filenames in os.walk(root):
        depth = Path(dirpath).relative_to(root).parts
        if len(depth) >= max_depth:
            dirnames.clear()
            continue
        yield Path(dirpath), dirnames, filenames


# Per-game result

@dataclass
class GameResult:
    appid: str
    name: str
    total_requests: int = 0
    total_opcodes: int = 0
    impl_opcodes: int = 0
    stub_opcodes: int = 0
    uptime_ms: int = 0
    avg_ns: int = 0
    verdict: str = "unknown"
    milestones: list[str] = field(default_factory=list)
    opcode_counts: dict[str, int] = field(default_factory=dict)
    errors: list[str] = field(default_factory=list)
    exit_code: int = -1


def run_game(game: GameInfo, timeout: int, skip_build: bool) -> GameResult:
    """Run iterate.py for a single game and parse results."""
    result = GameResult(appid=game.appid, name=game.name)

    cmd = [
        sys.executable, str(ITERATE_PY),
        "--appid", game.appid,
        "--timeout", str(timeout),
        "--skip-build",
    ]

    log_info(f"  launching {game.name} ({game.appid})...")

    try:
        proc = subprocess.run(
            cmd, capture_output=True, text=True,
            timeout=timeout + 60,  # extra margin for startup/teardown
        )
        result.exit_code = proc.returncode
        output = proc.stdout + proc.stderr
    except subprocess.TimeoutExpired:
        result.verdict = "timeout_hard"
        result.errors.append("iterate.py itself timed out")
        return result

    # Parse verdict from output
    if "verdict: timeout" in output:
        result.verdict = "timeout"
    elif "verdict: hung" in output:
        result.verdict = "hung"
    elif "verdict:" in output:
        m = re.search(r"verdict:\s+(.+)", output)
        result.verdict = m.group(1).strip() if m else "unknown"

    # Parse milestones from output
    for milestone in ["x11drv", "XRandR", "STEAM_API", "RENDERING", "DXVK", "CRASH"]:
        if milestone in output:
            result.milestones.append(milestone)

    # Parse opcode stats from daemon output
    if OPCODE_STATS.exists():
        try:
            text = OPCODE_STATS.read_text()
            for line in text.splitlines():
                m = re.match(r"total_dispatch_ns:\s+(\d+)", line)
                if m:
                    continue
                m = re.match(r"avg_ns_per_request:\s+(\d+)", line)
                if m:
                    result.avg_ns = int(m.group(1))
                m = re.match(r"uptime_ms:\s+(\d+)", line)
                if m:
                    result.uptime_ms = int(m.group(1))
                # Opcode lines: count ns ns/call name
                m = re.match(r"\s+(\d+)\s+\d+ns\s+\d+ns/call\s+(\w+)", line)
                if m:
                    count = int(m.group(1))
                    name = m.group(2)
                    result.opcode_counts[name] = count
                    result.total_requests += count
                    result.total_opcodes += 1

            # Count from first line
            first = text.splitlines()[0] if text.strip() else ""
            m = re.match(r"triskelion opcode stats \((\d+) total", first)
            if m:
                result.total_requests = int(m.group(1))
        except OSError:
            result.errors.append("could not read opcode stats")

    # Parse coverage from iterate.py output
    m = re.search(r"opcodes=(\d+) \(impl=(\d+) stub=(\d+)\)", output)
    if m:
        result.total_opcodes = int(m.group(1))
        result.impl_opcodes = int(m.group(2))
        result.stub_opcodes = int(m.group(3))

    return result


# Prometheus output

def write_prom(results: list[GameResult], path: Path) -> None:
    """Write Prometheus exposition format file."""
    lines: list[str] = []
    ts = int(time.time() * 1000)

    lines.append("# HELP quark_game_requests Total wineserver requests processed")
    lines.append("# TYPE quark_game_requests gauge")
    for r in results:
        lines.append(f'quark_game_requests{{appid="{r.appid}",game="{_escape(r.name)}"}} {r.total_requests} {ts}')

    lines.append("")
    lines.append("# HELP quark_game_opcodes Unique opcodes seen")
    lines.append("# TYPE quark_game_opcodes gauge")
    for r in results:
        lines.append(f'quark_game_opcodes{{appid="{r.appid}",game="{_escape(r.name)}"}} {r.total_opcodes} {ts}')

    lines.append("")
    lines.append("# HELP quark_game_impl_opcodes Implemented opcodes hit")
    lines.append("# TYPE quark_game_impl_opcodes gauge")
    for r in results:
        lines.append(f'quark_game_impl_opcodes{{appid="{r.appid}",game="{_escape(r.name)}"}} {r.impl_opcodes} {ts}')

    lines.append("")
    lines.append("# HELP quark_game_stub_opcodes Stub opcodes hit")
    lines.append("# TYPE quark_game_stub_opcodes gauge")
    for r in results:
        lines.append(f'quark_game_stub_opcodes{{appid="{r.appid}",game="{_escape(r.name)}"}} {r.stub_opcodes} {ts}')

    lines.append("")
    lines.append("# HELP quark_game_uptime_ms Session uptime in milliseconds")
    lines.append("# TYPE quark_game_uptime_ms gauge")
    for r in results:
        lines.append(f'quark_game_uptime_ms{{appid="{r.appid}",game="{_escape(r.name)}"}} {r.uptime_ms} {ts}')

    lines.append("")
    lines.append("# HELP quark_game_avg_ns Average dispatch latency in nanoseconds")
    lines.append("# TYPE quark_game_avg_ns gauge")
    for r in results:
        lines.append(f'quark_game_avg_ns{{appid="{r.appid}",game="{_escape(r.name)}"}} {r.avg_ns} {ts}')

    lines.append("")
    lines.append("# HELP quark_game_verdict Session outcome (0=unknown 1=timeout 2=hung 3=crash 4=clean)")
    lines.append("# TYPE quark_game_verdict gauge")
    verdict_map = {"unknown": 0, "timeout": 1, "hung": 2, "crash": 3, "clean": 4, "timeout_hard": 5}
    for r in results:
        v = verdict_map.get(r.verdict, 0)
        lines.append(f'quark_game_verdict{{appid="{r.appid}",game="{_escape(r.name)}",verdict="{r.verdict}"}} {v} {ts}')

    lines.append("")
    lines.append("# HELP quark_game_milestone Game reached this milestone (1=yes)")
    lines.append("# TYPE quark_game_milestone gauge")
    for r in results:
        for m in ["x11drv", "XRandR", "STEAM_API", "RENDERING", "DXVK", "CRASH"]:
            val = 1 if m in r.milestones else 0
            lines.append(f'quark_game_milestone{{appid="{r.appid}",game="{_escape(r.name)}",milestone="{m}"}} {val} {ts}')

    lines.append("")
    lines.append("# HELP quark_opcode_requests Per-opcode request count per game")
    lines.append("# TYPE quark_opcode_requests gauge")
    for r in results:
        for opcode, count in sorted(r.opcode_counts.items(), key=lambda x: -x[1]):
            lines.append(f'quark_opcode_requests{{appid="{r.appid}",game="{_escape(r.name)}",opcode="{opcode}"}} {count} {ts}')

    path.write_text("\n".join(lines) + "\n")


def _escape(s: str) -> str:
    """Escape string for Prometheus label value."""
    return s.replace("\\", "\\\\").replace('"', '\\"').replace("\n", "\\n")


# Summary table

def print_summary(results: list[GameResult]) -> None:
    """Print human-readable summary table."""
    print()
    print(" ")
    print("  quark diagnostic — all games")
    print(" ")
    print(f"  {'Game':<35} {'Reqs':>7} {'Ops':>4} {'I/S':>5} {'Time':>6} {'Milestones':<25} {'Verdict'}")
    

    total_requests = 0
    total_games = len(results)
    verdicts: dict[str, int] = {}

    for r in results:
        total_requests += r.total_requests
        verdicts[r.verdict] = verdicts.get(r.verdict, 0) + 1

        name = r.name[:33] + ".." if len(r.name) > 35 else r.name
        impl_stub = f"{r.impl_opcodes}/{r.stub_opcodes}"
        time_s = f"{r.uptime_ms / 1000:.1f}s" if r.uptime_ms else "—"
        milestones = " ".join(r.milestones) if r.milestones else "—"

        verdict_display = r.verdict.upper()
        if "CRASH" in r.milestones:
            verdict_display = "CRASH"

        print(f"  {name:<35} {r.total_requests:>7} {r.total_opcodes:>4} {impl_stub:>5} {time_s:>6} {milestones:<25} {verdict_display}")

    
    print(f"  {total_games} games | {total_requests:,} total requests | verdicts: {verdicts}")
    print(" ")
    print()


# Main

def main() -> int:
    parser = argparse.ArgumentParser(description="Run quark diagnostics on all Steam games")
    parser.add_argument("--timeout", type=int, default=30, help="Per-game timeout (seconds)")
    parser.add_argument("--skip-build", action="store_true", help="Skip cargo build")
    parser.add_argument("--games", type=str, default=None,
                       help="Comma-separated appids to test (default: all)")
    args = parser.parse_args()

    OUT_DIR.mkdir(parents=True, exist_ok=True)

    # Build once (not per-game)
    if not args.skip_build:
        log_info("Building quark stack...")
        ret = subprocess.run(
            ["cargo", "build", "--release", "-p", "triskelion"],
            cwd=REPO_ROOT,
        ).returncode
        if ret != 0:
            log_error("Build failed")
            return 1
        log_info("Build complete")

    # Discover games
    all_games = discover_games()
    if args.games:
        filter_ids = set(args.games.split(","))
        all_games = [g for g in all_games if g.appid in filter_ids]

    if not all_games:
        log_error("No games found")
        return 1

    log_info(f"Found {len(all_games)} games to test")
    for g in all_games:
        exe_name = g.exe.name if g.exe else "no exe"
        log_info(f"  {g.appid:>8}  {g.name}  ({exe_name})")
    print()

    # Run each game
    results: list[GameResult] = []
    for i, game in enumerate(all_games, 1):
        print()
        log_info(f"GAME {i}/{len(all_games)}: {game.name} ({game.appid})")

        if not game.exe:
            log_warn(f"  no .exe found — skipping")
            r = GameResult(appid=game.appid, name=game.name, verdict="no_exe")
            results.append(r)
            continue

        # Archive previous daemon log
        if DAEMON_LOG.exists():
            archive = OUT_DIR / f"{game.appid}_daemon.log"
            DAEMON_LOG.rename(archive) if not archive.exists() else None

        r = run_game(game, args.timeout, args.skip_build)
        results.append(r)

        # Archive this game's logs
        if DAEMON_LOG.exists():
            archive = OUT_DIR / f"{game.appid}_daemon.log"
            try:
                import shutil
                shutil.copy2(DAEMON_LOG, archive)
            except OSError:
                pass
        if OPCODE_STATS.exists():
            archive = OUT_DIR / f"{game.appid}_opcode_stats.txt"
            try:
                import shutil
                shutil.copy2(OPCODE_STATS, archive)
            except OSError:
                pass

        log_info(f"  result: {r.total_requests} requests, {r.total_opcodes} opcodes, verdict={r.verdict}")
        log_info(f"  milestones: {' '.join(r.milestones) if r.milestones else 'none'}")

    # Write Prometheus output
    prom_path = OUT_DIR / f"quark_diagnose_{datetime.now().strftime('%Y%m%d_%H%M%S')}.prom"
    write_prom(results, prom_path)
    log_info(f"Prometheus metrics: {prom_path}")

    # Summary
    print_summary(results)

    return 0


if __name__ == "__main__":
    sys.exit(main())
