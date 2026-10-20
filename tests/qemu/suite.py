#!/usr/bin/env python3
"""The quark suite as it runs inside the QEMU guest (tests/qemu/guest.py
pushes the repo and starts this). Never run it on a host: it installs quark
for real, owns the display and audio server, and expects the provisioned
stand-in Steam root.

  1. Xvfb and a PulseAudio null sink, so Wine has a display and an endpoint
  2. install.py --yes, with the Wine build cached on the persistent disk
  3. montauk built from the pushed tree and given its tracing capabilities
  4. every probe built with mingw and launched through quark the way Steam
     launches a game, under quark's montauk observer, judged by the verdict
     file it writes; a failed probe's capture is analyzed to JSON
  5. tests/run.py with the unit, gate and deploy layers

Usage (in the guest):
  python3 tests/qemu/suite.py [-k NAME]

The verdict, probe results, DXVK logs and analyses land in SUITE_DIR, apart
from LOG_DIR, which install.py's clean step removes; the launcher's logs and
montauk's captures land in LOG_DIR after it. guest.py copies both back.
"""

import argparse
import json
import os
import subprocess
import sys
import time
from pathlib import Path

REPO = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(REPO))
from quark_common import (
    STEAM_ROOT, STEAMAPPS, COMPAT_DIR, LOG_DIR, SUITE_DIR, MONTAUK_SRC, WINE_SRC_DIR,
    log_info, log_warn, log_error, stop_observer, stop_prefix_wineserver,
)

PROBE_SRC = Path(__file__).resolve().parent / "probes"
PROBE_BIN = SUITE_DIR / "probes"
RESULTS = SUITE_DIR / "probe-results"
PROBE_PREFIX_DATA = STEAMAPPS / "compatdata" / "quarkprobe"
VERDICT = SUITE_DIR / "qemu-verdict.json"
DISPLAY = ":99"
PROBE_TIMEOUT = 180  # seconds; the first launch builds the prefix
INCIDENTS = LOG_DIR / "incidents"  # where the launcher's observer writes
MONTAUK_BUILD = WINE_SRC_DIR.parent / "montauk-build"  # on the persistent disk
MONTAUK = Path("/usr/local/bin/montauk")
# montauk's own install.py grants this set: load and attach BPF, attach
# tracepoints, raise RLIMIT_MEMLOCK and read the root-only tracepoint ids.
TRACE_CAPS = "cap_sys_admin,cap_bpf,cap_perfmon,cap_sys_resource,cap_dac_read_search+ep"

# Libraries beyond mingw's defaults (kernel32, user32, advapi32, ...).
PROBE_LIBS = {
    "d3d11_clear": ["-ld3d11"],
    "audio": ["-lole32"],
    "gl_heap_stress": ["-lopengl32", "-lgdi32", "-lole32", "-lwinmm"],
}
# Also built for i386 and run under WoW64 as <name>32, so a prefix that
# cannot start a 32-bit process fails the suite.
PROBES_I386 = {"window"}


def start_display_and_audio() -> None:
    subprocess.Popen(["Xvfb", DISPLAY, "-screen", "0", "1280x720x24"],
                     stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    os.environ["DISPLAY"] = DISPLAY
    time.sleep(1)
    # The user session socket-activates PulseAudio, where `pulseaudio --start`
    # fails as a second daemon; start one only when nothing answers.
    load_sink = ["pactl", "load-module", "module-null-sink", "sink_name=quark_null"]
    if subprocess.run(load_sink, capture_output=True).returncode != 0:
        subprocess.run(["pulseaudio", "--start", "--exit-idle-time=-1"], capture_output=True)
        if subprocess.run(load_sink, capture_output=True).returncode != 0:
            log_warn("PulseAudio null sink unavailable; the audio probe will fail")


def install() -> int:
    log_info("install.py --yes")
    return subprocess.run([sys.executable, "install.py", "--yes"], cwd=REPO,
                          stdin=subprocess.DEVNULL).returncode


def build_montauk() -> bool:
    log_info(f"building montauk from {MONTAUK_SRC}")
    steps = [
        ["cmake", "-B", str(MONTAUK_BUILD), "-S", str(MONTAUK_SRC), "-DCMAKE_BUILD_TYPE=Release",
         "-DMONTAUK_KERNEL=OFF", "-DMONTAUK_BUILD_TESTS=OFF"],
        ["cmake", "--build", str(MONTAUK_BUILD), "-j", str(os.cpu_count() or 4)],
        ["sudo", "install", "-Dm755", str(MONTAUK_BUILD / "montauk"), str(MONTAUK)],
        ["sudo", "setcap", TRACE_CAPS, str(MONTAUK)],
    ]
    for cmd in steps:
        r = subprocess.run(cmd, capture_output=True, text=True)
        if r.returncode != 0:
            log_error(f"montauk: {' '.join(cmd[:2])} failed:\n{(r.stdout + r.stderr).strip()}")
            return False
    return True


def build_probes(selector: str | None) -> list[tuple[str, Path]]:
    PROBE_BIN.mkdir(parents=True, exist_ok=True)
    built = []
    for src in sorted(PROBE_SRC.glob("*.c")):
        variants = [(src.stem, "x86_64-w64-mingw32-gcc")]
        if src.stem in PROBES_I386:
            variants.append((src.stem + "32", "i686-w64-mingw32-gcc"))
        for name, cc in variants:
            if selector and selector not in name:
                continue
            exe = PROBE_BIN / f"{name}.exe"
            cmd = [cc, "-O2", "-Wall", f'-DPROBE_NAME="{name}"',
                   "-o", str(exe), str(src), *PROBE_LIBS.get(src.stem, [])]
            r = subprocess.run(cmd, capture_output=True, text=True)
            if r.returncode != 0:
                log_error(f"build {name}:\n{r.stderr.strip()}")
                built.append((name, None))
                continue
            built.append((name, exe))
    return built


def probe_env(traced: bool) -> dict[str, str]:
    env = dict(os.environ)
    if traced:
        env.update({"QUARK_TRACE": "1", "QUARK_TRACE_OUT": "1"})
    env.update({
        "STEAM_COMPAT_DATA_PATH": str(PROBE_PREFIX_DATA),
        "STEAM_COMPAT_CLIENT_INSTALL_PATH": str(STEAM_ROOT),
        "SteamAppId": "480",
        "SteamGameId": "480",
        "DISPLAY": DISPLAY,
        "DXVK_LOG_LEVEL": "info",
        "DXVK_LOG_PATH": str(RESULTS),
    })
    return env


def analyze_capture(name: str, trace: Path) -> None:
    """The full report set for a failed probe's capture, as JSON beside its
    verdict, where guest.py copies it back to the host."""
    if not trace.exists():
        log_warn(f"probe {name}: no capture at {trace}")
        return
    out = RESULTS / f"{name}-analyze.json"
    with out.open("w") as f:
        rc = subprocess.run([str(MONTAUK), "--analyze", str(trace), "--json"],
                            stdout=f, stderr=subprocess.PIPE, text=True)
    if rc.returncode != 0:
        log_warn(f"probe {name}: montauk --analyze exited {rc.returncode}: {rc.stderr.strip()[-400:]}")
    else:
        log_info(f"probe {name}: analysis in {out}")


def run_probe(name: str, exe: Path, traced: bool) -> tuple[bool, str]:
    verdict_file = RESULTS / f"{name}.txt"
    verdict_file.unlink(missing_ok=True)
    # The launcher names the capture after the exe stem, clamped to the
    # kernel's 15-character comm.
    trace = INCIDENTS / f"{name[:15]}-io.bin"
    trace.unlink(missing_ok=True)
    try:
        subprocess.run([str(COMPAT_DIR / "proton"), "waitforexitandrun", str(exe)],
                       env=probe_env(traced), timeout=PROBE_TIMEOUT,
                       stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        ok, detail = judge(name, verdict_file)
    except subprocess.TimeoutExpired:
        ok, detail = False, f"no verdict within {PROBE_TIMEOUT}s (hung)"
    finally:
        stop_prefix_wineserver(PROBE_PREFIX_DATA / "pfx")
        if traced:
            stop_observer(trace)
    if traced and not ok:
        analyze_capture(name, trace)
    return ok, detail


def judge(name: str, verdict_file: Path) -> tuple[bool, str]:
    if not verdict_file.exists():
        return False, "no verdict written (crashed before reporting)"
    line = verdict_file.read_text(errors="replace").strip()
    ok = line.startswith("PASS ")
    if ok and name == "d3d11_clear":
        # The clear has to have gone through DXVK, not wined3d's GL path.
        # DXVK writes one log per DLL; its "DXVK: v<version>" banner is in
        # the first one loaded (dxgi), not necessarily the d3d11 log.
        if not any("DXVK: v" in log.read_text(errors="replace")
                   for log in RESULTS.glob(f"{name}_*.log")):
            return False, f"{line.removeprefix('PASS ')}, but no DXVK log: rendered without DXVK"
    return ok, line.split(" ", 1)[1] if " " in line else line


def run_layers() -> int:
    log_info("tests/run.py --layer unit --layer gate --layer deploy")
    return subprocess.run([sys.executable, "tests/run.py", "--continue",
                           "--layer", "unit", "--layer", "gate", "--layer", "deploy"],
                          cwd=REPO).returncode


def main() -> int:
    parser = argparse.ArgumentParser(description="quark suite, in the QEMU guest")
    parser.add_argument("-k", metavar="NAME", help="run only probes whose name contains NAME")
    args = parser.parse_args()

    RESULTS.mkdir(parents=True, exist_ok=True)
    start_display_and_audio()

    install_rc = install()
    if install_rc != 0:
        log_warn(f"install.py exited {install_rc}; continuing so the probes show the effect")
    if not (COMPAT_DIR / "proton").exists():
        log_error(f"nothing deployed at {COMPAT_DIR}; no probe can run")
        VERDICT.write_text(json.dumps({"install_rc": install_rc, "probes": {}, "layers_rc": None}, indent=2))
        return 1

    traced = build_montauk()
    if not traced:
        log_warn("montauk unavailable; the probes run untraced")

    probes = {}
    for name, exe in build_probes(args.k):
        if exe is None:
            ok, detail = False, "did not compile"
        else:
            log_info(f"probe {name}")
            ok, detail = run_probe(name, exe, traced)
        (log_info if ok else log_error)(f"probe {name}: {'PASS' if ok else 'FAIL'} {detail}")
        probes[name] = {"ok": ok, "detail": detail}

    layers_rc = None if args.k else run_layers()

    VERDICT.write_text(json.dumps(
        {"install_rc": install_rc, "traced": traced, "probes": probes, "layers_rc": layers_rc}, indent=2))
    failed = [n for n, p in probes.items() if not p["ok"]]
    passed = len(probes) - len(failed)
    summary = f"{passed}/{len(probes)} probes passed, install rc {install_rc}, layers rc {layers_rc}"
    if failed or install_rc != 0 or layers_rc not in (0, None):
        log_error(f"FAIL: {summary}; failed: {', '.join(failed) or 'none'}")
        return 1
    log_info(f"PASS: {summary}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
