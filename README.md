# amphetamine

amphetamine is a Steam compatibility layer that utilizes triskelion, a lock-free wineserver replacement. Together they replace the entire Proton/wineserver stack for Steam.

## Replacing Proton

| | Proton | amphetamine |
|---|---|---|
| Launcher | Python (~2,000 lines) | Rust (1,557 lines, compiled) |
| Wineserver | Wine's C wineserver (26,000+ lines) | triskelion (6,448 lines Rust, lock-free) |
| Binary count | 3+ (script, wineserver, toolchain) | 1 (942 KB) |
| Dependencies | Python 3, runtime libraries | libc |
| Deployment cache | None (re-evaluates every launch) | v3 per-component (wine, dxvk, vkd3d, steam) |
| Prefix setup | Python shutil (readdir + copy per file) | getdents64 (32 KB bulk reads) + hardlinks |
| Wineserver sync | pthread mutexes, kernel locks | ntsync (kernel-native, Linux 6.14+), CAS/futex fallback |
| Timer precision | Fixed polling interval | timerfd (kernel-precise deadlines) |
| Message passing | All through wineserver socket | Shared-memory SPSC bypass |
| Save data | No protection | Pre-launch snapshot + post-game restore |

## Execution Stack

Steam calls amphetamine. amphetamine sets up the game prefix, deploys DXVK/VKD3D-Proton for both 32-bit and 64-bit, bridges the Steam client, injects runtime registry keys, and launches wine64 with triskelion as the wineserver. Games run.

```
Steam
  └─ amphetamine (proton binary = triskelion)
       ├─ Prefix setup from Proton's Wine template
       │    ├─ getdents64 bulk directory walking (32 KB buffer, one syscall per batch)
       │    ├─ Hardlinks for regular files (instant, same filesystem)
       │    ├─ Absolute symlinks resolved against Proton tree (1500+ DLL symlinks)
       │    └─ Repair mode: fixes broken symlinks and replaces stale files
       ├─ DXVK deployment (d3d9/10/11 → Vulkan)
       │    ├─ 64-bit → system32
       │    └─ 32-bit → syswow64
       ├─ VKD3D-Proton deployment (d3d12 → Vulkan)
       │    ├─ 64-bit → system32
       │    └─ 32-bit → syswow64
       ├─ Steam client bridge (steam.exe → lsteamclient → steamclient.so)
       ├─ Registry injection (VC++ 2015-2022, .NET Framework 4.8)
       ├─ Save data protection (pre-launch snapshot, post-game restore)
       ├─ Deployment cache (v3 per-component) — cache hit skips all file ops
       └─ wine64 with WINESERVER=triskelion
            ├─ ntsync kernel driver (Linux 6.14+) — native NT semaphore/mutex/event
            │    └─ Fallback: CAS + futex wake (older kernels)
            ├─ Shared-memory ring buffers (256 slots, atomic indices)
            ├─ Handle tables, process/thread state
            ├─ In-memory registry (HashMap values, O(1) lookup)
            ├─ timerfd-driven wait deadlines (kernel-precise)
            └─ epoll event loop (306 protocol opcodes, 41 handlers)
```

## Games Tested (v0.11.0)

| Game | Engine | Architecture | Status |
|------|--------|-------------|--------|
| Halls of Torment | Godot 4.3 (Vulkan) | 64-bit | Running. Sub-second cached launch. |
| TMNT: Shredder's Revenge | FNA / .NET (SDL2) | 64-bit | Running. Needed VC++ registry keys. |
| Balatro | Love2D | 64-bit | Near-instantaneous launch. |
| Wedding Witch | Custom | 64-bit | Steady. |
| Duke Nukem 3D: 20th Anniversary | Build engine (D3D) | 32-bit | Running. First 32-bit game tested. |

First launch sets up the prefix (~1500 symlinks + files via getdents64/hardlinks). Every launch after hits the deployment cache and skips straight to wine64.

## Anti-Cheat Compliance

amphetamine does not interfere with VAC, EAC, or BattlEye. Triskelion runs as a separate native Linux process — it communicates with Wine via Unix domain sockets and never appears in the game's memory maps. No game memory modification, no DLL hooking, no import table patching.

Steam's `compatibilitytools.d/` infrastructure exists for custom compatibility tools. Proton-GE and wine-tkg have operated for years with zero VAC ban incidents. amphetamine is not a cheat, a bypass, or a modification to game code.

## Performance

- **Stack-allocated replies** — Fixed-size replies use a `[u8; 64]` stack buffer (`Reply::Fixed`). Zero heap allocation per request. `Vec` used only for VARARG replies (registry ops, startup info — rare).

- **Reusable request buffers** — Per-event-loop accumulation buffer extracted via `std::mem::take()` and reused across all request dispatch cycles. After warmup, never reallocates.

- **getdents64 bulk reads** — Prefix setup reads directories via `SYS_getdents64` with 32 KB buffers. One syscall returns hundreds of directory entries. Replaces per-entry `readdir()`.

- **Hardlinks over copies** — Regular files in the prefix are hardlinked from the Proton template (same filesystem, near-zero cost). Copy is the cross-device fallback.

- **Per-component deployment cache** — v3 cache stores 4 independent hashes: wine, dxvk, vkd3d, steam. Each component invalidates independently. A DXVK update does not force a full prefix rebuild.

- **ntsync kernel driver** — On Linux 6.14+, sync primitives (semaphore, mutex, event) use `/dev/ntsync` — the kernel implements NT synchronization natively via ioctls. WaitForSingleObject/WaitForMultipleObjects resolve in a single ioctl instead of userspace CAS loops. Falls back to CAS + futex on older kernels.

- **Futex wake** — Sync primitives and message queues use direct `SYS_futex` calls with `FUTEX_WAKE` to notify blocked threads immediately. No polling loops. No pthread mutexes in server code. Used as the fallback path when ntsync is unavailable.

- **timerfd precision** — Select handler timeouts use `timerfd_create(CLOCK_MONOTONIC)`. The kernel delivers exact deadline wakeups via epoll. Replaces fixed-interval polling.

- **epoll event loop** — O(1) fd readiness notification. Listener, signals, timers, and all client fds handled in a single `epoll_wait()`. No select/poll linear scan.

- **O(1) fd extraction** — SCM_RIGHTS file descriptors stored in per-client `VecDeque`. `pop_front()` is O(1).

- **O(1) registry lookups** — Values stored in `HashMap<RegName, RegistryValue>` with a separate `Vec<RegName>` for insertion-ordered enumeration.

---

## Install

### Dependencies

- **Any Proton** (Experimental, 10.0, etc.) — Install via Steam (Steam → Library → search "Proton" → Install). amphetamine replaces Proton's launcher and wineserver but sources Wine binaries, DXVK, VKD3D-Proton, and the Steam client bridge from a Proton installation. These are independent components that Valve bundles inside Proton.
- **Rust** (1.85+, 2024 edition)

```bash
# Arch Linux
pacman -S rust

# Or via rustup
rustup default stable
```

```bash
./install.py
```

Builds triskelion, deploys to Steam's `~/.local/share/Steam/compatibilitytools.d/amphetamine/`, and patches Wine source for shared-memory message bypass.

Then select "amphetamine" as the compatibility tool for any game in Steam.

### Manual build

```bash
cargo build --release -p triskelion
cp target/release/triskelion ~/.local/share/Steam/compatibilitytools.d/amphetamine/proton
```

Requires Rust 2024 edition (rustc 1.85+). Single dependency: `libc`.

## Architecture

### amphetamine (Proton replacement)

`launcher.rs` — 1,557 lines of Rust that replace Proton's ~2,000-line Python script.

**Discovery**:
- Wine: `TRISKELION_WINE_DIR` → Proton Experimental → any Proton → system Wine
- Steam: `STEAM_COMPAT_CLIENT_INSTALL_PATH` → `~/.steam/root` → `~/.local/share/Steam`

**Prefix setup**:
- Copies Proton's `default_pfx/` template using `getdents64` (Linux kernel syscall for bulk directory entry reads — one syscall fills a 32 KB buffer with hundreds of entries)
- Regular files: hardlink first (instant, shares disk blocks), copy fallback for cross-device
- Symlinks: resolved via `canonicalize()` against the Proton source tree, written as absolute symlinks (Proton's relative symlinks like `../../../../../lib/wine/...` break when copied to the game prefix)
- Repair mode: detects broken symlinks and regular-files-that-should-be-symlinks from previous deploys, replaces them

**DLL deployment**:
- DXVK: `d3d11.dll`, `d3d10core.dll`, `d3d9.dll`, `dxgi.dll` — both 64-bit (system32) and 32-bit (syswow64)
- VKD3D-Proton: `d3d12.dll`, `d3d12core.dll` — both 64-bit and 32-bit
- Removes destination before copy (hardlinked files from Proton are read-only; unlinking is a directory op, always allowed)
- Conditional: skips files that match by size and mtime

**Steam client bridge**:
- `steamclient64.dll`, `steamclient.dll`, `GameOverlayRenderer64.dll`, `Steam.dll`, `steam.exe` from Steam's `legacycompat/`
- `LD_LIBRARY_PATH` includes `~/.steam/root/linux64/` for `steamclient.so`

**Registry injection**:
- VC++ 2015-2022 Redistributable (x64 and x86 WOW6432Node)
- .NET Framework 4.8
- Runs on every launch with idempotency check (games like TMNT: Shredder's Revenge check registry keys before loading DLLs)

**Deployment cache**:
- `.triskelion_deployed` stores `v3:<wine_hash>,<dxvk_hash>,<vkd3d_hash>,<steam_hash>`
- Per-component hashing: each hash = `dev * prime ^ ino * prime ^ mtime`
- Cache hit: all file operations skipped, straight to wine64
- Components invalidate independently (DXVK update doesn't force prefix rebuild)

**Save data protection**:
- Pre-launch: snapshots all save data under `pfx/drive_c/users/steamuser/` (`AppData/Roaming`, `AppData/Local`, `AppData/LocalLow`, `Documents`) to `$STEAM_COMPAT_DATA_PATH/save_backup/`
- Post-game: compares backup against originals — restores only files that existed before launch but are now missing (Steam Cloud sync wipe). Never overwrites saves the game just wrote.
- Skips system directories (`Microsoft/`, `Temp/`) and empty folder trees
- Always runs, every launch — the cost is trivial (save data is typically KB to low MB)
- Backup is cleaned up automatically when saves are intact; kept as safety net when files were restored

**Launch**:
- `wine64 c:\windows\system32\steam.exe <game.exe>` — through Wine's built-in Steam bridge
- `WINEDLLOVERRIDES` sets DXVK/VKD3D to native, steam.exe to builtin
- `WINEDLLPATH` orders vkd3d before wine (vkd3d-proton shadows Wine's stubs)
- Opcode tracing: `touch /tmp/amphetamine/TRACE_OPCODES` or set `AMPHETAMINE_TRACE_OPCODES`

### triskelion (wineserver replacement)

*Quocunque Jeceris Stabit*

Lock-free wineserver replacement. 942 KB binary, single dependency (libc), 41 handlers across 306 opcodes.

| Leg | File | Domain |
|-----|------|--------|
| 1: queue | `queue.rs` | Per-thread SPSC ring buffers (256 slots). Cache-line aligned atomics. Shared memory in `/dev/shm`. Futex wake on post/send. |
| 2: sync | `ntsync.rs` | ntsync kernel driver (Linux 6.14+) for native NT semaphore/mutex/event via `/dev/ntsync` ioctls. |
| 3: objects | `objects.rs` | Handle tables (dense array + free list), process/thread state, Windows handle encoding. |

**Protocol**: 306 opcodes auto-generated from Wine's `protocol.def` by `build.rs` (829 lines). 41 handlers with logic; rest return `STATUS_NOT_IMPLEMENTED`. Adding a handler = one function.

**IPC**: Unix domain socket with SCM_RIGHTS fd passing. Per-client accumulation buffers with request pipelining. Variable-length reply support (VARARG) for startup info and registry.

**Event loop**: epoll hub with timerfd for deadline precision. Stack-allocated `[u8; 64]` replies for fixed-size opcodes. Reusable request buffers via `std::mem::take()`. Deferred replies for select with timeout (arm timerfd, check on expiry).

**Select handler**: Wine's universal wait mechanism. Handles polls, object waits, and deferred sleep with real timeouts. On ntsync-capable kernels, select polls kernel objects via `NTSYNC_IOC_WAIT_ANY`/`WAIT_ALL` ioctls for immediate acquisition. fsync/esync enabled as fallback for non-ntsync operations.

### Shared-Memory Message Bypass

PostMessage/GetMessage bypass the wineserver entirely via shared-memory SPSC ring buffers. Patched into Wine's ntdll and win32u:

- **ntdll**: `triskelion.c` (964 lines C) intercepts PostMessage/GetMessage via shared-memory rings, and bypasses wineserver for ntsync sync operations
- **win32u**: `triskelion_has_posted()` forces server call path when ring has messages
- **Bridge**: `TEB->glReserved2` passes queue pointer from ntdll to win32u

### Dynamic Protocol Codegen

`build.rs` (829 lines) parses `protocol.def` → `RequestCode` enum, 306 request/reply structs, `RequestHandler` trait, `dispatch_request()`. Handles Proton's enum divergence (esync/fsync/flush_key_done entries that shift values vs upstream Wine).

Wine source resolution: `WINE_SRC` → `~/.local/share/amphetamine/wine-src/` → `/tmp/proton-wine` → committed fallback.

## Project Structure

```
amphetamine/
  install.py               Build + deploy + Wine patching pipeline
  amphetamine/              triskelion Rust crate (6,448 lines)
    build.rs                protocol.def codegen (829 lines, 306 opcodes)
    include/
      triskelion_shm.h      C header matching Rust shm layout
    src/
      main.rs               Entry point, signal handling, socket path
      launcher.rs            Full Proton replacement layer (1,557 lines)
      event_loop.rs          epoll hub, handler dispatch (1,457 lines)
      profile.rs             strace/perf profiling harness (637 lines)
      ipc.rs                 Unix socket IPC with SCM_RIGHTS
      objects.rs             Handle tables, process/thread state
      registry.rs            In-memory registry tree (HashMap values)
      queue.rs               SPSC ring buffer message queues (futex wake)
      ntsync.rs              ntsync kernel driver wrapper (/dev/ntsync ioctls)
      shm.rs                 Shared memory management
      protocol.rs            Protocol types and dispatch
      packager.rs            Steam compatibility tool packaging
      configure.rs           Wine configure generation
      clone.rs               Valve Wine source cloner
      cli.rs                 CLI argument parsing
      gaming.rs              Gaming DLL/program definitions (100 DLLs, 26 programs)
      pe_patch.rs            PE .idata section patcher
      analyze.rs             DLL surface area analysis
      status.rs              Project status reporting
      log.rs                 Logging macros
  patches/
    wine/dlls/ntdll/unix/triskelion.c      Shared-memory bypass + ntsync shadow table (964 lines C)
    wine/dlls/win32u/triskelion_message.c  win32u peek_message integration reference
    APPLY.md                                Patch application guide
  tests/
    triskelion-tests.py      Integration test harness (deploy, launch, profile, compare)
    amphetamine-tests.py     amphetamine-specific tests
    test_package.py          Package integrity tests (47 tests)
```

## Steam Integration

```
~/.local/share/Steam/compatibilitytools.d/amphetamine/
  compatibilitytool.vdf     Steam discovery metadata
  toolmanifest.vdf          Invocation: /proton %verb%
  proton                    triskelion binary (942 KB)
```

Steam calls `./proton waitforexitandrun <game.exe>`. triskelion's CLI parses it as launcher mode, sets up the environment, launches wine64 with `WINESERVER=$SELF`, and when wine64 calls back to wineserver — it's talking to another instance of itself.

## Testing

47 package integrity tests (test_package.py). Integration suite covers deploy, launch, profile, and multi-game comparison (triskelion-tests.py). Orchestrator covers patch, build, bypass, package, and compat validation (amphetamine-tests.py).

```bash
# Integration tests
python3 tests/triskelion-tests.py test-deploy          # build + deploy
python3 tests/triskelion-tests.py test-launch           # launch game via Steam
python3 tests/triskelion-tests.py test-iterate          # build → deploy → launch → diagnose
python3 tests/triskelion-tests.py test-profile          # capture opcode + timing profile
python3 tests/triskelion-tests.py test-compare --game hot --game hades  # diff multiple games

# Package integrity (47 tests)
python3 tests/test_package.py
```

### Debugging

```bash
# Enable opcode tracing (writes to /tmp/amphetamine/opcode_trace.log)
touch /tmp/amphetamine/TRACE_OPCODES

# Check launcher timing
cat /tmp/amphetamine/launcher_timing.json

# Force full redeploy (clear all caches)
find ~/.steam/root/steamapps/compatdata/ -name ".triskelion_deployed" -delete

# Nuke a specific game prefix for clean redeploy
rm -rf ~/.steam/root/steamapps/compatdata/<app_id>/pfx
```

## Tooling

triskelion is a multi-mode binary:

```bash
triskelion server                           # wineserver daemon
triskelion <verb> <exe>                     # Proton launcher
triskelion package <wine_dir>               # package as Steam compat tool
triskelion configure <wine_dir> [--execute] # Wine ./configure with 631 --disable-* flags
triskelion clone                            # clone Valve Wine (proton_10.0)
triskelion status                           # project status
triskelion analyze                          # Wine DLL surface area
triskelion profile <app_id>                 # strace profiling
triskelion profile-attach                   # attach to running game
triskelion profile-compare                  # compare profile outputs
triskelion profile-opcodes                  # analyze opcode traces
```

## License

GPL-2.0
