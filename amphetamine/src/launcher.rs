// amphetamine launcher -- full Proton replacement.
//
// Steam calls: ./proton <verb> <appid> [args...]
// amphetamine sets up the prefix, deploys DXVK/VKD3D, bridges Steam client,
// then launches wine64 with WINESERVER=triskelion. One binary. Full stack.
//
// Optimization: after first launch, the .triskelion_deployed cache records
// what was deployed and from where. Subsequent launches skip all file ops
// when the cache is valid — straight to wine64.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

// DXVK: DirectX 9/10/11 → Vulkan translation
const DXVK_DLLS: &[&str] = &["d3d11", "d3d10core", "d3d9", "dxgi"];

// VKD3D-Proton: DirectX 12 → Vulkan translation
const VKD3D_DLLS: &[&str] = &["d3d12", "d3d12core"];

// Base DLL overrides (before DXVK/VKD3D additions)
const BASE_OVERRIDES: &[(&str, &str)] = &[
    ("steam.exe", "b"),
    ("dotnetfx35.exe", "b"),
    ("dotnetfx35setup.exe", "b"),
    ("beclient.dll", "b,n"),
    ("beclient_x64.dll", "b,n"),
];

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
    match verb {
        "getcompatpath" | "getnativepath" => {
            if let Some(path) = args.first() {
                println!("{path}");
            }
            return 0;
        }
        "installscript" | "runinprefix" => {
            log_info!("{verb}: no-op (Wine provides these APIs natively)");
            return 0;
        }
        _ => {}
    }

    let t_start = Instant::now();

    // Phase 1: Locate everything
    let wine_dir = find_wine();
    let steam_dir = find_steam_dir();
    let wine_bin = wine_dir.join("bin");
    let wine64 = wine_bin.join("wine64");

    if !wine64.exists() {
        log_error!("wine64 not found at {}", wine64.display());
        log_error!("Need Wine binaries. Set TRISKELION_WINE_DIR or install Proton.");
        return 1;
    }

    let compat_data = std::env::var("STEAM_COMPAT_DATA_PATH").unwrap_or_default();
    if compat_data.is_empty() {
        log_error!("STEAM_COMPAT_DATA_PATH not set — not launched from Steam?");
        return 1;
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

    log_info!("wine64: {}", wine64.display());
    log_info!("wineserver: {} (triskelion)", self_exe.display());
    log_info!("steam: {}", steam_dir.display());

    // Per-component cache — only redeploy what actually changed
    let cache = DeployCache::load(&pfx);
    let wine_hash = dir_hash(&wine_dir);
    let dxvk_hash = dir_hash(&wine_dir.join("lib/wine/dxvk"));
    let vkd3d_hash = dir_hash(&wine_dir.join("lib/wine/vkd3d-proton"));
    let steam_hash = dir_hash(&steam_dir.join("legacycompat"));

    let wine_valid = cache.as_ref().is_some_and(|c| c.wine_hash == wine_hash);
    let dxvk_valid = cache.as_ref().is_some_and(|c| c.dxvk_hash == dxvk_hash);
    let vkd3d_valid = cache.as_ref().is_some_and(|c| c.vkd3d_hash == vkd3d_hash);
    let steam_valid = cache.as_ref().is_some_and(|c| c.steam_hash == steam_hash);

    // Phase 2: Prefix setup (idempotent)
    let t2 = Instant::now();
    if !wine_valid {
        setup_prefix(&wine_dir, &pfx, &wine64, &self_exe);
    }
    let t_prefix = t2.elapsed();

    // Phase 3: Deploy DXVK/VKD3D into prefix (per-component)
    let t3 = Instant::now();
    let dxvk_deployed = if dxvk_valid {
        DXVK_DLLS.to_vec()
    } else {
        deploy_dxvk(&wine_dir, &pfx)
    };
    let vkd3d_deployed = if vkd3d_valid {
        VKD3D_DLLS.to_vec()
    } else {
        deploy_vkd3d(&wine_dir, &pfx)
    };
    let t_dxvk = t3.elapsed();

    // Phase 4: Deploy Steam client DLLs (per-component)
    let t4 = Instant::now();
    if !steam_valid {
        deploy_steam_client(&steam_dir, &pfx);
    }
    let t_steam = t4.elapsed();

    // Registry keys run outside cache gate — has its own idempotency check
    inject_registry_keys(&pfx);

    // Save cache if anything changed
    let all_valid = wine_valid && dxvk_valid && vkd3d_valid && steam_valid;
    if !all_valid {
        DeployCache { wine_hash, dxvk_hash, vkd3d_hash, steam_hash }.save(&pfx);
        log_verbose!("deployment cache written");
    } else {
        log_verbose!("cache hit — skipped all file ops");
    }

    // Phase 5: Build environment
    let trace = std::env::var("AMPHETAMINE_TRACE_OPCODES").is_ok()
        || Path::new("/tmp/amphetamine/TRACE_OPCODES").exists();

    let env_vars = build_env_vars(
        &wine_dir, &steam_dir, &pfx, &self_exe,
        &dxvk_deployed, &vkd3d_deployed, trace,
    );
    let t_total_setup = t_start.elapsed();

    // Write timing data (verbose only)
    if crate::log::is_verbose() {
        if let Err(e) = std::fs::create_dir_all("/tmp/amphetamine") {
            log_warn!("Cannot create /tmp/amphetamine: {e}");
        }
        let timing = format!(
            "{{\"discover_ms\":{},\"prefix_ms\":{},\"dxvk_ms\":{},\"steam_ms\":{},\"total_setup_ms\":{},\"cache_hit\":{}}}",
            t_discover.as_millis(), t_prefix.as_millis(),
            t_dxvk.as_millis(), t_steam.as_millis(), t_total_setup.as_millis(),
            all_valid,
        );
        if let Err(e) = std::fs::write("/tmp/amphetamine/launcher_timing.json", &timing) {
            log_warn!("Cannot write timing data: {e}");
        }
        log_verbose!("timing: discover={}ms prefix={}ms dxvk={}ms steam={}ms total={}ms",
            t_discover.as_millis(), t_prefix.as_millis(),
            t_dxvk.as_millis(), t_steam.as_millis(), t_total_setup.as_millis());
    }

    // Phase 6: Launch
    if verb == "waitforexitandrun" || verb == "run" {
        let game_exe = match args.first() {
            Some(exe) => exe,
            None => {
                log_error!("No executable specified");
                return 1;
            }
        };

        // Steam runs iscriptevaluator.exe for DirectX/vcredist install scripts.
        // Wine provides these APIs natively — skip it.
        if game_exe.contains("iscriptevaluator") {
            log_info!("skipping install script evaluator (Wine provides these APIs)");
            return 0;
        }

        // Sanity check: does the game exe exist?
        let game_path = Path::new(game_exe);
        if !game_path.exists() {
            log_warn!("game exe not found at {game_exe} (may be a Windows path — continuing)");
        }

        log_info!("launching: {game_exe}");

        let mut cmd = Command::new(&wine64);
        // Launch through Wine's built-in steam.exe bridge for Steam client connection
        cmd.arg("c:\\windows\\system32\\steam.exe");
        cmd.arg(game_exe);
        cmd.args(&args[1..]);
        for (k, v) in &env_vars {
            cmd.env(k, v);
        }

        if trace {
            if let Err(e) = std::fs::create_dir_all("/tmp/amphetamine") {
                log_warn!("Cannot create trace dir: {e}");
            }
            let trace_file = Path::new("/tmp/amphetamine/opcode_trace.log");
            match std::fs::File::create(trace_file) {
                Ok(f) => { cmd.stderr(std::process::Stdio::from(f)); }
                Err(e) => log_warn!("Tracing enabled but cannot create {}: {e}", trace_file.display()),
            }
        }

        let status = cmd.status().unwrap_or_else(|e| {
            log_error!("Failed to exec wine64: {e}");
            std::process::exit(1);
        });

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
}

impl DeployCache {
    fn load(pfx: &Path) -> Option<Self> {
        let data = std::fs::read_to_string(pfx.join(CACHE_FILE)).ok()?;
        let parts: Vec<&str> = data.strip_prefix("v3:")?.trim().split(',').collect();
        if parts.len() != 4 { return None; }
        Some(DeployCache {
            wine_hash: parts[0].parse().ok()?,
            dxvk_hash: parts[1].parse().ok()?,
            vkd3d_hash: parts[2].parse().ok()?,
            steam_hash: parts[3].parse().ok()?,
        })
    }

    fn save(&self, pfx: &Path) {
        let data = format!("v3:{},{},{},{}", self.wine_hash, self.dxvk_hash, self.vkd3d_hash, self.steam_hash);
        if let Err(e) = std::fs::write(pfx.join(CACHE_FILE), data) {
            log_warn!("Cannot write deployment cache: {e} — next launch will re-deploy");
        }
    }
}

/// Quick hash of a directory: combine dev+ino+mtime of the path itself.
/// Changes when Proton updates (new inode or mtime on files/ dir).
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
            log_info!("Setting up prefix from template...");
        } else {
            log_info!("Repairing prefix from template...");
        }
        let count = copy_dir_fast(&default_pfx, pfx);
        if count > 0 {
            log_info!("Prefix: {count} files deployed");
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
        // No template available (system Wine) — fall back to wineboot
        log_info!("No default_pfx template, running wineboot --init...");
        let mut cmd = Command::new(wine64);
        cmd.args(["wineboot", "--init"]);
        cmd.env("WINEPREFIX", pfx.as_os_str());
        cmd.env("WINESERVER", self_exe.as_os_str());
        match cmd.status() {
            Ok(s) if !s.success() => log_error!("wineboot --init failed with exit code {}", s.code().unwrap_or(-1)),
            Err(e) => log_error!("Failed to run wineboot: {e}"),
            _ => {}
        }
    }
}

/// Inject registry keys that Proton normally installs via redistributable packages.
/// The DLLs are already in the prefix (from template symlinks), but some games
/// check registry keys to verify installation before even trying to load them.
fn inject_registry_keys(pfx: &Path) {
    let sys_reg = pfx.join("system.reg");
    let marker = "VC\\\\Runtimes\\\\x64";

    // Check if already injected
    if let Ok(contents) = std::fs::read_to_string(&sys_reg) {
        if contents.contains(marker) {
            return;
        }
    }

    // VC++ 2015-2022 Redistributable (covers 2017, 2019, 2022 — all use v14.x)
    let keys = r#"

[Software\\Microsoft\\VisualStudio\\14.0\\VC\\Runtimes\\x64] 1772204972
#time=1dca7fb13d11a48
"Installed"=dword:00000001
"Major"=dword:0000000e
"Minor"=dword:00000024
"Bld"=dword:00007280

[Software\\WOW6432Node\\Microsoft\\VisualStudio\\14.0\\VC\\Runtimes\\x86] 1772204972
#time=1dca7fb13d11a48
"Installed"=dword:00000001
"Major"=dword:0000000e
"Minor"=dword:00000024
"Bld"=dword:00007280

[Software\\Microsoft\\NET Framework Setup\\NDP\\v4\\Full] 1772204972
#time=1dca7fb13d11a48
"Install"=dword:00000001
"Release"=dword:00080ff4
"Version"="4.8.09037"

"#;

    let mut f = match std::fs::OpenOptions::new().append(true).open(&sys_reg) {
        Ok(f) => f,
        Err(e) => {
            log_warn!("Cannot open system.reg for registry injection: {e}");
            return;
        }
    };
    use std::io::Write;
    match f.write_all(keys.as_bytes()) {
        Ok(_) => log_info!("Registry: injected VC++ and .NET Framework keys"),
        Err(e) => log_error!("Registry injection write failed: {e}"),
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
                    std::fs::remove_file(&dst_path).ok();
                    // Canonicalize resolves the relative symlink in the source tree
                    // to an absolute path pointing at the real file in Proton
                    if let Ok(real_target) = std::fs::canonicalize(&src_path) {
                        std::os::unix::fs::symlink(&real_target, &dst_path).ok();
                    } else if let Ok(target) = std::fs::read_link(&src_path) {
                        // Dead symlink in template — copy as-is
                        std::os::unix::fs::symlink(&target, &dst_path).ok();
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

/// Check if dst exists and matches src by size and mtime.
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

// ---------------------------------------------------------------------------
// Environment construction
// ---------------------------------------------------------------------------

fn build_env_vars(
    wine_dir: &Path, steam_dir: &Path, pfx: &Path, self_exe: &Path,
    dxvk: &[&str], vkd3d: &[&str], trace: bool,
) -> Vec<(&'static str, String)> {
    let cur_path = std::env::var("PATH").unwrap_or_default();
    let cur_ld = std::env::var("LD_LIBRARY_PATH").unwrap_or_default();

    let wine_bin = wine_dir.join("bin");
    let wine_lib = wine_dir.join("lib");
    let wine_dll = wine_dir.join("lib/wine");
    let wine_vkd3d = wine_dir.join("lib/vkd3d");
    let proton_native = wine_dir.join("lib/x86_64-linux-gnu");
    let steam_linux64 = steam_dir.join("linux64");

    // WINEDLLPATH: vkd3d before wine (so vkd3d-proton shadows Wine's stubs)
    let dll_path = format!("{}:{}", wine_vkd3d.display(), wine_dll.display());

    // LD_LIBRARY_PATH: Proton native libs + Steam client + Wine lib
    let mut ld_parts: Vec<String> = Vec::new();
    if proton_native.exists() {
        ld_parts.push(proton_native.display().to_string());
    }
    if steam_linux64.exists() {
        ld_parts.push(steam_linux64.display().to_string());
    }
    ld_parts.push(wine_lib.display().to_string());
    if !cur_ld.is_empty() {
        ld_parts.push(cur_ld);
    }
    let ld_path = ld_parts.join(":");

    // WINEDLLOVERRIDES: base + DXVK + VKD3D
    let mut overrides: Vec<String> = Vec::new();
    for (name, mode) in BASE_OVERRIDES {
        overrides.push(format!("{name}={mode}"));
    }
    for name in dxvk {
        overrides.push(format!("{name}=n"));
    }
    for name in vkd3d {
        overrides.push(format!("{name}=n"));
    }
    let dll_overrides = overrides.join(";");

    let mut vars = vec![
        ("WINEPREFIX", pfx.display().to_string()),
        ("WINESERVER", self_exe.display().to_string()),
        ("WINEDLLPATH", dll_path),
        ("PATH", format!("{}:{}", wine_bin.display(), cur_path)),
        ("LD_LIBRARY_PATH", ld_path),
        ("WINEDEBUG", if trace { "+server,+timestamp".into() } else { "-all".into() }),
        ("WINEFSYNC", "1".into()),
        ("WINEESYNC", "1".into()),
        ("WINEDLLOVERRIDES", dll_overrides),
        ("DXVK_LOG_LEVEL", "none".into()),
        ("VKD3D_DEBUG", "none".into()),
        ("WINE_LARGE_ADDRESS_AWARE", "1".into()),

        // -- Async shader compilation --
        // DXVK: compile pipelines on background threads instead of blocking
        // the render thread. Eliminates stutter at the cost of 1-2 frames of
        // missing shaders (invisible in practice).
        ("DXVK_ASYNC", "1".into()),
        // VKD3D-Proton (DX12): enable internal shader cache so translated
        // SPIR-V pipelines persist across runs.
        ("VKD3D_CONFIG", "shader_cache".into()),
    ];

    // Shader cache optimization — opt-in via install.py prompt.
    // Flag file is written by configure_shader_cache() in install.py.
    let shader_cache_enabled = self_exe.parent()
        .map(|dir| dir.join("shader_cache_enabled").exists())
        .unwrap_or(false);

    if shader_cache_enabled {
        let shader_cache = pfx.join("shader_cache");
        let _ = std::fs::create_dir_all(&shader_cache);
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

// ---------------------------------------------------------------------------
// Discovery
// ---------------------------------------------------------------------------

/// Find Wine binaries. Priority:
/// 1. TRISKELION_WINE_DIR env var (explicit override)
/// 2. Proton Experimental (Steam-installed, has everything)
/// 3. Any Proton version
/// 4. System Wine
fn find_wine() -> PathBuf {
    if let Ok(dir) = std::env::var("TRISKELION_WINE_DIR") {
        let p = PathBuf::from(dir);
        if p.join("bin/wine64").exists() {
            return p;
        }
    }

    let home = std::env::var("HOME").unwrap_or_default();

    // Proton Experimental
    let proton_exp = PathBuf::from(&home)
        .join(".steam/root/steamapps/common/Proton - Experimental/files");
    if proton_exp.join("bin/wine64").exists() {
        return proton_exp;
    }

    // Any Proton version
    let common = PathBuf::from(&home).join(".steam/root/steamapps/common");
    if let Ok(entries) = std::fs::read_dir(&common) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with("Proton") {
                let files = entry.path().join("files");
                if files.join("bin/wine64").exists() {
                    return files;
                }
            }
        }
    }

    // System Wine
    if Path::new("/usr/bin/wine64").exists() {
        return PathBuf::from("/usr");
    }

    PathBuf::from("/nonexistent/wine")
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
