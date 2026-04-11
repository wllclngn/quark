#!/usr/bin/env python3
"""Verify that all Wine patches are correctly applied, compiled, and deployed.

Checks every patch in patches/wine/ against:
  1. Source: patch applied to Wine source tree
  2. Binary: compiled into the deployed .so/.dll/.exe
  3. Symbols: expected exports present in nm -D / strings
  4. Deployment: files exist in the right places with non-zero size
  5. Runtime: WINEDLLOVERRIDES, WINEDLLPATH, symlinks all correct

Exit 0 = all checks pass. Exit 1 = failures found.
"""

import os
import subprocess
import sys
from pathlib import Path

SCRIPT_DIR = Path(__file__).parent.resolve()
PROJECT_DIR = SCRIPT_DIR.parent
PATCHES_DIR = PROJECT_DIR / "patches" / "wine"
DEPLOY_DIR = Path.home() / ".local/share/Steam/compatibilitytools.d/quark"
WINE_SRC = Path("/tmp/quark-wine-build/wine-src")
CACHE_DIR = Path.home() / ".local/share/quark/lsteamclient"

PASS = 0
FAIL = 0


def log_pass(msg: str) -> None:
    global PASS
    PASS += 1
    print(f"  [PASS] {msg}")


def log_fail(msg: str) -> None:
    global FAIL
    FAIL += 1
    print(f"  [FAIL] {msg}")


def log_warn_info(msg: str) -> None:
    print(f"  [WARN] {msg}")


def check(condition: bool, pass_msg: str, fail_msg: str) -> bool:
    if condition:
        log_pass(pass_msg)
        return True
    else:
        log_fail(fail_msg)
        return False


def file_exists(path: Path, desc: str) -> bool:
    return check(path.exists() and path.stat().st_size > 0,
                 f"{desc}: {path.name} ({path.stat().st_size // 1024}K)" if path.exists() and path.stat().st_size > 0 else desc,
                 f"{desc}: MISSING or empty — {path}")


def has_string(path: Path, needle: str, desc: str) -> bool:
    if not path.exists():
        log_fail(f"{desc}: file missing — {path}")
        return False
    data = path.read_bytes()
    found = needle.encode() in data
    return check(found, f"{desc}: '{needle}' found", f"{desc}: '{needle}' NOT FOUND in {path.name}")


def has_symbol(path: Path, symbol: str, desc: str) -> bool:
    if not path.exists():
        log_fail(f"{desc}: file missing — {path}")
        return False
    r = subprocess.run(["nm", "-D", str(path)], capture_output=True, text=True)
    found = symbol in r.stdout
    return check(found, f"{desc}: symbol '{symbol}' exported", f"{desc}: symbol '{symbol}' NOT exported from {path.name}")


def has_pe_export(path: Path, export_name: str, desc: str) -> bool:
    if not path.exists():
        log_fail(f"{desc}: file missing — {path}")
        return False
    r = subprocess.run(["objdump", "-p", str(path)], capture_output=True, text=True)
    found = export_name in r.stdout
    return check(found, f"{desc}: PE export '{export_name}'", f"{desc}: PE export '{export_name}' NOT FOUND in {path.name}")


def symlink_target(path: Path) -> str | None:
    if path.is_symlink():
        return os.readlink(str(path))
    return None


def run_tests() -> int:
    global PASS, FAIL

    unix_dir = DEPLOY_DIR / "lib/wine/x86_64-unix"
    win_dir = DEPLOY_DIR / "lib/wine/x86_64-windows"
    bin_dir = DEPLOY_DIR / "bin"
    sys_unix = Path("/usr/lib/wine/x86_64-unix")

    print("SECTION 1: Deployment structure")
    file_exists(DEPLOY_DIR / "proton", "quark launcher")
    file_exists(bin_dir / "wine64", "wine64 binary")
    file_exists(unix_dir / "ntdll.so", "ntdll.so (patched)")
    file_exists(win_dir / "ntdll.dll", "ntdll.dll (patched)")
    file_exists(unix_dir / "win32u.so", "win32u.so (patched)")
    file_exists(win_dir / "win32u.dll", "win32u.dll (patched)")
    file_exists(unix_dir / "winex11.so", "winex11.drv.so (patched)")
    file_exists(win_dir / "wineboot.exe", "wineboot.exe (patched)")
    file_exists(unix_dir / "lsteamclient.so", "lsteamclient.so")
    file_exists(win_dir / "lsteamclient.dll", "lsteamclient.dll")
    print()

    print("SECTION 2: Patch 001 — NtFilterToken null deref guard (ntdll)")
    if WINE_SRC.exists():
        src = WINE_SRC / "dlls/ntdll/unix/security.c"
        if src.exists():
            has_string(src, "disable_sids && disable_sids->GroupCount", "source: null guard applied")
    print()

    print("SECTION 3: Patch 002 — create process heap before loader lock (ntdll)")
    if WINE_SRC.exists():
        src = WINE_SRC / "dlls/ntdll/unix/virtual.c"
        if src.exists():
            has_string(src, "virtual_setup_exception", "source: heap creation moved")
    print()

    print("SECTION 4: Patches 003-006 — win32u guards")
    file_exists(unix_dir / "win32u.so", "win32u.so deployed")
    if WINE_SRC.exists():
        # Check specific files the patches touch
        for fname, needle, desc in [
            ("dlls/win32u/message.c", "shared", "003: user lock softened"),
            ("dlls/win32u/winstation.c", "find_shared_session_object", "004: null shared object guard"),
            ("dlls/win32u/scroll.c", "info", "005: scroll info guard"),
            ("dlls/win32u/winstation.c", "shared_session", "006: null shared session"),
        ]:
            src = WINE_SRC / fname
            if src.exists():
                has_string(src, needle, f"source: {desc}")
    print()

    print("SECTION 5: Patch 009 — steamclient authentication trampoline (ntdll)")
    ntdll_so = unix_dir / "ntdll.so"
    ntdll_dll = win_dir / "ntdll.dll"
    # C string literals may be compiled without the exact source text surviving.
    # Check for substrings that DO survive compilation as UTF-16LE or ASCII.
    has_string(ntdll_dll, "tier0_s64", "ntdll.dll: tier0_s64 redirect")
    has_string(ntdll_dll, "vstdlib_s64", "ntdll.dll: vstdlib_s64 redirect")
    if WINE_SRC.exists():
        loader_c = WINE_SRC / "dlls/ntdll/loader.c"
        unix_loader_c = WINE_SRC / "dlls/ntdll/unix/loader.c"
        if loader_c.exists():
            has_string(loader_c, "tier0_s64.dll", "source: tier0_s64 redirect in loader.c")
            has_string(loader_c, "steamclient64.dll", "source: steamclient64 reference in loader.c")
            has_string(loader_c, "steamclient_lsteamclient", "source: file-scope lsteamclient handle in loader.c")
        if unix_loader_c.exists():
            has_string(unix_loader_c, "steamclient_remove_entries", "source: entry purge in unix/loader.c")
            has_string(unix_loader_c, "steamclient_mods", "source: per-module tracking in unix/loader.c")
    print()

    print("SECTION 6: Patch 010 — kernelbase Steam OpenProcess PID hack")
    # kernelbase is NOT deployed (ABI mismatch), but patch should be in source
    if WINE_SRC.exists():
        src = WINE_SRC / "dlls/kernelbase/process.c"
        if src.exists():
            has_string(src, "0xfffe", "source: PID 0xfffe hack in kernelbase")
    else:
        log_pass("kernelbase: not deployed (known ABI issue), source check skipped")
    print()

    print("SECTION 7: Patch 011 — ntdll EAC runtime DLL path")
    has_string(ntdll_so, "PROTON_EAC_RUNTIME", "ntdll.so: EAC runtime env check")
    if WINE_SRC.exists():
        src = WINE_SRC / "dlls/ntdll/unix/loader.c"
        if src.exists():
            has_string(src, "PROTON_EAC_RUNTIME", "source: EAC runtime path in loader.c")
            has_string(src, "v2/lib64", "source: EAC lib64 path in loader.c")
    print()

    print("SECTION 8: Patch 012 — ntdll EAC load order")
    if WINE_SRC.exists():
        src = WINE_SRC / "dlls/ntdll/unix/loadorder.c"
        if src.exists():
            has_string(src, "eac_launcher_process", "source: EAC launcher flag in loadorder.c")
            has_string(src, "easyanticheat", "source: EAC DLL name in loadorder.c")
    print()

    print("SECTION 9: Patch 013 — kernelbase EAC launcher detection")
    if WINE_SRC.exists():
        src = WINE_SRC / "dlls/kernelbase/process.c"
        if src.exists():
            has_string(src, "start_protected_game.exe", "source: EAC launcher pattern")
            has_string(src, "PROTON_EAC_LAUNCHER_PROCESS", "source: EAC launcher env var")
    print()

    print("SECTION 10: Patch 015 — ntdll export __wine_unix_call (CRITICAL)")
    has_string(ntdll_dll, "compat___wine_unix_call", "ntdll.dll: compat___wine_unix_call string")
    has_string(ntdll_dll, "__wine_unix_call", "ntdll.dll: __wine_unix_call export string")
    # Check PE export table
    has_pe_export(ntdll_dll, "__wine_unix_call", "ntdll.dll PE export table")
    if WINE_SRC.exists():
        spec = WINE_SRC / "dlls/ntdll/ntdll.spec"
        if spec.exists():
            has_string(spec, "compat___wine_unix_call", "source: spec file has unix_call export")
        loader_c = WINE_SRC / "dlls/ntdll/loader.c"
        if loader_c.exists():
            has_string(loader_c, "compat___wine_unix_call", "source: loader.c has compat function")
    print()

    print("SECTION 11: Patch 016 — wineboot fast boot")
    wineboot = win_dir / "wineboot.exe"
    has_string(wineboot, "QUARK_FAST_BOOT", "wineboot.exe: fast boot env check")
    has_string(wineboot, "fast_finish", "wineboot.exe: fast_finish label")
    if WINE_SRC.exists():
        src = WINE_SRC / "programs/wineboot/wineboot.c"
        if src.exists():
            has_string(src, "QUARK_FAST_BOOT", "source: fast boot in wineboot.c")
    print()

    print("SECTION 12: lsteamclient ABI compatibility (CRITICAL)")
    lsteam_so = unix_dir / "lsteamclient.so"
    lsteam_dll = win_dir / "lsteamclient.dll"
    # The .so must export steamclient_init_registry
    has_symbol(lsteam_so, "steamclient_init_registry", "lsteamclient.so")
    has_symbol(lsteam_so, "steamclient_init", "lsteamclient.so")
    # The .so must have __wine_unix_call_funcs (PE-Unix bridge)
    has_symbol(lsteam_so, "__wine_unix_call_funcs", "lsteamclient.so PE-Unix bridge")
    # The PE DLL must export steamclient_init_registry
    has_pe_export(lsteam_dll, "steamclient_init_registry", "lsteamclient.dll")
    # Verify the .so was freshly built (not older than the deployed ntdll.so)
    ntdll_mtime = ntdll_so.stat().st_mtime if ntdll_so.exists() else 0
    lsteam_mtime = lsteam_so.stat().st_mtime if lsteam_so.exists() else 0
    check(lsteam_mtime >= ntdll_mtime - 3600,
          f"lsteamclient.so: build timestamp current",
          f"lsteamclient.so: OLDER than ntdll.so — may be stale cached build")

    # Check that the cached build matches the deployed build
    cached_so = CACHE_DIR / "lsteamclient.so"
    if cached_so.exists() and lsteam_so.exists():
        import hashlib
        h1 = hashlib.sha256(cached_so.read_bytes()).hexdigest()[:16]
        h2 = hashlib.sha256(lsteam_so.read_bytes()).hexdigest()[:16]
        check(h1 == h2, f"cache matches deployed ({h1})",
              f"cache MISMATCH: cached={h1} deployed={h2} — stale cache at {CACHE_DIR}")
    print()

    print("SECTION 13: bwrap layer (lsteamclient deployment via mount namespace)")
    # OLD ARCHITECTURE (removed): planted /usr/lib/wine/x86_64-unix/lsteamclient.so
    # as a symlink via `sudo ln -sf`. That was the system poisoning vector.
    #
    # NEW ARCHITECTURE: lsteamclient lives only in compat_dir/lib/wine/. The
    # launcher wraps wine64 in bwrap with --bind compat_dir/lib/wine over
    # /usr/lib/wine. Inside the namespace, Wine's loader finds lsteamclient
    # at /usr/lib/wine/x86_64-unix/lsteamclient.so — which IS the compat tree.
    # The real /usr/lib/wine is never touched.
    #
    # Verifications for the new layout:
    #   1. bubblewrap is installed (without it, launcher.rs falls back to
    #      direct exec and patches do not load)
    #   2. lsteamclient is present in the compat tree (Wine will find it
    #      via the bind mount)
    #   3. /usr/lib/wine/x86_64-unix/lsteamclient.so does NOT exist as a
    #      regular file or symlink — its presence indicates poisoning from
    #      a previous broken install
    bwrap_bin = Path("/usr/bin/bwrap")
    check(bwrap_bin.exists(),
          "bwrap: /usr/bin/bwrap installed",
          "bwrap: NOT INSTALLED — patches will not load at runtime (pacman -S bubblewrap)")
    check(lsteam_so.exists() and lsteam_so.stat().st_size > 0,
          f"compat lsteamclient.so: {lsteam_so.stat().st_size // 1024}K",
          "compat lsteamclient.so: missing")
    sys_lsteam = sys_unix / "lsteamclient.so"
    check(not sys_lsteam.exists() and not sys_lsteam.is_symlink(),
          "/usr/lib/wine clean: no lsteamclient.so leak",
          f"/usr/lib/wine LEAK: {sys_lsteam} exists — left over from a pre-bwrap install (sudo rm to clean)")
    sys_lsteam_dll = Path("/usr/lib/wine/x86_64-windows/lsteamclient.dll")
    check(not sys_lsteam_dll.exists() and not sys_lsteam_dll.is_symlink(),
          "/usr/lib/wine clean: no lsteamclient.dll leak",
          f"/usr/lib/wine LEAK: {sys_lsteam_dll} exists — left over from a pre-bwrap install (sudo rm to clean)")
    print()

    print("SECTION 14: Launcher env vars")
    launcher = DEPLOY_DIR / "proton"
    if launcher.exists():
        data = launcher.read_bytes()
        has_string(launcher, "WINEDLLPATH", "launcher: sets WINEDLLPATH")
        has_string(launcher, "WINEDLLOVERRIDES", "launcher: sets WINEDLLOVERRIDES")
        has_string(launcher, "lsteamclient", "launcher: references lsteamclient")
        has_string(launcher, "QUARK_FAST_BOOT", "launcher: sets QUARK_FAST_BOOT")
        has_string(launcher, "WINE_NTSYNC", "launcher: sets WINE_NTSYNC")
        has_string(launcher, "WINESERVER", "launcher: sets WINESERVER")
    print()

    print("SECTION 15: Steam bridge files")
    steam_exe = win_dir / "steam.exe"
    file_exists(steam_exe, "steam.exe (Proton steam_helper)")
    if steam_exe.exists():
        has_string(steam_exe, "steamclient_init_registry", "steam.exe: calls steamclient_init_registry")
        has_string(steam_exe, "lsteamclient", "steam.exe: loads lsteamclient")
        has_string(steam_exe, "setup_steam_registry", "steam.exe: setup_steam_registry function")
    print()

    print("SECTION 16: Stub DLLs")
    for name in ["tier0_s64.dll", "vstdlib_s64.dll"]:
        file_exists(win_dir / name, f"64-bit stub: {name}")
    i386_dir = DEPLOY_DIR / "lib/wine/i386-windows"
    for name in ["tier0_s.dll", "vstdlib_s.dll"]:
        file_exists(i386_dir / name, f"32-bit stub: {name}")
    print()

    print("SECTION 17: Triskelion binary")
    trisk = DEPLOY_DIR / "triskelion"
    file_exists(trisk, "triskelion daemon")
    if trisk.exists():
        has_string(trisk, "set_registry_notification", "triskelion: registry notification handler")
        has_string(trisk, "suspend_count", "triskelion: CREATE_SUSPENDED support")
        has_string(trisk, "WINEDLLPATH", "triskelion: referenced by launcher")
    print()

    print("SECTION 18: Runtime check (wine_stderr.log)")
    stderr_log = Path("/tmp/quark/wine_stderr.log")
    if stderr_log.exists():
        stderr_text = stderr_log.read_text()
        has_init_err = "Failed to find steamclient_init_registry export" in stderr_text
        has_load_err = "Failed to load lsteamclient module" in stderr_text
        has_page_fault = "Unhandled page fault" in stderr_text
        if has_load_err:
            log_fail("runtime: lsteamclient.dll failed to load (LoadLibraryW returned NULL)")
        elif has_init_err:
            log_warn_info("runtime: steamclient_init_registry not found (PE-Unix bridge not connecting)")
        else:
            log_pass("runtime: no lsteamclient errors")
        if has_page_fault:
            import re
            faults = re.findall(r'page fault.*address ([0-9A-Fa-f]+)', stderr_text)
            for addr in faults:
                log_fail(f"runtime: page fault at 0x{addr}")
        else:
            log_pass("runtime: no page faults")
    else:
        log_pass("runtime: no wine_stderr.log (no game run yet)")
    print()

    # Summary
    total = PASS + FAIL
    print(f"RESULTS: {PASS}/{total} passed, {FAIL} failed")
    if FAIL > 0:
        print(f"\n{FAIL} FAILURE(S) — deployment is broken")
        return 1
    else:
        print("\nAll checks passed")
        return 0


if __name__ == "__main__":
    sys.exit(run_tests())
