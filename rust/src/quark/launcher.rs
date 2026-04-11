// quark launcher -- full Proton replacement.
//
// Steam calls: ./proton <verb> <exe> [args...]
// quark sets up the prefix, deploys DXVK/VKD3D, bridges Steam client,
// then launches wine with WINESERVER=triskelion. One binary. Full stack.
//
// Optimization: after first launch, the .triskelion_deployed cache records
// what was deployed and from where. Subsequent launches skip all file ops
// when the cache is valid — straight to wine64.

use triskelion::{log_info, log_warn, log_error, log_verbose};

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

fn triskelion_exe() -> PathBuf {
    std::env::current_exe().unwrap().parent().unwrap().join("triskelion")
}

pub fn triskelion_exe_pub() -> PathBuf { triskelion_exe() }

fn parallax_exe() -> PathBuf {
    std::env::current_exe().unwrap().parent().unwrap().join("parallax")
}

/// Ensure fds 0, 1, 2 are open. Wine's socketpair() picks the lowest free fd.
/// Under Steam's reaper, stdin may be closed → fd 0 is free → socketpair gets
/// fd 0 → child's set_stdio_fd overwrites it with /dev/null → WINESERVERSOCKET
/// points to /dev/null instead of the socket → child crashes.
fn ensure_stdio_fds() {
    for fd in 0..3 {
        unsafe {
            if libc::fcntl(fd, libc::F_GETFD) == -1 {
                libc::open(b"/dev/null\0".as_ptr() as *const _, libc::O_RDWR);
            }
        }
    }
}

// DXVK: DirectX 9/10/11 → Vulkan translation
const DXVK_DLLS: &[&str] = &["d3d11", "d3d10core", "d3d9", "dxgi"];

// VKD3D-Proton: DirectX 12 → Vulkan translation
const VKD3D_DLLS: &[&str] = &["d3d12", "d3d12core"];

// DXVK-NVAPI: NVIDIA GPU API for DLSS/Reflex
const NVAPI_DLLS: &[&str] = &["nvapi64", "nvofapi64"];

// NVIDIA DLSS/NGX DLLs (system-installed by NVIDIA driver)
const DLSS_DLLS: &[&str] = &["nvngx", "_nvngx", "nvngx_dlssg"];

// Base DLL overrides (before DXVK/VKD3D additions)
const BASE_OVERRIDES: &[(&str, &str)] = &[
    ("lsteamclient", "b"),  // builtin: Wine bridge to native Steam client (built from Proton source)
    ("steam.exe", "b"),  // builtin: our Wine builtin steam.exe (built from Proton source)
    ("dotnetfx35.exe", "b"),
    ("dotnetfx35setup.exe", "b"),
    ("beclient.dll", "b,n"),
    ("beclient_x64.dll", "b,n"),
    // XInput: use Wine builtins — routes through winebus.sys for HID enumeration
    ("xinput1_1.dll", "b"),
    ("xinput1_2.dll", "b"),
    ("xinput1_3.dll", "b"),
    ("xinput1_4.dll", "b"),
    ("xinput9_1_0.dll", "b"),
    ("xinputuap.dll", "b"),
    // EasyAntiCheat: builtin from Proton EAC Runtime (Steam tool 1826330)
    ("easyanticheat", "b"),
    ("easyanticheat_x64", "b"),
    ("easyanticheat_x86", "b"),
    // winebth.sys crashes winedevice.exe — disable it (matches Proton)
    ("winebth.sys", "d"),
    // vrclient: SteamVR not needed — steam.exe asserts if it can't init VR
    ("vrclient_x64", "d"),
    // OpenCL: disabled for most games (Proton compat)
    ("opencl", "n,d"),
    // gameinput: crashes in some games (CW Bug 26376)
    ("gameinput", "d"),
];

// Adaptive deployment plan — built from PE scan result
struct DeployPlan {
    needs_dxvk: bool,
    needs_vkd3d: bool,
    needs_nvapi: bool,
    needs_dlss: bool,
    scan_hash: u64,
}

impl DeployPlan {
    fn from_scan(scan: Option<&crate::pe_scanner::PeScanResult>) -> Self {
        let Some(scan) = scan else {
            // No scan (non-game verb, missing exe) — deploy DXVK+VKD3D defensively
            return Self { needs_dxvk: true, needs_vkd3d: true, needs_nvapi: false, needs_dlss: false, scan_hash: 0 };
        };

        let is_nvidia = std::path::Path::new("/proc/driver/nvidia/version").exists();

        // Always deploy both DXVK and VKD3D-Proton. Many games discover D3D12
        // at runtime via DXGI (no direct d3d12.dll import), and launcher stubs
        // often have zero D3D imports. The translation layers are harmless when
        // unused — they only activate on actual API calls.
        let needs_dxvk = true;
        let needs_vkd3d = true;
        let needs_nvapi = is_nvidia && scan.needs_nvapi;
        let needs_dlss = is_nvidia && scan.imports.iter().any(|s| s.contains("nvngx"));

        // Hash for cache: if imports change, re-deploy
        use std::hash::{Hash, Hasher};
        use std::collections::hash_map::DefaultHasher;
        let mut h = DefaultHasher::new();
        scan.render_api.hash(&mut h);
        scan.imports.len().hash(&mut h);
        for imp in &scan.imports { imp.hash(&mut h); }
        let scan_hash = h.finish();

        Self { needs_dxvk, needs_vkd3d, needs_nvapi, needs_dlss, scan_hash }
    }
}

// Steam client files: (source_name in legacycompat/, dest_name in prefix)
const STEAM_CLIENT_FILES: &[(&str, &str)] = &[
    ("steamclient64.dll", "steamclient64.dll"),
    ("steamclient.dll", "steamclient.dll"),
    ("GameOverlayRenderer64.dll", "GameOverlayRenderer64.dll"),
    ("SteamService.exe", "steam.exe"),
    ("Steam.dll", "Steam.dll"),
];

const CACHE_FILE: &str = ".triskelion_deployed";

pub fn run(verb: &str, args: &[String]) -> i32 {
    ensure_stdio_fds();

    match verb {
        "getcompatpath" | "getnativepath" => {
            if let Some(path) = args.first() {
                println!("{path}");
            }
            return 0;
        }
        "installscript" | "runinprefix" => {
            log_verbose!("{verb}: no-op (Wine provides these APIs natively)");
            return 0;
        }
        _ => {}
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

    // DXVK/VKD3D source: check for actual DLL files (not just dirs) to avoid false positives
    let home = std::env::var("HOME").unwrap_or_default();
    let compat_tool_dir = PathBuf::from(&home)
        .join(".local/share/Steam/compatibilitytools.d/quark");
    let dxvk_src_dir = if compat_tool_dir.join("lib/wine/dxvk/x86_64-windows/d3d11.dll").exists() {
        compat_tool_dir
    } else if wine_dir.join("lib/wine/dxvk/x86_64-windows/d3d11.dll").exists() {
        wine_dir.clone()
    } else if let Some(ref proton) = proton_dir {
        if proton.join("lib/wine/dxvk/x86_64-windows/d3d11.dll").exists() {
            log_verbose!("DXVK/VKD3D: sourcing from Proton ({})", proton.display());
            proton.clone()
        } else {
            log_warn!("DXVK: no DLLs found — D3D games will use Wine's builtin (may crash on NVIDIA)");
            wine_dir.clone()
        }
    } else {
        log_warn!("DXVK/VKD3D: no source found — D3D games will use Wine's builtin");
        wine_dir.clone()
    };

    let compat_data = std::env::var("STEAM_COMPAT_DATA_PATH").unwrap_or_default();
    let steam_appid = std::env::var("SteamAppId").unwrap_or_default();
    log_verbose!("STEAM_COMPAT_DATA_PATH={compat_data} SteamAppId={steam_appid} verb={verb}");
    // Write to a file so we can always see it regardless of stderr redirection
    {
        let mut env_dump = format!("compat_data={compat_data}\nsteam_appid={steam_appid}\nverb={verb}\n\n");
        for (key, value) in std::env::vars() {
            env_dump.push_str(&format!("{key}={value}\n"));
        }
        let _ = std::fs::write("/tmp/quark/launcher_env.txt", env_dump);
    }
    if compat_data.is_empty() {
        log_error!("STEAM_COMPAT_DATA_PATH not set — not launched from Steam?");
        return 1;
    }
    // Steam calls "proton run" with compatdata/0 and empty SteamAppId as a
    // prefix probe before the real game launch. Starting a full wineserver for
    // this pollutes the socket directory. Skip it entirely.
    if (compat_data.ends_with("/0") || compat_data.ends_with("/0/")) && verb == "run" {
        log_verbose!("Skipping Steam prefix probe (compatdata/0, verb=run)");
        return 0;
    }
    let pfx = PathBuf::from(&compat_data).join("pfx");
    if let Err(e) = std::fs::create_dir_all(&pfx) {
        log_error!("Failed to create prefix directory {}: {e}", pfx.display());
        return 1;
    }

    // triskelion IS the wineserver
    let self_exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => {
            log_error!("Failed to determine own executable path: {e}");
            return 1;
        }
    };
    let t_discover = t_start.elapsed();

    log_verbose!("wine64: {}", wine64.display());
    log_verbose!("wineserver: {} (triskelion)", self_exe.display());
    log_verbose!("steam: {}", steam_dir.display());

    // Per-component cache — only redeploy what actually changed
    let cache = DeployCache::load(&pfx);
    let wine_hash = dir_hash(&wine_dir);
    let dxvk_hash = dir_hash(&dxvk_src_dir.join("lib/wine/dxvk"));
    let vkd3d_hash = dir_hash(&dxvk_src_dir.join("lib/wine/vkd3d-proton"));
    let steam_hash = dir_hash(&steam_dir.join("legacycompat"));

    let wine_valid = cache.as_ref().is_some_and(|c| c.wine_hash == wine_hash);
    let dxvk_valid = cache.as_ref().is_some_and(|c| c.dxvk_hash == dxvk_hash);
    let vkd3d_valid = cache.as_ref().is_some_and(|c| c.vkd3d_hash == vkd3d_hash);
    let steam_valid = cache.as_ref().is_some_and(|c| c.steam_hash == steam_hash);

    // Phase 2: Prefix setup (idempotent)
    let t2 = Instant::now();

    // Sync system Wine DLLs into prefix system32 BEFORE wineboot --init.
    // wineboot needs kernel32.dll et al to exist in the prefix. On a fresh
    // prefix this directory is empty, causing "could not load kernel32.dll".
    {
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
        let sys_ntdll = Path::new("/usr/lib/wine/x86_64-windows/ntdll.dll");
        let pfx_ntdll = sys32.join("ntdll.dll");
        if sys_ntdll.exists() {
            let sys_size = sys_ntdll.metadata().map(|m| m.len()).unwrap_or(0);
            let pfx_size = pfx_ntdll.metadata().map(|m| m.len()).unwrap_or(0);
            if sys_size != pfx_size {
                log_verbose!("Syncing system Wine DLLs to prefix...");
                let src_dir = Path::new("/usr/lib/wine/x86_64-windows");
                let mut copied = 0u32;
                if let Ok(entries) = std::fs::read_dir(src_dir) {
                    for entry in entries.flatten() {
                        let path = entry.path();
                        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
                        if ext == "dll" || ext == "exe" || ext == "drv" || ext == "sys" || ext == "ocx" || ext == "cpl" || ext == "acm" || ext == "ax" {
                            let target = sys32.join(entry.file_name());
                            let _ = std::fs::copy(&path, &target);
                            copied += 1;
                        }
                    }
                }
                log_verbose!("Synced {copied} PE files to prefix system32");
            }
        }
    }

    // Ensure pfx dir and stub .reg files exist BEFORE wineboot/daemon starts.
    // The daemon loads .reg files at startup — if we inject keys after that,
    // the daemon never sees them. So: create stubs → inject keys → wineboot.
    let _ = std::fs::create_dir_all(&pfx);
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

    // Inject registry keys BEFORE wineboot starts the daemon.
    // The daemon loads .reg files once at startup — keys must be present by then.
    inject_registry_keys(&pfx);

    // Deploy steam.exe and lsteamclient.dll BEFORE wineboot runs.
    // Wineboot's entry command is steam.exe — it must exist in system32 first.
    deploy_steam_exe(&self_exe, &pfx);
    deploy_lsteamclient_to_prefix(&self_exe, &pfx);

    if !wine_valid {
        setup_prefix(&wine_dir, &pfx, &wine64, &self_exe);
    }
    // Re-deploy after setup_prefix (wineboot may overwrite system32)
    deploy_steam_exe(&self_exe, &pfx);
    deploy_lsteamclient_to_prefix(&self_exe, &pfx);

    // Always ensure user profile directories exist — cheap no-op if already present.
    // Must be unconditional: setup_prefix is gated by cache but games always need these.
    {
        let username = std::env::var("USER").unwrap_or_else(|_| "user".to_string());
        let user_dir = pfx.join("drive_c/users").join(&username);
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
    let t_prefix = t2.elapsed();

    // PE scan: determine what this game needs BEFORE deploying DLLs
    // Some games (UE5) use a launcher stub that imports nothing — the real
    // binary is in Binaries/Win64/. Scan both and merge results.
    let game_exe_for_scan: Option<PathBuf> = if verb == "waitforexitandrun" || verb == "run" {
        args.first().map(|s| PathBuf::from(s))
    } else { None };
    let scan_result = game_exe_for_scan.as_ref().and_then(|exe| {
        let p = Path::new(exe);
        let primary = if p.exists() { crate::pe_scanner::scan_pe(p) } else { None };
        // If primary has no D3D imports, look for UE-style shipping binary
        let dominated = primary.as_ref().is_some_and(|s| s.imports.is_empty()
            || !s.imports.iter().any(|i| i.contains("d3d")));
        if dominated {
            if let Some(parent) = p.parent() {
                // UE pattern: <Game>/<Project>/Binaries/Win64/<Project>-Win64-Shipping.exe
                let stem = p.file_stem().and_then(|s| s.to_str()).unwrap_or("");
                let ue_bin = parent.join(stem).join("Binaries/Win64");
                if ue_bin.exists() {
                    let pattern = format!("{stem}-Win64-Shipping.exe");
                    if let Ok(rd) = std::fs::read_dir(&ue_bin) {
                        for entry in rd.flatten() {
                            if entry.file_name().to_str().is_some_and(|n| n.eq_ignore_ascii_case(&pattern)) {
                                if let Some(child_scan) = crate::pe_scanner::scan_pe(&entry.path()) {
                                    log_verbose!("PE scan: using UE child {}", entry.path().display());
                                    return Some(child_scan);
                                }
                            }
                        }
                    }
                }
            }
        }
        primary
    });
    let plan = DeployPlan::from_scan(scan_result.as_ref());
    let scan_valid = cache.as_ref().is_some_and(|c| c.scan_hash == plan.scan_hash);

    if let Some(ref scan) = scan_result {
        log_verbose!("PE scan: {:?} | dxvk={} vkd3d={} nvapi={} dlss={} steam_api={} imports={}",
            scan.render_api, plan.needs_dxvk, plan.needs_vkd3d,
            plan.needs_nvapi, plan.needs_dlss, scan.needs_steam_api, scan.imports.len());
    }

    // Phase 3: Deploy DLLs based on PE scan (per-component, scan-driven)
    let t3 = Instant::now();
    let dxvk_deployed: Vec<&str> = if plan.needs_dxvk {
        if dxvk_valid && scan_valid { DXVK_DLLS.to_vec() } else { deploy_dxvk(&dxvk_src_dir, &pfx) }
    } else { vec![] };
    let vkd3d_deployed: Vec<&str> = if plan.needs_vkd3d {
        if vkd3d_valid && scan_valid { VKD3D_DLLS.to_vec() } else { deploy_vkd3d(&dxvk_src_dir, &pfx) }
    } else { vec![] };
    let nvapi_deployed: Vec<&str> = if plan.needs_nvapi {
        if let Some(ref proton) = proton_dir { deploy_nvapi(proton, &pfx) } else { vec![] }
    } else { vec![] };
    let dlss_deployed: Vec<&str> = if plan.needs_dlss { deploy_dlss(&pfx) } else { vec![] };
    let t_dxvk = t3.elapsed();

    // Phase 4: Deploy Steam client DLLs (per-component)
    let t4 = Instant::now();
    if !steam_valid {
        deploy_steam_client(&steam_dir, &pfx);
    }
    // Deploy our lsteamclient as steamclient64.dll (replaces broken legacycompat version)
    repair_steamclient64(&pfx);
    // Ensure our steam_bridge exists in prefix system32 (overwrites Proton's).
    deploy_steam_exe(&self_exe, &pfx);
    // lsteamclient.dll must exist in system32 so build_module's LdrLoadDll can find it.
    // Without this, the steamclient trampoline (patch 009) never activates.
    deploy_lsteamclient_to_prefix(&self_exe, &pfx);
    let t_steam = t4.elapsed();

    // Registry keys already injected before wineboot (daemon loads them at startup)

    // Save cache if anything changed
    let all_valid = wine_valid && dxvk_valid && vkd3d_valid && steam_valid && scan_valid;
    if !all_valid {
        DeployCache { wine_hash, dxvk_hash, vkd3d_hash, steam_hash, scan_hash: plan.scan_hash }.save(&pfx);
        log_verbose!("deployment cache written");
    } else {
        log_verbose!("cache hit — skipped all file ops");
    }

    // Phase 5: Build environment
    let trace = std::env::var("QUARK_TRACE_OPCODES").is_ok()
        || Path::new("/tmp/quark/TRACE_OPCODES").exists();

    let shader_cache_enabled = self_exe.parent()
        .map(|dir| dir.join("shader_cache_enabled").exists())
        .unwrap_or(false);

    let env_vars = build_env_vars(
        &wine_dir, proton_dir.as_deref(), &steam_dir, &pfx, &self_exe,
        &dxvk_deployed, &vkd3d_deployed, &nvapi_deployed, &dlss_deployed,
        shader_cache_enabled,
    );
    let custom_env = parse_env_config(&self_exe);

    // Apply custom env vars to daemon's own env so EventLoop reads them at startup.
    // The daemon and launcher are the same process.
    // Safety: single-threaded at this point in launcher init.
    for (k, v) in &custom_env {
        unsafe { std::env::set_var(k, v); }
    }

    // Resolve WINEDEBUG once. Priority:
    // 1. env_config / shell env (user's explicit override)
    // 2. QUARK_TRACE_OPCODES → "+server,+timestamp"
    // 3. --verbose → "+process,+seh,err"
    // 4. default → "-all"
    let winedebug = resolve_winedebug(trace);

    let t_total_setup = t_start.elapsed();

    // Write timing + diagnostics (verbose only)
    if triskelion::log::is_verbose() {
        log_verbose!("timing: discover={}ms prefix={}ms dxvk={}ms steam={}ms total={}ms",
            t_discover.as_millis(), t_prefix.as_millis(),
            t_dxvk.as_millis(), t_steam.as_millis(), t_total_setup.as_millis());

    }

    // Phase 5b: Sync prefix with env config (display driver changes etc.)
    // Wine re-runs wineboot internally when it detects config changes (e.g. X11 → Wayland),
    // which consumes the entire game launch session. Pre-sync the prefix here so Wine
    // doesn't need to re-init at game time. Uses a hash file to skip when nothing changed.
    // Skip wineboot --update for now — service processes hang on init.
    // The prefix is already set up from stock Wine's initial wineboot.
    // TODO: Re-enable once named pipe I/O and completion ports work.
    // sync_prefix_env(&wine64, &pfx, &self_exe, &wine_dir, &env_vars, &custom_env);

    // Phase 6: Launch
    // Clean stale wineserver sockets and shared memory from previous runs.
    // Wine connects to any existing socket for the prefix's inode.
    // If a stale socket exists from a crashed/killed server, Wine hangs
    // trying to connect to a dead daemon.
    {
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
        // Clean stale triskelion shared memory segments
        if let Ok(entries) = std::fs::read_dir("/dev/shm") {
            for entry in entries.flatten() {
                let name = entry.file_name();
                if name.to_string_lossy().starts_with("triskelion-") {
                    log_verbose!("Removing stale shm: {}", entry.path().display());
                    let _ = std::fs::remove_file(entry.path());
                }
            }
        }
    }

    if verb == "waitforexitandrun" || verb == "run" {
        let game_exe = match args.first() {
            Some(exe) => exe,
            None => {
                log_error!("No executable specified");
                return 1;
            }
        };

        // Steam runs utility exes that don't need a full Wine session:
        // - iscriptevaluator.exe: DirectX/vcredist install scripts (Wine provides these)
        // - d3ddriverquery64.exe: GPU capability probe (fails without full display driver)
        // Skip them — returning 0 tells Steam the compat tool is healthy.
        let dominated_exes = ["iscriptevaluator", "d3ddriverquery"];
        if dominated_exes.iter().any(|pat| game_exe.to_ascii_lowercase().contains(pat)) {
            log_verbose!("skipping Steam utility: {game_exe}");
            return 0;
        }

        // Sanity check: does the game exe exist?
        let game_path = Path::new(game_exe);
        if !game_path.exists() {
            log_warn!("game exe not found at {game_exe} (may be a Windows path — continuing)");
        }

        log_info!("launching: {game_exe}");

        // Pre-launch validation: verify deployed DLLs exist in prefix
        if let Some(ref scan) = scan_result {
            let sys32 = pfx.join("drive_c/windows/system32");
            let mut fatal: Vec<String> = Vec::new();
            if plan.needs_dxvk && !sys32.join("d3d11.dll").exists() {
                fatal.push("d3d11.dll (DXVK)".into());
            }
            if plan.needs_vkd3d && !sys32.join("d3d12.dll").exists() {
                fatal.push("d3d12.dll (VKD3D-Proton)".into());
            }
            if !fatal.is_empty() {
                for dll in &fatal {
                    log_error!("FATAL: {dll} not deployed — game needs {:?}", scan.render_api);
                }
                log_error!("Check DXVK/VKD3D source dirs. Game will crash without these.");
            }
        }

        // Clean stale crash dumps — engines like Godot read their crash dir
        // on startup and may enter recovery paths if old dumps exist.
        clean_crash_dumps(&pfx);

        // Save data protection: snapshot before launch
        let save_backup = snapshot_save_data(&pfx);

        // Launch through steam.exe (Proton's steam_helper). steam.exe:
        // 1. Creates Win32 events games check (Steam3Master_SharedMemLock, etc.)
        // 2. Calls steamclient_init_registry() — dlopen's native steamclient.so,
        //    CreateSteamPipe()/ConnectToGlobalUser() to connect to running Steam
        // 3. Creates the "Steam" window (FindWindow detection)
        // 4. Spawns the game as a child via CreateProcess()
        // Without this, SteamAPI_IsSteamRunning() fails.

        let mut cmd = build_wine_command(&wine64, &wine_dir);
        cmd.arg("C:\\windows\\system32\\steam.exe");
        cmd.arg(game_exe);
        cmd.args(&args[1..]);
        if let Some(parent) = game_path.parent() {
            if parent.exists() { cmd.current_dir(parent); }
        }
        for (k, v) in &env_vars {
            cmd.env(k, v);
        }
        for (k, v) in &custom_env {
            cmd.env(k, v);
        }
        cmd.env("WINEPREFIX", &pfx);
        cmd.env("WINESERVER", triskelion_exe());
        cmd.env("WINE_NTSYNC", "1");
        // Hint game exe size to triskelion for adaptive linger timer
        if let Ok(meta) = std::fs::metadata(game_path) {
            cmd.env("QUARK_LINGER_HINT", meta.len().to_string());
        }
        let steam_app_id = std::env::var("SteamGameId")
            .or_else(|_| std::env::var("SteamAppId"))
            .or_else(|_| std::env::var("STEAM_COMPAT_APP_ID"))
            .unwrap_or_default();
        cmd.env("SteamAppId", &steam_app_id);
        cmd.env("SteamGameId", &steam_app_id);
        cmd.env("STEAM_COMPAT_CLIENT_INSTALL_PATH",
            std::env::var("STEAM_COMPAT_CLIENT_INSTALL_PATH")
                .unwrap_or_else(|_| format!("{}/.local/share/Steam", std::env::var("HOME").unwrap_or_default())));

        use std::os::unix::process::CommandExt;
        unsafe { cmd.pre_exec(|| { libc::setsid(); Ok(()) }); }
        unsafe {
            libc::signal(libc::SIGINT, libc::SIG_IGN);
            libc::signal(libc::SIGTERM, libc::SIG_IGN);
        }
        cmd.env("WINEDEBUG", &winedebug);

        // WINEDLLPATH: quark-specific modules (lsteamclient.so, steam.exe) live
        // in our tree, not system Wine. Without this, LoadLibraryW("lsteamclient")
        // creates an empty stub and Steam auth fails.
        {
            let quark_unix = wine_dir.join("lib/wine/x86_64-unix");
            let quark_win = wine_dir.join("lib/wine/x86_64-windows");
            let sys_unix = Path::new("/usr/lib/wine/x86_64-unix");
            let sys_win = Path::new("/usr/lib/wine/x86_64-windows");
            let mut dll_parts: Vec<String> = Vec::new();
            if quark_unix.exists() { dll_parts.push(quark_unix.display().to_string()); }
            if quark_win.exists() { dll_parts.push(quark_win.display().to_string()); }
            if sys_unix.exists() { dll_parts.push(sys_unix.display().to_string()); }
            if sys_win.exists() { dll_parts.push(sys_win.display().to_string()); }
            if !dll_parts.is_empty() {
                cmd.env("WINEDLLPATH", dll_parts.join(":"));
            }
        }

        // LD_LIBRARY_PATH: Wine .so modules need ntdll.so via NEEDED
        {
            let unix_dir = wine_dir.join("lib/wine/x86_64-unix");
            let lib_dir = wine_dir.join("lib");
            let mut ld_parts: Vec<String> = Vec::new();
            if unix_dir.exists() { ld_parts.push(unix_dir.display().to_string()); }
            if lib_dir.exists() { ld_parts.push(lib_dir.display().to_string()); }
            if let Ok(existing) = std::env::var("LD_LIBRARY_PATH") {
                ld_parts.push(existing);
            }
            if !ld_parts.is_empty() {
                cmd.env("LD_LIBRARY_PATH", ld_parts.join(":"));
            }
        }

        // Strip Steam overlay from LD_PRELOAD — it hooks SDL/GL calls and
        // crashes before the Wayland surface is configured.
        {
            let existing = std::env::var("LD_PRELOAD").unwrap_or_default();
            let filtered: Vec<&str> = existing.split(':')
                .filter(|p| !p.is_empty() && !p.contains("gameoverlayrenderer"))
                .collect();
            let preload = filtered.join(":");
            if preload.is_empty() {
                cmd.env_remove("LD_PRELOAD");
            } else {
                cmd.env("LD_PRELOAD", &preload);
            }
        }

        let _ = std::fs::create_dir_all("/tmp/quark");
        let stderr_log_path = {
            let path = PathBuf::from("/tmp/quark/wine_stderr.log");
            match std::fs::File::create(&path) {
                Ok(f) => {
                    cmd.stderr(std::process::Stdio::from(f));
                    if let Ok(f2) = std::fs::File::create("/tmp/quark/wine_stdout.log") {
                        cmd.stdout(std::process::Stdio::from(f2));
                    }
                    Some(path)
                }
                Err(_) => None,
            }
        };

        // Spawn PARALLAX before wine64 so triskelion can read hardware data at startup.
        // PARALLAX writes GPU/connector/mode info to /parallax-<hash> shared memory.
        // It exits after writing — not a long-running process.
        let parallax_bin = parallax_exe();
        if parallax_bin.exists() {
            let mut pcmd = Command::new(&parallax_bin);
            pcmd.env("WINEPREFIX", &pfx);
            pcmd.stdout(std::process::Stdio::null());
            pcmd.stderr(std::process::Stdio::null());
            match pcmd.spawn() {
                Ok(mut pc) => {
                    // PARALLAX enumerates DRM/KMS, writes SHM, exits.
                    // Wait for it so triskelion is guaranteed a warm SHM.
                    match pc.wait() {
                        Ok(s) if s.success() => log_verbose!("PARALLAX: enumerated display hardware"),
                        Ok(s) => log_warn!("PARALLAX: exited with {s}"),
                        Err(e) => log_warn!("PARALLAX: wait failed: {e}"),
                    }
                }
                Err(e) => log_warn!("PARALLAX: failed to start: {e}"),
            }
        }

        let mut child = cmd.spawn().unwrap_or_else(|e| {
            log_error!("Failed to exec wine64: {e}");
            std::process::exit(1);
        });

        // No explorer.exe. Triskelion pre-creates the desktop window, pre-populates
        // __wine_display_device_guid and GraphicsDriver registry keys at startup.
        // display driver (winewayland.drv) loads directly without the WM_NULL gate.
        // desktop_ready is set by the daemon at init_first_thread.

        // Now wait for the game to exit
        let child_pid = child.id() as i32;
        let status = child.wait().unwrap_or_else(|e| {
            log_error!("Failed to wait for wine64: {e}");
            std::process::exit(1);
        });
        // Kill the entire session spawned by wine64 (setsid at line 596).
        // Without this, orphaned Wine children (services.exe, game .exe)
        // survive after triskelion exits and spin at 100%+ CPU.
        unsafe { libc::kill(-child_pid, libc::SIGKILL); }

        // Update stderr symlink and log exit status
        if let Some(ref log_path) = stderr_log_path {
            let log_dir = triskelion::log::log_dir();
            let link = log_dir.join("stderr-latest.log");
            let _ = std::fs::remove_file(&link);
            if let Some(fname) = log_path.file_name() {
                let _ = std::os::unix::fs::symlink(fname, &link);
            }

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

            // Append exit status to the stderr log
            if let Ok(mut f) = std::fs::OpenOptions::new().append(true).open(log_path) {
                use std::io::Write;
                let _ = writeln!(f, "\n[quark] {exit_info}");
            }
            log_verbose!("stderr log: {}", log_path.display());
        }

        // Save data protection: restore any files that went missing
        if let Some(ref backup_dir) = save_backup {
            restore_save_data(&pfx, backup_dir);
        }

        if verb == "waitforexitandrun" {
            std::thread::sleep(std::time::Duration::from_secs(2));
        }

        return match status.code() {
            Some(code) => code,
            None => {
                use std::os::unix::process::ExitStatusExt;
                if let Some(sig) = status.signal() {
                    log_warn!("wine64 killed by signal {sig}");
                }
                1
            }
        };
    }

    log_error!("Unknown verb: {verb}");
    1
}

// ---------------------------------------------------------------------------
// Deployment cache
// ---------------------------------------------------------------------------

struct DeployCache {
    wine_hash: u64,
    dxvk_hash: u64,
    vkd3d_hash: u64,
    steam_hash: u64,
    scan_hash: u64,
}

impl DeployCache {
    fn load(pfx: &Path) -> Option<Self> {
        let data = std::fs::read_to_string(pfx.join(CACHE_FILE)).ok()?;
        let parts: Vec<&str> = data.strip_prefix("v4:")?.trim().split(',').collect();
        if parts.len() != 5 { return None; }
        Some(DeployCache {
            wine_hash: parts[0].parse().ok()?,
            dxvk_hash: parts[1].parse().ok()?,
            vkd3d_hash: parts[2].parse().ok()?,
            steam_hash: parts[3].parse().ok()?,
            scan_hash: parts[4].parse().ok()?,
        })
    }

    fn save(&self, pfx: &Path) {
        let data = format!("v4:{},{},{},{},{}", self.wine_hash, self.dxvk_hash,
            self.vkd3d_hash, self.steam_hash, self.scan_hash);
        if let Err(e) = std::fs::write(pfx.join(CACHE_FILE), data) {
            log_warn!("Cannot write deployment cache: {e} — next launch will re-deploy");
        }
    }
}

/// Quick hash of a directory's metadata: combine dev+ino+mtime of the dir entry.
/// Detects directory replacement (new inode) or direct modification (mtime change).
fn dir_hash(path: &Path) -> u64 {
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
    // Cache invalidation drives re-setup. copy_dir_fast skips existing files,
    // so re-running is safe and adds anything missing (e.g. symlinks from a buggy deploy).
    let default_pfx = wine_dir.join("share").join("default_pfx");
    if default_pfx.exists() {
        let fresh = !pfx.join("system.reg").exists();
        if fresh {
            log_verbose!("Setting up prefix from template...");
        } else {
            log_verbose!("Repairing prefix from template...");
        }
        let count = copy_dir_fast(&default_pfx, pfx);
        if count > 0 {
            log_verbose!("Prefix: {count} files deployed");
        }

        // dosdevices symlinks
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

        // Prevent Wine from re-updating the prefix on every launch
        if let Ok(inf_meta) = std::fs::metadata(wine_dir.join("share/wine/wine.inf")) {
            if let Ok(mtime) = inf_meta.modified() {
                let ts_file = pfx.join(".update-timestamp");
                if let Ok(epoch) = mtime.duration_since(std::time::UNIX_EPOCH) {
                    if let Err(e) = std::fs::write(&ts_file, epoch.as_secs().to_string()) {
                        log_warn!("Cannot write .update-timestamp: {e} — Wine may reinit prefix each launch");
                    }
                }
            }
        }
    } else {
        // No template available (system Wine) — fall back to wineboot.
        // Create user profile dirs FIRST — wineboot's SHGetFolderPath needs them.
        let username = std::env::var("USER").unwrap_or_else(|_| "user".to_string());
        let user_dir = pfx.join("drive_c/users").join(&username);
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
        // dosdevices
        let dosdevices = pfx.join("dosdevices");
        let _ = std::fs::create_dir_all(&dosdevices);
        let c_link = dosdevices.join("c:");
        let z_link = dosdevices.join("z:");
        if !c_link.exists() {
            let _ = std::os::unix::fs::symlink("../drive_c", &c_link);
        }
        if !z_link.exists() {
            let _ = std::os::unix::fs::symlink("/", &z_link);
        }
        // Windows system dirs
        let _ = std::fs::create_dir_all(pfx.join("drive_c/windows/system32"));
        let _ = std::fs::create_dir_all(pfx.join("drive_c/windows/syswow64"));
        let _ = std::fs::create_dir_all(pfx.join("drive_c/Program Files"));
        let _ = std::fs::create_dir_all(pfx.join("drive_c/Program Files (x86)"));

        // Copy quark's baked prefix template .reg files (COM registrations).
        // These contain 500+ CLSID entries from Wine's FakeDlls pass, baked
        // at install time by install.py's step_bake_prefix_template.
        let quark_template = self_exe.parent().map(|d| d.join("default_pfx"));
        if let Some(ref tpl) = quark_template {
            if tpl.join("system.reg").exists() {
                for reg in ["system.reg", "user.reg", "userdef.reg"] {
                    let src = tpl.join(reg);
                    let dst = pfx.join(reg);
                    if src.exists() && !dst.exists() {
                        let _ = std::fs::copy(&src, &dst);
                    }
                }
                log_verbose!("Prefix: loaded baked registry template from {}", tpl.display());
            }
        }

        // Skip wineboot --init if prefix has a FULL init (Fonts dir exists).
        // Our stub system.reg doesn't count — it's just for service injection.
        let prefix_fully_initialized = pfx.join("drive_c/windows/Fonts").exists();
        if prefix_fully_initialized {
            log_verbose!("Prefix fully initialized (Fonts dir exists), skipping wineboot --init");
        } else {
        log_verbose!("No default_pfx template, running wineboot --init...");
        let mut cmd = build_wine_command(wine64, wine_dir);
        cmd.args(["wineboot", "--init"]);
        cmd.env("WINEPREFIX", pfx.as_os_str());
        cmd.env("WINESERVER", triskelion_exe());
        cmd.env("QUARK_FAST_BOOT", "1");
        // Unix .so modules (ws2_32.so etc.) need ntdll.so via NEEDED — set LD_LIBRARY_PATH
        let unix_dir = wine_dir.join("lib/wine/x86_64-unix");
        let wine_lib = wine_dir.join("lib");
        let mut ld_path = String::new();
        if unix_dir.exists() {
            ld_path.push_str(&unix_dir.display().to_string());
        }
        if wine_lib.exists() {
            if !ld_path.is_empty() { ld_path.push(':'); }
            ld_path.push_str(&wine_lib.display().to_string());
        }
        if let Ok(existing) = std::env::var("LD_LIBRARY_PATH") {
            if !ld_path.is_empty() { ld_path.push(':'); }
            ld_path.push_str(&existing);
        }
        if !ld_path.is_empty() {
            cmd.env("LD_LIBRARY_PATH", &ld_path);
        }
        // Set WINELOADER so triskelion daemon can find ntdll.so for protocol detection.
        cmd.env("WINELOADER", wine64.as_os_str());
        // WINEDLLPATH: Wine needs this to find builtin PE DLLs (winex11.drv, lsteamclient, etc.)
        // Proton override dir has lsteamclient.dll; main wine dir has system DLLs.
        let wine_dll = wine_dir.join("lib/wine");
        let proton_override = wine_dir.join("lib/wine/proton");
        let dll_path = if proton_override.exists() {
            format!("{}:{}", proton_override.display(), wine_dll.display())
        } else {
            wine_dll.display().to_string()
        };
        cmd.env("WINEDLLPATH", &dll_path);
        let dll_dir = format!("\\??\\Z:{}", wine_dll.display());
        cmd.env("WINEDLLDIR0", &dll_dir);
        cmd.env("WINEDEBUG", resolve_winedebug(false));
        // No fsync/esync — system Wine uses ntsync inproc sync.
        cmd.env("WINEESYNC", "0");
        // Preload assertion suppressor: converts assert() failures in Wine's
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
        {
            let existing = std::env::var("LD_PRELOAD").unwrap_or_default();
            let filtered: Vec<&str> = existing.split(':')
                .filter(|p| !p.is_empty() && !p.contains("gameoverlayrenderer"))
                .collect();
            let mut preload = filtered.join(":");
            if noassert.exists() {
                if !preload.is_empty() { preload.push(':'); }
                preload.push_str(&noassert.display().to_string());
            }
            if preload.is_empty() {
                cmd.env_remove("LD_PRELOAD");
            } else {
                cmd.env("LD_PRELOAD", &preload);
            }
        }
        match cmd.status() {
            Ok(s) if !s.success() => log_error!("wineboot --init failed with exit code {}", s.code().unwrap_or(-1)),
            Err(e) => log_error!("Failed to run wineboot: {e}"),
            _ => {}
        }
        } // end of system.reg check else block
    }

    // Prevent Wine's internal wineboot from re-running wine.inf processing.
    // Our setup_prefix handles prefix init; the Wine-level update spawns
    // multiple rundll32 passes (PreInstall + DefaultInstall) that can take
    // minutes and block the game from loading.
    let ts_file = pfx.join(".update-timestamp");
    let _ = std::fs::write(&ts_file, "disable\n");
}

/// Inject registry keys that Proton normally installs via redistributable packages.
/// The DLLs are already in the prefix (from template symlinks), but some games
/// check registry keys to verify installation before even trying to load them.
fn inject_registry_keys(pfx: &Path) {
    use std::io::Write;
    let sys_reg = pfx.join("system.reg");
    let user_reg = pfx.join("user.reg");

    // ── Template-based registry: copy from stock Wine prefix ──
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
    // daemon may have saved updated registry state from a previous session.
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

    // Display driver (winewayland.drv) is set dynamically by triskelion's
    // init_runtime_keys at startup — no need to force it on disk.

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
}

/// Fast recursive directory copy using getdents64 + hardlinks.
/// getdents64: one syscall fills a 32KB buffer with hundreds of entries.
/// Hardlinks: near-instant, share disk blocks.
/// Falls back to copy on cross-device.
/// Returns number of files deployed.
fn copy_dir_fast(src: &Path, dst: &Path) -> u32 {
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
            // linux_dirent64: d_ino(8) + d_off(8) + d_reclen(2) + d_type(1) + d_name(...)
            let d_reclen = u16::from_ne_bytes([buf[pos + 16], buf[pos + 17]]) as usize;
            let d_type = buf[pos + 18];

            // Extract null-terminated name
            let name_start = pos + 19;
            let name_end = buf[name_start..pos + d_reclen]
                .iter()
                .position(|&b| b == 0)
                .map(|i| name_start + i)
                .unwrap_or(pos + d_reclen);
            let name = &buf[name_start..name_end];

            // Skip . and ..
            if name == b"." || name == b".." {
                pos += d_reclen;
                continue;
            }

            let name_os = std::ffi::OsStr::from_bytes(name);
            let src_path = src.join(name_os);
            let dst_path = dst.join(name_os);

            // DT_UNKNOWN=0, DT_DIR=4, DT_REG=8, DT_LNK=10
            // Some filesystems (NFS, FUSE) report DT_UNKNOWN — fall back to stat
            let d_type = if d_type == 0 {
                match src_path.symlink_metadata() {
                    Ok(m) if m.is_dir() => 4,
                    Ok(m) if m.file_type().is_symlink() => 10,
                    Ok(_) => 8, // regular or other → treat as regular
                    Err(e) => {
                        log_warn!("copy_dir_fast: cannot stat {}: {e}", src_path.display());
                        pos += d_reclen; continue;
                    }
                }
            } else {
                d_type
            };

            if d_type == 4 {
                count += copy_dir_fast(&src_path, &dst_path);
            } else if d_type == 10 {
                // Symlink: template uses relative paths (../../../../../lib/wine/...)
                // that only resolve inside Proton's tree. We must resolve against the
                // source and create absolute symlinks to the real files.
                let needs_fix = if let Ok(meta) = dst_path.symlink_metadata() {
                    if meta.file_type().is_symlink() {
                        // Already a symlink — fix only if broken (target missing)
                        !dst_path.exists()
                    } else {
                        true // regular file where symlink should be
                    }
                } else {
                    true // doesn't exist at all
                };
                if needs_fix {
                    if let Err(e) = std::fs::remove_file(&dst_path) {
                        if e.kind() != std::io::ErrorKind::NotFound {
                            log_warn!("Cannot remove {}: {e}", dst_path.display());
                        }
                    }
                    // Canonicalize resolves the relative symlink in the source tree
                    // to an absolute path pointing at the real file in Proton
                    if let Ok(real_target) = std::fs::canonicalize(&src_path) {
                        if let Err(e) = std::os::unix::fs::symlink(&real_target, &dst_path) {
                            log_warn!("Symlink failed {}: {e}", dst_path.display());
                        }
                    } else if let Ok(target) = std::fs::read_link(&src_path) {
                        // Dead symlink in template — copy as-is
                        if let Err(e) = std::os::unix::fs::symlink(&target, &dst_path) {
                            log_warn!("Symlink failed {}: {e}", dst_path.display());
                        }
                    }
                    count += 1;
                }
            } else if !dst_path.exists() {
                // Regular file: hardlink first (instant), copy fallback
                if std::fs::hard_link(&src_path, &dst_path).is_ok() {
                    count += 1;
                } else if std::fs::copy(&src_path, &dst_path).is_ok() {
                    count += 1;
                } else {
                    log_warn!("copy_dir_fast: failed to deploy {}", src_path.display());
                }
            }

            pos += d_reclen;
        }
    }

    unsafe { libc::close(fd) };
    count
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

fn deploy_nvapi(proton_dir: &Path, pfx: &Path) -> Vec<&'static str> {
    let src64 = proton_dir.join("lib/wine/nvapi/x86_64-windows");
    let sys32 = pfx.join("drive_c/windows/system32");
    deploy_dlls(&src64, &sys32, NVAPI_DLLS, "NVAPI")
}

fn deploy_dlss(pfx: &Path) -> Vec<&'static str> {
    let src = PathBuf::from("/usr/lib/nvidia/wine");
    let sys32 = pfx.join("drive_c/windows/system32");
    deploy_dlls(&src, &sys32, DLSS_DLLS, "DLSS")
}

fn deploy_dlls(
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
fn file_matches(src: &Path, dst: &Path) -> bool {
    let Ok(src_meta) = std::fs::metadata(src) else { return false };
    let Ok(dst_meta) = std::fs::metadata(dst) else { return false };
    src_meta.len() == dst_meta.len()
        && src_meta.modified().ok() <= dst_meta.modified().ok()
}

// ---------------------------------------------------------------------------
// Steam client integration (conditional)
// ---------------------------------------------------------------------------

fn deploy_steam_client(steam_dir: &Path, pfx: &Path) {
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

/// Repair steamclient64.dll in prefixes corrupted by the old deploy_steam_bridge.
/// The trampoline (patch 009) needs Valve's REAL native steamclient64.dll intact.
/// Old code overwrote it with our lsteamclient.dll — detect and fix.
fn repair_steamclient64(pfx: &Path) {
    let real = PathBuf::from(
        std::env::var("STEAM_COMPAT_CLIENT_INSTALL_PATH")
            .unwrap_or_else(|_| format!("{}/.local/share/Steam", std::env::var("HOME").unwrap_or_default()))
    ).join("steamclient64.dll");
    if !real.exists() { return; }
    let real_size = real.metadata().map(|m| m.len()).unwrap_or(0);
    if real_size == 0 { return; }

    for loc in ["drive_c/Program Files (x86)/Steam", "drive_c/windows/system32"] {
        let dst = pfx.join(loc).join("steamclient64.dll");
        if let Ok(meta) = dst.metadata() {
            if meta.len() != real_size {
                if let Err(e) = std::fs::copy(&real, &dst) {
                    log_warn!("repair_steamclient64: failed {loc}: {e}");
                }
            }
        }
    }
}

/// Ensure steam.exe exists in the prefix's system32 and syswow64.
/// Sources: cached copy (~/.local/share/quark/steam.exe) → Proton (fallback).
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
    if let Err(e) = std::fs::copy(src, &dst) {
        log_warn!("lsteamclient.dll: failed to deploy to system32: {e}");
    }
}

fn build_env_vars(
    wine_dir: &Path, proton_dir: Option<&Path>, steam_dir: &Path,
    pfx: &Path, self_exe: &Path,
    dxvk: &[&str], vkd3d: &[&str], nvapi: &[&str], dlss: &[&str],
    shader_cache_enabled: bool,
) -> Vec<(&'static str, String)> {
    let cur_path = std::env::var("PATH").unwrap_or_default();
    let cur_ld = std::env::var("LD_LIBRARY_PATH").unwrap_or_default();

    let wine_bin = wine_dir.join("bin");
    let wine_lib = wine_dir.join("lib");
    let wine_dll = wine_dir.join("lib/wine");
    let wine_vkd3d = wine_dir.join("lib/vkd3d");
    let steam_linux64 = steam_dir.join("linux64");

    // WINEDLLPATH: proton bridge DLLs → quark libs → vkd3d → wine DLLs → Proton DLLs
    let mut dll_parts: Vec<String> = Vec::new();
    // Proton bridge DLLs (lsteamclient, steam.exe) — deployed by install.py
    let home = std::env::var("HOME").unwrap_or_default();
    let proton_override = PathBuf::from(&home)
        .join(".local/share/Steam/compatibilitytools.d/quark/lib/wine/proton");
    if proton_override.exists() {
        dll_parts.push(proton_override.display().to_string());
    }
    // Our builds (lsteamclient, steam.exe) take priority over stock Proton DLLs
    let quark_wine_lib = self_exe.parent().map(|d| d.join("lib/wine"));
    if let Some(ref wlib) = quark_wine_lib {
        if wlib.exists() {
            dll_parts.push(wlib.display().to_string());
        }
    }
    if wine_vkd3d.exists() {
        dll_parts.push(wine_vkd3d.display().to_string());
    }
    dll_parts.push(wine_dll.display().to_string());
    if let Some(proton) = proton_dir {
        let proton_vkd3d = proton.join("lib/vkd3d");
        let proton_dll = proton.join("lib/wine");
        if proton_vkd3d.exists() {
            dll_parts.push(proton_vkd3d.display().to_string());
        }
        if proton_dll.exists() && proton_dll != wine_dll {
            dll_parts.push(proton_dll.display().to_string());
        }
    }
    let dll_path = dll_parts.join(":");

    // LD_LIBRARY_PATH: Proton native libs + Steam client + Wine lib
    let mut ld_parts: Vec<String> = Vec::new();
    // Proton's native libs (x86_64-linux-gnu) for runtime support
    if let Some(proton) = proton_dir {
        let proton_native = proton.join("lib/x86_64-linux-gnu");
        if proton_native.exists() {
            ld_parts.push(proton_native.display().to_string());
        }
    }
    let wine_native = wine_dir.join("lib/x86_64-linux-gnu");
    if wine_native.exists() {
        ld_parts.push(wine_native.display().to_string());
    }
    if steam_linux64.exists() {
        ld_parts.push(steam_linux64.display().to_string());
    }
    ld_parts.push(wine_lib.display().to_string());
    // Unix .so modules (crypt32.so, ws2_32.so, etc.) have NEEDED: ntdll.so.
    // The dynamic linker must find ntdll.so by name — add the x86_64-unix dirs.
    if let Some(ref wlib) = quark_wine_lib {
        let unix_dir = wlib.join("x86_64-unix");
        if unix_dir.exists() {
            ld_parts.push(unix_dir.display().to_string());
        }
    }
    let wine_unix = wine_dir.join("lib/wine/x86_64-unix");
    if wine_unix.exists() {
        ld_parts.push(wine_unix.display().to_string());
    }
    if let Some(proton) = proton_dir {
        let proton_lib = proton.join("lib");
        if proton_lib.exists() && proton_lib != wine_lib {
            ld_parts.push(proton_lib.display().to_string());
        }
    }
    if !cur_ld.is_empty() {
        ld_parts.push(cur_ld);
    }
    let ld_path = ld_parts.join(":");

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

    let mut vars = vec![
        ("WINEPREFIX", pfx.display().to_string()),
        ("WINESERVER", triskelion_exe().display().to_string()),
        ("WINELOADER", wine64.display().to_string()),
        ("WINEDLLPATH", dll_path),
        ("WINEDATADIR", wine_data_dir),
        ("WINEDLLDIR0", wine_dll_dir0.clone()),
        ("WINEDLLDIR1", wine_dll_dir0), // vkd3d also searched here
        ("WINECONFIGDIR", wine_config_dir),
        ("WINEHOMEDIR", wine_home_dir),
        ("WINEUSERNAME", std::env::var("USER").unwrap_or_else(|_| "user".into())),
        ("PATH", format!("{}:{}", wine_bin.display(), cur_path)),
        ("LD_LIBRARY_PATH", ld_path),
        ("WINEDLLOVERRIDES", dll_overrides),
        ("DXVK_LOG_LEVEL", std::env::var("DXVK_LOG_LEVEL").unwrap_or_else(|_| "none".into())),
        ("VKD3D_DEBUG", "none".into()),
        ("WINE_LARGE_ADDRESS_AWARE", "1".into()),
        ("QUARK_FAST_BOOT", "1".into()),
        ("PROTON_VERSION", format!("quark {}", env!("CARGO_PKG_VERSION"))),

    ];

    // EAC runtime: tell ntdll where to find the bridge DLLs
    let eac_runtime = PathBuf::from(&home)
        .join(".local/share/Steam/steamapps/common/Proton EasyAntiCheat Runtime");
    if eac_runtime.exists() {
        vars.push(("PROTON_EAC_RUNTIME", eac_runtime.display().to_string()));
    }

    // Windows profile environment variables — Wine's SHGetFolderPathW needs these
    // to resolve %APPDATA% etc. Without registry-backed ProfileList, the fallback
    // chain fails and games (e.g. LOVE2D) throw exceptions during filesystem init.
    let username = std::env::var("USER").unwrap_or_else(|_| "user".to_string());
    let profile = format!("C:\\users\\{username}");
    vars.push(("USERPROFILE", profile.clone()));
    vars.push(("APPDATA", format!("{profile}\\AppData\\Roaming")));
    vars.push(("LOCALAPPDATA", format!("{profile}\\AppData\\Local")));
    vars.push(("TEMP", format!("{profile}\\Temp")));
    vars.push(("TMP", format!("{profile}\\Temp")));
    vars.push(("HOMEDRIVE", "C:".into()));
    vars.push(("HOMEPATH", format!("\\users\\{username}")));
    vars.push(("ALLUSERSPROFILE", "C:\\ProgramData".into()));
    vars.push(("ProgramData", "C:\\ProgramData".into()));

    // Async shader compilation
    vars.push(("DXVK_ASYNC", "1".into()));
    vars.push(("VKD3D_CONFIG", "shader_cache".into()));

    // Sync: ntsync inproc — send device fd via init_first_thread.
    // Client does all sync in-process via /dev/ntsync ioctl.
    // Server-side ntsync used for Select waits that need cross-process coordination.
    
    if std::path::Path::new("/dev/ntsync").exists() {
        vars.push(("WINE_NTSYNC", "1".into()));
    }
    vars.push(("WINEESYNC", "0".into()));
    vars.push(("WINEFSYNC", "0".into()));

    // NVIDIA: DXVK NVAPI support for DLSS/NGX
    if std::path::Path::new("/proc/driver/nvidia/version").exists() {
        vars.push(("DXVK_ENABLE_NVAPI", "1".into()));
        // NVIDIA EGL/GLX — force NVIDIA's implementations, not mesa's
        let nvidia_egl = "/usr/share/glvnd/egl_vendor.d/10_nvidia.json";
        if std::path::Path::new(nvidia_egl).exists() {
            vars.push(("__EGL_VENDOR_LIBRARY_FILENAMES", nvidia_egl.into()));
        }
        vars.push(("__GLX_VENDOR_LIBRARY_NAME", "nvidia".into()));
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

    // DLL overrides from Proton: opencl and gameinput cause issues
    // These get merged into WINEDLLOVERRIDES by the override builder above,
    // but we can add them as env vars too for runtime override.

    // Steam ffmpeg libraries for game video/cutscene playback
    let steam_dir_str = steam_dir.display().to_string();
    let ffmpeg_path = format!("{steam_dir_str}/ubuntu12_64/video/:{steam_dir_str}/ubuntu12_32/video/");
    // Prepend to LD_LIBRARY_PATH if the dirs exist
    if std::path::Path::new(&format!("{steam_dir_str}/ubuntu12_64/video/")).exists() {
        if let Some(pos) = vars.iter().position(|(k, _)| *k == "LD_LIBRARY_PATH") {
            let existing = vars[pos].1.clone();
            vars[pos].1 = format!("{ffmpeg_path}:{existing}");
        }
    }

    // Steam Input: SDL 2.30+ reads SteamVirtualGamepadInfo to configure
    // virtual gamepads. Steam sets SteamVirtualGamepadInfo_Proton before
    // launching compat tools — Proton copies it to SteamVirtualGamepadInfo.
    // Without this, controllers are invisible to SDL-based games.
    if let Ok(gamepad_info) = std::env::var("SteamVirtualGamepadInfo_Proton") {
        vars.push(("SteamVirtualGamepadInfo", gamepad_info));
    }

    // Display driver: always winex11.drv + GLX over XWayland.

    // Shader cache optimization — opt-in via install.py prompt.
    if shader_cache_enabled {
        let shader_cache = pfx.join("shader_cache");
        if let Err(e) = std::fs::create_dir_all(&shader_cache) {
            log_warn!("Cannot create shader cache dir: {e}");
        }
        let shader_cache_str = shader_cache.display().to_string();

        // DXVK: compiled SPIR-V cache (DXBC/DXSO → SPIR-V translation results)
        vars.push(("DXVK_SHADER_CACHE_PATH", shader_cache_str.clone()));
        // VKD3D-Proton: compiled SPIR-V cache (DXBC/DXIL → SPIR-V translation results)
        vars.push(("VKD3D_SHADER_CACHE_PATH", shader_cache_str.clone()));

        // Detect GPU vendor for vendor-specific cache tuning.
        let is_nvidia = Path::new("/proc/driver/nvidia/version").exists();

        if is_nvidia {
            // NVIDIA: driver compiles SPIR-V → GPU ISA, caches in GLCache.
            // Default was 128 MB, recently raised to 1 GB — still too small for
            // AAA games with 5,000+ pipelines. 10 GB prevents eviction.
            vars.push(("__GL_SHADER_DISK_CACHE", "1".into()));
            vars.push(("__GL_SHADER_DISK_CACHE_PATH", shader_cache_str.clone()));
            vars.push(("__GL_SHADER_DISK_CACHE_SIZE", "10737418240".into())); // 10 GB
            vars.push(("__GL_SHADER_DISK_CACHE_SKIP_CLEANUP", "1".into()));
            // NVIDIA on Wayland: use NVIDIA's EGL ICD, not mesa's (which fails with dri2)
            let nvidia_egl = "/usr/share/glvnd/egl_vendor.d/10_nvidia.json";
            if std::path::Path::new(nvidia_egl).exists() {
                vars.push(("__EGL_VENDOR_LIBRARY_FILENAMES", nvidia_egl.into()));
            }
        } else {
            // Mesa/RADV (AMD, Intel): driver compiles SPIR-V → GPU ISA, caches
            // in mesa_shader_cache. Default size is small. 10 GB prevents eviction
            // across sessions. Single-file mode is Fossilize-compatible and has
            // less disk overhead than multi-file mode.
            vars.push(("MESA_SHADER_CACHE_DIR", shader_cache_str.clone()));
            vars.push(("MESA_SHADER_CACHE_MAX_SIZE", "10G".into()));
            vars.push(("MESA_DISK_CACHE_SINGLE_FILE", "1".into()));
            // RADV: Graphics Pipeline Library — allows partial pipeline
            // pre-compilation so full pipeline assembly at draw time is
            // near-instant instead of compiling from scratch.
            vars.push(("RADV_PERFTEST", "gpl".into()));
        }
    }

    vars
}

/// Parse user-supplied environment variables from env_config file.
/// Format: KEY=VALUE lines. # comments and blank lines are ignored.
/// Returns empty Vec if the file is missing or unreadable.
fn parse_env_config(self_exe: &Path) -> Vec<(String, String)> {
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
    } else if triskelion::log::is_verbose() {
        "+process,err".into()
    } else {
        "-all".into()
    }
}

/// Sync the Wine prefix with the current env config so Wine doesn't re-run
/// wineboot at game time. Compares a hash of env vars against a stored marker;
/// only runs `wineboot --update` when the config actually changed.
fn _sync_prefix_env(
    wine64: &Path, pfx: &Path, _self_exe: &Path, wine_dir: &Path,
    env_vars: &[(&str, String)], custom_env: &[(String, String)],
) {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    // Hash the env vars that affect prefix layout (display driver, etc.)
    let mut hasher = DefaultHasher::new();
    for (k, v) in custom_env {
        k.hash(&mut hasher);
        v.hash(&mut hasher);
    }
    let env_hash = format!("{:016x}", hasher.finish());

    let marker = pfx.join(".env-sync-hash");
    if let Ok(stored) = std::fs::read_to_string(&marker) {
        if stored.trim() == env_hash {
            return; // already synced
        }
    }

    log_verbose!("prefix env changed — running wineboot --update to sync");
    let mut cmd = build_wine_command(wine64, wine_dir);
    cmd.args(["wineboot", "--update"]);
    cmd.env("WINEPREFIX", pfx.as_os_str());
    cmd.env("WINESERVER", triskelion_exe());

    // Set up library paths so Wine can find its .so modules
    let unix_dir = wine_dir.join("lib/wine/x86_64-unix");
    let wine_lib = wine_dir.join("lib");
    let mut ld_path = String::new();
    if unix_dir.exists() {
        ld_path.push_str(&unix_dir.display().to_string());
    }
    if wine_lib.exists() {
        if !ld_path.is_empty() { ld_path.push(':'); }
        ld_path.push_str(&wine_lib.display().to_string());
    }
    if let Ok(existing) = std::env::var("LD_LIBRARY_PATH") {
        if !ld_path.is_empty() { ld_path.push(':'); }
        ld_path.push_str(&existing);
    }
    if !ld_path.is_empty() {
        cmd.env("LD_LIBRARY_PATH", &ld_path);
    }
    // WINEDLLPATH: system Wine finds its own builtins, but quark-specific modules
    // (lsteamclient.so, steam.exe) live in our tree. Wine needs this to find them.
    {
        let quark_unix = wine_dir.join("lib/wine/x86_64-unix");
        let quark_win = wine_dir.join("lib/wine/x86_64-windows");
        let sys_unix = Path::new("/usr/lib/wine/x86_64-unix");
        let sys_win = Path::new("/usr/lib/wine/x86_64-windows");
        let mut dll_parts: Vec<String> = Vec::new();
        if quark_unix.exists() { dll_parts.push(quark_unix.display().to_string()); }
        if quark_win.exists() { dll_parts.push(quark_win.display().to_string()); }
        if sys_unix.exists() { dll_parts.push(sys_unix.display().to_string()); }
        if sys_win.exists() { dll_parts.push(sys_win.display().to_string()); }
        if !dll_parts.is_empty() {
            cmd.env("WINEDLLPATH", dll_parts.join(":"));
        }
    }

    // Apply game env vars, but skip DLL path overrides from Steam (would conflict with ours)
    for (k, v) in env_vars {
        if *k == "WINEDLLPATH" || *k == "WINEDLLDIR0" || *k == "WINEDLLDIR1" { continue; }
        cmd.env(k, v);
    }
    for (k, v) in custom_env {
        cmd.env(k, v);
    }

    cmd.env("WINEDEBUG", resolve_winedebug(false));

    match cmd.status() {
        Ok(s) if s.success() => {
            let _ = std::fs::write(&marker, &env_hash);
            log_verbose!("prefix env sync complete");
        }
        Ok(s) => log_warn!("wineboot --update exited with {}", s.code().unwrap_or(-1)),
        Err(e) => log_warn!("wineboot --update failed: {e}"),
    }
}

// Prometheus launch diagnostics removed — montauk handles tracing now.

// ---------------------------------------------------------------------------
// Discovery
// ---------------------------------------------------------------------------

/// Check if a Wine directory has a usable Wine binary.
/// Wine 10.0+ unified to just "wine"; older builds use "wine64".
fn has_wine_bin(dir: &Path) -> bool {
    dir.join("lib/wine/x86_64-unix/wine").exists()
        || dir.join("bin/wine64").exists()
        || dir.join("bin/wine").exists()
}

/// Get the Wine binary path from a Wine directory.
/// Prefers the wine binary alongside ntdll.so — Wine resolves PE builtins
/// relative to ntdll.so's realpath, so the binary MUST be in the same
/// directory as ntdll.so for our patched ntdll.dll to be loaded.
fn wine_binary(dir: &Path) -> PathBuf {
    let unix_wine = dir.join("lib/wine/x86_64-unix/wine");
    if unix_wine.exists() { return unix_wine; }
    let w64 = dir.join("bin/wine64");
    if w64.exists() { w64 } else { dir.join("bin/wine") }
}

/// Build a Command that runs `wine_bin` inside a bwrap mount namespace where
/// `/usr/lib/wine` and `/usr/share/wine` are bind-mounted from quark's tree.
///
/// This is the architectural fix for "patched DLLs got poisoned in /usr."
/// Wine's loader has /usr/lib/wine baked in at compile time and ignores most
/// override knobs for the early bootstrap (ntdll.so, win32u.so). bwrap solves
/// it at the kernel level: inside the namespace, /usr/lib/wine *is* quark's
/// tree, so the unmodified system Wine binary loads quark's patched DLLs
/// without ever touching the real /usr.
///
/// Falls back to direct exec (no bwrap) if:
///   - bwrap is not installed
///   - quark's lib/wine or share/wine tree is missing (install.py never ran)
/// In the fallback case the user runs against unpatched system Wine, which
/// is ugly but not poisonous.
fn build_wine_command(wine_bin: &Path, wine_dir: &Path) -> Command {
    let bwrap = Path::new("/usr/bin/bwrap");
    let quark_lib_wine = wine_dir.join("lib/wine");
    let quark_share_wine = wine_dir.join("share/wine");

    if !bwrap.exists() {
        log_warn!("bwrap not found at /usr/bin/bwrap — exec'ing wine directly.");
        log_warn!("Patched DLLs will NOT be loaded. Install bubblewrap (pacman -S bubblewrap).");
        return Command::new(wine_bin);
    }
    if !quark_lib_wine.exists() || !quark_share_wine.exists() {
        log_warn!("quark Wine tree incomplete (missing {} or {}) — exec'ing wine directly.",
            quark_lib_wine.display(), quark_share_wine.display());
        log_warn!("Re-run install.py to populate the compat tree.");
        return Command::new(wine_bin);
    }

    let mut cmd = Command::new(bwrap);
    // Pass through the entire host filesystem first.
    cmd.arg("--dev-bind").arg("/").arg("/");
    // Overlay quark's Wine tree on top. Inside the namespace, when Wine's
    // loader opens /usr/lib/wine/x86_64-unix/ntdll.so it gets quark's patched
    // copy. The actual /usr on disk is never touched.
    cmd.arg("--bind").arg(&quark_lib_wine).arg("/usr/lib/wine");
    cmd.arg("--bind").arg(&quark_share_wine).arg("/usr/share/wine");
    // bwrap exits when its parent (this launcher) exits. Belt-and-braces
    // alongside the existing death-pipe.
    cmd.arg("--die-with-parent");
    cmd.arg("--");
    cmd.arg(wine_bin);
    cmd
}

/// Find Wine binaries. Priority:
/// 1. TRISKELION_WINE_DIR env var (explicit override)
/// 2. quark's own staged binaries — system Wine copied here by
///    install.py so argv[0]/../lib/ resolves to our custom ntdll.so
///    (built from matching upstream Wine version)
/// 3. Proton Experimental (prefix template source)
/// 4. Any Proton version
/// 5. System Wine (fallback)
fn find_wine() -> PathBuf {
    if let Ok(dir) = std::env::var("TRISKELION_WINE_DIR") {
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

    // System Wine — triskelion targets system Wine's protocol version (930 = Wine 11.4).
    // Using Proton's wine64 would cause protocol version mismatch (Proton has 856).
    if has_wine_bin(&PathBuf::from("/usr")) {
        return PathBuf::from("/usr");
    }

    // Proton Experimental (fallback only — protocol version may not match!)
    let proton_exp = PathBuf::from(&home)
        .join(".steam/root/steamapps/common/Proton - Experimental/files");
    if has_wine_bin(&proton_exp) {
        log_warn!("using Proton Experimental wine — protocol version may not match triskelion!");
        return proton_exp;
    }

    // Any Proton version (fallback)
    let common = PathBuf::from(&home).join(".steam/root/steamapps/common");
    if let Ok(entries) = std::fs::read_dir(&common) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with("Proton") {
                let files = entry.path().join("files");
                if has_wine_bin(&files) {
                    log_warn!("using {name} wine — protocol version may not match triskelion!");
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
fn snapshot_save_data(pfx: &Path) -> Option<PathBuf> {
    let user_dir = pfx.join("drive_c/users/steamuser");
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
fn restore_save_data(pfx: &Path, backup_dir: &Path) {
    if !backup_dir.exists() {
        return;
    }

    let user_dir = pfx.join("drive_c/users/steamuser");
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
        } else if let Err(e) = std::fs::copy(&src_path, &dst_path) {
            log_warn!("save backup: failed to copy {}: {e}", src_path.display());
        }
    }
}

/// Remove .dmp and .mdmp crash dumps from the prefix before launch.
/// Engines (Godot, Unity, Unreal) read their crash directories on startup
/// and may enter recovery/report paths that interfere with normal launch.
fn clean_crash_dumps(pfx: &Path) {
    let user_dir = pfx.join("drive_c/users/steamuser");
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
            if ext.eq_ignore_ascii_case("dmp") || ext.eq_ignore_ascii_case("mdmp") {
                if std::fs::remove_file(&path).is_ok() {
                    removed += 1;
                }
            }
        }
    }
    removed
}
