// quark launcher -- full Proton replacement.
//
// Steam calls: ./proton <verb> <exe> [args...]
// quark sets up the prefix, deploys DXVK/VKD3D, bridges Steam client,
// then launches wine.
//
// Optimization: After first launch, the .quark_deployed cache records
// what was deployed and from where. Subsequent launches skip all file ops
// when the cache is valid — straight to wine64.

use crate::{log_info, log_warn, log_error, log_verbose};

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

fn copy_dir_recursive(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());
        if ty.is_dir() {
            copy_dir_recursive(&src_path, &dst_path)?;
        } else if ty.is_symlink() {
            let target = std::fs::read_link(&src_path)?;
            let resolved = src_path.parent().map(|p| p.join(&target));
            if let Some(r) = resolved {
                if r.exists() {
                    std::fs::copy(&r, &dst_path)?;
                    continue;
                }
            }
        } else {
            std::fs::copy(&src_path, &dst_path)?;
        }
    }
    Ok(())
}

/// The wineserver that matches `wine_dir`'s ntdll. A Wine tree's own
/// bin/wineserver speaks its protocol version; anything else may not.
pub(crate) fn wineserver_exe(wine_dir: &Path) -> PathBuf {
    let own = wine_dir.join("bin/wineserver");
    if own.exists() { own } else { PathBuf::from("/usr/bin/wineserver") }
}

/// Ensure fds 0, 1, 2 are open. Wine's socketpair() picks the lowest free fd.
/// Under Steam's reaper, stdin may be closed → fd 0 is free → socketpair gets
/// fd 0 → child's set_stdio_fd overwrites it with /dev/null → WINESERVERSOCKET
/// points to /dev/null instead of the socket → child crashes.
fn ensure_stdio_fds() {
    for fd in 0..3 {
        unsafe {
            if libc::fcntl(fd, libc::F_GETFD) == -1 {
                libc::open(c"/dev/null".as_ptr(), libc::O_RDWR);
            }
        }
    }
}

const DXVK_DLLS: &[&str] = &["d3d11", "d3d10core", "d3d9", "dxgi"];
const VKD3D_DLLS: &[&str] = &["d3d12", "d3d12core"];
const NVAPI_DLLS: &[&str] = &["nvapi64", "nvofapi64"];
const NVAPI32_DLLS: &[&str] = &["nvapi"];
/// The NVIDIA driver's own, from its nvidia/wine dir; the two Proton copies.
const DLSS_DLLS: &[&str] = &["_nvngx", "nvngx"];

pub(crate) const BASE_OVERRIDES: &[(&str, &str)] = &[
    ("lsteamclient", "b"),  // builtin: Wine bridge to native Steam client (built from Proton source)
    ("steam.exe", "b"),  // builtin: our Wine builtin steam.exe (built from Proton source)
    ("dotnetfx35.exe", "b"),
    ("dotnetfx35setup.exe", "b"),
    ("beclient.dll", "b,n"),
    ("beclient_x64.dll", "b,n"),
    // XInput: Use Wine builtins — routes through winebus.sys for HID enumeration
    ("xinput1_1.dll", "b"),
    ("xinput1_2.dll", "b"),
    ("xinput1_3.dll", "b"),
    ("xinput1_4.dll", "b"),
    ("xinput9_1_0.dll", "b"),
    ("xinputuap.dll", "b"),
    // EasyAntiCheat: Builtin from Proton EAC Runtime (Steam tool 1826330)
    ("easyanticheat", "b"),
    ("easyanticheat_x64", "b"),
    ("easyanticheat_x86", "b"),
    // winebth.sys crashes winedevice.exe — disable it (matches Proton)
    ("winebth.sys", "d"),
    // vrclient: SteamVR not needed — steam.exe asserts if it can't init VR
    ("vrclient_x64", "d"),
    // OpenCL: Disabled for most games (Proton compat)
    ("opencl", "n,d"),
];

/// Size and mtime of a file folded into one stamp; 0 when it is missing.
pub(crate) fn file_stamp(path: &Path) -> u64 {
    use std::os::unix::fs::MetadataExt;
    match std::fs::metadata(path) {
        Ok(m) => (m.size() as u64).wrapping_mul(6364136223846793005) ^ (m.mtime() as u64) ^ 1,
        Err(_) => 0,
    }
}

/// The inherited LD_PRELOAD with the Steam overlay (gameoverlayrenderer) stripped
/// -- it hooks SDL/GL calls and crashes before the Wayland surface is configured.
fn ld_preload_without_overlay() -> String {
    std::env::var("LD_PRELOAD").unwrap_or_default()
        .split(':')
        .filter(|p| !p.is_empty() && !p.contains("gameoverlayrenderer"))
        .collect::<Vec<&str>>()
        .join(":")
}

// Steam client files: (source_name in legacycompat/, dest_name in prefix)
const STEAM_CLIENT_FILES: &[(&str, &str)] = &[
    ("steamclient64.dll", "steamclient64.dll"),
    ("steamclient.dll", "steamclient.dll"),
    ("GameOverlayRenderer64.dll", "GameOverlayRenderer64.dll"),
    ("SteamService.exe", "steam.exe"),
    ("Steam.dll", "Steam.dll"),
];

const CACHE_FILE: &str = ".quark_deployed";

fn launches_game(verb: &str) -> bool {
    verb == "waitforexitandrun" || verb == "run"
}

pub fn run(verb: &str, args: &[OsString]) -> i32 {
    ensure_stdio_fds();

    if matches!(verb, "getcompatpath" | "getnativepath") {
        print_compat_path(args);
        return 0;
    }

    let t_start = Instant::now();

    // Phase 1: Locate everything
    let wine_dir = find_wine();
    let steam_dir = find_steam_dir();
    let wine64 = wine_binary(&wine_dir);

    if !wine64.exists() {
        log_error!("wine not found at {}", wine64.display());
        log_error!("Need Wine binaries. Install Wine from your package manager.");
        return 1;
    }

    // Proton's tree is optional — only needed for steam.exe sourcing now.
    // DXVK/VKD3D are deployed by install.py into quark's own lib/ dir.
    let proton_dir = find_proton_files();
    let dxvk_src_dir = dxvk_source_dir(&wine_dir, proton_dir.as_deref());

    let pfx = match compat_prefix(verb) {
        Ok(Some(pfx)) => pfx,
        Ok(None) => return 0,
        Err(code) => return code,
    };

    // What the call runs is settled before anything starts: The graph below
    // starts the prefix's wineserver ahead of the session, a server no
    // client ever connects to never exits, and Steam's reaper waits on every
    // process under it, so an exit after the graph holds Steam's launch open.
    if !launches_game(verb) {
        log_error!("Unknown verb: {verb}");
        return 1;
    }
    let Some(game_exe) = args.first() else {
        log_error!("No executable specified");
        return 1;
    };
    if is_steam_utility(game_exe) {
        log_verbose!("skipping Steam utility: {}", game_exe.to_string_lossy());
        return 0;
    }

    let self_exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => {
            log_error!("Failed to determine own executable path: {e}");
            return 1;
        }
    };
    let t_discover = t_start.elapsed();

    log_verbose!("wine64: {}", wine64.display());
    log_verbose!("wineserver: {}", wineserver_exe(&wine_dir).display());
    log_verbose!("steam: {}", steam_dir.display());

    // Per-component cache: One stat per component, never a walk. The Wine
    // tree hashes at lib/wine, so a redeployed launcher binary beside it is
    // not a new tree.
    let cache = DeployCache::load(&pfx);
    let wine_hash = dir_hash(&wine_dir.join("lib/wine"));
    let dxvk_hash = dir_hash(&dxvk_src_dir.join("lib/wine/dxvk"));
    let vkd3d_hash = dir_hash(&dxvk_src_dir.join("lib/wine/vkd3d-proton"));
    let steam_hash = dir_hash(&steam_dir.join("legacycompat"));

    let wine_valid = cache.as_ref().is_some_and(|c| c.wine_hash == wine_hash);
    let dxvk_valid = cache.as_ref().is_some_and(|c| c.dxvk_hash == dxvk_hash);
    let vkd3d_valid = cache.as_ref().is_some_and(|c| c.vkd3d_hash == vkd3d_hash);
    let steam_valid = cache.as_ref().is_some_and(|c| c.steam_hash == steam_hash);
    let is_nvidia = Path::new("/proc/driver/nvidia/version").exists();

    // Phase 2: The launch graph. Every node names what it needs and the
    // executor in graph.rs runs the ready set on scoped threads in Kahn's
    // order. The wineserver is a node of its own: It needs only the
    // prefix's registry, so it is up and listening while the deploys and
    // the environment are still being written, instead of being started
    // by the first Wine process.
    let t2 = Instant::now();
    let wineserver = wineserver_exe(&wine_dir);
    let nodes: Vec<crate::graph::Node> = vec![
        crate::graph::Node::new("registry", &[], |_| {
            prepare_prefix_layout(&wine_dir, &pfx);
            seed_prefix_registry(&self_exe, &pfx);
            ensure_registry_stubs(&pfx);
            inject_registry_keys(&pfx);
            crate::graph::Out::Done
        }),
        crate::graph::Node::new("prefix", &["registry"], |_| {
            if !wine_valid {
                // Wineboot's entry command is steam.exe, so the bridge goes
                // in before setup_prefix and again after it (wineboot may
                // rewrite system32); the "bridge" node is the one after.
                deploy_steam_exe(&self_exe, &pfx);
                deploy_lsteamclient_to_prefix(&self_exe, &pfx);
                setup_prefix(&wine_dir, &pfx, &wine64, &self_exe);
            }
            ensure_user_profile_dirs(&pfx);
            map_game_drive(&pfx);
            crate::graph::Out::Done
        }),
        crate::graph::Node::new("wineserver", &["prefix"], |_| {
            prestart_wineserver(&wineserver, &pfx);
            crate::graph::Out::Done
        }),
        // DXVK and VKD3D-Proton deploy for every game: Many discover D3D12 at
        // runtime through DXGI with no d3d12.dll import, launcher stubs
        // import no D3D at all, and the layers only act on an API call.
        crate::graph::Node::new("dxvk", &["prefix"], |_| {
            crate::graph::Out::Dlls(if dxvk_valid { DXVK_DLLS.to_vec() } else { deploy_dxvk(&dxvk_src_dir, &pfx) })
        }),
        crate::graph::Node::new("vkd3d", &["prefix"], |_| {
            crate::graph::Out::Dlls(if vkd3d_valid { VKD3D_DLLS.to_vec() } else { deploy_vkd3d(&dxvk_src_dir, &pfx) })
        }),
        // On every NVIDIA launch, as Proton does: A game can load nvapi late
        // through a plugin, and NGX finds the driver's nvngx itself.
        crate::graph::Node::new("nvapi", &["prefix"], |_| {
            crate::graph::Out::Dlls(if is_nvidia { deploy_nvapi(&wine_dir, &pfx) } else { vec![] })
        }),
        crate::graph::Node::new("dlss", &["prefix"], |_| {
            crate::graph::Out::Dlls(if is_nvidia { deploy_dlss(&pfx) } else { vec![] })
        }),
        // Every launch: file_matches makes each a stat per file, and a prefix
        // that holds the bridge under Valve's names is repaired rather than
        // cached. lsteamclient.dll must sit in system32 so build_module's
        // LdrLoadDll finds it; without it the steamclient trampoline (Wine
        // patch 001) never activates.
        crate::graph::Node::new("bridge", &["prefix"], |_| {
            deploy_steam_client(&steam_dir, &pfx);
            deploy_steam_exe(&self_exe, &pfx);
            deploy_lsteamclient_to_prefix(&self_exe, &pfx);
            crate::graph::Out::Done
        }),
    ];
    let (outputs, node_timings) = crate::graph::run(nodes);
    let t_graph = t2.elapsed();
    let dxvk_deployed = outputs.dlls("dxvk");
    let vkd3d_deployed = outputs.dlls("vkd3d");
    let nvapi_deployed = outputs.dlls("nvapi");
    let dlss_deployed = outputs.dlls("dlss");

    // Save cache if anything changed
    if !(wine_valid && dxvk_valid && vkd3d_valid && steam_valid) {
        DeployCache { wine_hash, dxvk_hash, vkd3d_hash, steam_hash }.save(&pfx);
        log_verbose!("deployment cache written");
    } else {
        log_verbose!("cache hit: every component current");
    }

    // Phase 5: Build environment
    let log_dir = Path::new(crate::log::LOG_DIR);
    let trace = std::env::var("QUARK_TRACE_OPCODES").is_ok()
        || log_dir.join("TRACE_OPCODES").exists();

    // Parse env_config FIRST so launcher-affecting env vars (WINEDEBUG and
    // friends) are in place before build_env_vars and resolve_winedebug read them.
    let custom_env = parse_env_config(&self_exe);
    for (k, v) in &custom_env {
        unsafe { std::env::set_var(k, v); }
    }

    let env_vars = build_env_vars(
        &wine_dir, &pfx,
        &dxvk_deployed, &vkd3d_deployed, &nvapi_deployed, &dlss_deployed,
    );

    // Resolve WINEDEBUG once. Priority:
    // 1. env_config / shell env (user's explicit override)
    // 2. QUARK_TRACE_OPCODES → "+server,+timestamp"
    // 3. --verbose → "+process,err"
    // 4. default → "-all"
    let winedebug = resolve_winedebug(trace);

    let nodes_line: Vec<String> = node_timings.iter().map(|(n, ms)| format!("{n}={ms}ms")).collect();
    log_verbose!("timing: discover={}ms graph={}ms [{}] total={}ms",
        t_discover.as_millis(), t_graph.as_millis(), nodes_line.join(" "),
        t_start.elapsed().as_millis());

    // Phase 6: Launch
    remove_stale_wine_servers();

    let launch = Launch {
        wine_dir: &wine_dir,
        wine64: &wine64,
        pfx: &pfx,
        env_vars: &env_vars,
        custom_env: &custom_env,
        winedebug: &winedebug,
        log_dir,
        started: t_start,
    };
    launch_game(&launch, game_exe, &args[1..])
}

/// Steam runs utility exes that need no Wine session, and quark answers 0
/// for them, which tells Steam the tool is healthy: iscriptevaluator.exe
/// runs the DirectX and vcredist install scripts, whose runtimes Wine
/// provides, and d3ddriverquery64.exe probes the GPU, which fails without
/// a full display driver.
pub(crate) fn is_steam_utility(exe: &OsStr) -> bool {
    let exe = exe.to_string_lossy().to_ascii_lowercase();
    ["iscriptevaluator", "d3ddriverquery"].iter().any(|pat| exe.contains(pat))
}

/// getcompatpath / getnativepath: echo the path back, raw bytes intact.
fn print_compat_path(args: &[OsString]) {
    if let Some(path) = args.first() {
        use std::io::Write;
        use std::os::unix::ffi::OsStrExt;
        let mut out = std::io::stdout().lock();
        let _ = out.write_all(path.as_bytes()).and_then(|_| out.write_all(b"\n"));
    }
}

/// Where DXVK/VKD3D are sourced from. Checks for an actual DLL file (not
/// just the directory) to avoid false positives.
fn dxvk_source_dir(wine_dir: &Path, proton_dir: Option<&Path>) -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_default();
    let compat_tool_dir = PathBuf::from(&home)
        .join(".local/share/Steam/compatibilitytools.d/quark");
    if compat_tool_dir.join("lib/wine/dxvk/x86_64-windows/d3d11.dll").exists() {
        compat_tool_dir
    } else if wine_dir.join("lib/wine/dxvk/x86_64-windows/d3d11.dll").exists() {
        wine_dir.to_path_buf()
    } else if let Some(proton) = proton_dir {
        if proton.join("lib/wine/dxvk/x86_64-windows/d3d11.dll").exists() {
            log_verbose!("DXVK/VKD3D: sourcing from Proton ({})", proton.display());
            proton.to_path_buf()
        } else {
            log_warn!("DXVK: no DLLs found — D3D games will use Wine's builtin (may crash on NVIDIA)");
            wine_dir.to_path_buf()
        }
    } else {
        log_warn!("DXVK/VKD3D: no source found — D3D games will use Wine's builtin");
        wine_dir.to_path_buf()
    }
}

/// The prefix Steam assigned this launch: Ok(Some) to proceed, Ok(None) to
/// exit 0 without a Wine session, Err(exit code) on failure.
fn compat_prefix(verb: &str) -> Result<Option<PathBuf>, i32> {
    let compat_data = std::env::var("STEAM_COMPAT_DATA_PATH").unwrap_or_default();
    let steam_appid = std::env::var("SteamAppId").unwrap_or_default();
    log_verbose!("STEAM_COMPAT_DATA_PATH={compat_data} SteamAppId={steam_appid} verb={verb}");
    if compat_data.is_empty() {
        log_error!("STEAM_COMPAT_DATA_PATH not set — not launched from Steam?");
        return Err(1);
    }
    // Steam calls "proton run" with compatdata/0 and empty SteamAppId as a
    // prefix probe before the real game launch. Starting a full wineserver for
    // this pollutes the socket directory. Skip it entirely.
    if (compat_data.ends_with("/0") || compat_data.ends_with("/0/")) && verb == "run" {
        log_verbose!("Skipping Steam prefix probe (compatdata/0, verb=run)");
        return Ok(None);
    }
    let pfx = PathBuf::from(&compat_data).join("pfx");
    if let Err(e) = std::fs::create_dir_all(&pfx) {
        log_error!("Failed to create prefix directory {}: {e}", pfx.display());
        return Err(1);
    }
    Ok(Some(pfx))
}

/// Sync the Wine tree's PE files into prefix system32 BEFORE wineboot --init.
/// wineboot needs kernel32.dll et al to exist in the prefix. On a fresh
/// prefix this directory is empty, causing "could not load kernel32.dll".
/// The source is the tree quark runs (patched modules included), the same
/// layer install.py's prefix sync writes, so neither undoes the other.
pub(crate) fn prepare_prefix_layout(wine_dir: &Path, pfx: &Path) {
    let sys32 = pfx.join("drive_c/windows/system32");
    let _ = std::fs::create_dir_all(&sys32);
    // Create Windows directory structure that wineboot normally populates.
    // Games (especially LÖVE/SDL) need these to exist during init.
    let win = pfx.join("drive_c/windows");
    for d in [
        "syswow64", "Fonts", "temp", "system", "inf", "help", "logs",
        "winsxs", "resources", "security", "tasks", "performance",
        "Microsoft.NET", "Installer", "Globalization", "twain_32", "twain_64",
    ] {
        let _ = std::fs::create_dir_all(win.join(d));
    }
    let _ = std::fs::create_dir_all(pfx.join("drive_c/ProgramData"));
    let _ = std::fs::create_dir_all(pfx.join("drive_c/Program Files/Common Files"));
    let _ = std::fs::create_dir_all(pfx.join("drive_c/Program Files (x86)/Common Files"));
    // wine.inf installs drivers to %12% (system32\drivers) and registers each
    // service binary there; a .sys anywhere else fails to load (c0000135).
    let drivers = sys32.join("drivers");
    let src_dir = wine_dir.join("lib/wine/x86_64-windows");
    let src_ntdll = src_dir.join("ntdll.dll");
    let pfx_ntdll = sys32.join("ntdll.dll");
    if src_ntdll.exists() {
        let src_size = src_ntdll.metadata().map(|m| m.len()).unwrap_or(0);
        let pfx_size = pfx_ntdll.metadata().map(|m| m.len()).unwrap_or(0);
        // A prefix synced before drivers had their own directory gets its
        // drivers alone; a full sync would replace the DXVK DLLs the deploy
        // cache believes are in place.
        let full = src_size != pfx_size;
        let drivers_only = !full && !drivers.is_dir();
        if full || drivers_only {
            log_verbose!("Syncing Wine PE files to prefix from {}...", src_dir.display());
            let _ = std::fs::create_dir_all(&drivers);
            let mut copied = 0u32;
            if let Ok(entries) = std::fs::read_dir(&src_dir) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
                    if drivers_only && ext != "sys" {
                        continue;
                    }
                    if ext == "dll" || ext == "exe" || ext == "drv" || ext == "sys" || ext == "ocx" || ext == "cpl" || ext == "acm" || ext == "ax" {
                        let dir = if ext == "sys" { &drivers } else { &sys32 };
                        let target = dir.join(entry.file_name());
                        let _ = std::fs::copy(&path, &target);
                        copied += 1;
                    }
                }
            }
            log_verbose!("Synced {copied} PE files to prefix system32");
        }
    }
    // 32-bit processes load their builtins from syswow64, where wineboot
    // would have put them; without them a 32-bit game fails on kernel32. A
    // prefix with no 32-bit ntdll there gets only what it is missing, so
    // DXVK's 32-bit DLLs stay.
    let syswow64 = win.join("syswow64");
    let src32 = wine_dir.join("lib/wine/i386-windows");
    if src32.join("ntdll.dll").exists() && !syswow64.join("ntdll.dll").exists() {
        let mut copied = 0u32;
        if let Ok(entries) = std::fs::read_dir(&src32) {
            for entry in entries.flatten() {
                let path = entry.path();
                let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
                if !matches!(ext, "dll" | "exe" | "drv" | "ocx" | "cpl" | "acm" | "ax") {
                    continue;
                }
                let target = syswow64.join(entry.file_name());
                if !target.exists() && std::fs::copy(&path, &target).is_ok() {
                    copied += 1;
                }
            }
        }
        log_verbose!("Synced {copied} 32-bit PE files to prefix syswow64");
    }
}

/// Ensure pfx dir and stub .reg files exist BEFORE wineboot starts the
/// wineserver. It loads .reg files at startup — if we inject keys after
/// that, it never sees them. So: Create stubs → inject keys → wineboot.
fn ensure_registry_stubs(pfx: &Path) {
    let _ = std::fs::create_dir_all(pfx);
    for reg_file in ["system.reg", "user.reg", "userdef.reg"] {
        let reg_path = pfx.join(reg_file);
        if !reg_path.exists() {
            let header = if reg_file.starts_with("user") {
                "WINE REGISTRY Version 2\n;; All keys relative to REGISTRY\\User\n\n#arch=win64\n"
            } else {
                "WINE REGISTRY Version 2\n;; All keys relative to REGISTRY\\Machine\n\n#arch=win64\n"
            };
            let _ = std::fs::write(&reg_path, header);
        }
    }
}

/// Clean stale wineserver sockets from previous runs.
/// Wine connects to any existing socket for the prefix's inode.
/// If a stale socket exists from a crashed/killed server, Wine hangs
/// trying to connect to a dead daemon.
fn remove_stale_wine_servers() {
    let uid = unsafe { libc::getuid() };
    let wine_tmp = format!("/tmp/.wine-{uid}");
    if let Ok(entries) = std::fs::read_dir(&wine_tmp) {
        for entry in entries.flatten() {
            let sock = entry.path().join("socket");
            if sock.exists() {
                // Try connecting with timeout — if refused or timeout, it's stale
                use std::os::unix::net::UnixStream;
                match UnixStream::connect(&sock) {
                    Ok(s) => drop(s), // live server, leave it
                    Err(_) => {
                        log_verbose!("Removing stale server dir: {}", entry.path().display());
                        let _ = std::fs::remove_dir_all(entry.path());
                    }
                }
            }
        }
    }
}

/// Everything a game launch reads, resolved by `run` before Phase 6.
struct Launch<'a> {
    wine_dir: &'a Path,
    wine64: &'a Path,
    pfx: &'a Path,
    env_vars: &'a [(&'static str, String)],
    custom_env: &'a [(String, String)],
    winedebug: &'a str,
    log_dir: &'a Path,
    started: Instant,
}

fn launch_game(l: &Launch, game_exe: &OsString, game_args: &[OsString]) -> i32 {
    let game_exe_str = game_exe.to_string_lossy();

    // Sanity check: Does the game exe exist?
    let game_path = Path::new(game_exe);
    if !game_path.exists() {
        log_warn!("game exe not found at {game_exe_str} (may be a Windows path — continuing)");
    }

    log_info!("launching: {game_exe_str}");

    // Pre-launch validation: The translation layers the deploys promised
    // are in the prefix.
    {
        let sys32 = l.pfx.join("drive_c/windows/system32");
        let mut fatal: Vec<String> = Vec::new();
        if !sys32.join("d3d11.dll").exists() {
            fatal.push("d3d11.dll (DXVK)".into());
        }
        if !sys32.join("d3d12.dll").exists() {
            fatal.push("d3d12.dll (VKD3D-Proton)".into());
        }
        if !fatal.is_empty() {
            for dll in &fatal {
                log_error!("FATAL: {dll} not deployed");
            }
            log_error!("Check DXVK/VKD3D source dirs. Game will crash without these.");
        }
    }

    // Clean stale crash dumps — engines like Godot read their crash dir
    // on startup and may enter recovery paths if old dumps exist.
    clean_crash_dumps(l.pfx);

    // Save data protection: Snapshot before launch
    let save_backup = snapshot_save_data(l.pfx);

    let mut cmd = game_command(l, game_exe, game_args);
    // The launcher survives Steam's Stop and forwards it to the game. A
    // caught signal resets to default across exec, so the game gets its own
    // default dispositions regardless.
    let handler = forward_stop as extern "C" fn(libc::c_int) as libc::sighandler_t;
    unsafe {
        libc::signal(libc::SIGINT, handler);
        libc::signal(libc::SIGTERM, handler);
    }
    cmd.env("WINEDEBUG", l.winedebug);

    // Strip Steam overlay from LD_PRELOAD — it hooks SDL/GL calls and
    // crashes before the Wayland surface is configured.
    let preload = ld_preload_without_overlay();
    if preload.is_empty() {
        cmd.env_remove("LD_PRELOAD");
    } else {
        cmd.env("LD_PRELOAD", &preload);
    }

    let stderr_log_path = redirect_game_output(&mut cmd, l.log_dir);

    // User overrides win. Applied LAST so any cmd.env hardcoded above
    // (e.g. WINEDEBUG) yields to env_config when the user explicitly
    // sets a different value. Comment-out in env_config is now meaningful
    // for any var quark hardcodes — uncomment + change to override.
    for (k, v) in l.custom_env {
        cmd.env(k, v);
    }

    log_verbose!("timing: spawn at {}ms after the launcher started", l.started.elapsed().as_millis());
    let child = cmd.spawn().unwrap_or_else(|e| {
        log_error!("Failed to exec wine64: {e}");
        std::process::exit(1);
    });

    let incident_game_stderr = spawn_observer(l, game_path);
    end_session(l, child, stderr_log_path, incident_game_stderr, save_backup)
}

/// The session's end, once the game is spawned: Wait for it through a pidfd,
/// end the prefix's session (killing it on Stop or when the server outlives
/// its grace), record the exit and restore any saves that went missing. The
/// launcher exits with the game's code.
fn end_session(
    l: &Launch,
    mut child: std::process::Child,
    stderr_log_path: Option<PathBuf>,
    incident_game_stderr: Option<PathBuf>,
    save_backup: Option<PathBuf>,
) -> i32 {
    // Now wait for the game to exit; wait() retries on EINTR, so a forwarded
    // Stop returns here only once the game is gone.
    let child_pid = child.id() as i32;
    // A pidfd names this process and no other, so the Stop forwarder cannot
    // signal a reused pid, and its readability is the child's exit.
    let pidfd = unsafe { libc::syscall(libc::SYS_pidfd_open, child_pid, 0) } as i32;
    GAME_PIDFD.store(pidfd, std::sync::atomic::Ordering::Relaxed);
    GAME_PGID.store(child_pid, std::sync::atomic::Ordering::Relaxed);
    if pidfd >= 0 {
        let mut pfd = libc::pollfd { fd: pidfd, events: libc::POLLIN, revents: 0 };
        loop {
            let r = unsafe { libc::poll(&mut pfd, 1, -1) };
            if r >= 0 || std::io::Error::last_os_error().raw_os_error() != Some(libc::EINTR) {
                break;
            }
        }
    }
    let status = child.wait().unwrap_or_else(|e| {
        log_error!("Failed to wait for wine64: {e}");
        std::process::exit(1);
    });
    GAME_PIDFD.store(-1, std::sync::atomic::Ordering::Relaxed);
    if pidfd >= 0 {
        unsafe { libc::close(pidfd) };
    }
    let wineserver = wineserver_exe(l.wine_dir);
    if STOP_REQUESTED.load(std::sync::atomic::Ordering::Relaxed)
        || !await_session_end(&wineserver, l.pfx)
    {
        kill_session(child_pid, &wineserver, l.pfx);
    }

    if let Some(ref log_path) = stderr_log_path {
        record_exit(log_path, &status, incident_game_stderr.as_deref());
    }

    // Save data protection: Restore any files that went missing
    if let Some(ref backup_dir) = save_backup {
        restore_save_data(l.pfx, backup_dir);
    }

    match status.code() {
        Some(code) => code,
        None => {
            use std::os::unix::process::ExitStatusExt;
            if let Some(sig) = status.signal() {
                log_warn!("wine64 killed by signal {sig}");
            }
            1
        }
    }
}

/// Launch through steam.exe (Proton's steam_helper). steam.exe:
/// 1. Creates Win32 events games check (Steam3Master_SharedMemLock, etc.)
/// 2. Calls steamclient_init_registry() — dlopen's native steamclient.so,
///    CreateSteamPipe()/ConnectToGlobalUser() to connect to running Steam
/// 3. Creates the "Steam" window (FindWindow detection)
/// 4. Spawns the game as a child via CreateProcess()
///
/// Without this, SteamAPI_IsSteamRunning() fails.
fn game_command(l: &Launch, game_exe: &OsString, game_args: &[OsString]) -> Command {
    let game_path = Path::new(game_exe);
    let mut cmd = build_wine_command(l.wine64, l.wine_dir);
    cmd.arg("C:\\windows\\system32\\steam.exe");
    cmd.arg(game_exe);
    cmd.args(game_args);
    if let Some(parent) = game_path.parent() {
        if parent.exists() { cmd.current_dir(parent); }
    }
    // Order: Quark defaults → quark hardcoded essentials → user's
    // env_config (last write wins, so user overrides reach the game).
    for (k, v) in l.env_vars {
        cmd.env(k, v);
    }
    cmd.env("WINEPREFIX", l.pfx);
    cmd.env("WINESERVER", wineserver_exe(l.wine_dir));
    // Windows TEMP/TMP: Stock prefixes get these via shell32
    // create_extra_folders writing HKCU\Environment; that chain never
    // runs under quark, so NO Windows process had TEMP — wine-mono fed
    // the NULL into g_path_is_absolute and aborted (TMNT,
    // gmisc-win32.c:138). ntdll FILTERS plain TEMP=/TMP= out of the unix
    // env import (env.c is_special_env_var) but imports WINETEMP=/WINETMP=
    // AS TEMP=/TMP= (env.c ~:888) — that is the sanctioned channel. The
    // user-overrides-win custom_env loop below can still override.
    let win_temp = format!("C:\\users\\{}\\Temp", profile_user(l.pfx));
    cmd.env("WINETEMP", &win_temp);
    cmd.env("WINETMP", &win_temp);
    let steam_app_id = std::env::var("SteamGameId")
        .or_else(|_| std::env::var("SteamAppId"))
        .or_else(|_| std::env::var("STEAM_COMPAT_APP_ID"))
        .unwrap_or_default();
    cmd.env("SteamAppId", &steam_app_id);
    cmd.env("SteamGameId", &steam_app_id);
    cmd.env("STEAM_COMPAT_CLIENT_INSTALL_PATH",
        std::env::var("STEAM_COMPAT_CLIENT_INSTALL_PATH")
            .unwrap_or_else(|_| format!("{}/.local/share/Steam", std::env::var("HOME").unwrap_or_default())));

    // The game gets default SIGINT and SIGTERM dispositions whatever the
    // launcher's are; an ignored disposition would survive exec and make
    // a SIGTERM to the game a no-op.
    use std::os::unix::process::CommandExt;
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            libc::signal(libc::SIGINT, libc::SIG_DFL);
            libc::signal(libc::SIGTERM, libc::SIG_DFL);
            Ok(())
        });
    }
    cmd
}

/// Send the game's stderr and stdout to LOG_DIR; returns the stderr log.
fn redirect_game_output(cmd: &mut Command, log_dir: &Path) -> Option<PathBuf> {
    let _ = std::fs::create_dir_all(log_dir);
    let path = log_dir.join("wine_stderr.log");
    match std::fs::File::create(&path) {
        Ok(f) => {
            cmd.stderr(std::process::Stdio::from(f));
            if let Ok(f2) = std::fs::File::create(log_dir.join("wine_stdout.log")) {
                cmd.stdout(std::process::Stdio::from(f2));
            }
            Some(path)
        }
        Err(_) => None,
    }
}

/// Observer (opt-in via env_config QUARK_TRACE=1, set by
/// install.py --observer). Spawns montauk as a sibling watching the
/// game's process tree by name pattern (montauk's --trace expects a
/// substring/regex matched against process names — NOT a PID).
///
/// Pattern strategy: Use the game's exe basename without extension.
/// Wine spawns the game .exe as a child of wine64; montauk's tracepoint
/// on sched_process_fork auto-adds children once any process matches.
/// Fire-and-forget — observer death does not affect the game. Returns
/// where to preserve the game's stderr for this incident.
fn spawn_observer(l: &Launch, game_path: &Path) -> Option<PathBuf> {
    if std::env::var("QUARK_TRACE").as_deref() != Ok("1") {
        return None;
    }
    let incidents = l.log_dir.join("incidents");
    let _ = std::fs::create_dir_all(&incidents);
    // Kernel TASK_COMM_LEN is 16 (15 chars + NUL); the kernel
    // truncates process comm to 15 chars. Montauk's --trace
    // substring-matches against /proc/PID/comm — a pattern longer
    // than 15 chars CANNOT substring-match the truncated comm.
    // Clamp the pattern to 15 bytes so e.g. "VampireSurvivors"
    // becomes "VampireSurvivor" which actually matches.
    let raw = game_path
        .file_stem()
        .and_then(|s| s.to_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| "wine".to_string());
    let pattern: String = raw.bytes().take(15)
        .map(|b| b as char).collect();
    // Observer: Incident-only mode by default. We DON'T pass
    // --trace-out (would dump 500MB+ of per-event data to disk per
    // launch) and we DON'T pass --log (Prometheus snapshots —
    // useful for live monitoring, not for crash diagnosis). Just
    // attach BPF and let SIGNAL events surface to stderr.
    //
    // Diagnostic mode: Set QUARK_TRACE_OUT=1 to also dump
    // the full binary event stream (I/O syscalls including pread64,
    // sched events, ntsync) to LOG_DIR/incidents/<game>-io.bin.
    // Read it with `montauk --decode` / `montauk --analyze`.
    let stderr_log = incidents.join(format!("{pattern}-stderr.log"));
    // <pattern>-stderr.log above is MONTAUK's stderr. The game's
    // real stderr (glibc abort text, Wine err channels) goes to
    // LOG_DIR/wine_stderr.log, which the next launch truncates.
    // Preserve it per-incident after the game exits.
    let incident_game_stderr = incidents.join(format!("{pattern}-game-stderr.log"));
    // Trace the game, steam.exe (the game's parent on every launch, so a
    // hang in the Steam bridge is in it) and the wineserver in one capture:
    // the montauk `handles` join needs the wineserver's ntsync ops AND the
    // guest's waits in the same trace. montauk --trace takes a
    // comma-separated comm list. Game first = the BPF .rodata fast-path
    // primary (instant exec-time capture).
    let trace_pattern = format!("{},steam.exe,wineserver", pattern);
    let mut ocmd = std::process::Command::new("montauk");
    ocmd.arg("--trace").arg(&trace_pattern)
        .arg("--headless");
    // Point montauk's ntdll uprobes (keyed-event critical sections + the
    // wait-site stacks on NtWaitForSingleObject/NtWaitForMultipleObjects) at
    // the deployed unix-side ntdll.so. Without MONTAUK_NTDLL_PATH the uprobes
    // silently skip, so keyed-event and wait-site capture never attach.
    let ntdll_so = l.wine_dir.join("lib/wine/x86_64-unix/ntdll.so");
    if ntdll_so.exists() {
        ocmd.env("MONTAUK_NTDLL_PATH", &ntdll_so);
    }
    if std::env::var("QUARK_TRACE_OUT").ok().as_deref() == Some("1") {
        let trace_out = incidents.join(format!("{pattern}-io.bin"));
        ocmd.arg("--trace-out").arg(&trace_out);
        log_info!("montauk observer: --trace-out → {}", trace_out.display());
    }
    ocmd.stdout(std::process::Stdio::null())
        .stderr(std::fs::File::create(&stderr_log)
            .map(std::process::Stdio::from)
            .unwrap_or_else(|_| std::process::Stdio::null()));
    match ocmd.spawn() {
        Ok(c) => log_info!("montauk observer spawned (pid {}) tracing \"{}\" \
                            → {}", c.id(), pattern, stderr_log.display()),
        Err(e) => log_warn!("montauk observer spawn failed: {e} \
                            (install montauk or unset QUARK_TRACE)"),
    }
    Some(incident_game_stderr)
}

/// Kill the entire session spawned by wine64 (the setsid pre_exec in
/// game_command). Without this, orphaned Wine children (services.exe, game
/// .exe) survive after wine64 exits and spin at 100%+ CPU.
///
/// SIGTERM first so services.exe / wineboot / etc. get a chance to
/// flush registry hives, persist save data, and unlink temp files.
/// Most well-behaved Wine processes exit within a few hundred ms.
/// SIGKILL as the backstop for the ones that don't.
/// A normal exit: The child (steam.exe) is gone, but a launcher stub may
/// have started the real game and exited, and Wine's services shut down on
/// their own once no user process is left. Wait for the prefix's server to
/// exit, which is that moment plus Wine's 3 s persistence, the way Proton's
/// waitforexitandrun does. A Stop during the wait ends it. Returns false
/// when the wait was cut short and the session still has to be ended.
fn await_session_end(wineserver: &Path, pfx: &Path) -> bool {
    let mut wait = match Command::new(wineserver)
        .arg("-w")
        .env("WINEPREFIX", pfx)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            log_warn!("session: wineserver -w failed: {e}");
            return false;
        }
    };
    loop {
        if STOP_REQUESTED.load(std::sync::atomic::Ordering::Relaxed) {
            let _ = wait.kill();
            let _ = wait.wait();
            return false;
        }
        match wait.try_wait() {
            Ok(Some(_)) => return true,
            Ok(None) => std::thread::sleep(std::time::Duration::from_millis(50)),
            Err(_) => return false,
        }
    }
}

fn kill_session(child_pid: i32, wineserver: &Path, pfx: &Path) {
    // The child's process group (setsid in pre_exec) first, with the
    // kernel's own group liveness check, kill(-pgid, 0), bounding the grace.
    unsafe { libc::kill(-child_pid, libc::SIGTERM); }
    let deadline = Instant::now() + std::time::Duration::from_millis(500);
    while Instant::now() < deadline {
        if unsafe { libc::kill(-child_pid, 0) } != 0 {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    unsafe { libc::kill(-child_pid, libc::SIGKILL); }
    // Then the prefix. Wine starts its services and the game itself outside
    // that group, and the server was started ahead of the session in a
    // session of its own, so the group kill reaches neither; the prefix's
    // server ends every process attached to it, and nothing else.
    let r = Command::new(wineserver)
        .arg("-k")
        .env("WINEPREFIX", pfx)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    if let Err(e) = r {
        log_warn!("session: wineserver -k failed: {e}");
    }
}

/// The game's process group and its pidfd, for the signal handler below;
/// 0 and -1 until spawned.
static GAME_PGID: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);
static GAME_PIDFD: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(-1);
/// Set when Steam's Stop arrives, so the end of the session ends the prefix
/// instead of waiting for it.
static STOP_REQUESTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Steam's Stop reaches the launcher as SIGINT or SIGTERM. Forward it to the
/// child through its pidfd, which cannot hit a reused pid, and to the rest
/// of the group by pgid, then let the wait return so teardown runs the
/// normal path. Both calls are async-signal-safe.
extern "C" fn forward_stop(_sig: libc::c_int) {
    STOP_REQUESTED.store(true, std::sync::atomic::Ordering::Relaxed);
    let pidfd = GAME_PIDFD.load(std::sync::atomic::Ordering::Relaxed);
    if pidfd >= 0 {
        unsafe { libc::syscall(libc::SYS_pidfd_send_signal, pidfd, libc::SIGTERM, 0usize, 0u32); }
    }
    let pgid = GAME_PGID.load(std::sync::atomic::Ordering::Relaxed);
    if pgid > 0 {
        unsafe { libc::kill(-pgid, libc::SIGTERM); }
    }
}

/// Append the exit status to the game's stderr log, and preserve that log
/// next to the montauk trace — wine_stderr.log is truncated on every launch.
fn record_exit(log_path: &Path, status: &std::process::ExitStatus, incident_game_stderr: Option<&Path>) {
    let exit_info = match status.code() {
        Some(0) => "exit: 0 (clean)".to_string(),
        Some(code) => format!("exit: {code} (error)"),
        None => {
            use std::os::unix::process::ExitStatusExt;
            match status.signal() {
                Some(sig) => format!("killed by signal {sig}"),
                None => "unknown exit".to_string(),
            }
        }
    };

    if let Ok(mut f) = std::fs::OpenOptions::new().append(true).open(log_path) {
        use std::io::Write;
        let _ = writeln!(f, "\n[quark] {exit_info}");
    }
    log_verbose!("stderr log: {}", log_path.display());

    if let Some(dst) = incident_game_stderr {
        match std::fs::copy(log_path, dst) {
            Ok(_) => log_verbose!("incident game stderr: {}", dst.display()),
            Err(e) => log_warn!("incident game stderr copy failed: {e}"),
        }
    }
}

// ---------------------------------------------------------------------------
// Deployment cache
// ---------------------------------------------------------------------------

pub(crate) struct DeployCache {
    pub(crate) wine_hash: u64,
    pub(crate) dxvk_hash: u64,
    pub(crate) vkd3d_hash: u64,
    pub(crate) steam_hash: u64,
}

impl DeployCache {
    pub(crate) fn load(pfx: &Path) -> Option<Self> {
        let data = std::fs::read_to_string(pfx.join(CACHE_FILE)).ok()?;
        let parts: Vec<&str> = data.strip_prefix("v6:")?.trim().split(',').collect();
        if parts.len() != 4 { return None; }
        let mut v = [0u64; 4];
        for (slot, part) in v.iter_mut().zip(parts) {
            *slot = part.parse().ok()?;
        }
        Some(DeployCache { wine_hash: v[0], dxvk_hash: v[1], vkd3d_hash: v[2], steam_hash: v[3] })
    }

    pub(crate) fn save(&self, pfx: &Path) {
        let data = format!("v6:{},{},{},{}", self.wine_hash, self.dxvk_hash,
            self.vkd3d_hash, self.steam_hash);
        if let Err(e) = std::fs::write(pfx.join(CACHE_FILE), data) {
            log_warn!("Cannot write deployment cache: {e}; next launch will re-deploy");
        }
    }
}

/// The wineserver for this prefix, started ahead so it is listening before
/// the first Wine process asks for it. It daemonizes and returns at once; a
/// server already up for the prefix makes this a no-op. Wine's default
/// persistence, 3 s with no client, covers the few milliseconds left before
/// the spawn and ends the server on its own once the session is over.
fn prestart_wineserver(wineserver: &Path, pfx: &Path) {
    let r = Command::new(wineserver)
        .env("WINEPREFIX", pfx)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    match r {
        Ok(st) if st.success() => log_verbose!("wineserver: started ahead for {}", pfx.display()),
        // 2 is Wine's "another server holds this prefix's lock": One is up.
        Ok(st) if st.code() == Some(2) => log_verbose!("wineserver: already running for {}", pfx.display()),
        Ok(st) => log_verbose!("wineserver: prestart exited {st}; Wine will start its own"),
        Err(e) => log_warn!("wineserver: prestart failed: {e}; Wine will start its own"),
    }
}

pub(crate) fn dir_hash(path: &Path) -> u64 {
    use std::os::unix::fs::MetadataExt;
    let meta = match std::fs::metadata(path) {
        Ok(m) => m,
        Err(_) => return 0,
    };
    let dev = meta.dev();
    let ino = meta.ino();
    let mtime = meta.mtime() as u64;
    // Simple hash combine
    dev.wrapping_mul(6364136223846793005)
        ^ ino.wrapping_mul(1442695040888963407)
        ^ mtime
}

// ---------------------------------------------------------------------------
// Prefix setup
// ---------------------------------------------------------------------------

fn setup_prefix(wine_dir: &Path, pfx: &Path, wine64: &Path, self_exe: &Path) {
    let default_pfx = wine_dir.join("share").join("default_pfx");
    if default_pfx.exists() {
        setup_prefix_from_template(&default_pfx, pfx);
    } else {
        setup_prefix_with_wineboot(wine_dir, pfx, wine64, self_exe);
    }

    // Prevent Wine's internal wineboot from re-running wine.inf processing.
    // Our setup_prefix handles prefix init; the Wine-level update spawns
    // multiple rundll32 passes (PreInstall + DefaultInstall) that can take
    // minutes and block the game from loading.
    let ts_file = pfx.join(".update-timestamp");
    let _ = std::fs::write(&ts_file, "disable\n");
}

/// Cache invalidation drives re-setup. copy_dir_fast skips existing files,
/// so re-running is safe and adds anything missing (e.g. symlinks from a buggy deploy).
fn setup_prefix_from_template(default_pfx: &Path, pfx: &Path) {
    let fresh = !pfx.join("system.reg").exists();
    if fresh {
        log_verbose!("Setting up prefix from template...");
    } else {
        log_verbose!("Repairing prefix from template...");
    }
    let count = copy_dir_fast(default_pfx, pfx);
    if count > 0 {
        log_verbose!("Prefix: {count} files deployed");
    }
    ensure_dosdevices(pfx);
}

/// No template available (system Wine) — fall back to wineboot.
fn setup_prefix_with_wineboot(wine_dir: &Path, pfx: &Path, wine64: &Path, self_exe: &Path) {
    // Create user profile dirs FIRST — wineboot's SHGetFolderPath needs them.
    ensure_user_profile_dirs(pfx);
    ensure_dosdevices(pfx);
    // Windows system dirs
    let _ = std::fs::create_dir_all(pfx.join("drive_c/windows/system32"));
    let _ = std::fs::create_dir_all(pfx.join("drive_c/windows/syswow64"));
    let _ = std::fs::create_dir_all(pfx.join("drive_c/Program Files"));
    let _ = std::fs::create_dir_all(pfx.join("drive_c/Program Files (x86)"));

    // A prefix seeded from the baked template has its registry already
    // (seed_prefix_registry); without one, wineboot --init applies wine.inf.
    // The launcher's own stub .reg files and directory layout say nothing
    // about whether a prefix was initialized.
    if quark_template(self_exe).is_some() {
        seed_from_quark_template(self_exe, pfx);
    } else {
        run_wineboot_init(wine_dir, pfx, wine64, self_exe);
    }
}

/// The Steam library holding the game, which Proton maps to S:: Of the
/// library paths Steam passes, the one the install path lies under.
pub(crate) fn game_library_dir(install_path: &str, library_paths: &str) -> Option<PathBuf> {
    let library = library_paths.split(':').find(|p| !p.is_empty() && install_path.contains(p))?;
    Some(steamapps_parent(Path::new(library)).unwrap_or_else(|| PathBuf::from(library)))
}

/// A steamapps directory's parent, the library's root, when that is
/// writable and on the same filesystem; Proton keeps the path as passed
/// otherwise.
fn steamapps_parent(library: &Path) -> Option<PathBuf> {
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::MetadataExt;
    let steamapps = std::fs::canonicalize(library).ok()?;
    if !steamapps.is_dir() || steamapps.file_name()? != "steamapps" {
        return None;
    }
    let parent = steamapps.parent()?;
    let c_parent = std::ffi::CString::new(parent.as_os_str().as_bytes()).ok()?;
    let writable = unsafe { libc::access(c_parent.as_ptr(), libc::W_OK) } == 0;
    let same_fs = std::fs::metadata(parent).ok()?.dev() == std::fs::metadata(&steamapps).ok()?.dev();
    (writable && same_fs).then(|| parent.to_path_buf())
}

/// S: follows the game's library each launch, and goes when Steam names none.
fn map_game_drive(pfx: &Path) {
    let link = pfx.join("dosdevices/s:");
    let install = std::env::var("STEAM_COMPAT_INSTALL_PATH").unwrap_or_default();
    let libraries = std::env::var("STEAM_COMPAT_LIBRARY_PATHS").unwrap_or_default();
    match game_library_dir(&install, &libraries) {
        Some(dir) if std::fs::read_link(&link).ok().as_deref() == Some(dir.as_path()) => {}
        Some(dir) => {
            let _ = std::fs::remove_file(&link);
            if let Err(e) = std::os::unix::fs::symlink(&dir, &link) {
                log_warn!("Cannot map S: to {}: {e}", dir.display());
            }
        }
        None => {
            let _ = std::fs::remove_file(&link);
        }
    }
}

/// The c: And z: Drive mappings.
fn ensure_dosdevices(pfx: &Path) {
    let dosdevices = pfx.join("dosdevices");
    if let Err(e) = std::fs::create_dir_all(&dosdevices) {
        log_error!("Cannot create dosdevices dir: {e} — drive mappings will fail");
    }
    let c_link = dosdevices.join("c:");
    let z_link = dosdevices.join("z:");
    if !c_link.exists() {
        if let Err(e) = std::os::unix::fs::symlink("../drive_c", &c_link) {
            log_error!("Cannot create c: drive symlink: {e}");
        }
    }
    if !z_link.exists() {
        if let Err(e) = std::os::unix::fs::symlink("/", &z_link) {
            log_error!("Cannot create z: drive symlink: {e}");
        }
    }
}

/// Copy quark's baked prefix template .reg files (COM registrations).
/// These contain 500+ CLSID entries from Wine's FakeDlls pass, baked
/// at install time by install.py's step_bake_prefix_template.
/// install.py's baked prefix template beside the launcher, if it has a
/// registry.
fn quark_template(self_exe: &Path) -> Option<PathBuf> {
    let tpl = self_exe.parent()?.join("default_pfx");
    tpl.join("system.reg").exists().then_some(tpl)
}

/// A new prefix takes the baked template's registry (its full COM class
/// registrations) before ensure_registry_stubs writes header-only .reg
/// files, which would otherwise keep the template out for good. Existing
/// .reg files are never replaced.
pub(crate) fn seed_prefix_registry(self_exe: &Path, pfx: &Path) {
    let Some(tpl) = quark_template(self_exe) else { return };
    let _ = std::fs::create_dir_all(pfx);
    for reg in ["system.reg", "user.reg", "userdef.reg"] {
        let src = tpl.join(reg);
        let dst = pfx.join(reg);
        if src.exists() && !dst.exists() {
            match std::fs::copy(&src, &dst) {
                Ok(_) => log_verbose!("Prefix: {reg} seeded from {}", tpl.display()),
                Err(e) => log_warn!("Prefix: seeding {reg} from {} failed: {e}", tpl.display()),
            }
            // The user registry's shell folders (TEMP's AppData\Local\Temp
            // among them) name directories only the template's tree holds.
            if reg == "user.reg" {
                mirror_dirs(&tpl.join("drive_c/users"), &pfx.join("drive_c/users"));
            }
        }
    }
}

/// Recreate src's directories under dst, without its files or symlinks, so
/// nothing in the prefix shares an inode with the template.
fn mirror_dirs(src: &Path, dst: &Path) {
    let Ok(entries) = std::fs::read_dir(src) else { return };
    for entry in entries.flatten() {
        if entry.file_type().is_ok_and(|t| t.is_dir()) {
            let sub = dst.join(entry.file_name());
            if let Err(e) = std::fs::create_dir_all(&sub) {
                log_warn!("Prefix: creating {} failed: {e}", sub.display());
                continue;
            }
            mirror_dirs(&entry.path(), &sub);
        }
    }
}

fn seed_from_quark_template(self_exe: &Path, pfx: &Path) {
    let Some(tpl) = quark_template(self_exe) else { return };
    // The baked template includes filesystem layout that
    // wineboot --init does NOT recreate — most importantly
    // system32\wbem and syswow64\wbem, registered in
    // system.reg as the load paths for WBEM Locator
    // (CLSID 4590f811). Unity audio init goes through
    // CoCreateInstance(WBEM Locator) for device enumeration;
    // a missing wbemprox.dll on that path makes Wine's
    // apartment_add_dll fail and Unity's audio_client_timer
    // hits its internal debugbreak.
    for subdir in [
        "drive_c/windows/system32/wbem",
        "drive_c/windows/syswow64/wbem",
    ] {
        let src = tpl.join(subdir);
        let dst = pfx.join(subdir);
        if src.is_dir() && !dst.exists() {
            if let Err(e) = copy_dir_recursive(&src, &dst) {
                log_verbose!("Prefix: failed to seed {}: {}", subdir, e);
            }
        }
    }
    log_verbose!("Prefix: seeded wbem from baked template {}", tpl.display());
}

fn run_wineboot_init(wine_dir: &Path, pfx: &Path, wine64: &Path, self_exe: &Path) {
    log_verbose!("No default_pfx template, running wineboot --init...");
    let mut cmd = build_wine_command(wine64, wine_dir);
    cmd.args(["wineboot", "--init"]);
    cmd.env("WINEPREFIX", pfx.as_os_str());
    cmd.env("USER", profile_user(pfx));
    cmd.env("WINESERVER", wineserver_exe(wine_dir));
    cmd.env("LD_LIBRARY_PATH", wine_ld_path(wine_dir));
    // WINELOADER: Wine child processes re-exec via this path.
    cmd.env("WINELOADER", wine64.as_os_str());
    cmd.env("WINEDLLPATH", wine_dll_path(wine_dir));
    let dll_dir = format!("\\??\\Z:{}", wine_dir.join("lib/wine").display());
    cmd.env("WINEDLLDIR0", &dll_dir);
    cmd.env("WINEDEBUG", resolve_winedebug(false));
    // Preload assertion suppressor: Converts assert() failures in Wine's
    // ntdll unix layer to warnings. Targets add_fd_to_cache collision
    // and user_check_not_lock race during display driver init.
    let noassert = {
        // Check installed path first, then dev path
        let installed = PathBuf::from(std::env::var("HOME").unwrap_or_default())
            .join(".local/share/Steam/compatibilitytools.d/quark/lib/noassert.so");
        if installed.exists() { installed }
        else { self_exe.parent().unwrap_or(Path::new(".")).join("lib/noassert.so") }
    };
    // Strip Steam overlay from LD_PRELOAD for wineboot too
    let mut preload = ld_preload_without_overlay();
    if noassert.exists() {
        if !preload.is_empty() { preload.push(':'); }
        preload.push_str(&noassert.display().to_string());
    }
    if preload.is_empty() {
        cmd.env_remove("LD_PRELOAD");
    } else {
        cmd.env("LD_PRELOAD", &preload);
    }
    match cmd.status() {
        Ok(s) if !s.success() => log_error!("wineboot --init failed with exit code {}", s.code().unwrap_or(-1)),
        Err(e) => log_error!("Failed to run wineboot: {e}"),
        _ => {}
    }
}

/// Inject registry keys that Proton normally installs via redistributable packages.
/// The DLLs are already in the prefix (from template symlinks), but some games
/// check registry keys to verify installation before even trying to load them.
fn inject_registry_keys(pfx: &Path) {
    use std::io::Write;
    let sys_reg = pfx.join("system.reg");
    let user_reg = pfx.join("user.reg");
    // Both hives are read whole below; on a launch where they are exactly as
    // the last injection left them, by size and mtime, there is nothing to
    // read. The marker holds the stamps the last injection ended with.
    let marker = pfx.join(".quark_registry_seeded");
    let stamps = format!("{},{}", file_stamp(&sys_reg), file_stamp(&user_reg));
    if std::fs::read_to_string(&marker).is_ok_and(|m| m.trim() == stamps) {
        return;
    }

    // ── Template-based registry: Copy from stock Wine prefix ──
    // Instead of injecting dozens of individual keys (services, shell folders,
    // VC++ runtime, etc.), copy the complete registry from ~/.wine which was
    // created by wineboot --init under stock wineserver. This gives us a
    // fully correct registry matching the installed Wine version.
    let home = std::env::var("HOME").unwrap_or_else(|_| "/root".into());
    let stock_prefix = PathBuf::from(&home).join(".wine");
    let stock_sys = stock_prefix.join("system.reg");
    let stock_user = stock_prefix.join("user.reg");
    let stock_userdef = stock_prefix.join("userdef.reg");

    // Only copy if the target is a stub (our minimal header) or missing.
    // If the target already has real content (>1KB), don't overwrite — the
    // wineserver may have saved updated registry state from a previous session.
    let is_stub = |path: &Path| -> bool {
        path.metadata().map(|m| m.len() < 1024).unwrap_or(true)
    };

    if stock_sys.exists() && is_stub(&sys_reg) {
        match std::fs::copy(&stock_sys, &sys_reg) {
            Ok(bytes) => log_verbose!("Registry: copied stock system.reg ({bytes} bytes)"),
            Err(e) => log_error!("Failed to copy stock system.reg: {e}"),
        }
    }
    if stock_user.exists() && is_stub(&user_reg) {
        match std::fs::copy(&stock_user, &user_reg) {
            Ok(bytes) => log_verbose!("Registry: copied stock user.reg ({bytes} bytes)"),
            Err(e) => log_error!("Failed to copy stock user.reg: {e}"),
        }
    }
    if stock_userdef.exists() {
        let userdef_reg = pfx.join("userdef.reg");
        if is_stub(&userdef_reg) {
            let _ = std::fs::copy(&stock_userdef, &userdef_reg);
        }
    }

    // Steam paths (HKLM + HKCU) — only if not already present
    {
        let sys_contents = std::fs::read_to_string(&sys_reg).unwrap_or_default();
        if !sys_contents.contains("[Software\\\\Valve\\\\Steam]") {
            let keys = "\n[Software\\\\Valve\\\\Steam] 1772204972\n#time=1dca7fb13d11a48\n\"InstallPath\"=\"C:\\\\Program Files (x86)\\\\Steam\"\n\"SteamPath\"=\"C:\\\\Program Files (x86)\\\\Steam\"\n\n";
            if let Ok(mut f) = std::fs::OpenOptions::new().append(true).open(&sys_reg) {
                let _ = f.write_all(keys.as_bytes());
                log_verbose!("Registry: injected Steam InstallPath");
            }
        }

        let user_contents = std::fs::read_to_string(&user_reg).unwrap_or_default();
        if !user_contents.contains("Valve\\\\Steam\\\\ActiveProcess") {
            let keys = "\n[Software\\\\Valve\\\\Steam] 1772204972\n#time=1dca7fb13d11a48\n\"language\"=\"english\"\n\"SteamExe\"=\"C:\\\\Program Files (x86)\\\\Steam\\\\Steam.exe\"\n\"SteamPath\"=\"C:\\\\Program Files (x86)\\\\Steam\"\n\n[Software\\\\Valve\\\\Steam\\\\ActiveProcess] 1772204972\n#time=1dca7fb13d11a48\n\"PID\"=dword:0000fffe\n\"SteamClientDll\"=\"C:\\\\Program Files (x86)\\\\Steam\\\\steamclient.dll\"\n\"SteamClientDll64\"=\"C:\\\\Program Files (x86)\\\\Steam\\\\steamclient64.dll\"\n\"SteamPath\"=\"C:\\\\Program Files (x86)\\\\Steam\"\n\n";
            if let Ok(mut f) = std::fs::OpenOptions::new().append(true).open(&user_reg) {
                let _ = f.write_all(keys.as_bytes());
                log_verbose!("Registry: injected Steam client paths (HKCU)");
            }
        }
    }
    let stamps = format!("{},{}", file_stamp(&sys_reg), file_stamp(&user_reg));
    let _ = std::fs::write(&marker, stamps);
}

/// Fast recursive directory copy using getdents64 + hardlinks.
/// getdents64: One syscall fills a 32KB buffer with hundreds of entries.
/// Hardlinks: Near-instant, share disk blocks.
/// Falls back to copy on cross-device.
/// Returns number of files deployed.
pub(crate) fn copy_dir_fast(src: &Path, dst: &Path) -> u32 {
    copy_dir_level(src, dst, 0)
}

/// Subdirectories met in the top two levels copy on their own scoped
/// threads; a template prefix is a handful of wide directories under
/// drive_c, so this is where the files are. The walk itself stays one
/// getdents64 call per 32 KB of entries.
const COPY_FANOUT_DEPTH: u32 = 2;

fn copy_dir_level(src: &Path, dst: &Path, depth: u32) -> u32 {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let src_c = match CString::new(src.as_os_str().as_bytes()) {
        Ok(c) => c,
        Err(_) => return 0,
    };
    let fd = unsafe { libc::open(src_c.as_ptr(), libc::O_RDONLY | libc::O_DIRECTORY) };
    if fd < 0 {
        log_error!("copy_dir_fast: failed to open {}", src.display());
        return 0;
    }
    if let Err(e) = std::fs::create_dir_all(dst) {
        log_error!("copy_dir_fast: cannot create {}: {e}", dst.display());
        unsafe { libc::close(fd) };
        return 0;
    }

    // 32KB buffer — one syscall gets hundreds of entries
    let mut buf = [0u8; 32 * 1024];
    let mut count = 0u32;
    let mut subdirs: Vec<(PathBuf, PathBuf)> = Vec::new();

    loop {
        let nread = unsafe {
            libc::syscall(libc::SYS_getdents64, fd, buf.as_mut_ptr(), buf.len()) as isize
        };
        if nread < 0 {
            log_error!("copy_dir_fast: getdents64 failed on {} (errno={})",
                src.display(), std::io::Error::last_os_error());
            break;
        }
        if nread == 0 {
            break; // end of directory
        }

        let mut pos = 0usize;
        while pos < nread as usize {
            // linux_dirent64: D_ino(8) + d_off(8) + d_reclen(2) + d_type(1) + d_name(...)
            let d_reclen = u16::from_ne_bytes([buf[pos + 16], buf[pos + 17]]) as usize;
            let d_type = buf[pos + 18];
            let name_start = pos + 19;
            let name_end = buf[name_start..pos + d_reclen]
                .iter()
                .position(|&b| b == 0)
                .map(|i| name_start + i)
                .unwrap_or(pos + d_reclen);
            let name = &buf[name_start..name_end];
            pos += d_reclen;
            if name == b"." || name == b".." {
                continue;
            }

            let name_os = std::ffi::OsStr::from_bytes(name);
            let src_path = src.join(name_os);
            let dst_path = dst.join(name_os);

            // DT_UNKNOWN=0, DT_DIR=4, DT_REG=8, DT_LNK=10. Some filesystems
            // (NFS, FUSE) report DT_UNKNOWN; fall back to stat.
            let d_type = if d_type == 0 {
                match src_path.symlink_metadata() {
                    Ok(m) if m.is_dir() => 4,
                    Ok(m) if m.file_type().is_symlink() => 10,
                    Ok(_) => 8,
                    Err(e) => {
                        log_warn!("copy_dir_fast: cannot stat {}: {e}", src_path.display());
                        continue;
                    }
                }
            } else {
                d_type
            };

            match d_type {
                4 if depth < COPY_FANOUT_DEPTH => subdirs.push((src_path, dst_path)),
                4 => count += copy_dir_level(&src_path, &dst_path, depth + 1),
                10 => count += repair_symlink(&src_path, &dst_path) as u32,
                _ => count += link_or_copy(&src_path, &dst_path) as u32,
            }
        }
    }
    unsafe { libc::close(fd) };

    if !subdirs.is_empty() {
        count += std::thread::scope(|s| {
            let handles: Vec<_> = subdirs
                .iter()
                .map(|(sp, dp)| s.spawn(move || copy_dir_level(sp, dp, depth + 1)))
                .collect();
            handles.into_iter().map(|h| h.join().unwrap_or(0)).sum::<u32>()
        });
    }
    count
}

/// Template symlinks are relative paths (../../../../../lib/wine/...) that
/// only resolve inside Proton's tree, so the copy points at the resolved
/// file. An existing link is fixed only when broken and fixable: The source
/// now resolves, or the link differs from the template's. A link dead in the
/// template too is already the faithful copy. Returns whether a link was written.
fn repair_symlink(src_path: &Path, dst_path: &Path) -> bool {
    let needs_fix = match dst_path.symlink_metadata() {
        Ok(meta) if meta.file_type().is_symlink() => {
            !dst_path.exists()
                && (std::fs::canonicalize(src_path).is_ok()
                    || std::fs::read_link(dst_path).ok() != std::fs::read_link(src_path).ok())
        }
        Ok(_) => true,  // regular file where a symlink should be
        Err(_) => true, // absent
    };
    if !needs_fix {
        return false;
    }
    if let Err(e) = std::fs::remove_file(dst_path) {
        if e.kind() != std::io::ErrorKind::NotFound {
            log_warn!("Cannot remove {}: {e}", dst_path.display());
        }
    }
    // canonicalize resolves the relative link to the real file; a link dead
    // in the template is copied as-is.
    let target = std::fs::canonicalize(src_path).or_else(|_| std::fs::read_link(src_path));
    match target {
        Ok(t) => {
            if let Err(e) = std::os::unix::fs::symlink(&t, dst_path) {
                log_warn!("Symlink failed {}: {e}", dst_path.display());
            }
            true
        }
        Err(_) => false,
    }
}

/// Regular file: hardlink first (instant), copy fallback; an existing file
/// is left alone. Returns whether a file was deployed.
fn link_or_copy(src_path: &Path, dst_path: &Path) -> bool {
    if dst_path.exists() {
        return false;
    }
    if std::fs::hard_link(src_path, dst_path).is_ok() || std::fs::copy(src_path, dst_path).is_ok() {
        true
    } else {
        log_warn!("copy_dir_fast: failed to deploy {}", src_path.display());
        false
    }
}

// ---------------------------------------------------------------------------
// DXVK / VKD3D-Proton deployment (conditional)
// ---------------------------------------------------------------------------

fn deploy_dxvk(wine_dir: &Path, pfx: &Path) -> Vec<&'static str> {
    // 64-bit → system32
    let src64 = wine_dir.join("lib/wine/dxvk/x86_64-windows");
    let sys32 = pfx.join("drive_c/windows/system32");
    let deployed = deploy_dlls(&src64, &sys32, DXVK_DLLS, "DXVK");

    // 32-bit → syswow64 (for 32-bit games running under WoW64)
    let src32 = wine_dir.join("lib/wine/dxvk/i386-windows");
    let syswow64 = pfx.join("drive_c/windows/syswow64");
    deploy_dlls(&src32, &syswow64, DXVK_DLLS, "DXVK-32");

    deployed
}

fn deploy_vkd3d(wine_dir: &Path, pfx: &Path) -> Vec<&'static str> {
    // 64-bit → system32
    let src64 = wine_dir.join("lib/wine/vkd3d-proton/x86_64-windows");
    let sys32 = pfx.join("drive_c/windows/system32");
    let deployed = deploy_dlls(&src64, &sys32, VKD3D_DLLS, "VKD3D");

    // 32-bit → syswow64
    let src32 = wine_dir.join("lib/wine/vkd3d-proton/i386-windows");
    let syswow64 = pfx.join("drive_c/windows/syswow64");
    deploy_dlls(&src32, &syswow64, VKD3D_DLLS, "VKD3D-32");

    deployed
}

fn deploy_nvapi(wine_dir: &Path, pfx: &Path) -> Vec<&'static str> {
    // dxvk-nvapi from quark's own tree (install.py pins its release):
    // nvapi64 and nvofapi64 to system32, the 32-bit nvapi to syswow64.
    let src64 = wine_dir.join("lib/wine/dxvk-nvapi/x86_64-windows");
    let src32 = wine_dir.join("lib/wine/dxvk-nvapi/i386-windows");
    let sys32 = pfx.join("drive_c/windows/system32");
    let syswow64 = pfx.join("drive_c/windows/syswow64");
    let mut deployed = deploy_dlls(&src64, &sys32, NVAPI_DLLS, "NVAPI");
    deployed.extend(deploy_dlls(&src32, &syswow64, NVAPI32_DLLS, "NVAPI-32"));
    deployed
}

fn deploy_dlss(pfx: &Path) -> Vec<&'static str> {
    let Some(src) = nvidia_wine_dll_dir() else { return vec![] };
    let sys32 = pfx.join("drive_c/windows/system32");
    deploy_dlls(&src, &sys32, DLSS_DLLS, "DLSS")
}

/// The NVIDIA driver's Wine DLLs, where Proton finds them: The nvidia/wine
/// directory beside the real libGLX_nvidia, when it holds nvngx.dll. The
/// library is looked up in the loader's standard directories, never loaded.
pub(crate) fn nvidia_wine_dll_dir() -> Option<PathBuf> {
    let lib = ["/usr/lib", "/usr/lib64", "/usr/lib/x86_64-linux-gnu"].iter()
        .find_map(|dir| std::fs::canonicalize(Path::new(dir).join("libGLX_nvidia.so.0")).ok())?;
    let dir = lib.parent()?.join("nvidia/wine");
    dir.join("nvngx.dll").exists().then_some(dir)
}

pub(crate) fn deploy_dlls(
    src_dir: &Path, dst_dir: &Path, dlls: &[&'static str], label: &str,
) -> Vec<&'static str> {
    if !src_dir.exists() {
        log_warn!("{label}: source dir not found ({})", src_dir.display());
        return vec![];
    }
    if let Err(e) = std::fs::create_dir_all(dst_dir) {
        log_error!("{label}: cannot create {}: {e}", dst_dir.display());
        return vec![];
    }
    let mut deployed = Vec::new();
    let mut skipped = 0u32;
    for name in dlls {
        let src = src_dir.join(format!("{name}.dll"));
        let dst = dst_dir.join(format!("{name}.dll"));
        if !src.exists() {
            continue;
        }
        // Skip if already deployed and same size
        if file_matches(&src, &dst) {
            skipped += 1;
            deployed.push(*name);
            continue;
        }
        // Remove destination first — hardlinked files from Proton are read-only,
        // and copy() can't overwrite them. Unlinking is always allowed.
        if let Err(e) = std::fs::remove_file(&dst) {
            if dst.exists() {
                log_warn!("{label}: cannot remove old {name}.dll: {e}");
            }
        }
        match std::fs::copy(&src, &dst) {
            Ok(_) => deployed.push(*name),
            Err(e) => log_warn!("{label}: failed to copy {name}.dll: {e}"),
        }
    }
    let copied = deployed.len() as u32 - skipped;
    if copied > 0 {
        log_verbose!("{label}: deployed {copied} DLLs ({skipped} already current)");
    } else if skipped > 0 {
        log_verbose!("{label}: {skipped} DLLs already current");
    }
    deployed
}

/// Check if dst exists and is up-to-date relative to src (same size, dst mtime >= src mtime).
pub(crate) fn file_matches(src: &Path, dst: &Path) -> bool {
    let Ok(src_meta) = std::fs::metadata(src) else { return false };
    let Ok(dst_meta) = std::fs::metadata(dst) else { return false };
    src_meta.len() == dst_meta.len()
        && src_meta.modified().ok() <= dst_meta.modified().ok()
}

// ---------------------------------------------------------------------------
// Steam client integration (conditional)
// ---------------------------------------------------------------------------

/// Valve's own client DLLs into the prefix's Steam dir, as Proton has them.
/// The trampoline (Wine patch 001) treats whatever loads as steamclient64.dll
/// as Valve's native DLL: It fixes up its imports, skips its DllMain and
/// redirects its exports to the builtin lsteamclient it loads beside it. The
/// bridge deployed under that name is the builtin itself, so the skipped
/// DllMain was its own init and no bridge call ever reached Steam.
pub(crate) fn deploy_steam_client(steam_dir: &Path, pfx: &Path) {
    let legacy = steam_dir.join("legacycompat");
    if !legacy.exists() {
        log_warn!("Steam legacycompat not found at {}", legacy.display());
        return;
    }

    let steam_pfx = pfx.join("drive_c/Program Files (x86)/Steam");
    if let Err(e) = std::fs::create_dir_all(&steam_pfx) {
        log_error!("Cannot create Steam client dir: {e}");
        return;
    }
    // Left by the deploy that copied the bridge there as well.
    let stale = pfx.join("drive_c/windows/system32/steamclient64.dll");
    if stale.exists() {
        match std::fs::remove_file(&stale) {
            Ok(_) => log_verbose!("Steam client: removed the bridge copy in system32"),
            Err(e) => log_warn!("Steam client: cannot remove {}: {e}", stale.display()),
        }
    }

    let mut copied = 0u32;
    let mut skipped = 0u32;
    for (src_name, dst_name) in STEAM_CLIENT_FILES {
        let src = legacy.join(src_name);
        let dst = steam_pfx.join(dst_name);
        if !src.exists() {
            continue;
        }
        if file_matches(&src, &dst) {
            skipped += 1;
            continue;
        }
        if let Err(e) = std::fs::remove_file(&dst) {
            if dst.exists() {
                log_warn!("Steam client: cannot remove old {dst_name}: {e}");
            }
        }
        if let Err(e) = std::fs::copy(&src, &dst) {
            log_warn!("Steam client: failed to copy {src_name}: {e}");
        } else {
            copied += 1;
        }
    }
    if copied > 0 {
        log_verbose!("Steam client: deployed {copied} files ({skipped} already current)");
    } else if skipped > 0 {
        log_verbose!("Steam client: {skipped} files already current");
    }
}

/// Ensure steam.exe exists in the prefix's system32 and syswow64.
/// Sources: Cached copy (~/.local/share/quark/steam.exe) → Proton (fallback).
fn deploy_steam_exe(self_exe: &Path, pfx: &Path) {
    // Source our steam_bridge from the quark lib directory (deployed by install.py).
    // Always overwrite — Proton's builtin steam.exe may already exist in the prefix,
    // and our native bridge must replace it (WINEDLLOVERRIDES=steam.exe=n).
    let bridge_src = self_exe.parent()
        .map(|d| d.join("lib/wine/x86_64-windows/steam.exe"));

    let src = match bridge_src {
        Some(ref p) if p.exists() => p,
        _ => {
            log_warn!("steam_bridge not found — games requiring Steam API will fail");
            return;
        }
    };

    let sys32 = pfx.join("drive_c/windows/system32");
    let syswow64 = pfx.join("drive_c/windows/syswow64");

    for dst_dir in [&sys32, &syswow64] {
        let dst = dst_dir.join("steam.exe");
        // Clean up broken symlinks
        if dst.symlink_metadata().is_ok() && !dst.exists() {
            let _ = std::fs::remove_file(&dst);
        }
        let _ = std::fs::create_dir_all(dst_dir);
        if file_matches(src, &dst) { continue; }  // already current; cheap re-deploy no-op
        if let Err(e) = std::fs::copy(src, &dst) {
            log_warn!("steam.exe: failed to deploy to {}: {e}", dst_dir.display());
        }
    }
}

// ---------------------------------------------------------------------------
// Environment construction
// ---------------------------------------------------------------------------

fn deploy_lsteamclient_to_prefix(self_exe: &Path, pfx: &Path) {
    let src = self_exe.parent()
        .map(|d| d.join("lib/wine/x86_64-windows/lsteamclient.dll"));
    let src = match src {
        Some(ref p) if p.exists() => p,
        _ => return,
    };
    let sys32 = pfx.join("drive_c/windows/system32");
    let _ = std::fs::create_dir_all(&sys32);
    let dst = sys32.join("lsteamclient.dll");
    if file_matches(src, &dst) { return; }  // already current; cheap re-deploy no-op
    if let Err(e) = std::fs::copy(src, &dst) {
        log_warn!("lsteamclient.dll: failed to deploy to system32: {e}");
    }
}

/// WINEDLLPATH: The Wine tree's PE and unix module dirs, then system Wine's.
/// quark-built modules (lsteamclient, steam.exe) live in the Wine tree;
/// without them on the path LoadLibraryW("lsteamclient") gets an empty stub
/// and Steam auth fails.
pub(crate) fn wine_dll_path(wine_dir: &Path) -> String {
    [
        wine_dir.join("lib/wine/x86_64-unix"),
        wine_dir.join("lib/wine/x86_64-windows"),
        PathBuf::from("/usr/lib/wine/x86_64-unix"),
        PathBuf::from("/usr/lib/wine/x86_64-windows"),
    ]
    .iter()
    .filter(|p| p.exists())
    .map(|p| p.display().to_string())
    .collect::<Vec<_>>()
    .join(":")
}

/// LD_LIBRARY_PATH: Wine's unix modules list ntdll.so as NEEDED, so the
/// x86_64-unix dir must resolve by name, ahead of the inherited path.
pub(crate) fn wine_ld_path(wine_dir: &Path) -> String {
    let mut parts: Vec<String> = [wine_dir.join("lib/wine/x86_64-unix"), wine_dir.join("lib")]
        .iter()
        .filter(|p| p.exists())
        .map(|p| p.display().to_string())
        .collect();
    if let Ok(existing) = std::env::var("LD_LIBRARY_PATH") {
        parts.push(existing);
    }
    parts.join(":")
}

pub(crate) fn build_env_vars(
    wine_dir: &Path, pfx: &Path,
    dxvk: &[&str], vkd3d: &[&str], nvapi: &[&str], dlss: &[&str],
) -> Vec<(&'static str, String)> {
    let cur_path = std::env::var("PATH").unwrap_or_default();
    let wine_bin = wine_dir.join("bin");

    // WINEDLLOVERRIDES: base + deployed DLLs
    let mut overrides: Vec<String> = Vec::new();
    for (name, mode) in BASE_OVERRIDES {
        overrides.push(format!("{name}={mode}"));
    }
    for name in dxvk { overrides.push(format!("{name}=n")); }
    for name in vkd3d { overrides.push(format!("{name}=n")); }
    for name in nvapi { overrides.push(format!("{name}=n")); }
    for name in dlss { overrides.push(format!("{name}=n")); }
    if !nvapi.is_empty() { overrides.push("nvcuda=b".into()); }
    // GameInput crashes one game (CW Bug 26376); Proton disables it there
    // and nowhere else.
    if std::env::var("SteamGameId").is_ok_and(|id| id == "1928420") {
        overrides.push("gameinput=d".into());
    }
    let dll_overrides = overrides.join(";");

    // WINELOADER: Wine child processes re-exec via this path. Without it,
    // fork+exec fails with "could not exec the wine loader".
    let wine64 = wine_binary(wine_dir);

    // Wine NT-path environment variables. Wine's ntdll uses these to find
    // DLLs, shared data, and the prefix. Format: \\??\\Z:\\unix\\path
    let home = std::env::var("HOME").unwrap_or_else(|_| "/home/user".into());
    // NT path prefix: \??\Z: (one backslash, ??, one backslash, Z:)
    let nt_prefix = "\\??\\Z:";
    let wine_data_dir = format!("{}{}", nt_prefix, wine_dir.join("share/wine").display().to_string().replace('/', "\\"));
    let wine_dll_dir0 = format!("{}{}", nt_prefix, wine_dir.join("lib/wine").display().to_string().replace('/', "\\"));
    let wine_config_dir = format!("{}{}", nt_prefix, pfx.display().to_string().replace('/', "\\"));
    let wine_home_dir = format!("{}{}", nt_prefix, home.replace('/', "\\"));
    let user = profile_user(pfx);

    let mut vars = vec![
        ("WINEPREFIX", pfx.display().to_string()),
        ("WINESERVER", wineserver_exe(wine_dir).display().to_string()),
        ("WINELOADER", wine64.display().to_string()),
        ("WINEDLLPATH", wine_dll_path(wine_dir)),
        ("WINEDATADIR", wine_data_dir),
        ("WINEDLLDIR0", wine_dll_dir0.clone()),
        ("WINEDLLDIR1", wine_dll_dir0), // vkd3d also searched here
        ("WINECONFIGDIR", wine_config_dir),
        ("WINEHOMEDIR", wine_home_dir),
        // Wine names its user after $USER: Ntdll's user_name, the
        // WINEUSERNAME it appends (which GetUserNameW returns, and which
        // shell32 writes into every %USERPROFILE% shell folder) and the
        // profile wineboot creates. The prefix's own profile is the one
        // source; the host user would split the prefix between two homes.
        ("USER", user.clone()),
        ("PATH", format!("{}:{}", wine_bin.display(), cur_path)),
        ("LD_LIBRARY_PATH", wine_ld_path(wine_dir)),
        ("WINEDLLOVERRIDES", dll_overrides),
        // As Proton sets them, unless the environment already does:
        // Translation-layer logging off, and GLVND's entry-point patching off.
        ("DXVK_LOG_LEVEL", env_or("DXVK_LOG_LEVEL", "none")),
        ("VKD3D_DEBUG", env_or("VKD3D_DEBUG", "none")),
        ("VKD3D_SHADER_DEBUG", env_or("VKD3D_SHADER_DEBUG", "none")),
        ("__GLVND_DISALLOW_PATCHING", env_or("__GLVND_DISALLOW_PATCHING", "1")),
        ("PROTON_VERSION", format!("quark {}", env!("CARGO_PKG_VERSION"))),
    ];

    // EAC runtime: Its v2 lib dirs hold the builtin bridge DLLs, searched
    // after the Wine tree's, where Proton's ntdll adds them.
    let eac_runtime = PathBuf::from(&home)
        .join(".local/share/Steam/steamapps/common/Proton EasyAntiCheat Runtime");
    if eac_runtime.exists() {
        if let Some((_, dll_path)) = vars.iter_mut().find(|(k, _)| *k == "WINEDLLPATH") {
            for sub in ["v2/lib32", "v2/lib64"] {
                dll_path.push(':');
                dll_path.push_str(&eac_runtime.join(sub).display().to_string());
            }
        }
        vars.push(("PROTON_EAC_RUNTIME", eac_runtime.display().to_string()));
    }

    // Windows profile environment variables — Wine's SHGetFolderPathW needs these
    // to resolve %APPDATA% etc. Without registry-backed ProfileList, the fallback
    // chain fails and games (e.g. LOVE2D) throw exceptions during filesystem init.
    let profile = format!("C:\\users\\{user}");
    vars.push(("USERPROFILE", profile.clone()));
    vars.push(("APPDATA", format!("{profile}\\AppData\\Roaming")));
    vars.push(("LOCALAPPDATA", format!("{profile}\\AppData\\Local")));
    vars.push(("HOMEDRIVE", "C:".into()));
    vars.push(("HOMEPATH", format!("\\users\\{user}")));
    vars.push(("ALLUSERSPROFILE", "C:\\ProgramData".into()));
    vars.push(("ProgramData", "C:\\ProgramData".into()));

    vars.extend(host_env_vars());
    vars
}

/// What the host brings to the session: The GPU driver's settings, the
/// locale and library path Steam hands over and Steam Input's gamepads. The
/// shader caches are Steam's: It points each driver's cache at the game's
/// `steamapps/shadercache/<appid>` before the tool starts, as it does for
/// Proton, and nothing here moves them.
fn host_env_vars() -> Vec<(&'static str, String)> {
    let mut vars = Vec::new();
    let is_nvidia = Path::new("/proc/driver/nvidia/version").exists();

    // NVIDIA: DXVK NVAPI support for DLSS/NGX. NGX looks for its DLLs in
    // the driver store, which NVIDIA_WINE_DLL_DIR stands in for.
    if is_nvidia {
        vars.push(("DXVK_ENABLE_NVAPI", "1".into()));
        if let Some(dir) = nvidia_wine_dll_dir() {
            vars.push(("NVIDIA_WINE_DLL_DIR", dir.display().to_string()));
        }
        // NVIDIA EGL/GLX — force NVIDIA's implementations, not mesa's
        let nvidia_egl = "/usr/share/glvnd/egl_vendor.d/10_nvidia.json";
        if std::path::Path::new(nvidia_egl).exists() {
            vars.push(("__EGL_VENDOR_LIBRARY_FILENAMES", nvidia_egl.into()));
        }
        vars.push(("__GLX_VENDOR_LIBRARY_NAME", "nvidia".into()));
        // PRIME render offload for NVIDIA Optimus hybrid laptops. Without
        // these, Vulkan only enumerates the integrated iGPU (we saw
        // "vulkan-only gpu Intel(R) HD Graphics 630" in the log on
        // a system with a GTX 1050 Mobile dGPU). DXVK then runs on the
        // weak iGPU and games can spin in display-config polling loops
        // waiting for capabilities the iGPU lacks.
        vars.push(("__NV_PRIME_RENDER_OFFLOAD", "1".into()));
        vars.push(("__VK_LAYER_NV_optimus", "NVIDIA_only".into()));
    }

    // Proton compatibility fixes
    // Don't set WINEARCH — Wine rejects empty string. System Wine defaults to win64.
    // Restore locale (Steam sets LC_ALL=C which breaks Wine path conversion)
    if let Ok(host_lc) = std::env::var("HOST_LC_ALL") {
        if !host_lc.is_empty() {
            vars.push(("LC_ALL", host_lc));
        }
    } else {
        vars.push(("LC_ALL", "".into())); // clear Steam's LC_ALL=C
    }
    // Save original LD_LIBRARY_PATH for Wine's external app calls
    if std::env::var("ORIG_LD_LIBRARY_PATH").is_err() {
        vars.push(("ORIG_LD_LIBRARY_PATH",
            std::env::var("LD_LIBRARY_PATH").unwrap_or_default()));
    }

    // Steam Input: SDL 2.30+ reads SteamVirtualGamepadInfo to configure
    // virtual gamepads. Steam sets SteamVirtualGamepadInfo_Proton before
    // launching compat tools — Proton copies it to SteamVirtualGamepadInfo.
    // Without this, controllers are invisible to SDL-based games.
    if let Ok(gamepad_info) = std::env::var("SteamVirtualGamepadInfo_Proton") {
        vars.push(("SteamVirtualGamepadInfo", gamepad_info));
    }

    // Display driver: Always winex11.drv + GLX over XWayland.

    vars
}

/// The environment's value for `key`, or `default` when it has none.
fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.into())
}

/// Parse user-supplied environment variables from env_config file.
/// Format: KEY=VALUE lines. # comments and blank lines are ignored.
/// Returns empty Vec if the file is missing or unreadable.
pub(crate) fn parse_env_config(self_exe: &Path) -> Vec<(String, String)> {
    let config_path = match self_exe.parent() {
        Some(dir) => dir.join("env_config"),
        None => return Vec::new(),
    };

    let contents = match std::fs::read_to_string(&config_path) {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };

    let mut vars = Vec::new();
    for raw_line in contents.lines() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some(eq_pos) = line.find('=') else {
            log_warn!("env_config: ignoring malformed line: {line}");
            continue;
        };
        let key = line[..eq_pos].trim();
        let value = line[eq_pos + 1..].trim();
        if key.is_empty() {
            continue;
        }
        vars.push((key.to_string(), value.to_string()));
    }

    if !vars.is_empty() {
        log_verbose!("env_config: loaded {} custom variable(s)", vars.len());
    }

    vars
}

/// Resolve WINEDEBUG value once. Called after env_config is applied.
/// Priority: env_config/shell > QUARK_TRACE_OPCODES > --verbose > default.
fn resolve_winedebug(trace: bool) -> String {
    // If the user (or env_config) explicitly set WINEDEBUG, respect it.
    if let Ok(val) = std::env::var("WINEDEBUG") {
        return val;
    }
    if trace {
        "+server,+timestamp".into()
    } else if crate::log::is_verbose() {
        "+process,err".into()
    } else {
        "-all".into()
    }
}

// Discovery

/// Check if a Wine directory has a usable Wine binary.
pub(crate) fn has_wine_bin(dir: &Path) -> bool {
    dir.join("lib/wine/x86_64-unix/wine").exists() || dir.join("bin/wine").exists()
}

/// Get the Wine binary path from a Wine directory.
/// Prefers the wine binary alongside ntdll.so — Wine resolves PE builtins
/// relative to ntdll.so's realpath, so the binary MUST be in the same
/// directory as ntdll.so for our patched ntdll.dll to be loaded.
pub(crate) fn wine_binary(dir: &Path) -> PathBuf {
    let unix_wine = dir.join("lib/wine/x86_64-unix/wine");
    if unix_wine.exists() { unix_wine } else { dir.join("bin/wine") }
}

/// A Command running `wine_bin` straight from quark's tree, with a warning
/// when the tree install.py lays out is incomplete.
fn build_wine_command(wine_bin: &Path, wine_dir: &Path) -> Command {
    // The tree is Wine's own install layout: Its loader is a copy of the
    // system's, so /proc/self/exe puts it inside quark's tree, and Wine
    // resolves lib/wine and share/wine relative to that. No namespace: A
    // user namespace unmaps root, the NVIDIA driver then sees its device
    // nodes owned by nobody and fork-execs nvidia-modprobe on every GPU
    // initialization in every process.
    let quark_lib_wine = wine_dir.join("lib/wine");
    let quark_share_wine = wine_dir.join("share/wine");
    if !quark_lib_wine.exists() || !quark_share_wine.exists() {
        log_warn!("quark Wine tree incomplete (missing {} or {}); patched modules will not load.",
            quark_lib_wine.display(), quark_share_wine.display());
        log_warn!("Re-run install.py to populate the compat tree.");
    }
    Command::new(wine_bin)
}

/// Find Wine binaries. Priority:
/// 1. QUARK_WINE_DIR env var (explicit override)
/// 2. quark's own staged binaries — system Wine copied here by
///    install.py so argv[0]/../lib/ resolves to our custom ntdll.so
///    (built from matching upstream Wine version)
/// 3. System Wine
/// 4. Proton Experimental
/// 5. Any Proton version
fn find_wine() -> PathBuf {
    if let Ok(dir) = std::env::var("QUARK_WINE_DIR") {
        let p = PathBuf::from(dir);
        if has_wine_bin(&p) {
            return p;
        }
    }

    // quark's staged wine binaries — uses our patched ntdll.so
    if let Ok(self_exe) = std::env::current_exe() {
        if let Some(self_dir) = self_exe.parent() {
            if has_wine_bin(self_dir)
                && self_dir.join("lib/wine/x86_64-unix/ntdll.so").exists()
            {
                return self_dir.to_path_buf();
            }
        }
    }

    let home = std::env::var("HOME").unwrap_or_default();

    if has_wine_bin(&PathBuf::from("/usr")) {
        return PathBuf::from("/usr");
    }

    let proton_exp = PathBuf::from(&home)
        .join(".steam/root/steamapps/common/Proton - Experimental/files");
    if has_wine_bin(&proton_exp) {
        log_warn!("system Wine not found — using Proton Experimental's wine");
        return proton_exp;
    }

    let common = PathBuf::from(&home).join(".steam/root/steamapps/common");
    if let Ok(entries) = std::fs::read_dir(&common) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with("Proton") {
                let files = entry.path().join("files");
                if has_wine_bin(&files) {
                    log_warn!("system Wine not found — using {name}'s wine");
                    return files;
                }
            }
        }
    }

    PathBuf::from("/nonexistent/wine")
}

/// Find Proton's files directory for steam.exe sourcing (fallback).
/// DXVK/VKD3D are now deployed directly by install.py.
fn find_proton_files() -> Option<PathBuf> {
    let home = std::env::var("HOME").unwrap_or_default();

    // Proton Experimental
    let proton_exp = PathBuf::from(&home)
        .join(".steam/root/steamapps/common/Proton - Experimental/files");
    if proton_exp.join("lib/wine").exists() {
        return Some(proton_exp);
    }

    // Any Proton version
    let common = PathBuf::from(&home).join(".steam/root/steamapps/common");
    if let Ok(entries) = std::fs::read_dir(&common) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with("Proton") {
                let files = entry.path().join("files");
                if files.join("lib/wine").exists() {
                    return Some(files);
                }
            }
        }
    }

    None
}

/// Find Steam installation directory.
fn find_steam_dir() -> PathBuf {
    // Steam sets this when launching compat tools
    if let Ok(dir) = std::env::var("STEAM_COMPAT_CLIENT_INSTALL_PATH") {
        let p = PathBuf::from(&dir);
        if p.join("linux64/steamclient.so").exists() {
            return p;
        }
    }

    let home = std::env::var("HOME").unwrap_or_default();

    // Standard Steam symlink
    let steam = PathBuf::from(&home).join(".steam/root");
    if steam.join("linux64/steamclient.so").exists() {
        return steam;
    }

    // Flatpak / alternative
    let alt = PathBuf::from(&home).join(".local/share/Steam");
    if alt.join("linux64/steamclient.so").exists() {
        return alt;
    }

    log_warn!("Steam directory not found — game may not connect to Steam");
    PathBuf::from(&home).join(".steam/root")
}

/// The user the prefix belongs to: Whose profile its registry's ProfileList
/// names, which is the template's (Proton bakes steamuser) or wineboot's.
/// A prefix with no registry yet is about to be created for this name.
pub(crate) fn profile_user(pfx: &Path) -> String {
    // One read of the hive and a byte scan over its lines, no allocation per
    // line: System.reg is 4 MB and the value sits near line 82,000, where a
    // String per line cost 285 ms a call.
    const KEY: &[u8] = b"\"ProfileImagePath\"=\"";
    if let Ok(data) = std::fs::read(pfx.join("system.reg")) {
        for line in data.split(|&b| b == b'\n') {
            let Some(path) = line.strip_prefix(KEY) else { continue };
            let Ok(path) = std::str::from_utf8(path) else { continue };
            if let Some(name) = path.trim_end_matches(['"', '\r']).rsplit("\\\\").next() {
                if !name.is_empty() {
                    return name.to_string();
                }
            }
        }
    }
    "steamuser".to_string()
}

/// The prefix's user profile, `drive_c/users/<profile_user>`.
fn user_dir(pfx: &Path) -> PathBuf {
    pfx.join("drive_c/users").join(profile_user(pfx))
}

/// Create the standard Windows user-profile directory tree in the prefix.
/// Idempotent (create_dir_all no-ops when present); games and wineboot's
/// SHGetFolderPath both need these to exist.
fn ensure_user_profile_dirs(pfx: &Path) {
    let user_dir = user_dir(pfx);
    for subdir in [
        "", "AppData", "AppData/Roaming", "AppData/Local",
        "AppData/Local/Microsoft", "AppData/Local/Microsoft/Windows",
        "AppData/Local/Microsoft/Windows/INetCache",
        "AppData/Local/Microsoft/Windows/History",
        "AppData/Local/Microsoft/Windows/INetCookies",
        "AppData/LocalLow", "Desktop", "Documents", "Downloads",
        "Music", "Pictures", "Videos", "Temp",
    ] {
        let _ = std::fs::create_dir_all(user_dir.join(subdir));
    }
    let public_dir = pfx.join("drive_c/users/Public");
    let _ = std::fs::create_dir_all(public_dir.join("Desktop"));
    let _ = std::fs::create_dir_all(public_dir.join("Documents"));
}

// ---------------------------------------------------------------------------
// Save data protection
// ---------------------------------------------------------------------------
// Snapshot save data before launch, restore any files that go missing.
// Prevents Steam Cloud sync from wiping saves on first launch with a new
// compatibility tool (empty registry → game thinks first run → sync conflict).

const SAVE_SCAN_DIRS: &[&str] = &[
    "AppData/Roaming",
    "AppData/Local",
    "AppData/LocalLow",
    "Documents",
];

const SAVE_SKIP_DIRS: &[&str] = &["Microsoft", "Temp"];

/// Snapshot all save data under the prefix before game launch.
/// Returns the backup directory path if anything was backed up.
pub(crate) fn snapshot_save_data(pfx: &Path) -> Option<PathBuf> {
    let user_dir = user_dir(pfx);
    if !user_dir.exists() {
        return None;
    }

    // Backup dir lives in $STEAM_COMPAT_DATA_PATH (parent of pfx)
    let backup_dir = pfx.parent()?.join("save_backup");

    // Remove old backup — we only keep the latest snapshot
    if backup_dir.exists() {
        if let Err(e) = std::fs::remove_dir_all(&backup_dir) {
            log_warn!("save backup: cannot remove old backup: {e}");
        }
    }

    let mut total_files = 0u32;
    let mut total_bytes = 0u64;

    for scan_dir in SAVE_SCAN_DIRS {
        let src = user_dir.join(scan_dir);
        if !src.is_dir() {
            continue;
        }

        // Scan subdirectories (game-specific folders like "FromSoftware/EldenRing")
        let entries = match std::fs::read_dir(&src) {
            Ok(e) => e,
            Err(_) => continue,
        };

        for entry in entries.flatten() {
            let name = entry.file_name();
            let name_str = name.to_string_lossy();

            // Skip non-save system directories
            if SAVE_SKIP_DIRS.iter().any(|s| name_str.eq_ignore_ascii_case(s)) {
                continue;
            }

            let entry_path = entry.path();
            if !entry_path.is_dir() {
                continue;
            }

            // Only backup dirs that contain actual files
            let (files, bytes) = count_files_recursive(&entry_path);
            if files == 0 {
                continue;
            }

            let dst = backup_dir.join(scan_dir).join(&name);
            copy_save_recursive(&entry_path, &dst);
            total_files += files;
            total_bytes += bytes;
        }
    }

    if total_files == 0 {
        return None;
    }

    // Write manifest for diagnostics
    let manifest = format!("{total_files} files, {total_bytes} bytes\n");
    if let Err(e) = std::fs::write(backup_dir.join("manifest.txt"), manifest) {
        log_warn!("save backup: cannot write manifest: {e}");
    }

    log_verbose!("save data snapshot: {total_files} files, {total_bytes} bytes");

    Some(backup_dir)
}

/// After game exits, check if any save files that existed pre-launch are now
/// missing. Restore only those — never overwrite saves the game just wrote.
pub(crate) fn restore_save_data(pfx: &Path, backup_dir: &Path) {
    if !backup_dir.exists() {
        return;
    }

    let user_dir = user_dir(pfx);
    let mut restored = 0u32;
    let mut unchanged = 0u32;

    for scan_dir in SAVE_SCAN_DIRS {
        let backup_scan = backup_dir.join(scan_dir);
        if !backup_scan.is_dir() {
            continue;
        }

        let entries = match std::fs::read_dir(&backup_scan) {
            Ok(e) => e,
            Err(_) => continue,
        };

        for entry in entries.flatten() {
            let backup_path = entry.path();
            if !backup_path.is_dir() {
                continue;
            }

            let original_path = user_dir.join(scan_dir).join(entry.file_name());
            let (r, u) = restore_missing_files(&backup_path, &original_path);
            restored += r;
            unchanged += u;
        }
    }

    log_verbose!("save data check: {restored} files restored, {unchanged} unchanged");

    if restored > 0 {
        // Keep backup as extra safety layer
        log_warn!("{restored} save files were missing after game exit — restored from backup");
    } else {
        // All good — clean up backup
        if let Err(e) = std::fs::remove_dir_all(backup_dir) {
            log_warn!("save backup: cleanup failed: {e}");
        }
    }
}

/// Recursively restore files that exist in backup but are missing from original.
fn restore_missing_files(backup: &Path, original: &Path) -> (u32, u32) {
    let mut restored = 0u32;
    let mut unchanged = 0u32;

    let entries = match std::fs::read_dir(backup) {
        Ok(e) => e,
        Err(_) => return (0, 0),
    };

    for entry in entries.flatten() {
        let backup_file = entry.path();
        let original_file = original.join(entry.file_name());

        if backup_file.is_dir() {
            let (r, u) = restore_missing_files(&backup_file, &original_file);
            restored += r;
            unchanged += u;
        } else {
            if original_file.exists() {
                unchanged += 1;
            } else {
                // File existed pre-launch but is now gone — restore it
                if let Some(parent) = original_file.parent() {
                    if let Err(e) = std::fs::create_dir_all(parent) {
                        log_warn!("save restore: cannot create dir {}: {e}", parent.display());
                    }
                }
                match std::fs::copy(&backup_file, &original_file) {
                    Ok(_) => restored += 1,
                    Err(e) => log_warn!("save restore: failed to restore {}: {e}", original_file.display()),
                }
            }
        }
    }

    (restored, unchanged)
}

/// Count files and total bytes in a directory tree.
fn count_files_recursive(dir: &Path) -> (u32, u64) {
    let mut files = 0u32;
    let mut bytes = 0u64;

    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return (0, 0),
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            let (f, b) = count_files_recursive(&path);
            files += f;
            bytes += b;
        } else {
            files += 1;
            bytes += entry.metadata().map(|m| m.len()).unwrap_or(0);
        }
    }

    (files, bytes)
}

/// Recursive copy for save data (actual copies, not hardlinks).
/// Save data is small (KB to low MB) so std::fs::copy is fine.
fn copy_save_recursive(src: &Path, dst: &Path) {
    if let Err(e) = std::fs::create_dir_all(dst) {
        log_warn!("save backup: cannot create dir {}: {e}", dst.display());
        return;
    }

    let entries = match std::fs::read_dir(src) {
        Ok(e) => e,
        Err(_) => return,
    };

    for entry in entries.flatten() {
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());

        if src_path.is_dir() {
            copy_save_recursive(&src_path, &dst_path);
        } else if std::fs::hard_link(&src_path, &dst_path).is_err() {
            // A hardlink keeps the data alive if the game deletes the
            // file, which is the case the restore covers; copy across
            // devices or where links are refused.
            if let Err(e) = std::fs::copy(&src_path, &dst_path) {
                log_warn!("save backup: failed to copy {}: {e}", src_path.display());
            }
        }
    }
}

/// Remove .dmp and .mdmp crash dumps from the prefix before launch.
/// Engines (Godot, Unity, Unreal) read their crash directories on startup
/// and may enter recovery/report paths that interfere with normal launch.
fn clean_crash_dumps(pfx: &Path) {
    let user_dir = user_dir(pfx);
    if !user_dir.exists() {
        return;
    }

    let mut removed = 0u32;
    for scan_dir in SAVE_SCAN_DIRS {
        let dir = user_dir.join(scan_dir);
        if dir.is_dir() {
            removed += remove_dumps_recursive(&dir);
        }
    }
    if removed > 0 {
        log_verbose!("cleaned {removed} stale crash dump(s)");
    }
}

fn remove_dumps_recursive(dir: &Path) -> u32 {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return 0,
    };
    let mut removed = 0u32;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            removed += remove_dumps_recursive(&path);
        } else if let Some(ext) = path.extension() {
            let ext = ext.to_string_lossy();
            if (ext.eq_ignore_ascii_case("dmp") || ext.eq_ignore_ascii_case("mdmp"))
                && std::fs::remove_file(&path).is_ok()
            {
                removed += 1;
            }
        }
    }
    removed
}
