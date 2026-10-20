# quark

quark is a Proton replacement for Steam on Linux, built in Rust, that
launches Windows games on the Wine your distribution ships instead of a
fork of it. It answers the four verbs Steam speaks to a compatibility tool
(`waitforexitandrun`, `run`, `getcompatpath`, `getnativepath`) and owns
everything around the launch: The prefix and its registry, DXVK,
VKD3D-Proton and dxvk-nvapi deployed into it, the bridge between the game's
`steam_api` and the running Steam client, the environment the game sees,
the session it runs in and, when asked, an eBPF capture of the whole run.
Seven small patches sit on stock Wine 11.20. Nothing is vendored, and
nothing runs in a container or a namespace.

A launch ordered as a topological sort over declared nodes, the wineserver
started as a node ahead of the first Wine process, a deploy cache keyed per
component by one stat each, Proton's steamclient trampoline ported to upstream Wine, the
compat tree run from a copy of Wine's own loader so its patched modules
load relative to it, a session held by the game's pidfd that ends the whole
prefix on Steam's Stop and waits for it on a normal exit, save data
snapshotted as hardlinks and restored only where a file went missing, an
installer that is itself a build system with verifying traces, and a test
suite with one entry point. The launcher's own work on a cache hit is 11 to
15 ms, and the game is spawned 31 ms after Steam starts the launcher.

Every number in this file comes from a [montauk](https://github.com/wllclngn/montauk)
capture or the harness's own clock.

## Performance

12 AMD Zen CPUs (Ryzen 5 3600), GeForce RTX 2060 (NVIDIA 615.78.08), kernel
7.2.11-arch1-1, Wine 11.20. The scoreboard tables ran under EEVDF; the
wineserver-node timings, near 3.6 s to the game's process, are the
PANDEMONIUM scheduler's.

Games are launched through `tests/run.py launch`. A cold launch starts with
no wineserver for the prefix. A Proton run gets its own prefix beside
quark's, so neither tool touches the other's.

### Launch to the Game's Process (scoreboard, median of three)

| Game | Proton 11 | Proton Experimental | quark |
|---|---|---|---|
| Hollow Knight | 1,574 ms | 1,637 ms | **723 ms** |
| Balatro | 1,654 ms | 1,657 ms | **1,319 ms** |
| DOOM (2016) | 1,559 ms | 1,572 ms | **789 ms** |
| TMNT: Shredder's Revenge | 1,505 ms | 1,502 ms | **671 ms** |
| Wedding Witch | 1,499 ms | 1,504 ms | **671 ms** |
| Halls of Torment | 1,562 ms | 1,566 ms | **723 ms** |

`tests/run.py scoreboard --tools proton-exp,proton11`, three runs a cell
on warm prefixes, 30 s each. The harness also times the game's first
window, but that number does not compare across tools: Proton names every
window of the session `steam_app_<appid>`, steam.exe's included, so its
first match is not the game's.

### Launch to the First Frame (scoreboard, median of three)

| Game | Proton 11 | Proton Experimental | quark |
|---|---|---|---|
| Hollow Knight | 3,899 ms | 5,484 ms | **2,608 ms** |
| Balatro | 2,988 ms | 3,018 ms | **2,661 ms** |
| DOOM (2016) | none in 30 s | none in 30 s | **3,594 ms** |
| TMNT: Shredder's Revenge | none in 30 s | none in 30 s | **4,642 ms** |
| Wedding Witch | 3,297 ms | 9,576 ms | **2,239 ms** |
| Halls of Torment | 2,901 ms | 10,908 ms | **2,583 ms** |

The first montauk snapshot that attributes GPU to the game's own
processes. Under either Proton, neither it nor MangoHud read a frame from
DOOM or TMNT within the cell.

### Frame Times (scoreboard, MangoHud, median of three)

| Game | Proton 11 | Proton Experimental | quark |
|---|---|---|---|
| Hollow Knight, p99 | 17.4 ms | 19.4 ms | 18.7 ms |
| Halls of Torment, p99 | 18.8 ms | 19.0 ms | 19.5 ms |
| DOOM (2016), p99 | none | none | 17.5 ms |
| TMNT: Shredder's Revenge, p99 | none | none | 19.5 ms |

Once a game draws, its frames pace alike under all three. Balatro draws
through OpenGL, and the harness turns on MangoHud's Vulkan layer alone;
Wedding Witch reads a p99 near 1 s and a 1% low of 1 under every tool,
over the intro its 30 s cover.

### The Launcher's Own Work (cache hit)

| Phase | ms |
|---|---|
| Discover | 0 |
| Launch graph, all nine nodes | 7 to 11 |
| Through the environment | 11 to 15 |
| Game spawned | **31** |

From the timing line `QUARK_VERBOSE=1` writes, which lists every node of
the graph with its own time.

### Wineserver Started as a Node (cold server, Balatro, median of three)

| | Without the node | With the node |
|---|---|---|
| To the game's process | 3,613 ms | **3,577 ms** |
| To the game's window | 4,462 ms | **4,317 ms** |

### NVIDIA Helper Spawns

| Workload | Inside `bwrap --dev-bind / /` | quark, no namespace |
|---|---|---|
| Vulkan instance, NVML init, GLX and EGL display (three C probes) | 8 | **0** |
| A Balatro launch | 24 | **0** |

`nvidia-modprobe` exec events, counted from montauk captures.

### Memory (scoreboard, at the cell's 30 s, median of three)

| Game | Proton 11 | Proton Experimental | quark |
|---|---|---|---|
| Hollow Knight | 1,689 MB, 13 processes | 1,766 MB, 13 | **1,371 MB, 11** |
| Balatro | 1,177 MB, 12 | 1,175 MB, 12 | **916 MB, 10** |
| DOOM (2016) | 1,516 MB, 12 | 1,522 MB, 12 | 1,925 MB, **10** |
| TMNT: Shredder's Revenge | 1,078 MB, 12 | 1,079 MB, 12 | 1,465 MB, **10** |
| Wedding Witch | 1,962 MB, 13 | 1,962 MB, 13 | **1,585 MB, 11** |
| Halls of Torment | 2,024 MB, 12 | 2,024 MB, 12 | **1,602 MB, 10** |

DOOM and TMNT hold more under quark because they are drawing by then, and
under Proton they have not drawn yet.

### ntsync on the Stock Wineserver (traced launch)

| Game | Engine | ntsync ioctls | Window |
|---|---|---|---|
| Balatro | LÖVE | 21,700 | 40 s |
| Hollow Knight | Unity 6 | 36,734 | 30 s |
| TMNT: Shredder's Revenge | FNA/.NET | 59,437 | 30 s |
| Halls of Torment | Godot/.NET | 141,521 | 30 s |
| Wedding Witch | Unity | 420,795 | 30 s |
| DOOM (2016) | id Tech 6 | 600,497 | 30 s |

Wine 11.20 uses `/dev/ntsync` on its own when the device opens; no
variable turns it on.

### Session End (Balatro)

| Event | Result |
|---|---|
| Steam's Stop button, launched from Steam's library | First process gone to the last in **793 ms**, nothing left |
| Steam's Stop, SIGTERM to the launcher | Nothing left on the prefix after **1.6 s** |
| steam.exe killed while the game runs | The game keeps running and the launcher waits |
| The game quits | The launcher exits **4.6 s** later with nothing left |

### Installer (seventeen steps)

| Run | Wall time |
|---|---|
| As a graph, empty cache | **95 s** (the 7 s clone, then Patch Wine 74 s beside the Steam bridge 89 s); 130 s with the bridge after Patch Wine |
| As a graph, every trace rebuilt, Wine object dir warm | 72 s |
| As a graph, nothing changed | **1 s** |

The Steam bridge builds in its own worktree of the Wine source, so it runs
beside Patch Wine instead of after it, and the two set the cold time
between them; every other step runs beside them.

## Compatibility

| Game | Engine | Result |
|---|---|---|
| Hollow Knight | Unity 6 | Language menu, Steam logged in, D3D11 through DXVK on the GPU |
| Balatro | LÖVE | Main menu, Steam API live |
| DOOM (2016) | id Tech 6 | Main menu, native Vulkan, Steam logged in |
| TMNT: Shredder's Revenge | FNA/.NET | Intro cutscene |
| Wedding Witch | Unity | Intro, skip prompt live |
| Halls of Torment | Godot | Hero select, Steam API live |

Each launches from Steam's library through quark. "Steam API live" is
Steam's own game process log tracking the game's process.

The patched modules are built for x86_64; a 32-bit game starts under WoW64
on Wine's own i386 modules, without the patches. EasyAntiCheat is untested:
The kernelbase patches deploy, and no EasyAntiCheat title has run through
quark yet. BattlEye is not wired.

## Key Features

### Launch Graph

The launch is a topological sort. Each node names the nodes it needs;
`graph.rs` keeps every node's in-degree, starts each node whose count
reaches zero on its own scoped thread, and as each one finishes releases
the nodes waiting on it (Kahn's algorithm [1]). Outputs are keyed by node
and read by dependents, which always run after their producers.

| Node | Needs | Does |
|---|---|---|
| **registry** | | Lays out a new prefix from the Wine tree's PE modules, seeds the template's registry and profile directories, and injects the launcher's stubs and the Steam keys after the seed, never before, since a `.reg` file that exists is never replaced. Reads neither hive when both are as its last run left them. |
| **prefix** | registry | On a first launch, the template copy, its subdirectories on scoped threads over getdents64 and hardlinks, or `wineboot --init` without a template. On every launch, the profile directories and `S:`, the game's Steam library, mapped as Proton maps it. |
| **wineserver** | prefix | Starts the prefix's wineserver ahead of the session. |
| **dxvk**, **vkd3d** | prefix | DXVK (`d3d9`, `d3d10core`, `d3d11`, `dxgi`) and VKD3D-Proton (`d3d12`, `d3d12core`) into `system32`, unconditionally: Many games find D3D12 through DXGI at run time. |
| **nvapi**, **dlss** | prefix | On every NVIDIA box, as Proton does: dxvk-nvapi, nvapi64 and nvofapi64 to `system32` and the 32-bit nvapi to `syswow64`, and the driver's own `nvngx` and `_nvngx` from its `nvidia/wine` directory. |
| **bridge** | prefix | Valve's `steamclient64.dll`, `steamclient.dll`, `GameOverlayRenderer64.dll` and `Steam.dll` into `Program Files (x86)\Steam` from Steam's `legacycompat/`, and quark's `steam.exe` and `lsteamclient.dll` into `system32`. |

### Wineserver Ahead of the Session

The wineserver needs only the prefix, so it is a node rather than something
the first Wine process starts on demand. It daemonizes with Wine's default
persistence of 3 s without a client, is listening by the time the deploys
and the environment are done, and ends on its own once the session is over.
A prestart that finds a server already holding the prefix's lock exits 2,
which is Wine's own signal that one is up.

### No Namespace

The compat tree is Wine's own install layout, and its `bin/wine` is a copy
of the system loader rather than a link to it. Wine resolves `lib/wine` and
`share/wine` relative to `/proc/self/exe`, so the patched modules load from
the tree with no bind mount and `/usr` is never written. A namespace would
cost a process per GPU initialization: An unprivileged user namespace
leaves root unmapped, the NVIDIA driver finds its device nodes owned by
`nobody:nobody`, and its userspace fork-execs the setuid `nvidia-modprobe`
to repair them, which cannot act inside the namespace, in every Wine
process that touches the GPU. The installer's smoke test proves the tree
is used: It runs the tree's `wine --version` under `LD_DEBUG=libs` and
fails unless the dynamic linker names the tree's `ntdll.so`.

### Deploy Cache

One stat per component decides what a launch redeploys: The Wine tree at
`lib/wine`, DXVK, VKD3D-Proton and Steam's `legacycompat/`. The cache file,
`.quark_deployed` in the prefix, holds the four component hashes, so a
launch over unchanged components does no DLL copy and no hive read. A
redeployed launcher binary beside the tree is not a new Wine tree. The
prefix's user comes from its registry's `ProfileList` in one byte scan of
the 4 MB `system.reg`, 3 ms where a line-by-line read took 285 ms a call
at about six calls a launch.

### The Steam Bridge

A Windows game talks to Steam through `steam_api64.dll`, which loads
`steamclient64.dll` from the path Steam's registry keys name and calls its
exports. Under Proton those exports are served by `lsteamclient`, a Wine
builtin whose PE side is the Steamworks interface and whose unix side
`dlopen`s Steam's own `linux64/steamclient.so` and talks to the running
client over its IPC. The join is a trampoline in ntdll, Proton's patch
ported to upstream Wine as `wine/001`: When a module named `steamclient64`
loads, ntdll fixes up its imports (the `tier0_s64` and `vstdlib_s64` it
links against are redirected to ntdll, and the installer builds stubs for
them as well), skips its DllMain, loads `lsteamclient.dll` beside it and
writes a trampoline from every export of Valve's DLL to the builtin's,
flushing the instruction cache after. The module is pinned so a
`FreeLibrary` can never unmap it from under a cached pointer. A full-path
load, which is how `steam_api64` loads it, takes the same path, so the
bridge needs no alias under Valve's names.

The prefix holds Valve's DLL, not the builtin under its name. The DllMain
the trampoline skips belongs to whatever loads as `steamclient64.dll`; with
the builtin there, the skipped DllMain is the bridge's own init, its unix
side never loads, and the game waits in `SteamAPI_Init` with a window open
and nothing drawn.

`steam.exe`, a Wine builtin built from Proton's steam_helper, is the first
process: It creates the Win32 events games check for a running Steam,
calls `steamclient_init_registry` so the registry holds the client paths,
creates the "Steam" window `FindWindow` looks for and spawns the game as
its child.

### Session and Stop

The game's command is `wine C:\windows\system32\steam.exe <game.exe>
[args]`, spawned in a process group of its own with default signal
dispositions, and the launcher holds the child's pidfd. Steam's Stop
reaches the launcher as SIGINT or SIGTERM. It forwards the signal to the
child through `pidfd_send_signal`, which cannot reach a reused pid, and to
the child's group by pgid, then ends the prefix with `wineserver -k`:
Wine starts its services and the game outside that group, and the server
runs in a session of its own, so the prefix's server is the one handle on
everything the launch started. A normal exit does the opposite and waits
for the prefix's server to exit on its own, the way Proton's
`waitforexitandrun` does, so the real game a launcher stub started
survives the stub. A Stop during that wait still ends it. The tool
manifest declares `use_tool_subprocess_reaper "1"`.

### Save-Data Protection

Before the spawn, the save directories under the prefix's profile are
snapshotted as hardlinks, which keep a file's data alive if the game
deletes it. After the session, a save file that existed before the launch
and is gone after it is restored from the snapshot; a file the game wrote
is never overwritten.

### Installer as a Build System

`install.py` is a dependency graph of seventeen steps. Each step declares
the steps it needs, the files it reads and the files it writes;
`graphlib.TopologicalSorter` orders them, every ready step runs at once on
a thread pool, and its log lines carry its name. The rebuilder is a
verifying trace [2]: On success a step records a hash of its inputs and a
hash of its outputs, and it is skipped only when both still match, so a
file another step rewrote brings its producer back rather than shipping
stale. The runner refuses a graph where two steps claim the same output,
a failed step's dependents are skipped by name, and a run either produces
the tree the serial order would or reports an error [3][4]. Patch Wine's
outputs include every source file its patches touch, read from the
patches' own `+++ b/` lines.

| Step | Needs | Does |
|---|---|---|
| Preflight | | The tools, `/dev/ntsync` (by opening it), Steam's root |
| Clean | Preflight | Stops each quark-mapped prefix's wineserver with `wineserver -k`, never a global kill, and removes only sockets nothing answers on |
| Sync wine source | Clean | The Wine source at the installed version, in `~/.cache/quark/wine-src` |
| Build | Clean | `cargo build --release` |
| Deploy binaries | Build | `quark` and the `proton` link into `compatibilitytools.d/quark/`, the two VDF manifests |
| Deploy Wine | Clean | The system Wine tree beside it, `bin/wine` a copy of the loader |
| Patch Wine | Sync, Deploy Wine | `git apply --check` then apply, each patch, then ntdll and winepulse built |
| Build stubs | Deploy Wine | `tier0_s64`, `vstdlib_s64` and their 32-bit twins, with clang and lld |
| Bake prefix | Patch Wine, Build stubs | Proton 11.0's `default_pfx` as the template, its relative symlinks repaired |
| Steam bridge | Patch Wine, Deploy Wine | lsteamclient and steam.exe from Proton 11.0's source; after Patch Wine, since both rewrite the source tree |
| GPU translation | Deploy Wine | DXVK 3.1.1, VKD3D-Proton 3.0.1 and dxvk-nvapi 0.9.2 from their release URLs, each verified by sha256 |
| Deploy drivers | Deploy Wine | The PE driver files |
| EAC runtime | Deploy Wine | The bridge DLLs from the Proton EasyAntiCheat Runtime tool, if installed |
| Trace | Patch Wine, Steam bridge | The observer's symbol files, with `--trace` |
| Prefix sync | the deploys | Every quark-mapped prefix gets real files where Proton left symlinks, then quark's layer on top |
| Finalize | Prefix sync | `env_config` if absent |
| Smoke test | Finalize | The tree's own `wine --version`, its `ntdll.so` proven by the dynamic linker |

### Wine Patches

The Wine source matching the installed Wine is cloned, the patches apply
strictly and in order, and only the modules they touch are built. A patch
that does not apply, or a module that builds but does not produce its
file, fails the install by name; there is no fuzzy apply.

| Patch | Mechanism |
|---|---|
| `wine/001-ntdll-steamclient-trampoline` | Proton's trampoline on upstream Wine 11.20. The renamed tier0/vstdlib import passes its new length, the redirect honors `PROTON_DISABLE_LSTEAMCLIENT`, the instruction cache is flushed after the jumps are written and WoW64 callers go through a 32-bit thunk. |
| `wine/002-ntdll-export-wine-unix-call` | Exports `__wine_unix_call` from the PE ntdll, which the EasyAntiCheat bridge DLLs import. |
| `wine/003-ntdll-eac-loadorder` | Builtin-first load order for the EasyAntiCheat bridge DLLs, so the runtime's bridge wins over the game's copy. |
| `wine/004-ntdll-thread-stack-floor` | A floor under small explicit stack reserves of 64-bit threads; WoW64 threads keep the reserve they ask for. |
| `wine/005-kernelbase-eac-launcher-detection` | `CreateProcess` sets `PROTON_EAC_LAUNCHER_PROCESS` only for a child whose version resource names it the EasyAntiCheat Launcher, through kernelbase's own version-info calls. |
| `wine/006-kernelbase-steam-openprocess-pid` | `OpenProcess` on Steam's placeholder PID opens the calling process. Deploys with 005. |
| `wine/007-winepulse-nonblocking-timing-update` | The timer loop never blocks on a PulseAudio timing update and keeps at most one in flight. |
| `lsteamclient/001-lsteamclient-link-stdcxx` | Links libstdc++ into the unix library. |
| `lsteamclient/002-lsteamclient-non-fatal-steam-call` | A failed Steam client call returns instead of faulting. |

### Anti-cheat

EasyAntiCheat follows Proton's model: The bridge DLLs come from Valve's
"Proton EasyAntiCheat Runtime" Steam tool, whose `v2/lib32` and `v2/lib64`
join `WINEDLLPATH` after the Wine tree's; patches 002 and 003 give them
their export and load order, and 005 marks the launcher process. VAC needs
nothing from quark; the game talks to the real Steam client through the
bridge.

## Architecture

```
rust/src/quark/
  main.rs              Entry point; the verb from argv
  cli.rs               The four verbs Steam speaks to a compatibility tool
  launcher.rs          Locate, the launch graph's nodes, the environment, the
                         spawn, the session and its end, save-data protection
  graph.rs             The launch graph's executor: Kahn's algorithm on scoped
                         threads, outputs keyed by node, per-node timings
  log.rs               The [HH:MM:SS] [LEVEL] macros
install.py             The installer: a step graph with verifying traces
quark_common.py        Paths, logger, Steam and game discovery, Wine process
                         hygiene; imported by install.py and every test
patches/
  wine/                001-007
  lsteamclient/        001-002
tests/
  run.py               The one entry point: suite, launch, scoreboard, qemu,
                         heap-repro
  harness/             The game harness: base, tools, procs, trace, metrics,
                         sched, baselines, launch, scoreboard
  rust/                launcher.rs: the cargo tests, compiling the source
                         modules they cover
  verify_patches.py    Deployed package and patch verification
  test_package.py      The compat tool layout
  test_steam_safety.py The Steam-safe scoping gate
  heap_repro/          The heap-corruption repro
  qemu/                guest.py (host side), suite.py (inside the VM),
                         probes/*.c
PKGBUILD
```

### Data Flow

```
Steam
  └─ proton waitforexitandrun <game.exe>          (quark, by its proton link)
       ├─ locate: Wine tree, Steam, prefix, cache stats
       ├─ launch graph
       │    registry ── prefix ──┬── wineserver (ahead of the session)
       │                         ├── dxvk, vkd3d, nvapi, dlss
       │                         └── bridge
       ├─ environment
       ├─ save snapshot (hardlinks)
       └─ spawn: bin/wine steam.exe <game.exe>    (own pgid, pidfd held)
            └─ steam.exe ── lsteamclient ── Steam client IPC
                 └─ game
       end: Stop → pidfd and pgid signal, wineserver -k
            exit → wait for the prefix's server, then save restore
```

## Requirements

- Wine 11.20 from the distribution; the installer clones its source at
  that version.
- `/dev/ntsync`, used when it opens. On Arch, `wine` pulls in
  `ntsync-autoload`; until the next boot, `sudo modprobe ntsync`.
- Steam, with the multilib repo enabled, a 32-bit Vulkan driver for the
  GPU and `lib32-libpulse` or `lib32-pipewire`.
- Proton 11.0 installed in Steam (Library → Tools), for the prefix template
  and the lsteamclient source tag.
- Rust 1.85+, clang and lld, llvm, git, patch, autoconf, bison, flex,
  base-devel, and the Wine build dependencies for ntdll and winepulse.
  `ccache` is used for the host compiler when present.

```bash
# Arch Linux
sudo pacman -S --needed steam wine wine-mono rust clang lld llvm git patch \
  autoconf bison flex base-devel gnutls libxslt libgphoto2 \
  libpulse alsa-lib alsa-plugins openal libxcomposite libxinerama \
  libxrandr libxcursor libxi libxxf86vm libxdamage libxfixes libxkbcommon \
  wayland fontconfig freetype2 sdl2-compat v4l-utils libusb \
  gst-plugins-base gst-plugins-good lib32-vulkan-icd-loader lib32-mesa \
  lib32-libpulse
```

Add the GPU's 32-bit Vulkan driver (`lib32-nvidia-utils`,
`lib32-vulkan-radeon` or `lib32-vulkan-intel`). `wine-mono` is what .NET
and FNA games run on.

## Build & Install

```bash
./install.py            # asks before anything destructive
./install.py --yes      # answers every prompt
./install.py --trace    # also turns the montauk observer on for every launch
```

Then restart Steam fully (Steam → Exit; Steam reads
`compatibilitytools.d` only at startup), open a game's Properties →
Compatibility, tick "Force the use of a specific Steam Play compatibility
tool" and pick **quark 0.3.0**. Steam fetches the Windows build of a game
it had installed natively.

## Usage

`~/.local/share/Steam/compatibilitytools.d/quark/env_config` holds
`KEY=VALUE` lines read on every launch; a line there overrides the
launcher's own value for the same key. Any of them also works for one game
in Steam's launch options, as `QUARK_VERBOSE=1 %command%`.

| Variable | Effect |
|---|---|
| `QUARK_VERBOSE=1` | The launcher's `[DEBUG]` lines, the per-node timing line among them, and Wine's `+process,err` channels |
| `QUARK_TRACE=1` | montauk beside the game, covering the game, steam.exe and the wineserver |
| `QUARK_TRACE_OUT=1` | With `QUARK_TRACE`, keep the capture as `/tmp/quark/incidents/<game>-io.bin` |
| `QUARK_WINE_DIR=/path` | Launch with another Wine tree than the one the installer deployed |
| `WINEDEBUG=...` | Passed through as given; otherwise `-all`, or `+process,err` under `QUARK_VERBOSE` |

### Logs

`/tmp/quark/`: `wine_stderr.log` and `wine_stdout.log` for the last
launch, `launcher_stderr.log` for the launcher's own lines, and
`incidents/` for the montauk capture, the process memory maps and a copy
of the game's stderr with its exit status.

### Diagnosing a Launch

A capture is analyzed before any log is read:

```bash
montauk --analyze /tmp/quark/incidents/<game>-io.bin --json        # the full report set
montauk --analyze <cap> --select ntsync --where comm=<game> --by tid,op,result --count --json
montauk --analyze <cap> --select exec --by comm --count --json
```

A report is most useful with the game's stderr and the launcher's, the
capture or its `--analyze --json` output, the GPU and driver, and whether
the same game reaches its menu under Proton Experimental on the same box
(`tests/run.py launch <appid> --tool proton-exp`).

## Testing

```bash
python3 tests/run.py                          # the suite, every layer
python3 tests/run.py suite --layer unit       # one layer (repeatable)
python3 tests/run.py launch 379720 --trace    # one game, under a montauk capture
python3 tests/run.py launch --all             # every installed Windows game
python3 tests/run.py qemu provision           # once: The VM image
python3 tests/run.py qemu test                # the suite in a disposable VM
python3 tests/run.py scoreboard               # the going-to-PROD step
python3 tests/run.py scoreboard --static-only # its static lane alone, no games
```

`tests/run.py` is the one entry point; every other file under `tests/` is
a module it calls, apart from `qemu/suite.py`, which runs inside the VM.
Each layer step runs as its own bounded subprocess, so a hung test fails on
its timeout, and a layer that cannot run reports SKIP, never PASS.

| Layer | Covers |
|---|---|
| unit | The Rust tests under `tests/rust/`, each a `[[test]]` target compiling the module it covers: Environment assembly, the deploy cache, DLL deployment, registry seeding, the profile user, the driver layout, the Steam client files, save-data restore and the `S:` drive |
| gate | Steam-safe process scoping, with no install: No bare-name kill anywhere in the harness, and discovery that returns nothing with no quark running |
| deploy | The deployed package's layout; every patch's added lines in the source, and the strings, symbols and PE exports it compiles in; the tree's loader a copy |
| launch | Balatro through the harness |
| qemu | The whole suite in a disposable Arch guest over KVM with no host root: The real install, then seven mingw probes through the launcher under montauk (Direct3D 11 through DXVK on lavapipe, audio, the window and message pump, the environment and registry, threads and sync, the Steam bridge with no Steam, and a GL plus audio stress) |

`launch` is the harness: Build, install, launch with the environment Steam
would set, a 50 ms watcher for the game's process and window, stderr
milestones, checkpoint screenshots, an optional capture, and a verdict:
`timeout` for a game still running at the deadline, `crash` or `exit` for
one that ended before it, decided by Wine's own crash lines since steam.exe
returns 0 for a game killed by signal. quark is the tool under test; a
stock Proton exists only behind `--tool`.

`scoreboard` is the going-to-PROD step, run by hand before a release and
never a layer. It runs montauk's static lane first: `montauk --static` over
the launcher, the installer and the suite, read with `montauk --analyze` as
the lexer's guards, each function's control flow graph and the static
analyzer's reachability; a block unreachable from entry fails the step
before any game launches. Then every game goes through quark and any Proton
tool named with `--tools` or `--all-tools`, once per scheduler with
`--schedulers pandemonium,eevdf`, written to `tests/scoreboard.md`,
`tests/scoreboard.json` and `tests/scoreboard.prom` after every cell.
Each cell runs three times by default and every run is archived under
`~/.cache/quark/archive/`; the report's Baselines section is montauk's
verdict over that archive by version (best, tied or not-best per cell, and
the change-point trajectory), with `--tools proton11` beside quark as the
arm that tells drift in the box from a change in quark. `--static-only`
runs the static lane alone.

## Attribution

- `lsteamclient` and `steam_helper` are built from the source of Valve's
  [Proton](https://github.com/ValveSoftware/Proton) 11.0, and `wine/001`
  ports Proton's steamclient trampoline to upstream Wine.
- [DXVK](https://github.com/doitsujin/dxvk),
  [VKD3D-Proton](https://github.com/HansKristian-Work/vkd3d-proton) and
  [dxvk-nvapi](https://github.com/jp7677/dxvk-nvapi) are deployed from
  their own releases.
- Wine is the distribution's, with the seven patches above.

## References

[1] A.B. Kahn. "Topological Sorting of Large Networks." *Communications of
the ACM* 5(11), 558-562, 1962.

[2] A. Mokhov, N. Mitchell, S. Peyton Jones. "Build Systems à la Carte:
Theory and Practice." *Journal of Functional Programming* 30, 2020.
[doi:10.1017/S0956796820000088](https://doi.org/10.1017/S0956796820000088)

[3] S. Spall, N. Mitchell, S. Tobin-Hochstadt. "Build Scripts with Perfect
Dependencies." *OOPSLA 2020*.
[doi:10.1145/3428237](https://doi.org/10.1145/3428237)

[4] S. Spall, N. Mitchell, S. Tobin-Hochstadt. "Forward Build Systems,
Formally." *CPP 2022*.
[doi:10.1145/3497775.3503687](https://doi.org/10.1145/3497775.3503687)

## License

GPL-2.0
