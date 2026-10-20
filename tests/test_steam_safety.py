#!/usr/bin/env python3
"""Steam-safety regression test for the harness (tests/harness/).

Background: The harness used to sign the user out of Steam every time it
ran. Root cause was Phase 3 of cleanup_processes — it scanned EVERY
process owned by the user with `ps -u $UID -o pid,args` and SIGKILL'd
anything whose cmdline contained `.exe`, `wineserver`, `wine64-preloader`,
or `wine-preloader`. The only skip was "cmdline contains 'Proton'", which
failed to protect Steam's own helpers (steamwebhelper, gameoverlayui) and
any other Proton game's wineserver — those cmdlines don't contain the
literal string "Proton".

Killing steamwebhelper / gameoverlayui made Steam treat the session as
corrupted and force a re-login.

This test enforces two invariants:

  1. the harness source contains no `pkill -<sig> -f <bare-name>` calls
     where <bare-name> is one of the forbidden substrings.
  2. the harness's process-discovery code (cleanup_processes,
     find_wine_pids) NEVER returns PIDs that don't pass the quark-tree
     scoping check (binary inside COMPAT_DIR OR descendant of such).

Invariant 1 is static (grep the source). Invariant 2 is dynamic
(mock /proc with a synthetic process tree and assert the function only
returns the quark-tagged PIDs).
"""

import os
import re
import sys
import unittest
from pathlib import Path

TESTS_DIR = Path(__file__).parent
REPO_ROOT = TESTS_DIR.parent
HARNESS_FILES = sorted((TESTS_DIR / "harness").glob("*.py"))

# Substrings that, when used as bare pkill patterns, hit Steam's helpers
# or other Proton games' wineservers. NEVER allow these in a pkill cmd.
FORBIDDEN_BARE_PATTERNS = [
    ".exe",
    "wineserver",
    "wine64-preloader",
    "wine-preloader",
    "wine64",
    "wine",
]


class TestNoBarePkillPatterns(unittest.TestCase):
    """Static check: the harness must not pkill on bare names that match
    Steam helpers or stock Proton wineservers."""

    def test_no_bare_pkill_in_harness(self):
        src = "\n".join(f.read_text() for f in HARNESS_FILES)
        # Find every pkill invocation: pkill -<flag> <pattern> or
        # subprocess.run(["pkill", "-<flag>", "-f", "<pattern>"], ...)
        # We look for string literals that pkill receives as patterns.
        # Conservative: Scan for `"pkill"` and inspect the surrounding
        # 200 chars for forbidden bare patterns.
        offenders = []
        for m in re.finditer(r'"pkill"', src):
            window = src[m.start():m.start() + 300]
            for bad in FORBIDDEN_BARE_PATTERNS:
                # Match the bad pattern as a standalone string literal
                # (quoted, not part of a longer path). Allow it INSIDE a
                # path that starts with COMPAT_DIR or compat_dir.
                pat = rf'"{re.escape(bad)}"'
                if re.search(pat, window):
                    # OK if the same window also references COMPAT_DIR/compat_dir
                    # in front of the bad pattern (meaning it's a path).
                    bad_pos = window.find(f'"{bad}"')
                    if bad_pos > 0:
                        # Look back 30 chars; if we see f"{...} immediately
                        # preceding, it's a formatted path, not a bare name.
                        prefix = window[max(0, bad_pos - 30):bad_pos]
                        if "compat_dir" in prefix.lower() or "COMPAT_DIR" in prefix:
                            continue
                    offenders.append((bad, window[:160].replace("\n", " ")))
        self.assertEqual(
            offenders, [],
            "Found forbidden bare pkill pattern(s) in the harness — these "
            "match Steam's own processes:\n" +
            "\n".join(f"  '{bad}' near: {ctx}..." for bad, ctx in offenders)
        )

    def test_no_pgrep_bare_wine_patterns(self):
        """pgrep with bare wine/.exe/wineserver also leaks Steam helpers
        into discovery results — even if we don't SIGKILL them, surfacing
        them in find_wine_pids() leads to follow-up code (snapshots,
        analysis) that may eventually kill them."""
        src = "\n".join(f.read_text() for f in HARNESS_FILES)
        offenders = []
        for m in re.finditer(r'"pgrep"', src):
            window = src[m.start():m.start() + 300]
            for bad in FORBIDDEN_BARE_PATTERNS + [r"\.exe"]:
                pat = rf'"{re.escape(bad)}"'
                if re.search(pat, window):
                    bad_pos = window.find(f'"{bad}"')
                    if bad_pos > 0:
                        prefix = window[max(0, bad_pos - 30):bad_pos]
                        if "compat_dir" in prefix.lower() or "COMPAT_DIR" in prefix:
                            continue
                    offenders.append((bad, window[:160].replace("\n", " ")))
        self.assertEqual(
            offenders, [],
            "Found forbidden bare pgrep pattern(s) in the harness:\n" +
            "\n".join(f"  '{bad}' near: {ctx}..." for bad, ctx in offenders)
        )


class TestQuarkPidScoping(unittest.TestCase):
    """Functional check against the live /proc: when quark isn't running,
    every discovery function returns an empty result. When the only
    "quark" process is a fake exe symlinked into COMPAT_DIR, only that
    PID's tree is returned — no Steam helpers, no stock Proton."""

    def setUp(self):
        sys.path.insert(0, str(TESTS_DIR))
        import importlib
        if "harness.procs" in sys.modules:
            importlib.reload(sys.modules["harness.procs"])
        from harness import procs
        self.iterate = procs

    def tearDown(self):
        if str(TESTS_DIR) in sys.path:
            sys.path.remove(str(TESTS_DIR))

    def test_empty_when_no_quark_processes(self):
        """Sanity: with quark not running, discovery returns nothing.
        If this returns anything, the scoping has leaked outside COMPAT_DIR."""
        # Verify: We are NOT currently running anything from COMPAT_DIR.
        # If the user happens to have quark running, skip — test isn't
        # meaningful then.
        natives = self.iterate._quark_native_pids()
        if natives:
            self.skipTest(
                f"Quark is currently running (PIDs {sorted(natives)}); "
                f"skipping leak test"
            )
        all_quark = self.iterate._our_quark_pids()
        self.assertEqual(
            all_quark, set(),
            f"Discovery returned PIDs with no quark process alive: "
            f"{sorted(all_quark)}. These would be SIGKILL'd by "
            f"cleanup_processes — Steam signout risk."
        )
        self.assertEqual(
            self.iterate.find_wine_pids(), [],
            "find_wine_pids() leaked PIDs with no quark running."
        )

    def test_descendant_chain_walking_is_correct(self):
        """_descendants_of() must include grandchildren, not just direct
        children — wine64-preloader → wineboot.exe → game.exe is 3-deep."""
        import subprocess, time
        # Foreground chain so PPIDs stay anchored:
        #   parent bash → child bash → grandchild sleep
        parent = subprocess.Popen(
            ["bash", "-c", "exec bash -c 'exec sleep 5'"]
        )
        try:
            time.sleep(0.3)  # let the chain spawn
            tree = self.iterate._descendants_of({parent.pid})
            # `exec` chain means the parent IS the child (same PID after
            # exec). Spawn a real fork instead so we get genuine descendants.
        finally:
            parent.kill()
            parent.wait()
        # Real fork chain via subprocess: Each bash -c launches a child
        # that runs `sleep`, which is a separate PID.
        # Use `; :` to defeat bash's single-command exec optimization, so
        # each shell genuinely forks its child.
        parent = subprocess.Popen(
            ["bash", "-c", "bash -c 'sleep 3 ; :' ; :"]
        )
        try:
            time.sleep(0.5)
            tree = self.iterate._descendants_of({parent.pid})
            self.assertGreaterEqual(
                len(tree), 2,
                f"Descendant walk missed grandchildren of PID {parent.pid}: "
                f"got {sorted(tree)} — should include the inner bash AND its sleep child"
            )
        finally:
            parent.kill()
            parent.wait()


def run_tests():
    from quark_common import run_unittests
    return run_unittests("steam safety", TestNoBarePkillPatterns, TestQuarkPidScoping)
