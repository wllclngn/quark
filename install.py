#!/usr/bin/env python3
"""quark installer — build, patch, and deploy quark as a Steam compatibility tool.

Pipeline architecture: Seventeen steps in a dependency graph, each with explicit inputs, outputs,
and failure reporting. No silent failures. No swallowed errors.
"""

from __future__ import annotations

import argparse
import graphlib
import hashlib
import json
import os
import re
import shutil
import subprocess
import sys
import tarfile
import threading
import time
import types
import urllib.request
from concurrent.futures import ThreadPoolExecutor, wait, FIRST_COMPLETED
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

from quark_common import (
    REPO_ROOT, RUST_DIR, STEAM_ROOT, STEAMAPPS, COMPAT_DIR, DATA_DIR, CACHE_DIR,
    LSTEAMCLIENT_CACHE, LOG_DIR, WINE_SRC_DIR, WINE_OBJ_DIR,
    log, log_blank, log_info, log_section, log_warn, log_error, log_debug, log_finish,
    log_interrupted, log_intro, install_exit_guard, get_version,
    get_quark_appids, stop_prefix_wineserver, stale_wine_sockets,
    remove_stale_wine_sockets,
)

# Constants

SCRIPT_DIR = REPO_ROOT

WINE_CLONE_URL = "https://gitlab.winehq.org/wine/wine.git"
WINE_TAG = None  # Detected from system Wine at runtime; never hardcoded
EAC_RUNTIME_DIR = STEAMAPPS / "common" / "Proton EasyAntiCheat Runtime" / "v2"

PROTON_GIT_URL = "https://github.com/ValveSoftware/Proton.git"


def _derive_proton_tag() -> str:
    """The Proton release tag lsteamclient/steam_helper are built from.

    Dynamic to the installed Proton, mirroring derive_wine_tag() for Wine: Read
    the installed Proton's `version` file (format "<timestamp> <tag>") instead
    of pinning a literal that rots on a Proton bump. Point releases within a
    major line (proton-11.0-2 -> -2e) track automatically, and the lsteamclient
    repo layout is stable within a major line so this stays safe; crossing a
    major line is where Valve reorganizes, and the derivation never jumps major
    on its own. QUARK_PROTON_TAG overrides; the last validated tag is the
    fallback when no installed Proton exposes a proton-* release tag.
    """
    override = os.environ.get("QUARK_PROTON_TAG")
    if override:
        return override
    steam_common = STEAMAPPS / "common"
    for pname in ("Proton 11.0", "Proton - Experimental", "Proton Hotfix"):
        try:
            parts = (steam_common / pname / "version").read_text().split()
        except OSError:
            continue
        if len(parts) >= 2 and parts[1].startswith("proton-"):
            # The version file names the build ("proton-11.0-2e-x86_64");
            # the release tag has no architecture suffix.
            return re.sub(r"-(x86_64|aarch64|i386|arm64)$", "", parts[1])
    return "proton-11.0-2e"


PROTON_GIT_TAG = _derive_proton_tag()
PROTON_SOURCE_CACHE = CACHE_DIR / "proton-source" / PROTON_GIT_TAG
# The Steam bridge's own worktree of wine-src (_bridge_worktree).
WINE_BRIDGE_SRC = CACHE_DIR / "wine-src-bridge"

def install_hint(*packages: str) -> str:
    """The command that installs packages here: Shelly on Devario, which
    ships no pacman, and pacman everywhere else."""
    names = " ".join(packages)
    if not shutil.which("pacman") and shutil.which("shelly"):
        return f"shelly install standard {names}"
    return f"pacman -S {names}"


# Each tool and the package that provides it.
REQUIRED_TOOLS: dict[str, tuple[str | None, str]] = {
    "rustc":      ("1.85",  "rust"),
    "cargo":      (None,    "rust"),
    "cc":         (None,    "gcc"),
    "clang":      (None,    "clang"),
    "ld.lld":     (None,    "lld"),
    "git":        (None,    "git"),
    "autoconf":   (None,    "autoconf"),
    "bison":      (None,    "bison"),
    "flex":       (None,    "flex"),
    "make":       (None,    "make"),
    "patch":      (None,    "patch"),
    "pkg-config": (None,    "pkgconf"),
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
# Logging
# WINEDEBUG=-all
# DXVK_LOG_LEVEL=none
# VKD3D_DEBUG=none
# VKD3D_SHADER_DEBUG=none
#
# Overlays
# MANGOHUD=1
# DXVK_HUD=fps
#
# Performance
# mesa_glthread=true
#
# NVIDIA
# DXVK_ENABLE_NVAPI=1
"""


def _env_has_active_key(text: str, key: str) -> bool:
    """True only if env_config has an ACTIVE (non-comment) `key=...` line.
    A plain substring check is fooled by comments that mention the key
    (e.g. a '# active only with QUARK_TRACE=1' header), which silently
    suppressed the real assignment — see the 2026-06-12 observer regression."""
    prefix = key + "="
    for line in text.splitlines():
        s = line.strip()
        if s.startswith("#"):
            continue
        if s.startswith(prefix):
            return True
    return False


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


# THE STEP GRAPH
#
# Each step declares what it needs (other steps), what it reads (inputs: A
# path, walked if a directory, or a callable returning a string) and what it
# writes (outputs). graphlib.TopologicalSorter orders them and every step
# that is ready runs at once on a pool.
#
# The rebuilder is a verifying trace (Mokhov, Mitchell and Peyton Jones,
# "Build Systems à la Carte"): On success a step records a hash of its
# inputs and a hash of its outputs, and it is skipped only when both still
# match. A step with no inputs declared always runs.
#
# The invariant, from Spall, Mitchell and Tobin-Hochstadt, "Forward Build
# Systems, Formally": A run of this graph either produces the same tree as
# running the steps in order, or reports an error. Three rules keep it:
#   1. A step may skip only if every file it produces is in its outputs.
#      An undeclared write by a cached step is a bug, not an optimization.
#   2. Each output has exactly one producing step; the runner refuses a
#      graph where two steps declare the same path.
#   3. A step that rewrites another step's output invalidates that step's
#      trace, so the other step reruns rather than ship a stale file.
# The shared state the rules do not yet see (the one known undeclared write
# is the bridge's hunks in wine-src/configure.ac, which Patch Wine's reset
# also touches) is what a montauk capture of each step will check.

@dataclass
class Step:
    name: str
    func: Any
    needs: tuple[str, ...] = ()
    inputs: tuple[Any, ...] = ()
    outputs: tuple[Path, ...] = ()
    gate: bool = False   # a failure here aborts the whole install


INSTALL_MANIFEST = CACHE_DIR / "install-manifest.json"
_step_local = threading.local()


def _stamp_path(p: Path, h: "hashlib._Hash") -> None:
    if p.is_dir():
        for root, dirs, files in os.walk(p):
            dirs.sort()
            for f in sorted(files):
                fp = Path(root) / f
                try:
                    st = fp.stat()
                except OSError:
                    continue
                h.update(f"{fp}\0{st.st_size}\0{st.st_mtime_ns}\0".encode())
    elif p.exists():
        st = p.stat()
        h.update(f"{p}\0{st.st_size}\0{st.st_mtime_ns}\0".encode())
    else:
        h.update(f"{p}\0missing\0".encode())


def _stamp(inputs: tuple[Any, ...]) -> str:
    h = hashlib.sha256()
    for i in inputs:
        if callable(i):
            h.update(str(i()).encode() + b"\0")
        else:
            _stamp_path(Path(i), h)
    return h.hexdigest()


def _code_stamp(func: Any) -> str:
    """A step's own code, so a step whose code changed reruns though its
    inputs did not: Its bytecode, names and constants, nested code included,
    and the same of every install.py function it calls, none of which carry
    comments or line numbers. A frozenset constant hashes sorted, since its
    order follows the run's string hashing."""
    h = hashlib.sha256()
    seen = {func}

    def const_repr(const: Any) -> str:
        if isinstance(const, frozenset):
            return "frozenset" + repr(sorted(const_repr(c) for c in const))
        if isinstance(const, tuple):
            return "(" + ",".join(const_repr(c) for c in const) + ")"
        return repr(const)

    def walk(code: types.CodeType) -> None:
        h.update(code.co_code)
        h.update(repr(code.co_names).encode())
        for const in code.co_consts:
            if isinstance(const, types.CodeType):
                walk(const)
            else:
                h.update(const_repr(const).encode() + b"\0")
        for name in code.co_names:
            callee = globals().get(name)
            if (isinstance(callee, types.FunctionType) and callee.__module__ == func.__module__
                    and callee not in seen):
                seen.add(callee)
                walk(callee.__code__)

    walk(func.__code__)
    return h.hexdigest()


def _load_manifest() -> dict[str, dict[str, str]]:
    try:
        m = json.loads(INSTALL_MANIFEST.read_text())
        return {k: v for k, v in m.items() if isinstance(v, dict)}
    except (OSError, ValueError):
        return {}


def _save_manifest(m: dict[str, dict[str, str]]) -> None:
    CACHE_DIR.mkdir(parents=True, exist_ok=True)
    INSTALL_MANIFEST.write_text(json.dumps(m, indent=1, sort_keys=True) + "\n")


def _patched_files(patch_dir: Path, root: Path) -> tuple[Path, ...]:
    """The files a directory of patches writes, from their +++ b/ lines, so a
    step that applies them can declare them as outputs."""
    files: list[Path] = []
    for patch in sorted(patch_dir.glob("*.patch")):
        for line in patch.read_text(errors="replace").splitlines():
            if line.startswith("+++ b/"):
                p = root / line[6:].strip()
                if p not in files:
                    files.append(p)
    return tuple(files)


def _dir_listing(path: Path):
    """A callable input: the names in a directory, for a tree whose contents
    a step patches (so the contents cannot be the input) but whose identity
    must still be tracked."""
    def listing() -> str:
        try:
            return "\n".join(sorted(p.name for p in path.iterdir()))
        except OSError:
            return "missing"
    return listing


def _git_head(path: Path):
    def head() -> str:
        try:
            r = subprocess.run(["git", "-C", str(path), "rev-parse", "HEAD"],
                               capture_output=True, text=True, timeout=10)
            return r.stdout.strip() if r.returncode == 0 else "no-git"
        except (OSError, subprocess.TimeoutExpired):
            return "no-git"
    return head


def _prefixed_loggers() -> None:
    """When steps run side by side, each log line names its step."""
    def wrap(fn):
        def inner(msg: str) -> None:
            step = getattr(_step_local, "name", None)
            fn(f"{step}: {msg}" if step else msg)
        return inner
    g = globals()
    for name in ("log_info", "log_warn", "log_error", "log_debug"):
        g[name] = wrap(g[name])


def run_graph(steps: list[Step], workers: int) -> tuple[list[StepResult], int]:
    """Run the step graph. Returns (results, exit code); 0 means every step
    that ran succeeded, 1 a step failed, 130 interrupted."""
    by_name = {s.name: s for s in steps}
    producers: dict[Path, str] = {}
    for s in steps:
        for out in s.outputs:
            if out in producers:
                log_error(f"graph: {out} is declared as output by both {producers[out]} and {s.name}")
                return [], 1
            producers[out] = s.name
    sorter = graphlib.TopologicalSorter({s.name: set(s.needs) for s in steps})
    sorter.prepare()
    manifest = _load_manifest()
    results: dict[str, StepResult] = {}
    failed: set[str] = set()
    counter = {"n": 0}
    total = len(steps)
    lock = threading.Lock()

    def blocked_by_failure(step: Step) -> str | None:
        for n in step.needs:
            if n in failed:
                return n
        return None

    def run_one(step: Step) -> StepResult:
        _step_local.name = step.name
        with lock:
            counter["n"] += 1
            k = counter["n"]
        log_section(step.name, f"step {k} of {total}")
        trace = None
        if step.inputs:
            trace = {"inputs": _stamp(step.inputs), "code": _code_stamp(step.func),
                     "outputs": _stamp(step.outputs)}
            recorded = manifest.get(step.name)
            if recorded == trace:
                log_info("inputs, code and outputs match the last run, skipped")
                return StepResult(step.name, True, skipped=True)
            if recorded and recorded.get("code") != trace["code"]:
                log_info("the step's code changed since the last run; rerunning")
            elif recorded and recorded.get("inputs") == trace["inputs"]:
                log_info("inputs unchanged but an output was rewritten since the last run; rerunning")
        t0 = time.monotonic()
        result = step.func()
        if result.elapsed_ms == 0:
            result.elapsed_ms = int((time.monotonic() - t0) * 1000)
        if trace is not None and result.success and not result.skipped:
            with lock:
                manifest[step.name] = {"inputs": trace["inputs"], "code": trace["code"],
                                       "outputs": _stamp(step.outputs)}
                _save_manifest(manifest)
        return result

    pool = ThreadPoolExecutor(max_workers=workers)
    pending: dict[Any, Step] = {}
    try:
        while sorter.is_active():
            for name in sorter.get_ready():
                step = by_name[name]
                blocker = blocked_by_failure(step)
                if blocker:
                    log_warn(f"{step.name}: skipped, needs {blocker} which failed")
                    results[name] = StepResult(step.name, False, skipped=True)
                    failed.add(name)
                    sorter.done(name)
                    continue
                pending[pool.submit(run_one, step)] = step
            if not pending:
                continue
            done, _ = wait(list(pending), return_when=FIRST_COMPLETED)
            for fut in done:
                step = pending.pop(fut)
                try:
                    results[step.name] = fut.result()
                except StepFailed as e:
                    log_error(f"FAILED: {e.detail}")
                    results[step.name] = StepResult(step.name, False)
                    failed.add(step.name)
                    if step.gate:
                        pool.shutdown(wait=False, cancel_futures=True)
                        return [results[s.name] for s in steps if s.name in results], 1
                    log_warn(f"Continuing despite {step.name} failure...")
                sorter.done(step.name)
    except KeyboardInterrupt:
        log_interrupted()
        pool.shutdown(wait=False, cancel_futures=True)
        return [results[s.name] for s in steps if s.name in results], 130
    pool.shutdown(wait=True)
    ordered = [results[s.name] for s in steps if s.name in results]
    return ordered, (1 if failed else 0)


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

    On failure with check=True: Logs last 20 lines of stderr, raises StepFailed.
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
                log_error(f"{desc}: {line}")
        raise StepFailed(desc, f"exit code {result.returncode}")
    return result


# Write guardrail
#
# Quark must NEVER write to system-owned paths. Every recurring DLL incident
# on this project has come from an agent "fixing" a load problem by copying a
# patched DLL into /usr/lib/wine. The relocatable compat tree makes this
# unnecessary at runtime; the guardrail makes it impossible at install time.
#
# Allowlist of write roots — anything not under one of these is forbidden.
# Add a new entry here, with a comment, if a legitimate new write target appears.

def _quark_write_roots() -> list[Path]:
    return [
        COMPAT_DIR,                 # the only deploy target that matters
        WINE_SRC_DIR,                     # cloned wine source for patching
        WINE_OBJ_DIR,                     # wine build artifacts
        CACHE_DIR,                        # ~/.cache/quark — provenance manifests, downloads
        DATA_DIR,                         # ~/.local/share/quark — lsteamclient cache, downloads, staging
        SCRIPT_DIR / "target",            # cargo build output
        Path("/tmp"),                     # tarballs, scratch (must be under /tmp/...)
    ]


def assert_quark_writable(path: Path) -> None:
    """Raise StepFailed if `path` resolves outside any allowed quark write root.

    Use this at every NEW write call site. Catches accidental writes to /usr,
    /etc, /opt, /var, /lib, /sbin, /bin, and anything else owned by the system
    or by another package. Nothing at runtime will save you if install.py
    poisons /usr at install time — this is the line of defense.
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
    try:
        r = subprocess.run(["wine", "--version"], capture_output=True, text=True, timeout=5)
        if r.returncode == 0 and r.stdout.strip():
            return r.stdout.strip()
    except (FileNotFoundError, subprocess.TimeoutExpired, OSError):
        pass
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
    skip-rebuild gate: Only skip if the stored manifest is byte-identical."""
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

    Tracking symlinks is critical: The previous bug was an agent shipping
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
    detective control that pairs with assert_quark_writable: Even if a write
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
            log_error(f"{kind}: {path}")
        if len(changed) > 20:
            log_error(f"... and {len(changed) - 20} more")
        raise StepFailed(
            "system_wine_audit",
            f"install.py modified {len(changed)} entry(ies) in /usr/lib/wine or /usr/share/wine"
        )


_AUTO_YES = False
_TRACE = False
_WIPE_PREFIXES = False


def prompt_yn(question: str, auto: bool = True) -> bool:
    """Prompt the user with a [Y/N] question. Under --yes the answer is
    `auto`: Y for the questions an unattended install should agree to, N for
    a question that throws work away."""
    if _AUTO_YES:
        log_info(f"{question} {'yes' if auto else 'no'} (--yes)")
        return auto
    while True:
        answer = input(f"{question} [Y/N] ").strip().lower()
        if answer == "y":
            return True
        if answer == "n":
            return False


def cpu_count() -> int:
    return max(os.cpu_count() or 4, 1)


def make_cmd() -> list[str]:
    """make -j, through ccache for the host compiler when ccache is on PATH."""
    cmd = ["make", f"-j{cpu_count()}"]
    if shutil.which("ccache"):
        cmd.append("CC=ccache cc")
    return cmd


def fmt_size(n: int) -> str:
    if n >= 1 << 30:
        return f"{n / (1 << 30):.1f} GB"
    if n >= 1 << 20:
        return f"{n / (1 << 20):.1f} MB"
    if n >= 1 << 10:
        return f"{n / (1 << 10):.0f} KB"
    return f"{n} B"


def find_proton_wine_dir() -> Path | None:
    """Find a Proton installation's Wine directory for PE drivers."""
    steam_common = STEAMAPPS / "common"
    if not steam_common.exists():
        steam_common = Path.home() / ".steam" / "root" / "steamapps" / "common"
    if not steam_common.exists():
        return None
    for name in ["Proton - Experimental", "Proton Hotfix", "Proton 11.0", "Proton 10.0"]:
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

    # Check all tools. Rust builds quark from a source tree; a packaged
    # binary stands in for that build, so a package install needs no Rust.
    packaged = prebuilt_binary() is not None
    for tool, (min_ver, package) in REQUIRED_TOOLS.items():
        if packaged and tool in ("rustc", "cargo"):
            continue
        path = shutil.which(tool)
        if not path:
            missing.append(f"  {tool:12s} — install with: {install_hint(package)}")
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
    steam_root = STEAM_ROOT
    if not steam_root.exists():
        raise StepFailed("preflight", f"Steam not found ({steam_root}) — install Steam natively")
    log_info("Steam: found")

    # System Wine
    sys_wine = Path("/usr/lib/wine/x86_64-unix")
    if not sys_wine.exists() or not shutil.which("wine"):
        raise StepFailed("preflight", f"System Wine not found — {install_hint('wine')}")
    wine_ver = subprocess.run(["wine", "--version"], capture_output=True, text=True)
    log_info(f"Wine: {wine_ver.stdout.strip() if wine_ver.returncode == 0 else 'found'}")

    # ntsync is optional on the stock wineserver: Wine falls back to its own
    # sync when the device is absent. The test is whether the device opens,
    # not what the kernel version string says.
    kver = os.uname().release
    try:
        with open("/dev/ntsync", "rb"):
            pass
        log_info(f"ntsync: /dev/ntsync opens (kernel {kver}); kernel-native NT sync in use")
    except OSError as e:
        warnings.append(f"/dev/ntsync does not open ({e.strerror}); Wine will use its own sync. "
                        f"Try: {install_hint('ntsync-autoload')}, or modprobe ntsync")
        log_warn(f"{warnings[-1]}")
        log_warn("ntsync: /dev/ntsync not present — try: sudo modprobe ntsync")

    # Wine Mono (required for .NET/FNA games: TMNT, etc.), where Wine looks
    # for it (dlls/mscoree/metahost.c): Its build's /usr/share/wine/mono and
    # /opt/wine/mono, a wine-mono-<version> directory in either.
    mono_paths = [Path("/usr/share/wine/mono"), Path("/opt/wine/mono")]
    has_mono = any(p.is_dir() and any(p.glob("wine-mono-*")) for p in mono_paths)
    if has_mono:
        log_info("wine-mono: found")
    else:
        warnings.append(f"wine-mono not installed — .NET/FNA games will crash ({install_hint('wine-mono')})")
        log_warn("wine-mono: NOT FOUND — .NET/FNA games (TMNT, etc.) will crash")
        log_warn(f"install with: {install_hint('wine-mono')}")

    # 32-bit games: A WoW64 build runs them through wow64.dll on its i386 PE
    # modules, with no 32-bit unix side; an older multilib build has an
    # i386-unix tree instead.
    has_i386_win = Path("/usr/lib/wine/i386-windows").exists()
    has_wow64 = Path("/usr/lib/wine/x86_64-windows/wow64.dll").exists()
    has_i386_unix = Path("/usr/lib/wine/i386-unix").exists()
    if has_i386_win and (has_wow64 or has_i386_unix):
        log_info(f"wow64: 32-bit Wine modules found ({'WoW64' if has_wow64 else 'i386-unix'})")
    else:
        warnings.append("32-bit Wine modules missing — 32-bit games will not start")
        log_warn("wow64: 32-bit Wine modules NOT FOUND — 32-bit games will not start")
        log_warn(f"the wine package ships them (WoW64): {install_hint('wine')}")

    elapsed = (time.monotonic_ns() - t0) // 1_000_000
    return StepResult("Preflight", True, warnings=warnings, elapsed_ms=elapsed)


# Step 2: Clean

def _quark_prefixes() -> list[tuple[str, Path]]:
    """(appid, prefix) for every existing prefix of a quark-mapped game."""
    compatdata = STEAMAPPS / "compatdata"
    return [(appid, compatdata / appid / "pfx") for appid in sorted(get_quark_appids())
            if (compatdata / appid / "pfx").exists()]


def step_clean() -> StepResult:
    """Detect old builds and offer cleanup."""
    t0 = time.monotonic_ns()

    def dir_size(path: Path) -> int:
        try:
            return sum(f.stat().st_size for f in path.rglob("*") if f.is_file())
        except OSError:
            return 0

    found: list[tuple[str, Any, int]] = []

    if COMPAT_DIR.exists():
        found.append(("Steam compat tool", COMPAT_DIR, dir_size(COMPAT_DIR)))
    cargo_target = SCRIPT_DIR / "target"
    if cargo_target.exists():
        found.append(("Cargo build cache", cargo_target, dir_size(cargo_target)))
    if WINE_SRC_DIR.exists():
        found.append(("Wine source clone", WINE_SRC_DIR, dir_size(WINE_SRC_DIR)))
    if WINE_BRIDGE_SRC.exists():
        found.append(("Steam bridge worktree", WINE_BRIDGE_SRC, dir_size(WINE_BRIDGE_SRC)))
    if WINE_OBJ_DIR.exists():
        found.append(("Wine build objects", WINE_OBJ_DIR, dir_size(WINE_OBJ_DIR)))

    # Runtime state
    tmp_dir = LOG_DIR
    runtime_count = (1 if tmp_dir.exists() else 0) + len(stale_wine_sockets())
    if runtime_count:
        found.append(("Runtime state (logs, stale sockets)", None, 0))

    if not found:
        elapsed = (time.monotonic_ns() - t0) // 1_000_000
        return StepResult("Clean", True, skipped=True, elapsed_ms=elapsed)

    total = sum(size for _, _, size in found)
    log_section("existing install", "found")
    for label, path, size in found:
        line = f"{label}: {path}" if path and isinstance(path, Path) else label
        if size:
            line += f" ({fmt_size(size)})"
        log_info(line)
    if total:
        log_info(f"existing install: {fmt_size(total)} total")

    # Not under --yes: The manifest gate discards the Wine clone and build
    # when an input changed, and every deploy step overwrites, so an
    # unattended install builds on what is there instead of recloning Wine.
    if not prompt_yn("  Clean everything for a fresh build?", auto=False):
        log_info("Keeping existing artifacts — building on top")
        elapsed = (time.monotonic_ns() - t0) // 1_000_000
        return StepResult("Clean", True, skipped=True, elapsed_ms=elapsed)

    _clean_install(found, tmp_dir)
    log_info("Clean slate — proceeding with fresh build")
    elapsed = (time.monotonic_ns() - t0) // 1_000_000
    return StepResult("Clean", True, elapsed_ms=elapsed)


def _clean_install(found: list[tuple[str, Any, int]], tmp_dir: Path) -> None:
    """The accepted clean: Quark's wineservers stopped, the runtime state and
    every artifact found removed, and under --wipe-prefixes the quark-mapped
    prefixes with their shader caches."""
    # Stop quark's wineservers, one prefix at a time
    quark_prefixes = _quark_prefixes()
    for _, pfx in quark_prefixes:
        stop_prefix_wineserver(pfx)

    # Clean runtime state (may contain root-owned files from sudo traces)
    if tmp_dir.exists():
        try:
            shutil.rmtree(tmp_dir)
        except (PermissionError, OSError):
            log_warn(f"Could not fully remove {tmp_dir} (root-owned files from sudo?)")
            log_warn("Run: sudo rm -rf /tmp/quark")
    remove_stale_wine_sockets()

    # Wipe Wine prefixes and shader caches for games mapped to quark, only on
    # --wipe-prefixes (never implied by --yes). Stale prefixes cache Wine state
    # (registry, DLL load order, PHYSFS mount state) from previous builds, and
    # a shader cache left truncated by a hard-kill makes Fossilize double-free
    # on the next launch. Steam recreates both on next launch; save data in
    # Steam Cloud is unaffected, local-only saves in the prefix are lost.
    if quark_prefixes and not _WIPE_PREFIXES:
        log_info(f"Keeping {len(quark_prefixes)} quark prefix(es) "
                 "(--wipe-prefixes removes them and their shader caches)")
    elif quark_prefixes:
        log_warn(f"Wiping {len(quark_prefixes)} Wine prefix(es) for quark-mapped games")
        log_warn("Save data in Steam Cloud is NOT affected. Local-only saves")
        log_warn("in the prefix WILL be lost. Games: " +
                 ", ".join(a for a, _ in quark_prefixes))
        shadercache = STEAMAPPS / "shadercache"
        for appid, pfx in quark_prefixes:
            shutil.rmtree(pfx)
            log_info(f"Removed prefix: {pfx}")
            cache = shadercache / appid
            if cache.exists():
                shutil.rmtree(cache)
                log_info(f"Removed shader cache: {cache}")

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


# Step 3a: Sync wine source
#
# Clones the Wine source matching the system Wine version (auto-derived from
# `wine --version`) into WINE_SRC_DIR. step_patch_wine builds the patched
# modules from this tree and step_steam_bridge builds lsteamclient in it.
#
# Also runs the manifest gate: If any build input changed since the last
# install (wine version, patches, configure args), nukes WINE_SRC_DIR and
# WINE_OBJ_DIR so the rebuild starts from a clean slate.

def step_sync_wine_source() -> StepResult:
    """Clone wine source matching system Wine, run manifest gate."""
    t0 = time.monotonic_ns()
    wine_tag = derive_wine_tag()
    log_info(f"target wine source: {wine_tag}")

    # Manifest gate. Has to run before any rebuild decision: If inputs changed,
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
                    log_warn(f"{k}: {old}  →  {v}")
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
        # A shallow clone of one tag is still over 100 MB; give a slow link
        # the time, and let git's own failure end it otherwise.
        run_cmd(
            ["git", "clone", "--depth", "1", "-b", wine_tag, WINE_CLONE_URL, str(WINE_SRC_DIR)],
            desc="clone Wine", timeout=1800,
        )
    else:
        log_info(f"Wine source present at {WINE_SRC_DIR}")

    # Reset to clean state — patches get applied later in step_patch_wine
    run_cmd(["git", "checkout", "HEAD", "--", "."], desc="git reset wine-src",
            cwd=WINE_SRC_DIR, check=False)
    run_cmd(["git", "clean", "-fd"], desc="git clean wine-src",
            cwd=WINE_SRC_DIR, check=False)

    # Sanity: Protocol.def is the marker of a complete clone
    proto_def = WINE_SRC_DIR / "server" / "protocol.def"
    if not proto_def.exists():
        raise StepFailed("sync wine source", f"protocol.def missing after clone: {proto_def}")

    elapsed = (time.monotonic_ns() - t0) // 1_000_000
    return StepResult("Sync wine source", True, elapsed_ms=elapsed)


# Step 3: Build

def prebuilt_binary() -> Path | None:
    """The binary a package installed beside this script, used when there is
    no source tree to build from (PKGBUILD lays the tree out this way)."""
    binary = SCRIPT_DIR / "quark"
    if binary.is_file() and not (RUST_DIR / "src").is_dir():
        return binary
    return None


def step_build() -> StepResult:
    """Build quark via cargo, unless a packaged binary stands in for the source."""
    t0 = time.monotonic_ns()
    if (binary := prebuilt_binary()) is not None:
        log_info(f"Build: packaged binary {binary}")
        elapsed = (time.monotonic_ns() - t0) // 1_000_000
        return StepResult("Build", True, artifacts=[binary], elapsed_ms=elapsed)
    log_info("Building quark...")

    # Captured: A failure logs cargo's stderr tail under its name; on success
    # cargo's own lines are DEBUG, in the install's log.
    result = run_cmd(["cargo", "build", "--release", "-p", "quark"],
                     desc="cargo build", cwd=SCRIPT_DIR, timeout=600)
    for line in result.stderr.strip().splitlines():
        log_debug(f"cargo build: {line.strip()}")

    target_dir = SCRIPT_DIR / "target" / "release"
    binary = target_dir / "quark"
    if not binary.exists():
        raise StepFailed("build", f"binary not found: {binary}")
    artifacts = [binary]

    elapsed = (time.monotonic_ns() - t0) // 1_000_000
    return StepResult("Build", True, artifacts=artifacts, elapsed_ms=elapsed)


# Step 4: Deploy Binaries

def step_deploy_binaries() -> StepResult:
    """Deploy binaries to Steam compat dir, write VDFs, create symlinks."""
    t0 = time.monotonic_ns()
    COMPAT_DIR.mkdir(parents=True, exist_ok=True)
    src = prebuilt_binary() or SCRIPT_DIR / "target" / "release" / "quark"
    dst = COMPAT_DIR / "quark"
    shutil.copy2(src, dst)
    os.chmod(dst, 0o755)
    artifacts = [dst]

    # proton → quark symlink (Steam VDF expects "proton")
    proton_link = COMPAT_DIR / "proton"
    if proton_link.exists() or proton_link.is_symlink():
        proton_link.unlink()
    proton_link.symlink_to("quark")

    # VDF
    version = get_version()
    vdf = COMPAT_DIR / "compatibilitytool.vdf"
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

    manifest = COMPAT_DIR / "toolmanifest.vdf"
    manifest.write_text('''"manifest"
{
  "commandline" "/proton %verb%"
  "version" "2"
  "use_sessions" "1"
  "use_tool_subprocess_reaper" "1"
}
''')

    log_info(f"Deployed: quark {version}")
    elapsed = (time.monotonic_ns() - t0) // 1_000_000
    return StepResult("Deploy binaries", True, artifacts=artifacts, elapsed_ms=elapsed)


# Step 5: Deploy Wine

def step_deploy_wine() -> StepResult:
    """Deploy system Wine tree."""
    t0 = time.monotonic_ns()
    warnings: list[str] = []

    sys_unix = Path("/usr/lib/wine/x86_64-unix")
    sys_bin = Path("/usr/bin")

    if not sys_unix.exists() or not (sys_bin / "wine").exists():
        raise StepFailed("deploy wine", f"system Wine not found at /usr/lib/wine/ — {install_hint('wine')}")

    # bin/ — wine loader + wineserver. The preloader is not here: Ntdll execs
    # the wine-preloader beside its own loader in lib/wine/x86_64-unix, which
    # the lib/wine sync below carries.
    bin_dir = COMPAT_DIR / "bin"
    bin_dir.mkdir(parents=True, exist_ok=True)

    dst = bin_dir / "wine"
    if dst.exists() or dst.is_symlink():
        dst.unlink()
    # COPY — Wine resolves symlinks for wineserver discovery
    shutil.copy2(sys_bin / "wine", dst)
    # An earlier install's preloader and wine64 link; nothing runs either.
    for stale in ("wine-preloader", "wine64"):
        stale_path = bin_dir / stale
        if stale_path.exists() or stale_path.is_symlink():
            stale_path.unlink()

    # wineserver → system wineserver (matches the system Wine protocol)
    wineserver_link = bin_dir / "wineserver"
    if wineserver_link.exists() or wineserver_link.is_symlink():
        wineserver_link.unlink()
    wineserver_link.symlink_to(sys_bin / "wineserver")

    # lib/wine/ — hardlink files from system Wine
    wine_lib = COMPAT_DIR / "lib" / "wine"
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
            log_warn(f"{w}")

    share_files = _deploy_share_wine(warnings)
    log_info(f"System Wine deployed ({total_files} lib files, {share_files} share files)")
    elapsed = (time.monotonic_ns() - t0) // 1_000_000
    return StepResult("Deploy Wine", True, warnings=warnings, elapsed_ms=elapsed)


def _deploy_share_wine(warnings: list[str]) -> int:
    """The tree's share/wine from the system's; returns the files placed."""
    # share/wine/ — wine.inf, nls, fonts, winmd. Copies, NOT symlinks.
    # Wine resolves share/wine relative to the tree's bin/wine at launch time;
    # symlinks pointing back into /usr would hand it the system's files.
    # Mono and Gecko stay where their packages put them: Wine also looks in
    # its build's /usr/share/wine, through the prefix's Z: drive
    # (dlls/mscoree/metahost.c, dlls/appwiz.cpl/addons.c), so a copy here
    # would only duplicate them.
    share_wine = COMPAT_DIR / "share" / "wine"
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

    # Directory trees: Nls (locale data), fonts, winmd (WinRT metadata).
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

    # An earlier install's copy of Mono goes; Wine loads the system's.
    dst_mono = share_wine / "mono"
    if dst_mono.is_symlink():
        dst_mono.unlink()
    elif dst_mono.exists():
        assert_quark_writable(dst_mono)
        shutil.rmtree(dst_mono)
        log_info(f"wine-mono: removed the tree's copy; Wine loads {sys_share / 'mono'}")
    return share_files


# Step 6: Patch Wine

def step_patch_wine() -> StepResult:
    """Apply patches to wine source (already cloned by step_sync_wine_source),
    configure, build patched DLLs."""
    t0 = time.monotonic_ns()
    wine_lib = COMPAT_DIR / "lib" / "wine"

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

    # Apply patches strictly. A patch that does not apply exactly as written
    # no longer fits this Wine version and fails the install, by name, to be
    # re-ported. No fuzzy fallback: `patch --fuzz` exits 0 after landing a
    # hunk in the wrong function or inside a comment, which ships a patch
    # that silently does nothing or breaks the build somewhere else.
    for patch in patches:
        check_result = run_cmd(
            ["git", "apply", "--check", str(patch)],
            desc=f"check {patch.name}", cwd=src_dir, check=False,
        )
        if check_result.returncode != 0:
            log_error(f"Patch does NOT apply to this Wine version: {patch.name}")
            for line in (check_result.stderr or "").strip().splitlines()[-6:]:
                log_error(f"{patch.name}: {line}")
            raise StepFailed(
                "patch wine",
                f"{patch.name} does not apply strictly to {derive_wine_tag()} — re-port it",
            )
        run_cmd(["git", "apply", str(patch)],
                desc=f"apply {patch.name}", cwd=src_dir, check=True)
        log_info(f"Applied: {patch.name}")

    log_info(f"Applied {len(patches)}/{len(patches)} Wine patches")

    # Configure
    if not (build_dir / "Makefile").exists():
        log_info("Configuring Wine build...")
        build_dir.mkdir(parents=True, exist_ok=True)
        # Regenerate configure from configure.ac, which a patch may touch (none
        # does today; the Steam bridge registers lsteamclient and steam_helper
        # in its own worktree's). Use plain `autoconf`, NOT
        # `autoreconf -f`: autoreconf pulls in aclocal/automake/libtoolize and,
        # on a newer host autoconf (2.73+), regenerates a BROKEN configure whose
        # conftest compiles all fail — surfacing as a false "pthread 64-bit
        # development files not found". Wine's own build regenerates configure
        # with plain autoconf; that is version-robust across host autoconf bumps.
        run_cmd(["autoconf"], desc="autoconf (regenerate configure)",
                cwd=src_dir, check=False)
        run_cmd(
            [str(src_dir / "configure"), "--prefix=/usr", "--enable-win64",
             "--with-x", "--without-wayland"],
            desc="Wine configure", cwd=build_dir, timeout=300,
        )

    # Build ALL patched DLLs. The curated map below pins module names that need
    # explicit Wine-side rename (so_name) or are arch-only. Anything else is
    # auto-discovered from the patches themselves: Scan each patch's diff
    # headers for `a/dlls/<name>/` and `a/programs/<name>/` paths, then
    # build whatever PE+unix targets exist for that module. New patches
    # land automatically — no need to remember to edit this dict.
    dll_targets: dict[str, tuple] = {
        "ntdll": ("dlls/ntdll/ntdll.so", "ntdll.so",
                  "dlls/ntdll/x86_64-windows/ntdll.dll", "ntdll.dll"),
        # winepulse.drv: Unixlib-only rebuild. The audio fix lives entirely in
        # the unix side (pulse.c -> winepulse.so); the PE stub is unchanged, so
        # no x86_64-windows target. Auto-discovery would derive the wrong PE
        # name ("winepulse.drv.dll") from the .drv module dir and fail the make.
        "winepulse.drv": ("dlls/winepulse.drv/winepulse.so", "winepulse.so", None, None),
    }

    diff_path_re = re.compile(r"^[-+]{3} [ab]/(dlls|programs)/([^/]+)/")
    discovered: set[tuple[str, str]] = set()  # (kind, name)
    for patch in patches:
        for line in patch.read_text(errors="replace").splitlines():
            m = diff_path_re.match(line)
            if m:
                discovered.add((m.group(1), m.group(2)))

    for kind, name in sorted(discovered):
        if name in dll_targets:
            continue
        if kind == "dlls":
            mk = src_dir / "dlls" / name / "Makefile.in"
            mk_text = mk.read_text(errors="replace") if mk.exists() else ""
            has_unixlib = "UNIXLIB" in mk_text and f"{name}.so" in mk_text
            so_path = f"dlls/{name}/{name}.so" if has_unixlib else None
            so_name = f"{name}.so" if has_unixlib else None
            dll_targets[name] = (so_path, so_name,
                                 f"dlls/{name}/x86_64-windows/{name}.dll", f"{name}.dll")
        else:  # programs
            dll_targets[name] = (None, None,
                                 f"programs/{name}/x86_64-windows/{name}.exe", f"{name}.exe")
        log_info(f"Auto-discovered patched module: {kind}/{name}")

    artifacts, failed = _build_patched_modules(dll_targets, build_dir, wine_lib)

    # A patched module that did not build ships its unpatched system copy,
    # so the patch is silently absent; that fails the step, by name.
    if failed:
        raise StepFailed("patch wine", f"patched module(s) failed to build: {', '.join(sorted(set(failed)))}")

    # Build succeeded — record the inputs that produced these artifacts so the
    # next install can decide whether to trust the cache or rebuild.
    write_build_manifest(manifest_path, current_inputs)
    log_info(f"Build manifest written: {manifest_path.name}")

    log_info(f"Deployed {len(artifacts)} patched Wine module(s)")
    elapsed = (time.monotonic_ns() - t0) // 1_000_000
    return StepResult("Patch Wine", True, artifacts=artifacts, elapsed_ms=elapsed)


def _build_patched_modules(dll_targets: dict[str, tuple], build_dir: Path,
                           wine_lib: Path) -> tuple[list[Path], list[str]]:
    """Build each patched module's targets and copy what they produce into
    the tree. Returns the files placed and the modules that failed."""
    artifacts: list[Path] = []
    failed: list[str] = []
    for dll, (so_path, so_name, pe_path, pe_name) in dll_targets.items():
        build_targets = []
        if so_path:
            build_targets.append(so_path)
        if pe_path:
            build_targets.append(pe_path)

        log_info(f"Building {dll}...")
        result = run_cmd(
            make_cmd() + build_targets,
            desc=f"build {dll}", cwd=build_dir, timeout=300, check=False,
        )
        if result.returncode != 0:
            log_error(f"Build failed for {dll}")
            if result.stderr:
                for line in result.stderr.strip().splitlines()[-10:]:
                    log_error(f"{dll}: {line}")
            failed.append(dll)
            continue

        if so_path:
            built_so = build_dir / so_path
            if built_so.exists():
                dst = wine_lib / "x86_64-unix" / so_name
                if dst.exists():
                    dst.unlink()
                shutil.copy2(built_so, dst)
                artifacts.append(dst)
                log_info(f"Deployed {so_name} ({built_so.stat().st_size // 1024}K)")
            else:
                log_error(f"{so_name} not produced")
                failed.append(dll)

        if pe_path:
            built_pe = build_dir / pe_path
            if built_pe.exists():
                dst = wine_lib / "x86_64-windows" / pe_name
                if dst.exists():
                    dst.unlink()
                shutil.copy2(built_pe, dst)
                artifacts.append(dst)
                log_info(f"Deployed {pe_name} ({built_pe.stat().st_size // 1024}K)")
            else:
                log_error(f"{pe_name} not produced")
                failed.append(dll)
    return artifacts, failed


# Step 6b: Bake prefix template

def step_bake_prefix_template() -> StepResult:
    """Copy Proton's pre-baked default_pfx into the compat tree as the prefix
    template, repairing the relative symlinks Proton's directory depth breaks.

    Its system.reg/user.reg carry the COM class registrations games need for
    CoCreateInstance to find mmdevapi, devenum, quartz, etc. Without them,
    audio init fails and DXVK games show white windows.

    The template is copied ONCE during install and into each game prefix at
    launch time. This replaces the 45-second update_wineprefix call.
    """
    t0 = time.monotonic_ns()
    template_dir = COMPAT_DIR / "default_pfx"
    system_reg = template_dir / "system.reg"

    if system_reg.exists() and system_reg.stat().st_size > 100_000:
        log_info("prefix template: already baked")
        elapsed = (time.monotonic_ns() - t0) // 1_000_000
        return StepResult("Bake prefix", True, elapsed_ms=elapsed)

    # Source: Proton's pre-baked default_pfx (9000+ CLSID entries, complete COM registration).
    # Proton bakes this at BUILD time via wineboot --init. We steal the result.
    proton_dirs = [
        "Proton 11.0", "Proton - Experimental", "Proton Hotfix", "Proton 10.0",
    ]
    steam_common = STEAMAPPS / "common"
    source_pfx = None
    for pname in proton_dirs:
        candidate = steam_common / pname / "files/share/default_pfx"
        if (candidate / "system.reg").exists():
            source_pfx = candidate
            break

    if source_pfx is None:
        # Skipping this step ships a broken install — the launcher seeds new
        # prefixes from the baked default_pfx. A silent skip used to let
        # users complete install and then fail mysteriously at first game
        # launch.
        raise StepFailed(
            "Bake prefix",
            f"No Proton default_pfx found in {steam_common}. Install at least one of: "
            f"{', '.join(proton_dirs)} via Steam first, then re-run install.py."
        )

    # Copy the ENTIRE prefix directory structure (not just .reg files).
    # Games need Fonts, system32 DLLs, directory structure, etc.
    if template_dir.exists():
        shutil.rmtree(template_dir)
    log_info(f"Copying full prefix template from {source_pfx.parent.parent.name}...")
    shutil.copytree(source_pfx, template_dir, symlinks=True)

    # Proton's default_pfx ships RELATIVE symlinks in subdirs (e.g.
    # drive_c/windows/system32/wbem/*.dll → ../../../../../../lib/wine/...)
    # whose `../` count assumes Proton's exact directory depth. Our compat
    # tree has a different parent layout, so those symlinks resolve to
    # paths that don't exist — Wine then fails LoadLibrary on wbemprox.dll
    # and Unity/Mono games crash on the WMI apartment init (apartment_add_dll).
    # Walk the template and replace any broken symlink whose target name
    # exists in our deployed wine lib with a real file. Idempotent.
    wine_lib_win = COMPAT_DIR / "lib" / "wine" / "x86_64-windows"
    fixed = 0
    for link in template_dir.rglob("*"):
        if not link.is_symlink():
            continue
        # Resolve manually — if target doesn't exist, fix it.
        try:
            target_resolved = link.resolve(strict=False)
            if target_resolved.exists():
                continue
        except OSError:
            continue
        # Broken. Try to find the actual file in our wine lib by basename.
        replacement = wine_lib_win / link.name
        if replacement.exists() and replacement.is_file():
            try:
                link.unlink()
                shutil.copy2(replacement, link)
                fixed += 1
            except OSError as e:
                log_warn(f"failed to repair {link.name}: {e}")
    if fixed > 0:
        log_info(f"repaired {fixed} broken symlinks in default_pfx")

    total_size = sum(f.stat().st_size for f in template_dir.rglob("*") if f.is_file())
    log_info(f"prefix template: {total_size // 1024 // 1024}MB from Proton ({source_pfx.parent.parent.name})")

    elapsed = (time.monotonic_ns() - t0) // 1_000_000
    return StepResult("Bake prefix", True, elapsed_ms=elapsed)


# Step 7: Build Stubs

def step_build_stubs() -> StepResult:
    """Build tier0/vstdlib DLL stubs via clang+lld (no mingw)."""
    t0 = time.monotonic_ns()
    wine_lib = COMPAT_DIR / "lib" / "wine"

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
            log_info(f"{stub_name}.dll stub deployed (64-bit)")
        else:
            log_error(f"Failed to build {stub_name}.dll")

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
            log_info(f"{stub_name}.dll stub deployed (32-bit)")
        else:
            log_error(f"Failed to build {stub_name}.dll")

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
    """Build lsteamclient.dll + steam.exe from Proton source against the system Wine's source."""
    t0 = time.monotonic_ns()
    wine_lib = COMPAT_DIR / "lib" / "wine"

    dll_cache = LSTEAMCLIENT_CACHE / "lsteamclient.dll"
    so_cache = LSTEAMCLIENT_CACHE / "lsteamclient.so"
    steam_cache = LSTEAMCLIENT_CACHE / "steam.exe"

    # Always rebuild. A stale cache with Proton-ABI binaries is the #1 cause
    # of Steam auth failure.
    if LSTEAMCLIENT_CACHE.exists():
        shutil.rmtree(LSTEAMCLIENT_CACHE)

    # Download Proton source (targeted — just lsteamclient + steam_helper)
    proton_src = _download_proton_source()

    wine_src = _bridge_worktree()

    # Symlink lsteamclient into the worktree
    src_ls = proton_src / "lsteamclient"
    if not src_ls.exists():
        raise StepFailed(
            "steam bridge",
            f"lsteamclient source missing at {src_ls} — Proton {PROTON_GIT_TAG} "
            f"download/extract incomplete; delete {PROTON_SOURCE_CACHE} and re-run",
        )
    dll_link = wine_src / "dlls" / "lsteamclient"
    dll_link.parent.mkdir(parents=True, exist_ok=True)
    if dll_link.is_symlink() or dll_link.exists():
        dll_link.unlink()
    dll_link.symlink_to(src_ls)
    log_info(f"lsteamclient: symlinked into {wine_src.name}/dlls/")

    # Register in configure.ac
    configure_ac = wine_src / "configure.ac"
    text = configure_ac.read_text()
    if "dlls/lsteamclient" not in text:
        text = text.replace(
            "WINE_CONFIG_MAKEFILE(dlls/lz32/tests)",
            "WINE_CONFIG_MAKEFILE(dlls/lz32/tests)\nWINE_CONFIG_MAKEFILE(dlls/lsteamclient)",
        )
        configure_ac.write_text(text)

    artifacts = _build_bridge(proton_src, wine_src, configure_ac, dll_cache, so_cache, steam_cache)

    # Deploy
    _deploy_lsteamclient(
        dll_cache if dll_cache.exists() else None,
        so_cache if so_cache.exists() else None,
        steam_cache if steam_cache.exists() else None,
        wine_lib,
    )

    elapsed = (time.monotonic_ns() - t0) // 1_000_000
    return StepResult("Steam bridge", True, artifacts=artifacts, elapsed_ms=elapsed)


def _bridge_worktree() -> Path:
    """The bridge's own worktree of wine-src at wine-src's commit, reset and
    cleaned, so the bridge's symlinks, configure.ac hunks and autoconf never
    meet the tree Patch Wine resets, patches and builds in, and the two steps
    run side by side. The bridge builds against stock Wine: lsteamclient
    uses nothing the Wine patches add."""
    if not (WINE_SRC_DIR / ".git").exists():
        raise StepFailed("steam bridge", "wine-src not found — sync wine source must run first")
    head = run_cmd(["git", "rev-parse", "HEAD"], desc="wine-src commit", cwd=WINE_SRC_DIR).stdout.strip()
    # A reclone of wine-src drops the worktree's registration; the stale
    # checkout goes and a new one is added.
    if WINE_BRIDGE_SRC.exists():
        valid = run_cmd(["git", "rev-parse", "--is-inside-work-tree"], desc="bridge worktree",
                        cwd=WINE_BRIDGE_SRC, check=False).returncode == 0
        if not valid:
            assert_quark_writable(WINE_BRIDGE_SRC)
            shutil.rmtree(WINE_BRIDGE_SRC)
    run_cmd(["git", "worktree", "prune"], desc="prune wine-src worktrees", cwd=WINE_SRC_DIR, check=False)
    if WINE_BRIDGE_SRC.exists():
        run_cmd(["git", "checkout", "--detach", "--force", head], desc="reset the bridge worktree",
                cwd=WINE_BRIDGE_SRC)
        run_cmd(["git", "clean", "-fd"], desc="clean the bridge worktree", cwd=WINE_BRIDGE_SRC)
    else:
        assert_quark_writable(WINE_BRIDGE_SRC)
        run_cmd(["git", "worktree", "add", "--detach", "--force", str(WINE_BRIDGE_SRC), head],
                desc="add the bridge worktree", cwd=WINE_SRC_DIR, timeout=600)
    return WINE_BRIDGE_SRC


def _build_bridge(proton_src: Path, wine_src: Path, configure_ac: Path,
                  dll_cache: Path, so_cache: Path, steam_cache: Path) -> list[Path]:
    """steam_helper registered beside lsteamclient, the bridge's patches
    applied, its build configured and run in /tmp, and what it produced
    cached. Returns the cached files."""
    # steam_helper beside lsteamclient, registered the same way
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

    # Apply lsteamclient patches (patches/lsteamclient/). Strict, like the Wine
    # patches: One that does not apply fails the step by name. The Proton
    # source cache persists across installs, so a patch that strictly
    # reverse-applies is already in and is skipped.
    for pfile in sorted((SCRIPT_DIR / "patches" / "lsteamclient").glob("*.patch")):
        forward = run_cmd(["git", "apply", "--check", str(pfile)],
                          desc=f"check {pfile.name}", cwd=proton_src, check=False)
        if forward.returncode == 0:
            run_cmd(["git", "apply", str(pfile)],
                    desc=f"apply {pfile.name}", cwd=proton_src, check=True)
            log_info(f"Applied: {pfile.name}")
            continue
        reverse = run_cmd(["git", "apply", "--check", "-R", str(pfile)],
                          desc=f"check {pfile.name} applied", cwd=proton_src, check=False)
        if reverse.returncode == 0:
            log_info(f"Already applied: {pfile.name}")
            continue
        for line in (forward.stderr or "").strip().splitlines()[-6:]:
            log_error(f"{pfile.name}: {line}")
        raise StepFailed("steam bridge",
                         f"{pfile.name} does not apply strictly to Proton {PROTON_GIT_TAG} — re-port it")

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
        make_cmd() + targets,
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
            log_info(f"{label}: {path.stat().st_size // 1024}K")

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
    return artifacts


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
        # No steamclient[64] aliases: The trampoline (Wine patch 001) covers
        # a full-path load of steamclient64.dll, which is how steam_api64
        # loads it, so the bridge is reached under its own name.
        for stale in ("steamclient64.dll", "steamclient.dll"):
            alias_dst = win_dir / stale
            if alias_dst.exists() or alias_dst.is_symlink():
                alias_dst.unlink()

    if so_path and so_path.exists():
        dst = unix_dir / "lsteamclient.so"
        assert_quark_writable(dst)
        if dst.exists():
            dst.chmod(0o755)
        shutil.copy2(so_path, dst)
        for stale in ("steamclient64.so", "steamclient.so"):
            alias_dst = unix_dir / stale
            if alias_dst.exists() or alias_dst.is_symlink():
                alias_dst.unlink()

    # lsteamclient lives only in the compat tree. Wine resolves lib/wine
    # relative to the tree's own bin/wine at launch time, so its builtin
    # loader finds lsteamclient.{so,dll} at the path it expects (the tree's
    # lib/wine, no bind mount involved). The previous version of
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


# Step 8b: Trace capture (montauk; opt-in via --trace or --observer)
#
# Pre-symbolicates our patched Wine PE/ELF binaries into Breakpad .sym files,
# verifies montauk is reachable, and toggles QUARK_TRACE=1 in env_config so
# launcher.rs spawns montauk as a sibling watcher for each game launch.
#
# When the user does NOT pass --observer, this step is a no-op (does not even
# touch env_config). The flag is sticky in env_config — once set on, future
# installs without the flag leave the existing setting alone.

def step_trace() -> StepResult:
    """Pre-symbolicate patched Wine DLLs and switch the montauk observer on.

    --observer is one switch; montauk is the user's to install. Quark only:
      1. Checks montauk is on PATH; without it the observer stays off and
         the install continues
      2. Names dump_syms's package when it is missing; symbolication is
         skipped without it
      3. Restores montauk's eBPF capabilities when a reinstall stripped them
      4. Symbolicates our patched Wine PE/ELF into Breakpad .sym files
      5. Sets QUARK_TRACE=1 in env_config so launcher.rs spawns montauk as
         a sibling watcher
    """
    t0 = time.monotonic_ns()

    if not _TRACE:
        return StepResult("Trace", True, skipped=True,
                          elapsed_ms=(time.monotonic_ns() - t0) // 1_000_000)

    artifacts: list[Path] = []

    # Quark and montauk are separate projects. Quark does NOT install or
    # build montauk. The user installs montauk themselves (once, via its
    # own install.py). --observer is the OPT-IN switch that tells quark:
    # "montauk is present, please hook into it." If either montauk or
    # dump_syms is missing, print the install hint and disable the feature
    # — the rest of quark continues fine.
    warnings: list[str] = []
    montauk = shutil.which("montauk")
    dump_syms = shutil.which("dump_syms")
    if not montauk:
        log_warn("Trace: `montauk` not on PATH; install it to use the observer. Observer disabled.")
        warnings.append("montauk missing")
        return StepResult("Trace", True, warnings=warnings, skipped=True,
                          elapsed_ms=(time.monotonic_ns() - t0) // 1_000_000)
    if not dump_syms:
        log_warn(f"Trace: `dump_syms` not on PATH. Install: {install_hint('dump_syms')}. "
                 "Continuing — observer will work, symbolication will be skipped.")
        warnings.append("dump_syms missing")

    # 1b. Ensure montauk has the eBPF trace capabilities. A montauk reinstall
    #     (plain cp) strips them, and the observer then silently produces empty
    #     header-only traces ("need root or CAP_BPF") — a recurring foot-gun.
    #     Idempotent: Only runs setcap (and prompts for sudo) when caps are
    #     actually missing, so it doesn't nag on every quark install.
    TRACE_CAPS = "cap_sys_admin,cap_bpf,cap_perfmon,cap_sys_resource,cap_dac_read_search+ep"
    _need = ["cap_bpf", "cap_perfmon", "cap_dac_read_search", "cap_sys_admin", "cap_sys_resource"]
    try:
        _have = subprocess.run(["getcap", montauk], capture_output=True, text=True).stdout
        if not all(c in _have for c in _need):
            if subprocess.run(["sudo", "setcap", TRACE_CAPS, montauk]).returncode == 0:
                log_info(f"Trace: applied eBPF trace caps to {montauk}")
            else:
                log_warn(f"Trace: setcap failed — run: sudo setcap {TRACE_CAPS} {montauk}")
        else:
            log_info("Trace: montauk trace caps already present")
    except FileNotFoundError:
        log_warn("Trace: getcap/setcap missing (install libcap); montauk --trace needs sudo")

    # 2. Symbolicate every patched PE/ELF we built. Breakpad .sym layout uses
    #    the module's build-id (first line of the .sym), so consumers (montauk,
    #    quark crash-digest, sentry symbolic) can index by build-id without us
    #    needing to construct the path.
    sym_dir = COMPAT_DIR / "symbols"
    sym_dir.mkdir(parents=True, exist_ok=True)

    # Targets are the *built-from-source* artifacts in wine-obj. System
    # Wine DLLs hardlinked in by step_deploy_wine are stripped and
    # dump_syms would emit empty .sym for them — skip those.
    candidates = [
        WINE_OBJ_DIR / "dlls/ntdll/x86_64-windows/ntdll.dll",
        WINE_OBJ_DIR / "dlls/ntdll/ntdll.so",
        WINE_OBJ_DIR / "dlls/winepulse.drv/winepulse.so",
        LSTEAMCLIENT_CACHE / "lsteamclient.dll",
        LSTEAMCLIENT_CACHE / "lsteamclient.so",
    ]
    rust_target = SCRIPT_DIR / "target" / "release"
    if (rust_target / "quark").exists():
        candidates.append(rust_target / "quark")

    produced = 0
    if not dump_syms:
        log_warn("Trace: skipping symbolication — dump_syms unavailable")
    for src in candidates:
        if not dump_syms or not src.exists():
            continue
        dst = sym_dir / f"{src.name}.sym"
        result = run_cmd([dump_syms, str(src)], desc=f"dump_syms {src.name}",
                         check=False, timeout=60)
        if result.returncode == 0 and result.stdout:
            dst.write_text(result.stdout)
            artifacts.append(dst)
            produced += 1
            log_debug(f"symbolicated {src.name} → {dst.name} "
                      f"({len(result.stdout) // 1024}K)")
        else:
            stderr_tail = (result.stderr or "").strip()[:80]
            log_debug(f"dump_syms skipped {src.name}: {stderr_tail or 'no debug info'}")
    log_info(f"Trace: symbolicated {produced} modules into {sym_dir}")

    # 3. Toggle QUARK_TRACE=1 in env_config. Sticky — present-and-set means
    #    launcher.rs reads it and spawns montauk; absent or '0' means skip.
    config_file = COMPAT_DIR / "env_config"
    if config_file.exists():
        text = config_file.read_text()
        changed = False
        if not _env_has_active_key(text, "QUARK_TRACE"):
            text = text.rstrip() + "\nQUARK_TRACE=1\n"
            changed = True
        # Full binary trace dump (montauk --trace-out) for deep ntsync-layer
        # diagnosis. Coupled to --observer; remove to silence the dump later.
        if not _env_has_active_key(text, "QUARK_TRACE_OUT"):
            text = text.rstrip() + "\nQUARK_TRACE_OUT=1\n"
            changed = True
        if changed:
            config_file.write_text(text)
            log_info("Trace: enabled QUARK_TRACE=1 + QUARK_TRACE_OUT=1 in env_config")
        else:
            log_info("Trace: QUARK_TRACE already configured in env_config")
    else:
        # env_config doesn't exist yet — step_finalize will create it from
        # the template. Touch a marker so finalize knows to fold the env var in.
        (COMPAT_DIR / ".trace-pending").write_text("1\n")

    elapsed = (time.monotonic_ns() - t0) // 1_000_000
    return StepResult("Trace", True, artifacts=artifacts,
                      warnings=warnings, elapsed_ms=elapsed)


# Step 9: GPU Translation

# (owner, repo, tag, asset, sha256, tree label, 64-bit dir, 32-bit dir, consequence)
PINNED_RELEASES = (
    ("doitsujin", "dxvk", "v3.1.1", "dxvk-3.1.1.tar.gz",
     "40565b4a724aadc4433fa4e010b4b23916d9b1f1baeee64e17186db94f54e608",
     "dxvk", "x64", "x32", "D3D9/11 games may fail"),
    ("HansKristian-Work", "vkd3d-proton", "v3.0.1", "vkd3d-proton-3.0.1.tar.zst",
     "3cf2315522af5e43605ef6d3c41dad91387040bf97199934f3f7ab76caaa2f0c",
     "vkd3d-proton", "x64", "x86", "D3D12 games may fail"),
    ("jp7677", "dxvk-nvapi", "v0.9.2", "dxvk-nvapi-v0.9.2.tar.gz",
     "60c284223530d643c446c263f1e1a96c6de7b5ff21796219646da734d97a70d6",
     "dxvk-nvapi", "x64", "x32", "NVAPI and DLSS will be unavailable on NVIDIA"),
)


def step_gpu_translation() -> StepResult:
    """Deploy DXVK, VKD3D-Proton and dxvk-nvapi from their pinned releases."""
    t0 = time.monotonic_ns()
    artifacts: list[Path] = []
    warnings: list[str] = []

    log_info("Fetching DXVK, VKD3D-Proton and dxvk-nvapi at their pinned releases...")
    lib_dir = COMPAT_DIR / "lib"
    for owner, repo, tag, asset, sha, label, d64, d32, consequence in PINNED_RELEASES:
        tar = _download_release(owner, repo, tag, asset, sha)
        if tar:
            artifacts.extend(_deploy_tarball_dlls(tar, lib_dir, label, d64, d32))
        else:
            warnings.append(f"{label} download failed — {consequence}")
            log_warn(f"{label}: download failed")

    elapsed = (time.monotonic_ns() - t0) // 1_000_000
    return StepResult("GPU translation", True, artifacts=artifacts,
                     warnings=warnings, elapsed_ms=elapsed)


def _download_release(owner: str, repo: str, tag: str, asset: str, sha256: str) -> Path | None:
    """Fetch one pinned release asset by tag and name, verified by sha256
    before it is used. A cached copy that matches is used as is; one that
    does not is deleted and fetched again. No release listing is consulted."""
    cache_dir = DATA_DIR / "downloads"
    cache_dir.mkdir(parents=True, exist_ok=True)
    dest = cache_dir / asset

    def digest(path: Path) -> str:
        h = hashlib.sha256()
        with open(path, "rb") as f:
            for chunk in iter(lambda: f.read(1 << 20), b""):
                h.update(chunk)
        return h.hexdigest()

    if dest.exists():
        if digest(dest) == sha256:
            log_info(f"{repo}: {tag} (cached, sha256 verified)")
            return dest
        log_warn(f"{repo}: cached {asset} does not match its pinned sha256; refetching")
        dest.unlink()

    url = f"https://github.com/{owner}/{repo}/releases/download/{tag}/{asset}"
    tmp = dest.with_suffix(dest.suffix + ".part")
    log_info(f"{repo}: downloading {tag}...")
    try:
        urllib.request.urlretrieve(url, tmp)
    except Exception as e:
        log_error(f"{repo}: download failed: {e}")
        tmp.unlink(missing_ok=True)
        return None
    got = digest(tmp)
    if got != sha256:
        log_error(f"{repo}: {asset} sha256 {got[:16]}... does not match the pin {sha256[:16]}...")
        tmp.unlink(missing_ok=True)
        return None
    tmp.rename(dest)
    log_info(f"{repo}: downloaded {asset}, sha256 verified")
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
    # dxvk-nvapi's tarball is rooted at ./ with x64 and x32 directly inside;
    # DXVK's and VKD3D's wrap everything in one versioned directory.
    root = staging if (staging / dir_64).is_dir() else extracted[0]

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
    wine_lib = COMPAT_DIR / "lib" / "wine"
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
            log_warn(f"{w}")

    elapsed = (time.monotonic_ns() - t0) // 1_000_000
    return StepResult("Deploy drivers", True, artifacts=artifacts,
                     warnings=warnings, elapsed_ms=elapsed)


# Step 11: EAC Runtime

def step_eac_runtime() -> StepResult:
    """Deploy EasyAntiCheat bridge DLLs (optional)."""
    t0 = time.monotonic_ns()

    if not EAC_RUNTIME_DIR.exists():
        log_warn("Proton EasyAntiCheat Runtime not found")
        log_warn("EAC games (Elden Ring, etc.) will not work")
        log_warn("Install via: Steam > Library > Tools > 'Proton EasyAntiCheat Runtime'")
        elapsed = (time.monotonic_ns() - t0) // 1_000_000
        return StepResult("EAC runtime", True, skipped=True,
                         warnings=["EAC runtime not installed"],
                         elapsed_ms=elapsed)

    dst_pe64 = COMPAT_DIR / "lib" / "wine" / "x86_64-windows"
    dst_so64 = COMPAT_DIR / "lib" / "wine" / "x86_64-unix"
    dst_pe32 = COMPAT_DIR / "lib" / "wine" / "i386-windows"
    dst_so32 = COMPAT_DIR / "lib" / "wine" / "i386-unix"
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

    compatdata = STEAMAPPS / "compatdata"
    if not compatdata.exists():
        return StepResult("Prefix sync", True, skipped=True,
                         elapsed_ms=(time.monotonic_ns() - t0) // 1_000_000)

    # Build the source map: What quark wants in every prefix's system32
    # Later entries override earlier (DXVK d3d11.dll wins over Wine's stub)
    sys_wine_64 = Path("/usr/lib/wine/x86_64-windows")
    sys_wine_32 = Path("/usr/lib/wine/i386-windows")
    quark_wine = COMPAT_DIR / "lib" / "wine"

    src_64: dict[str, Path] = {}
    for d in [sys_wine_64,
              quark_wine / "x86_64-windows",
              quark_wine / "dxvk" / "x86_64-windows",
              quark_wine / "vkd3d-proton" / "x86_64-windows"]:
        if d.exists():
            for f in d.iterdir():
                if f.is_file() and f.suffix in (".dll", ".exe", ".tlb", ".ocx", ".cpl", ".acm", ".ax", ".drv", ".sys"):
                    # lsteamclient.dll goes into the prefix like the launcher
                    # puts it there on every launch: build_module's LdrLoadDll
                    # finds it in system32 and the trampoline activates.
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
        if not (entry / "pfx/drive_c/windows/system32").exists():
            continue

        replaced = _sync_prefix(entry, src_64, src_32)
        if replaced:
            log_info(f"prefix {appid}: replaced {replaced} files")
            total_replaced += replaced
            total_prefixes += 1
        else:
            log_info(f"prefix {appid}: up to date")

    log_info(f"prefix sync: {total_replaced} files across {total_prefixes} prefix(es), {len(quark_appids)} mapped")
    return StepResult("Prefix sync", True,
                     elapsed_ms=(time.monotonic_ns() - t0) // 1_000_000)


def _sync_prefix(entry: Path, src_64: dict[str, Path], src_32: dict[str, Path]) -> int:
    """One quark prefix's system32 and syswow64 against the source maps:
    Every symlink is replaced with the file it stands for and every missing
    file copied in. Returns how many it wrote."""
    replaced = 0
    for sub, src_map in (("system32", src_64), ("syswow64", src_32)):
        win_dir = entry / "pfx/drive_c/windows" / sub
        if not win_dir.exists() or not src_map:
            continue
        for target in win_dir.iterdir():
            if target.is_symlink() and target.name in src_map:
                if _replace_file(src_map[target.name], target):
                    replaced += 1
        for name, src in src_map.items():
            target = win_dir / name
            if not target.exists() and not target.is_symlink():
                if _replace_file(src, target):
                    replaced += 1
    return replaced


# Step 13: Finalize

def step_finalize() -> StepResult:
    """Env config."""
    t0 = time.monotonic_ns()

    # Env config
    config_file = COMPAT_DIR / "env_config"
    if config_file.exists():
        # Lines quark wrote and has retired: WINE_NTSYNC=1, which no Wine
        # reads, and the #18 heap-hunt knobs, which stock Wine 11.20 runs
        # clean without.
        retired = {"WINE_NTSYNC=1", "GLIBC_TUNABLES=glibc.malloc.arena_max=1",
                   "MALLOC_ARENA_MAX=1", "MONTAUK_HEAP_STACK_SIZE=288"}
        lines = config_file.read_text().splitlines(keepends=True)
        kept = [line for line in lines if line.strip() not in retired]
        if len(kept) != len(lines):
            config_file.write_text("".join(kept))
            log_info(f"Env config: removed {len(lines) - len(kept)} retired line(s)")
    else:
        config_file.write_text(ENV_CONFIG_TEMPLATE)
    # If step_trace ran before env_config existed, fold its toggle in now.
    pending = COMPAT_DIR / ".trace-pending"
    if pending.exists():
        text = config_file.read_text()
        changed = False
        if not _env_has_active_key(text, "QUARK_TRACE"):
            text = text.rstrip() + "\nQUARK_TRACE=1\n"
            changed = True
        if not _env_has_active_key(text, "QUARK_TRACE_OUT"):
            text = text.rstrip() + "\nQUARK_TRACE_OUT=1\n"
            changed = True
        if changed:
            config_file.write_text(text)
            log_info("Trace: folded QUARK_TRACE=1 + QUARK_TRACE_OUT=1 into newly-created env_config")
        pending.unlink()
    log_info(f"Env config: {config_file}")

    elapsed = (time.monotonic_ns() - t0) // 1_000_000
    return StepResult("Finalize", True, elapsed_ms=elapsed)


# Step 14: Smoke test
#
# Spawn the deployed compat tool's own bin/wine exactly as the launcher does,
# ask it for `--version`, and verify it returns the system Wine version with
# the tree's ntdll.so loaded. This catches "deployed but broken" before Steam
# ever launches a game: Missing files in the compat tree, version drift between
# the patched DLLs and the system binaries, a symlinked loader, all surface here.

def step_smoke_test() -> StepResult:
    """Run the deployed tree's own wine --version and prove it loaded the
    tree's ntdll.so, not the system's. bin/wine is a copy of the system
    loader, so Wine resolves lib/wine relative to /proc/self/exe; the
    dynamic linker's own trace (LD_DEBUG=libs) names the ntdll.so it found."""
    t0 = time.monotonic_ns()
    warnings: list[str] = []

    quark_lib_wine = COMPAT_DIR / "lib" / "wine"
    quark_share_wine = COMPAT_DIR / "share" / "wine"
    wine_bin = COMPAT_DIR / "bin" / "wine"

    if not wine_bin.exists():
        raise StepFailed("smoke test", f"compat wine binary missing: {wine_bin}")
    if wine_bin.is_symlink():
        raise StepFailed("smoke test", f"{wine_bin} is a symlink; /proc/self/exe would resolve "
                         "to the system loader and the tree would not be used")
    if not quark_lib_wine.exists():
        raise StepFailed("smoke test", f"compat lib/wine missing: {quark_lib_wine}")
    if not quark_share_wine.exists():
        raise StepFailed("smoke test", f"compat share/wine missing: {quark_share_wine}")

    env = dict(os.environ, LD_DEBUG="libs")
    try:
        r = subprocess.run([str(wine_bin), "--version"], capture_output=True, text=True,
                           timeout=15, env=env)
    except subprocess.TimeoutExpired:
        raise StepFailed("smoke test", "wine --version timed out (15s)")
    except FileNotFoundError as e:
        raise StepFailed("smoke test", f"failed to spawn wine: {e}")

    if r.returncode != 0:
        for line in (r.stderr or "").strip().splitlines()[-10:]:
            log_error(f"wine --version: {line}")
        raise StepFailed("smoke test", f"wine --version exited {r.returncode}")

    reported = (r.stdout or "").strip()
    if not reported.startswith("wine-"):
        raise StepFailed("smoke test", f"unexpected wine --version output: {reported!r}")

    # The loader reports the path as it built it, bin/../lib/wine/..., so
    # compare resolved paths rather than strings.
    tree_ntdll = (quark_lib_wine / "x86_64-unix" / "ntdll.so").resolve()
    loaded = [l for l in (r.stderr or "").splitlines() if "ntdll.so" in l and "calling init" in l]
    loaded_paths = [Path(l.split("calling init:", 1)[1].strip()).resolve() for l in loaded if "calling init:" in l]
    if tree_ntdll not in loaded_paths:
        raise StepFailed("smoke test", "the loader did not take the tree's ntdll.so: "
                         + (str(loaded_paths[-1]) if loaded_paths else "no ntdll.so init seen"))

    expected = get_system_wine_version()
    if expected and reported != expected:
        # The binary is the system loader and the modules are the tree's; if
        # they disagree the tree holds files from a different build.
        warnings.append(
            f"compat wine reports {reported} but system wine is {expected} — "
            "compat tree may be ABI-mixed; re-run install.py"
        )
        log_warn(f"{warnings[-1]}")

    log_info(f"smoke test: {wine_bin.name} reports {reported}, ntdll.so from the tree")
    elapsed = (time.monotonic_ns() - t0) // 1_000_000
    return StepResult("Smoke test", True, warnings=warnings, elapsed_ms=elapsed)


# Verbose config

def configure_verbose(enable: bool | None) -> None:
    """Handle verbose flag. Sticky: stays enabled until explicitly disabled.

    The flag lives in CACHE_DIR (~/.cache/quark/) — NOT in COMPAT_DIR —
    because step_clean wipes the compat dir when a clean is accepted. Putting the flag
    there meant `./install.py --verbose` wrote the flag, then step_clean
    deleted it five seconds later, and the launcher always saw verbose=false
    at game launch time. Moving it to CACHE_DIR makes it survive a clean.
    The launcher reads the same path via quark::log::is_verbose().
    """
    flag = CACHE_DIR / "trace_verbose"
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
    parser.add_argument("--verbose", action="store_true",
                        help="Turn on the launcher's runtime diagnostics (sticky, read at game time)")
    parser.add_argument("--no-verbose", action="store_true", help="Turn the launcher's runtime diagnostics off")
    parser.add_argument("-v", dest="debug", action="store_true", help="DEBUG lines on the terminal too")
    parser.add_argument("-q", dest="quiet", action="store_true", help="only WARN and ERROR on the terminal")
    parser.add_argument("--yes", "-y", action="store_true",
                        help="Auto-answer Y to all prompts (non-interactive)")
    parser.add_argument("--wipe-prefixes", action="store_true",
                        help="With the clean step, also delete the Wine prefixes and "
                             "shader caches of quark-mapped games (local-only saves "
                             "in those prefixes are lost)")
    parser.add_argument("--observer", action="store_true",
                        help="Opt in to the montauk observer integration: pre-symbolicate "
                             "patched Wine PE DLLs to Breakpad .sym, sets QUARK_TRACE=1 "
                             "in env_config so launcher.rs spawns montauk as a sibling "
                             "watcher for each game. Requires `dump_syms` and `montauk` "
                             "on PATH (hints printed if missing — install is non-fatal).")
    parser.add_argument("--trace", action="store_true",
                        help="The one diagnosis switch. Full game + wineserver montauk "
                             "capture with verbose logging — exactly what `--observer "
                             "--verbose` did, under one name. Implies --observer "
                             "(QUARK_TRACE=1 + QUARK_TRACE_OUT=1, eBPF caps, "
                             "montauk reachability) AND --verbose.")
    args = parser.parse_args()
    install_exit_guard()
    log.start("install", quiet=args.quiet, verbose=args.debug)
    log_intro("install")

    if args.yes:
        global _AUTO_YES
        _AUTO_YES = True

    if args.wipe_prefixes:
        global _WIPE_PREFIXES
        _WIPE_PREFIXES = True

    # --trace is the unified diagnosis switch: It IS --observer + --verbose under one
    # name (montauk is the observer; --trace is the interface). The component flags
    # still work for granular use, but --trace is the canonical "watch everything".
    if args.observer or args.trace:
        global _TRACE
        _TRACE = True

    if args.verbose or args.trace:
        configure_verbose(True)
    elif args.no_verbose:
        configure_verbose(False)
    else:
        configure_verbose(None)

    wine_src_head = _git_head(WINE_SRC_DIR)
    unix = COMPAT_DIR / "lib" / "wine" / "x86_64-unix"
    win64 = COMPAT_DIR / "lib" / "wine" / "x86_64-windows"
    proton_src_dirs = sorted(p for p in (CACHE_DIR / "proton-source").glob("proton-*") if p.is_dir())
    proton_src = proton_src_dirs[-1] if proton_src_dirs else CACHE_DIR / "proton-source" / "none"
    wine_patch_files = _patched_files(SCRIPT_DIR / "patches" / "wine", WINE_SRC_DIR)
    bridge_patch_files = _patched_files(SCRIPT_DIR / "patches" / "lsteamclient", proton_src)
    steps = [
        Step("Preflight",        step_preflight, gate=True),
        Step("Clean",            step_clean, needs=("Preflight",)),
        # Sync resets the tree to the installed Wine's tag, undoing the
        # patches. Its outputs are the clone's identity, which Patch Wine
        # never writes, so the two cannot invalidate each other in a loop.
        Step("Sync wine source", step_sync_wine_source, needs=("Clean",),
             inputs=(get_system_wine_version,),
             outputs=(WINE_SRC_DIR / ".git" / "HEAD", WINE_SRC_DIR / "server" / "protocol.def")),
        Step("Build",            step_build, needs=("Clean",), gate=True),
        Step("Deploy binaries",  step_deploy_binaries, needs=("Build",)),
        # Deploy Wine rewrites the whole tree. Its declared outputs are files
        # no later step overlays; the overlaid ones (ntdll, winepulse, the
        # bridge) belong to their producers, whose traces see the rewrite.
        Step("Deploy Wine",      step_deploy_wine, needs=("Clean",),
             inputs=(Path("/usr/lib/wine"), Path("/usr/share/wine"), Path("/usr/bin/wine"),
                     Path("/usr/bin/wineserver")),
             outputs=(COMPAT_DIR / "bin" / "wine", unix / "win32u.so", win64 / "kernel32.dll",
                      COMPAT_DIR / "share" / "wine" / "wine.inf")),
        Step("Patch Wine",       step_patch_wine, needs=("Sync wine source", "Deploy Wine"),
             inputs=(wine_src_head, SCRIPT_DIR / "patches" / "wine", Path("/usr/bin/wine"),
                     Path("/usr/lib/wine/x86_64-unix/ntdll.so")),
             outputs=(WINE_OBJ_DIR / "dlls" / "ntdll" / "ntdll.so",
                      WINE_OBJ_DIR / "dlls" / "ntdll" / "x86_64-windows" / "ntdll.dll",
                      WINE_OBJ_DIR / "dlls" / "winepulse.drv" / "winepulse.so",
                      unix / "ntdll.so", win64 / "ntdll.dll", unix / "winepulse.so")
                     + wine_patch_files),
        Step("Build stubs",      step_build_stubs, needs=("Deploy Wine",)),
        Step("Bake prefix",      step_bake_prefix_template, needs=("Patch Wine", "Build stubs")),
        # The bridge builds in its own worktree of wine-src, whose symlinks,
        # configure.ac hunks and autoconf Patch Wine's tree never sees, so the
        # two run side by side. The Proton sources are an input by directory
        # identity only, since the bridge patches their contents and declares
        # those as outputs.
        Step("Steam bridge",     step_steam_bridge, needs=("Sync wine source", "Deploy Wine"),
             inputs=(wine_src_head, SCRIPT_DIR / "patches" / "lsteamclient",
                     _dir_listing(CACHE_DIR / "proton-source")),
             outputs=(LSTEAMCLIENT_CACHE / "lsteamclient.dll", LSTEAMCLIENT_CACHE / "lsteamclient.so",
                      LSTEAMCLIENT_CACHE / "steam.exe", unix / "lsteamclient.so",
                      win64 / "lsteamclient.dll", WINE_BRIDGE_SRC / "configure.ac")
                     + bridge_patch_files),
        Step("Trace",            step_trace, needs=("Patch Wine", "Steam bridge")),
        Step("GPU translation",  step_gpu_translation, needs=("Deploy Wine",)),
        Step("Deploy drivers",   step_deploy_drivers, needs=("Deploy Wine",)),
        Step("EAC runtime",      step_eac_runtime, needs=("Deploy Wine",)),
        Step("Prefix sync",      step_prefix_sync,
             needs=("Bake prefix", "Steam bridge", "Trace", "GPU translation",
                    "Deploy drivers", "EAC runtime", "Deploy binaries")),
        Step("Finalize",         step_finalize, needs=("Prefix sync",)),
        Step("Smoke test",       step_smoke_test, needs=("Finalize",)),
    ]

    # Snapshot system Wine before any step runs. The detective half of the
    # write guardrail: If anything in /usr/lib/wine or /usr/share/wine differs
    # after the pipeline finishes, install.py poisoned the system and we abort.
    log_info("Snapshotting system Wine state for poisoning audit...")
    system_wine_snapshot = snapshot_system_wine()
    log_info(f"{len(system_wine_snapshot)} system Wine files tracked")

    _prefixed_loggers()
    t_graph = time.monotonic()
    results, rc = run_graph(steps, workers=min(4, cpu_count()))
    graph_s = time.monotonic() - t_graph
    if rc:
        _print_manifest(results, graph_s)
        return rc

    # System Wine poisoning audit. Runs even if some steps failed — a partial
    # install that touched /usr is still a poisoning event we need to know about.
    log_section("system Wine", "audit")
    try:
        verify_system_wine_unchanged(system_wine_snapshot)
        log_info("system Wine untouched — clean install")
    except StepFailed as e:
        log_error(f"POISONING DETECTED: {e.detail}")
        results.append(StepResult("System Wine audit", False))
        _print_manifest(results, graph_s)
        return 2

    # Deployment manifest
    log_blank()
    _print_manifest(results, graph_s)

    failed = sum(1 for r in results if not r.success)
    if failed:
        log_warn(f"{failed} step(s) failed — check errors above")
        return 1

    log_info("Save data protection: enabled (automatic pre-launch snapshots)")
    return 0


def _print_manifest(results: list[StepResult], graph_s: float) -> None:
    version = get_version()
    failed = [r for r in results if not r.success]

    log_info(f"DEPLOYMENT COMPLETE: quark {version}, the step graph in {graph_s:.1f} s")

    # Only list what was deployed, with counts and each step's time
    for r in results:
        if r.skipped:
            continue
        took = f"{r.elapsed_ms / 1000:.1f} s"
        if not r.success:
            log_error(f"{r.name}: FAILED after {took}")
        elif r.artifacts:
            log_info(f"{r.name}: {len(r.artifacts)} files in {took}")
        else:
            log_info(f"{r.name}: done in {took}")

    for r in results:
        for w in r.warnings:
            log_warn(w)

    if failed:
        log_error(f"{len(failed)} step(s) failed")


if __name__ == "__main__":
    try:
        rc = main()
    except KeyboardInterrupt:
        log_interrupted()
        rc = 130
    log_finish()
    sys.exit(rc)
