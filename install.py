#!/usr/bin/env python3
"""Build and deploy amphetamine: triskelion binary, DXVK/VKD3D, optional ntsync ntdll."""

import filecmp
import json
import os
import re
import shutil
import subprocess
import sys
import tarfile
import urllib.request
from datetime import datetime
from pathlib import Path

SCRIPT_DIR = Path(__file__).parent.resolve()
RUST_DIR = SCRIPT_DIR / "amphetamine"
PATCHES_DIR = SCRIPT_DIR / "patches" / "wine"

DATA_DIR = Path.home() / ".local" / "share" / "amphetamine"
WINE_SRC_DIR = DATA_DIR / "wine-src"
WINE_OBJ_DIR = DATA_DIR / "wine-obj"
STEAM_COMPAT_DIR = Path.home() / ".local" / "share" / "Steam" / "compatibilitytools.d" / "amphetamine"
WINE_CLONE_URL = "https://gitlab.winehq.org/wine/wine.git"

# Essential build deps for Wine on Arch-based systems.
# WoW64 mode (--enable-archs=x86_64,i386) uses mingw for 32-bit PE DLLs,
# so lib32 system packages are NOT required.
WINE_BUILD_DEPS_ARCH = [
    # Build tools
    "base-devel", "mingw-w64-gcc", "autoconf", "bison", "flex", "perl",
    # Graphics / display
    "freetype2", "fontconfig", "vulkan-headers", "vulkan-icd-loader",
    "libx11", "libxext", "libxrandr", "libxinerama", "libxcursor",
    "libxcomposite", "libxi", "libxxf86vm",
    "wayland", "wayland-protocols",
    # Audio
    "alsa-lib", "libpulse",
    "gst-plugins-base-libs",
    # Networking / crypto
    "gnutls",
    # Input
    "sdl2",
    # Other
    "libusb", "v4l-utils",
]


def get_version():
    """Read version from amphetamine/Cargo.toml."""
    cargo_toml = RUST_DIR / "Cargo.toml"
    for line in cargo_toml.read_text().splitlines():
        if line.startswith("version"):
            return line.split('"')[1]
    return "0.0.0"

# Patch text: triskelion_has_posted inline function for win32u/message.c
WIN32U_FUNCTION = """\

/* triskelion: check if the shm ring has pending posted messages.
 * queue_ptr is from TEB->glReserved2, set by ntdll triskelion_claim_slot.
 * The ring's write_pos (offset 0) and read_pos (offset 64) are cacheline-aligned uint64_t. */
static inline BOOL triskelion_has_posted( volatile void *queue_ptr )
{
    volatile ULONGLONG *wp, *rp;
    if (!queue_ptr) return FALSE;
    wp = (volatile ULONGLONG *)queue_ptr;
    rp = (volatile ULONGLONG *)((char *)queue_ptr + 64);
    return *wp > *rp;
}
"""

# Patch text: server.c bypass block (inserted before send_request)
SERVER_BYPASS = """\
    /* triskelion: shared memory bypass for hot-path messages */
    ret = triskelion_try_bypass( req_ptr );
    if (ret != STATUS_NOT_IMPLEMENTED)
        return ret;

"""

# Patch text: win32u peek_message condition prefix
PEEK_MSG_GUARD = """\
        /* triskelion: if the shm ring has pending posted messages,
         * skip check_queue_bits and force the server call path.
         * The bypass in ntdll server_call_unlocked will pop from the ring. */
        if (triskelion_has_posted(NtCurrentTeb()->glReserved2))
            ;  /* fall through to server call */
        else """


ENV_CONFIG_TEMPLATE = """\
# amphetamine custom environment variables
#
# Format: KEY=VALUE (one per line)
# Lines starting with # are comments. Blank lines are ignored.
# Variables set here override amphetamine's built-in defaults.
# Edit this file any time — changes apply on next game launch.
#
# --- Logging ---
# WINEDEBUG=-all
# DXVK_LOG_LEVEL=none
# DXVK_NVAPI_LOG_LEVEL=none
# VKD3D_DEBUG=none
# VKD3D_SHADER_DEBUG=none
# PROTON_LOG=0
#
# --- Wayland ---
# WINE_ENABLE_WAYLAND=1
#
# --- Sync ---
# WINE_NTSYNC=1
# PROTON_NO_FSYNC=1
# WINEFSYNC_SPINCOUNT=100
#
# --- Overlays ---
# MANGOHUD=1
# MANGOHUD_CONFIG=fps,frametime,gpu_temp,cpu_temp
# DXVK_HUD=fps
#
# --- Frame rate ---
# DXVK_FRAME_RATE=0
#
# --- Upscaling ---
# WINE_FULLSCREEN_FSR=1
# WINE_FULLSCREEN_FSR_STRENGTH=2
#
# --- Performance ---
# DXVK_ASYNC=1
# mesa_glthread=true
# RADV_PERFTEST=gpl
# STAGING_SHARED_MEMORY=1
# __GL_THREADED_OPTIMIZATIONS=1
#
# --- CPU topology ---
# WINE_CPU_TOPOLOGY=8:0,1,2,3,4,5,6,7
#
# --- NVIDIA (DLSS, Reflex, NVAPI) ---
# PROTON_ENABLE_NVAPI=1
# DXVK_ENABLE_NVAPI=dxgi
# PROTON_HIDE_NVIDIA_GPU=0
#
# --- Gamescope ---
# ENABLE_GAMESCOPE_WSI=1
"""


def log(level, msg):
    ts = datetime.now().strftime("%H:%M:%S")
    print(f"[{ts}] [{level}]   {msg}", file=sys.stderr)


def prompt_yn(question):
    """Prompt the user with a [Y/N] question. Returns True for yes, False for no."""
    while True:
        answer = input(f"{question} [Y/N] ").strip().lower()
        if answer == "y":
            return True
        if answer == "n":
            return False


def get_latest_wine_tag():
    """Query upstream Wine for the latest stable release tag (e.g. wine-11.4)."""
    out = subprocess.run(
        ["git", "ls-remote", "--tags", WINE_CLONE_URL],
        capture_output=True, text=True,
    )
    if out.returncode != 0:
        log("ERROR", "Failed to query Wine release tags")
        return None

    # Match stable tags: wine-X.Y (no -rc, no -dev)
    tag_re = re.compile(r"refs/tags/(wine-(\d+)\.(\d+))$")
    tags = []
    for line in out.stdout.splitlines():
        m = tag_re.search(line)
        if m:
            tags.append((int(m.group(2)), int(m.group(3)), m.group(1)))

    if not tags:
        log("ERROR", "No stable Wine tags found")
        return None

    tags.sort(reverse=True)
    return tags[0][2]


def build_triskelion():
    log("INFO", "Building triskelion...")
    ret = subprocess.run(["cargo", "build", "--release", "-p", "triskelion"], cwd=SCRIPT_DIR).returncode
    if ret != 0:
        log("ERROR", "Build failed (cargo error)")
        return ret

    # Workspace builds go to <repo>/target/, not <crate>/target/
    binary = SCRIPT_DIR / "target" / "release" / "triskelion"
    if not binary.exists():
        log("ERROR", f"Binary not found: {binary}")
        return 1

    dest = SCRIPT_DIR / "triskelion"
    shutil.copy2(binary, dest)
    os.chmod(dest, 0o755)
    log("INFO", f"Installed: {dest}")

    # Deploy to Steam compatibility tools directory
    STEAM_COMPAT_DIR.mkdir(parents=True, exist_ok=True)
    proton_dest = STEAM_COMPAT_DIR / "proton"
    shutil.copy2(binary, proton_dest)
    os.chmod(proton_dest, 0o755)
    log("INFO", f"Deployed to Steam: {proton_dest}")

    # Write VDF with current version
    version = get_version()
    vdf = STEAM_COMPAT_DIR / "compatibilitytool.vdf"
    vdf.write_text(f'''"compatibilitytools"
{{
  "compat_tools"
  {{
    "amphetamine"
    {{
      "install_path" "."
      "display_name" "amphetamine {version}"
      "from_oslist"  "windows"
      "to_oslist"    "linux"
    }}
  }}
}}
''')
    log("INFO", f"Updated VDF: amphetamine {version}")

    # Write toolmanifest.vdf (required by Steam's compatmanager)
    manifest = STEAM_COMPAT_DIR / "toolmanifest.vdf"
    manifest.write_text('''"manifest"
{
  "commandline" "/proton %verb%"
  "version" "2"
  "use_sessions" "1"
}
''')
    log("INFO", "Updated toolmanifest.vdf")

    return 0


def clone_wine():
    if (WINE_SRC_DIR / "dlls").exists():
        log("INFO", "Wine source: already cloned")
        return

    tag = get_latest_wine_tag()
    if not tag:
        log("ERROR", "Cannot determine latest Wine release")
        return

    log("INFO", f"Cloning upstream Wine ({tag})...")
    WINE_SRC_DIR.parent.mkdir(parents=True, exist_ok=True)
    ret = subprocess.run([
        "git", "clone", "--depth", "1", "-b", tag,
        WINE_CLONE_URL, str(WINE_SRC_DIR),
    ]).returncode
    if ret != 0:
        log("ERROR", "Clone failed — GitLab may be down, retry later")
        return
    log("INFO", f"Clone complete: {WINE_SRC_DIR}")


def patch_copy_triskelion_c():
    src = PATCHES_DIR / "dlls" / "ntdll" / "unix" / "triskelion.c"
    dst = WINE_SRC_DIR / "dlls" / "ntdll" / "unix" / "triskelion.c"
    if dst.exists() and filecmp.cmp(src, dst, shallow=False):
        log("INFO", "triskelion.c: already patched")
        return
    shutil.copy2(src, dst)
    log("INFO", "Patched triskelion.c")


def patch_makefile_in():
    path = WINE_SRC_DIR / "dlls" / "ntdll" / "Makefile.in"
    text = path.read_text()
    if "unix/triskelion.c" in text:
        log("INFO", "Makefile.in: already patched")
        return
    anchor = "\tunix/thread.c \\"
    if anchor not in text:
        log("ERROR", f"Anchor not found in {path}: {anchor!r}")
        sys.exit(1)
    text = text.replace(anchor, anchor + "\n\tunix/triskelion.c \\")
    path.write_text(text)
    log("INFO", "Patched Makefile.in: added unix/triskelion.c")


def patch_server_c():
    path = WINE_SRC_DIR / "dlls" / "ntdll" / "unix" / "server.c"
    text = path.read_text()
    patched = False

    # Pre-hook: triskelion_try_bypass before send_request
    if "triskelion_try_bypass" not in text:
        anchor = "    if ((ret = send_request( req ))) return ret;\n    return wait_reply( req );"
        if anchor not in text:
            log("ERROR", f"Anchor not found in {path}: send_request/wait_reply block")
            sys.exit(1)
        text = text.replace(anchor,
            SERVER_BYPASS + anchor)
        patched = True

    # Post-hook: triskelion_post_call after wait_reply (ntsync shadow creation)
    if "triskelion_post_call" not in text:
        post_anchor = "    return wait_reply( req );"
        if post_anchor not in text:
            log("ERROR", f"Anchor not found in {path}: return wait_reply")
            sys.exit(1)
        text = text.replace(post_anchor,
            "    ret = wait_reply( req );\n"
            "    /* triskelion: shadow newly created sync objects with ntsync */\n"
            "    triskelion_post_call( req_ptr, ret );\n"
            "    return ret;")
        patched = True

    if patched:
        path.write_text(text)
        log("INFO", "Patched server.c: triskelion_try_bypass + triskelion_post_call")
    else:
        log("INFO", "server.c: already patched")


def patch_unix_private_h():
    path = WINE_SRC_DIR / "dlls" / "ntdll" / "unix" / "unix_private.h"
    text = path.read_text()
    patched = False

    if "triskelion_try_bypass" not in text:
        anchor = "extern unsigned int server_call_unlocked( void *req_ptr );"
        if anchor not in text:
            log("ERROR", f"Anchor not found in {path}: {anchor!r}")
            sys.exit(1)
        text = text.replace(anchor, anchor +
            "\nextern unsigned int triskelion_try_bypass( void *req_ptr );")
        patched = True

    if "triskelion_post_call" not in text:
        anchor2 = "extern unsigned int triskelion_try_bypass( void *req_ptr );"
        text = text.replace(anchor2, anchor2 +
            "\nextern void triskelion_post_call( void *req_ptr, unsigned int ret );")
        patched = True

    if patched:
        path.write_text(text)
        log("INFO", "Patched unix_private.h: triskelion declarations")
    else:
        log("INFO", "unix_private.h: already patched")


def patch_win32u_message():
    path = WINE_SRC_DIR / "dlls" / "win32u" / "message.c"
    text = path.read_text()
    if "triskelion_has_posted" in text:
        log("INFO", "win32u/message.c: already patched")
        return

    # Modification A: insert triskelion_has_posted function after debug channel declarations
    func_anchor = "WINE_DECLARE_DEBUG_CHANNEL(relay);"
    if func_anchor not in text:
        log("ERROR", f"Anchor not found in {path}: {func_anchor!r}")
        sys.exit(1)
    text = text.replace(func_anchor, func_anchor + WIN32U_FUNCTION)

    # Modification B: prepend triskelion check to check_queue_bits condition
    original_condition = "if (check_queue_bits( wake_mask, filter->mask, wake_mask | signal_bits, filter->mask | clear_bits,"
    if original_condition not in text:
        log("ERROR", f"Anchor not found in {path}: check_queue_bits condition")
        sys.exit(1)

    text = text.replace(
        "        " + original_condition,
        PEEK_MSG_GUARD + original_condition,
    )
    path.write_text(text)
    log("INFO", "Patched win32u/message.c: triskelion_has_posted + peek_message bypass")


def configure_shader_cache():
    """Ask the user whether to enable per-game Vulkan shader cache optimization."""
    flag = STEAM_COMPAT_DIR / "shader_cache_enabled"

    if flag.exists():
        log("INFO", "Shader cache optimization: enabled (use install.py to reconfigure)")
        return

    print()
    print("  Shader cache optimization: amphetamine can configure per-game Vulkan")
    print("  shader caches (organized per-prefix, 10 GB cap — actual usage is")
    print("  typically 50-500 MB per game). This prevents compiled shaders from")
    print("  being evicted between sessions and reduces stutter.")
    print()
    print("  If you already manage shader cache settings yourself, say no.")
    print()

    if prompt_yn("  Enable shader cache optimization?"):
        flag.write_text("1")
        log("INFO", "Shader cache: enabled")
    else:
        if flag.exists():
            flag.unlink()
        log("INFO", "Shader cache: disabled")


def configure_custom_env():
    """Ask the user whether to create a custom environment variable config."""
    config_file = STEAM_COMPAT_DIR / "env_config"

    if config_file.exists():
        log("INFO", f"Custom env config: {config_file}")
        log("INFO", "  Edit it directly — changes apply on next game launch")
        return

    print()
    print("  Custom environment variables: amphetamine can create a config file")
    print("  where you define extra environment variables applied at game launch.")
    print("  Variables you set override amphetamine's built-in defaults.")
    print()
    print(f"  Config location: {config_file}")
    print()

    if prompt_yn("  Create custom environment config?"):
        config_file.write_text(ENV_CONFIG_TEMPLATE)
        log("INFO", f"Custom env config created: {config_file}")
        log("INFO", "  Uncomment variables to enable them")
    else:
        log("INFO", "Custom env config: skipped")


def check_ntsync():
    """Check if the kernel supports ntsync."""
    if Path("/dev/ntsync").exists():
        log("INFO", "ntsync: /dev/ntsync available — kernel-native NT sync enabled")
        log("INFO", "  triskelion.c patches handle ntsync via ioctls (no external deps)")
    else:
        log("INFO", "ntsync: not available (using fsync fallback)")
        log("INFO", "  ntsync requires Linux 6.14+. Sync works fine without it.")


def install_wine_build_deps():
    """Install Wine build dependencies via pacman (Arch-based only)."""
    try:
        subprocess.run(["pacman", "--version"], capture_output=True)
    except FileNotFoundError:
        log("ERROR", "pacman: not found — Wine build requires Arch-based system")
        log("ERROR", "  Install deps manually, then re-run install.py")
        return False

    missing = []
    for pkg in WINE_BUILD_DEPS_ARCH:
        ret = subprocess.run(["pacman", "-Q", pkg], capture_output=True, text=True)
        if ret.returncode != 0:
            missing.append(pkg)

    if not missing:
        log("INFO", "Wine build deps: all installed")
        return True

    log("INFO", f"Wine build deps: {len(missing)} packages needed")
    for pkg in missing:
        print(f"    {pkg}")

    if not prompt_yn(f"\n  Install {len(missing)} packages via pacman?"):
        log("WARN", "Wine build cancelled — missing dependencies")
        return False

    ret = subprocess.run(
        ["sudo", "pacman", "-S", "--needed", "--noconfirm"] + missing,
    ).returncode
    if ret != 0:
        log("ERROR", "Wine build deps: install failed")
        return False

    log("INFO", "Wine build deps: installed")
    return True


def build_wine():
    """Compile ntdll.so directly with gcc from Wine source — no Wine build system.
    We patched triskelion.c and server.c. This compiles all ntdll unix sources
    (20 files) into a single .so and deploys it to Steam compat dir."""
    if not (WINE_SRC_DIR / "dlls").exists():
        log("ERROR", "Wine source: not found")
        return False

    ntdll_dest = STEAM_COMPAT_DIR / "lib" / "ntdll.so"
    if ntdll_dest.exists():
        log("INFO", "ntdll.so: already deployed")
        if not prompt_yn("  Rebuild?"):
            return True

    # One-time configure to generate config.h (the only thing we need)
    config_h = WINE_OBJ_DIR / "include" / "config.h"
    if not config_h.exists():
        log("INFO", "Running configure (one-time, for config.h)...")
        WINE_OBJ_DIR.mkdir(parents=True, exist_ok=True)
        configure = WINE_SRC_DIR / "configure"
        if not configure.exists():
            ret = subprocess.run(["autoreconf", "-fi"], cwd=WINE_SRC_DIR).returncode
            if ret != 0:
                log("ERROR", "autoreconf: failed")
                return False
        ret = subprocess.run([
            str(configure),
            "--enable-archs=x86_64,i386",
            "--with-wayland", "--with-vulkan",
            "--disable-tests",
        ], cwd=WINE_OBJ_DIR).returncode
        if ret != 0:
            log("ERROR", "Configure failed")
            return False

    # Compile all ntdll unix sources (skip non-x86_64 signal handlers)
    skip = {"signal_arm.c", "signal_arm64.c", "signal_i386.c"}
    unix_dir = WINE_SRC_DIR / "dlls" / "ntdll" / "unix"
    sources = sorted(f for f in unix_dir.glob("*.c") if f.name not in skip)

    gcc_base = [
        "gcc", "-c", "-fPIC", "-O2", "-pipe",
        f"-I{WINE_OBJ_DIR}/include",
        f"-I{WINE_SRC_DIR}/include",
        f"-I{WINE_SRC_DIR}/dlls/ntdll",
        "-D__WINESRC__", "-DHAVE_CONFIG_H", "-DWINE_UNIX_LIB",
        "-D_NTSYSTEM_", "-DLTC_NO_PROTOTYPES", "-DLTC_SOURCE",
        "-D_ACRTIMP=", "-DWINBASEAPI=",
        '-DBINDIR="/usr/bin"', '-DLIBDIR="/usr/lib"',
        '-DDATADIR="/usr/share"', '-DSYSTEMDLLPATH="/usr/lib/wine"',
        "-fcf-protection=none", "-fvisibility=hidden",
        "-fno-stack-protector", "-fno-strict-aliasing",
    ]

    obj_dir = WINE_OBJ_DIR / "amphetamine_objs"
    obj_dir.mkdir(parents=True, exist_ok=True)

    log("INFO", f"Compiling ntdll ({len(sources)} files, gcc)...")
    objects = []
    for src in sources:
        obj = obj_dir / src.with_suffix(".o").name
        ret = subprocess.run(
            gcc_base + ["-o", str(obj), str(src)],
            capture_output=True, text=True,
        ).returncode
        if ret != 0:
            log("ERROR", f"Failed to compile {src.name}")
            return False
        objects.append(obj)

    # Link into ntdll.so
    ntdll_so = obj_dir / "ntdll.so"
    ret = subprocess.run(
        ["gcc", "-shared", "-o", str(ntdll_so)] + [str(o) for o in objects]
        + ["-lpthread", "-lrt", "-lm"],
    ).returncode
    if ret != 0:
        log("ERROR", "Failed to link ntdll.so")
        return False

    # Deploy
    lib_dir = STEAM_COMPAT_DIR / "lib"
    lib_dir.mkdir(parents=True, exist_ok=True)
    shutil.copy2(ntdll_so, ntdll_dest)
    log("INFO", f"Deployed ntdll.so ({ntdll_dest.stat().st_size // 1024} KB)")
    return True


def download_github_release(owner, repo, asset_glob):
    """Download latest release asset from GitHub. Returns path to cached tarball or None."""
    cache_dir = DATA_DIR / "downloads"
    cache_dir.mkdir(parents=True, exist_ok=True)

    # Check what we already have cached
    version_file = cache_dir / f"{repo}.version"
    cached_version = version_file.read_text().strip() if version_file.exists() else None

    # Query GitHub API for latest release
    api_url = f"https://api.github.com/repos/{owner}/{repo}/releases/latest"
    try:
        req = urllib.request.Request(api_url, headers={"Accept": "application/vnd.github+json"})
        with urllib.request.urlopen(req, timeout=15) as resp:
            data = json.loads(resp.read())
    except Exception as e:
        log("WARN", f"{repo}: failed to query GitHub API: {e}")
        # Fall back to cached version if available
        existing = list(cache_dir.glob(f"{repo}-*.tar.*"))
        if existing:
            log("INFO", f"{repo}: using cached download")
            return existing[0]
        return None

    tag = data.get("tag_name", "")
    if tag == cached_version:
        existing = list(cache_dir.glob(f"{repo}-*.tar.*"))
        if existing:
            log("INFO", f"{repo}: {tag} (cached)")
            return existing[0]

    # Find matching asset
    download_url = None
    asset_name = None
    for asset in data.get("assets", []):
        name = asset["name"]
        if asset_glob in name and name.endswith((".tar.gz", ".tar.xz", ".tar.zst")):
            download_url = asset["browser_download_url"]
            asset_name = name
            break

    if not download_url:
        log("WARN", f"{repo}: no matching release asset found (looking for '{asset_glob}')")
        return None

    # Clean old cached versions
    for old in cache_dir.glob(f"{repo}-*.tar.*"):
        old.unlink()

    dest = cache_dir / asset_name
    log("INFO", f"{repo}: downloading {tag}...")
    try:
        urllib.request.urlretrieve(download_url, dest)
    except Exception as e:
        log("ERROR", f"{repo}: download failed: {e}")
        return None

    version_file.write_text(tag)
    log("INFO", f"{repo}: downloaded {asset_name}")
    return dest


def download_dxvk_vkd3d():
    """Download DXVK and VKD3D-proton from GitHub releases."""
    dxvk_tar = download_github_release("doitsujin", "dxvk", "dxvk-")
    vkd3d_tar = download_github_release("HansKristian-Work", "vkd3d-proton", "vkd3d-proton-")
    return dxvk_tar, vkd3d_tar


def deploy_dxvk_vkd3d(dxvk_tar, vkd3d_tar):
    """Extract DXVK and VKD3D-proton DLLs into amphetamine's lib directory."""
    lib_dir = STEAM_COMPAT_DIR / "lib"

    if dxvk_tar:
        _deploy_tarball_dlls(dxvk_tar, lib_dir, "dxvk", "x64", "x32")
    if vkd3d_tar:
        _deploy_tarball_dlls(vkd3d_tar, lib_dir, "vkd3d-proton", "x64", "x86")


def _deploy_tarball_dlls(tarball, lib_dir, label, dir_64, dir_32):
    """Extract DLLs from a DXVK or VKD3D-proton tarball into lib/wine/{label}/."""
    staging = DATA_DIR / "staging" / label
    if staging.exists():
        shutil.rmtree(staging)
    staging.mkdir(parents=True, exist_ok=True)

    try:
        with tarfile.open(tarball) as tf:
            try:
                tf.extractall(staging, filter="data")
            except TypeError:
                # Python < 3.12: no filter parameter
                tf.extractall(staging)
    except Exception as e:
        log("ERROR", f"{label}: failed to extract tarball: {e}")
        return

    # Find the extracted directory (e.g., dxvk-2.5.3/)
    extracted = list(staging.iterdir())
    if not extracted:
        log("ERROR", f"{label}: tarball extracted empty")
        return
    root = extracted[0]

    # Deploy 64-bit DLLs
    src_64 = root / dir_64
    dst_64 = lib_dir / "wine" / label / "x86_64-windows"
    if src_64.exists():
        dst_64.mkdir(parents=True, exist_ok=True)
        count = 0
        for dll in src_64.glob("*.dll"):
            shutil.copy2(dll, dst_64 / dll.name)
            count += 1
        log("INFO", f"{label}: deployed {count} 64-bit DLLs")

    # Deploy 32-bit DLLs
    src_32 = root / dir_32
    dst_32 = lib_dir / "wine" / label / "i386-windows"
    if src_32.exists():
        dst_32.mkdir(parents=True, exist_ok=True)
        count = 0
        for dll in src_32.glob("*.dll"):
            shutil.copy2(dll, dst_32 / dll.name)
            count += 1
        log("INFO", f"{label}: deployed {count} 32-bit DLLs")

    # Cleanup staging
    shutil.rmtree(staging, ignore_errors=True)


def find_proton():
    """Find Proton installation (optional). Returns path to files/ dir or None."""
    steam_common = Path.home() / ".steam" / "root" / "steamapps" / "common"

    proton_exp = steam_common / "Proton - Experimental" / "files"
    if (proton_exp / "bin").exists():
        return proton_exp

    if steam_common.exists():
        for entry in steam_common.iterdir():
            if entry.name.startswith("Proton"):
                files = entry / "files"
                if (files / "bin").exists():
                    return files

    return None


def deploy_steam_exe():
    """Deploy steam.exe — cached copy or extract from Proton once."""
    cached = DATA_DIR / "steam.exe"

    # Already cached? Just verify it's still there
    if cached.exists():
        log("INFO", "steam.exe: cached")
        return True

    # Extract from Proton
    proton = find_proton()
    if not proton:
        log("WARN", "steam.exe: Proton not found — cannot extract")
        log("WARN", "  Games will run but Steam overlay/achievements won't work")
        log("WARN", "  Install any Proton from Steam to fix this (one-time extraction)")
        return False

    # Look for steam.exe in Proton's Wine DLL dirs
    for subdir in ("x86_64-windows", "i386-windows"):
        src = proton / "lib" / "wine" / subdir / "steam.exe"
        if src.exists():
            DATA_DIR.mkdir(parents=True, exist_ok=True)
            shutil.copy2(src, cached)
            log("INFO", f"steam.exe: extracted from Proton and cached")
            return True

    log("WARN", "steam.exe: not found in Proton installation")
    return False


def configure_verbose():
    """Handle --verbose / --no-verbose flags. Sticky: once enabled, stays
    enabled until explicitly disabled with --no-verbose."""
    flag = STEAM_COMPAT_DIR / "verbose_enabled"
    if "--verbose" in sys.argv:
        STEAM_COMPAT_DIR.mkdir(parents=True, exist_ok=True)
        flag.write_text("1")
        log("INFO", "Verbose diagnostics enabled (~/.cache/amphetamine/*.prom)")
    elif "--no-verbose" in sys.argv:
        if flag.exists():
            flag.unlink()
        log("INFO", "Verbose diagnostics disabled")
    elif flag.exists():
        log("INFO", "Verbose diagnostics: on (use --no-verbose to disable)")


def check_dependencies():
    """Verify required dependencies: Rust, Git, Steam, system Wine."""
    ok = True

    # Rust 1.85+ (edition 2024)
    try:
        out = subprocess.run(["rustc", "--version"], capture_output=True, text=True)
        if out.returncode == 0:
            ver = out.stdout.split()[1]
            parts = ver.split(".")
            major, minor = int(parts[0]), int(parts[1])
            if major < 1 or (major == 1 and minor < 85):
                log("ERROR", f"Rust: {ver} — requires 1.85+ (edition 2024)")
                log("ERROR", "  Update: rustup update stable")
                ok = False
            else:
                log("INFO", f"Rust: {ver}")
        else:
            log("ERROR", "Rust: not found")
            log("ERROR", "  Install from https://rustup.rs")
            ok = False
    except FileNotFoundError:
        log("ERROR", "Rust: not found")
        log("ERROR", "  Install from https://rustup.rs")
        ok = False

    # Git
    try:
        out = subprocess.run(["git", "--version"], capture_output=True, text=True)
        if out.returncode == 0:
            log("INFO", "Git: found")
        else:
            log("ERROR", "Git: not found")
            ok = False
    except FileNotFoundError:
        log("ERROR", "Git: not found")
        ok = False

    # Steam (native)
    steam_root = Path.home() / ".steam" / "root"
    if steam_root.exists():
        log("INFO", "Steam: found")
    else:
        log("ERROR", "Steam: not found (~/.steam/root)")
        log("ERROR", "  Install Steam natively (not Flatpak)")
        ok = False

    # System Wine
    try:
        out = subprocess.run(["wine", "--version"], capture_output=True, text=True)
        if out.returncode == 0:
            log("INFO", f"Wine: {out.stdout.strip()}")
        else:
            log("ERROR", "Wine: not found")
            log("ERROR", "  Install Wine from your package manager")
            ok = False
    except FileNotFoundError:
        log("ERROR", "Wine: not found")
        log("ERROR", "  Install Wine from your package manager")
        ok = False

    # gcc (needed for ntsync build, warn but don't fail)
    try:
        out = subprocess.run(["gcc", "--version"], capture_output=True, text=True)
        if out.returncode == 0:
            log("INFO", "gcc: found (needed for ntsync build)")
        else:
            log("WARN", "gcc: not found (needed only for ntsync build)")
    except FileNotFoundError:
        log("WARN", "gcc: not found (needed only for ntsync build)")

    return ok


def main():
    configure_verbose()

    if not check_dependencies():
        return 1

    # Step 1: Build and deploy triskelion binary
    ret = build_triskelion()
    if ret != 0:
        return ret

    # Step 2: ntsync build (optional — user prompt)
    print()
    check_ntsync()
    print()

    if prompt_yn("  Build triskelion with ntsync with Wine's upstream compatibility?"):
        if not install_wine_build_deps():
            log("WARN", "Skipping ntsync build — missing dependencies")
        else:
            clone_wine()
            if (WINE_SRC_DIR / "dlls").exists():
                patch_copy_triskelion_c()
                patch_makefile_in()
                patch_server_c()
                patch_unix_private_h()
                patch_win32u_message()
                log("INFO", f"Wine source patched: {WINE_SRC_DIR}")

                print()
                if build_wine():
                    log("INFO", "ntdll.so built: ntsync + shared-memory message bypass enabled")
                else:
                    log("WARN", "ntdll.so build failed — games still work without ntsync")
            else:
                log("ERROR", "Wine source: clone failed — skipping ntsync build")
    else:
        log("INFO", "ntsync build: skipped")

    # Step 3: Download and deploy DXVK/VKD3D-proton from GitHub
    print()
    log("INFO", "Downloading DXVK and VKD3D-proton...")
    dxvk_tar, vkd3d_tar = download_dxvk_vkd3d()
    if dxvk_tar or vkd3d_tar:
        deploy_dxvk_vkd3d(dxvk_tar, vkd3d_tar)
    else:
        log("WARN", "DXVK/VKD3D: download failed — games needing D3D translation may fail")

    # Step 4: Cache and deploy steam.exe
    print()
    deploy_steam_exe()

    # Step 5: User configuration
    configure_shader_cache()
    configure_custom_env()

    print()
    log("INFO", "Save data protection: enabled (automatic)")
    log("INFO", "  Pre-launch snapshots save data, restores if Steam Cloud sync")
    log("INFO", "  wipes files during first launch with a new compatibility tool.")
    print()

    log("INFO", "Installation complete!")
    return 0


if __name__ == "__main__":
    sys.exit(main())
