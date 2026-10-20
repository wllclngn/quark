#!/usr/bin/env python3
"""Fast local repro loop for the #18 heap corruption — no Steam, no game.

Cross-compiles repro.exe (mingw) under /tmp/quark, launches it under quark's
proton entrypoint directly with the montauk observer armed, waits for
crash/exit, and writes montauk's abortpm report. Seconds per iteration vs a
10-minute Steam game run.

    python3 tests/run.py heap-repro             # build + run once
    python3 tests/run.py heap-repro --runs 5    # run N times (it's racy)
    python3 tests/run.py heap-repro --no-build  # skip recompile
"""

import argparse
import json
import os
import shutil
import signal
import subprocess
import sys
import time
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE.parent.parent))
from quark_common import (
    CACHE_DIR, COMPAT_DIR, LOG_DIR, STEAM_ROOT, SUITE_DIR,
    log_info, log_warn, log_error, log_interrupted, stop_observer, stop_prefix_wineserver,
)

PROTON = COMPAT_DIR / "proton"
PROBE_SRC = HERE.parent / "qemu" / "probes" / "gl_heap_stress.c"
WORK = LOG_DIR / "heaprepro"
EXE = WORK / "repro.exe"
# A throwaway prefix outside Steam's compatdata, on the compat tree's disk
# rather than the tmpfs under /tmp.
PFX = CACHE_DIR / "heaprepro"
# The probe writes its verdict to VERDICT, the launcher sends the game's
# stderr to GAME_STDERR, and the observer names its capture after the exe.
VERDICT = SUITE_DIR / "probe-results" / "gl_heap_stress.txt"
GAME_STDERR = LOG_DIR / "wine_stderr.log"
TRACE = LOG_DIR / "incidents" / "repro-io.bin"


class _Stop(Exception):
    """Raised from a SIGTERM/SIGINT handler so teardown runs on `timeout`/Ctrl-C."""


def build() -> bool:
    log_info(f"building {EXE} (mingw)")
    WORK.mkdir(parents=True, exist_ok=True)
    r = subprocess.run([
        "x86_64-w64-mingw32-gcc", "-O2", '-DPROBE_NAME="gl_heap_stress"', "-o", str(EXE), str(PROBE_SRC),
        "-lopengl32", "-lgdi32", "-luser32", "-lole32", "-lwinmm",
    ], capture_output=True, text=True)
    if r.returncode != 0:
        log_error(f"mingw build failed rc={r.returncode}: {r.stderr.strip()[-400:]}")
        return False
    return True


def _kill_by_prefix_env(prefix_path: str) -> int:
    """SIGKILL every process whose environ references the throwaway prefix.

    The wineserver forks+setsids into its own session so it detaches from the
    launcher's process group and survives a group kill. But every process the
    launch spawned (the wineserver,
    montauk, wine, repro.exe) inherits STEAM_COMPAT_DATA_PATH / WINEPREFIX
    pointing at this prefix, which is unique to the repro. Matching on it reaps
    the detached wineserver without ever touching a real game's wineserver."""
    needle = prefix_path.encode()
    killed = 0
    me = os.getpid()
    for pid in os.listdir("/proc"):
        if not pid.isdigit():
            continue
        p = int(pid)
        if p == me:
            continue
        try:
            with open(f"/proc/{pid}/environ", "rb") as f:
                if needle not in f.read():
                    continue
            os.kill(p, signal.SIGKILL)
            killed += 1
        except (OSError, ProcessLookupError):
            continue
    return killed


def teardown(proc) -> None:
    """Reap the whole launch: The observer first, so its capture is closed
    rather than cut, then the launcher's group, the prefix's wineserver and
    anything else still bound to the throwaway prefix."""
    stop_observer(TRACE)
    # The launcher's own session group (wine and its non-detached children).
    if proc is not None and proc.poll() is None:
        for sig in (signal.SIGTERM, signal.SIGKILL):
            try:
                os.killpg(proc.pid, sig)
            except ProcessLookupError:
                break
            try:
                proc.wait(timeout=5)
                break
            except subprocess.TimeoutExpired:
                continue
    stop_prefix_wineserver(PFX / "pfx")
    # The reliable layer: Anything (a detached wineserver, wine) still bound
    # to the throwaway prefix.
    n = _kill_by_prefix_env(str(PFX))
    if n:
        log_info(f"teardown: reaped {n} lingering process(es) bound to {PFX.name}")


def run_once(idx: int, timeout_s: int) -> str:
    """Launch repro.exe under quark; return 'CRASH', 'CLEAN', 'STALL' or
    'UNKNOWN'. Always reaps the spawned tree, even on stall/timeout/signal."""
    PFX.mkdir(parents=True, exist_ok=True)
    VERDICT.parent.mkdir(parents=True, exist_ok=True)
    VERDICT.unlink(missing_ok=True)
    TRACE.unlink(missing_ok=True)
    env = os.environ.copy()
    env["STEAM_COMPAT_DATA_PATH"] = str(PFX)
    env["STEAM_COMPAT_CLIENT_INSTALL_PATH"] = str(STEAM_ROOT)
    env["STEAM_COMPAT_APP_ID"] = "0"
    env["SteamAppId"] = "0"
    env["PROTON_VERB"] = "waitforexitandrun"
    env["QUARK_TRACE"] = "1"
    env["QUARK_TRACE_OUT"] = "1"
    env["MONTAUK_HEAP_STACK_SIZE"] = "288"

    launcher_log = WORK / f"run-{idx}-launcher.log"
    log_info(f"run {idx}: launching repro.exe under quark (timeout {timeout_s}s)")
    t0 = time.monotonic()
    stalled = False
    proc = None
    with open(launcher_log, "w") as ef:
        # start_new_session: The launcher leads its own session/pgroup so the
        # whole non-detached tree is killable together and a SIGTERM to this
        # script does not race-kill half of it.
        proc = subprocess.Popen([str(PROTON), "run", str(EXE)],
                                env=env, cwd="/tmp", stdout=subprocess.DEVNULL,
                                stderr=ef, start_new_session=True)
        try:
            proc.wait(timeout=timeout_s)
        except subprocess.TimeoutExpired:
            stalled = True
            log_warn(f"run {idx}: exceeded {timeout_s}s — STALL (the #18 stall variant); reaping")
        finally:
            teardown(proc)

    dt = time.monotonic() - t0
    txt = GAME_STDERR.read_text(errors="replace") if GAME_STDERR.exists() else ""
    crashed = any(s in txt for s in (
        "sysmalloc", "double free or corruption", "!prev",
        "Assertion", "free_dce"))
    clean = VERDICT.exists() and VERDICT.read_text(errors="replace").startswith("PASS ")
    if crashed:
        verdict = "CRASH"
    elif stalled:
        verdict = "STALL"
    elif clean:
        verdict = "CLEAN"
    else:
        verdict = "UNKNOWN"
    rc = proc.returncode if proc is not None else None
    log_info(f"run {idx}: {verdict} in {dt:.1f}s (rc={rc})")
    return verdict


def analyze() -> None:
    """montauk's abortpm report over the run's capture, as JSON beside it."""
    if not TRACE.exists():
        log_warn(f"no capture at {TRACE} (the observer did not attach)")
        return
    montauk = shutil.which("montauk")
    if not montauk:
        log_warn("montauk not on PATH; no report")
        return
    out = WORK / "abortpm.json"
    with out.open("w") as f:
        r = subprocess.run([montauk, "--analyze", str(TRACE), "--report", "abortpm", "--json"],
                           stdout=f, stderr=subprocess.PIPE, text=True)
    if r.returncode != 0:
        log_warn(f"montauk --analyze exited {r.returncode}: {r.stderr.strip()[-400:]}")
        return
    try:
        reports = json.loads(out.read_text()).get("reports", [])
    except ValueError:
        reports = []
    for report in reports:
        log_info(f"{report.get('name')}: {report.get('verdict')}")
    log_info(f"abortpm report: {out}")


def add_arguments(p: argparse.ArgumentParser) -> None:
    p.add_argument("--runs", type=int, default=1, help="run up to N times (it's racy); a hit stops the loop")
    p.add_argument("--no-build", action="store_true", help="launch the repro.exe already built")
    p.add_argument("--timeout", type=int, default=120,
                   help="per-run wall-clock limit (s); exceeding it is a STALL")


def run(args: argparse.Namespace) -> int:
    if not PROTON.exists():
        log_error(f"{PROTON} missing — install quark first")
        return 1
    if not args.no_build and not build():
        return 1
    if not EXE.exists():
        log_error(f"{EXE} missing — run once without --no-build")
        return 1

    # Turn SIGTERM/SIGINT (from a `timeout` wrapper or Ctrl-C) into an exception
    # so the active run's teardown still reaps the spawned tree.
    def _stop(_signum, _frame):
        raise _Stop()
    signal.signal(signal.SIGTERM, _stop)
    signal.signal(signal.SIGINT, _stop)

    hits = 0
    for i in range(1, args.runs + 1):
        try:
            verdict = run_once(i, args.timeout)
        except _Stop:
            log_interrupted()
            break
        # A crash or a stall is a #18 hit worth analyzing; both stop the loop.
        if verdict in ("CRASH", "STALL"):
            hits += 1
            analyze()
            break
    log_info(f"done: {hits}/{args.runs} hit (#18 crash or stall)")
    return 0
