"""Shared by install.py and the tests/ suite: the paths, the logger, Steam
discovery and Wine process hygiene. One definition each; nothing here is
reimplemented elsewhere."""

from __future__ import annotations

import atexit
import os
import pwd
import re
import shutil
import signal
import socket
import subprocess
import time
import unittest
from datetime import datetime
from pathlib import Path


# PATHS

def _user_home() -> Path:
    """The invoking user's home, also under sudo."""
    sudo_user = os.environ.get("SUDO_USER")
    if sudo_user:
        return Path(pwd.getpwnam(sudo_user).pw_dir)
    return Path.home()


REPO_ROOT = Path(__file__).resolve().parent
RUST_DIR = REPO_ROOT / "rust"
QUARK_BIN = REPO_ROOT / "target" / "release" / "quark"

USER_HOME = _user_home()
STEAM_ROOT = USER_HOME / ".local" / "share" / "Steam"
STEAMAPPS = STEAM_ROOT / "steamapps"
COMPAT_DIR = STEAM_ROOT / "compatibilitytools.d" / "quark"

DATA_DIR = USER_HOME / ".local" / "share" / "quark"
CACHE_DIR = USER_HOME / ".cache" / "quark"
LSTEAMCLIENT_CACHE = DATA_DIR / "lsteamclient"
LOG_DIR = Path("/tmp/quark")  # the launcher's log::LOG_DIR
WINE_SRC_DIR = CACHE_DIR / "wine-src"
WINE_OBJ_DIR = CACHE_DIR / "wine-obj"
MONTAUK_SRC = REPO_ROOT.parent / "montauk"  # quark's sibling in the monorepo

# Every command's log, and the QEMU suite's own outputs (probes, verdicts,
# analyses, pulled-back guest runs), apart from LOG_DIR, which install.py's
# clean step removes.
SUITE_DIR = Path("/tmp/quark-suite")

# The QEMU suite's disks. QEMU_READY exists only after a provision finished;
# the image file alone exists from the first moment a provision starts.
QEMU_CACHE = USER_HOME / ".cache" / "quark-qemu"
QEMU_READY = QEMU_CACHE / "provisioned.ok"


# LOGGING
# Every line is `[HH:MM:SS] [LEVEL]   message` (tests/TERMINAL_STYLE.md); the
# pad after the level tag keeps the message column identical for every level,
# and leading whitespace a caller typed is stripped, so a message is flat.

_LEVELS = {"DEBUG": 10, "INFO": 20, "WARN": 30, "ERROR": 40}
_TAG_PAD = {"INFO": "   ", "WARN": "   ", "ERROR": "  ", "DEBUG": "  "}


def _timestamp() -> str:
    return datetime.now().strftime("[%H:%M:%S]")


class _Log:
    """The one logger. Every line goes to the command's log file once one is
    open, and to the terminal at or above the floor (INFO; -v lowers it to
    DEBUG, -q raises it to WARN). A child (QUARK_CHILD=1) inherits its
    parent's log file and floor through the environment, so a suite run is
    one log on disk as well as one progress log on the terminal."""

    def __init__(self):
        self.floor = _LEVELS.get(os.environ.get("QUARK_FLOOR", "INFO"), _LEVELS["INFO"])
        self.child = os.environ.get("QUARK_CHILD") == "1"
        self.path: Path | None = None
        self._file = None
        if os.environ.get("QUARK_LOG"):
            self._open(Path(os.environ["QUARK_LOG"]))

    def _open(self, path: Path) -> None:
        path.parent.mkdir(parents=True, exist_ok=True)
        self._file = open(path, "a", buffering=1)
        self.path = path

    def start(self, command: str, quiet: bool = False, verbose: bool = False) -> None:
        """Set the terminal floor and open `command`'s log under SUITE_DIR;
        a child keeps its parent's of both."""
        if self.child:
            return
        level = "WARN" if quiet else "DEBUG" if verbose else "INFO"
        self.floor = _LEVELS[level]
        os.environ["QUARK_FLOOR"] = level
        self._open(SUITE_DIR / f"{command}-{datetime.now():%Y%m%d-%H%M%S}.log")
        os.environ["QUARK_LOG"] = str(self.path)

    def emit(self, level: str, msg) -> None:
        line = f"{_timestamp()} [{level}]{_TAG_PAD[level]}{str(msg).lstrip()}"
        if self._file:
            self._file.write(line + "\n")
        if _LEVELS[level] >= self.floor:
            print(line, flush=True)

    def raw(self, text: str = "") -> None:
        """A blank or a table row, verbatim: Shown with INFO, kept in the log."""
        if self._file:
            self._file.write(text + "\n")
        if self.floor <= _LEVELS["INFO"]:
            print(text, flush=True)


log = _Log()


def log_info(msg: str) -> None:
    log.emit("INFO", msg)


def log_warn(msg: str) -> None:
    log.emit("WARN", msg)


def log_error(msg: str) -> None:
    log.emit("ERROR", msg)


def log_debug(msg: str) -> None:
    log.emit("DEBUG", msg)


def log_blank() -> None:
    """The one separator: A blank line, owned by the logger."""
    log.raw()


def log_interrupted() -> None:
    """The one Ctrl+C line, everywhere. A child's goes to the log only,
    since its parent says it on the terminal."""
    (log_debug if log.child else log_warn)("interrupted: Cleaning up")


def get_version() -> str:
    """quark's version, from rust/Cargo.toml."""
    for line in (RUST_DIR / "Cargo.toml").read_text().splitlines():
        if line.startswith("version"):
            return line.split('"')[1]
    return "0.0.0"


def git_describe() -> str:
    """`<commit>` or `<commit>-dirty` for the quark tree, `unknown` outside a
    checkout. --no-optional-locks keeps git from writing the index, which
    another process on the box may hold."""
    try:
        head = subprocess.run(["git", "-C", str(REPO_ROOT), "rev-parse", "--short", "HEAD"],
                              capture_output=True, text=True, timeout=10)
        if head.returncode != 0:
            return "unknown"
        changed = subprocess.run(["git", "--no-optional-locks", "-C", str(REPO_ROOT), "status",
                                  "--porcelain", "--untracked-files=no", "--", "."],
                                 capture_output=True, text=True, timeout=60).stdout.strip()
        return head.stdout.strip() + ("-dirty" if changed else "")
    except (OSError, subprocess.TimeoutExpired):
        return "unknown"


def log_intro(command: str, mode: str = "") -> None:
    """A command's first line: `quark <command> v<version> [<commit>] <mode>`.
    A child prints none."""
    if log.child:
        return
    log_info(f"quark {command} v{get_version()} [{git_describe()}] {mode}".rstrip())


def log_finish() -> None:
    """A command's last line: Where its log landed. A child's parent says it."""
    if not log.child and log.path:
        log_info(f"LOG: {log.path}")


def log_section(name: str, verb: str) -> None:
    """A phase: One blank line, then `<Name>: <VERB>`."""
    log_blank()
    log_info(f"{name}: {verb.upper()}")


class Checks:
    """One layer's checks. Each prints a check line whose level is its
    verdict (INFO pass, ERROR fail, WARN skip); summary() prints the one
    counting status line and returns the exit code."""

    def __init__(self, name: str):
        self.name = name
        self.passed = self.failed = self.skipped = 0

    def ok(self, msg: str) -> None:
        self.passed += 1
        log_info(msg)

    def fail(self, msg: str, reason: str = "") -> None:
        self.failed += 1
        log_error(f"{msg}: {reason}" if reason else msg)

    def skip(self, msg: str, reason: str = "") -> None:
        self.skipped += 1
        log_warn(f"{msg}: {reason}" if reason else msg)

    def check(self, condition: bool, pass_msg: str, fail_msg: str) -> bool:
        (self.ok if condition else self.fail)(pass_msg if condition else fail_msg)
        return condition

    def summary(self) -> int:
        line = f"{self.name.upper()}: {self.passed} passed, {self.failed} failed, {self.skipped} skipped"
        log_blank()
        (log_error if self.failed else log_info)(line)
        return 1 if self.failed else 0


def run_unittests(name: str, *cases) -> int:
    """Run unittest TestCases as one layer's checks: Each test is a check
    line labelled by its docstring, a failure's last traceback line is the
    ERROR and the whole traceback goes to DEBUG."""
    checks = Checks(name)

    def label(test) -> str:
        return test.shortDescription() or test._testMethodName.replace("_", " ")

    class Result(unittest.TestResult):
        def addSuccess(self, test):
            super().addSuccess(test)
            checks.ok(label(test))

        def _failed(self, test, err):
            trace = self._exc_info_to_string(err, test).rstrip().splitlines()
            checks.fail(label(test), trace[-1] if trace else "failed")
            for line in trace:
                log_debug(line)

        def addFailure(self, test, err):
            super().addFailure(test, err)
            self._failed(test, err)

        def addError(self, test, err):
            super().addError(test, err)
            self._failed(test, err)

        def addSkip(self, test, reason):
            super().addSkip(test, reason)
            checks.skip(label(test), reason)

        def addUnexpectedSuccess(self, test):
            super().addUnexpectedSuccess(test)
            checks.fail(label(test), "passed but was expected to fail")

    loader = unittest.TestLoader()
    unittest.TestSuite(loader.loadTestsFromTestCase(c) for c in cases).run(Result())
    return checks.summary()


_CARGO_TEST = re.compile(r"^test (\S+) \.\.\. (ok|FAILED|ignored)")
_CARGO_BLOCK = re.compile(r"^---- (\S+) stdout ----$")


def _cargo_failures(stdout: list[str]) -> dict[str, list[str]]:
    """Each failed test's captured output, from cargo's `---- name stdout ----`
    blocks, which run until the next block or the closing `failures:` list."""
    blocks: dict[str, list[str]] = {}
    current = None
    for line in stdout:
        m = _CARGO_BLOCK.match(line)
        if m:
            current = m.group(1)
            blocks[current] = []
        elif line == "failures:" and current:
            current = None
        elif current:
            blocks[current].append(line)
    return blocks


def _panic_message(block: list[str]) -> str:
    """The line after `panicked at`, which is the assertion's own message."""
    for i, line in enumerate(block):
        if "panicked at" in line:
            rest = [l.strip() for l in block[i + 1:] if l.strip()]
            return rest[0][:200] if rest else line.strip()[:200]
    return "failed"


def run_cargo_tests(name: str, args: list[str], cwd: Path) -> int:
    """`cargo test` as one layer's checks: Each test is a check line, a
    failure carries its panic message, its captured output goes to DEBUG,
    and a build that fails before any test runs is one failed check."""
    checks = Checks(name)
    r = subprocess.run(["cargo", "test", *args], cwd=cwd, capture_output=True, text=True)
    stdout = r.stdout.splitlines()
    failures = _cargo_failures(stdout)
    for line in stdout:
        m = _CARGO_TEST.match(line)
        if not m:
            continue
        test, verdict = m.groups()
        if verdict == "ok":
            checks.ok(test)
        elif verdict == "ignored":
            checks.skip(test, "ignored")
        else:
            checks.fail(test, _panic_message(failures.get(test, [])))
            for out in failures.get(test, []):
                if out.strip():
                    log_debug(out)
    if r.returncode != 0 and not checks.failed:
        # The first error is the cause; cargo's last is "could not compile".
        errors = [l for l in r.stderr.splitlines() if l.startswith("error")]
        checks.fail("cargo test", errors[0] if errors else f"exit {r.returncode}")
        for line in r.stderr.splitlines()[-40:]:
            if line.strip():
                log_debug(line)
    return checks.summary()


# TABLES
# A header row of plain column names, then aligned rows: The label column
# left-justified, the rest right-justified, no separator row. Printed
# verbatim through log_table, framed by one blank line.

LABEL_W = 28
COL_W = 10


def _table_widths(n: int, col_w) -> list[int]:
    """One width for every column, or one per column, recycled if short."""
    if isinstance(col_w, int):
        return [col_w] * n
    widths = list(col_w)
    return [widths[i % len(widths)] for i in range(n)]


def table_header(label: str, columns: list[str], label_w: int = LABEL_W, col_w=COL_W) -> str:
    ws = _table_widths(len(columns), col_w)
    return f"{label:<{label_w}}" + "".join(f" {c:>{w}}" for c, w in zip(columns, ws))


def table_row(label: str, cells: list[str], label_w: int = LABEL_W, col_w=COL_W) -> str:
    """`cells` are pre-formatted: The caller owns precision and units."""
    ws = _table_widths(len(cells), col_w)
    return f"{label:<{label_w}}" + "".join(f" {c:>{w}}" for c, w in zip(cells, ws))


def log_table(lines: list[str]) -> None:
    log_blank()
    for line in lines:
        log.raw(line)
    log_blank()


# EXIT GUARD
# The one interrupt and exit path. SIGTERM raises KeyboardInterrupt, as
# SIGINT already does, so every `finally:` in the suite (the QEMU teardown,
# the scoreboard's scheduler restore) runs on both; the cleanups registered
# here run once at exit, last registered first.

_EXIT_CLEANUPS: list = []
_GUARD_INSTALLED = [False]


def register_exit_cleanup(fn) -> None:
    if fn not in _EXIT_CLEANUPS:
        _EXIT_CLEANUPS.append(fn)


def _run_exit_cleanups() -> None:
    while _EXIT_CLEANUPS:
        fn = _EXIT_CLEANUPS.pop()
        try:
            fn()
        except Exception as e:
            log_warn(f"cleanup {getattr(fn, '__name__', fn)}: {e}")


def _sigterm(_signum, _frame) -> None:
    raise KeyboardInterrupt


def install_exit_guard() -> None:
    """Idempotent: Call once from an entry point's main."""
    if _GUARD_INSTALLED[0]:
        return
    _GUARD_INSTALLED[0] = True
    signal.signal(signal.SIGTERM, _sigterm)
    atexit.register(_run_exit_cleanups)


# PROMETHEUS
# One emitter for every .prom the suite writes. Names are
# quark_<bench>_<metric>; exactly one quark_<bench>_info{version,git_commit,
# git_dirty} gauge and one quark_<bench>_timestamp_seconds gauge carry the
# metadata, never repeated per sample; labels are lowercase and quoted; HELP
# and TYPE come once per metric name, before its first sample.

class PrometheusBuilder:
    """    pb = PrometheusBuilder("scoreboard")
        pb.info(version=v, git_commit=c, git_dirty=d)
        pb.gauge("exec_ms", 3573.8, help="launch to exec", labels={"game": "2379780"})
        path.write_text(pb.render())"""

    def __init__(self, bench: str):
        self.prefix = f"quark_{bench}"
        self._declared: set[str] = set()
        self._lines: list[str] = []

    @staticmethod
    def _labels(labels: dict | None) -> str:
        if not labels:
            return ""
        return "{" + ",".join(f'{k}="{v}"' for k, v in labels.items()) + "}"

    def _declare(self, name: str, help_text: str, mtype: str) -> None:
        if name in self._declared:
            return
        self._declared.add(name)
        if help_text:
            self._lines.append(f"# HELP {name} {help_text}")
        self._lines.append(f"# TYPE {name} {mtype}")

    def gauge(self, metric: str, value, help: str = "", labels: dict | None = None) -> None:
        name = f"{self.prefix}_{metric}"
        self._declare(name, help, "gauge")
        self._lines.append(f"{name}{self._labels(labels)} {value}")

    def info(self, ts: int | None = None, **labels) -> None:
        """The metadata gauge and the run's timestamp; call once, first."""
        if "git_dirty" in labels:
            labels["git_dirty"] = 1 if labels["git_dirty"] else 0
        self.gauge("info", 1, help="build and run metadata (value always 1)", labels=labels)
        self.gauge("timestamp_seconds", int(ts if ts is not None else time.time()),
                   help="unix time of the run")

    def render(self) -> str:
        return "\n".join(self._lines) + "\n"


# STEAM

def get_quark_appids() -> set[str]:
    """Appids Steam's config.vdf maps to the quark compat tool."""
    config_vdf = STEAM_ROOT / "config" / "config.vdf"
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
        return set(re.findall(r'"(\d+)"\s*\{[^}]*"name"\s*"quark"', block))
    except (OSError, ValueError):
        return set()


# WINE PROCESS HYGIENE
# Scoped by prefix or by liveness, never by process name: A bare
# `pkill wineserver` or a sweep of every socket takes down other Wine and
# Proton sessions.

def stop_prefix_wineserver(pfx: Path) -> None:
    """Stop the wineserver serving `pfx`, and nothing else."""
    wineserver = shutil.which("wineserver")
    if not wineserver:
        return
    try:
        subprocess.run([wineserver, "-k"], env={**os.environ, "WINEPREFIX": str(pfx)},
                       capture_output=True, timeout=10)
    except subprocess.TimeoutExpired:
        log_warn(f"wineserver -k timed out for {pfx}")


def stop_observer(trace: Path) -> None:
    """SIGINT the launcher's montauk observer writing `trace` so it flushes
    and closes the capture; it is matched by that path, which only one
    launch's observer has."""
    needle = str(trace).encode()
    for pid in filter(str.isdigit, os.listdir("/proc")):
        try:
            if needle not in Path(f"/proc/{pid}/cmdline").read_bytes():
                continue
            os.kill(int(pid), signal.SIGINT)
        except OSError:
            continue
        for _ in range(100):
            if not Path(f"/proc/{pid}").exists():
                break
            time.sleep(0.1)


def stale_wine_sockets() -> list[Path]:
    """Wineserver sockets nothing answers on. A live server, quark's or any
    other Wine/Proton session's, accepts the connect and is left alone."""
    wine_tmp = Path(f"/tmp/.wine-{os.getuid()}")
    if not wine_tmp.exists():
        return []
    stale = []
    for sock_path in wine_tmp.glob("server-*/socket"):
        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as s:
            try:
                s.connect(str(sock_path))
            except OSError:
                stale.append(sock_path)
    return stale


def remove_stale_wine_sockets() -> None:
    """Remove each stale socket and its lock. The server directory stays:
    Wine expects it, and recreating it races with a starting server's bind."""
    for sock in stale_wine_sockets():
        sock.unlink(missing_ok=True)
        (sock.parent / "lock").unlink(missing_ok=True)


# STEAM GAME DISCOVERY
#
# One definition of which installed Steam games are launchable and which
# exe is the game, for the harness and the scoreboard.

EXE_BLACKLIST = {
    "unitycrashhandler64.exe", "unitycrashhandler32.exe",
    "crashreport.exe", "crashhandler.exe", "crashpad_handler.exe",
    "ue4prereqsetup_x64.exe", "installermessage.exe",
    "dxsetup.exe", "vcredist_x64.exe", "vcredist_x86.exe",
    "dukeworkshopuploader.exe",
    "dotnetfx35.exe", "dotnetfx35setup.exe",
    "beservice.exe", "beservice_x64.exe",
    "easyanticheat_setup.exe", "easyanticheat.exe",
    "replication-server.exe",
}


def parse_appmanifest(path: Path) -> dict:
    text = path.read_text(errors="replace")
    result = {}
    for key in ("appid", "name", "installdir"):
        m = re.search(rf'"{key}"\s+"([^"]+)"', text)
        if m:
            result[key] = m.group(1)
    return result


def find_game_exe(install_dir: Path) -> str:
    if not install_dir.exists():
        return ""
    candidates = []
    for exe in install_dir.rglob("*.exe"):
        try:
            rel = exe.relative_to(install_dir)
            if len(rel.parts) > 3:
                continue
        except ValueError:
            continue
        if exe.name.lower() in EXE_BLACKLIST:
            continue
        parts_lower = [p.lower() for p in rel.parts]
        if any(skip in parts_lower for skip in
               ["easyanticheat", "battleye", "_commonredist", "directx", "redist"]):
            continue
        candidates.append((exe.stat().st_size, exe))
    if not candidates:
        return ""
    candidates.sort(key=lambda x: -x[0])
    return str(candidates[0][1])


def find_game(app_id: str) -> tuple:
    """Returns (name, exe_path) for an app ID."""
    for manifest in STEAMAPPS.glob("appmanifest_*.acf"):
        info = parse_appmanifest(manifest)
        if info.get("appid") == app_id:
            install_dir = STEAMAPPS / "common" / info.get("installdir", "")
            exe = find_game_exe(install_dir)
            return (info.get("name", "?"), exe)
    return ("?", "")


def discover_all_games() -> list[tuple[str, str, str]]:
    """Find all playable Steam games. Returns [(appid, name, exe_path), ...]."""
    _skip_appids = {
        "228980", "1493710", "1887720", "1826330", "2180100",
        "961940", "1391110", "1580130", "2348590",
    }
    _skip_names = {"proton", "steamworks", "steam linux runtime", "redistribut", "sdk"}
    games = []
    seen = set()
    for manifest in sorted(STEAMAPPS.glob("appmanifest_*.acf")):
        info = parse_appmanifest(manifest)
        appid = info.get("appid", "")
        name = info.get("name", "")
        installdir = info.get("installdir", "")
        if not appid or not name or not installdir:
            continue
        if appid in seen or appid in _skip_appids:
            continue
        if any(s in name.lower() for s in _skip_names):
            continue
        install_dir = STEAMAPPS / "common" / installdir
        if not install_dir.exists():
            continue
        exe = find_game_exe(install_dir)
        if not exe:
            log_info(f"{name} ({appid}): no Windows build installed, only the native one; "
                     "force quark as its compatibility tool in Steam to fetch it")
            continue
        seen.add(appid)
        games.append((appid, name, exe))
    games.sort(key=lambda g: g[1])
    return games
