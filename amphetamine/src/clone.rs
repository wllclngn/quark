// Git clone helpers and command execution utilities.

use std::path::Path;
use std::process::Command;

pub fn run_cmd(cmd: &[&str], cwd: Option<&Path>) -> i32 {
    eprintln!(">>> {}", cmd.join(" "));
    let mut c = Command::new(cmd[0]);
    c.args(&cmd[1..]);
    if let Some(dir) = cwd {
        c.current_dir(dir);
    }
    match c.status() {
        Ok(s) => s.code().unwrap_or(1),
        Err(e) => {
            log_error!("Failed to execute {}: {e}", cmd[0]);
            1
        }
    }
}

pub fn run_cmd_capture(cmd: &[&str]) -> (i32, String, String) {
    let mut c = Command::new(cmd[0]);
    c.args(&cmd[1..]);
    match c.output() {
        Ok(out) => {
            let code = out.status.code().unwrap_or(1);
            let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
            let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
            (code, stdout, stderr)
        }
        Err(e) => {
            log_error!("Failed to execute {}: {e}", cmd[0]);
            (1, String::new(), String::new())
        }
    }
}

pub fn ensure_wine_clone() -> &'static str {
    let dir = Path::new(crate::gaming::WINE_CLONE_DIR);
    if dir.exists() && dir.join("dlls").exists() {
        log_info!("Wine clone exists: {}", dir.display());
        return crate::gaming::WINE_CLONE_DIR;
    }

    log_info!("Cloning Valve Wine fork ({})...", crate::gaming::WINE_CLONE_BRANCH);
    let ret = run_cmd(&[
        "git", "clone", "--depth", "1",
        "-b", crate::gaming::WINE_CLONE_BRANCH,
        crate::gaming::WINE_CLONE_URL,
        crate::gaming::WINE_CLONE_DIR,
    ], None);
    if ret != 0 {
        log_error!("Failed to clone Wine");
        std::process::exit(1);
    }
    log_info!("Cloned to {}", crate::gaming::WINE_CLONE_DIR);
    crate::gaming::WINE_CLONE_DIR
}

pub fn ensure_proton_clone() -> &'static str {
    let dir = Path::new(crate::gaming::PROTON_CLONE_DIR);
    if dir.exists() && dir.join("Makefile.in").exists() {
        log_info!("Proton clone exists: {}", dir.display());
        return crate::gaming::PROTON_CLONE_DIR;
    }

    log_info!("Cloning Proton...");
    let ret = run_cmd(&[
        "git", "clone", "--depth", "1",
        crate::gaming::PROTON_CLONE_URL,
        crate::gaming::PROTON_CLONE_DIR,
    ], None);
    if ret != 0 {
        log_error!("Failed to clone Proton");
        std::process::exit(1);
    }
    log_info!("Cloned to {}", crate::gaming::PROTON_CLONE_DIR);
    crate::gaming::PROTON_CLONE_DIR
}
