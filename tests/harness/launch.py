"""The launch itself: build, install, launch, monitor, report, archive. One
game or every installed one. quark unless a flag names another tool."""
import os
import re
import shutil
import subprocess
import argparse
import json
import sys
import time
from datetime import datetime
from pathlib import Path
from quark_common import (COMPAT_DIR, LOG_DIR, REPO_ROOT, STEAMAPPS, STEAM_ROOT, discover_all_games, find_game,
                          log_debug, log_error, log_info, log_section, log_table, log_warn, register_exit_cleanup,
                          table_header, table_row)
from .base import DEFAULT_APPID, DEFAULT_TIMEOUT, LAUNCHER_STDERR_LOG, WINE_STDERR_LOG, WINE_STDOUT_LOG, clock_raw_ns, ns_to_ms
from . import metrics as metrics_mod
from .metrics import SCREENSHOT_CHECKPOINTS_S, _FirstSeen, _GpuWatch, _frametime_stats, _scheduler_label, take_screenshot
from .procs import _terminate_tree, _tree_pids, _tree_rss_mb, analyze_hang, cleanup_processes, snapshot_process, terminate_game
from .tools import COMPAT_TOOLS, _compat_data_path, build, install, tool_bin
from . import trace as trace_mod
from .trace import TRACE_BASE, _capture_stats, decode_trace, start_trace, stop_trace, trim_trace


_LAST_LAUNCH_STAMP: str = ""


def _report_stderr() -> None:
    """Wine stderr's errors, aborts and unhandled exceptions: One summary
    line with the count and where the log is; each line itself at DEBUG,
    since the full log is already on disk."""
    if not WINE_STDERR_LOG.exists():
        return
    try:
        text = WINE_STDERR_LOG.read_text(errors="replace")
    except OSError:
        return
    lines = [l.strip()[:200] for l in text.splitlines()
             if any(s in l.lower() for s in ("error", "abort", "assertion", "unhandled exception"))]
    if lines:
        log_warn(f"wine stderr: {len(lines)} error lines in {WINE_STDERR_LOG}")
        for line in lines:
            log_debug(f"wine: {line}")


CRASH_MARKERS = ("Unhandled exception", "Unhandled page fault", "page fault",
                 "wine: Assertion", "killed by signal", "(error)")


def _stderr_has_crash() -> bool:
    """Wine's own words for a process that died, from the game's stderr."""
    if not WINE_STDERR_LOG.exists():
        return False
    text = WINE_STDERR_LOG.read_text(errors="replace")
    return any(m in text for m in CRASH_MARKERS)


SHADER_ROOT: Path | None = None  # set by --shader-cache; Steam's steamapps/shadercache otherwise


def _steam_cache_env(shader: Path) -> dict[str, str]:
    """The cache variables a launch from Steam's library hands the tool, as
    Steam handed them to Balatro's: Every driver's cache, DXVK's state cache,
    Fossilize's layer and its dump path and the media caches, under the
    game's one folder. Steam makes the folders before the launch; Fossilize
    does not make its own."""
    for sub in ("nvidiav1", "fozpipelinesv6", "fozmediav1", "DXVK_state_cache"):
        (shader / sub).mkdir(parents=True, exist_ok=True)
    s = str(shader)
    return {
        "STEAM_COMPAT_SHADER_PATH": s,
        "STEAM_COMPAT_TRANSCODED_MEDIA_PATH": s,
        "STEAM_COMPAT_MEDIA_PATH": f"{s}/fozmediav1",
        "__GL_SHADER_DISK_CACHE": "1",
        "__GL_SHADER_DISK_CACHE_PATH": f"{s}/nvidiav1",
        "__GL_SHADER_DISK_CACHE_APP_NAME": "steamapp_shader_cache",
        "__GL_SHADER_DISK_CACHE_READ_ONLY_APP_NAME": "steam_shader_cache;steamapp_merged_shader_cache",
        "__GL_SHADER_DISK_CACHE_SKIP_CLEANUP": "1",
        "MESA_SHADER_CACHE_DIR": s,
        "MESA_SHADER_CACHE_MAX_SIZE": "5G",
        "MESA_GLSL_CACHE_DIR": s,
        "MESA_GLSL_CACHE_MAX_SIZE": "5G",
        "MESA_DISK_CACHE_SINGLE_FILE": "1",
        "MESA_DISK_CACHE_READ_ONLY_FOZ_DBS": "steam_cache,steam_precompiled",
        "AMD_VK_USE_PIPELINE_CACHE": "1",
        "AMD_VK_PIPELINE_CACHE_PATH": f"{s}/AMDv1",
        "AMD_VK_PIPELINE_CACHE_FILENAME": "steamapp_shader_cache",
        "DXVK_STATE_CACHE_PATH": f"{s}/DXVK_state_cache",
        "ENABLE_VK_LAYER_VALVE_steam_fossilize_1": "1",
        "STEAM_FOSSILIZE_DUMP_PATH": f"{s}/fozpipelinesv6/steamapprun_pipeline_cache",
        "STEAM_FOSSILIZE_DUMP_PATH_READ_ONLY": "$bucketdir/steam_pipeline_cache.foz;$bucketdir/steamapp_pipeline_cache.foz",
        "FOSSILIZE_APPLICATION_INFO_FILTER_PATH": str(STEAM_ROOT / "fossilize_engine_filters.json"),
    }


def _launch_env(app_id: str, exe_path: str, compat_tool: str, timeout: int) -> tuple[dict, Path | None]:
    """The environment Steam gives a compatibility tool, with the session's
    display and IPC variables and, on scoreboard runs, MangoHud's frame-time
    log. Returns it and the frame-time directory, if any."""
    env = os.environ.copy()

    # Core Steam-compat vars (what Steam ALWAYS sets)
    # quark's proton-replacement and downstream Wine/Mono code read these.
    # Missing any of these makes Mono's startup hit the
    # `g_path_is_absolute(NULL)` assertion because path-construction code
    # tries to concatenate with an empty/missing base.
    compat_data = _compat_data_path(app_id, compat_tool)
    compat_data.mkdir(parents=True, exist_ok=True)  # Proton 11 expects Steam to have made it
    env["STEAM_COMPAT_DATA_PATH"] = str(compat_data)
    env["STEAM_COMPAT_CLIENT_INSTALL_PATH"] = str(STEAM_ROOT)
    env["STEAM_COMPAT_APP_ID"] = app_id
    env["SteamAppId"] = app_id
    env["SteamGameId"] = app_id

    # Game install path (Mono uses this for assembly probing). Derive from
    # the exe path so it works for any appid.
    install_path = str(Path(exe_path).parent)
    env["STEAM_COMPAT_INSTALL_PATH"] = install_path
    env["STEAM_COMPAT_APP_LIBRARY_PATH"] = install_path
    env["STEAM_COMPAT_LIBRARY_PATHS"] = str(STEAMAPPS)

    # Compat-tool paths (where Proton/quark itself lives)
    env["STEAM_COMPAT_TOOL_PATHS"] = str(COMPAT_DIR)
    env["STEAM_COMPAT_MOUNTS"] = ""  # Steam sometimes sets this empty

    shader_path = (SHADER_ROOT or STEAMAPPS / "shadercache") / app_id
    env.update(_steam_cache_env(shader_path))

    # Proton verb hint — some downstream code branches on this even when
    # we invoke `proton run` directly (e.g., it controls whether Proton
    # sets up the in-prefix Steam shim differently).
    env["PROTON_VERB"] = "waitforexitandrun"

    # Debug / driver passthrough
    env["WINEDEBUG"] = os.environ.get(
        "QUARK_WINEDEBUG", "+driver,+loaddll,+wgl,+x11drv,+system,err")

    # Display, session, and IPC vars that Wine/Mono read at startup.
    # DBUS_SESSION_BUS_ADDRESS in particular: Missing it makes some Mono
    # initializers store NULL for the session-bus path and crash later
    # when something probes it.
    for var in ("DISPLAY", "WAYLAND_DISPLAY", "XDG_RUNTIME_DIR",
                "XAUTHORITY", "DBUS_SESSION_BUS_ADDRESS",
                "DBUS_SYSTEM_BUS_ADDRESS", "XDG_DATA_DIRS",
                "XDG_CONFIG_DIRS", "XDG_CONFIG_HOME", "XDG_DATA_HOME",
                "XDG_CACHE_HOME", "XDG_SESSION_TYPE", "XDG_CURRENT_DESKTOP",
                "LANG", "LC_ALL", "LC_CTYPE", "LC_MESSAGES",
                "PULSE_SERVER", "PULSE_RUNTIME_PATH", "QUARK_VERBOSE"):
        if var in os.environ:
            env[var] = os.environ[var]

    # Don't override EGL vendor — let Wine use GLX via UseEGL=N registry key
    env.pop("__EGL_VENDOR_LIBRARY_FILENAMES", None)

    # Frame times: MangoHud's per-frame log, overlay transparent, on
    # scoreboard runs only. MangoHud 0.8 logs nothing under no_display, so
    # the HUD stays on with no text or background drawn. Its CPU, GPU and
    # thermal columns stay unused; montauk owns those. Both sides of an A/B
    # carry the layer.
    frametime_dir = None
    if metrics_mod.FRAMETIMES:
        if shutil.which("mangohud"):
            frametime_dir = LOG_DIR / "frametimes" / f"{app_id}_{_LAST_LAUNCH_STAMP}_{compat_tool}"
            frametime_dir.mkdir(parents=True, exist_ok=True)
            env["MANGOHUD"] = "1"
            # The log is written when its duration ends, so it ends ten seconds
            # before the cell's kill; a duration equal to the cell never writes.
            env["MANGOHUD_CONFIG"] = (f"alpha=0,background_alpha=0,output_folder={frametime_dir},"
                                      f"autostart_log=1,log_duration={max(5, timeout - 10)},log_interval=0")
        else:
            log_warn("frametimes: mangohud not on PATH; frame-time columns will be n/a")
    return env, frametime_dir


STEAM_GAME_LOG = STEAM_ROOT / "logs" / "gameprocess_log.txt"
STEAM_SHIM = "steam.exe"  # the tool's in-prefix shim, which registers with the client on its own


def _steam_log_end() -> int:
    try:
        return STEAM_GAME_LOG.stat().st_size
    except OSError:
        return 0


def _steam_tracked(app_id: str, pids: set, log_from: int) -> bool:
    """Whether Steam's client tracks a process of the game's tree, from the
    lines its own log gained since the launch began. The tool's steam.exe
    registers whether the game calls the Steam API or not, so it counts for
    nothing; any other process registers only when its SteamAPI_Init reached
    a running client."""
    try:
        with STEAM_GAME_LOG.open("rb") as f:
            if f.seek(0, 2) < log_from:
                log_from = 0  # Steam started a new log
            f.seek(log_from)
            text = f.read().decode(errors="replace")
    except OSError:
        return False
    for m in re.finditer(rf"AppID {app_id} adding PID (\d+) as a tracked process", text):
        pid = int(m.group(1))
        if pid not in pids:
            continue
        try:
            comm = Path(f"/proc/{pid}/comm").read_text().strip()
        except OSError:
            continue
        if comm != STEAM_SHIM:
            return True
    return False


def _milestones() -> list[str]:
    """What Wine's stderr shows the game reached so far.

    IMPORTANT: Every milestone here must be a TRUE positive — substring
    matches that fire on error screens, helper windows, or unrelated
    codepaths are misleading and have caused multiple "looks playable"
    hallucinations. The bar:
      - "CRASH" requires an actual exit/SIGSEGV, not "Unhandled" which
        appears in stub fixme lines that don't crash anything
    """
    try:
        stderr_text = WINE_STDERR_LOG.read_text(errors="replace") if WINE_STDERR_LOG.exists() else ""
    except OSError:
        return []
    milestones = []
    if "winex11.drv" in stderr_text:
        milestones.append("x11drv")
    if "XRandR" in stderr_text:
        milestones.append("XRandR")
    if "NO GAME" in stderr_text or "love" in stderr_text.lower():
        milestones.append("LÖVE")

    # CRASH: A real exit from the launcher with non-zero status, OR
    # an actual SIGSEGV trace. "Unhandled" alone is too noisy.
    if re.search(r'\[quark\] exit: [1-9]', stderr_text) or "SIGSEGV" in stderr_text:
        milestones.append("CRASH")
    # LOVE error screen specifically — different from a quark crash
    if 'love "callbacks.lua"' in stderr_text or "module 'engine/object'" in stderr_text:
        milestones.append("LOVE_ERROR")

    if any(f"err:service" in l for l in stderr_text.splitlines()[-5:]):
        milestones.append("SVC_ERR")
    return milestones


HANG_QUIET_MS = 10_000  # a game that rendered, then ran on no CPU this long, is hung


def _monitor(proc, app_id: str, compat_tool: str, t0: int, timeout: int, gpu: _GpuWatch,
             steam_log_from: int) -> tuple:
    """Poll the launch every two seconds until the timeout: Its process tree,
    the checkpoint screenshots, the milestones in Wine's stderr, STEAM_API
    from Steam's own log, and RENDER and HANG from montauk's GPU
    attribution. The launcher waits on the game,
    so its exit with the tree empty means the game is gone, and the watch
    ends there. Returns the launcher's exit code then, or None at the
    timeout, the elapsed seconds and the milestones."""
    poll_interval = 2
    milestones_seen: set = set()
    for _ in range(timeout // poll_interval):
        time.sleep(poll_interval)
        ret = proc.poll()
        elapsed = ns_to_ms(clock_raw_ns() - t0) / 1000
        pids = sorted(_tree_pids(app_id, compat_tool))
        if ret is not None and not pids:
            return ret, elapsed, milestones_seen

        # Screenshot checkpoints — capture full screen at fixed elapsed times
        # so we can see what was actually rendered without the user typing it.
        # Files land in LOG_DIR/screenshots/ with the same {appid}_{stamp}
        # prefix as the archived wine_stderr.log.
        for cp in SCREENSHOT_CHECKPOINTS_S:
            if cp <= elapsed < cp + poll_interval:
                shot = take_screenshot(app_id, _LAST_LAUNCH_STAMP, cp)
                if shot:
                    log_info(f"screenshot: {shot.name}")
                break

        milestones = _milestones()
        if "STEAM_API" in milestones_seen or _steam_tracked(app_id, set(pids), steam_log_from):
            milestones.append("STEAM_API")
        if gpu.render_ms is not None:
            milestones.append("RENDER")
        if gpu.quiet_since_ms is not None and elapsed * 1000 - gpu.quiet_since_ms >= HANG_QUIET_MS:
            if "HANG" not in milestones_seen:
                log_warn(f"[{elapsed:.0f}s] the game has run on no CPU for "
                         f"{HANG_QUIET_MS // 1000} s since it rendered: hung")
            milestones.append("HANG")
        milestones_seen.update(milestones)
        pid_str = f" pids={pids}" if pids else ""
        ms_str = f" | {' '.join(milestones)}" if milestones else ""
        log_info(f"[{elapsed:.0f}s]{pid_str}{ms_str}")
    return None, ns_to_ms(clock_raw_ns() - t0) / 1000, milestones_seen


def _finish_metrics(metrics: dict, watcher, gpu: _GpuWatch, pids: set, frametime_dir: Path | None) -> None:
    """The launch's metrics once it is over: Its first exec, window and GPU
    draw, its tree's size and memory and its frame times."""
    watcher.stop()
    gpu.stop()
    metrics["exec_ms"] = watcher.exec_ms
    metrics["window_ms"] = watcher.window_ms
    metrics["render_ms"] = gpu.render_ms
    metrics["procs"] = len(pids)
    metrics["rss_mb"] = round(_tree_rss_mb(pids), 1)
    metrics.update(_frametime_stats(frametime_dir))
    log_info(f"metrics: exec={metrics['exec_ms']}ms window={metrics['window_ms']}ms "
             f"render={metrics['render_ms']}ms procs={metrics['procs']} rss={metrics['rss_mb']}MB")


def _teardown(proc, compat_tool: str, tree: set | None, trace_proc, trace_dir, metrics: dict) -> None:
    """The launch's end: The game tree when it outlived the timeout, then the
    trace, its report set and anything left of the session."""
    if tree is not None:
        if compat_tool == "quark":
            terminate_game(proc)
        else:
            _terminate_tree(tree - {proc.pid})
            proc.kill()
            proc.wait()
    stop_trace(trace_proc)
    decode_trace(trace_dir)
    metrics.update(_capture_stats(trace_dir))
    trim_trace(trace_dir)
    cleanup_processes()


def launch_and_monitor(app_id: str, game_name: str, exe_path: str,
                       timeout: int, compat_tool: str = "quark",
                       trace: bool = False) -> tuple:
    """Launch game, monitor, return (verdict, elapsed, snapshots, err, milestones, metrics)."""
    global _LAST_LAUNCH_STAMP
    _LAST_LAUNCH_STAMP = datetime.now().strftime("%Y%m%d-%H%M%S")
    log_info(f"launch: {game_name} ({app_id}) via {compat_tool}")
    log_info(f"exe: {exe_path}")

    cleanup_processes()
    env, frametime_dir = _launch_env(app_id, exe_path, compat_tool, timeout)

    # Optional montauk trace, started before the workload so BPF programs
    # attach in time to catch early file syscalls (PHYSFS archive scan).
    trace_proc = None
    trace_dir = None
    if trace:
        # Pattern matches the game exe basename (e.g. "Balatro.exe").
        # Linux comm is 15 chars max. Match an EARLIER process in the launch
        # chain (wine-preloader, the immediate parent of the game) so that
        # auto_track_pid catches the root before exec, and handle_fork's
        # parent→child inheritance gives us every descendant from the start.
        # Otherwise we miss the first 15-20s of allocations because the
        # game's comm doesn't match the pattern until well after Mono init.
        # The launcher is exec'd through the compat dir's `proton` symlink,
        # so its comm is "proton", and montauk follows its whole tree from
        # there.
        exe_name = "proton"
        trace_proc, trace_dir = start_trace(
            app_id, compat_tool, _LAST_LAUNCH_STAMP, exe_name)

    steam_log_from = _steam_log_end()
    t0 = clock_raw_ns()

    try:
        # Capture both stdout and stderr to files. LOVE2D writes Lua errors
        # to stdout via lua's print() and via callbacks.lua's error handler,
        # so capturing stdout is essential for diagnosing fused-exe issues.
        proc = subprocess.Popen(
            [str(tool_bin(compat_tool)), "run", exe_path],
            env=env,
            cwd="/tmp",
            # The launcher's own stderr. The launcher reopens wine_stderr.log
            # for the game with truncation, so sharing that file would wipe
            # the launcher's lines (its verbose timing among them).
            stdout=open(WINE_STDOUT_LOG, "w"),
            stderr=open(LAUNCHER_STDERR_LOG, "w"),
            start_new_session=True,  # own process group so we can kill the tree
        )
    except OSError as e:
        stop_trace(trace_proc)
        decode_trace(trace_dir)
        return ("launch_fail", 0, [], f"Failed to start: {e}", set(), {})

    watcher = _FirstSeen(exe_path, app_id, t0)
    watcher.start()
    gpu = _GpuWatch(watcher, t0)
    gpu.start()
    metrics = {"appid": app_id, "tool": compat_tool, "scheduler": _scheduler_label(),
               "exec_ms": None, "window_ms": None, "render_ms": None, "procs": 0, "rss_mb": 0.0}
    ret, elapsed, milestones_seen = _monitor(proc, app_id, compat_tool, t0, timeout, gpu, steam_log_from)

    if ret is not None:
        # The launcher's code alone cannot score a game gone before the
        # timeout: steam.exe returns 0 for a game that died by signal, so
        # Wine's own crash lines in stderr decide alongside it.
        verdict = "crash" if ret != 0 or _stderr_has_crash() else "exit"
        log_warn(f"[{elapsed:.0f}s] game tree gone, launcher exit {ret}: {verdict}")
        tree, snaps, err = set(), [], f"launcher exit {ret}"
    else:
        verdict = "timeout"
        tree = _tree_pids(app_id, compat_tool)
        snaps = [snapshot_process(p) for p in sorted(tree)]
        err = ""
    _report_stderr()
    _finish_metrics(metrics, watcher, gpu, tree, frametime_dir)
    _teardown(proc, compat_tool, tree if ret is None else None, trace_proc, trace_dir, metrics)
    return (verdict, elapsed, snaps, err, milestones_seen, metrics)


# Report
def print_report(verdict: str, elapsed: float, snapshots: list):
    log_info(f"verdict: {verdict} ({elapsed:.1f}s)")

    # Display driver
    stderr_text = WINE_STDERR_LOG.read_text() if WINE_STDERR_LOG.exists() else ""
    x11_loaded = "winex11.drv" in stderr_text or "x11drv" in stderr_text
    log_info(f"display: x11={x11_loaded}")

    # Hang analysis
    if snapshots:
        analysis = analyze_hang(snapshots)
        if analysis:
            for line in analysis.split("\n"):
                if line.strip():
                    log_info(f"hang: {line.strip()}")


def run_single_game(appid, game_name, exe_path, timeout, skip_build,
                    compat_tool: str = "quark", trace: bool = False):
    """Run one game through iterate cycle. Returns result dict."""
    # Build is only relevant for quark; stock Proton has nothing to compile.
    if compat_tool == "quark" and not build(skip=skip_build):
        return {"appid": appid, "name": game_name, "verdict": "build_fail"}

    if not install(compat_tool=compat_tool):
        return {"appid": appid, "name": game_name, "verdict": "install_fail"}

    result = launch_and_monitor(appid, game_name, exe_path, timeout,
                                compat_tool=compat_tool, trace=trace)
    verdict, elapsed, snapshots, err, milestones, metrics = result
    milestones = sorted(milestones)

    # Check wine_stderr for key errors
    stderr_text = ""
    if WINE_STDERR_LOG.exists():
        stderr_text = WINE_STDERR_LOG.read_text(errors="replace")
    has_crash = "Unhandled page fault" in stderr_text or "page fault" in stderr_text

    # Archive — reuse the launch stamp so archived logs and screenshots
    # captured during the run share the same {appid}_{stamp}_{tool} prefix.
    stamp = _LAST_LAUNCH_STAMP or datetime.now().strftime("%Y%m%d-%H%M%S")
    archive_dir = LOG_DIR / "runs"
    archive_dir.mkdir(parents=True, exist_ok=True)
    prefix = f"{appid}_{stamp}_{compat_tool}"
    for src, suffix in [(WINE_STDERR_LOG, "wine_stderr.log"), (WINE_STDOUT_LOG, "wine_stdout.log"),
                         (LOG_DIR / "launcher_env.txt", "launcher_env.txt"),
                         (LAUNCHER_STDERR_LOG, "launcher_stderr.log")]:
        if src.exists() and src.stat().st_size > 0:
            dst = archive_dir / f"{prefix}_{suffix}"
            try:
                shutil.copy2(src, dst)
            except OSError:
                pass

    # Trace archive: Copy montauk's log dir alongside if it exists.
    if trace:
        src_trace = TRACE_BASE / f"{appid}_{stamp}_{compat_tool}"
        if src_trace.exists():
            dst_trace = archive_dir / f"{prefix}_trace"
            try:
                if dst_trace.exists():
                    shutil.rmtree(dst_trace)
                shutil.move(str(src_trace), str(dst_trace))
                log_info(f"trace: archived → {dst_trace}")
            except OSError as e:
                log_warn(f"trace: archive copy failed: {e}")

    return {
        "appid": appid,
        "name": game_name,
        "verdict": verdict,
        "elapsed": elapsed,
        "steam_api": "STEAM_API" in milestones,
        "crash": has_crash,
        "milestones": milestones,
        "compat_tool": compat_tool,
        "metrics": metrics,
    }


def run_all_games(timeout, skip_build, compat_tool: str = "quark", trace: bool = False) -> int:
    """Discover and test all Steam games and print the summary table.
    Returns the number that crashed."""
    games = discover_all_games()
    if not games:
        log_error("no games found")
        return 0

    log_info(f"found {len(games)} games to test (compat_tool={compat_tool}, trace={trace})")
    for appid, name, _ in games:
        log_info(f"game: {appid} {name}")

    # Build/deploy once. Stock Proton has nothing to compile or copy.
    if compat_tool == "quark":
        if not build(skip=skip_build):
            log_error("build failed")
            return 0
    if not install(compat_tool=compat_tool):
        log_error("install failed")
        return 0

    # Verify patches only when running against quark (otherwise irrelevant).
    if compat_tool == "quark":
        log_section("patches", "verify")
        r = subprocess.run([sys.executable, str(REPO_ROOT / "tests" / "verify_patches.py")],
                           capture_output=True, text=True, env=dict(os.environ, QUARK_CHILD="1"))
        # The child's lines are already in the house format: Relay only its
        # failures and its counting line, verbatim.
        for line in r.stdout.splitlines():
            if "[ERROR]" in line or "PATCHES:" in line:
                print(line, flush=True)
        if r.returncode != 0:
            log_warn("patch verification had failures (continuing anyway for dev testing)")

    results = []
    for i, (appid, name, exe) in enumerate(games):
        log_section(name, "launch")
        log_info(f"game {i + 1} of {len(games)}, appid {appid}")
        results.append(run_single_game(appid, name, exe, timeout, skip_build=True,
                                       compat_tool=compat_tool, trace=trace))

    widths = [10, 6, 6]
    rows = [table_header("GAME", ["VERDICT", "STEAM", "CRASH"], col_w=widths)]
    for r in results:
        rows.append(table_row(r["name"][:28], [r["verdict"], "live" if r.get("steam_api") else "no",
                                               "yes" if r.get("crash") else "no"], col_w=widths))
    log_table(rows)

    ok = sum(1 for r in results if r["verdict"] == "timeout" and not r.get("crash"))
    crashed = sum(1 for r in results if r.get("crash"))
    (log_warn if crashed else log_info)(f"LAUNCH: {len(results)} games, {ok} running, {crashed} crashed")
    return crashed


# THE COMMAND LINE: tests/run.py launch

def add_arguments(parser: argparse.ArgumentParser) -> None:
    parser.add_argument("appid", nargs="?", default=DEFAULT_APPID,
                        help=f"Steam app id (default {DEFAULT_APPID}, Balatro)")
    parser.add_argument("--all", action="store_true", help="every installed Windows game")
    parser.add_argument("--timeout", type=int, default=DEFAULT_TIMEOUT,
                        help=f"seconds to run before the verdict (default {DEFAULT_TIMEOUT})")
    parser.add_argument("--skip-build", action="store_true", help="launch the quark already built")
    parser.add_argument("--exe", default=None, help="launch this exe instead of the one found for the app")
    parser.add_argument("--trace", action="store_true",
                        help="capture the launch under montauk (needs cap_bpf and cap_perfmon on it)")
    parser.add_argument("--trace-classes", metavar="LIST",
                        help="with --trace, capture only these event classes (montauk's --trace-classes)")
    parser.add_argument("--frametimes", action="store_true",
                        help="log frame times through MangoHud, overlay transparent, when it is installed")
    parser.add_argument("--shader-cache", metavar="DIR", default=None,
                        help="keep the game's shader caches under DIR/<appid> instead of Steam's "
                             "steamapps/shadercache, for a launch from an empty cache")
    parser.add_argument("--metrics-json", default=None, help="write the verdict and metrics to this file")
    parser.add_argument("--keep-trace", action="store_true",
                        help="with --trace, keep the raw 200 MB capture after its report set is written")
    parser.add_argument("--tool", choices=list(COMPAT_TOOLS), default="quark",
                        help="the compatibility tool to launch through. quark is the tool under "
                             "test; a Proton entry is an A/B run against stock Proton and nothing "
                             "else in the suite knows those exist")


def run(args: argparse.Namespace) -> int:
    global SHADER_ROOT
    # An interrupted or failed launch must not leave the game's tree behind.
    register_exit_cleanup(cleanup_processes)
    SHADER_ROOT = Path(args.shader_cache) if args.shader_cache else None
    metrics_mod.FRAMETIMES = args.frametimes
    trace_mod.KEEP_TRACE = args.keep_trace
    trace_mod.CLASSES = args.trace_classes
    tool = args.tool

    if args.all:
        crashed = run_all_games(args.timeout, args.skip_build, compat_tool=tool, trace=args.trace)
        return 1 if crashed else 0

    game_name, exe_path = find_game(args.appid)
    if args.exe:
        exe_path = args.exe
    if not exe_path:
        log_error(f"could not find game exe for app {args.appid}")
        return 1

    if tool == "quark" and not build(skip=args.skip_build):
        return 1
    if not install(compat_tool=tool):
        return 1

    result = launch_and_monitor(args.appid, game_name, exe_path, args.timeout,
                                compat_tool=tool, trace=args.trace)
    verdict, elapsed, snapshots, err, milestones, run_metrics = result
    milestones = sorted(milestones)

    # The metrics file is written for every verdict: A crash under one tool
    # is a scoreboard row, not a missing cell.
    print_report(verdict, elapsed, snapshots)
    if args.metrics_json:
        out = dict(run_metrics, verdict=verdict, elapsed_s=round(elapsed, 1),
                   milestones=milestones, name=game_name, error=err)
        Path(args.metrics_json).write_text(json.dumps(out, indent=1) + "\n")
    if err:
        log_error(err)
        return 1

    # Archive the run's logs under the launch stamp, so logs, screenshots and
    # the capture share one {appid}_{stamp}_{tool} prefix.
    stamp = _LAST_LAUNCH_STAMP or datetime.now().strftime("%Y%m%d-%H%M%S")
    archive_dir = LOG_DIR / "runs"
    archive_dir.mkdir(parents=True, exist_ok=True)
    prefix = f"{args.appid}_{stamp}_{tool}"
    for src, suffix in [(WINE_STDERR_LOG, "wine_stderr.log"), (WINE_STDOUT_LOG, "wine_stdout.log"),
                        (LOG_DIR / "launcher_env.txt", "launcher_env.txt"),
                        (LAUNCHER_STDERR_LOG, "launcher_stderr.log")]:
        if src.exists() and src.stat().st_size > 0:
            try:
                shutil.copy2(src, archive_dir / f"{prefix}_{suffix}")
            except OSError:
                pass
    if args.trace:
        src_trace = TRACE_BASE / f"{args.appid}_{stamp}_{tool}"
        if src_trace.exists():
            dst_trace = archive_dir / f"{prefix}_trace"
            try:
                if dst_trace.exists():
                    shutil.rmtree(dst_trace)
                shutil.move(str(src_trace), str(dst_trace))
            except OSError as e:
                log_warn(f"trace: archive copy failed: {e}")
    log_info(f"logs archived: {LOG_DIR}/runs/{prefix}_*")

    return 1 if verdict == "crash" else 0
