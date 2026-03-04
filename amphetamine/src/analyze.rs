// DLL surface area analysis.
// Categorizes Wine DLLs into keep/remove, counts LOC, checks dependencies.

use std::fs;
use std::io::{BufRead, BufReader};
use std::path::Path;

pub fn run() -> i32 {
    let wine_dir = crate::clone::ensure_wine_clone();
    let dlls_dir = Path::new(wine_dir).join("dlls");

    let mut all_dlls: Vec<String> = Vec::new();
    if let Ok(entries) = fs::read_dir(&dlls_dir) {
        for entry in entries.flatten() {
            if entry.path().is_dir() {
                all_dlls.push(entry.file_name().to_string_lossy().into_owned());
            }
        }
    }
    all_dlls.sort();

    let mut keep: Vec<&str> = Vec::new();
    let mut remove: Vec<&str> = Vec::new();
    for dll in &all_dlls {
        if crate::gaming::is_gaming_dll(dll) {
            keep.push(dll);
        } else {
            remove.push(dll);
        }
    }

    println!();
    log_info!("Total DLL directories: {}", all_dlls.len());
    log_info!("Gaming subset (keep):  {}", keep.len());
    log_info!("Non-gaming (remove):   {}", remove.len());

    // LOC analysis -- compute keep DLL sizes in the same pass
    log_info!("Counting lines of code (this takes a moment)...");
    let mut keep_loc: u64 = 0;
    let mut dll_sizes: Vec<(&str, u64)> = Vec::new();

    for dll in &keep {
        let loc = count_loc_in_dir(&dlls_dir.join(dll));
        keep_loc += loc;
        if loc > 0 {
            dll_sizes.push((dll, loc));
        }
    }

    let mut remove_loc: u64 = 0;
    for dll in &remove {
        remove_loc += count_loc_in_dir(&dlls_dir.join(dll));
    }

    let total_loc = keep_loc + remove_loc;
    let reduction = if total_loc > 0 { (remove_loc * 100) / total_loc } else { 0 };

    println!();
    log_info!("Code to keep:    {:>10} LOC", crate::log::format_with_commas(keep_loc));
    log_info!("Code to remove:  {:>10} LOC", crate::log::format_with_commas(remove_loc));
    log_info!("Reduction:       {reduction}%");

    // Top gaming DLLs by size
    println!();
    log_info!("Top gaming DLLs by size:");
    dll_sizes.sort_by(|a, b| b.1.cmp(&a.1));
    for (dll, loc) in dll_sizes.iter().take(20) {
        println!("  {:<25} {:>8} LOC", dll, crate::log::format_with_commas(*loc));
    }

    // Dependency check
    println!();
    log_info!("Checking for surprise dependencies...");
    let mut surprises: Vec<(String, String)> = Vec::new();

    for dll in &keep {
        let makefile = dlls_dir.join(dll).join("Makefile.in");
        if !makefile.exists() {
            continue;
        }
        if let Ok(text) = fs::read_to_string(&makefile) {
            for line in text.lines() {
                if !line.starts_with("IMPORTS") && !line.starts_with("DELAYIMPORTS") {
                    continue;
                }
                if let Some((_, rhs)) = line.split_once('=') {
                    for imp in rhs.split_whitespace() {
                        if imp.starts_with('$') {
                            continue;
                        }
                        if crate::gaming::is_gaming_dll(imp) || crate::gaming::is_infra_dll(imp) {
                            continue;
                        }
                        surprises.push((dll.to_string(), imp.to_string()));
                    }
                }
            }
        }
    }

    surprises.sort();
    surprises.dedup();

    if !surprises.is_empty() {
        log_warn!("Found {} surprise dependencies:", surprises.len());
        for (dll, imp) in &surprises {
            println!("  {dll} -> {imp}");
        }
    } else {
        log_info!("No surprise dependencies found");
    }

    println!();
    0
}

fn count_loc_in_dir(dir: &Path) -> u64 {
    let mut total = 0u64;
    walk_c_files(dir, &mut |path| {
        if path.to_string_lossy().contains("/tests/") {
            return;
        }
        if let Ok(f) = fs::File::open(path) {
            total += BufReader::new(f).lines().count() as u64;
        }
    });
    total
}

fn walk_c_files(dir: &Path, cb: &mut dyn FnMut(&Path)) {
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            walk_c_files(&path, cb);
        } else if path.extension().and_then(|e| e.to_str()) == Some("c") {
            cb(&path);
        }
    }
}
