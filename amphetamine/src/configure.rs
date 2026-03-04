// Wine configure generation.
// Parses configure.ac, generates --disable-{module} flags for non-gaming DLLs,
// optionally runs ./configure with those flags.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

pub fn run(wine_dir: &str, execute: bool) -> i32 {
    let wine_path = PathBuf::from(wine_dir);
    let wine_path = match fs::canonicalize(&wine_path) {
        Ok(p) => p,
        Err(_) => {
            log_error!("Cannot resolve path: {wine_dir}");
            return 1;
        }
    };

    if !wine_path.join("dlls").exists() {
        log_error!("Not a Wine tree: {}", wine_path.display());
        return 1;
    }

    let (all_dlls, all_progs, win16_modules) = parse_configure_ac(&wine_path);
    if all_dlls.is_empty() {
        log_error!("Failed to parse configure.ac");
        return 1;
    }

    let disable_dlls: Vec<&str> = all_dlls.iter()
        .map(|s| s.as_str())
        .filter(|d| !crate::gaming::is_gaming_dll(d) && !win16_modules.contains(*d))
        .collect();
    let disable_progs: Vec<&str> = all_progs.iter()
        .map(|s| s.as_str())
        .filter(|p| !crate::gaming::is_gaming_program(p) && !win16_modules.contains(*p))
        .collect();

    println!();
    log_info!("Wine source: {}", wine_path.display());
    log_info!("configure.ac DLLs:     {}", all_dlls.len());
    log_info!("  keep:                {}", all_dlls.len() - disable_dlls.len());
    log_info!("  disable:             {}", disable_dlls.len());
    log_info!("  win16 (--disable-win16): {}", win16_modules.len());
    log_info!("configure.ac programs: {}", all_progs.len());
    log_info!("  keep:                {}", all_progs.len() - disable_progs.len());
    log_info!("  disable:             {}", disable_progs.len());

    let mut flags: Vec<String> = vec![
        "--enable-win64".into(),
        "--disable-tests".into(),
        "--disable-win16".into(),
    ];
    for dll in &disable_dlls {
        flags.push(format!("--disable-{dll}"));
    }
    for prog in &disable_progs {
        flags.push(format!("--disable-{prog}"));
    }

    if !execute {
        println!();
        log_info!("Dry run. {} configure flags generated:", flags.len());
        println!();
        println!("./configure \\");
        for (i, flag) in flags.iter().enumerate() {
            let suffix = if i < flags.len() - 1 { " \\" } else { "" };
            println!("    {flag}{suffix}");
        }
        println!();
        log_info!("Pass --execute to run ./configure with these flags.");
        log_info!("Source tree is NOT modified. makedep still generates import libs.");
        return 0;
    }

    run_wine_generators(&wine_path);

    if !wine_path.join("configure").exists() {
        log_info!("No ./configure found, running autoreconf...");
        let ret = crate::clone::run_cmd(&["autoreconf", "-fi"], Some(&wine_path));
        if ret != 0 {
            log_error!("autoreconf failed");
            return 1;
        }
    }

    log_info!("Running ./configure with {} disable flags...", flags.len());
    let flag_refs: Vec<&str> = flags.iter().map(|s| s.as_str()).collect();
    let mut cmd_parts: Vec<&str> = vec!["./configure"];
    cmd_parts.extend_from_slice(&flag_refs);
    let ret = crate::clone::run_cmd(&cmd_parts, Some(&wine_path));
    if ret != 0 {
        log_error!("./configure failed");
        return ret;
    }

    log_info!("Configure complete. Run 'make -j$(nproc)' to build.");
    0
}

// Parse configure.ac for WINE_CONFIG_MAKEFILE entries.
// Returns (dll_names, program_names, win16_module_set).
//
// Matches lines like:
//   WINE_CONFIG_MAKEFILE(dlls/d3d11)
//   WINE_CONFIG_MAKEFILE(programs/wineboot)
//   WINE_CONFIG_MAKEFILE(dlls/avifile.dll16,enable_win16)
fn parse_configure_ac(wine_path: &Path) -> (Vec<String>, Vec<String>, BTreeSet<String>) {
    let configure_ac = wine_path.join("configure.ac");
    let text = match fs::read_to_string(&configure_ac) {
        Ok(t) => t,
        Err(_) => {
            log_error!("No configure.ac in {}", wine_path.display());
            return (Vec::new(), Vec::new(), BTreeSet::new());
        }
    };

    let mut dll_set = BTreeSet::new();
    let mut prog_set = BTreeSet::new();
    let mut win16 = BTreeSet::new();

    let marker = "WINE_CONFIG_MAKEFILE(";
    for line in text.lines() {
        let trimmed = line.trim();
        let Some(start) = trimmed.find(marker) else { continue };
        let rest = &trimmed[start + marker.len()..];
        let Some(close) = rest.find(')') else { continue };
        let inner = &rest[..close];

        let (path_part, is_win16) = if let Some((path, flag)) = inner.split_once(',') {
            (path, flag.trim() == "enable_win16")
        } else {
            (inner, false)
        };

        let Some((category, name)) = path_part.split_once('/') else { continue };
        if name.contains('/') {
            continue;
        }

        let name = name.to_string();
        if is_win16 {
            win16.insert(name.clone());
        }
        match category {
            "dlls" => { dll_set.insert(name); }
            "programs" => { prog_set.insert(name); }
            _ => {}
        }
    }

    (dll_set.into_iter().collect(), prog_set.into_iter().collect(), win16)
}

fn run_wine_generators(wine_path: &Path) {
    // Vulkan header
    let vulkan_h = wine_path.join("include").join("wine").join("vulkan.h");
    let make_vulkan = wine_path.join("dlls").join("winevulkan").join("make_vulkan");
    if !vulkan_h.exists() && make_vulkan.exists() {
        log_info!("Generating wine/vulkan.h...");
        let path_str = make_vulkan.to_string_lossy();
        let cwd = wine_path.join("dlls").join("winevulkan");
        let ret = crate::clone::run_cmd(&["python3", &path_str], Some(&cwd));
        if ret != 0 {
            log_warn!("make_vulkan failed (vulkan-headers installed?)");
        }
    }

    // Syscall tables
    let make_specfiles = wine_path.join("tools").join("make_specfiles");
    if make_specfiles.exists() {
        log_info!("Regenerating syscall tables (make_specfiles)...");
        let path_str = make_specfiles.to_string_lossy();
        crate::clone::run_cmd(&["perl", &path_str], Some(wine_path));
    }

    // Server protocol headers
    let make_requests = wine_path.join("tools").join("make_requests");
    if make_requests.exists() {
        log_info!("Regenerating server protocol headers (make_requests)...");
        let path_str = make_requests.to_string_lossy();
        crate::clone::run_cmd(&["perl", &path_str], Some(wine_path));
    }
}
