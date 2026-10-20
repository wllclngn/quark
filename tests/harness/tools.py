"""The compatibility tools under test. quark is the tool; the Proton entries
exist only for an A/B run asked for by flag, and nothing else imports them
by name. Also the quark build and the per-run deploy into the compat dir."""
import glob
import os
import re
import shutil
import signal
import subprocess
import time
from pathlib import Path
from quark_common import COMPAT_DIR, LOG_DIR, QUARK_BIN, REPO_ROOT, STEAMAPPS, log_error, log_info, log_warn
from .base import WINE_INIT_GLOB


# Compat-tool launcher table. Default is quark; stock Proton entries enable
# side-by-side A/B comparison ("does the game work outside quark?")
# without leaving the iterate harness.
def _proton_ge() -> Path:
    """The newest GE-Proton under compatibilitytools.d, by version in the
    directory name; a path that does not exist when none is installed."""
    found = sorted(COMPAT_DIR.parent.glob("GE-Proton*"),
                   key=lambda p: [int(x) if x.isdigit() else x for x in re.split(r"(\d+)", p.name)])
    return (found[-1] if found else COMPAT_DIR.parent / "GE-Proton") / "proton"


COMPAT_TOOLS = {
    "quark":     COMPAT_DIR / "proton",
    "proton11":  STEAMAPPS / "common/Proton 11.0/proton",
    "proton10":  STEAMAPPS / "common/Proton 10.0/proton",
    "proton9":   STEAMAPPS / "common/Proton 9.0/proton",
    "proton-exp": STEAMAPPS / "common/Proton - Experimental/proton",
    "proton-ge": _proton_ge(),
}


def _compat_data_path(app_id: str, compat_tool: str) -> Path:
    """quark owns compatdata/<appid>; an A/B tool gets its own prefix beside
    it, so Proton never rewrites the prefix quark is measured on."""
    if compat_tool == "quark":
        return STEAMAPPS / "compatdata" / app_id
    return STEAMAPPS / "compatdata" / f"{app_id}.{compat_tool}-ab"


# Build & Install
def build(skip: bool = False) -> bool:
    if skip:
        log_info("build: skipped (--skip-build)")
        return True

    log_info("build: cargo build --release")
    t0 = time.monotonic()
    result = subprocess.run(
        ["cargo", "build", "--release"],
        cwd=str(REPO_ROOT),
        capture_output=True, text=True, timeout=300,
    )
    elapsed = time.monotonic() - t0

    if result.returncode != 0:
        log_error(f"build: failed ({elapsed:.1f}s)")
        for line in result.stderr.splitlines()[-20:]:
            log_error(f"build: {line}")
        return False

    warnings = sum(1 for line in result.stderr.splitlines() if "warning" in line.lower())
    log_info(f"build: ok ({elapsed:.1f}s, {warnings} warnings)")
    return True


def install(compat_tool: str = "quark") -> bool:
    # Stock Proton compat tools are pre-installed by Steam — nothing to deploy.
    if compat_tool != "quark":
        if not tool_bin(compat_tool).exists():
            log_error(f"install: {tool_bin(compat_tool)} not found (Steam install incomplete?)")
            return False
        log_info(f"install: using stock {compat_tool} at {tool_bin(compat_tool)}")
        # Still clean logs so they reflect this run only.
        for p in glob.glob(WINE_INIT_GLOB):
            try:
                os.unlink(p)
            except OSError:
                pass
        LOG_DIR.mkdir(parents=True, exist_ok=True)
        return True

    # Kill stale processes holding the binary
    killed = 0
    try:
        result = subprocess.run(["fuser", str(tool_bin(compat_tool))],
                                capture_output=True, text=True, timeout=5)
        if result.stdout.strip():
            pids = result.stdout.strip().split()
            for pid in pids:
                try:
                    os.kill(int(pid.strip()), signal.SIGKILL)
                    killed += 1
                except (ValueError, ProcessLookupError, PermissionError):
                    pass
    except (subprocess.TimeoutExpired, FileNotFoundError):
        pass

    if killed:
        log_warn(f"install: killed {killed} stale process(es)")
        time.sleep(1)

    if not QUARK_BIN.exists():
        log_error(f"install: {QUARK_BIN} not found")
        return False

    try:
        # Deploy using atomic rename to avoid ETXTBSY. If the old binary is
        # still mapped by a running process, copy+rename succeeds because
        # rename replaces the directory entry while the old inode stays
        # alive until the last process closes it.
        dst = COMPAT_DIR / "quark"
        tmp = COMPAT_DIR / ".quark.tmp"
        # Copy only when the built binary differs, so a launch of an
        # unchanged quark is measured on the deploy cache it left.
        src_st, dst_st = QUARK_BIN.stat(), (dst.stat() if dst.exists() else None)
        if dst_st is None or (src_st.st_size, int(src_st.st_mtime)) != (dst_st.st_size, int(dst_st.st_mtime)):
            shutil.copy2(str(QUARK_BIN), str(tmp))
            os.rename(str(tmp), str(dst))
        # proton symlink -> quark (the launcher)
        proton_link = COMPAT_DIR / "proton"
        if proton_link.exists() or proton_link.is_symlink():
            proton_link.unlink()
        proton_link.symlink_to("quark")
    except (OSError, shutil.SameFileError) as e:
        log_error(f"install: {e}")
        return False

    # Clean logs so they reflect this run only
    for p in glob.glob(WINE_INIT_GLOB):
        try:
            os.unlink(p)
        except OSError:
            pass
    LOG_DIR.mkdir(parents=True, exist_ok=True)
    log_info(f"install: deployed to {tool_bin(compat_tool)}")
    return True


def tool_bin(name: str) -> Path:
    """The launcher binary Steam would exec for this tool."""
    return COMPAT_TOOLS[name]
