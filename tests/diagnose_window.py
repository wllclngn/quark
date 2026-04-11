#!/usr/bin/env python3
"""Compare window management pipelines between working and broken games.

Parses daemon logs and wine_stderr logs to build a per-window timeline,
then diffs opcode usage, surface assignment, paint flags, rendering
calls, and errors between games.
"""

import re
import sys
from collections import Counter, defaultdict
from dataclasses import dataclass, field
from datetime import datetime
from pathlib import Path

# Log locations
GAMES = {
    "Balatro (WORKS)": {
        "appid": "2379780",
        "daemon": Path("/tmp/quark/runs/2379780_20260403-072856_daemon.log"),
        "stderr": Path("/tmp/quark/runs/2379780_20260403-072856_wine_stderr.log"),
    },
    "Dark Souls (BROKEN)": {
        "appid": "570940",
        "daemon": Path("/tmp/quark/runs/570940_20260403-072943_daemon.log"),
        "stderr": Path("/tmp/quark/runs/570940_20260403-072943_wine_stderr.log"),
    },
    "Hades (BROKEN)": {
        "appid": "1145360",
        "daemon": Path("/tmp/quark/runs/1145360_20260403-073111_daemon.log"),
        "stderr": Path("/tmp/quark/runs/1145360_20260403-073111_wine_stderr.log"),
    },
}


@dataclass
class WindowInfo:
    handle: str = ""
    atom: str = ""
    style: str = ""
    parent: str = ""
    tid: str = ""
    surface_win: str = "0x0000"
    has_paint: bool = False
    max_paint: int = 0
    xwin: str = ""
    client_xwin: str = ""
    swp_count: int = 0
    create_count: int = 0
    swp_flags_seen: set = field(default_factory=set)
    styles_seen: set = field(default_factory=set)
    has_client_window: bool = False
    has_pixel_format: bool = False
    has_swap_buffers: bool = False
    has_vulkan_surface: bool = False
    swap_buffer_count: int = 0
    rects: list = field(default_factory=list)


@dataclass
class GameAnalysis:
    name: str
    windows: dict = field(default_factory=lambda: defaultdict(WindowInfo))
    daemon_opcode_counts: Counter = field(default_factory=Counter)
    daemon_window_opcodes: Counter = field(default_factory=Counter)
    stub_opcodes: Counter = field(default_factory=Counter)
    stderr_errors: list = field(default_factory=list)
    stderr_warnings: list = field(default_factory=list)
    stderr_fixmes: list = field(default_factory=list)
    timeline: list = field(default_factory=list)
    total_daemon_lines: int = 0
    total_stderr_lines: int = 0
    rendering_api: str = "unknown"
    create_client_windows: list = field(default_factory=list)
    destroy_client_windows: list = field(default_factory=list)
    swap_buffer_count: int = 0
    pixel_format_calls: list = field(default_factory=list)
    vulkan_surface_calls: list = field(default_factory=list)
    display_settings_errors: int = 0


# Daemon log patterns
RE_TIMESTAMP = re.compile(r"^\[(\d{2}:\d{2}:\d{2})\]")
RE_CREATE_WINDOW = re.compile(
    r"create_window: handle=(0x[\da-f]+) atom=(\d+) style=(0x[\da-f]+) "
    r"req_parent=(0x[\da-f]+) parent=(0x[\da-f]+) tid=(0x[\da-f]+)"
)
RE_SET_WINDOW_POS = re.compile(
    r"set_window_pos: handle=(0x[\da-f]+) swp=(0x[\da-f]+) "
    r"paint=(0x[\da-f]+) style=(0x[\da-f]+) surface_win=(0x[\da-f]+)"
)
RE_TRACE_OPCODE = re.compile(r"\[trace\] #\d+ (\S+)")
RE_STUB = re.compile(r"\[auto-stub\] (\S+)")

# Wine stderr patterns
RE_WIN_POS_CHANGING = re.compile(
    r"X11DRV_WindowPosChanging hwnd (0x[\da-f]+), swp_flags (0x[\da-f]+), "
    r"shaped \d+, rects \{ window (.+?) \}"
)
RE_WIN_POS_CHANGED = re.compile(
    r"X11DRV_WindowPosChanged win (0x[\da-f]+)/([\da-f]+) new_rects \{ (.+?) \} "
    r"style ([\da-f]+) flags ([\da-f]+)"
)
RE_CREATE_CLIENT = re.compile(
    r"create_client_window (0x[\da-f]+) xwin ([\da-f]+)/([\da-f]+)"
)
RE_DESTROY_CLIENT = re.compile(r"destroy_client_window .+ destroying client window ([\da-f]+)")
RE_SET_PIXEL_FORMAT = re.compile(
    r"wglSetPixelFormat .+/(0x[\da-f]+) format (\d+)"
)
RE_SWAP_BUFFERS = re.compile(r"wglSwapBuffers .+ hwnd (0x[\da-f]+)")
RE_VK_SURFACE = re.compile(r"vkCreateWin32SurfaceKHR")
RE_CLIENT_SURFACE_CREATE = re.compile(
    r"client_surface_create Created (0x[\da-f]+)/(0x[\da-f]+) for client window ([\da-f]+)"
)
RE_WINE_ERR = re.compile(r"^[\da-f]+:err:(\S+):(\S+) (.+)")
RE_WINE_FIXME = re.compile(r"^[\da-f]+:fixme:(\S+):(\S+) (.+)")
RE_WINE_WARN = re.compile(r"^[\da-f]+:warn:(\S+):(\S+) (.+)")
RE_DISPLAY_SETTINGS_FAIL = re.compile(r"NtUserEnumDisplaySettings Failed")


def norm_handle(h: str) -> str:
    """Normalize a hex handle to consistent 0x0000 format (4 hex digits)."""
    val = int(h, 16)
    return f"0x{val:04x}"


def parse_daemon(path: Path, analysis: GameAnalysis):
    """Parse daemon log for window opcodes and stubs."""
    line_count = 0
    with open(path, "r", errors="replace") as f:
        for line in f:
            line_count += 1

            m = RE_TRACE_OPCODE.search(line)
            if m:
                analysis.daemon_opcode_counts[m.group(1)] += 1

            ts_m = RE_TIMESTAMP.match(line)
            ts = ts_m.group(1) if ts_m else ""

            m = RE_CREATE_WINDOW.search(line)
            if m:
                handle, atom, style, req_parent, parent, tid = m.groups()
                handle = norm_handle(handle)
                parent = norm_handle(parent)
                w = analysis.windows[handle]
                w.handle = handle
                w.atom = atom
                w.style = style
                w.parent = parent
                w.tid = tid
                w.create_count += 1
                w.styles_seen.add(style)
                analysis.daemon_window_opcodes["create_window"] += 1
                analysis.timeline.append(
                    (ts, "create_window", handle, f"atom={atom} style={style} parent={parent} tid={tid}")
                )
                continue

            m = RE_SET_WINDOW_POS.search(line)
            if m:
                handle, swp, paint, style, surface_win = m.groups()
                handle = norm_handle(handle)
                surface_win = norm_handle(surface_win)
                w = analysis.windows[handle]
                w.handle = handle
                w.swp_count += 1
                w.swp_flags_seen.add(swp)
                w.styles_seen.add(style)
                paint_val = int(paint, 16)
                if paint_val > 0:
                    w.has_paint = True
                if paint_val > w.max_paint:
                    w.max_paint = paint_val
                if surface_win != "0x0000":
                    w.surface_win = norm_handle(surface_win)
                analysis.daemon_window_opcodes["set_window_pos"] += 1
                continue

            m = RE_STUB.search(line)
            if m:
                analysis.stub_opcodes[m.group(1)] += 1

    analysis.total_daemon_lines = line_count


def parse_stderr(path: Path, analysis: GameAnalysis):
    """Parse wine_stderr for X11 driver, rendering, and error messages."""
    line_count = 0
    err_categories = Counter()
    fixme_categories = Counter()
    warn_categories = Counter()

    with open(path, "r", errors="replace") as f:
        for line in f:
            line_count += 1

            m = RE_WIN_POS_CHANGED.search(line)
            if m:
                hwnd, xwin, rects_str, style, flags = m.groups()
                hwnd = norm_handle(hwnd)
                w = analysis.windows[hwnd]
                w.xwin = xwin
                continue

            m = RE_CREATE_CLIENT.search(line)
            if m:
                hwnd, parent_xwin, client_xwin = m.groups()
                hwnd = norm_handle(hwnd)
                w = analysis.windows[hwnd]
                w.has_client_window = True
                w.client_xwin = client_xwin
                analysis.create_client_windows.append((hwnd, parent_xwin, client_xwin))
                continue

            m = RE_DESTROY_CLIENT.search(line)
            if m:
                analysis.destroy_client_windows.append(m.group(1))
                continue

            m = RE_SET_PIXEL_FORMAT.search(line)
            if m:
                hwnd, fmt = m.groups()
                hwnd = norm_handle(hwnd)
                w = analysis.windows[hwnd]
                w.has_pixel_format = True
                analysis.pixel_format_calls.append((hwnd, fmt))
                analysis.rendering_api = "OpenGL"
                continue

            m = RE_SWAP_BUFFERS.search(line)
            if m:
                hwnd = norm_handle(m.group(1))
                w = analysis.windows[hwnd]
                w.has_swap_buffers = True
                w.swap_buffer_count += 1
                analysis.swap_buffer_count += 1
                continue

            if RE_VK_SURFACE.search(line):
                analysis.rendering_api = "Vulkan"
                analysis.vulkan_surface_calls.append(line.strip())
                continue

            m = RE_CLIENT_SURFACE_CREATE.search(line)
            if m:
                hwnd = norm_handle(m.group(1))
                w = analysis.windows[hwnd]
                continue

            if RE_DISPLAY_SETTINGS_FAIL.search(line):
                analysis.display_settings_errors += 1
                continue

            m = RE_WINE_ERR.match(line)
            if m:
                cat, func, msg = m.groups()
                key = f"{cat}:{func}"
                err_categories[key] += 1
                if len(analysis.stderr_errors) < 50:
                    analysis.stderr_errors.append((key, msg.strip()[:120]))
                continue

            m = RE_WINE_FIXME.match(line)
            if m:
                cat, func, msg = m.groups()
                key = f"{cat}:{func}"
                fixme_categories[key] += 1
                if len(analysis.stderr_fixmes) < 50:
                    analysis.stderr_fixmes.append((key, msg.strip()[:120]))
                continue

            m = RE_WINE_WARN.match(line)
            if m:
                cat, func, msg = m.groups()
                key = f"{cat}:{func}"
                warn_categories[key] += 1
                continue

    analysis.total_stderr_lines = line_count

    # Detect vulkan from loaded DLLs if not already detected
    if analysis.rendering_api == "unknown":
        with open(path, "r", errors="replace") as f:
            for line in f:
                if "vulkan-1.dll" in line or "winevulkan.dll" in line:
                    if "build_module Loaded" in line:
                        analysis.rendering_api = "Vulkan (loaded, no surface)"
                        break


def fmt_hex_set(s):
    """Format a set of hex values compactly."""
    if not s:
        return "-"
    items = sorted(s)
    if len(items) <= 4:
        return " ".join(items)
    return f"{items[0]} {items[1]} ... {items[-1]} ({len(items)} total)"


def print_game_report(analysis: GameAnalysis):
    """Print per-game window analysis."""
    print(f"\n[{datetime.now().strftime('%H:%M:%S')}] [INFO]   Game: {analysis.name}")
    print(f"[{datetime.now().strftime('%H:%M:%S')}] [INFO]   Rendering API: {analysis.rendering_api}")
    print(f"[{datetime.now().strftime('%H:%M:%S')}] [INFO]   Daemon log: {analysis.total_daemon_lines} lines")
    print(f"[{datetime.now().strftime('%H:%M:%S')}] [INFO]   Wine stderr: {analysis.total_stderr_lines} lines")

    # Window creation timeline (first 20 events)
    ts = datetime.now().strftime("%H:%M:%S")
    print(f"\n[{ts}] [INFO]   Window creation timeline")
    print(f"{'Time':<10} {'Op':<16} {'Handle':<8} {'Details'}")
    timeline_creates = [e for e in analysis.timeline if e[1] == "create_window"]
    for t, op, handle, details in timeline_creates[:20]:
        print(f"{t:<10} {op:<16} {handle:<8} {details}")
    if len(timeline_creates) > 20:
        print(f"  ... {len(timeline_creates) - 20} more create_window events")

    # Per-window summary table
    ts = datetime.now().strftime("%H:%M:%S")
    print(f"\n[{ts}] [INFO]   Per-window summary")
    print(f"{'Handle':<8} {'Create':<7} {'SWP':<6} {'Paint':<6} {'SurfWin':<10} {'Xwin':<10} "
          f"{'Client':<8} {'PxFmt':<6} {'Swaps':<7} {'VkSurf':<7} {'Style(last)':<14}")

    # Sort windows by handle for consistent display
    sorted_wins = sorted(analysis.windows.items(), key=lambda x: int(x[0], 16))
    for handle, w in sorted_wins:
        # Skip windows with no interesting activity
        if w.create_count == 0 and w.swp_count == 0:
            continue
        last_style = sorted(w.styles_seen)[-1] if w.styles_seen else "-"
        xwin_str = w.xwin if w.xwin else "-"
        client_str = "yes" if w.has_client_window else "no"
        pxfmt_str = "yes" if w.has_pixel_format else "no"
        swaps_str = str(w.swap_buffer_count) if w.swap_buffer_count > 0 else "-"
        vk_str = "yes" if w.has_vulkan_surface else "no"
        surf_str = w.surface_win if w.surface_win != "0x0000" else "-"
        paint_str = f"0x{w.max_paint:02x}" if w.has_paint else "-"
        print(f"{handle:<8} {w.create_count:<7} {w.swp_count:<6} {paint_str:<6} {surf_str:<10} "
              f"{xwin_str:<10} {client_str:<8} {pxfmt_str:<6} {swaps_str:<7} {vk_str:<7} {last_style:<14}")

    # Surface windows (surface_win != 0)
    ts = datetime.now().strftime("%H:%M:%S")
    surface_wins = [(h, w) for h, w in sorted_wins if w.surface_win != "0x0000"]
    print(f"\n[{ts}] [INFO]   Windows with surface_win assigned: {len(surface_wins)}")
    for handle, w in surface_wins:
        print(f"  {handle} -> surface_win={w.surface_win} paint=0x{w.max_paint:02x} "
              f"swp_count={w.swp_count} client={w.has_client_window}")

    # Client window creation
    ts = datetime.now().strftime("%H:%M:%S")
    print(f"\n[{ts}] [INFO]   create_client_window calls: {len(analysis.create_client_windows)}")
    for hwnd, parent, client in analysis.create_client_windows:
        print(f"  hwnd={hwnd} parent_xwin={parent} client_xwin={client}")
    if analysis.destroy_client_windows:
        print(f"[{ts}] [INFO]   destroy_client_window calls: {len(analysis.destroy_client_windows)}")
        for xwin in analysis.destroy_client_windows:
            print(f"  client_xwin={xwin}")

    # Rendering details
    ts = datetime.now().strftime("%H:%M:%S")
    if analysis.pixel_format_calls:
        print(f"\n[{ts}] [INFO]   SetPixelFormat calls: {len(analysis.pixel_format_calls)}")
        for hwnd, fmt in analysis.pixel_format_calls[:10]:
            print(f"  hwnd={hwnd} format={fmt}")
    if analysis.swap_buffer_count > 0:
        print(f"[{ts}] [INFO]   wglSwapBuffers total: {analysis.swap_buffer_count}")
    if analysis.vulkan_surface_calls:
        print(f"[{ts}] [INFO]   Vulkan surface creation calls: {len(analysis.vulkan_surface_calls)}")
        for call in analysis.vulkan_surface_calls[:5]:
            print(f"  {call[:120]}")

    # Stub opcodes
    ts = datetime.now().strftime("%H:%M:%S")
    if analysis.stub_opcodes:
        print(f"\n[{ts}] [WARN]   Auto-stub opcodes (missing implementations)")
        print(f"{'Opcode':<30} {'Count':<8}")
        for opcode, count in analysis.stub_opcodes.most_common(20):
            print(f"{opcode:<30} {count:<8}")

    # Top daemon opcodes
    ts = datetime.now().strftime("%H:%M:%S")
    print(f"\n[{ts}] [INFO]   Top daemon opcodes (traced)")
    print(f"{'Opcode':<35} {'Count':<8}")
    for opcode, count in analysis.daemon_opcode_counts.most_common(15):
        print(f"{opcode:<35} {count:<8}")

    # Display settings errors
    if analysis.display_settings_errors > 0:
        ts = datetime.now().strftime("%H:%M:%S")
        print(f"\n[{ts}] [WARN]   NtUserEnumDisplaySettings failures: {analysis.display_settings_errors}")

    # Wine errors (unique)
    ts = datetime.now().strftime("%H:%M:%S")
    if analysis.stderr_errors:
        seen = set()
        unique = []
        for key, msg in analysis.stderr_errors:
            sig = f"{key}|{msg[:60]}"
            if sig not in seen:
                seen.add(sig)
                unique.append((key, msg))
        print(f"\n[{ts}] [ERROR]  Wine errors (unique)")
        print(f"{'Category':<40} {'Message'}")
        for key, msg in unique[:20]:
            print(f"{key:<40} {msg[:80]}")


def print_diff(analyses: list[GameAnalysis]):
    """Print comparison between working and broken games."""
    ts = datetime.now().strftime("%H:%M:%S")
    print(f"\n[{ts}] [INFO]   COMPARISON: Working vs Broken")

    working = analyses[0]
    broken = analyses[1:]

    # Opcode diff
    working_ops = set(working.daemon_opcode_counts.keys())
    for b in broken:
        broken_ops = set(b.daemon_opcode_counts.keys())
        only_working = working_ops - broken_ops
        only_broken = broken_ops - working_ops

        ts = datetime.now().strftime("%H:%M:%S")
        print(f"\n[{ts}] [INFO]   Opcode diff: {working.name} vs {b.name}")
        if only_working:
            print(f"  Only in {working.name}:")
            for op in sorted(only_working):
                print(f"    {op} (x{working.daemon_opcode_counts[op]})")
        else:
            print(f"  No opcodes unique to {working.name}")
        if only_broken:
            print(f"  Only in {b.name}:")
            for op in sorted(only_broken):
                print(f"    {op} (x{b.daemon_opcode_counts[op]})")
        else:
            print(f"  No opcodes unique to {b.name}")

    # Window count comparison
    ts = datetime.now().strftime("%H:%M:%S")
    print(f"\n[{ts}] [INFO]   Window statistics comparison")
    print(f"{'Metric':<40} ", end="")
    for a in analyses:
        short = a.name.split("(")[0].strip()
        print(f"{short:<20} ", end="")
    print()

    metrics = [
        ("create_window calls", lambda a: a.daemon_window_opcodes["create_window"]),
        ("set_window_pos calls", lambda a: a.daemon_window_opcodes["set_window_pos"]),
        ("Windows with surface_win", lambda a: sum(1 for w in a.windows.values() if w.surface_win != "0x0000")),
        ("Windows with paint > 0", lambda a: sum(1 for w in a.windows.values() if w.has_paint)),
        ("Windows with client_window", lambda a: sum(1 for w in a.windows.values() if w.has_client_window)),
        ("create_client_window calls", lambda a: len(a.create_client_windows)),
        ("destroy_client_window calls", lambda a: len(a.destroy_client_windows)),
        ("SetPixelFormat calls", lambda a: len(a.pixel_format_calls)),
        ("wglSwapBuffers calls", lambda a: a.swap_buffer_count),
        ("Vulkan surface calls", lambda a: len(a.vulkan_surface_calls)),
        ("Auto-stub opcodes (unique)", lambda a: len(a.stub_opcodes)),
        ("Auto-stub calls (total)", lambda a: sum(a.stub_opcodes.values())),
        ("DisplaySettings failures", lambda a: a.display_settings_errors),
        ("Rendering API", lambda a: a.rendering_api),
        ("Daemon lines", lambda a: a.total_daemon_lines),
        ("Wine stderr lines", lambda a: a.total_stderr_lines),
    ]

    for label, fn in metrics:
        print(f"{label:<40} ", end="")
        for a in analyses:
            val = fn(a)
            print(f"{str(val):<20} ", end="")
        print()

    # Surface window styles comparison
    ts = datetime.now().strftime("%H:%M:%S")
    print(f"\n[{ts}] [INFO]   Surface window styles (windows that got surface_win)")
    for a in analyses:
        short = a.name.split("(")[0].strip()
        surface_wins = [w for w in a.windows.values() if w.surface_win != "0x0000"]
        if surface_wins:
            all_styles = set()
            for w in surface_wins:
                all_styles.update(w.styles_seen)
            print(f"  {short}: {' '.join(sorted(all_styles))}")
        else:
            print(f"  {short}: (no surface windows)")

    # Key differences / diagnosis
    ts = datetime.now().strftime("%H:%M:%S")
    print(f"\n[{ts}] [INFO]   Key findings")

    # Check if broken games lack rendering calls
    for b in broken:
        short = b.name.split("(")[0].strip()
        if b.swap_buffer_count == 0 and not b.vulkan_surface_calls:
            print(f"[{ts}] [WARN]   {short}: NO rendering calls (no SwapBuffers, no VkSurface)")
        if b.swap_buffer_count == 0 and len(b.pixel_format_calls) == 0:
            print(f"[{ts}] [WARN]   {short}: NO SetPixelFormat calls (OpenGL never initialized on window)")
        if len(b.create_client_windows) > 0 and b.swap_buffer_count == 0 and not b.vulkan_surface_calls:
            print(f"[{ts}] [WARN]   {short}: client_window created but no rendering attached")

    # Check for destroy_client_window before rendering
    if working.destroy_client_windows:
        print(f"[{ts}] [INFO]   {working.name.split('(')[0].strip()}: "
              f"destroyed {len(working.destroy_client_windows)} client windows "
              f"(normal GL context switching)")

    # Compare stub counts
    working_stubs = set(working.stub_opcodes.keys())
    for b in broken:
        broken_stubs = set(b.stub_opcodes.keys())
        only_broken_stubs = broken_stubs - working_stubs
        if only_broken_stubs:
            short = b.name.split("(")[0].strip()
            print(f"[{ts}] [WARN]   {short}: stubs unique to broken: {', '.join(sorted(only_broken_stubs))}")

    # Check for display settings failures as potential cause
    for b in broken:
        if b.display_settings_errors > 0 and working.display_settings_errors == 0:
            short = b.name.split("(")[0].strip()
            print(f"[{ts}] [WARN]   {short}: has {b.display_settings_errors} display settings failures "
                  f"({working.name.split('(')[0].strip()} has 0)")

    # Check window paint progression
    ts = datetime.now().strftime("%H:%M:%S")
    print(f"\n[{ts}] [INFO]   Paint flag progression (max paint value per surface window)")
    for a in analyses:
        short = a.name.split("(")[0].strip()
        surface_wins = [(h, w) for h, w in sorted(a.windows.items(), key=lambda x: int(x[0], 16))
                        if w.surface_win != "0x0000"]
        if surface_wins:
            parts = [f"{h}=0x{w.max_paint:02x}" for h, w in surface_wins]
            print(f"  {short}: {' '.join(parts)}")
        else:
            print(f"  {short}: (none)")


def main():
    analyses = []

    for name, paths in GAMES.items():
        daemon_path = paths["daemon"]
        stderr_path = paths["stderr"]

        if not daemon_path.exists():
            print(f"[{datetime.now().strftime('%H:%M:%S')}] [ERROR]  Missing: {daemon_path}")
            continue
        if not stderr_path.exists():
            print(f"[{datetime.now().strftime('%H:%M:%S')}] [ERROR]  Missing: {stderr_path}")
            continue

        ts = datetime.now().strftime("%H:%M:%S")
        print(f"[{ts}] [INFO]   Parsing {name} ...")
        analysis = GameAnalysis(name=name)
        parse_daemon(daemon_path, analysis)
        parse_stderr(stderr_path, analysis)
        analyses.append(analysis)

    if not analyses:
        print(f"[{datetime.now().strftime('%H:%M:%S')}] [ERROR]  No logs found")
        return 1

    for a in analyses:
        print_game_report(a)

    if len(analyses) >= 2:
        print_diff(analyses)

    return 0


if __name__ == "__main__":
    sys.exit(main())
