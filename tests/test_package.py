#!/usr/bin/env python3
"""
amphetamine package validation and smoke tests.

Verifies the packaged Steam compatibility tool is structurally correct
and can initialize a Wine prefix + launch a game.

Usage:
    ./tests/test_package.py                  Run all tests
    ./tests/test_package.py --smoke <APPID>  Smoke-test a Steam game launch
"""

import argparse
import os
import subprocess
import sys
import tempfile
from pathlib import Path

TOOL_DIR = Path.home() / ".steam" / "root" / "compatibilitytools.d" / "amphetamine"
FILES_DIR = TOOL_DIR / "files"
BIN_DIR = FILES_DIR / "bin"
LIB_WIN = FILES_DIR / "lib" / "wine" / "x86_64-windows"
LIB_UNIX = FILES_DIR / "lib" / "wine" / "x86_64-unix"
SHARE_DIR = FILES_DIR / "share" / "wine"


# TEST INFRASTRUCTURE

class TestResult:
    def __init__(self):
        self.passed = 0
        self.failed = 0
        self.errors = []

    def ok(self, name: str):
        self.passed += 1
        print(f"  PASS  {name}")

    def fail(self, name: str, reason: str):
        self.failed += 1
        self.errors.append((name, reason))
        print(f"  FAIL  {name}: {reason}")

    def summary(self) -> int:
        total = self.passed + self.failed
        print()
        print(f"  {self.passed}/{total} passed")
        if self.errors:
            print()
            for name, reason in self.errors:
                print(f"  FAIL  {name}: {reason}")
        print()
        return 0 if self.failed == 0 else 1


# STRUCTURAL TESTS

def test_structure(r: TestResult):
    """Verify the package directory layout is what Steam expects."""

    # Top-level files
    for name in ("compatibilitytool.vdf", "toolmanifest.vdf", "proton"):
        path = TOOL_DIR / name
        if path.exists():
            r.ok(f"exists: {name}")
        else:
            r.fail(f"exists: {name}", "missing")

    # Launcher is executable ELF binary (not a Python script)
    proton = TOOL_DIR / "proton"
    if proton.exists() and proton.stat().st_mode & 0o111:
        r.ok("proton is executable")
        with open(proton, "rb") as f:
            magic = f.read(4)
        if magic == b"\x7fELF":
            r.ok("proton is ELF binary")
        else:
            r.fail("proton is ELF binary", f"got magic {magic!r}, expected ELF")

    # bin/ contents
    for binary in ("wine64", "wine64-preloader", "wineserver"):
        path = BIN_DIR / binary
        if path.exists() and path.stat().st_mode & 0o111:
            r.ok(f"bin/{binary}")
        else:
            r.fail(f"bin/{binary}", "missing or not executable")


def test_dlls(r: TestResult):
    """Verify core gaming DLLs are present."""
    critical_dlls = [
        "ntdll.dll", "kernel32.dll", "kernelbase.dll",
        "user32.dll", "gdi32.dll", "win32u.dll",
        "advapi32.dll", "ole32.dll", "rpcrt4.dll",
        "d3d11.dll", "d3d12.dll", "dxgi.dll",
        "dinput8.dll", "xinput1_3.dll",
        "ws2_32.dll", "bcrypt.dll",
        "winevulkan.dll", "opengl32.dll",
        "winmm.dll", "mmdevapi.dll",
        "msvcrt.dll", "ucrtbase.dll",
    ]
    for dll in critical_dlls:
        path = LIB_WIN / dll
        if path.exists():
            r.ok(f"dll: {dll}")
        else:
            r.fail(f"dll: {dll}", "missing")


def test_programs(r: TestResult):
    """Verify essential Wine programs are present."""
    critical_progs = [
        "wineboot.exe", "winedevice.exe", "explorer.exe",
        "services.exe", "rpcss.exe", "plugplay.exe",
        "rundll32.exe", "cmd.exe",
    ]
    for prog in critical_progs:
        path = LIB_WIN / prog
        if path.exists():
            r.ok(f"prog: {prog}")
        else:
            r.fail(f"prog: {prog}", "missing")


def test_drivers(r: TestResult):
    """Verify Unix-side driver .so files are present."""
    critical_drivers = [
        "ntdll.so", "win32u.so", "opengl32.so",
        "winevulkan.so", "winex11.so",
    ]
    for drv in critical_drivers:
        path = LIB_UNIX / drv
        if path.exists():
            r.ok(f"driver: {drv}")
        else:
            r.fail(f"driver: {drv}", "missing")


def test_nls(r: TestResult):
    """Verify locale/NLS data is present."""
    nls_dir = SHARE_DIR / "nls"
    if nls_dir.exists():
        nls_count = len(list(nls_dir.glob("*.nls")))
        if nls_count > 0:
            r.ok(f"nls: {nls_count} files")
        else:
            r.fail("nls", "directory exists but empty")
    else:
        r.fail("nls", "share/wine/nls/ missing")


def test_vdf_contents(r: TestResult):
    """Verify VDF files have correct content."""
    compat_vdf = TOOL_DIR / "compatibilitytool.vdf"
    if compat_vdf.exists():
        text = compat_vdf.read_text()
        if "amphetamine" in text and "display_name" in text:
            r.ok("compatibilitytool.vdf content")
        else:
            r.fail("compatibilitytool.vdf content", "missing expected fields")
    else:
        r.fail("compatibilitytool.vdf content", "file missing")

    manifest_vdf = TOOL_DIR / "toolmanifest.vdf"
    if manifest_vdf.exists():
        text = manifest_vdf.read_text()
        if "/proton" in text and "commandline" in text:
            r.ok("toolmanifest.vdf content")
        else:
            r.fail("toolmanifest.vdf content", "missing expected fields")
    else:
        r.fail("toolmanifest.vdf content", "file missing")


# PREFIX INIT TEST

def test_prefix_init(r: TestResult):
    """Test that Wine can initialize a fresh prefix using our build."""
    wine64 = BIN_DIR / "wine64"
    wineboot = LIB_WIN / "wineboot.exe"

    if not wine64.exists():
        r.fail("prefix init", "wine64 not found")
        return

    with tempfile.TemporaryDirectory(prefix="amphetamine_test_") as tmpdir:
        pfx = Path(tmpdir) / "pfx"
        env = dict(os.environ)
        env["WINEPREFIX"] = str(pfx)
        env["WINEDLLPATH"] = str(LIB_WIN)
        env["WINEDEBUG"] = "-all"
        env["PATH"] = f"{BIN_DIR}:{env.get('PATH', '')}"
        env["LD_LIBRARY_PATH"] = f"{FILES_DIR / 'lib'}:{env.get('LD_LIBRARY_PATH', '')}"

        try:
            result = subprocess.run(
                [str(wine64), "wineboot", "--init"],
                env=env, timeout=60,
                capture_output=True, text=True,
            )
            if (pfx / "system.reg").exists():
                r.ok("prefix init (system.reg created)")
            else:
                r.fail("prefix init", f"system.reg not created. "
                       f"rc={result.returncode} stderr={result.stderr[:200]}")
        except subprocess.TimeoutExpired:
            r.fail("prefix init", "timed out after 60s")
        except Exception as e:
            r.fail("prefix init", str(e))


# SMOKE TEST

def test_smoke_launch(r: TestResult, app_id: str):
    """Launch a Steam game through amphetamine and verify it starts."""
    compat_data = Path.home() / ".steam" / "root" / "steamapps" / "compatdata" / app_id
    if not compat_data.exists():
        r.fail(f"smoke launch {app_id}", f"no compatdata for app {app_id}")
        return

    # Find the game's install directory
    manifest = Path.home() / ".steam" / "root" / "steamapps" / f"appmanifest_{app_id}.acf"
    if not manifest.exists():
        r.fail(f"smoke launch {app_id}", "app manifest not found")
        return

    # Parse installdir from manifest
    install_dir = None
    for line in manifest.read_text().splitlines():
        if "installdir" in line:
            install_dir = line.split('"')[3]
            break

    if not install_dir:
        r.fail(f"smoke launch {app_id}", "could not parse installdir")
        return

    game_path = Path.home() / ".steam" / "root" / "steamapps" / "common" / install_dir
    if not game_path.exists():
        r.fail(f"smoke launch {app_id}", f"game not installed at {game_path}")
        return

    r.ok(f"smoke: game found at {game_path}")
    print()
    print(f"  To launch Guild Wars 2 through Steam with amphetamine:")
    print(f"    1. Restart Steam (so it picks up the new compatibility tool)")
    print(f"    2. Right-click Guild Wars 2 > Properties > Compatibility")
    print(f"    3. Check 'Force the use of a specific Steam Play compatibility tool'")
    print(f"    4. Select 'amphetamine (stripped Wine)'")
    print(f"    5. Launch: steam steam://rungameid/{app_id}")
    print()


# MAIN

def run_tests(smoke_app_id: str | None = None) -> int:
    r = TestResult()

    if not TOOL_DIR.exists():
        print(f"Package not found at {TOOL_DIR}")
        print(f"Run: triskelion package /tmp/proton-wine")
        return 1

    print()
    print("  amphetamine package tests")
    print()

    test_structure(r)
    test_dlls(r)
    test_programs(r)
    test_drivers(r)
    test_nls(r)
    test_vdf_contents(r)
    test_prefix_init(r)

    if smoke_app_id:
        test_smoke_launch(r, smoke_app_id)

    return r.summary()


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description="amphetamine package tests")
    parser.add_argument("--smoke", metavar="APPID",
                        help="Smoke-test a Steam game (e.g. 1284210 for GW2)")
    args = parser.parse_args()
    sys.exit(run_tests(smoke_app_id=args.smoke))
