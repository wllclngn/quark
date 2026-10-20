"""What a launch is measured with: the 50 ms watcher for the game's process
and window, montauk's GPU attribution for its painting and its hangs,
checkpoint screenshots, the frame-time log when MangoHud is present, and the
scheduler label."""
import json
import shutil
import subprocess
import threading
import time
from pathlib import Path
from quark_common import LOG_DIR, log_debug
from .base import clock_raw_ns, ns_to_ms


SCREENSHOTS_DIR = LOG_DIR / "screenshots"


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


FRAMETIMES = False  # set by --frametimes; MangoHud's log is read, nothing else of it


def _scheduler_label() -> str:
    """The loaded sched_ext scheduler's name, or eevdf when none is loaded."""
    try:
        name = Path("/sys/kernel/sched_ext/root/ops").read_text().strip()
        return name or "eevdf"
    except OSError:
        return "eevdf"


def _frametime_stats(log_dir) -> dict:
    """p99 frame time and the 1% low from the newest MangoHud per-frame log in
    log_dir, through sublimation. Column 2 of the log is frametime in ms; the
    values follow the column-names line, after MangoHud's system header, and
    the _summary.csv written beside the log is not one."""
    out = {"frametime_p99_ms": None, "fps_1pct_low": None}
    if not log_dir:
        return out
    csvs = sorted((p for p in Path(log_dir).glob("*.csv") if not p.name.endswith("_summary.csv")),
                  key=lambda p: p.stat().st_mtime)
    if not csvs:
        return out
    try:
        cols = subprocess.run(["sublimation", "field", "2", "-d", ",", str(csvs[-1])],
                              capture_output=True, text=True, timeout=60).stdout.splitlines()
        if "frametime" not in cols:
            return out
        values = "\n".join(c for c in cols[cols.index("frametime") + 1:] if c.strip()) + "\n"
        if not values.strip():
            return out
        p99 = subprocess.run(["sublimation", "quantile", "0.99"], input=values,
                             capture_output=True, text=True, timeout=60).stdout.strip()
        out["frametime_p99_ms"] = round(float(p99), 2)
        out["fps_1pct_low"] = round(1000.0 / float(p99), 1) if float(p99) > 0 else None
    except (subprocess.TimeoutExpired, ValueError, OSError, IndexError):
        pass
    return out


class _FirstSeen(threading.Thread):
    """Polls at 50 ms for the game's process and at 100 ms for its X window.

    Wine names the process and the window class after the exe, so both are
    matched on the exe's basename, case-insensitive, comm truncated to 15.
    Resolution is the poll period; montauk's exec events carry no timestamp
    yet, so this is the launch-to-exec source until they do."""

    def __init__(self, exe_path: str, app_id: str, t0_ns: int):
        super().__init__(daemon=True)
        self.exe = Path(exe_path).name.lower()
        self.comm = self.exe[:15]
        # Proton names the window class steam_app_<appid>; Wine, the exe.
        self.classes = (f'"{self.exe}"', f'"steam_app_{app_id}"')
        self.t0 = t0_ns
        self.exec_ms: float | None = None
        self.window_ms: float | None = None
        self._stop = threading.Event()

    def stop(self) -> None:
        self._stop.set()

    def game_pids(self) -> set[int]:
        """The processes named after the game's exe."""
        pids = set()
        for p in Path("/proc").iterdir():
            if not p.name.isdigit():
                continue
            try:
                if (p / "comm").read_text().strip().lower() == self.comm:
                    pids.add(int(p.name))
            except OSError:
                continue
        return pids

    def _scan_procs(self) -> bool:
        return bool(self.game_pids())

    def _scan_windows(self) -> bool:
        try:
            out = subprocess.run(["xwininfo", "-root", "-tree"], capture_output=True,
                                 text=True, timeout=2).stdout.lower()
        except (subprocess.TimeoutExpired, FileNotFoundError):
            return False
        return any(c in out for c in self.classes)

    def run(self) -> None:
        tick = 0
        while not self._stop.is_set():
            now = ns_to_ms(clock_raw_ns() - self.t0)
            if self.exec_ms is None and self._scan_procs():
                self.exec_ms = now
            if self.window_ms is None and tick % 2 == 0 and self._scan_windows():
                self.window_ms = now
            if self.exec_ms is not None and self.window_ms is not None:
                return
            tick += 1
            time.sleep(0.05)


class _GpuWatch(threading.Thread):
    """montauk's per-process GPU attribution over the game's own processes,
    one live snapshot at a time, each sampling about 1.5 s. The first
    snapshot in which the game draws on the GPU is the rendering signal, a
    real window painting where a root-window screenshot of a fullscreen game
    comes back black, and a game alive with almost no CPU since some moment
    after it is the hang signal. A stopped game's GPU attribution still
    reads a few percent in about half its snapshots, so the CPU decides a
    hang, and the rest of the tree, which keeps working while the game is
    stuck, is not counted. Neither needs a daemon log. A snapshot costs
    about 430 ms of CPU, so the watch waits for the game's exec and the
    launch-to-exec time it would disturb is taken."""

    QUIET_CPU_PCT = 1.0

    def __init__(self, first_seen: _FirstSeen, t0_ns: int):
        super().__init__(daemon=True)
        self.first_seen = first_seen
        self.t0 = t0_ns
        self.render_ms: float | None = None
        self.quiet_since_ms: float | None = None
        self._stop = threading.Event()

    def stop(self) -> None:
        self._stop.set()

    def run(self) -> None:
        montauk = shutil.which("montauk")
        if montauk is None:
            return
        while self.first_seen.exec_ms is None:
            if self._stop.wait(0.1):
                return
        while not self._stop.is_set():
            try:
                snap = json.loads(subprocess.run([montauk, "--json"], capture_output=True,
                                                 text=True, timeout=10).stdout)
                rows = snap["processes"]["anomaly_features"]
            except (subprocess.TimeoutExpired, OSError, ValueError, KeyError):
                return
            now = ns_to_ms(clock_raw_ns() - self.t0)
            pids = self.first_seen.game_pids()
            game = [f for f in rows if f.get("pid") in pids]
            gpu = sum(f.get("gpu_util_pct") or 0 for f in game)
            cpu = sum(f.get("cpu_pct") or 0 for f in game)
            log_debug(f"gpu watch: {now:.0f} ms pids={sorted(pids)} gpu={gpu}% cpu={cpu:.1f}%")
            if self.render_ms is None and gpu > 0:
                self.render_ms = now
            if self.render_ms is not None and game and cpu < self.QUIET_CPU_PCT:
                self.quiet_since_ms = self.quiet_since_ms or now
            else:
                self.quiet_since_ms = None
