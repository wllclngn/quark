#!/usr/bin/env python3
"""Verify that the Wine and lsteamclient patches are applied, compiled and
deployed.

  1. Deployment: The patched files exist with non-zero size
  2. Source: Every patch's added lines are present in the tree it patches,
     derived from the patch itself, so a new patch is covered with no edit
  3. Binary: strings, symbols and PE exports the patches compile in
  4. Runtime: The tree's own loader, the launcher's env and the last game log

A check that cannot run (no source tree, no game log yet) is a SKIP, never a
PASS. Exit 0 = no failures. Exit 1 = failures found.
"""

import hashlib
import re
import subprocess
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))
from quark_common import (
    Checks, log_section, log_warn,
    REPO_ROOT, COMPAT_DIR, CACHE_DIR, LOG_DIR, WINE_SRC_DIR, LSTEAMCLIENT_CACHE,
)

WINE_PATCHES = REPO_ROOT / "patches" / "wine"
LSTEAMCLIENT_PATCHES = REPO_ROOT / "patches" / "lsteamclient"
PROTON_SOURCE_DIR = CACHE_DIR / "proton-source"

C = Checks("patches")


def file_exists(path: Path, desc: str) -> bool:
    if path.exists() and path.stat().st_size > 0:
        C.ok(f"{desc}: {path.name} ({path.stat().st_size // 1024}K)")
        return True
    C.fail(f"{desc}: MISSING or empty — {path}")
    return False


def has_string(path: Path, needle: str, desc: str) -> bool:
    if not path.exists():
        C.fail(f"{desc}: file missing — {path}")
        return False
    data = path.read_bytes()
    found = needle.encode() in data or needle.encode("utf-16-le") in data
    return C.check(found, f"{desc}: '{needle}' found", f"{desc}: '{needle}' NOT FOUND in {path.name}")


def has_symbol(path: Path, symbol: str, desc: str) -> bool:
    if not path.exists():
        C.fail(f"{desc}: file missing — {path}")
        return False
    r = subprocess.run(["nm", "-D", str(path)], capture_output=True, text=True)
    return C.check(symbol in r.stdout, f"{desc}: symbol '{symbol}' exported",
                 f"{desc}: symbol '{symbol}' NOT exported from {path.name}")


def has_pe_export(path: Path, export_name: str, desc: str) -> bool:
    if not path.exists():
        C.fail(f"{desc}: file missing — {path}")
        return False
    r = subprocess.run(["objdump", "-p", str(path)], capture_output=True, text=True)
    return C.check(export_name in r.stdout, f"{desc}: PE export '{export_name}'",
                 f"{desc}: PE export '{export_name}' NOT FOUND in {path.name}")


def patch_targets(patch: Path) -> list[tuple[str, str]]:
    """(file, added line) for each file the patch edits. The longest added
    line is the one least likely to exist in an unpatched tree."""
    longest: dict[str, str] = {}
    current = None
    for line in patch.read_text(errors="replace").splitlines():
        if line.startswith("+++ "):
            path = line[4:].split("\t")[0]
            current = None if path == "/dev/null" else path.removeprefix("b/")
            continue
        if current and line.startswith("+"):
            text = line[1:].strip()
            if len(text) > len(longest.get(current, "")):
                longest[current] = text
    return list(longest.items())


def check_patch_source(patch: Path, tree: Path | None) -> None:
    if tree is None or not tree.exists():
        C.skip(f"{patch.name}: no source tree to check against")
        return
    for rel, added in patch_targets(patch):
        src = tree / rel
        if not src.exists():
            C.fail(f"{patch.name}: patched file missing — {src}")
            continue
        C.check(added in src.read_text(errors="replace"),
              f"{patch.name}: {rel}",
              f"{patch.name}: NOT APPLIED to {rel} (missing: {added[:60]})")


def newest_proton_source() -> Path | None:
    trees = [d for d in PROTON_SOURCE_DIR.glob("*") if (d / "lsteamclient").is_dir()]
    return max(trees, key=lambda d: d.stat().st_mtime, default=None)


def run_tests() -> int:
    unix_dir = COMPAT_DIR / "lib/wine/x86_64-unix"
    win_dir = COMPAT_DIR / "lib/wine/x86_64-windows"
    sys_unix = Path("/usr/lib/wine/x86_64-unix")
    ntdll_so = unix_dir / "ntdll.so"
    ntdll_dll = win_dir / "ntdll.dll"
    lsteam_so = unix_dir / "lsteamclient.so"
    lsteam_dll = win_dir / "lsteamclient.dll"

    log_section("deployment", "verify")
    file_exists(COMPAT_DIR / "proton", "quark launcher")
    file_exists(ntdll_so, "ntdll.so (patched)")
    file_exists(ntdll_dll, "ntdll.dll (patched)")
    file_exists(lsteam_so, "lsteamclient.so")
    file_exists(lsteam_dll, "lsteamclient.dll")

    log_section("Wine source", "verify")
    for patch in sorted(WINE_PATCHES.glob("*.patch")):
        check_patch_source(patch, WINE_SRC_DIR)

    log_section("Proton source", "verify")
    proton_src = newest_proton_source()
    for patch in sorted(LSTEAMCLIENT_PATCHES.glob("*.patch")):
        check_patch_source(patch, proton_src)

    log_section("patch 001 steamclient trampoline", "verify")
    # C string literals may be compiled without the exact source text surviving.
    # Check for substrings that DO survive compilation as UTF-16LE or ASCII.
    has_string(ntdll_dll, "tier0_s64", "ntdll.dll: tier0_s64 redirect")
    has_string(ntdll_dll, "vstdlib_s64", "ntdll.dll: vstdlib_s64 redirect")

    log_section("patch 002 __wine_unix_call export", "verify")
    has_string(ntdll_dll, "compat___wine_unix_call", "ntdll.dll: compat___wine_unix_call string")
    has_pe_export(ntdll_dll, "__wine_unix_call", "ntdll.dll PE export table")

    log_section("patch 003 EAC load order", "verify")
    has_string(ntdll_so, "PROTON_EAC_LAUNCHER_PROCESS", "ntdll.so: EAC launcher env check")

    log_section("lsteamclient ABI", "verify")
    has_symbol(lsteam_so, "steamclient_init_registry", "lsteamclient.so")
    has_symbol(lsteam_so, "steamclient_init", "lsteamclient.so")
    has_symbol(lsteam_so, "__wine_unix_call_funcs", "lsteamclient.so PE-Unix bridge")
    has_pe_export(lsteam_dll, "steamclient_init_registry", "lsteamclient.dll")
    # The .so must be freshly built (not older than the deployed ntdll.so)
    ntdll_mtime = ntdll_so.stat().st_mtime if ntdll_so.exists() else 0
    lsteam_mtime = lsteam_so.stat().st_mtime if lsteam_so.exists() else 0
    C.check(lsteam_mtime >= ntdll_mtime - 3600,
          "lsteamclient.so: build timestamp current",
          "lsteamclient.so: OLDER than ntdll.so — may be stale cached build")
    cached_so = LSTEAMCLIENT_CACHE / "lsteamclient.so"
    if cached_so.exists() and lsteam_so.exists():
        h1 = hashlib.sha256(cached_so.read_bytes()).hexdigest()[:16]
        h2 = hashlib.sha256(lsteam_so.read_bytes()).hexdigest()[:16]
        C.check(h1 == h2, f"cache matches deployed ({h1})",
              f"cache MISMATCH: cached={h1} deployed={h2} — stale cache at {LSTEAMCLIENT_CACHE}")

    log_section("loader", "verify")
    # lsteamclient lives only in compat_dir/lib/wine/. The launcher runs the
    # tree's bin/wine, a copy of the system loader, and Wine resolves lib/wine
    # relative to it; a symlinked loader would resolve /proc/self/exe to the
    # system's and load the system tree. A copy of the bridge in the real
    # /usr/lib/wine means a past install poisoned the system.
    tree_wine = COMPAT_DIR / "bin" / "wine"
    C.check(tree_wine.is_file() and not tree_wine.is_symlink(),
          "bin/wine: a copy of the loader, not a symlink",
          "bin/wine: SYMLINK or missing — the tree's modules would not load (re-run install.py)")
    for leak in (sys_unix / "lsteamclient.so",
                 Path("/usr/lib/wine/x86_64-windows/lsteamclient.dll")):
        C.check(not leak.exists() and not leak.is_symlink(),
              f"/usr/lib/wine clean: no {leak.name} leak",
              f"/usr/lib/wine LEAK: {leak} exists — left over from an old install (sudo rm to clean)")

    log_section("launcher env", "verify")
    launcher = COMPAT_DIR / "proton"
    for needle in ("WINEDLLPATH", "WINEDLLOVERRIDES", "lsteamclient", "WINESERVER"):
        has_string(launcher, needle, f"launcher: {needle}")

    log_section("Steam bridge", "verify")
    steam_exe = win_dir / "steam.exe"
    if file_exists(steam_exe, "steam.exe (Proton steam_helper)"):
        has_string(steam_exe, "steamclient_init_registry", "steam.exe: calls steamclient_init_registry")
        has_string(steam_exe, "lsteamclient", "steam.exe: loads lsteamclient")
        has_string(steam_exe, "setup_steam_registry", "steam.exe: setup_steam_registry function")

    log_section("stub DLLs", "verify")
    for name in ["tier0_s64.dll", "vstdlib_s64.dll"]:
        file_exists(win_dir / name, f"64-bit stub: {name}")
    i386_dir = COMPAT_DIR / "lib/wine/i386-windows"
    for name in ["tier0_s.dll", "vstdlib_s.dll"]:
        file_exists(i386_dir / name, f"32-bit stub: {name}")

    log_section("runtime", "verify")
    stderr_log = LOG_DIR / "wine_stderr.log"
    if stderr_log.exists():
        stderr_text = stderr_log.read_text(errors="replace")
        if "Failed to load lsteamclient module" in stderr_text:
            C.fail("runtime: lsteamclient.dll failed to load (LoadLibraryW returned NULL)")
        elif "Failed to find steamclient_init_registry export" in stderr_text:
            log_warn("runtime: steamclient_init_registry not found (PE-Unix bridge not connecting)")
        else:
            C.ok("runtime: no lsteamclient errors")
        faults = re.findall(r'page fault.*address ([0-9A-Fa-f]+)', stderr_text)
        for addr in faults:
            C.fail(f"runtime: page fault at 0x{addr}")
        if not faults:
            C.ok("runtime: no page faults")
    else:
        C.skip("runtime: no wine_stderr.log (no game run yet)")

    return C.summary()
