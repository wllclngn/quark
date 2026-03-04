#![allow(dead_code)]

// triskelion -- lock-free wineserver replacement + proton launcher
//
// Multi-mode binary:
//   ./proton <verb> <exe>   Proton launcher (Steam compatibility tool)
//   triskelion package      Package a built Wine tree for Steam
//   triskelion server       Wineserver replacement daemon
//
// Three legs of the server, always spinning:
//   queue   -- per-thread message queues (SPSC ring buffers in shared memory)
//   sync    -- sync primitive arbitration (futex-backed atomics)
//   objects -- handle tables, process/thread state

#[macro_use]
mod log;
mod cli;
mod gaming;
mod clone;
mod status;
mod analyze;
mod configure;
mod profile;
mod launcher;
mod packager;
mod pe_patch;
mod protocol;
mod queue;
mod sync;
mod objects;
mod registry;
mod event_loop;
mod ipc;
mod shm;

use std::sync::atomic::{AtomicBool, Ordering};

static SHUTDOWN: AtomicBool = AtomicBool::new(false);

fn main() {
    match cli::parse_args() {
        cli::Mode::Server => run_server(),
        cli::Mode::Launch { verb, args } => {
            std::process::exit(launcher::run(&verb, &args));
        }
        cli::Mode::Package { wine_dir } => {
            std::process::exit(packager::run(&wine_dir));
        }
        cli::Mode::Status => {
            std::process::exit(status::run());
        }
        cli::Mode::Analyze => {
            std::process::exit(analyze::run());
        }
        cli::Mode::Configure { wine_dir, execute } => {
            std::process::exit(configure::run(&wine_dir, execute));
        }
        cli::Mode::Profile { app_id, game_name } => {
            std::process::exit(profile::run_profile(&app_id, game_name.as_deref()));
        }
        cli::Mode::ProfileAttach { label } => {
            std::process::exit(profile::run_profile_attach(label.as_deref()));
        }
        cli::Mode::ProfileCompare { dir_a, dir_b } => {
            std::process::exit(profile::run_profile_compare(&dir_a, &dir_b));
        }
        cli::Mode::ProfileOpcodes { trace_file } => {
            std::process::exit(profile::run_profile_opcodes(&trace_file));
        }
        cli::Mode::Clone => {
            clone::ensure_wine_clone();
            clone::ensure_proton_clone();
            log_info!("Both clones ready");
        }
    }
}

fn run_server() {
    let sigfd = install_signal_handler();

    eprintln!("[triskelion] starting (pid {})", std::process::id());

    let socket_path = resolve_socket_path();

    let prefix_hash = compute_prefix_hash();
    let shm = shm::ShmManager::create(&prefix_hash);

    let listener = ipc::create_listener(&socket_path);
    let mut ev = event_loop::EventLoop::new(listener, sigfd, shm);

    eprintln!("[triskelion] listening on {}", socket_path.display());

    while !SHUTDOWN.load(Ordering::Relaxed) {
        ev.tick();
    }

    eprintln!("[triskelion] shutting down");
}

fn resolve_socket_path() -> std::path::PathBuf {
    let prefix = std::env::var("WINEPREFIX")
        .unwrap_or_else(|_| {
            let home = std::env::var("HOME").expect("HOME not set");
            format!("{home}/.wine")
        });

    let prefix = std::path::Path::new(&prefix);

    let stat = std::fs::metadata(prefix).expect("WINEPREFIX does not exist");
    use std::os::unix::fs::MetadataExt;
    let dev = stat.dev();
    let ino = stat.ino();

    // Proton uses /tmp/.wine-<uid>/server-<dev>-<ino>/ (not $WINEPREFIX/server-...)
    let uid = unsafe { libc::getuid() };
    let base_dir = std::path::PathBuf::from(format!("/tmp/.wine-{uid}"));
    let server_dir = base_dir.join(format!("server-{dev:x}-{ino:x}"));
    std::fs::create_dir_all(&server_dir).ok();

    server_dir.join("socket")
}

fn compute_prefix_hash() -> String {
    let prefix = std::env::var("WINEPREFIX")
        .unwrap_or_else(|_| {
            let home = std::env::var("HOME").expect("HOME not set");
            format!("{home}/.wine")
        });

    let stat = std::fs::metadata(&prefix).expect("WINEPREFIX does not exist");
    use std::os::unix::fs::MetadataExt;
    format!("{:x}{:x}", stat.dev(), stat.ino())
}

fn install_signal_handler() -> i32 {
    unsafe {
        let mut mask: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut mask);
        libc::sigaddset(&mut mask, libc::SIGTERM);
        libc::sigaddset(&mut mask, libc::SIGINT);
        libc::sigaddset(&mut mask, libc::SIGHUP);
        libc::sigprocmask(libc::SIG_BLOCK, &mask, std::ptr::null_mut());
        libc::signalfd(-1, &mask, libc::SFD_NONBLOCK | libc::SFD_CLOEXEC)
    }
}
