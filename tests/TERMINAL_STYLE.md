# quark test-suite terminal style

The single reference for everything the suite prints to a terminal: One
logger, one shape per output kind. Every command `tests/run.py` exposes
(`suite`, `launch`, `scoreboard`, `qemu`, `heap-repro`), every script it runs
(`test_package.py`, `test_steam_safety.py`, `verify_patches.py`) and
`install.py` render through `quark_common`, so a user sees the same grammar
everywhere.

Binding rules (from the project doctrine, restated so this file is
self-contained):

- Every line is `[HH:MM:SS] [LEVEL]   message`. No exceptions for "headers" or
  "banners": A header is just a message.
- No decorative separators anywhere: No `===`, `---`, `***`, `###`, no
  `== text ==`, no box-drawing, no underlines. A blank line is the only
  separator.
- Numeric or structural data is a table (header row and aligned columns) or
  routes to sublimation or `montauk --analyze`. Never an ad-hoc dump.
- Run commands bare: `python3 tests/run.py ...`, never with sudo in front, not
  in the docs, not in program output, not in any instruction shown to the user.
  A step that needs root prefixes `sudo` on that one command (as
  `harness/sched.py` does), so the prompt comes after the bare command, once,
  when the step actually needs it.

## The line format

```
[HH:MM:SS] [LEVEL]   message
```

- Timestamp: `[%H:%M:%S]` then one space.
- Level tag padded so the message column is fixed for every level: `[INFO]`
  and `[WARN]` plus 3 spaces, `[ERROR]` and `[DEBUG]` plus 2. The message
  always begins at column 19.
- Levels: `DEBUG` `INFO` `WARN` `ERROR`. INFO and up print to the terminal;
  DEBUG goes to the run's log under `/tmp/quark-suite` only, unless `-v`. `-q`
  raises the terminal floor to WARN.

```
[08:51:58] [INFO]   an informational line
[08:51:58] [WARN]   a warning
[08:51:58] [ERROR]  an error
[08:51:58] [DEBUG]  a debug line (log file, or -v)
```

## No indentation: Every line is flat

Every message begins at column 19, whatever it belongs to. There is no nesting
depth and no leading spaces in the message field. Grouping is the SECTION line
and a single blank line between sections, never indent. A line never carries
spaces to sit under another.

```
[09:01:22] [INFO]   ntsync: /dev/ntsync present
[09:01:22] [INFO]   deployed: ~/.local/share/Steam/compatibilitytools.d/quark
[09:01:29] [INFO]   2379780 Balatro   reached STEAM_API in 4.1s
```

## INTRO line

First line of every command: Identity, version, build and mode, one line.

```
[08:51:58] [INFO]   quark suite v0.2.0 [c5da2fa45-dirty] [layers=unit,gate,deploy]
```

Form: `quark <command> <version> [<commit><-dirty?>] <mode-tag?>`. One intro
per invocation. A command run under another (child mode, below) prints none.

## SECTION header

A phase. One grammar for all of them, preceded by one blank line.

Form: `<Name>: <VERB>`, the verb uppercase. Names are fixed labels: The layer
names stay lowercase (`unit`, `gate`, `deploy`, `launch`, `qemu`), proper
nouns keep their casing (`Balatro`, `QEMU`).

```

[09:01:21] [INFO]   unit: RUN
[09:01:21] [INFO]   gate: RUN
[09:01:21] [INFO]   deploy: SKIP
[09:01:21] [INFO]   Balatro: LAUNCH
```

## Status and summary lines

A standalone status or completion line leads with an uppercase keyword and a
colon, the same grammar as artifacts. Used for layer results and environment
state, not per-step progress.

```
[11:05:17] [INFO]   SUITE: 4 passed, 1 skipped (nothing deployed)
[11:03:36] [WARN]   DEPLOY: STALE (deployed build predates the tree)
```

## STEP and result lines

Progress inside a section. The action and its result are both flush lines;
the result reads as a continuation by its wording, not by indent.

```
[08:58:55] [INFO]   verifying 6 patches against the deployed Wine
[08:59:11] [INFO]   6 of 6 applied cleanly
```

## Check lines

A single pass or fail inside a section is a STEP line whose level carries the
verdict: INFO for a pass, ERROR for a failure, WARN for a skip with its
reason. No `ok`, `FAIL` or check-mark prefixes of its own; the level tag is the
verdict. A layer ends with one status line counting them.

```
[09:10:02] [INFO]   package: compatibilitytool.vdf names quark
[09:10:02] [ERROR]  package: toolmanifest.vdf missing commandline
[09:10:02] [WARN]   package: no lsteamclient cache, skipped
[09:10:02] [INFO]   PACKAGE: 11 passed, 1 failed, 1 skipped
```

## Tables

Numeric results. A header row of plain column names, then aligned data rows:
The label column left-justified to width 28, numeric columns right-justified to
width 10, units inline. No separator row. Built through `table_header()` and
`table_row()` in `quark_common`.

```
GAME                              START     FRAMES    VS BASE
Balatro                           4.12s       1440     -3.1%
Hollow Knight                     6.80s       1438    (baseline)
```

A short table is printed verbatim, framed by one blank line, without a
timestamp per row.

## Reports are absorbed, not dumped

A full report (a `montauk --analyze` digest, the scoreboard table, a QEMU run's
pulled-back logs) is the file artifact, not terminal output. The terminal stays
the progress log; the report is written to disk and its path announced as an
artifact line.

```
[09:02:07] [INFO]   REPORT: tests/scoreboard.md
```

## WARN and ERROR

Same format, different level. Used for conditions, never for normal progress.
No manual `[WARN]` or `WARNING:` inside the text; the level tag carries it.

```
[08:58:54] [WARN]   ntsync: /dev/ntsync not present; try modprobe ntsync
[08:52:17] [ERROR]  launch: Balatro exited 139 before STEAM_API
```

## Artifacts (end of run)

Where output landed, grouped at the end, one line each.

```
[09:02:07] [INFO]   LOG: /tmp/quark-suite/suite-20261018-090121.log
[09:02:07] [INFO]   TRACE: /tmp/quark-suite/launch-2379780-20261018-090121.events
```

## Interrupt (Ctrl+C)

One line, one wording, one level everywhere, through `log_interrupted()`.

```
[09:03:00] [WARN]   interrupted: Cleaning up
```

## Child mode

When one command runs another (`suite` running a layer's script, the QEMU
suite running the suite in the guest), the child runs with `QUARK_CHILD=1`. A
child prints no INTRO; it emits its SECTION, STEP, WARN and ERROR lines and its
one counting line, which is the layer's result, and appends to its parent's log
(`QUARK_LOG`), so a suite run is one log on disk. Its interrupt line goes to
that log only; the parent's is the one on the terminal. A suite run reads as
one uniform progress log.

## montauk as the utility

The logger handles presentation; measurement and analysis route through
montauk and sublimation, not hand-rolled code:

- Percentiles, mean and stdev: `sublimation quantile`, `mean`, `stdev`.
- Trace analysis: `montauk --analyze`, read as JSON.
- A report is a `montauk --analyze` result written to a file and announced as
  an artifact line, not echoed.

A command presents; montauk measures; the logger renders. No command
hand-formats a number that a table helper, sublimation or montauk should
produce.

## What this replaces

- 59 bare `print()` calls that bypass the logger: `verify_patches.py` 31,
  `harness/launch.py` 14, `test_package.py` 12, `harness/scoreboard.py` and
  `harness/tools.py` one each.
- Indented continuation lines inside messages (`install.py`'s `"  Run: ..."`).
- Per-script pass and fail markers of their own, replaced by the level tag and
  one counting status line per layer.
