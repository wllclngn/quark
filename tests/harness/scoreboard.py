"""The scoreboard: the going-to-PROD step, run once by hand when a release
is being prepared and never a layer of the suite.

It runs montauk's static lane first (harness/static.py): The lexer's
guards, each function's control flow graph and the static analyzer's
reachability, over the launcher, the installer and the suite. A block
unreachable from entry fails the step there, before any game launches.
Then every game goes through every tool on the same harness, taking the
GPU and the screen for the whole matrix, each game once per tool (twice
with a warm-up). Both land in scoreboard.md, scoreboard.json and
scoreboard.prom; `--static-only` writes the static part alone and launches
nothing.

Each cell is a full iterate.py run with --metrics-json. The table reports,
per game and tool, launch-to-exec and launch-to-window from the harness's
50 ms watcher, the process count and resident set at the end of the run,
and the verdict. Runs are serial by design: Two games share one GPU.

    python3 tests/scoreboard.py                         # six games, three tools
    python3 tests/scoreboard.py --games 2379780 --tools quark,proton-exp
    python3 tests/scoreboard.py --runs 3                # median of three
    python3 tests/scoreboard.py --static-only           # the static lane, no games
"""

import argparse
import json
import os
import re
import subprocess
import sys
import tempfile
from datetime import datetime
from pathlib import Path

from quark_common import PrometheusBuilder, get_version, git_describe, log_error, log_info, log_warn
from . import baselines, sched
from . import static as static_lane
from .tools import COMPAT_TOOLS

TESTS = Path(__file__).resolve().parent.parent
RUN = TESTS / "run.py"
OUT_MD = TESTS / "scoreboard.md"
OUT_JSON = TESTS / "scoreboard.json"
OUT_PROM = TESTS / "scoreboard.prom"

GAMES = {
    "367520": "Hollow Knight",
    "2379780": "Balatro",
    "379720": "DOOM (2016)",
    "1361510": "TMNT: Shredder's Revenge",
    "2529820": "Wedding Witch",
    "2218750": "Halls of Torment",
}
TOOLS = ["quark"]  # the field; Proton entries join it only by flag
COLUMNS = ("exec_ms", "window_ms", "render_ms", "procs", "rss_mb", "frametime_p99_ms", "fps_1pct_low",
           "ntsync_ops", "helper_execs")
TRACE_COLUMNS = ("ntsync_ops", "helper_execs")
FRAMETIMES = False
TRACE = False


def run_cell(appid: str, tool: str, timeout: int) -> dict | None:
    with tempfile.NamedTemporaryFile(suffix=".json", delete=False) as f:
        out_path = Path(f.name)
    out_path.unlink()
    cmd = [sys.executable, str(RUN), "launch", appid, "--tool", tool,
           "--timeout", str(timeout), "--skip-build", "--metrics-json", str(out_path)]
    if FRAMETIMES:
        cmd.append("--frametimes")
    if TRACE:
        # The traced pass reads ntsync ioctls and helper execs; the heap
        # classes would fill 177 MB of a RAM-backed /tmp per Balatro cell.
        cmd += ["--trace", "--trace-classes", "fork,exec,exit,comm,ntsync"]
    log_info(f"cell: {GAMES.get(appid, appid)} via {tool}")
    try:
        r = subprocess.run(cmd, capture_output=True, text=True, timeout=timeout + 120,
                           env=dict(os.environ, QUARK_CHILD="1"))
    except subprocess.TimeoutExpired:
        log_warn("cell: the launch did not finish; no metrics")
        return None
    if not out_path.exists():
        tail = [l for l in (r.stdout + r.stderr).strip().splitlines() if l.strip()][-3:]
        log_warn(f"cell: no metrics written (exit {r.returncode})")
        for line in tail:
            # The child's lines carry their own stamp and level; keep the message.
            log_warn(f"cell: {re.sub(r'^\[[0-9:]{8}\] \[[A-Z]+\]\s+', '', line)}")
        return None
    data = json.loads(out_path.read_text())
    out_path.unlink()
    return data


def median_cell(runs: list[dict]) -> dict:
    """Each column's median across the runs, through sublimation's group
    median, which interpolates between the two middle runs of an even count."""
    cell = dict(runs[-1])
    rows = "".join(f"{k} {r[k]!r}\n" for r in runs for k in COLUMNS if r.get(k) is not None)
    medians = {}
    if rows:
        out = subprocess.run(["sublimation", "group", "1", "median", "2"], input=rows,
                             capture_output=True, text=True, check=True).stdout
        for line in out.splitlines():
            k, v = line.split()
            medians[k] = int(v) if v.lstrip("-").isdigit() else float(v)
    for k in COLUMNS:
        cell[k] = medians.get(k)
    cell["verdicts"] = [r["verdict"] for r in runs]
    return cell


def fmt(v, unit: str = "") -> str:
    if v is None:
        return "n/a"
    if isinstance(v, float):
        return f"{v:,.0f}{unit}" if unit == " ms" else f"{v:,.1f}{unit}"
    return f"{v:,}{unit}"


def render(board: dict[str, dict[str, dict]], tools: list[str]) -> str:
    ft, tr = FRAMETIMES, TRACE
    scheds = sorted({c.get("scheduler", "?") for per_tool in board.values() for c in per_tool.values()})
    lines = [f"Scheduler: {', '.join(scheds) or 'unknown'}. Ratios are quark over the tool; "
             "lower is better for times, processes and RSS.", "",
             "| Game | Tool | Verdict | Launch to exec | Launch to window | Launch to render | Processes | RSS |"
             + (" Frame time p99 | 1% low |" if ft else "") + (" ntsync ops | helper execs |" if tr else ""),
             "|---|---|---|---|---|---|---|---|" + ("---|---|" if ft else "") + ("---|---|" if tr else "")]
    for appid, per_tool in board.items():
        base = per_tool.get(tools[0])
        for tool in tools:
            c = per_tool.get(tool)
            if not c:
                lines.append(f"| {GAMES.get(appid, appid)} | {tool} | no run | | | | | |")
                continue
            def ratio(k):
                if tool == tools[0] or not base or base.get(k) is None or c.get(k) in (None, 0):
                    return ""
                return f" ({base[k] / c[k]:.2f}x)"
            lines.append(
                f"| {GAMES.get(appid, appid)} | {tool} | {'/'.join(c['verdicts'])} | "
                f"{fmt(c['exec_ms'], ' ms')}{ratio('exec_ms')} | "
                f"{fmt(c['window_ms'], ' ms')}{ratio('window_ms')} | "
                f"{fmt(c.get('render_ms'), ' ms')}{ratio('render_ms')} | "
                f"{fmt(c['procs'])}{ratio('procs')} | {fmt(c['rss_mb'], ' MB')}{ratio('rss_mb')} |"
                + (f" {fmt(c.get('frametime_p99_ms'), ' ms')}{ratio('frametime_p99_ms')} | "
                   f"{fmt(c.get('fps_1pct_low'))} |" if ft else "")
                + (f" {fmt(c.get('ntsync_ops'))} | {fmt(c.get('helper_execs'))} |" if tr else ""))
    return "\n".join(lines) + "\n"


HELP = {
    "exec_ms": "launch to the game's exec, milliseconds",
    "window_ms": "launch to the game's first window, milliseconds",
    "render_ms": "launch to the first snapshot with the game's tree on the GPU, milliseconds",
    "procs": "processes in the game's tree at the end of the run",
    "rss_mb": "resident set of the game's tree at the end of the run, MB",
    "frametime_p99_ms": "frame time p99, milliseconds",
    "fps_1pct_low": "1% low frames per second",
    "ntsync_ops": "ntsync operations in the traced run",
    "helper_execs": "helper processes exec'd in the traced run",
}


def render_prom(board: dict[str, dict[str, dict]], static: dict) -> str:
    """The board as Prometheus gauges, one per cell and column, labelled by
    game, tool and scheduler, and the static lane's gauges beside them, so
    `montauk --analyze` can compare one release's run against another's."""
    pb = PrometheusBuilder("scoreboard")
    commit = git_describe()
    pb.info(version=get_version(), git_commit=commit.removesuffix("-dirty"),
            git_dirty=commit.endswith("-dirty"))
    for metric, value, help_text, labels in static_lane.gauges(static):
        pb.gauge(f"static_{metric}", value, help=help_text, labels=labels)
    for appid, per_tool in board.items():
        for key, c in per_tool.items():
            tool, _, sched_name = key.partition("@")
            labels = {"game": appid, "tool": tool, "scheduler": sched_name or c.get("scheduler", "unknown")}
            for k in COLUMNS:
                if c.get(k) is not None:
                    pb.gauge(baselines.metric_name(k), c[k], help=HELP[k], labels=labels)
    return pb.render()


def add_arguments(ap: argparse.ArgumentParser) -> None:
    ap.add_argument("--games", default=",".join(GAMES), help="comma-separated app ids")
    ap.add_argument("--tools", default="",
                    help="comma-separated Proton tools to add beside quark, e.g. proton-exp,proton11; "
                         "quark is always in the field")
    ap.add_argument("--all-tools", action="store_true",
                    help="add every Proton tool that is installed instead of a named list")
    ap.add_argument("--timeout", type=int, default=45, help="seconds per cell (default 45)")
    ap.add_argument("--runs", type=int, default=3,
                    help="runs per cell, median reported and each archived (default 3, the fewest "
                         "montauk's verdict over the archive needs)")
    ap.add_argument("--warmup", type=int, default=1,
                    help="uncounted runs per cell first, so a cold prefix or cache is not scored (default 1)")
    ap.add_argument("--frametimes", action="store_true",
                    help="add MangoHud frame-time columns (needs mangohud installed)")
    ap.add_argument("--trace", action="store_true",
                    help="capture each cell under montauk and add the ntsync and helper-exec columns")
    ap.add_argument("--static-only", action="store_true",
                    help="run montauk's static lane and write its reports; launch no game")
    ap.add_argument("--merge", action="store_true",
                    help="start from the board in scoreboard.json and add to it, so a tool or a "
                         "scheduler can be added without rerunning the rest")
    ap.add_argument("--schedulers", default="",
                    help="comma-separated schedulers to run the whole matrix under, e.g. "
                         "pandemonium,eevdf; the default is whichever is loaded now. Switching "
                         "stops or starts pandemonium.service through sudo and restores the "
                         "original at the end")


def run(args: argparse.Namespace) -> int:
    global FRAMETIMES, TRACE
    FRAMETIMES = args.frametimes
    TRACE = args.trace
    games = [g for g in args.games.split(",") if g]
    tools = list(TOOLS)
    if args.all_tools:
        tools += [t for t, p in COMPAT_TOOLS.items() if t != "quark" and p.exists()]
    else:
        for t in (x for x in args.tools.split(",") if x):
            if t not in COMPAT_TOOLS:
                log_warn(f"scoreboard: unknown tool {t}; known: {', '.join(COMPAT_TOOLS)}")
                return 2
            if t not in tools:
                tools.append(t)
    schedulers = [x for x in args.schedulers.split(",") if x] or [sched.current()]
    for name in schedulers:
        if name not in sched.KNOWN:
            log_warn(f"scoreboard: unknown scheduler {name}; known: {', '.join(sched.KNOWN)}")
            return 2
    columns = [f"{t}@{s}" if len(schedulers) > 1 else t for s in schedulers for t in tools]
    static = static_lane.analyze()
    if static is None:
        return 2
    static_md = static_lane.render_md(static)
    if static_lane.judge(static) or args.static_only:
        failed = bool(static["cfg"].get("unreachable"))
        # The last matrix stays in the report: Re-rendered from its JSON.
        last = json.loads(OUT_JSON.read_text()) if OUT_JSON.exists() else {}
        last_tools = sorted({k for per_tool in last.values() for k in per_tool})
        OUT_MD.write_text(static_md + (render(last, last_tools) if last else ""))
        OUT_PROM.write_text(render_prom(last, static))
        log_info(f"REPORT: {OUT_MD}")
        log_info(f"METRICS: {OUT_PROM}")
        if failed:
            log_error("scoreboard: unreachable code; fix it before the matrix runs")
        return 1 if failed else 0
    original = sched.current()
    board: dict[str, dict[str, dict]] = {}
    cell_runs: dict[tuple[str, str, str], list[dict]] = {}
    if "proton11" not in tools:
        log_warn("scoreboard: no proton11 arm (--tools proton11); quark's cells have no drift "
                 "control beside them in the baselines")
    if args.merge and OUT_JSON.exists():
        board = json.loads(OUT_JSON.read_text())
        have = sorted({k for per_tool in board.values() for k in per_tool})
        columns = [c for c in have if c not in columns] + columns
        log_info(f"scoreboard: merging into the board of {len(board)} game(s), columns {', '.join(have)}")
    try:
        for name in schedulers:
            if not sched.switch(name):
                return 2
            for appid in games:
                board.setdefault(appid, {})
                for tool in tools:
                    key = f"{tool}@{name}" if len(schedulers) > 1 else tool
                    runs = []
                    for _ in range(args.warmup):
                        run_cell(appid, tool, args.timeout)
                    for _ in range(args.runs):
                        cell = run_cell(appid, tool, args.timeout)
                        if cell:
                            runs.append(cell)
                    if runs:
                        cell = median_cell(runs)
                        if TRACE:
                            # A traced run's times carry the capture's cost: It
                            # gives its two columns, and the untraced times stand.
                            base = board[appid].get(key) or dict(cell, **{k: None for k in COLUMNS})
                            cell = dict(base, **{k: cell[k] for k in TRACE_COLUMNS})
                        board[appid][key] = cell
                        cell_runs[(appid, tool, name)] = runs
                    # Persist after every cell so a stopped matrix keeps its rows.
                    OUT_JSON.write_text(json.dumps(board, indent=1) + "\n")
                    OUT_PROM.write_text(render_prom(board, static))
                    OUT_MD.write_text(static_md + render(board, columns))
    finally:
        if sched.current() != original:
            sched.switch(original)
    tools = columns

    # Every run goes into the archive, then montauk judges the archive.
    archived = baselines.archive_runs(cell_runs, TRACE_COLUMNS if TRACE else COLUMNS, HELP, get_version(), git_describe(),
                                      datetime.now().strftime("%Y%m%d-%H%M%S"))
    baseline_md = baselines.render_md(rates=FRAMETIMES)
    OUT_MD.write_text(static_md + render(board, tools) + "\n" + baseline_md)
    OUT_JSON.write_text(json.dumps(board, indent=1) + "\n")
    OUT_PROM.write_text(render_prom(board, static))
    log_info(f"REPORT: {OUT_MD}")
    log_info(f"METRICS: {OUT_JSON}")
    log_info(f"METRICS: {OUT_PROM}")
    log_info(f"ARCHIVE: {len(archived)} run(s) in {baselines.ARCHIVE_DIR}")
    return 0

