# quark · triskelion · PARALLAX

ATTENTION: quark is a still-in-development software.

Three Rust binaries replacing Proton, wineserver, and explorer.exe. quark replaces Proton's Python launcher. triskelion replaces Wine's 26,000-line C wineserver with a ~18,000-line Rust daemon built on `/dev/ntsync` kernel primitives, CSP event loop, shared-memory message queues, and adaptive message routing. PARALLAX replaces explorer.exe with DRM/KMS display enumeration and shared-memory hardware data.

Three binaries, one crate. Drops in as a Steam compatibility tool.

```
Steam
  └─ quark (Proton replacement launcher)
       ├─ triskelion (wineserver replacement daemon)
       │    ├─ CSP event loop (authority + I/O threads, flume channels)
       │    ├─ /dev/ntsync (kernel-native NT semaphore/mutex/event)
       │    ├─ Wine protocol v931 (306 opcodes, 246 with handlers)
       │    └─ Adaptive routing (PANDEMONIUM-inspired observe/classify/decide/persist)
       ├─ PARALLAX (display compositor, DRM/KMS enumeration)
       │    └─ shared memory → triskelion (GPU, connectors, modes, DEVMODEW)
       ├─ Wine client (system Wine, winex11.drv via XWayland)
       │    └─ /dev/ntsync ioctls (inproc sync, bypasses daemon)
       ├─ steam.exe (Wine builtin, built from Proton steam_helper source)
       │    └─ lsteamclient.dll/.so (built from Proton source, patched for system Wine)
       ├─ DXVK (d3d9/d3d10/d3d11/dxgi → Vulkan, deployed to prefix)
       ├─ VKD3D-Proton (d3d12 → Vulkan, deployed to prefix)
       └─ EAC bridge (from Proton EasyAntiCheat Runtime, Steam tool 1826330)
```

## Status

| Game | Engine | Requests | Status |
|------|--------|----------|--------|
| Hollow Knight | Unity/Mono | 3,558 | PLAYABLE (renders, interactable, Steam API) |
| Wedding Witch | Unity/Mono | 2,088 | PLAYABLE (renders, interactable, Steam API) |
| Balatro | LOVE2D/SDL2 | 1,168 | x11drv, LOVE runs, PHYSFS require error (game code) |
| Halls of Torment | .NET | 2,102 | x11drv, Mono loads, kernelbase crash |
| TMNT Shredder's Revenge | FNA/.NET | 1,962 | x11drv, Mono + FNA loads, kernelbase crash |
| Duke Nukem 3D | Native i386 | 174 | WoW64 loads, 32-bit DLLs chain, needs Steam IPC |

## quark, Proton Replacement

| | Proton | quark |
|---|---|---|
| Launcher | Python (~2,000 lines) | Rust (1,717 lines, compiled) |
| Wineserver | Wine's C wineserver (26,000+ lines) | triskelion (~17,000 lines Rust) |
| Display | explorer.exe (X11/Wayland via Wine) | PARALLAX (~1,036 lines Rust, DRM/KMS) |
| Binary count | 3+ (script, wineserver, toolchain) | 3 (quark, triskelion, parallax) |
| Dependencies | Python 3, runtime libraries | libc, flume, rustc-hash |
| Deployment cache | None (re-evaluates every launch) | v4 per-component (wine, dxvk, vkd3d, steam, pe_scan) |
| Prefix setup | Python shutil (readdir + copy per file) | getdents64 (32 KB bulk reads) + hardlinks |
| Wineserver sync | pthread mutexes, kernel locks | `/dev/ntsync` (kernel-native NT sync) |
| Timer precision | Fixed polling interval | timerfd (kernel-precise deadlines) |
| Message passing | All through wineserver socket | Shared-memory SPSC bypass |
| Display driver | winex11.drv (XWayland) | winex11.drv (XWayland), winewayland.drv planned |
| Save data | No protection | Pre-launch snapshot + post-game restore |
| Steam bridge | In-process game loading | Wine builtin steam.exe + lsteamclient (built from Proton source) |
| EAC integration | wine-valve address space model | Bridge DLLs from Proton EAC Runtime + Wine patches |
| GPU translation | Bundled DXVK/VKD3D | Downloaded from upstream + Proton drivers deployed |

## triskelion, wineserver Replacement

| | Wine wineserver | triskelion |
|---|---|---|
| Architecture | Single-threaded select() loop | CSP: authority thread + I/O thread, flume channels |
| Protocol | 306 opcodes (C, hand-written dispatch) | 306 generated, 246 with handlers |
| Handle tables | Linked list + linear search | HeapSlab (O(1) bump alloc, generation counters) |
| SHM thread slots | N/A | MmapSlab (O(1) alloc/free, LIFO free list) |
| Sync primitives | In-process futex/poll | `/dev/ntsync` kernel ioctls |
| NT timers | Full lifecycle | set/cancel/get_timer_info with authority-tick expiry |
| Process memory | /proc/pid/mem | process_vm_readv/writev syscalls |
| Display modes | Driver callback → registry | PARALLAX → DEVMODEW serialization → registry |
| APC delivery | wait_fd + SIGUSR1 | Same: worker interrupt + SIGUSR1 for inproc waits |
| Alert lifecycle | Signal for APC_USER only | Same: daemon never signals alert for system APCs |
| Async I/O | async_set_result in APC destructor | Two-phase: STATUS_KERNEL_APC -> prev_apc -> deferred event |
| Named pipes | Full async state machine | Create, listen (sync+overlapped), connect, transceive, blocking PIPE_WAIT |
| Registry | On-disk hive files | In-memory tree, persisted on shutdown |
| Process lifecycle | fork + exec tracking | new_process, init_thread, exit events, job objects, completion ports |

## PARALLAX, explorer.exe Replacement

| | Wine explorer.exe | PARALLAX |
|---|---|---|
| Runtime | Windows PE process inside Wine | Native Linux binary |
| Display enumeration | Win32 EnumDisplayDevices (through Wine driver) | DRM/KMS ioctls (direct kernel interface) |
| GPU detection | Driver-reported via winex11/winewayland | PCI vendor/device from /dev/dri/card*, sysfs |
| Monitor detection | Driver-reported display modes | EDID parsing (names, manufacturer codes, physical size) |
| Mode data | Driver → registry (DEVMODEW) | PARALLAX → triskelion → DEVMODEW serialization → registry |
| Data sharing | Registry writes inside Wine prefix | POSIX shared memory (`/parallax-<hash>`, seqlock) |
| Desktop window | explorer.exe creates and manages it | triskelion pre-creates at daemon startup |
| Startup cost | Fork + exec + DLL load + message loop | Single enumeration pass, exits immediately |
| Future DSR | N/A | Lanczos-2 compute shader (planned) |

## Architecture

### quark

- **Deployment cache** -- Per-component hashes (wine, dxvk, vkd3d, steam, pe_scan). Cache hit skips all file I/O, straight to `wine64`.
- **Prefix setup** -- `getdents64` bulk directory reads + hardlinks. Falls back to copy for cross-device.
- **Registry injection** -- Template-based from stock `~/.wine` prefix. Quark-specific overrides (display driver, Steam paths) injected on top.
- **Death-pipe lifecycle** -- Launcher creates pipe, passes read end to triskelion via QUARK_DEATH_FD. When launcher exits, triskelion gets POLLHUP and shuts down gracefully. Socket file and PID file cleaned up on exit.

`launcher.rs` (1,717 lines) replaces Proton's Python script.

**Discovery**: Wine from `QUARK_WINE_DIR` -> Proton Experimental -> any Proton -> system Wine. Steam from `STEAM_COMPAT_CLIENT_INSTALL_PATH` -> `~/.steam/root`.

**Prefix**: `getdents64` bulk reads + hardlinks from Wine's `default_pfx/`. Repair mode fixes broken symlinks from previous deploys.

**DLLs**: DXVK (`d3d11`, `d3d10core`, `d3d9`, `dxgi`), VKD3D-Proton (`d3d12`, `d3d12core`) -- 64-bit and 32-bit deployed to prefix. Proton drivers (`sharedgpures.sys`, `nvcuda.dll`, `amd_ags_x64.dll`, `amdxc64.dll`, `atiadlxx.dll`, `dxcore.dll`, `audioses.dll`, `belauncher.exe`) deployed from Proton's PE tree. Always deployed defensively (launcher stubs often have zero D3D imports).

**Steam integration**: Game launched through `wine64 C:\windows\system32\steam.exe <game.exe>`. steam.exe (Proton's steam_helper, built from source) creates Win32 events, connects to the running Steam daemon via native steamclient.so, writes `ActiveProcess\PID=0xfffe`, then spawns the game as a child via CreateProcess.

**Save protection**: Pre-launch snapshot of save directories. Post-game restore of files deleted by Steam Cloud sync.

**Logging**: Silent by default. `./install.py --verbose` or `QUARK_VERBOSE=1` enables diagnostics. Three tiers: default (`-all`), verbose (`+module,+loaddll,+process,err`), trace (`+server,+timestamp`).

**Per-request dispatch overhead** measured via CLOCK_MONOTONIC_RAW. Opcode stats dumped on shutdown with per-handler timing breakdown (total ns, avg ns/request, requests/sec).

### triskelion

*Quocunque Jeceris Stabit*

- **CSP architecture** -- Authority thread owns all mutable state (EventLoop, handle tables, process/thread maps). I/O thread owns epoll and all file descriptors. Communication via bounded flume channels. No shared mutable state, no locks on the hot path.
- **Zero-alloc hot path** -- Fixed-size replies use a `[u8; 64]` stack buffer. Accumulation buffers reused via `std::mem::take()`. After warmup, the request path never allocates.
- **Slab allocators** -- HeapSlab for handle tables (O(1) alloc, bump-only for Wine fd cache compatibility, LIFO free list with generation counters). MmapSlab for SHM thread slots (O(1) alloc/free, metadata on heap, data in caller-owned mmap).
- **epoll + timerfd** -- O(1) fd readiness. Wait deadlines use `timerfd_create(CLOCK_MONOTONIC)` for kernel-precise wakeups. No polling loops.
- **Effect system** -- RegisterClient, SendFd, WatchPipeFd, WatchQueueFd, RearmQueueFd. I/O thread executes effects before writing replies, guaranteeing fd ordering.
- **ntsync inproc** -- With `WINE_NTSYNC=1`, Wine clients perform synchronization directly via `/dev/ntsync` ioctls, bypassing the daemon entirely for wait/signal operations. The daemon only handles creation, destruction, and APC delivery.
- **Split alert/interrupt** -- Thread alerts (returned to Wine for inproc waits) are never signaled by the daemon. A separate auto-reset worker interrupt event wakes daemon-side ntsync worker threads. System APCs delivered via `tgkill(SIGUSR1)` matching stock wineserver's `send_thread_signal`. Wine's SIGUSR1 handler calls `wait_suspend` -> `server_select(SELECT_INTERRUPTIBLE)` -> daemon delivers the APC inside the signal handler.
- **pending_fd ordering** -- All ntsync fd sends go through the I/O thread (effects executed before reply write), guaranteeing fd arrives before the reply that references it.
- **Two-phase APC** -- Pipe listen completion: queue APC_ASYNC_IO -> STATUS_KERNEL_APC -> `invoke_system_apc` (irp_completion writes IOSB) -> prev_apc ACK -> deferred event signal. Matches stock wineserver's `async_set_result` flow. Prevents rpcrt4 pipe floods.
- **Display mode pipeline** -- PARALLAX enumerates DRM/KMS modes -> shared memory -> triskelion reads at startup -> serializes DEVMODEW structs (188 bytes each) -> writes Modes/ModeCount/Current/Registry/GPUID to registry -> Wine's NtUserEnumDisplaySettings reads from registry.
- **NT kernel timers** -- set_timer/cancel_timer/get_timer_info with real deadlines. check_nt_timers() in authority tick signals expired timers, reschedules periodic ones.
- **SIGTERM + death-pipe** -- Graceful shutdown on signal or launcher exit. Socket file and PID file cleaned up.

**FxHashMap** (rustc-hash) replaces std HashMap for all hot-path lookups: client table, handle table, ntsync objects, pending waits. Measurably faster hashing for integer keys.

**Protocol**: 306 opcodes auto-generated from Wine's `protocol.def` by `build.rs`. Handles Proton's enum divergence (esync/fsync entries shift opcode values vs upstream Wine). 246 opcodes have handlers. Adding a handler = one function in the appropriate event_loop module.

**Auto-stub**: If a handler returns STATUS_NOT_IMPLEMENTED, the dispatch layer consults intel.rs to decide whether to convert it to a zeroed success reply. Vararg-reply opcodes are never auto-stubbed (would corrupt the wire format). Esync/fsync opcodes are never auto-stubbed (client needs NOT_IMPLEMENTED to fall back to server-side waits). Handle-returning opcodes are never auto-stubbed (zeroed handle=0 causes Wine crashes). Panic recovery: dispatch wraps every handler in catch_unwind. A panicking handler returns STATUS_INTERNAL_ERROR instead of killing the daemon.

**IPC**: Unix domain socket per process (shared by all threads). SCM_RIGHTS for fd passing. Per-thread request/reply/wait pipes created during init_thread. Variable-length replies (VARARG) for startup info, registry, and APC data.

**Event loop**: CSP authority thread processes requests sequentially. I/O thread manages epoll hub, executes effects (RegisterClient, SendFd), and writes replies. `timerfd` for wait deadlines. Select returns STATUS_PENDING on reply_fd for deferred waits; immediate results return directly in the reply. Linger timer (5s) bridges the gap between wineboot exit and game connect.

**Event loop modules**:

| Module | Lines | Handlers | Purpose |
|--------|-------|----------|---------|
| window.rs | 2,999 | 89 | Window tree, messages, desktop, input, hooks, clipboard |
| mod.rs | 1,413 | 0 | EventLoop struct, shared helpers, display data |
| sync.rs | 1,345 | 25 | Select, events, mutexes, semaphores, APCs |
| file_io.rs | 1,259 | 24 | Files, mappings, PE image info |
| thread.rs | 964 | 14 | Thread lifecycle, startup info synthesis |
| completion.rs | 749 | 34 | Completion ports, jobs, timers, sockets, devices |
| process.rs | 734 | 16 | Process lifecycle, memory read/write |
| pipes.rs | 710 | 3 | Named pipes, two-phase APC, PIPE_WAIT |
| handles.rs | 620 | 14 | Handle table, dup, object info |
| registry_handlers.rs | 381 | 17 | Registry opcodes |
| client.rs | 231 | 0 | Connection lifecycle, exit events, SHM slot free |
| dispatch.rs | 184 | 0 | Opcode routing, auto-stub, panic recovery |
| token.rs | 139 | 10 | Security tokens, privileges |
| sent_messages.rs | ~160 | 0 | Adaptive message routing: per-msg_code profiles, persistence via intel.rs |

### Adaptive Message Routing

Replaces the earlier Sibyl module. Cross-process SendMessage routing inspired by PANDEMONIUM's procdb: observe, classify, decide, persist. Profiles persist across launches in .quark_cache v3 (intel.rs Section 6).

| | Stock wineserver | triskelion |
|---|---|---|
| Sent message routing | Static: same-process inline, cross-process via message_result chain | Adaptive: tracked default, fast-path when reply patterns learned |
| Cold start behavior | Full infrastructure from first message | All cross-process sends tracked (correct Wine semantics, won't break games) |
| Learning | None | Per-msg_code vote counting (fast vs tracked), promotion at 90% confidence after 8 observations |
| Persistence | None | .quark_cache v3 Section 6 (up to 256 msg_code profiles per game) |
| Daemon-owned windows | Desktop has message loop (explorer.exe) | Always fast-path (no message loop exists) |

**Sent message fork** (in handle_send_message):
- **Same-process**: Sender blocks (tracked). MSG_OTHER_PROCESS rewritten to MSG_UNICODE so Wine skips unpack_message.
- **Cross-process, daemon-owned target**: Immediate QS_SMRESULT (desktop/msg windows have no message loop).
- **Cross-process, promoted**: Learned fire-and-forget. Ring buffer + immediate QS_SMRESULT.
- **Cross-process, tracked**: Ring buffer + QS_SENDMESSAGE wake on receiver. No QS_SMRESULT until reply_message.

**ntsync lifecycle**:
- Device fd and per-thread alert fd sent to Wine via pending_fd (guaranteed ordering).
- Wine performs inproc waits directly via `/dev/ntsync` ioctls, bypassing the daemon.
- Daemon-side waits use a separate auto-reset worker interrupt event as `alert_fd` -- never touches the thread's inproc alert.
- System APCs (APC_ASYNC_IO) interrupt inproc waits via `tgkill(SIGUSR1)`. Wine's signal handler calls `wait_suspend` -> `server_select(SELECT_INTERRUPTIBLE)` -> daemon delivers the APC inside the signal handler.
- Thread alerts are never signaled by the daemon for system APCs. Stock wineserver only signals alerts for `APC_USER` (Wine `thread.c:1339`).

**Named pipes**: Create, listen (synchronous + overlapped), connect, disconnect, transceive, and blocking PIPE_WAIT for pipes that don't exist yet. Two-phase APC handshake for async completion: STATUS_KERNEL_APC -> invoke_system_apc -> prev_apc -> deferred event signal. Completion port integration for rpcrt4 worker threads.

**Registry**: In-memory tree (HashMap of keys, Vec of values). Loaded from prefix `*.reg` files at startup. Symlink resolution for `CurrentControlSet` -> `ControlSet001`. DEVMODEW display mode serialization from PARALLAX data. Service entries force-overwritten on startup to prevent stale prefix data. Saved on shutdown and on last-user-process exit.

**Process lifecycle**: new_process with handle inheritance, init_first_thread/init_thread with startup info synthesis (curdir + imagepath + cmdline), thread suspend/resume, exit events, job objects with completion port notifications, process idle events (WaitForInputIdle), system PID tracking for shutdown. Process memory read/write via process_vm_readv/writev.

**Session diagnostics**: Prometheus exposition format (.prom) output per session. Tracks uptime, total requests, peak clients, process/thread init counts, ntsync object stats, freelist efficiency, per-opcode counts. Written to log_dir with timestamp filenames.

### PARALLAX

PARALLAX replaces Wine's explorer.exe for display management. Instead of spawning a Windows process that talks to the display server through Wine's driver layer, PARALLAX runs as a native Linux binary that enumerates display hardware directly via DRM/KMS ioctls and writes the results to POSIX shared memory. triskelion reads this shared memory at startup, serializes DEVMODEW structs (188 bytes per mode), and populates the registry with real GPU and monitor data for Wine's NtUserEnumDisplaySettings.

- **DRM/KMS enumeration** -- GPU info (PCI vendor/device/subsys/revision, driver, bus ID), connector info (type, EDID, physical size), all display modes (resolution, refresh rate)
- **EDID parsing** -- Monitor names and manufacturer codes extracted from raw EDID binary data
- **Shared memory** -- `/parallax-<hash>` segment with seqlock, read by triskelion at startup
- **Desktop window** -- Pre-created by triskelion at daemon startup (no explorer.exe needed), `desktop_ready` set immediately
- **Mode data** -- All available modes per connector fed into DEVMODEW serialization (Modes, ModeCount, Current, Registry, GPUID registry values)
- **Future DSR** -- Lanczos-2 compute shader (planned)

## Wine Patches

| Patch | Target | Purpose |
|-------|--------|---------|
| 001-ntdll-guard-NtFilterToken-null-deref | ntdll | Null deref guard |
| 002-ntdll-create-process-heap-before-loader-lock | ntdll | Heap before loader lock |
| 003-win32u-soften-user-lock-assert | win32u | Soften USER lock assert |
| 004-win32u-guard-null-shared-object-deref | win32u | Null shared object guard (seqlock/id) |
| 009-ntdll-steamclient-authentication-trampoline | ntdll | Steam auth trampoline (PE + Unix) |
| 010-kernelbase-steam-openprocess-pid-hack | kernelbase | OpenProcess(0xfffe) PID substitution |
| 011-ntdll-eac-runtime-dll-path | ntdll/unix/loader | PROTON_EAC_RUNTIME DLL path injection |
| 012-ntdll-eac-loadorder | ntdll/unix/loadorder | EAC builtin/native load order |
| 013-kernelbase-eac-launcher-detection | kernelbase | PROTON_EAC_LAUNCHER_PROCESS env |
| 015-ntdll-export-wine-unix-call | ntdll | __wine_unix_call PE export for EAC |

lsteamclient patches (applied to Proton source during build):

| Patch | Purpose |
|-------|---------|
| 004-configure-add-lsteamclient-dll | Register lsteamclient in Wine configure |
| 005-configure-add-steam-helper | Register steam_helper in Wine configure |
| 006-lsteamclient-wine11-api-compat | Path API compat for system Wine |
| 007-lsteamclient-link-stdcxx | Link libstdc++ |

All wine patches applied automatically by install.py (`sorted(patch_dir.glob("*.patch"))`).

## Anti-Cheat

triskelion itself does not interfere with VAC, EAC, or BattlEye. It runs as a separate native Linux process -- communicates with Wine via Unix domain sockets and never appears in the game's memory maps. No game memory modification, no DLL hooking, no import table patching.

EAC integration uses Valve's official Proton EasyAntiCheat Runtime (Steam tool 1826330). Wine patches (011-013, 015) add DLL path injection, load order overrides, launcher process detection, and __wine_unix_call export so the bridge DLLs load correctly. The bridge .so files handle all Unix-side EAC communication.

## Install

### Dependencies

- **Linux 6.14+** with `/dev/ntsync` enabled
- **Wine 11.5+** (detected from system, never pinned)
- **Rust 1.85+** (2024 edition)
- **clang + lld** (for PE stub DLLs — no mingw needed)
- **autoconf, make** (for patched Wine DLL builds)
- **Proton EasyAntiCheat Runtime** (Steam tool, optional, for EAC games)

Optional:
- **wine-mono** (for .NET/FNA games: TMNT, Halls of Torment)
- **lib32-wine** (for 32-bit games: Duke Nukem 3D, Half-Life 2)

```bash
# Arch Linux / CachyOS
pacman -S wine rust clang lld autoconf base-devel
pacman -S wine-mono      # optional, for .NET/FNA games
pacman -S lib32-wine      # optional, for 32-bit games
```

```bash
./install.py
```

The installer:
1. Builds all three binaries (`cargo build --release` -- quark, triskelion, parallax)
2. Deploys system Wine tree (hardlinks)
3. Syncs PE DLLs to game prefixes
4. Downloads + deploys DXVK and VKD3D-Proton
5. Builds lsteamclient.dll/.so + steam.exe from Proton source (patched for system Wine)
6. Applies wine patches (001-015) and builds patched ntdll + kernelbase + win32u
7. Deploys EAC bridge DLLs from Proton EAC Runtime (warns if not installed)
8. Creates Steam compatibility tool VDFs (proton symlink -> quark, bin/wineserver -> triskelion)

Then select **quark** as the compatibility tool for any game in Steam.

### PKGBUILD

An Arch Linux PKGBUILD is included for system-wide installation:

```bash
makepkg -si
```

Installs all three binaries to `/usr/lib/quark/` with Steam compatibility tool registration.

### Verbose mode

```bash
./install.py --verbose    # Enable runtime diagnostics
./install.py --no-verbose # Disable runtime diagnostics
```

Or set `QUARK_VERBOSE=1` in Steam launch options: `QUARK_VERBOSE=1 %command%`

### Manual build

```bash
cd rust && cargo build --release
# Produces: target/release/quark, target/release/triskelion, target/release/parallax
```

## Project Structure

```
quark/
  install.py                 Build + deploy pipeline (2,383 lines)
  PKGBUILD                   Arch Linux package
  rust/
    build.rs                  protocol.def codegen (306 opcodes)
    Cargo.toml                Three [[bin]] targets, one [lib]
    src/
      lib.rs                  Shared log macros (#[macro_export])
      log.rs                  Timestamped logging (verbose gating)
      quark/            Proton replacement launcher (12 files, ~4,200 LOC)
        main.rs               Entry, mode dispatch (Launch, Package, Status, etc.)
        launcher.rs           Wine discovery, prefix setup, game launch
        cli.rs                CLI argument parsing
        gaming.rs             Gaming DLL/program definitions
        pe_scanner.rs         PE header parsing, render API detection
        packager.rs           Steam compatibility tool packaging
        configure.rs          Wine ./configure generation
        clone.rs              Upstream Wine source cloner
        analyze.rs            Wine DLL surface area analysis
        status.rs             Project status reporting
        profile.rs            strace/perf profiling harness
      triskelion/             wineserver replacement daemon (~17,000 LOC)
        main.rs               Daemon entry, daemonize, signal handling, socket path
        csp_loop.rs           CSP I/O thread: epoll hub, effect execution, reply writes
        ipc.rs                Unix socket IPC, SCM_RIGHTS, pending_fd
        slab.rs               HeapSlab<T> + MmapSlab -- O(1) slab allocators
        objects.rs            HandleTable (HeapSlab), HandleEntry, Process, Thread
        shm.rs                ShmManager (MmapSlab), desktop_ready atomic
        ntsync.rs             /dev/ntsync ioctl wrapper (semaphore, mutex, event, wait)
        queue.rs              SPSC ring buffer message queues, futex wake
        registry.rs           In-memory registry tree, DEVMODEW serialization (1,429 lines)
        intel.rs              Game intelligence cache (engine detection, opcode coverage)
        protocol.rs           Protocol types and request codes
        protocol_remap.rs     Proton/Wine opcode divergence mapping
        parallax_display.rs   PARALLAX shared memory reader
        event_loop/           246 handler functions across 13 modules (11,728 lines)
          mod.rs              EventLoop struct, field init, shared helpers (1,413 lines)
          sync.rs             Select, APC delivery, events, mutexes, semaphores (1,345 lines)
          window.rs           Window messages, desktop, atoms, clipboard (2,999 lines)
          file_io.rs          Files, mappings, GENERIC_* access mapping (1,259 lines)
          thread.rs           Thread lifecycle, startup info synthesis (964 lines)
          completion.rs       Completion ports, jobs, timers, sockets, devices (749 lines)
          process.rs          Process lifecycle, memory read/write (734 lines)
          pipes.rs            Named pipes, two-phase APC, PIPE_WAIT (710 lines)
          handles.rs          get_handle_fd, dup_handle, create_file_handle (620 lines)
          registry_handlers.rs  Registry opcodes (381 lines)
          client.rs           Disconnect, cleanup, exit events, SHM slot free (231 lines)
          dispatch.rs         Opcode -> handler routing, auto-stub (184 lines)
          token.rs            Security tokens, privileges (139 lines)
      parallax/               Display compositor (~1,036 LOC)
        main.rs               DRM/KMS enumeration, shared memory writer, child launch
        output.rs             DRM/KMS ioctls, connector/mode/EDID parsing
        display_info.rs       Shared memory layout and writer (seqlock)
        config.rs             TOML config with per-output DSR multiplier
        shaders/
          downscale.comp      Lanczos-2 compute shader for future DSR
  c23/
    launcher.rs               Rust launcher with death-pipe lifecycle (1,717 lines)
  patches/
    wine/                     Wine patches (001-015), applied by install.py
    wine/dlls/ntdll/unix/triskelion.c       SHM bypass + ntsync shadow table
    wine/dlls/win32u/triskelion_message.c   win32u peek_message integration
  tests/
    iterate.py                Build-deploy-launch iteration loop (1,202 lines)
    montauk_compare.py        eBPF trace comparison (with montauk)
    triskelion-tests.py       Protocol-level tests
    discover_opcodes.py       Opcode discovery from Wine protocol
    test_package.py           Package integrity tests (47 tests)
```

## Tooling

quark is a multi-mode binary:

```bash
quark <verb> <exe>                     # Proton-compatible launcher (started by Steam)
quark package <wine_dir>               # package as Steam compatibility tool
quark configure <wine_dir> [--execute] # Wine ./configure with --disable-* flags
quark clone                            # clone upstream Wine source
quark status                           # project status
quark analyze                          # Wine DLL surface area analysis
quark profile <app_id>                 # strace profiling
quark profile-attach                   # attach to running game
quark profile-compare                  # compare profile outputs
quark profile-opcodes                  # analyze opcode traces
```

triskelion runs as the wineserver daemon (started automatically by Wine via the `bin/wineserver` symlink).

## Testing

```bash
# Integration: build, deploy, launch, check logs
python3 tests/iterate.py --appid 2379780 --timeout 30

# Dark Souls Remastered
python3 tests/iterate.py --appid 570940 --timeout 45

# Package integrity (47 tests)
python3 tests/test_package.py

# With montauk eBPF tracing
python3 tests/montauk_compare.py --appid 2379780
```

### Logs

All logs go to `/tmp/quark/`:

| File | Contents |
|------|----------|
| `daemon.log` | Timestamped daemon events: opcodes, sync state, APC delivery, errors |
| `wine_stderr.log` | Wine's stderr (empty unless `--verbose` enabled) |
| `launcher_env.txt` | Full environment snapshot at launch |
| `daemon.pid` | Daemon PID for stale sentinel detection |

### Logging tiers

| Mode | WINEDEBUG | Launcher output |
|------|-----------|-----------------|
| Default | `-all` | `launching: <game.exe>` + errors/warnings only |
| `--verbose` | `+module,+loaddll,+process,err` | Full diagnostics |
| `QUARK_TRACE_OPCODES` | `+server,+timestamp` | Full wineserver protocol dump |

### Debugging

```bash
# Enable verbose diagnostics
./install.py --verbose
# Or per-launch: QUARK_VERBOSE=1 %command%

# Enable opcode tracing
touch /tmp/quark/TRACE_OPCODES

# Force full redeploy
find ~/.steam/root/steamapps/compatdata/ -name ".triskelion_deployed" -delete

# Nuke a game prefix
rm -rf ~/.steam/root/steamapps/compatdata/<app_id>/pfx
```

## License

GPL-2.0
