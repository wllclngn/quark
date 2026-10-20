#!/usr/bin/env python3
"""quark tests: one entry point for everything the suite can do.

    python3 tests/run.py                          # the suite, every layer
    python3 tests/run.py suite --layer unit       # one layer (repeatable)
    python3 tests/run.py suite --continue         # keep going after a failure
    python3 tests/run.py launch                   # Balatro through quark, 30 s
    python3 tests/run.py launch 379720 --trace    # one game, under a montauk capture
    python3 tests/run.py launch --all             # every installed Windows game
    python3 tests/run.py scoreboard               # the going-to-PROD step, quark only
    python3 tests/run.py scoreboard --static-only # its static lane alone, no games
    python3 tests/run.py scoreboard --tools proton-exp,proton11   # with the A/B columns
    python3 tests/run.py qemu provision           # once: The VM image
    python3 tests/run.py qemu test                # the suite in the VM
    python3 tests/run.py heap-repro               # the heap-corruption repro under the observer

The layers of `suite`:
  unit    cargo test: The Rust tests under tests/rust, each compiling the
          source module it covers
  gate    checks that need no install (Steam-safe process scoping)
  deploy  the compat tool install.py deployed: Package structure and patch
          verification; skipped (not failed) when nothing is deployed
  launch  a real game through the harness; skipped when nothing is deployed
  qemu    the full suite in a disposable VM (tests/qemu/): install,
          probes and the layers above; skipped when no image is provisioned

quark is the tool under test everywhere. The Proton tools exist only behind
`--tool` and `--tools`, for an A/B run, the way PANDEMONIUM's suite keeps
other schedulers behind a flag. `scoreboard` is the going-to-PROD step,
montauk's static lane and then the game matrix, run by hand before a
release; it is not a layer.
"""
import argparse
import os
import subprocess
import sys
from pathlib import Path

TESTS = Path(__file__).resolve().parent
sys.path.insert(0, str(TESTS.parent))
sys.path.insert(0, str(TESTS))
from quark_common import (REPO_ROOT, COMPAT_DIR, QEMU_READY, install_exit_guard, log, log_blank,  # noqa: E402
                          log_error, log_finish, log_info, log_interrupted, log_intro, log_section,
                          log_warn, run_cargo_tests)
import test_package  # noqa: E402
import test_steam_safety  # noqa: E402
import verify_patches  # noqa: E402

PY = sys.executable
RUN = str(TESTS / "run.py")

# A hung test must fail, not wait: Every step is bounded. The QEMU suite's
# first run builds patched Wine on the cache disk, hence its own bound.
STEP_TIMEOUT = 900  # seconds
LAYER_TIMEOUT = {"qemu": 7200}

LAYERS = {
    "unit": [
        ("cargo test", [PY, RUN, "_step", "cargo"]),
    ],
    "gate": [
        ("steam safety", [PY, RUN, "_step", "steam-safety"]),
    ],
    "deploy": [
        ("package", [PY, RUN, "_step", "package"]),
        ("patches", [PY, RUN, "_step", "patches"]),
    ],
    "launch": [
        ("launch", [PY, RUN, "launch", "2379780", "--skip-build", "--timeout", "20"]),
    ],
    "qemu": [
        ("qemu suite", [PY, RUN, "qemu", "test"]),
    ],
}

def cargo_unit() -> int:
    return run_cargo_tests("unit", ["--release"], REPO_ROOT)


# Each layer step runs as its own bounded subprocess, so a hung test fails
# on its timeout; the step itself is a function, named here directly so the
# static lane sees the call.
STEPS = {
    "cargo": cargo_unit,
    "steam-safety": test_steam_safety.run_tests,
    "package": test_package.run_tests,
    "patches": verify_patches.run_tests,
}
NEEDS_DEPLOY = {"deploy", "launch"}


def run_step(name: str, cmd: list[str], timeout: int) -> bool:
    log_info(f"{name}: {' '.join(cmd)}")
    try:
        # A child prints no INTRO and appends to this run's log.
        rc = subprocess.run(cmd, cwd=REPO_ROOT, timeout=timeout,
                            env=dict(os.environ, QUARK_CHILD="1")).returncode
    except subprocess.TimeoutExpired:
        log_error(f"{name}: TIMEOUT after {timeout}s (a test that cannot finish has not passed)")
        return False
    if rc != 0:
        log_error(f"{name}: FAIL (exit {rc})")
        return False
    log_info(f"{name}: PASS")
    return True


def run_suite(args: argparse.Namespace) -> int:
    from harness.procs import cleanup_processes
    deployed = (COMPAT_DIR / "proton").exists()
    passed, failed, skipped = 0, 0, 0
    for layer in args.layer or list(LAYERS):
        steps = LAYERS[layer]
        if layer in NEEDS_DEPLOY and not deployed:
            log_section(layer, "skip")
            log_warn(f"{layer}: nothing deployed at {COMPAT_DIR}; run ./install.py")
            skipped += len(steps)
            continue
        if layer == "qemu" and not QEMU_READY.exists():
            log_section(layer, "skip")
            log_warn(f"{layer}: no finished provision; run tests/run.py qemu provision")
            skipped += len(steps)
            continue
        log_section(layer, "run")
        if layer == "launch":
            cleanup_processes()
        for name, cmd in steps:
            if run_step(name, cmd, LAYER_TIMEOUT.get(layer, STEP_TIMEOUT)):
                passed += 1
            else:
                failed += 1
                if not args.keep_going:
                    break
        if layer == "launch":
            cleanup_processes()
        if failed and not args.keep_going:
            break

    summary = f"SUITE: {passed} passed, {failed} failed, {skipped} skipped"
    log_blank()
    if failed:
        log_error(summary)
        return 1
    log_info(summary)
    return 0


def run_qemu(args: argparse.Namespace) -> int:
    from qemu import guest
    return guest.main(args.action, args.k)


def run_step_module(args: argparse.Namespace) -> int:
    return int(STEPS[args.name]() or 0)


def main() -> int:
    parser = argparse.ArgumentParser(description="quark tests", formatter_class=argparse.RawDescriptionHelpFormatter,
                                     epilog=__doc__.split("\n", 2)[2])
    sub = parser.add_subparsers(dest="command", metavar="{suite,launch,scoreboard,qemu,heap-repro}")
    floor = argparse.ArgumentParser(add_help=False)
    floor.add_argument("-v", dest="verbose", action="store_true", help="DEBUG lines on the terminal too")
    floor.add_argument("-q", dest="quiet", action="store_true", help="only WARN and ERROR on the terminal")

    p = sub.add_parser("suite", parents=[floor], help="the layered suite (the default)")
    p.add_argument("--layer", action="append", choices=list(LAYERS), help="run only this layer (repeatable)")
    p.add_argument("--continue", dest="keep_going", action="store_true", help="keep going after a failure")
    p.set_defaults(func=run_suite)

    from harness import launch, scoreboard
    p = sub.add_parser("launch", parents=[floor], help="build, install, launch and monitor one game, or every one")
    launch.add_arguments(p)
    p.set_defaults(func=launch.run)

    p = sub.add_parser("scoreboard", parents=[floor], help="the going-to-PROD step: montauk's static lane, then every game through quark and any tool named")
    scoreboard.add_arguments(p)
    p.set_defaults(func=scoreboard.run)

    p = sub.add_parser("qemu", parents=[floor], help="the disposable-VM suite")
    p.add_argument("action", choices=["fetch", "provision", "test", "shell"])
    p.add_argument("-k", metavar="NAME", default=None, help="test: run only probes whose name contains NAME")
    p.set_defaults(func=run_qemu)

    from heap_repro import run as heap_repro
    p = sub.add_parser("heap-repro", parents=[floor], help="the heap-corruption repro under the observer")
    heap_repro.add_arguments(p)
    p.set_defaults(func=heap_repro.run)

    p = sub.add_parser("_step", parents=[floor])
    p.add_argument("name", choices=list(STEPS))
    p.set_defaults(func=run_step_module)

    # No command means the suite; -v and -q may come before the command too.
    argv = sys.argv[1:]
    lead = []
    while argv and argv[0] in ("-v", "-q"):
        lead.append(argv.pop(0))
    if not argv or argv[0] not in sub.choices and argv[0] not in ("-h", "--help"):
        argv.insert(0, "suite")
    if argv[0] in sub.choices:
        argv[1:1] = lead
    args = parser.parse_args(argv)
    install_exit_guard()
    log.start(args.command, quiet=args.quiet, verbose=args.verbose)
    log_intro(args.command, f"[layers={','.join(args.layer or LAYERS)}]" if args.command == "suite" else "")
    try:
        rc = args.func(args)
    except KeyboardInterrupt:
        log_interrupted()
        rc = 130
    log_finish()
    return rc


if __name__ == "__main__":
    sys.exit(main())
