// triskelion -- lock-free wineserver replacement daemon
//
// Three legs:
//   queue   -- per-thread message queues (shared-memory ring buffers)
//   ntsync  -- sync primitives via /dev/ntsync kernel driver
//   objects -- handle tables, process/thread state

#[macro_use]
extern crate triskelion;

mod slab;
mod protocol;
mod protocol_remap;
mod queue;
mod ntsync;
mod objects;
mod registry;
mod event_loop;
mod ipc;
mod shm;
mod csp_loop;
mod sent_messages;
mod display;
mod intel;
mod com_classes;

use std::sync::atomic::AtomicBool;

pub static SHUTDOWN: AtomicBool = AtomicBool::new(false);

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() >= 2 {
        match args[1].as_str() {
            "-k" | "--kill" => std::process::exit(0),
            "-w" | "--wait" => std::process::exit(0),
            "-f" | "--foreground" | "server" => {}
            _ => {}
        }
    }
    run_server();
}

fn run_server() {
    std::panic::set_hook(Box::new(|info| {
        let log_dir = "/tmp/quark";
        let _ = std::fs::create_dir_all(log_dir);
        let log_path = format!("{log_dir}/daemon_panic.log");
        let bt = std::backtrace::Backtrace::force_capture();
        let msg = format!("[triskelion] PANIC: {info}\n\nBacktrace:\n{bt}\n");
        eprintln!("{msg}");
        let _ = std::fs::write(&log_path, &msg);
    }));

    unsafe { libc::signal(libc::SIGPIPE, libc::SIG_IGN); }

    let socket_path = resolve_socket_path();

    if socket_path.exists() {
        let sock = unsafe {
            libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0)
        };
        if sock >= 0 {
            let mut addr: libc::sockaddr_un = unsafe { std::mem::zeroed() };
            addr.sun_family = libc::AF_UNIX as u16;
            let path_bytes = socket_path.to_str().unwrap_or("").as_bytes();
            let copy_len = path_bytes.len().min(addr.sun_path.len() - 1);
            for i in 0..copy_len {
                addr.sun_path[i] = path_bytes[i] as i8;
            }
            let ret = unsafe {
                libc::connect(
                    sock,
                    &addr as *const _ as *const libc::sockaddr,
                    std::mem::size_of::<libc::sockaddr_un>() as u32,
                )
            };
            unsafe { libc::close(sock); }
            if ret == 0 {
                unsafe { libc::_exit(2); }
            }
        }
    }

    let listener = ipc::create_listener(&socket_path);

    daemonize();

    let log_dir = std::path::Path::new("/tmp/quark");
    let _ = std::fs::create_dir_all(log_dir);
    let _ = std::fs::remove_file(log_dir.join("desktop_ready"));
    let _ = std::fs::write(log_dir.join("daemon.pid"), std::process::id().to_string());
    let log_path = log_dir.join("daemon.log");
    if let Ok(f) = std::fs::File::create(&log_path) {
        use std::os::unix::io::IntoRawFd;
        let fd = f.into_raw_fd();
        unsafe { libc::dup2(fd, 2); libc::close(fd); }
    }

    let sigfd = install_signal_handler();

    log_info!("starting (pid {})", std::process::id());

    let protocol_remap = protocol_remap::detect_and_remap();
    ipc::set_runtime_protocol_version(protocol_remap.version);

    let prefix_hash = compute_prefix_hash();
    let shm = match shm::ShmManager::create(&prefix_hash) {
        Ok(shm) => shm,
        Err(e) => {
            log_error!("FATAL: {e}");
            std::process::exit(1);
        }
    };

    // Read PARALLAX display data if available
    let display_data = display::read_parallax_shm(&prefix_hash);
    if let Some(ref dd) = display_data {
        let (w, h) = dd.primary_resolution();
        log_info!("display: using PARALLAX data — GPU {} {:04x}:{:04x}, primary {}x{}",
            dd.gpu.driver, dd.gpu.pci_vendor, dd.gpu.pci_device, w, h);
    } else {
        log_info!("display: PARALLAX not running, using defaults");
    }

    let (user_sid_str, user_sid) = parse_prefix_sid();

    let mut ev = event_loop::EventLoop::new(listener, sigfd, shm, protocol_remap, user_sid, &user_sid_str);

    // Apply PARALLAX display data to EventLoop (desktop resolution, registry)
    if let Some(dd) = display_data {
        ev.apply_display_data(&dd);
    }

    let listener = ev.take_listener();
    let listener_fd = listener.fd();

    // Death pipe: if QUARK_DEATH_FD is set, the launcher holds the write end.
    // When the launcher exits, we get POLLHUP → graceful shutdown.
    // Pattern from μEmacs ext_host.c / ext_runner.c.
    let death_fd: i32 = std::env::var("QUARK_DEATH_FD")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(-1);
    if death_fd >= 0 {
        log_info!("death pipe: fd={death_fd} (launcher will trigger POLLHUP on exit)");
    }

    log_info!("listening on {}", socket_path.display());

    csp_loop::csp_main(&mut ev, listener_fd, sigfd, death_fd);

    drop(listener);

    // Clean up socket file so next launch doesn't find a stale socket.
    // μEmacs pattern: cleanup resources before exit, set fds to -1.
    if socket_path.exists() {
        let _ = std::fs::remove_file(&socket_path);
        log_info!("removed socket {}", socket_path.display());
    }
    // Remove PID file
    let _ = std::fs::remove_file("/tmp/quark/daemon.pid");

    log_info!("shutting down");
}

fn daemonize() {
    let pid = unsafe { libc::fork() };
    match pid {
        -1 => {
            log_error!("fork failed: {}", std::io::Error::last_os_error());
            std::process::exit(1);
        }
        0 => {
            unsafe { libc::setsid(); }
        }
        _ => {
            unsafe { libc::_exit(0); }
        }
    }
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

    let uid = unsafe { libc::getuid() };
    let base_dir = std::path::PathBuf::from(format!("/tmp/.wine-{uid}"));
    let server_dir = base_dir.join(format!("server-{dev:x}-{ino:x}"));
    use std::os::unix::fs::DirBuilderExt;
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true).mode(0o700);
    if let Err(e) = builder.create(&server_dir) {
        log_error!("Cannot create server dir {}: {e}", server_dir.display());
        std::process::exit(1);
    }

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

fn parse_prefix_sid() -> (String, Vec<u8>) {
    let prefix = std::env::var("WINEPREFIX")
        .unwrap_or_else(|_| {
            let home = std::env::var("HOME").expect("HOME not set");
            format!("{home}/.wine")
        });

    let reg_path = std::path::Path::new(&prefix).join("user.reg");

    let sid_str = std::fs::read_to_string(&reg_path)
        .ok()
        .and_then(|content| {
            for line in content.lines().take(5) {
                if let Some(pos) = line.find("\\\\User\\\\") {
                    return Some(line[pos + 8..].trim().to_string());
                }
            }
            None
        });

    if let Some(sid) = sid_str {
        if let Some(bytes) = sid_string_to_bytes(&sid) {
            log_info!("prefix SID: {sid} ({} bytes)", bytes.len());
            return (sid, bytes);
        }
        log_warn!("failed to parse SID \"{sid}\" from {}", reg_path.display());
    } else {
        log_warn!("no SID found in {} — using fallback", reg_path.display());
    }

    let fallback = "S-1-5-21-0-0-0-1000".to_string();
    let bytes = sid_string_to_bytes(&fallback).unwrap();
    (fallback, bytes)
}

fn sid_string_to_bytes(sid: &str) -> Option<Vec<u8>> {
    let parts: Vec<&str> = sid.split('-').collect();
    if parts.len() < 4 || parts[0] != "S" { return None; }
    let revision: u8 = parts[1].parse().ok()?;
    let authority: u64 = parts[2].parse().ok()?;
    let sub_authorities: Vec<u32> = parts[3..].iter()
        .map(|s| s.parse::<u32>())
        .collect::<Result<Vec<_>, _>>()
        .ok()?;

    let mut bytes = Vec::with_capacity(8 + sub_authorities.len() * 4);
    bytes.push(revision);
    bytes.push(sub_authorities.len() as u8);
    bytes.extend_from_slice(&(authority as u64).to_be_bytes()[2..8]);
    for &sub in &sub_authorities {
        bytes.extend_from_slice(&sub.to_le_bytes());
    }
    Some(bytes)
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
