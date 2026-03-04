#!/usr/bin/env python3
"""Build triskelion, clone Valve Wine, and apply triskelion patches."""

import filecmp
import os
import shutil
import subprocess
import sys
from datetime import datetime
from pathlib import Path

SCRIPT_DIR = Path(__file__).parent.resolve()
RUST_DIR = SCRIPT_DIR / "amphetamine"
PATCHES_DIR = SCRIPT_DIR / "patches" / "wine"

WINE_SRC_DIR = Path.home() / ".local" / "share" / "amphetamine" / "wine-src"
STEAM_COMPAT_DIR = Path.home() / ".local" / "share" / "Steam" / "compatibilitytools.d" / "amphetamine"
WINE_CLONE_URL = "https://github.com/ValveSoftware/wine.git"
WINE_CLONE_BRANCH = "proton_10.0"


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

# Patch text: server.c bypass block (inserted before FTRACE_BLOCK_START)
SERVER_BYPASS = """\
    /* triskelion: shared memory bypass for hot-path messages */
    ret = triskelion_try_bypass( req_ptr );
    if (ret != STATUS_NOT_IMPLEMENTED)
        return ret;

"""

# Patch text: win32u peek_message condition prefix
PEEK_MSG_PREFIX = """\
        /* triskelion: if the shm ring has pending posted messages,
         * disable check_queue_bits and force the server call path.
         * The bypass in ntdll server_call_unlocked will pop from the ring. */
        if (!triskelion_has_posted(NtCurrentTeb()->glReserved2) &&
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


def build_triskelion():
    log("INFO", "Building triskelion binary")
    ret = subprocess.run(["cargo", "build", "--release", "-p", "triskelion"], cwd=SCRIPT_DIR).returncode
    if ret != 0:
        log("ERROR", "cargo build failed")
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
        log("INFO", f"Wine source exists: {WINE_SRC_DIR}")
        return
    log("INFO", f"Cloning Valve Wine ({WINE_CLONE_BRANCH}) to {WINE_SRC_DIR}")
    WINE_SRC_DIR.parent.mkdir(parents=True, exist_ok=True)
    ret = subprocess.run([
        "git", "clone", "--depth", "1", "-b", WINE_CLONE_BRANCH,
        WINE_CLONE_URL, str(WINE_SRC_DIR),
    ]).returncode
    if ret != 0:
        log("ERROR", "git clone failed (GitHub may be down, retry later)")
        return
    log("INFO", "Clone complete")


def patch_copy_triskelion_c():
    src = PATCHES_DIR / "dlls" / "ntdll" / "unix" / "triskelion.c"
    dst = WINE_SRC_DIR / "dlls" / "ntdll" / "unix" / "triskelion.c"
    if dst.exists() and filecmp.cmp(src, dst, shallow=False):
        log("INFO", "triskelion.c already in place")
        return
    shutil.copy2(src, dst)
    log("INFO", f"Copied triskelion.c -> {dst}")


def patch_makefile_in():
    path = WINE_SRC_DIR / "dlls" / "ntdll" / "Makefile.in"
    text = path.read_text()
    if "unix/triskelion.c" in text:
        log("INFO", "Makefile.in already patched")
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
    if "triskelion_try_bypass" in text:
        log("INFO", "server.c already patched")
        return
    anchor = '    FTRACE_BLOCK_START("req %s", req->name)'
    if anchor not in text:
        log("ERROR", f"Anchor not found in {path}: {anchor!r}")
        sys.exit(1)
    text = text.replace(anchor, SERVER_BYPASS + anchor)
    path.write_text(text)
    log("INFO", "Patched server.c: added triskelion_try_bypass call")


def patch_unix_private_h():
    path = WINE_SRC_DIR / "dlls" / "ntdll" / "unix" / "unix_private.h"
    text = path.read_text()
    if "triskelion_try_bypass" in text:
        log("INFO", "unix_private.h already patched")
        return
    anchor = "extern unsigned int server_call_unlocked( void *req_ptr );"
    if anchor not in text:
        log("ERROR", f"Anchor not found in {path}: {anchor!r}")
        sys.exit(1)
    text = text.replace(anchor, anchor + "\nextern unsigned int triskelion_try_bypass( void *req_ptr );")
    path.write_text(text)
    log("INFO", "Patched unix_private.h: added triskelion_try_bypass declaration")


def patch_win32u_message():
    path = WINE_SRC_DIR / "dlls" / "win32u" / "message.c"
    text = path.read_text()
    if "triskelion_has_posted" in text:
        log("INFO", "win32u/message.c already patched")
        return

    # Modification A: insert triskelion_has_posted function after debug channel declarations
    func_anchor = "WINE_DECLARE_DEBUG_CHANNEL(relay);"
    if func_anchor not in text:
        log("ERROR", f"Anchor not found in {path}: {func_anchor!r}")
        sys.exit(1)
    text = text.replace(func_anchor, func_anchor + WIN32U_FUNCTION)

    # Modification B: prepend triskelion check to peek_message condition
    # Original line starts with "        if (!filter->waited && NtGetTickCount()"
    peek_anchor = "        if (!filter->waited && NtGetTickCount() - thread_info->last_getmsg_time < 3000"
    if peek_anchor not in text:
        log("ERROR", f"Anchor not found in {path}: {peek_anchor!r}")
        sys.exit(1)
    # Replace the original "if (" with comment + triskelion check + original condition as continuation
    original_condition = "!filter->waited && NtGetTickCount() - thread_info->last_getmsg_time < 3000"
    text = text.replace(
        "        if (" + original_condition,
        PEEK_MSG_PREFIX + original_condition,
    )
    path.write_text(text)
    log("INFO", "Patched win32u/message.c: added triskelion_has_posted function + peek_message integration")


def configure_shader_cache():
    """Ask the user whether to enable per-game Vulkan shader cache optimization."""
    flag = STEAM_COMPAT_DIR / "shader_cache_enabled"

    print()
    print("  Shader cache optimization: amphetamine can configure per-game Vulkan")
    print("  shader caches (10 GB, organized per-prefix). This prevents compiled")
    print("  shaders from being evicted between sessions and reduces stutter.")
    print()
    print("  If you already manage shader cache settings yourself, say no.")
    print()

    if prompt_yn("  Enable shader cache optimization?"):
        flag.write_text("1")
        log("INFO", "Shader cache optimization enabled")
    else:
        if flag.exists():
            flag.unlink()
        log("INFO", "Shader cache optimization disabled")


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
    """Verify all required dependencies before building."""
    ok = True

    # Rust 1.85+ (edition 2024)
    try:
        out = subprocess.run(["rustc", "--version"], capture_output=True, text=True)
        if out.returncode == 0:
            # "rustc 1.93.1 (01f6ddf75 ...)" -> "1.93.1"
            ver = out.stdout.split()[1]
            parts = ver.split(".")
            major, minor = int(parts[0]), int(parts[1])
            if major < 1 or (major == 1 and minor < 85):
                log("ERROR", f"Rust {ver} is too old. Need 1.85+ (edition 2024).")
                log("ERROR", "Update with: rustup update stable")
                ok = False
            else:
                log("INFO", f"Rust {ver}")
        else:
            log("ERROR", "rustc not found. Install Rust: https://rustup.rs")
            ok = False
    except FileNotFoundError:
        log("ERROR", "rustc not found. Install Rust: https://rustup.rs")
        ok = False

    # Git
    try:
        out = subprocess.run(["git", "--version"], capture_output=True, text=True)
        if out.returncode != 0:
            log("ERROR", "git not found. Install git.")
            ok = False
    except FileNotFoundError:
        log("ERROR", "git not found. Install git.")
        ok = False

    # Steam (native)
    steam_root = Path.home() / ".steam" / "root"
    if not steam_root.exists():
        log("ERROR", "Steam not found at ~/.steam/root")
        log("ERROR", "Install Steam natively (not Flatpak).")
        ok = False

    # Proton (Wine binaries)
    steam_common = Path.home() / ".steam" / "root" / "steamapps" / "common"
    proton_found = False

    proton_exp = steam_common / "Proton - Experimental" / "files" / "bin" / "wine64"
    if proton_exp.exists():
        log("INFO", f"Found Proton Experimental")
        proton_found = True
    elif steam_common.exists():
        for entry in steam_common.iterdir():
            if entry.name.startswith("Proton") and (entry / "files" / "bin" / "wine64").exists():
                log("INFO", f"Found {entry.name}")
                proton_found = True
                break

    if not proton_found:
        log("ERROR", "Proton not found. amphetamine requires Proton's Wine binaries.")
        log("ERROR", "Install 'Proton Experimental' from your Steam Library.")
        ok = False

    return ok


def main():
    configure_verbose()

    if not check_dependencies():
        return 1

    clone_wine()

    ret = build_triskelion()
    if ret != 0:
        return ret

    configure_shader_cache()

    if (WINE_SRC_DIR / "dlls").exists():
        patch_copy_triskelion_c()
        patch_makefile_in()
        patch_server_c()
        patch_unix_private_h()
        patch_win32u_message()

        log("INFO", f"Wine source patched: {WINE_SRC_DIR}")
        log("INFO", f"Next: triskelion configure {WINE_SRC_DIR} --execute && cd {WINE_SRC_DIR} && make -j$(nproc)")
    else:
        log("WARN", "Wine source not available, skipping patches")
        log("WARN", "Binary deployed to Steam -- tracing and profiling will work")

    return 0


if __name__ == "__main__":
    sys.exit(main())
