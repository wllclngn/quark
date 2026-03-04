// Project status display.

use std::fs;
use std::path::Path;

pub fn run() -> i32 {
    println!();
    println!("  amphetamine -- performance-focused Wine fork for Linux gaming");
    println!();

    println!("  Rust layer:    v{}", env!("CARGO_PKG_VERSION"));

    let wine_dir = Path::new(crate::gaming::WINE_CLONE_DIR);
    if wine_dir.exists() && wine_dir.join("dlls").exists() {
        let dll_count = fs::read_dir(wine_dir.join("dlls"))
            .map(|e| e.filter_map(|e| e.ok()).filter(|e| e.path().is_dir()).count())
            .unwrap_or(0);
        let wine_ver = fs::read_to_string(wine_dir.join("VERSION"))
            .map(|s| s.trim().to_string())
            .unwrap_or_else(|_| "unknown".into());
        println!("  Wine clone:    {} ({wine_ver}, {dll_count} DLLs)", wine_dir.display());
    } else {
        println!("  Wine clone:    NOT CLONED (run triskelion clone)");
    }

    let log_dir = Path::new(crate::gaming::LOG_DIR);
    if log_dir.exists() {
        let count = count_files_recursive(log_dir);
        println!("  Logs:          {}/ ({count} file(s))", log_dir.display());
    } else {
        println!("  Logs:          {}/ (not created yet)", log_dir.display());
    }

    println!();
    0
}

fn count_files_recursive(dir: &Path) -> usize {
    let mut count = 0;
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                count += count_files_recursive(&path);
            } else {
                count += 1;
            }
        }
    }
    count
}
