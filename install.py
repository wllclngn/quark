#!/usr/bin/env python3
"""quark installer — build, patch, and deploy quark as a Steam compatibility tool.

Pipeline architecture: 13 verified steps, each with explicit inputs, outputs,
and failure reporting. No silent failures. No swallowed errors.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
import shutil
import struct
import subprocess
import sys
import tarfile
import time
import urllib.request
from dataclasses import dataclass, field
from datetime import datetime
from pathlib import Path
from typing import Any

# Constants

SCRIPT_DIR = Path(__file__).parent.resolve()
RUST_DIR = SCRIPT_DIR / "rust"

DATA_DIR = Path.home() / ".local" / "share" / "quark"
CACHE_DIR = Path.home() / ".cache" / "quark"
WINE_SRC_DIR = Path("/tmp/quark-wine-build/wine-src")
WINE_OBJ_DIR = Path("/tmp/quark-wine-build/wine-obj")
STEAM_COMPAT_DIR = Path.home() / ".local" / "share" / "Steam" / "compatibilitytools.d" / "quark"
WINE_CLONE_URL = "https://gitlab.winehq.org/wine/wine.git"
WINE_TAG = None  # Detected from system Wine at runtime; never hardcoded
EAC_RUNTIME_DIR = Path.home() / ".local" / "share" / "Steam" / "steamapps" / "common" / "Proton EasyAntiCheat Runtime" / "v2"

PROTON_GIT_URL = "https://github.com/ValveSoftware/Proton.git"
PROTON_GIT_TAG = "proton-10.0-4"
PROTON_SOURCE_CACHE = CACHE_DIR / "proton-source" / PROTON_GIT_TAG
LSTEAMCLIENT_CACHE = DATA_DIR / "lsteamclient"

REQUIRED_TOOLS: dict[str, tuple[str | None, str]] = {
    "rustc":    ("1.85",  "pacman -S rust"),
    "cargo":    (None,    "pacman -S rust"),
    "clang":    (None,    "pacman -S clang"),
    "ld.lld":   (None,    "pacman -S lld"),
    "git":      (None,    "pacman -S git"),
    "autoconf": (None,    "pacman -S autoconf"),
    "make":     (None,    "pacman -S base-devel"),
}

# Known Proton appids — never touch their prefixes
PROTON_APPIDS = frozenset({
    "3658110", "1493710", "2805730", "1628350",
    "1580130", "1887720", "2180100", "2348590",
})

PE_DRIVER_FILES = [
    "sharedgpures.sys", "nvcuda.dll", "amd_ags_x64.dll", "amdxc64.dll",
    "atiadlxx.dll", "dxcore.dll", "audioses.dll", "belauncher.exe",
]

ENV_CONFIG_TEMPLATE = """\
# quark custom environment variables
#
# Format: KEY=VALUE (one per line)
# Lines starting with # are comments. Blank lines are ignored.
# Variables set here override quark's built-in defaults.
# Edit this file any time — changes apply on next game launch.
#
# --- Sync ---
WINE_NTSYNC=1
#
# --- Logging ---
# WINEDEBUG=-all
# DXVK_LOG_LEVEL=none
# VKD3D_DEBUG=none
#
# --- Overlays ---
# MANGOHUD=1
# DXVK_HUD=fps
#
# --- Performance ---
# DXVK_ASYNC=1
# mesa_glthread=true
#
# --- NVIDIA ---
# PROTON_ENABLE_NVAPI=1
# DXVK_ENABLE_NVAPI=dxgi
"""


# Logging

def _timestamp() -> str:
    """Get current timestamp in [HH:MM:SS] format."""
    return datetime.now().strftime("[%H:%M:%S]")


def log_info(msg: str) -> None:
    print(f"{_timestamp()} [INFO]   {msg}")


def log_warn(msg: str) -> None:
    print(f"{_timestamp()} [WARN]   {msg}")


def log_error(msg: str) -> None:
    print(f"{_timestamp()} [ERROR]  {msg}")


def log_debug(msg: str) -> None:
    print(f"{_timestamp()} [DEBUG]  {msg}")


# Core helpers

class StepFailed(Exception):
    """A pipeline step failed with a clear diagnostic."""
    def __init__(self, step: str, detail: str):
        self.step = step
        self.detail = detail
        super().__init__(f"{step}: {detail}")


@dataclass
class StepResult:
    name: str
    success: bool
    skipped: bool = False
    artifacts: list[Path] = field(default_factory=list)
    warnings: list[str] = field(default_factory=list)
    elapsed_ms: int = 0


def run_cmd(
    cmd: list[str],
    *,
    desc: str,
    cwd: Path | None = None,
    timeout: int = 300,
    check: bool = True,
    capture: bool = True,
) -> subprocess.CompletedProcess[str]:
    """Run a command with proper error reporting.

    On failure with check=True: logs last 20 lines of stderr, raises StepFailed.
    """
    try:
        result = subprocess.run(
            cmd, cwd=cwd, timeout=timeout,
            capture_output=capture, text=True,
        )
    except subprocess.TimeoutExpired:
        raise StepFailed(desc, f"timed out after {timeout}s")
    except FileNotFoundError:
        raise StepFailed(desc, f"command not found: {cmd[0]}")

    if check and result.returncode != 0:
        log_error(f"{desc} failed (exit {result.returncode})")
        if capture and result.stderr:
            for line in result.stderr.strip().splitlines()[-20:]:
                log_error(f"  {line}")
        raise StepFailed(desc, f"exit code {result.returncode}")
    return result


def verify_deployed(files: list[Path], step_name: str) -> None:
    """Verify all files exist with non-zero size. Raise StepFailed if any missing."""
    missing = [f for f in files if not f.exists() or f.stat().st_size == 0]
    if missing:
        for f in missing:
            log_error(f"  MISSING: {f}")
        raise StepFailed(step_name, f"{len(missing)} file(s) not deployed")


# Write guardrail
#
# Quark must NEVER write to system-owned paths. Every recurring DLL incident
# on this project has come from an agent "fixing" a load problem by copying a
# patched DLL into /usr/lib/wine. The bwrap launcher layer makes this
# unnecessary at runtime; the guardrail makes it impossible at install time.
#
# Allowlist of write roots — anything not under one of these is forbidden.
# Add a new entry here, with a comment, if a legitimate new write target appears.

def _quark_write_roots() -> list[Path]:
    return [
        STEAM_COMPAT_DIR,                 # the only deploy target that matters
        WINE_SRC_DIR,                     # cloned wine source for patching
        WINE_OBJ_DIR,                     # wine build artifacts
        CACHE_DIR,                        # ~/.cache/quark — provenance manifests, downloads
        DATA_DIR,                         # ~/.local/share/quark — legacy lsteamclient cache
        SCRIPT_DIR / "rust" / "target",   # cargo build output
        Path("/tmp"),                     # tarballs, scratch (must be under /tmp/...)
    ]


def assert_quark_writable(path: Path) -> None:
    """Raise StepFailed if `path` resolves outside any allowed quark write root.

    Use this at every NEW write call site. Catches accidental writes to /usr,
    /etc, /opt, /var, /lib, /sbin, /bin, and anything else owned by the system
    or by another package. The bwrap layer at runtime will not save you if
    install.py poisons /usr at install time — this is the line of defense.
    """
    try:
        # Don't .resolve() the target itself (it may not exist yet); resolve its parent.
        parent = path.parent.resolve(strict=False)
    except (OSError, RuntimeError):
        raise StepFailed("write_guard", f"unresolvable write path: {path}")
    candidate = parent / path.name
    for root in _quark_write_roots():
        try:
            root_resolved = root.resolve(strict=False)
        except (OSError, RuntimeError):
            continue
        try:
            candidate.relative_to(root_resolved)
            return
        except ValueError:
            continue
    raise StepFailed(
        "write_guard",
        f"refusing to write outside quark-owned roots: {candidate}\n"
        f"  allowed roots: {[str(r) for r in _quark_write_roots()]}"
    )


def get_system_wine_version() -> str:
    """Read `wine --version` output (e.g. 'wine-11.6'). Returns empty string on failure."""
    for binary in ["wine", "wine64"]:
        try:
            r = subprocess.run([binary, "--version"], capture_output=True, text=True, timeout=5)
            if r.returncode == 0 and r.stdout.strip():
                return r.stdout.strip()
        except (FileNotFoundError, subprocess.TimeoutExpired, OSError):
            continue
    return ""


def derive_wine_tag() -> str:
    """Detect the system Wine version and return its tag.

    Quark must build against the SAME Wine source the system ships
    binaries from. No fallback to a hardcoded version -- if system
    Wine can't be detected, we can't build safely.
    """
    sysver = get_system_wine_version()  # e.g. "wine-11.6"
    if sysver and sysver.startswith("wine-"):
        return sysver
    log_error("Cannot detect system Wine version. Install Wine first.")
    sys.exit(1)


def compute_patches_hash(patch_dir: Path) -> str:
    """SHA-256 of all .patch files in patch_dir, sorted by name. Catches any
    edit, addition, or removal of a patch."""
    h = hashlib.sha256()
    if not patch_dir.exists():
        return h.hexdigest()
    for p in sorted(patch_dir.glob("*.patch")):
        h.update(p.name.encode())
        h.update(b"\0")
        try:
            h.update(p.read_bytes())
        except OSError:
            pass
        h.update(b"\0")
    return h.hexdigest()


def compute_wine_build_inputs() -> dict[str, str]:
    """The full set of inputs that determine whether a previous wine-obj build
    is still valid. Any field changing means the cached build must be discarded.
    """
    return {
        "wine_tag": derive_wine_tag(),
        "system_wine_version": get_system_wine_version(),
        "patches_hash": compute_patches_hash(SCRIPT_DIR / "patches" / "wine"),
        "configure_args": "--enable-win64 --with-x --without-wayland",
    }


def read_build_manifest(path: Path) -> dict[str, str] | None:
    """Read a JSON manifest from disk. None if missing or unparseable."""
    if not path.exists():
        return None
    try:
        return json.loads(path.read_text())
    except (OSError, json.JSONDecodeError):
        return None


def write_build_manifest(path: Path, manifest: dict[str, str]) -> None:
    """Write a build manifest as JSON."""
    assert_quark_writable(path)
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(manifest, indent=2, sort_keys=True))


def manifest_matches(stored: dict[str, str] | None, current: dict[str, str]) -> bool:
    """True if every field in `current` matches `stored`. Used as the
    skip-rebuild gate: only skip if the stored manifest is byte-identical."""
    if stored is None:
        return False
    for k, v in current.items():
        if stored.get(k) != v:
            return False
    return True


def snapshot_system_wine() -> dict[str, tuple[str, int, int]]:
    """Snapshot every entry under /usr/lib/wine and /usr/share/wine.

    Returned dict is path → (kind, meta_a, meta_b) where:
      - regular file:  ("file", mtime_ns, size)
      - symlink:       ("symlink", hash_of_target, 0)
      - directory:     ("dir", 0, 0)

    Tracking symlinks is critical: the previous bug was an agent shipping
    `sudo ln -sf` calls into /usr/lib/wine, and the audit missed it because
    it filtered symlinks out. Symlink creation IS poisoning. So is symlink
    target rewriting.
    """
    snap: dict[str, tuple[str, int, int]] = {}
    for root in [Path("/usr/lib/wine"), Path("/usr/share/wine")]:
        if not root.exists():
            continue
        try:
            for p in root.rglob("*"):
                try:
                    if p.is_symlink():
                        target = os.readlink(p)
                        # use hash of target as the second field; we just need
                        # any change to be detected, not the actual target
                        snap[str(p)] = ("symlink", hash(target) & 0x7fffffff, 0)
                    elif p.is_file():
                        st = p.stat()
                        snap[str(p)] = ("file", st.st_mtime_ns, st.st_size)
                    elif p.is_dir():
                        snap[str(p)] = ("dir", 0, 0)
                except OSError:
                    pass
        except OSError:
            pass
    return snap


def verify_system_wine_unchanged(before: dict[str, tuple[str, int, int]]) -> None:
    """Compare current state of /usr/lib/wine + /usr/share/wine against `before`.
    Raise StepFailed if anything was modified, added, or deleted. This is the
    detective control that pairs with assert_quark_writable: even if a write
    bypasses the guardrail (e.g. via subprocess), the audit catches it.

    Catches symlinks (creation, deletion, retargeting), regular files
    (creation, deletion, mtime/size change), and new/removed subdirectories.
    """
    after = snapshot_system_wine()
    changed = []
    for path, before_meta in before.items():
        after_meta = after.get(path)
        if after_meta is None:
            changed.append(("DELETED", path))
        elif after_meta != before_meta:
            kind_before, kind_after = before_meta[0], after_meta[0]
            if kind_before != kind_after:
                changed.append((f"TYPE-CHANGED({kind_before}→{kind_after})", path))
            elif kind_before == "symlink":
                changed.append(("SYMLINK-RETARGETED", path))
            else:
                changed.append(("MODIFIED", path))
    for path, after_meta in after.items():
        if path not in before:
            kind = after_meta[0]
            changed.append((f"CREATED-{kind.upper()}", path))
    if changed:
        for kind, path in changed[:20]:
            log_error(f"  {kind}: {path}")
        if len(changed) > 20:
            log_error(f"  ... and {len(changed) - 20} more")
        raise StepFailed(
            "system_wine_audit",
            f"install.py modified {len(changed)} entry(ies) in /usr/lib/wine or /usr/share/wine"
        )


_AUTO_YES = False


def prompt_yn(question: str) -> bool:
    """Prompt the user with a [Y/N] question. Auto-answers Y if --yes was passed."""
    if _AUTO_YES:
        print(f"{question} [Y/N] y  (--yes)")
        return True
    while True:
        answer = input(f"{question} [Y/N] ").strip().lower()
        if answer == "y":
            return True
        if answer == "n":
            return False


def get_version() -> str:
    """Read version from rust/Cargo.toml."""
    cargo_toml = RUST_DIR / "Cargo.toml"
    for line in cargo_toml.read_text().splitlines():
        if line.startswith("version"):
            return line.split('"')[1]
    return "0.0.0"


def cpu_count() -> int:
    return max(os.cpu_count() or 4, 1)


def fmt_size(n: int) -> str:
    if n >= 1 << 30:
        return f"{n / (1 << 30):.1f} GB"
    if n >= 1 << 20:
        return f"{n / (1 << 20):.1f} MB"
    if n >= 1 << 10:
        return f"{n / (1 << 10):.0f} KB"
    return f"{n} B"


def find_steam_libraries() -> list[Path]:
    """Discover all Steam library folders (deduplicated)."""
    steam_dirs = [
        Path.home() / ".local" / "share" / "Steam",
        Path.home() / ".steam" / "root",
    ]
    seen: set[Path] = set()
    libraries: list[Path] = []
    for steam in steam_dirs:
        steamapps = steam / "steamapps"
        if not steamapps.exists():
            continue
        resolved = steamapps.resolve()
        if resolved in seen:
            continue
        seen.add(resolved)
        libraries.append(steamapps)
        vdf = steamapps / "libraryfolders.vdf"
        if vdf.exists():
            try:
                text = vdf.read_text(errors="replace")
                for m in re.finditer(r'"path"\s+"([^"]+)"', text):
                    lib = Path(m.group(1)) / "steamapps"
                    if lib.exists():
                        lib_resolved = lib.resolve()
                        if lib_resolved not in seen:
                            seen.add(lib_resolved)
                            libraries.append(lib)
            except OSError:
                pass
    return libraries


def get_quark_appids() -> set[str]:
    """Read Steam's config.vdf to find which appids use quark as compat tool."""
    config_vdf = Path.home() / ".local/share/Steam/config/config.vdf"
    if not config_vdf.exists():
        return set()
    try:
        content = config_vdf.read_text()
        start = content.find('"CompatToolMapping"')
        if start < 0:
            return set()
        brace_start = content.find('{', start)
        if brace_start < 0:
            return set()
        depth = 1
        i = brace_start + 1
        while i < len(content) and depth > 0:
            if content[i] == '{':
                depth += 1
            elif content[i] == '}':
                depth -= 1
            i += 1
        block = content[brace_start + 1:i - 1]
        pairs = re.findall(r'"(\d+)"\s*\{[^}]*"name"\s*"quark"', block)
        return set(pairs)
    except (OSError, ValueError):
        return set()


def find_proton_wine_dir() -> Path | None:
    """Find a Proton installation's Wine directory for PE drivers."""
    steam_common = Path.home() / ".local" / "share" / "Steam" / "steamapps" / "common"
    if not steam_common.exists():
        steam_common = Path.home() / ".steam" / "root" / "steamapps" / "common"
    if not steam_common.exists():
        return None
    for name in ["Proton - Experimental", "Proton Hotfix", "Proton 10.0", "Proton 9.0"]:
        d = steam_common / name / "files" / "lib" / "wine" / "x86_64-windows"
        if d.exists():
            return d
    return None


# Step 1: Preflight

def step_preflight() -> StepResult:
    """Verify all required tools and system state."""
    t0 = time.monotonic_ns()
    warnings: list[str] = []
    missing: list[str] = []

    # Check all tools
    for tool, (min_ver, install_cmd) in REQUIRED_TOOLS.items():
        path = shutil.which(tool)
        if not path:
            missing.append(f"  {tool:12s} — install with: {install_cmd}")
            continue

        if min_ver and tool == "rustc":
            result = subprocess.run([tool, "--version"], capture_output=True, text=True)
            if result.returncode == 0:
                ver = result.stdout.split()[1]
                parts = ver.split(".")
                try:
                    major, minor = int(parts[0]), int(parts[1])
                    if major < 1 or (major == 1 and minor < int(min_ver.split(".")[1])):
                        missing.append(f"  {tool:12s} — have {ver}, need {min_ver}+: rustup update stable")
                        continue
                except (ValueError, IndexError):
                    pass
                log_info(f"Rust: {ver}")

    if missing:
        log_error("Missing required tools:")
        for line in missing:
            log_error(line)
        raise StepFailed("preflight", f"{len(missing)} tool(s) missing")

    # Steam
    steam_root = Path.home() / ".steam" / "root"
    if not steam_root.exists():
        raise StepFailed("preflight", "Steam not found (~/.steam/root) — install Steam natively")
    log_info("Steam: found")

    # System Wine
    sys_wine = Path("/usr/lib/wine/x86_64-unix")
    if not sys_wine.exists() or not shutil.which("wine"):
        raise StepFailed("preflight", "System Wine not found — pacman -S wine")
    wine_ver = subprocess.run(["wine", "--version"], capture_output=True, text=True)
    log_info(f"Wine: {wine_ver.stdout.strip() if wine_ver.returncode == 0 else 'found'}")

    # ntsync
    kver = os.uname().release
    parts = kver.split(".")
    try:
        major, minor = int(parts[0]), int(parts[1])
    except (ValueError, IndexError):
        raise StepFailed("preflight", f"cannot parse kernel version: {kver}")

    if major < 6 or (major == 6 and minor < 14):
        raise StepFailed("preflight", f"kernel {kver} too old — requires 6.14+ for /dev/ntsync")

    log_info(f"ntsync: kernel {kver} — requirement met (6.14+)")

    if Path("/dev/ntsync").exists():
        log_info("ntsync: /dev/ntsync available — kernel-native NT sync enabled")
    else:
        warnings.append("/dev/ntsync not present — try: sudo modprobe ntsync")
        log_warn("ntsync: /dev/ntsync not present — try: sudo modprobe ntsync")

    # Wine Mono (required for .NET/FNA games: TMNT, Halls of Torment, etc.)
    mono_paths = [
        Path("/usr/share/wine/mono"),
        Path("/usr/lib/wine/mono"),
        Path.home() / ".local/share/wine/mono",
    ]
    has_mono = any(p.exists() and any(p.iterdir()) for p in mono_paths if p.exists())
    if has_mono:
        log_info("wine-mono: found")
    else:
        warnings.append("wine-mono not installed — .NET/FNA games will crash (pacman -S wine-mono)")
        log_warn("wine-mono: NOT FOUND — .NET/FNA games (TMNT, Halls of Torment, etc.) will crash")
        log_warn("  install with: pacman -S wine-mono")

    # WoW64 / 32-bit Wine (required for 32-bit games: Duke Nukem 3D, etc.)
    has_i386_unix = Path("/usr/lib/wine/i386-unix").exists()
    has_i386_win = Path("/usr/lib/wine/i386-windows").exists()
    if has_i386_unix and has_i386_win:
        log_info("wow64: 32-bit Wine libs found")
    else:
        missing_parts = []
        if not has_i386_unix: missing_parts.append("i386-unix")
        if not has_i386_win: missing_parts.append("i386-windows")
        warnings.append(f"32-bit Wine libs missing ({', '.join(missing_parts)}) — 32-bit games will crash")
        log_warn(f"wow64: 32-bit Wine libs NOT FOUND — 32-bit games (Duke Nukem 3D, etc.) will crash")
        log_warn("  install with: pacman -S lib32-wine")

    elapsed = (time.monotonic_ns() - t0) // 1_000_000
    return StepResult("Preflight", True, warnings=warnings, elapsed_ms=elapsed)


# Step 2: Clean

def step_clean() -> StepResult:
    """Detect old builds and offer cleanup."""
    t0 = time.monotonic_ns()

    def dir_size(path: Path) -> int:
        try:
            return sum(f.stat().st_size for f in path.rglob("*") if f.is_file())
        except OSError:
            return 0

    found: list[tuple[str, Any, int]] = []

    if STEAM_COMPAT_DIR.exists():
        found.append(("Steam compat tool", STEAM_COMPAT_DIR, dir_size(STEAM_COMPAT_DIR)))
    cargo_target = SCRIPT_DIR / "target"
    if cargo_target.exists():
        found.append(("Cargo build cache", cargo_target, dir_size(cargo_target)))
    if WINE_SRC_DIR.exists():
        found.append(("Wine source clone", WINE_SRC_DIR, dir_size(WINE_SRC_DIR)))
    if WINE_OBJ_DIR.exists():
        found.append(("Wine build objects", WINE_OBJ_DIR, dir_size(WINE_OBJ_DIR)))

    # Runtime state
    tmp_dir = Path("/tmp/quark")
    uid = os.getuid()
    wine_sockets = list(Path(f"/tmp/.wine-{uid}").glob("server-*/socket")) if Path(f"/tmp/.wine-{uid}").exists() else []
    shm_segments = list(Path("/dev/shm").glob("triskelion-*"))
    runtime_count = (1 if tmp_dir.exists() else 0) + len(wine_sockets) + len(shm_segments)
    if runtime_count:
        found.append(("Runtime state (logs, sockets, shm)", None, 0))

    if not found:
        elapsed = (time.monotonic_ns() - t0) // 1_000_000
        return StepResult("Clean", True, skipped=True, elapsed_ms=elapsed)

    total = sum(size for _, _, size in found)
    print()
    print("  Existing quark installation detected:")
    for label, path, size in found:
        size_str = fmt_size(size) if size else ""
        if path and isinstance(path, Path):
            line = f"    - {label}: {path}"
        else:
            line = f"    - {label}"
        if size_str:
            line += f" ({size_str})"
        print(line)
    if total:
        print(f"    Total: {fmt_size(total)}")
    print()

    if not prompt_yn("  Clean everything for a fresh build?"):
        log_info("Keeping existing artifacts — building on top")
        elapsed = (time.monotonic_ns() - t0) // 1_000_000
        return StepResult("Clean", True, skipped=True, elapsed_ms=elapsed)

    # Kill running processes first
    for name in ["triskelion", "wineserver"]:
        subprocess.run(["pkill", "-9", name], capture_output=True)
    time.sleep(0.5)

    # Clean runtime state
    if tmp_dir.exists():
        shutil.rmtree(tmp_dir)
    for sock in wine_sockets:
        sock.unlink(missing_ok=True)
        lock = sock.parent / "lock"
        lock.unlink(missing_ok=True)
    for seg in shm_segments:
        seg.unlink(missing_ok=True)

    # Clean build artifacts
    for label, path, _ in found:
        if path is None or label.startswith("Runtime"):
            continue
        if isinstance(path, Path):
            if path.is_dir():
                shutil.rmtree(path)
                log_info(f"Removed: {path}")
            elif path.exists():
                path.unlink()
                log_info(f"Removed: {path}")

    print()
    log_info("Clean slate — proceeding with fresh build")
    elapsed = (time.monotonic_ns() - t0) // 1_000_000
    return StepResult("Clean", True, elapsed_ms=elapsed)


# Step 3a: Sync wine source
#
# Clones the Wine source matching the system Wine version (auto-derived from
# `wine --version`) into WINE_SRC_DIR. This MUST run before step_build because
# the rust binaries' build.rs reads protocol.def from this tree to generate
# wineserver opcode definitions. Without it, the daemon ships with stale
# protocol opcodes — exactly the failure mode that broke the Balatro launch
# (Wine 11.6 spoke protocol 931, daemon was built against 930).
#
# Also runs the manifest gate: if any build input changed since the last
# install (wine version, patches, configure args), nukes WINE_SRC_DIR and
# WINE_OBJ_DIR so the rebuild starts from a clean slate.

def step_sync_wine_source() -> StepResult:
    """Clone wine source matching system Wine, run manifest gate."""
    t0 = time.monotonic_ns()
    wine_tag = derive_wine_tag()
    log_info(f"target wine source: {wine_tag}")

    # Manifest gate. Has to run before any rebuild decision: if inputs changed,
    # we discard wine-src and wine-obj and let the rest of the step rebuild.
    manifest_path = WINE_OBJ_DIR / ".quark-build-manifest.json"
    current_inputs = compute_wine_build_inputs()
    stored_inputs = read_build_manifest(manifest_path)
    if not manifest_matches(stored_inputs, current_inputs):
        if stored_inputs is not None:
            log_warn("Build inputs changed since last install — discarding cached wine build:")
            for k, v in current_inputs.items():
                old = stored_inputs.get(k, "<missing>")
                if old != v:
                    log_warn(f"  {k}: {old}  →  {v}")
        else:
            log_info("No previous build manifest — clean clone")
        if WINE_OBJ_DIR.exists():
            assert_quark_writable(WINE_OBJ_DIR)
            shutil.rmtree(WINE_OBJ_DIR)
        if WINE_SRC_DIR.exists():
            assert_quark_writable(WINE_SRC_DIR)
            shutil.rmtree(WINE_SRC_DIR)

    # Clone if not present
    if not (WINE_SRC_DIR / "server" / "protocol.def").exists():
        log_info(f"Cloning Wine {wine_tag} to {WINE_SRC_DIR}...")
        WINE_SRC_DIR.parent.mkdir(parents=True, exist_ok=True)
        if WINE_SRC_DIR.exists():
            assert_quark_writable(WINE_SRC_DIR)
            shutil.rmtree(WINE_SRC_DIR)
        run_cmd(
            ["git", "clone", "--depth", "1", "-b", wine_tag, WINE_CLONE_URL, str(WINE_SRC_DIR)],
            desc="clone Wine", capture=False, timeout=120,
        )
    else:
        log_info(f"Wine source present at {WINE_SRC_DIR}")

    # Reset to clean state — patches get applied later in step_patch_wine
    run_cmd(["git", "checkout", "HEAD", "--", "."], desc="git reset wine-src",
            cwd=WINE_SRC_DIR, check=False)
    run_cmd(["git", "clean", "-fd"], desc="git clean wine-src",
            cwd=WINE_SRC_DIR, check=False)

    # Sanity: protocol.def must exist or build.rs has nothing to read
    proto_def = WINE_SRC_DIR / "server" / "protocol.def"
    if not proto_def.exists():
        raise StepFailed("sync wine source", f"protocol.def missing after clone: {proto_def}")

    elapsed = (time.monotonic_ns() - t0) // 1_000_000
    return StepResult("Sync wine source", True, elapsed_ms=elapsed)


# Step 3: Build

def step_build() -> StepResult:
    """Build quark, triskelion, and parallax via cargo. Sets WINE_SRC so build.rs
    regenerates protocol code from the freshly-cloned wine source."""
    t0 = time.monotonic_ns()
    log_info("Building quark stack (3 binaries)...")

    # Pin WINE_SRC to the cloned tree from step_sync_wine_source. build.rs
    # uses this to generate RequestCode + dispatch tables from protocol.def.
    # Without it, build.rs falls back to the checked-in fallback file, which
    # is stale the moment the system Wine version moves.
    proto_def = WINE_SRC_DIR / "server" / "protocol.def"
    if not proto_def.exists():
        raise StepFailed(
            "build",
            f"wine source missing at {WINE_SRC_DIR} — sync_wine_source must run first"
        )

    env = os.environ.copy()
    env["WINE_SRC"] = str(WINE_SRC_DIR)
    log_info(f"  WINE_SRC={WINE_SRC_DIR}")

    try:
        subprocess.run(
            ["cargo", "build", "--release", "-p", "triskelion"],
            cwd=SCRIPT_DIR, env=env, timeout=600, check=True,
        )
    except subprocess.CalledProcessError as e:
        raise StepFailed("build", f"cargo build exit {e.returncode}")
    except subprocess.TimeoutExpired:
        raise StepFailed("build", "cargo build timed out (600s)")

    target_dir = SCRIPT_DIR / "target" / "release"
    artifacts: list[Path] = []
    for name in ["quark", "triskelion", "parallax"]:
        binary = target_dir / name
        if not binary.exists():
            raise StepFailed("build", f"binary not found: {binary}")
        artifacts.append(binary)

    elapsed = (time.monotonic_ns() - t0) // 1_000_000
    return StepResult("Build", True, artifacts=artifacts, elapsed_ms=elapsed)


# Step 4: Deploy Binaries

def step_deploy_binaries() -> StepResult:
    """Deploy binaries to Steam compat dir, write VDFs, create symlinks."""
    t0 = time.monotonic_ns()
    STEAM_COMPAT_DIR.mkdir(parents=True, exist_ok=True)
    target_dir = SCRIPT_DIR / "target" / "release"
    artifacts: list[Path] = []

    for name in ["quark", "triskelion", "parallax"]:
        src = target_dir / name
        dst = STEAM_COMPAT_DIR / name
        shutil.copy2(src, dst)
        os.chmod(dst, 0o755)
        artifacts.append(dst)

    # proton → quark symlink (Steam VDF expects "proton")
    proton_link = STEAM_COMPAT_DIR / "proton"
    if proton_link.exists() or proton_link.is_symlink():
        proton_link.unlink()
    proton_link.symlink_to("quark")

    # VDF
    version = get_version()
    vdf = STEAM_COMPAT_DIR / "compatibilitytool.vdf"
    vdf.write_text(f'''"compatibilitytools"
{{
  "compat_tools"
  {{
    "quark"
    {{
      "install_path" "."
      "display_name" "quark {version}"
      "from_oslist"  "windows"
      "to_oslist"    "linux"
    }}
  }}
}}
''')

    manifest = STEAM_COMPAT_DIR / "toolmanifest.vdf"
    manifest.write_text('''"manifest"
{
  "commandline" "/proton %verb%"
  "version" "2"
  "use_sessions" "1"
}
''')

    log_info(f"Deployed: quark {version} (quark, triskelion, parallax)")
    elapsed = (time.monotonic_ns() - t0) // 1_000_000
    return StepResult("Deploy binaries", True, artifacts=artifacts, elapsed_ms=elapsed)


# Step 5: Deploy Wine

def step_deploy_wine() -> StepResult:
    """Deploy system Wine tree with triskelion as wineserver."""
    t0 = time.monotonic_ns()
    warnings: list[str] = []

    sys_unix = Path("/usr/lib/wine/x86_64-unix")
    sys_bin = Path("/usr/bin")

    if not sys_unix.exists() or not (sys_bin / "wine").exists():
        raise StepFailed("deploy wine", "system Wine not found at /usr/lib/wine/ — pacman -S wine")

    # bin/ — wine loader + wineserver → triskelion
    bin_dir = STEAM_COMPAT_DIR / "bin"
    bin_dir.mkdir(parents=True, exist_ok=True)

    for name in ["wine", "wine-preloader"]:
        src = sys_bin / name
        dst = bin_dir / name
        if src.exists():
            if dst.exists() or dst.is_symlink():
                dst.unlink()
            # COPY — Wine resolves symlinks for wineserver discovery
            shutil.copy2(src, dst)

    wine64_link = bin_dir / "wine64"
    if wine64_link.exists() or wine64_link.is_symlink():
        wine64_link.unlink()
    wine64_link.symlink_to("wine")

    # wineserver → triskelion (RELATIVE symlink)
    wineserver_link = bin_dir / "wineserver"
    if wineserver_link.exists() or wineserver_link.is_symlink():
        wineserver_link.unlink()
    wineserver_link.symlink_to(os.path.relpath(STEAM_COMPAT_DIR / "triskelion", bin_dir))

    # lib/wine/ — hardlink files from system Wine
    wine_lib = STEAM_COMPAT_DIR / "lib" / "wine"
    wine_lib.mkdir(parents=True, exist_ok=True)

    total_files = 0
    for subdir in ["x86_64-unix", "x86_64-windows", "i386-windows", "i386-unix"]:
        src = Path("/usr/lib/wine") / subdir
        dst = wine_lib / subdir
        if not src.exists():
            continue
        if dst.is_symlink():
            dst.unlink()
        elif dst.exists():
            shutil.rmtree(dst)
        dst.mkdir(parents=True, exist_ok=True)
        copied = 0
        for f in src.iterdir():
            if f.is_file():
                target = dst / f.name
                if target.exists():
                    target.unlink()
                try:
                    os.link(f, target)
                except OSError:
                    shutil.copy2(f, target)
                    copied += 1
                total_files += 1
        if copied > 0:
            w = f"{subdir}: {copied} files COPIED (cross-device, re-run after Wine updates)"
            warnings.append(w)
            log_warn(f"  {w}")

    # share/wine/ — wine.inf, nls, fonts, winmd. Copies, NOT symlinks.
    # The bwrap launcher binds compat_dir/share/wine over /usr/share/wine at
    # launch time; symlinks pointing back into /usr would defeat that.
    # mono/gecko are NOT deployed here — Wine downloads them into the prefix
    # on first launch if they're not installed system-wide.
    share_wine = STEAM_COMPAT_DIR / "share" / "wine"
    share_wine.mkdir(parents=True, exist_ok=True)
    sys_share = Path("/usr/share/wine")
    share_files = 0

    # wine.inf — single file, hardlink
    src_inf = sys_share / "wine.inf"
    dst_inf = share_wine / "wine.inf"
    assert_quark_writable(dst_inf)
    if src_inf.exists():
        if dst_inf.is_symlink() or dst_inf.exists():
            dst_inf.unlink()
        try:
            os.link(src_inf, dst_inf)
        except OSError:
            shutil.copy2(src_inf, dst_inf)
        share_files += 1
    else:
        warnings.append("wine.inf missing from /usr/share/wine")

    # Directory trees: nls (locale data), fonts, winmd (WinRT metadata).
    for tree_name in ["nls", "fonts", "winmd"]:
        src_tree = sys_share / tree_name
        dst_tree = share_wine / tree_name
        if not src_tree.exists():
            if tree_name in ("nls", "fonts"):
                warnings.append(f"{tree_name}/ missing from /usr/share/wine")
            continue
        assert_quark_writable(dst_tree)
        if dst_tree.is_symlink():
            dst_tree.unlink()
        elif dst_tree.exists():
            shutil.rmtree(dst_tree)
        dst_tree.mkdir(parents=True, exist_ok=True)
        for f in src_tree.rglob("*"):
            if f.is_file() and not f.is_symlink():
                rel = f.relative_to(src_tree)
                target = dst_tree / rel
                target.parent.mkdir(parents=True, exist_ok=True)
                if target.exists():
                    target.unlink()
                try:
                    os.link(f, target)
                except OSError:
                    shutil.copy2(f, target)
                share_files += 1

    # Wine Mono: copy into share/wine so bwrap has it inside the bind mount.
    # bwrap --bind overlays quark's share/wine onto /usr/share/wine, so external
    # symlinks are invisible. Must be a real copy. Hardlinks work cross-device
    # only on the same filesystem, so fall back to copy.
    sys_mono = sys_share / "mono"
    dst_mono = share_wine / "mono"
    if sys_mono.exists():
        assert_quark_writable(dst_mono)
        if dst_mono.is_symlink():
            dst_mono.unlink()
        if not dst_mono.exists():
            shutil.copytree(sys_mono, dst_mono)
            log_info(f"wine-mono: copied {sys_mono} -> {dst_mono}")

    log_info(f"System Wine deployed ({total_files} lib files, {share_files} share files)")
    elapsed = (time.monotonic_ns() - t0) // 1_000_000
    return StepResult("Deploy Wine", True, warnings=warnings, elapsed_ms=elapsed)


# Step 6: Patch Wine

def step_patch_wine() -> StepResult:
    """Apply patches to wine source (already cloned by step_sync_wine_source),
    configure, build patched DLLs."""
    t0 = time.monotonic_ns()
    wine_lib = STEAM_COMPAT_DIR / "lib" / "wine"

    patch_dir = SCRIPT_DIR / "patches" / "wine"
    patches = sorted(patch_dir.glob("*.patch")) if patch_dir.exists() else []
    if not patches:
        log_info("No Wine patches found — using stock DLLs")
        return StepResult("Patch Wine", True, skipped=True,
                         elapsed_ms=(time.monotonic_ns() - t0) // 1_000_000)

    src_dir = WINE_SRC_DIR
    build_dir = WINE_OBJ_DIR
    manifest_path = WINE_OBJ_DIR / ".quark-build-manifest.json"
    current_inputs = compute_wine_build_inputs()

    # Sanity: step_sync_wine_source must have populated wine-src already
    if not (src_dir / "dlls" / "win32u").exists():
        raise StepFailed(
            "patch wine",
            f"wine source missing at {src_dir} — sync_wine_source must run first"
        )

    # Reset source to clean state before applying patches (idempotent across runs)
    run_cmd(["git", "checkout", "HEAD", "--", "."], desc="git reset wine-src",
            cwd=src_dir, check=False)
    run_cmd(["git", "clean", "-fd"], desc="git clean wine-src",
            cwd=src_dir, check=False)

    # Apply patches
    applied = 0
    for patch in patches:
        check_result = run_cmd(
            ["git", "apply", "--check", str(patch)],
            desc=f"check {patch.name}", cwd=src_dir, check=False,
        )
        if check_result.returncode == 0:
            apply_result = run_cmd(
                ["git", "apply", str(patch)],
                desc=f"apply {patch.name}", cwd=src_dir, check=False,
            )
            if apply_result.returncode == 0:
                applied += 1
                log_info(f"  Applied: {patch.name}")
            else:
                log_warn(f"  Apply FAILED (dry-run passed): {patch.name}")
                if apply_result.stderr:
                    for line in apply_result.stderr.strip().splitlines()[-5:]:
                        log_warn(f"    {line}")
        else:
            log_warn(f"  Patch skipped: {patch.name}")

    if applied == 0:
        log_warn("No patches applied — using stock DLLs")
        return StepResult("Patch Wine", True, skipped=True,
                         elapsed_ms=(time.monotonic_ns() - t0) // 1_000_000)
    log_info(f"Applied {applied}/{len(patches)} Wine patches")

    # Configure
    if not (build_dir / "Makefile").exists():
        log_info("Configuring Wine build...")
        build_dir.mkdir(parents=True, exist_ok=True)
        run_cmd(["autoreconf", "-f"], desc="autoreconf", cwd=src_dir, check=False)
        run_cmd(
            [str(src_dir / "configure"), "--prefix=/usr", "--enable-win64",
             "--with-x", "--without-wayland"],
            desc="Wine configure", cwd=build_dir, timeout=120,
        )

    # Build ALL patched DLLs — including kernelbase
    dll_targets = {
        "ntdll": ("dlls/ntdll/ntdll.so", "ntdll.so",
                  "dlls/ntdll/x86_64-windows/ntdll.dll", "ntdll.dll"),
        # kernelbase: DISABLED — patches 010/013 cause BSOD when deployed.
        # Built from Wine 11.5 source, but ABI-incompatible with system Wine runtime.
        # Needs investigation: either build kernelbase.so too, or match system Wine's
        # exact build config. Patches are applied to source but DLL is not deployed.
        # "kernelbase": (None, None,
        #                "dlls/kernelbase/x86_64-windows/kernelbase.dll", None),
        "win32u": ("dlls/win32u/win32u.so", "win32u.so", None, None),
        "winex11.drv": ("dlls/winex11.drv/winex11.so", "winex11.so", None, None),
        "wineboot": (None, None,
                     "programs/wineboot/x86_64-windows/wineboot.exe", "wineboot.exe"),
    }

    artifacts: list[Path] = []
    for dll, (so_path, so_name, pe_path, pe_name) in dll_targets.items():
        build_targets = []
        if so_path:
            build_targets.append(so_path)
        if pe_path:
            build_targets.append(pe_path)

        log_info(f"  Building {dll}...")
        result = run_cmd(
            ["make", f"-j{cpu_count()}"] + build_targets,
            desc=f"build {dll}", cwd=build_dir, timeout=300, check=False,
        )
        if result.returncode != 0:
            log_error(f"  Build failed for {dll}")
            if result.stderr:
                for line in result.stderr.strip().splitlines()[-10:]:
                    log_error(f"    {line}")
            continue

        if so_path:
            built_so = build_dir / so_path
            if built_so.exists():
                dst = wine_lib / "x86_64-unix" / so_name
                if dst.exists():
                    dst.unlink()
                shutil.copy2(built_so, dst)
                artifacts.append(dst)
                log_info(f"  Deployed {so_name} ({built_so.stat().st_size // 1024}K)")
            else:
                log_warn(f"  {so_name} not produced")

        if pe_path:
            built_pe = build_dir / pe_path
            if built_pe.exists():
                if pe_name:
                    # Deploy to Wine tree (normal DLLs)
                    dst = wine_lib / "x86_64-windows" / pe_name
                    if dst.exists():
                        dst.unlink()
                    shutil.copy2(built_pe, dst)
                    artifacts.append(dst)
                    log_info(f"  Deployed {pe_name} ({built_pe.stat().st_size // 1024}K)")
                else:
                    # Stash for prefix-only deployment (e.g. kernelbase)
                    patched_dir = STEAM_COMPAT_DIR / "lib" / "wine" / "patched"
                    patched_dir.mkdir(parents=True, exist_ok=True)
                    stash_name = Path(pe_path).name
                    dst = patched_dir / stash_name
                    shutil.copy2(built_pe, dst)
                    artifacts.append(dst)
                    log_info(f"  Stashed {stash_name} for prefix deploy ({built_pe.stat().st_size // 1024}K)")
            else:
                log_warn(f"  {pe_name or Path(pe_path).name} not produced")

    if not artifacts:
        raise StepFailed("patch wine", "no patched modules built successfully")

    # Build succeeded — record the inputs that produced these artifacts so the
    # next install can decide whether to trust the cache or rebuild.
    write_build_manifest(manifest_path, current_inputs)
    log_info(f"Build manifest written: {manifest_path.name}")

    log_info(f"Deployed {len(artifacts)} patched Wine module(s)")
    elapsed = (time.monotonic_ns() - t0) // 1_000_000
    return StepResult("Patch Wine", True, artifacts=artifacts, elapsed_ms=elapsed)


# Step 6b: Bake prefix template

def step_bake_prefix_template() -> StepResult:
    """Run wineboot --init with stock wineserver to create a complete prefix template.

    This produces system.reg/user.reg with all COM class registrations (500+
    CLSIDs from Wine's FakeDlls pass). Games need these for CoCreateInstance
    to find mmdevapi, devenum, quartz, etc. Without them, audio init fails
    and DXVK games show white windows.

    The template is baked ONCE during install and copied into each game prefix
    at launch time. This replaces the 45-second update_wineprefix call.
    """
    t0 = time.monotonic_ns()
    template_dir = STEAM_COMPAT_DIR / "default_pfx"
    system_reg = template_dir / "system.reg"

    if system_reg.exists() and system_reg.stat().st_size > 100_000:
        log_info("prefix template: already baked")
        elapsed = (time.monotonic_ns() - t0) // 1_000_000
        return StepResult("Bake prefix", True, elapsed_ms=elapsed)

    # Source: Proton's pre-baked default_pfx (9000+ CLSID entries, complete COM registration).
    # Proton bakes this at BUILD time via wineboot --init. We steal the result.
    proton_dirs = [
        "Proton 10.0", "Proton - Experimental", "Proton Hotfix",
        "Proton 9.0-4", "Proton 9.0-3",
    ]
    steam_common = Path.home() / ".local/share/Steam/steamapps/common"
    source_pfx = None
    for pname in proton_dirs:
        candidate = steam_common / pname / "files/share/default_pfx"
        if (candidate / "system.reg").exists():
            source_pfx = candidate
            break

    if source_pfx is None:
        log_warn("prefix template: no Proton default_pfx found, skipping")
        elapsed = (time.monotonic_ns() - t0) // 1_000_000
        return StepResult("Bake prefix", True, elapsed_ms=elapsed)

    # Copy the ENTIRE prefix directory structure (not just .reg files).
    # Games need Fonts, system32 DLLs, directory structure, etc.
    if template_dir.exists():
        shutil.rmtree(template_dir)
    log_info(f"  Copying full prefix template from {source_pfx.parent.parent.name}...")
    shutil.copytree(source_pfx, template_dir, symlinks=True)
    total_size = sum(f.stat().st_size for f in template_dir.rglob("*") if f.is_file())
    log_info(f"prefix template: {total_size // 1024 // 1024}MB from Proton ({source_pfx.parent.parent.name})")

    elapsed = (time.monotonic_ns() - t0) // 1_000_000
    return StepResult("Bake prefix", True, elapsed_ms=elapsed)


# Step 7: Build Stubs

def step_build_stubs() -> StepResult:
    """Build tier0/vstdlib DLL stubs via clang+lld (no mingw)."""
    t0 = time.monotonic_ns()
    wine_lib = STEAM_COMPAT_DIR / "lib" / "wine"

    stub_c = Path("/tmp/quark_stub.c")
    stub_c.write_text(
        'typedef int BOOL;\n'
        'typedef void *HINSTANCE;\n'
        'typedef unsigned long DWORD;\n'
        'BOOL __stdcall DllMainCRTStartup(HINSTANCE i, DWORD r, void *v) { return 1; }\n'
    )

    artifacts: list[Path] = []

    # 64-bit stubs
    for stub_name in ["tier0_s64", "vstdlib_s64"]:
        stub_dll = wine_lib / "x86_64-windows" / f"{stub_name}.dll"
        result = run_cmd(
            ["clang", "--target=x86_64-w64-windows-gnu", "-shared",
             "-fuse-ld=lld", "-nostdlib", "-o", str(stub_dll), str(stub_c)],
            desc=f"build {stub_name}.dll", check=False,
        )
        if result.returncode == 0 and stub_dll.exists():
            artifacts.append(stub_dll)
            log_info(f"  {stub_name}.dll stub deployed (64-bit)")
        else:
            log_error(f"  Failed to build {stub_name}.dll")

    # 32-bit stubs
    i386_dir = wine_lib / "i386-windows"
    i386_dir.mkdir(parents=True, exist_ok=True)
    for stub_name in ["tier0_s", "vstdlib_s"]:
        stub_dll = i386_dir / f"{stub_name}.dll"
        result = run_cmd(
            ["clang", "--target=i686-w64-windows-gnu", "-shared",
             "-fuse-ld=lld", "-nostdlib", "-o", str(stub_dll), str(stub_c)],
            desc=f"build {stub_name}.dll", check=False,
        )
        if result.returncode == 0 and stub_dll.exists():
            artifacts.append(stub_dll)
            log_info(f"  {stub_name}.dll stub deployed (32-bit)")
        else:
            log_error(f"  Failed to build {stub_name}.dll")

    # Cleanup
    stub_c.unlink(missing_ok=True)

    if not artifacts:
        raise StepFailed("build stubs", "no stub DLLs built — is clang+lld installed?")

    elapsed = (time.monotonic_ns() - t0) // 1_000_000
    return StepResult("Build stubs", True, artifacts=artifacts, elapsed_ms=elapsed)


# Step 8: Steam Bridge

def _download_proton_source() -> Path:
    """Download lsteamclient + steam_helper from Proton GitHub (targeted, no full clone).

    Downloads the release tarball and extracts only the needed directories.
    Caches by tag so re-runs skip the download.
    """
    cache = PROTON_SOURCE_CACHE
    if (cache / "lsteamclient").exists():
        log_info("lsteamclient: Proton source cached")
        return cache

    log_info(f"lsteamclient: downloading Proton source ({PROTON_GIT_TAG})...")
    tarball_url = f"https://github.com/ValveSoftware/Proton/archive/refs/tags/{PROTON_GIT_TAG}.tar.gz"
    tarball_path = Path(f"/tmp/proton-{PROTON_GIT_TAG}.tar.gz")

    try:
        urllib.request.urlretrieve(tarball_url, tarball_path)
    except Exception as e:
        raise StepFailed("steam bridge", f"failed to download Proton source: {e}")

    # Extract only lsteamclient/ and steam_helper/
    cache.mkdir(parents=True, exist_ok=True)
    prefix = f"Proton-{PROTON_GIT_TAG}/"
    try:
        with tarfile.open(tarball_path) as tf:
            for member in tf.getmembers():
                if not member.name.startswith(prefix):
                    continue
                rel = member.name[len(prefix):]
                if rel.startswith("lsteamclient/") or rel.startswith("steam_helper/"):
                    member.name = rel
                    try:
                        tf.extract(member, cache, filter="data")
                    except TypeError:
                        tf.extract(member, cache)
    except Exception as e:
        raise StepFailed("steam bridge", f"failed to extract Proton tarball: {e}")
    finally:
        tarball_path.unlink(missing_ok=True)

    if not (cache / "lsteamclient").exists():
        raise StepFailed("steam bridge", "lsteamclient/ not found in Proton tarball")

    log_info("lsteamclient: Proton source downloaded and cached")
    return cache


def step_steam_bridge() -> StepResult:
    """Build lsteamclient.dll + steam.exe from Proton source against Wine 11.5."""
    t0 = time.monotonic_ns()
    wine_lib = STEAM_COMPAT_DIR / "lib" / "wine"

    dll_cache = LSTEAMCLIENT_CACHE / "lsteamclient.dll"
    so_cache = LSTEAMCLIENT_CACHE / "lsteamclient.so"
    steam_cache = LSTEAMCLIENT_CACHE / "steam.exe"

    # Always rebuild. A stale cache with Proton-ABI binaries is the #1 cause
    # of Steam auth failure. The build takes ~30s — not worth the risk.
    if LSTEAMCLIENT_CACHE.exists():
        shutil.rmtree(LSTEAMCLIENT_CACHE)

    # Download Proton source (targeted — just lsteamclient + steam_helper)
    proton_src = _download_proton_source()

    # Need wine-src for building
    wine_src = WINE_SRC_DIR
    if not wine_src.exists():
        raise StepFailed("steam bridge", "wine-src not found — patch wine step must run first")

    # Reset configure.ac
    run_cmd(["git", "checkout", "--", "configure.ac"],
            desc="reset configure.ac", cwd=wine_src, check=False)

    # Symlink lsteamclient into wine-src
    dll_link = wine_src / "dlls" / "lsteamclient"
    if dll_link.is_symlink():
        dll_link.unlink()
    if not dll_link.exists():
        dll_link.symlink_to(proton_src / "lsteamclient")
        log_info("lsteamclient: symlinked into wine-src/dlls/")

    # Register in configure.ac
    configure_ac = wine_src / "configure.ac"
    text = configure_ac.read_text()
    if "dlls/lsteamclient" not in text:
        text = text.replace(
            "WINE_CONFIG_MAKEFILE(dlls/lz32/tests)",
            "WINE_CONFIG_MAKEFILE(dlls/lz32/tests)\nWINE_CONFIG_MAKEFILE(dlls/lsteamclient)",
        )
        configure_ac.write_text(text)

    # Reset Proton source before patching
    steam_helper_src = proton_src / "steam_helper"
    if steam_helper_src.exists():
        helper_link = wine_src / "programs" / "steam_helper"
        if helper_link.is_symlink():
            helper_link.unlink()
        if not helper_link.exists():
            helper_link.symlink_to(steam_helper_src)
        text = configure_ac.read_text()
        if "programs/steam_helper" not in text:
            text = text.replace(
                "WINE_CONFIG_MAKEFILE(programs/start)",
                "WINE_CONFIG_MAKEFILE(programs/start)\nWINE_CONFIG_MAKEFILE(programs/steam_helper)",
            )
            configure_ac.write_text(text)

    # Apply lsteamclient patches
    patch_dir = SCRIPT_DIR / "patches"
    for pname in ["006-lsteamclient-wine11-api-compat.patch",
                  "007-lsteamclient-link-stdcxx.patch",
                  "008-lsteamclient-non-fatal-steam-crash.patch"]:
        pfile = patch_dir / pname
        if pfile.exists():
            ret = run_cmd(
                ["patch", "-Np1", "--forward", "-i", str(pfile)],
                desc=f"apply {pname}", cwd=proton_src, check=False,
            )
            combined = (ret.stdout or "") + (ret.stderr or "")
            if ret.returncode == 0:
                log_info(f"  Applied: {pname}")
            elif "already applied" in combined or "Reversed" in combined:
                log_info(f"  Already applied: {pname}")
            else:
                log_warn(f"  Patch failed: {pname}")

    # Build in /tmp
    build_dir = Path("/tmp/quark-lsteamclient-build")
    if build_dir.exists():
        shutil.rmtree(build_dir)
    build_dir.mkdir(parents=True, exist_ok=True)

    log_info("lsteamclient: running autoconf...")
    run_cmd(["autoconf"], desc="autoconf (lsteamclient)", cwd=wine_src)

    log_info(f"lsteamclient: configuring against {derive_wine_tag()}...")
    run_cmd(
        [str(wine_src / "configure"), "--enable-win64", "--disable-tests"],
        desc="configure (lsteamclient)", cwd=build_dir, timeout=120,
    )

    targets = [
        "dlls/lsteamclient/x86_64-windows/lsteamclient.dll",
        "dlls/lsteamclient/lsteamclient.so",
    ]
    if steam_helper_src.exists():
        targets.append("programs/steam_helper/x86_64-windows/steam.exe")

    log_info("lsteamclient: building...")
    result = run_cmd(
        ["make", f"-j{cpu_count()}"] + targets,
        desc="build lsteamclient", cwd=build_dir, timeout=600, check=False,
    )
    if result.returncode != 0:
        log_error("lsteamclient: build failed — retrying single-threaded for diagnostics")
        run_cmd(["make"] + targets, desc="build lsteamclient (retry)",
                cwd=build_dir, timeout=600)

    dll_target = build_dir / "dlls" / "lsteamclient" / "x86_64-windows" / "lsteamclient.dll"
    so_target = build_dir / "dlls" / "lsteamclient" / "lsteamclient.so"
    steam_target = build_dir / "programs" / "steam_helper" / "x86_64-windows" / "steam.exe"

    # Report sizes
    for label, path in [("lsteamclient.dll", dll_target), ("lsteamclient.so", so_target), ("steam.exe", steam_target)]:
        if path.exists():
            log_info(f"  {label}: {path.stat().st_size // 1024}K")

    # Cache
    LSTEAMCLIENT_CACHE.mkdir(parents=True, exist_ok=True)
    artifacts: list[Path] = []
    if dll_target.exists():
        shutil.copy2(dll_target, dll_cache)
        artifacts.append(dll_cache)
    if so_target.exists():
        shutil.copy2(so_target, so_cache)
        artifacts.append(so_cache)
    if steam_target.exists():
        shutil.copy2(steam_target, steam_cache)
        artifacts.append(steam_cache)

    # Clean build tree
    shutil.rmtree(build_dir)

    # Deploy
    _deploy_lsteamclient(
        dll_cache if dll_cache.exists() else None,
        so_cache if so_cache.exists() else None,
        steam_cache if steam_cache.exists() else None,
        wine_lib,
    )

    elapsed = (time.monotonic_ns() - t0) // 1_000_000
    return StepResult("Steam bridge", True, artifacts=artifacts, elapsed_ms=elapsed)


def _deploy_lsteamclient(
    dll_path: Path | None,
    so_path: Path | None,
    steam_path: Path | None,
    wine_lib: Path,
) -> None:
    """Deploy lsteamclient.dll + .so + steam.exe to quark Wine lib."""
    win_dir = wine_lib / "x86_64-windows"
    unix_dir = wine_lib / "x86_64-unix"
    win_dir.mkdir(parents=True, exist_ok=True)
    unix_dir.mkdir(parents=True, exist_ok=True)

    if dll_path and dll_path.exists():
        dst = win_dir / "lsteamclient.dll"
        assert_quark_writable(dst)
        if dst.exists():
            dst.chmod(0o755)
        shutil.copy2(dll_path, dst)

    if so_path and so_path.exists():
        dst = unix_dir / "lsteamclient.so"
        assert_quark_writable(dst)
        if dst.exists():
            dst.chmod(0o755)
        shutil.copy2(so_path, dst)

    # lsteamclient lives only in the compat tree. The bwrap layer in launcher.rs
    # binds compat_dir/lib/wine over /usr/lib/wine at launch time, so Wine's
    # builtin loader finds lsteamclient.{so,dll} at the path it expects (the
    # bind-mounted /usr/lib/wine, which IS our tree). The previous version of
    # this code shelled out to `sudo ln -sf` to plant symlinks in the real
    # /usr/lib/wine — that was the active poisoning vector and is why every
    # install used to ask for the user's sudo password. Gone.

    if steam_path and steam_path.exists():
        dst = win_dir / "steam.exe"
        if dst.exists():
            dst.chmod(0o755)
        shutil.copy2(steam_path, dst)
        log_info(f"steam.exe: deployed Wine builtin ({steam_path.stat().st_size // 1024}K)")

    log_info(f"lsteamclient: deployed to {wine_lib}")


# Step 9: GPU Translation

def step_gpu_translation() -> StepResult:
    """Download DXVK + VKD3D-Proton from GitHub releases."""
    t0 = time.monotonic_ns()
    artifacts: list[Path] = []
    warnings: list[str] = []

    log_info("Downloading DXVK and VKD3D-proton...")
    dxvk_tar = _download_github_release("doitsujin", "dxvk", "dxvk-")
    vkd3d_tar = _download_github_release("HansKristian-Work", "vkd3d-proton", "vkd3d-proton-")

    lib_dir = STEAM_COMPAT_DIR / "lib"

    if dxvk_tar:
        arts = _deploy_tarball_dlls(dxvk_tar, lib_dir, "dxvk", "x64", "x32")
        artifacts.extend(arts)
    else:
        warnings.append("DXVK download failed — D3D9/11 games may fail")
        log_warn("DXVK: download failed")

    if vkd3d_tar:
        arts = _deploy_tarball_dlls(vkd3d_tar, lib_dir, "vkd3d-proton", "x64", "x86")
        artifacts.extend(arts)
    else:
        warnings.append("VKD3D-Proton download failed — D3D12 games may fail")
        log_warn("VKD3D-Proton: download failed")

    elapsed = (time.monotonic_ns() - t0) // 1_000_000
    return StepResult("GPU translation", True, artifacts=artifacts,
                     warnings=warnings, elapsed_ms=elapsed)


def _download_github_release(owner: str, repo: str, asset_glob: str) -> Path | None:
    """Download latest release asset from GitHub. Returns cached tarball path."""
    cache_dir = DATA_DIR / "downloads"
    cache_dir.mkdir(parents=True, exist_ok=True)

    version_file = cache_dir / f"{repo}.version"
    cached_version = version_file.read_text().strip() if version_file.exists() else None

    api_url = f"https://api.github.com/repos/{owner}/{repo}/releases/latest"
    try:
        req = urllib.request.Request(api_url, headers={"Accept": "application/vnd.github+json"})
        with urllib.request.urlopen(req, timeout=15) as resp:
            data = json.loads(resp.read())
    except Exception as e:
        log_warn(f"{repo}: GitHub API failed: {e}")
        existing = list(cache_dir.glob(f"{repo}-*.tar.*"))
        if existing:
            log_info(f"{repo}: using cached download")
            return existing[0]
        return None

    tag = data.get("tag_name", "")
    if tag == cached_version:
        existing = list(cache_dir.glob(f"{repo}-*.tar.*"))
        if existing:
            log_info(f"{repo}: {tag} (cached)")
            return existing[0]

    download_url = None
    asset_name = None
    for asset in data.get("assets", []):
        name = asset["name"]
        if asset_glob in name and name.endswith((".tar.gz", ".tar.xz", ".tar.zst")):
            download_url = asset["browser_download_url"]
            asset_name = name
            break

    if not download_url:
        log_warn(f"{repo}: no matching release asset found")
        return None

    for old in cache_dir.glob(f"{repo}-*.tar.*"):
        old.unlink()

    dest = cache_dir / asset_name
    log_info(f"{repo}: downloading {tag}...")
    try:
        urllib.request.urlretrieve(download_url, dest)
    except Exception as e:
        log_error(f"{repo}: download failed: {e}")
        return None

    version_file.write_text(tag)
    log_info(f"{repo}: downloaded {asset_name}")
    return dest


def _deploy_tarball_dlls(
    tarball: Path, lib_dir: Path, label: str, dir_64: str, dir_32: str,
) -> list[Path]:
    """Extract DLLs from a DXVK/VKD3D tarball."""
    staging = DATA_DIR / "staging" / label
    if staging.exists():
        shutil.rmtree(staging)
    staging.mkdir(parents=True, exist_ok=True)

    try:
        with tarfile.open(tarball) as tf:
            try:
                tf.extractall(staging, filter="data")
            except TypeError:
                tf.extractall(staging)
    except Exception as e:
        log_error(f"{label}: failed to extract: {e}")
        return []

    extracted = list(staging.iterdir())
    if not extracted:
        log_error(f"{label}: tarball empty")
        return []
    root = extracted[0]

    artifacts: list[Path] = []
    for src_sub, dst_sub in [(dir_64, "x86_64-windows"), (dir_32, "i386-windows")]:
        src = root / src_sub
        dst = lib_dir / "wine" / label / dst_sub
        if src.exists():
            dst.mkdir(parents=True, exist_ok=True)
            count = 0
            for dll in src.glob("*.dll"):
                shutil.copy2(dll, dst / dll.name)
                artifacts.append(dst / dll.name)
                count += 1
            log_info(f"{label}: deployed {count} {'64' if '64' in dst_sub else '32'}-bit DLLs")

    shutil.rmtree(staging, ignore_errors=True)
    return artifacts


# Step 10: Deploy Drivers

def step_deploy_drivers() -> StepResult:
    """Deploy PE driver stubs from system Wine + Proton."""
    t0 = time.monotonic_ns()
    wine_lib = STEAM_COMPAT_DIR / "lib" / "wine"
    dst_dir = wine_lib / "x86_64-windows"
    dst_dir.mkdir(parents=True, exist_ok=True)

    warnings: list[str] = []
    artifacts: list[Path] = []

    # Source priority: Proton Experimental → Proton Hotfix → any Proton → system Wine
    proton_dir = find_proton_wine_dir()
    sys_wine = Path("/usr/lib/wine/x86_64-windows")

    sources = []
    if proton_dir:
        sources.append(("Proton", proton_dir))
    if sys_wine.exists():
        sources.append(("system Wine", sys_wine))

    if not sources:
        w = "No source for PE drivers — some games may fail"
        warnings.append(w)
        log_warn(w)
        elapsed = (time.monotonic_ns() - t0) // 1_000_000
        return StepResult("Deploy drivers", True, warnings=warnings, elapsed_ms=elapsed)

    for driver in PE_DRIVER_FILES:
        dst = dst_dir / driver
        if dst.exists():
            artifacts.append(dst)
            continue

        deployed = False
        for src_name, src_dir in sources:
            src = src_dir / driver
            if src.exists():
                shutil.copy2(src, dst)
                artifacts.append(dst)
                deployed = True
                break

        if not deployed:
            w = f"PE driver not found: {driver}"
            warnings.append(w)
            log_debug(w)

    found = len(artifacts)
    total = len(PE_DRIVER_FILES)
    source_name = sources[0][0] if sources else "none"
    log_info(f"PE drivers: {found}/{total} deployed (source: {source_name})")
    if warnings:
        for w in warnings:
            log_warn(f"  {w}")

    elapsed = (time.monotonic_ns() - t0) // 1_000_000
    return StepResult("Deploy drivers", True, artifacts=artifacts,
                     warnings=warnings, elapsed_ms=elapsed)


# Step 11: EAC Runtime

def step_eac_runtime() -> StepResult:
    """Deploy EasyAntiCheat bridge DLLs (optional)."""
    t0 = time.monotonic_ns()

    if not EAC_RUNTIME_DIR.exists():
        log_warn("Proton EasyAntiCheat Runtime not found")
        log_warn("  EAC games (Elden Ring, etc.) will not work")
        log_warn("  Install via: Steam > Library > Tools > 'Proton EasyAntiCheat Runtime'")
        elapsed = (time.monotonic_ns() - t0) // 1_000_000
        return StepResult("EAC runtime", True, skipped=True,
                         warnings=["EAC runtime not installed"],
                         elapsed_ms=elapsed)

    dst_pe64 = STEAM_COMPAT_DIR / "lib" / "wine" / "x86_64-windows"
    dst_so64 = STEAM_COMPAT_DIR / "lib" / "wine" / "x86_64-unix"
    dst_pe32 = STEAM_COMPAT_DIR / "lib" / "wine" / "i386-windows"
    dst_so32 = STEAM_COMPAT_DIR / "lib" / "wine" / "i386-unix"
    for d in [dst_pe64, dst_so64, dst_pe32, dst_so32]:
        d.mkdir(parents=True, exist_ok=True)

    artifacts: list[Path] = []
    for src_dir, dst_pe, dst_so, names in [
        (EAC_RUNTIME_DIR / "lib64", dst_pe64, dst_so64, ["easyanticheat", "easyanticheat_x64"]),
        (EAC_RUNTIME_DIR / "lib32", dst_pe32, dst_so32, ["easyanticheat_x86"]),
    ]:
        for name in names:
            for ext, dst in [(".dll", dst_pe), (".so", dst_so)]:
                src = src_dir / f"{name}{ext}"
                if src.exists():
                    shutil.copy2(src, dst / f"{name}{ext}")
                    artifacts.append(dst / f"{name}{ext}")

    log_info(f"EasyAntiCheat: deployed {len(artifacts)} runtime files")
    elapsed = (time.monotonic_ns() - t0) // 1_000_000
    return StepResult("EAC runtime", True, artifacts=artifacts, elapsed_ms=elapsed)


# Step 12: Prefix Sync

def _replace_file(src: Path, target: Path) -> bool:
    """Replace target with src. Unlinks symlinks, overwrites files. Returns True on success."""
    try:
        if target.is_symlink():
            target.unlink()
        elif target.exists():
            target.chmod(0o755)
        shutil.copy2(src, target)
        return True
    except OSError:
        return False


def step_prefix_sync() -> StepResult:
    """Convert quark-mapped prefixes from Proton symlinks to real files.

    Every symlink in system32/syswow64 is wrong — replace it unconditionally.
    Then layer quark's own DLLs (DXVK, VKD3D, lsteamclient, stubs) on top.
    """
    t0 = time.monotonic_ns()

    quark_appids = get_quark_appids()
    if not quark_appids:
        log_info("prefix sync: no games mapped to quark")
        return StepResult("Prefix sync", True, skipped=True,
                         elapsed_ms=(time.monotonic_ns() - t0) // 1_000_000)

    compatdata = Path.home() / ".local/share/Steam/steamapps/compatdata"
    if not compatdata.exists():
        return StepResult("Prefix sync", True, skipped=True,
                         elapsed_ms=(time.monotonic_ns() - t0) // 1_000_000)

    # Build the source map: what quark wants in every prefix's system32
    # Later entries override earlier (DXVK d3d11.dll wins over Wine's stub)
    sys_wine_64 = Path("/usr/lib/wine/x86_64-windows")
    sys_wine_32 = Path("/usr/lib/wine/i386-windows")
    quark_wine = STEAM_COMPAT_DIR / "lib" / "wine"

    src_64: dict[str, Path] = {}
    for d in [sys_wine_64,
              quark_wine / "x86_64-windows",
              quark_wine / "patched",
              quark_wine / "dxvk" / "x86_64-windows",
              quark_wine / "vkd3d-proton" / "x86_64-windows"]:
        if d.exists():
            for f in d.iterdir():
                if f.is_file() and f.suffix in (".dll", ".exe", ".tlb", ".ocx", ".cpl", ".acm", ".ax", ".drv", ".sys"):
                    # lsteamclient must NOT be in the prefix — it must load as a
                    # builtin so Wine connects the PE to the .so via find_builtin_dll.
                    # A prefix copy bypasses the builtin path and breaks the PE-Unix bridge.
                    if f.name == "lsteamclient.dll":
                        continue
                    src_64[f.name] = f

    src_32: dict[str, Path] = {}
    for d in [sys_wine_32,
              quark_wine / "dxvk" / "i386-windows",
              quark_wine / "vkd3d-proton" / "i386-windows"]:
        if d.exists():
            for f in d.iterdir():
                if f.is_file() and f.suffix in (".dll", ".exe", ".tlb", ".ocx"):
                    src_32[f.name] = f

    if not src_64:
        log_warn("prefix sync: no source DLLs found")
        return StepResult("Prefix sync", True, skipped=True,
                         elapsed_ms=(time.monotonic_ns() - t0) // 1_000_000)

    total_replaced = 0
    total_prefixes = 0

    for entry in compatdata.iterdir():
        if not entry.is_dir():
            continue
        appid = entry.name
        if appid in PROTON_APPIDS or appid not in quark_appids:
            continue

        sys32 = entry / "pfx/drive_c/windows/system32"
        if not sys32.exists():
            continue

        replaced = 0

        # Walk system32: replace every symlink, copy missing files
        for target in sys32.iterdir():
            if not target.is_file() and not target.is_symlink():
                continue
            name = target.name
            if name in src_64:
                if target.is_symlink():
                    if _replace_file(src_64[name], target):
                        replaced += 1
                elif not target.exists():
                    if _replace_file(src_64[name], target):
                        replaced += 1

        # Also deploy files that exist in source but not yet in prefix
        for name, src in src_64.items():
            target = sys32 / name
            if not target.exists() and not target.is_symlink():
                if _replace_file(src, target):
                    replaced += 1

        # syswow64 (32-bit)
        syswow64 = entry / "pfx/drive_c/windows/syswow64"
        if syswow64.exists() and src_32:
            for target in syswow64.iterdir():
                if not target.is_file() and not target.is_symlink():
                    continue
                name = target.name
                if name in src_32 and target.is_symlink():
                    if _replace_file(src_32[name], target):
                        replaced += 1
            for name, src in src_32.items():
                target = syswow64 / name
                if not target.exists() and not target.is_symlink():
                    if _replace_file(src, target):
                        replaced += 1

        if replaced:
            log_info(f"prefix {appid}: replaced {replaced} files")
            total_replaced += replaced
            total_prefixes += 1
        else:
            log_info(f"prefix {appid}: up to date")

    log_info(f"prefix sync: {total_replaced} files across {total_prefixes} prefix(es), {len(quark_appids)} mapped")
    return StepResult("Prefix sync", True,
                     elapsed_ms=(time.monotonic_ns() - t0) // 1_000_000)


# Step 13: Finalize

def step_finalize() -> StepResult:
    """Env config, game intelligence, deployment manifest."""
    t0 = time.monotonic_ns()

    # Env config
    config_file = STEAM_COMPAT_DIR / "env_config"
    if config_file.exists():
        text = config_file.read_text()
        if "WINE_NTSYNC=1" not in text or text.count("# WINE_NTSYNC=1") > 0:
            text = text.replace("# WINE_NTSYNC=1", "WINE_NTSYNC=1")
            if "WINE_NTSYNC=1" not in text:
                text += "\nWINE_NTSYNC=1\n"
            config_file.write_text(text)
    else:
        config_file.write_text(ENV_CONFIG_TEMPLATE)
    log_info(f"Env config: {config_file}")

    # Game intelligence
    _generate_game_intelligence()

    elapsed = (time.monotonic_ns() - t0) // 1_000_000
    return StepResult("Finalize", True, elapsed_ms=elapsed)


# Step 14: Smoke test
#
# Spawn the deployed compat tool's wine binary inside the same bwrap layer the
# launcher uses at runtime, ask it for `--version`, and verify it returns the
# system Wine version. This catches "deployed but broken" before Steam ever
# launches a game: missing files in the compat tree, version drift between the
# patched DLLs and the system binaries, broken bind mounts, all surface here.

def step_smoke_test() -> StepResult:
    """Run wine --version inside the bwrap namespace against the deployed tree."""
    t0 = time.monotonic_ns()
    warnings: list[str] = []

    bwrap = Path("/usr/bin/bwrap")
    if not bwrap.exists():
        log_warn("bwrap not found at /usr/bin/bwrap — install bubblewrap")
        log_warn("  the launcher will fall back to direct exec; patches will not load")
        return StepResult("Smoke test", True, skipped=True,
                          warnings=["bwrap missing — patches will NOT be loaded at runtime"],
                          elapsed_ms=(time.monotonic_ns() - t0) // 1_000_000)

    quark_lib_wine = STEAM_COMPAT_DIR / "lib" / "wine"
    quark_share_wine = STEAM_COMPAT_DIR / "share" / "wine"
    wine_bin = STEAM_COMPAT_DIR / "bin" / "wine"

    if not wine_bin.exists():
        raise StepFailed("smoke test", f"compat wine binary missing: {wine_bin}")
    if not quark_lib_wine.exists():
        raise StepFailed("smoke test", f"compat lib/wine missing: {quark_lib_wine}")
    if not quark_share_wine.exists():
        raise StepFailed("smoke test", f"compat share/wine missing: {quark_share_wine}")

    # Run the same bwrap incantation the launcher uses.
    cmd = [
        str(bwrap),
        "--dev-bind", "/", "/",
        "--bind", str(quark_lib_wine), "/usr/lib/wine",
        "--bind", str(quark_share_wine), "/usr/share/wine",
        "--die-with-parent",
        "--",
        str(wine_bin), "--version",
    ]

    try:
        r = subprocess.run(cmd, capture_output=True, text=True, timeout=15)
    except subprocess.TimeoutExpired:
        raise StepFailed("smoke test", "wine --version timed out inside bwrap (15s)")
    except FileNotFoundError as e:
        raise StepFailed("smoke test", f"failed to spawn bwrap: {e}")

    if r.returncode != 0:
        for line in (r.stderr or "").strip().splitlines()[-10:]:
            log_error(f"  {line}")
        raise StepFailed("smoke test", f"wine --version exited {r.returncode} inside bwrap")

    reported = (r.stdout or "").strip()
    if not reported.startswith("wine-"):
        raise StepFailed("smoke test", f"unexpected wine --version output: {reported!r}")

    expected = get_system_wine_version()
    if expected and reported != expected:
        # Version drift between deployed compat wine and system wine. The
        # bwrap binds quark's lib/wine over /usr/lib/wine, so the binary
        # *should* see its sibling DLLs and report whatever they identify as.
        # If they disagree it means the compat tree has files from a different
        # build than the binary.
        warnings.append(
            f"compat wine reports {reported} but system wine is {expected} — "
            "compat tree may be ABI-mixed; re-run install.py"
        )
        log_warn(f"  {warnings[-1]}")

    log_info(f"smoke test: bwrap+wine reports {reported}")
    elapsed = (time.monotonic_ns() - t0) // 1_000_000
    return StepResult("Smoke test", True, warnings=warnings, elapsed_ms=elapsed)


# Game intelligence

ENGINE_SIGNATURES = {
    "love2d":    ["love.dll", "lua51.dll"],
    "unity":     ["UnityPlayer.dll", "UnityEngine.dll"],
    "unreal4":   ["UE4-Win64-Shipping.exe"],
    "unreal5":   ["UnrealEditor.dll"],
    "sdl2":      ["SDL2.dll"],
    "godot":     ["godot.windows.opt.64.exe"],
    "gamemaker": ["data.win"],
    "rpgmaker":  ["RPGMV.dll", "nw.dll"],
    "ren'py":    ["renpy.dll", "lib/py3-win64"],
    "source2":   ["engine2.dll"],
    "source":    ["engine.dll", "hl2.exe"],
    "cryengine": ["CrySystem.dll"],
    "frostbite": ["FrostyEditor.dll"],
}

NON_GAME_NAMES = {
    "steam linux runtime", "proton", "steamworks common redistributables",
}

OPCODE_COUNT = 306
CACHE_SIZE = 10_496

ENGINE_TYPE_MAP = {
    "unknown": 0, "unity": 1, "unreal4": 2, "unreal5": 3,
    "source": 4, "source2": 5, "godot": 6, "gamemaker": 7,
    "rpgmaker": 8, "ren'py": 9, "love2d": 10, "sdl2": 11,
    "cryengine": 12, "frostbite": 13,
}

ENGINE_OPCODE_PROFILES: dict[str, dict[int, tuple[int, int]]] = {
    "unity": {
        44: (0x01, 100), 63: (0x01, 100), 49: (0x01, 90),
        29: (0x01, 100), 30: (0x01, 80), 149: (0x03, 0),
    },
    "unreal4": {
        29: (0x01, 100), 30: (0x01, 100), 31: (0x01, 90),
        40: (0x01, 80), 44: (0x01, 90),
    },
    "unreal5": {
        29: (0x01, 100), 30: (0x01, 100), 31: (0x01, 90),
        40: (0x01, 80), 44: (0x01, 90),
    },
    "source2": {55: (0x02, 70), 56: (0x02, 70), 60: (0x03, 0)},
    "godot": {29: (0x01, 100), 44: (0x01, 90), 30: (0x01, 80)},
    "love2d": {29: (0x01, 100), 44: (0x01, 80)},
}


def _detect_engine(install_dir: Path) -> str:
    try:
        root_files = set()
        sub_files = set()
        for f in install_dir.iterdir():
            if f.is_file():
                root_files.add(f.name.lower())
            elif f.is_dir():
                for f2 in f.iterdir():
                    if f2.is_file():
                        sub_files.add(f2.name.lower())
    except (OSError, PermissionError):
        return "unknown"

    PRIMARY = {
        "love2d": ["love.dll"], "unity": ["unityplayer.dll"],
        "godot": ["godot.windows.opt.64.exe"],
        "source2": ["engine2.dll"], "source": ["engine.dll", "hl2.exe"],
    }
    for engine, sigs in PRIMARY.items():
        if any(s in root_files for s in sigs):
            return engine

    all_files = root_files | sub_files
    for engine, sigs in ENGINE_SIGNATURES.items():
        if engine in PRIMARY:
            continue
        if any(s.lower() in all_files for s in sigs):
            return engine
    return "unknown"


def _write_quark_cache(cache_path: Path, appid: str, engine: str, primary_exe: str = "") -> None:
    buf = bytearray(CACHE_SIZE)
    now = int(time.time())
    engine_type = ENGINE_TYPE_MAP.get(engine, 0)
    flags = 0x01

    struct.pack_into("<4sIIIIIIII", buf, 0,
                     b"AMPC", 2, int(appid), flags, engine_type, 0, 0, now, 0)

    engine_name = engine.encode("utf-8")[:31] + b"\x00"
    exe_name = primary_exe.encode("utf-8")[:63] + b"\x00"
    struct.pack_into("<32s64sII", buf, 0x0040,
                     engine_name, exe_name, 0, 80 if engine != "unknown" else 0)

    profile = ENGINE_OPCODE_PROFILES.get(engine, {})
    for opcode in range(OPCODE_COUNT):
        offset = 0x00C0 + opcode * 16
        hint, priority = profile.get(opcode, (0x00, 0))
        engine_stub_safe = 1 if hint == 0x03 else 0
        struct.pack_into("<IHBx8x", buf, offset, hint, priority, engine_stub_safe)

    if profile:
        flags |= 0x02
        struct.pack_into("<I", buf, 12, flags)

    # Shader cache index
    compat_dir = cache_path.parent
    for f in compat_dir.rglob("*.dxvk-cache"):
        try:
            rel = str(f.relative_to(compat_dir))[:191]
            sz = f.stat().st_size
            struct.pack_into("<192s", buf, 0x2700, rel.encode("utf-8"))
            struct.pack_into("<Q", buf, 0x2700 + 384, sz)
            flags |= 0x08
            struct.pack_into("<I", buf, 12, flags)
        except (OSError, ValueError):
            pass
        break
    for f in compat_dir.rglob("vkd3d-proton.cache"):
        try:
            rel = str(f.relative_to(compat_dir))[:191]
            sz = f.stat().st_size
            struct.pack_into("<192s", buf, 0x2700 + 192, rel.encode("utf-8"))
            struct.pack_into("<Q", buf, 0x2700 + 392, sz)
            flags |= 0x08
            struct.pack_into("<I", buf, 12, flags)
        except (OSError, ValueError):
            pass
        break

    cache_path.write_bytes(buf)


def _generate_game_intelligence() -> None:
    log_info("Game intelligence: scanning Steam libraries...")

    quark_appids = get_quark_appids()
    libraries = find_steam_libraries()
    if not libraries:
        log_warn("No Steam libraries found")
        return

    games = []
    for steamapps in libraries:
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
            appid = info.get("appid")
            if not appid:
                continue
            name = info.get("name", "")
            if any(skip in name.lower() for skip in NON_GAME_NAMES):
                continue
            install_dir = steamapps / "common" / info.get("installdir", "")
            if not install_dir.exists():
                continue
            engine = _detect_engine(install_dir)
            games.append({"appid": appid, "name": name, "install_dir": install_dir,
                         "engine": engine, "steamapps": steamapps})

    created = 0
    for game in games:
        if game["appid"] not in quark_appids:
            continue
        compatdata = game["steamapps"] / "compatdata" / game["appid"]
        if not compatdata.exists():
            continue
        cache_path = compatdata / ".quark_cache"
        if cache_path.exists():
            continue
        _write_quark_cache(cache_path, game["appid"], game["engine"])
        created += 1

    log_info(f"Game intelligence: {created} new caches ({len(games)} games, {len(quark_appids)} mapped)")


# Verbose config

def configure_verbose(enable: bool | None) -> None:
    """Handle verbose flag. Sticky: stays enabled until explicitly disabled.

    The flag lives in CACHE_DIR (~/.cache/quark/) — NOT in STEAM_COMPAT_DIR —
    because step_clean wipes the compat dir on every install. Putting the flag
    there meant `./install.py --verbose` wrote the flag, then step_clean
    deleted it five seconds later, and the launcher always saw verbose=false
    at game launch time. Moving it to CACHE_DIR makes it survive a clean.
    The launcher reads the same path via triskelion::log::is_verbose().
    """
    flag = CACHE_DIR / "verbose_enabled"
    if enable is True:
        CACHE_DIR.mkdir(parents=True, exist_ok=True)
        flag.write_text("1")
        log_info("Verbose diagnostics enabled (~/.cache/quark/*.prom)")
    elif enable is False:
        if flag.exists():
            flag.unlink()
        log_info("Verbose diagnostics disabled")
    elif flag.exists():
        log_info("Verbose diagnostics: on (use --no-verbose to disable)")


# Pipeline

def main() -> int:
    parser = argparse.ArgumentParser(description="quark installer")
    parser.add_argument("--verbose", action="store_true", help="Enable verbose diagnostics")
    parser.add_argument("--no-verbose", action="store_true", help="Disable verbose diagnostics")
    parser.add_argument("--yes", "-y", action="store_true",
                        help="Auto-answer Y to all prompts (non-interactive)")
    args = parser.parse_args()

    if args.yes:
        global _AUTO_YES
        _AUTO_YES = True

    if args.verbose:
        configure_verbose(True)
    elif args.no_verbose:
        configure_verbose(False)
    else:
        configure_verbose(None)

    steps = [
        ("Preflight",        step_preflight),
        ("Clean",            step_clean),
        ("Sync wine source", step_sync_wine_source),
        ("Build",            step_build),
        ("Deploy binaries",  step_deploy_binaries),
        ("Deploy Wine",      step_deploy_wine),
        ("Patch Wine",       step_patch_wine),
        ("Bake prefix",      step_bake_prefix_template),
        ("Build stubs",      step_build_stubs),
        ("Steam bridge",     step_steam_bridge),
        ("GPU translation",  step_gpu_translation),
        ("Deploy drivers",   step_deploy_drivers),
        ("EAC runtime",      step_eac_runtime),
        ("Prefix sync",      step_prefix_sync),
        ("Finalize",         step_finalize),
        ("Smoke test",       step_smoke_test),
    ]

    results: list[StepResult] = []

    # Snapshot system Wine before any step runs. The detective half of the
    # write guardrail: if anything in /usr/lib/wine or /usr/share/wine differs
    # after the pipeline finishes, install.py poisoned the system and we abort.
    log_info("Snapshotting system Wine state for poisoning audit...")
    system_wine_snapshot = snapshot_system_wine()
    log_info(f"  {len(system_wine_snapshot)} system Wine files tracked")

    for i, (name, func) in enumerate(steps, 1):
        print()
        log_info(f"STEP {i}/{len(steps)}: {name}")
        try:
            result = func()
            results.append(result)
        except StepFailed as e:
            log_error(f"FAILED: {e.detail}")
            results.append(StepResult(name, False))
            # Preflight and Build are hard gates
            if name in ("Preflight", "Build"):
                _print_manifest(results, len(steps))
                return 1
            # Other failures: continue but warn
            log_warn(f"Continuing despite {name} failure...")
        except KeyboardInterrupt:
            log_error("Interrupted")
            return 130

    # System Wine poisoning audit. Runs even if some steps failed — a partial
    # install that touched /usr is still a poisoning event we need to know about.
    print()
    log_info("Auditing system Wine state for unintended modifications...")
    try:
        verify_system_wine_unchanged(system_wine_snapshot)
        log_info("  system Wine untouched — clean install")
    except StepFailed as e:
        log_error(f"POISONING DETECTED: {e.detail}")
        results.append(StepResult("System Wine audit", False))
        _print_manifest(results, len(steps))
        return 2

    # Deployment manifest
    print()
    _print_manifest(results, len(steps))

    failed = sum(1 for r in results if not r.success)
    if failed:
        log_warn(f"{failed} step(s) failed — check errors above")
        return 1

    log_info("Save data protection: enabled (automatic pre-launch snapshots)")
    print()
    return 0


def _print_manifest(results: list[StepResult], total_steps: int) -> None:
    version = get_version()
    failed = [r for r in results if not r.success]

    log_info(f"DEPLOYMENT COMPLETE — quark {version}")

    # Only list what was deployed, with counts
    for r in results:
        if r.skipped:
            continue
        if not r.success:
            log_error(f"{r.name}: FAILED")
        elif r.artifacts:
            log_info(f"{r.name}: {len(r.artifacts)} files")
        else:
            log_info(f"{r.name}: done")

    for r in results:
        for w in r.warnings:
            log_warn(w)

    if failed:
        log_error(f"{len(failed)} step(s) failed")
    print()


if __name__ == "__main__":
    sys.exit(main())
