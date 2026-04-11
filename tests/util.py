"""Shared test infrastructure for quark/triskelion.

Every test imports from here instead of rolling its own kill/launch logic.
The kill functions are scoped to quark's compat tool directory — they
NEVER match bare 'wine' or 'wineserver', which would hit Proton's running
processes and corrupt prefixes.
"""

import json
import os
import shutil
import signal
import subprocess
import tempfile
import time
from pathlib import Path

# ── Paths ──

REPO_ROOT = Path(__file__).resolve().parent.parent
TESTS_DIR = REPO_ROOT / "tests"
REFERENCE_DIR = TESTS_DIR / "reference"

_USER_HOME = Path(f"/home/{os.environ.get('SUDO_USER', os.environ.get('USER', 'mod'))}")
STEAM_ROOT = _USER_HOME / ".local/share/Steam"
COMPAT_DIR = STEAM_ROOT / "compatibilitytools.d/quark"
COMPAT_DIR_STR = str(COMPAT_DIR)

PROTON_DIR = STEAM_ROOT / "steamapps/common/Proton 10.0"
PROTON_BIN = PROTON_DIR / "proton"


# ── Process Management ──

def kill_quark_processes(graceful_timeout=0.3):
    """Kill ONLY processes from quark's compat tool directory.

    1. SIGTERM triskelion (lets Drop handlers fire, writes stats)
    2. Wait graceful_timeout for clean shutdown
    3. SIGKILL processes matching quark directory path
    4. SIGKILL stragglers found via pgrep

    NEVER matches bare 'wine' or 'wineserver' — those patterns hit Proton's
    running wineserver for other games. SIGKILL mid-operation leaves Proton
    prefixes in an inconsistent state (partial registry writes).
    """
    subprocess.run(["pkill", "-TERM", "-f", "triskelion"],
                   capture_output=True, timeout=5)
    time.sleep(graceful_timeout)

    for pattern in [f"{COMPAT_DIR_STR}/proton",
                    f"{COMPAT_DIR_STR}/lib/wine",
                    f"{COMPAT_DIR_STR}/bin/wine",
                    "triskelion"]:
        subprocess.run(["pkill", "-9", "-f", pattern],
                       capture_output=True, timeout=5)
    time.sleep(1)

    # Straggler sweep scoped to our directory
    try:
        result = subprocess.run(["pgrep", "-a", "-f", COMPAT_DIR_STR],
                                capture_output=True, text=True, timeout=5)
        if result.returncode == 0 and result.stdout.strip():
            for line in result.stdout.strip().splitlines():
                try:
                    pid = int(line.split()[0])
                    os.kill(pid, signal.SIGKILL)
                except (ValueError, ProcessLookupError, PermissionError):
                    pass
    except subprocess.TimeoutExpired:
        pass
    time.sleep(0.5)


def kill_process_group(proc, graceful_timeout=1.0):
    """Kill entire process tree spawned by proc.

    Requires proc was started with start_new_session=True (or via
    launch_with_process_group). Sends SIGTERM to process group first
    (lets wineserver flush), waits, then SIGKILL.
    """
    try:
        pgid = os.getpgid(proc.pid)
        os.killpg(pgid, signal.SIGTERM)
        time.sleep(graceful_timeout)
        os.killpg(pgid, signal.SIGKILL)
    except (ProcessLookupError, PermissionError, OSError):
        pass
    if proc.poll() is None:
        proc.kill()
    proc.wait()


def launch_with_process_group(cmd, env, cwd="/tmp", **kwargs):
    """Launch a subprocess in its own process group.

    All children (wineserver, wine-preloader, .exe processes) inherit the
    group. kill_process_group() catches them all — even orphaned services.exe
    that survive individual pkill patterns.
    """
    return subprocess.Popen(
        cmd, env=env, cwd=cwd,
        start_new_session=True,
        **kwargs,
    )


# ── Environment ──

def make_game_env(appid="2379780", winedebug="-all", extra=None):
    """Build the standard env dict for launching a game through quark.

    Includes display vars (DISPLAY, WAYLAND_DISPLAY, XDG_RUNTIME_DIR),
    Steam vars (STEAM_COMPAT_DATA_PATH, SteamAppId), and Wine config.
    """
    compat_data = STEAM_ROOT / "steamapps/compatdata" / str(appid)

    env = os.environ.copy()
    env["STEAM_COMPAT_DATA_PATH"] = str(compat_data)
    env["STEAM_COMPAT_CLIENT_INSTALL_PATH"] = str(STEAM_ROOT)
    env["SteamAppId"] = str(appid)
    env["SteamGameId"] = str(appid)
    env["WINEDEBUG"] = winedebug
    env["HOME"] = str(_USER_HOME)

    if extra:
        env.update(extra)
    return env


# Helper executables shipped alongside games — never the actual game.
# Mirrors EXE_BLACKLIST in iterate.py. Without filtering, get_game_exe
# can pick UnityCrashHandler64.exe for Unity games or
# DukeWorkshopUploader.exe for Duke Nukem, then every downstream test
# launches the wrong .exe and the game never even tries to run.
GAME_EXE_BLACKLIST = {
    "unitycrashhandler64.exe", "unitycrashhandler32.exe",
    "crashreport.exe", "crashhandler.exe", "crashpad_handler.exe",
    "ue4prereqsetup_x64.exe", "ue4prereqsetup_x86.exe",
    "installermessage.exe",
    "dxsetup.exe", "vcredist_x64.exe", "vcredist_x86.exe",
    "dukeworkshopuploader.exe",
    "dotnetfx35.exe", "dotnetfx35setup.exe",
    "beservice.exe", "beservice_x64.exe",
    "easyanticheat_setup.exe", "easyanticheat.exe",
    "uninstall.exe", "unins000.exe",
}


def get_game_exe(appid="2379780"):
    """Find the actual game executable for a Steam appid.

    Picks the largest .exe in the install directory (up to 3 levels deep)
    that is NOT a known helper / installer / crash reporter. Returns None
    if no valid candidate exists.
    """
    steamapps = STEAM_ROOT / "steamapps"
    manifest = steamapps / f"appmanifest_{appid}.acf"
    if not manifest.exists():
        return None

    import re
    text = manifest.read_text(errors="replace")
    m = re.search(r'"installdir"\s+"([^"]+)"', text)
    if not m:
        return None

    install_dir = steamapps / "common" / m.group(1)
    if not install_dir.exists():
        return None

    candidates = []
    for exe in install_dir.rglob("*.exe"):
        # Skip depth > 3 (avoid bundled vendor tools deep in subdirs)
        try:
            depth = len(exe.relative_to(install_dir).parts)
            if depth > 3:
                continue
        except ValueError:
            continue
        # Skip known-helper exes
        if exe.name.lower() in GAME_EXE_BLACKLIST:
            continue
        candidates.append(exe)

    if not candidates:
        return None
    return max(candidates, key=lambda p: p.stat().st_size)


# ── Prefix Management ──

def make_temp_prefix():
    """Create an isolated temporary prefix.

    Returns (prefix_path, cleanup_fn). Call cleanup_fn() when done.
    For stock captures and tests that must not touch the real Steam prefix.
    """
    tmpdir = tempfile.mkdtemp(prefix="quark_test_")
    prefix = Path(tmpdir) / "pfx"
    prefix.mkdir(parents=True, exist_ok=True)

    def cleanup():
        shutil.rmtree(tmpdir, ignore_errors=True)

    return prefix, cleanup


def sudo_as_user(cmd):
    """Wrap a command to run as the real user when under sudo.

    Returns the command prefixed with sudo -u $SUDO_USER if running as root.
    """
    real_user = os.environ.get("SUDO_USER", "")
    if real_user and os.geteuid() == 0:
        preserve = "STEAM_COMPAT_DATA_PATH,STEAM_COMPAT_CLIENT_INSTALL_PATH"
        preserve += ",SteamAppId,SteamGameId,WINEDEBUG,HOME,WINEPREFIX"
        preserve += ",DISPLAY,WAYLAND_DISPLAY,XDG_RUNTIME_DIR,XAUTHORITY"
        return ["sudo", "-u", real_user, f"--preserve-env={preserve}"] + cmd
    return cmd


# ── Reference Artifacts ──

def reference_exists(layer):
    """Check if a reference layer has been captured."""
    layer_dir = REFERENCE_DIR / layer
    return layer_dir.exists() and any(layer_dir.iterdir())


def load_reference(layer, filename):
    """Load a reference artifact file. Returns text content or None."""
    path = REFERENCE_DIR / layer / filename
    if path.exists():
        return path.read_text(errors="replace")
    return None


def reference_metadata():
    """Load reference metadata. Returns dict or None."""
    meta_path = REFERENCE_DIR / "metadata.json"
    if meta_path.exists():
        return json.loads(meta_path.read_text())
    return None
