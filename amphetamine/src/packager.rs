// Packaging pipeline.
// Packages a built Wine tree as a Steam compatibility tool.
//
// Output: ~/.steam/root/compatibilitytools.d/amphetamine/
//   compatibilitytool.vdf
//   toolmanifest.vdf
//   proton              (this binary)
//   files/
//     bin/              (wine64, wineserver)
//     lib/wine/
//       x86_64-unix/    (.so drivers)
//       x86_64-windows/ (.dll PE files)
//     share/wine/       (nls, wine.inf)

use std::path::{Path, PathBuf};
use std::fs;

fn compatibilitytool_vdf() -> String {
    let version = env!("CARGO_PKG_VERSION");
    format!(
        r#""compatibilitytools"
{{
  "compat_tools"
  {{
    "amphetamine"
    {{
      "install_path" "."
      "display_name" "amphetamine {version}"
      "from_oslist"  "windows"
      "to_oslist"    "linux"
    }}
  }}
}}
"#
    )
}

const TOOLMANIFEST_VDF: &str = r#""manifest"
{
  "commandline" "/proton %verb%"
  "version" "2"
  "use_sessions" "1"
}
"#;

const PE_SUFFIXES: &[&str] = &[
    ".dll", ".exe", ".sys", ".drv", ".tlb", ".cpl",
    ".acm", ".ax", ".ocx", ".ds",
];

pub fn run(wine_dir: &str) -> i32 {
    let wine_path = PathBuf::from(wine_dir);
    let wine_path = match fs::canonicalize(&wine_path) {
        Ok(p) => p,
        Err(_) => {
            log_error!("Cannot resolve path: {wine_dir}");
            return 1;
        }
    };

    // Verify built
    let wineserver = wine_path.join("server").join("wineserver");
    if !wineserver.exists() {
        log_error!("No built wineserver in {}. Run 'make' first.", wine_path.display());
        return 1;
    }

    let home = std::env::var("HOME").expect("HOME not set");
    let dest = PathBuf::from(&home)
        .join(".steam")
        .join("root")
        .join("compatibilitytools.d")
        .join("amphetamine");
    let files = dest.join("files");

    if dest.exists() {
        log_warn!("Removing existing package: {}", dest.display());
        let _ = fs::remove_dir_all(&dest);
    }

    log_info!("Packaging to {}", dest.display());

    let bin_dir = files.join("bin");
    let lib_wine = files.join("lib").join("wine");
    let lib_unix = lib_wine.join("x86_64-unix");
    let lib_win = lib_wine.join("x86_64-windows");
    let share_wine = files.join("share").join("wine");

    for d in [&bin_dir, &lib_unix, &lib_win, &share_wine] {
        fs::create_dir_all(d).expect("Failed to create directory");
    }

    // Binaries
    let exe_count = copy_binaries(&wine_path, &bin_dir);

    // PE files (DLLs and programs)
    let (dll_count, prog_count) = copy_pe_files(&wine_path, &lib_win);

    // Patch .idata sections (binutils 2.44+)
    match crate::pe_patch::fix_idata_sections(&lib_win) {
        Ok(n) if n > 0 => log_info!("Patched .idata -> RW in {n} PE files (binutils 2.44+ fix)"),
        Ok(_) => {}
        Err(e) => log_warn!("PE patching error: {e}"),
    }

    // Unix drivers (.so)
    let so_count = copy_unix_drivers(&wine_path, &lib_unix);

    // NLS files
    let nls_count = copy_nls(&wine_path, &share_wine);
    if nls_count > 0 {
        log_info!("Copied {nls_count} NLS files");
    }

    // wine.inf
    copy_wine_inf(&wine_path, &share_wine);

    // VDF files
    fs::write(dest.join("compatibilitytool.vdf"), compatibilitytool_vdf())
        .expect("Failed to write VDF");
    fs::write(dest.join("toolmanifest.vdf"), TOOLMANIFEST_VDF)
        .expect("Failed to write VDF");

    // Install this binary as the proton launcher
    install_launcher(&dest);

    println!();
    log_info!("Package complete: {}", dest.display());
    log_info!("  DLLs:      {dll_count}");
    log_info!("  Programs:  {prog_count}");
    log_info!("  Drivers:   {so_count}");
    log_info!("  Binaries:  {exe_count}");
    log_info!("");
    log_info!("Restart Steam, then set a game's compatibility tool to");
    log_info!("'amphetamine (stripped Wine)' in Properties > Compatibility.");
    log_info!("");
    log_info!("Launch from terminal:  steam steam://rungameid/<APPID>");
    println!();

    0
}

fn copy_binaries(wine_path: &Path, bin_dir: &Path) -> u32 {
    let mut count = 0u32;

    // wineserver
    let src = wine_path.join("server").join("wineserver");
    if src.exists() {
        let _ = fs::copy(&src, bin_dir.join("wineserver"));
        set_executable(bin_dir.join("wineserver"));
        count += 1;
    }

    // wine64 loader
    let wine64 = wine_path.join("loader").join("wine64");
    let wine_fallback = wine_path.join("wine");
    let src = if wine64.exists() {
        wine64
    } else if wine_fallback.exists() {
        wine_fallback
    } else {
        return count;
    };
    let _ = fs::copy(&src, bin_dir.join("wine64"));
    set_executable(bin_dir.join("wine64"));
    count += 1;

    // wine64-preloader
    let preloader = wine_path.join("loader").join("wine64-preloader");
    if preloader.exists() {
        let _ = fs::copy(&preloader, bin_dir.join("wine64-preloader"));
        set_executable(bin_dir.join("wine64-preloader"));
        count += 1;
    }

    count
}

fn copy_pe_files(wine_path: &Path, lib_win: &Path) -> (u32, u32) {
    let mut dll_count = 0u32;
    let mut prog_count = 0u32;

    for (src_parent, counter) in [("dlls", &mut dll_count), ("programs", &mut prog_count)] {
        let parent = wine_path.join(src_parent);
        if !parent.is_dir() {
            continue;
        }
        let entries = match fs::read_dir(&parent) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for mod_entry in entries.flatten() {
            if !mod_entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                continue;
            }
            let win_dir = mod_entry.path().join("x86_64-windows");
            if !win_dir.is_dir() {
                continue;
            }
            if let Ok(pe_entries) = fs::read_dir(&win_dir) {
                for pe_entry in pe_entries.flatten() {
                    let name = pe_entry.file_name();
                    let name_str = name.to_string_lossy();
                    if PE_SUFFIXES.iter().any(|s| name_str.ends_with(s)) {
                        let _ = fs::copy(pe_entry.path(), lib_win.join(&name));
                        *counter += 1;
                    }
                }
            }
        }
    }

    (dll_count, prog_count)
}

fn copy_unix_drivers(wine_path: &Path, lib_unix: &Path) -> u32 {
    let mut count = 0u32;
    let dlls = wine_path.join("dlls");
    if !dlls.is_dir() {
        return 0;
    }

    if let Ok(entries) = fs::read_dir(&dlls) {
        for entry in entries.flatten() {
            if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                continue;
            }
            let dir = entry.path();
            if let Ok(files) = fs::read_dir(&dir) {
                for f in files.flatten() {
                    let name = f.file_name();
                    if name.to_string_lossy().ends_with(".so") {
                        let _ = fs::copy(f.path(), lib_unix.join(&name));
                        count += 1;
                    }
                }
            }
        }
    }
    count
}

fn copy_nls(wine_path: &Path, share_wine: &Path) -> u32 {
    let nls_src = wine_path.join("nls");
    if !nls_src.is_dir() {
        return 0;
    }

    let nls_dest = share_wine.join("nls");
    fs::create_dir_all(&nls_dest).ok();

    let mut count = 0u32;
    if let Ok(entries) = fs::read_dir(&nls_src) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            if name.to_string_lossy().ends_with(".nls") {
                let _ = fs::copy(entry.path(), nls_dest.join(&name));
                count += 1;
            }
        }
    }
    count
}

fn copy_wine_inf(wine_path: &Path, share_wine: &Path) {
    let candidates = [
        wine_path.join("loader").join("wine.inf"),
        wine_path.join("wine.inf"),
    ];
    for src in &candidates {
        if src.exists() {
            let _ = fs::copy(src, share_wine.join("wine.inf"));
            log_info!("Copied wine.inf");
            return;
        }
    }
    log_warn!("wine.inf not found -- prefix init will fail");
}

fn install_launcher(dest: &Path) {
    let exe = std::env::current_exe().expect("cannot resolve current executable");
    let exe = fs::canonicalize(exe).expect("cannot canonicalize executable path");
    let proton = dest.join("proton");
    fs::copy(&exe, &proton).expect("Failed to install proton launcher");
    set_executable(&proton);
}

fn set_executable<P: AsRef<Path>>(path: P) {
    use std::os::unix::fs::PermissionsExt;
    if let Ok(meta) = fs::metadata(path.as_ref()) {
        let mut perms = meta.permissions();
        perms.set_mode(0o755);
        let _ = fs::set_permissions(path.as_ref(), perms);
    }
}
