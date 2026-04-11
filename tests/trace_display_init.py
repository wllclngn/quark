#!/usr/bin/env python3
"""Side-by-side display initialization trace: quark vs stock wineserver.

Runs Balatro twice — once with our daemon, once with stock wineserver — and
captures the exact display device registration sequence. Diffs the two to
identify what our daemon does differently.

Usage:
    python3 tests/trace_display_init.py
    python3 tests/trace_display_init.py --timeout 20
    python3 tests/trace_display_init.py --quark-only
    python3 tests/trace_display_init.py --stock-only
"""

import argparse
import os
import signal
import subprocess
import sys
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from util import kill_quark_processes, STEAM_ROOT

# ── Paths ─────────────────────────────────────────────────────────────

REPO_ROOT = Path(__file__).resolve().parent.parent
STEAMAPPS = STEAM_ROOT / "steamapps"
COMPAT_DIR = STEAM_ROOT / "compatibilitytools.d/quark"
PROTON_BIN = COMPAT_DIR / "proton"
GAME_EXE = STEAMAPPS / "common/Balatro/Balatro.exe"
COMPAT_DATA = STEAMAPPS / "compatdata/2379780"

TRACE_DIR = Path("/tmp/quark/display_trace")

# Wine debug channels that matter for display init
WINEDEBUG = ",".join([
    "+system",      # add_gpu, commit_gpu, write_gpu_to_registry, read_gpu, monitor_release
    "+x11drv",      # X11DRV_UpdateDisplayDevices, DisplayDevices_SetHandler
    "+driver",      # load_desktop_driver, trying driver
    "+display",     # display cache, update_display_cache
    "+wgl",         # OpenGL init, GLX, pixel formats
    "+loaddll",     # DLL loading (winex11.drv, opengl32)
    "err",          # all errors
])

# Keywords to extract from Wine stderr for display init analysis
DISPLAY_KEYWORDS = [
    "add_gpu", "commit_gpu", "write_gpu_to_registry", "read_gpu_from_registry",
    "add_source", "commit_source", "add_monitor", "commit_monitor",
    "update_display_devices", "commit_display_devices", "update_display_cache",
    "lock_display_devices", "Failed to read display config", "Failed to update",
    "Failed to write gpu", "Failed to find",
    "X11DRV_UpdateDisplayDevices", "X11DRV_DisplayDevices_SetHandler",
    "DisplayDevices_SetHandler", "GPU count", "adapter count", "monitor count",
    "load_desktop_driver", "trying driver", "winex11.drv",
    "GLX is up", "GL version", "Direct rendering",
    "enum_device_keys", "enum_gpus", "enum_monitors",
    "read_source_from_registry", "read_monitor_from_registry",
    "set_winstation_monitors", "monitor_serial",
    "prepare_devices", "get_display_device_init_mutex",
    "create_gpu_device_key", "guid",
    "StateFlags", "VideoID", "GPUID", "SymbolicLinkValue",
    "register_extension", "init_pixel_formats",
    "RegisterTouchWindow", "create_whole_window", "create_win_data",
    "WindowPosChanged", "drawable_create", "opengl_drawable",
    "Class.*4D36E968", "Class.*4d36e968",
    "err:", "warn:",
]

# Registry operations to extract from daemon.log
REGISTRY_KEYWORDS = [
    "create_key.*Class", "create_key.*Video", "create_key.*DISPLAY",
    "create_key.*PCI", "create_key.*Device Parameters",
    "set_key_value.*Class", "set_key_value.*Driver",
    "set_key_value.*StateFlags", "set_key_value.*VideoID",
    "set_key_value.*GPUID", "set_key_value.*SymbolicLink",
    "set_key_value.*GraphicsDriver", "set_key_value.*DeviceDesc",
    "set_key_value.*AdapterLuid", "set_key_value.*DriverVersion",
    "open_key.*Class.*4D36E968", "open_key.*Class.*4d36e968",
    "open_key MISS.*0000",
    "DEVICEMAP.*VIDEO", "Device.Video",
    "set_winstation_monitors",
    "get_desktop_window",
    "enum_key.*Class", "enum_key.*Video",
]


def cleanup():
    """Kill all Wine processes."""
    kill_quark_processes()


def run_trace(label, proton_bin, timeout, trace_dir):
    """Run Balatro and capture display init traces."""
    print(f"\n{'='*60}")
    print(f"  {label}")
    print(f"{'='*60}")

    cleanup()
    trace_dir.mkdir(parents=True, exist_ok=True)

    stderr_log = trace_dir / f"{label}_wine_stderr.log"
    daemon_log = Path("/tmp/quark/daemon.log")

    # Clean logs
    for p in [stderr_log, daemon_log]:
        try:
            p.unlink()
        except OSError:
            pass
    Path("/tmp/quark").mkdir(exist_ok=True)

    env = os.environ.copy()
    env["STEAM_COMPAT_DATA_PATH"] = str(COMPAT_DATA)
    env["STEAM_COMPAT_CLIENT_INSTALL_PATH"] = str(STEAM_ROOT)
    env["SteamAppId"] = "2379780"
    env["SteamGameId"] = "2379780"
    env["WINEDEBUG"] = WINEDEBUG

    # Pass display vars
    for var in ("DISPLAY", "WAYLAND_DISPLAY", "XDG_RUNTIME_DIR", "XAUTHORITY"):
        if var in os.environ:
            env[var] = os.environ[var]

    # Force NVIDIA EGL on NVIDIA systems
    nvidia_egl = "/usr/share/glvnd/egl_vendor.d/10_nvidia.json"
    if os.path.exists(nvidia_egl) and os.path.exists("/proc/driver/nvidia/version"):
        env["__EGL_VENDOR_LIBRARY_FILENAMES"] = nvidia_egl

    print(f"  Binary:   {proton_bin}")
    print(f"  Game:     {GAME_EXE}")
    print(f"  WINEDEBUG: {WINEDEBUG[:80]}...")
    print(f"  Timeout:  {timeout}s")
    print(f"  Stderr:   {stderr_log}")
    print()

    t0 = time.monotonic()
    stderr_file = open(stderr_log, "w")

    try:
        proc = subprocess.Popen(
            [str(proton_bin), "run", str(GAME_EXE)],
            env=env, cwd="/tmp",
            stdout=subprocess.PIPE, stderr=stderr_file,
        )
    except OSError as e:
        print(f"  FAILED to launch: {e}")
        stderr_file.close()
        return None

    # Wait with progress
    for tick in range(timeout):
        time.sleep(1)
        if proc.poll() is not None:
            break
        if tick % 5 == 0:
            sz = stderr_log.stat().st_size if stderr_log.exists() else 0
            print(f"  [{tick}s] running... stderr: {sz/1024:.0f} KB")

    elapsed = time.monotonic() - t0
    exit_code = proc.poll()
    if exit_code is None:
        proc.kill()
        proc.wait()
        print(f"  Timeout ({timeout}s), killed")
    else:
        print(f"  Exited with code {exit_code} after {elapsed:.1f}s")

    stderr_file.close()
    cleanup()
    return stderr_log


def extract_display_lines(log_path, keywords):
    """Extract lines matching any keyword from a log file."""
    if not log_path or not log_path.exists():
        return []
    lines = []
    try:
        text = log_path.read_text(errors="replace")
        for line in text.splitlines():
            for kw in keywords:
                if kw.lower() in line.lower():
                    lines.append(line.strip())
                    break
    except OSError:
        pass
    return lines


def extract_registry_ops(daemon_log):
    """Extract display-related registry operations from daemon.log."""
    if not daemon_log.exists():
        return []
    import re
    lines = []
    try:
        text = daemon_log.read_text(errors="replace")
        for line in text.splitlines():
            for pattern in REGISTRY_KEYWORDS:
                if re.search(pattern, line, re.IGNORECASE):
                    lines.append(line.strip())
                    break
    except OSError:
        pass
    return lines


def print_section(title, lines, max_lines=50):
    """Print a section of extracted lines."""
    print(f"\n  --- {title} ({len(lines)} lines) ---")
    for line in lines[:max_lines]:
        # Truncate long lines
        if len(line) > 150:
            line = line[:147] + "..."
        print(f"    {line}")
    if len(lines) > max_lines:
        print(f"    ... and {len(lines) - max_lines} more")


def main():
    parser = argparse.ArgumentParser(description="Trace display init: quark vs stock")
    parser.add_argument("--timeout", type=int, default=15)
    parser.add_argument("--quark-only", action="store_true")
    parser.add_argument("--stock-only", action="store_true")
    args = parser.parse_args()

    if not GAME_EXE.exists():
        print(f"ERROR: Balatro not found at {GAME_EXE}")
        sys.exit(1)

    trace_dir = TRACE_DIR
    trace_dir.mkdir(parents=True, exist_ok=True)

    # ── Quark run ──────────────────────────────────────────────
    quark_stderr = None
    quark_daemon = Path("/tmp/quark/daemon.log")
    if not args.stock_only:
        if not PROTON_BIN.exists():
            print(f"WARN: quark not installed at {PROTON_BIN}")
        else:
            quark_stderr = run_trace("QUARK", PROTON_BIN, args.timeout, trace_dir)

    # ── Stock wineserver run ─────────────────────────────────────────
    stock_stderr = None
    if not args.quark_only:
        # Find stock Proton
        proton_dirs = [
            STEAMAPPS / "common/Proton 10.0",
            STEAMAPPS / "common/Proton - Experimental",
        ]
        stock_proton = None
        for d in proton_dirs:
            p = d / "proton"
            if p.exists():
                stock_proton = p
                break

        if stock_proton:
            stock_stderr = run_trace("STOCK_PROTON", stock_proton, args.timeout, trace_dir)
        else:
            print("\nWARN: No stock Proton found, skipping comparison")

    # ── Analysis ─────────────────────────────────────────────────────
    print(f"\n{'='*60}")
    print(f"  ANALYSIS")
    print(f"{'='*60}")

    if quark_stderr:
        quark_display = extract_display_lines(quark_stderr, [
            "add_gpu", "commit_gpu", "write_gpu", "Failed",
            "update_display_devices", "GPU count", "adapter count",
            "lock_display", "read_gpu", "read_source", "read_monitor",
            "monitor_release", "gpu_release", "source_release",
            "add_source", "add_monitor", "commit_source", "commit_monitor",
            "prepare_devices", "create_gpu_device_key",
            "GLX", "GL version", "Direct rendering",
            "RegisterTouch", "create_whole", "drawable_create",
            "err:", "warn:",
        ])
        print_section("QUARK — Display Init Trace", quark_display)

        quark_registry = extract_registry_ops(quark_daemon)
        print_section("QUARK — Registry Ops (display-related)", quark_registry)

    if stock_stderr:
        stock_display = extract_display_lines(stock_stderr, [
            "add_gpu", "commit_gpu", "write_gpu", "Failed",
            "update_display_devices", "GPU count", "adapter count",
            "lock_display", "read_gpu", "read_source", "read_monitor",
            "monitor_release", "gpu_release", "source_release",
            "add_source", "add_monitor", "commit_source", "commit_monitor",
            "prepare_devices", "create_gpu_device_key",
            "GLX", "GL version", "Direct rendering",
            "RegisterTouch", "create_whole", "drawable_create",
            "err:", "warn:",
        ])
        print_section("STOCK PROTON — Display Init Trace", stock_display)

    # ── Diff ─────────────────────────────────────────────────────────
    if quark_stderr and stock_stderr:
        print(f"\n  --- DIFF: Display Init Sequence ---")

        quark_ops = set()
        stock_ops = set()

        for line in extract_display_lines(quark_stderr, ["add_gpu", "commit_gpu", "add_source",
                "commit_source", "add_monitor", "commit_monitor", "write_gpu",
                "create_gpu_device_key", "prepare_devices"]):
            # Extract the function name
            for fn in ["add_gpu", "commit_gpu", "add_source", "commit_source",
                        "add_monitor", "commit_monitor", "write_gpu_to_registry",
                        "create_gpu_device_key", "prepare_devices"]:
                if fn in line.lower():
                    quark_ops.add(fn)

        for line in extract_display_lines(stock_stderr, ["add_gpu", "commit_gpu", "add_source",
                "commit_source", "add_monitor", "commit_monitor", "write_gpu",
                "create_gpu_device_key", "prepare_devices"]):
            for fn in ["add_gpu", "commit_gpu", "add_source", "commit_source",
                        "add_monitor", "commit_monitor", "write_gpu_to_registry",
                        "create_gpu_device_key", "prepare_devices"]:
                if fn in line.lower():
                    stock_ops.add(fn)

        only_stock = stock_ops - quark_ops
        only_quark = quark_ops - stock_ops
        both = quark_ops & stock_ops

        if both:
            print(f"    BOTH:       {', '.join(sorted(both))}")
        if only_stock:
            print(f"    STOCK ONLY: {', '.join(sorted(only_stock))}")
        if only_quark:
            print(f"    AMP ONLY:   {', '.join(sorted(only_quark))}")
        if not only_stock and not only_quark:
            print(f"    No differences in display init functions called")

        # Check for window creation
        quark_window = any("RegisterTouch" in l or "create_whole" in l or "drawable_create" in l
                         for l in extract_display_lines(quark_stderr, ["RegisterTouch", "create_whole", "drawable_create"]))
        stock_window = any("RegisterTouch" in l or "create_whole" in l or "drawable_create" in l
                           for l in extract_display_lines(stock_stderr, ["RegisterTouch", "create_whole", "drawable_create"]))

        print(f"\n    Window created (quark): {'YES' if quark_window else 'NO'}")
        print(f"    Window created (stock):       {'YES' if stock_window else 'NO'}")

    # ── Summary ──────────────────────────────────────────────────────
    print(f"\n{'='*60}")
    print(f"  FILES")
    print(f"{'='*60}")
    if quark_stderr:
        print(f"  Quark stderr: {quark_stderr}")
    if stock_stderr:
        print(f"  Stock stderr:       {stock_stderr}")
    print(f"  Daemon log:         {quark_daemon}")
    print(f"  Trace dir:          {trace_dir}")
    print()


if __name__ == "__main__":
    main()
