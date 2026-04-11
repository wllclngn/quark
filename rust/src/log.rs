// Logging macros: [HH:MM:SS] [LEVEL]   message
// Uses libc::localtime_r for local time without pulling in chrono.
//
// Metrics output (timing, opcode stats, per-thread init) is gated behind
// QUARK_VERBOSE=1.  Use log_verbose!() for these.

use std::sync::OnceLock;

static VERBOSE: OnceLock<bool> = OnceLock::new();

pub fn is_verbose() -> bool {
    *VERBOSE.get_or_init(|| {
        // Environment variable (Steam launch options: QUARK_VERBOSE=1 %command%)
        if std::env::var("QUARK_VERBOSE").is_ok() {
            return true;
        }
        // Flag file written by `./install.py --verbose`. Lives in CACHE_DIR
        // so it survives step_clean (which wipes STEAM_COMPAT_DIR — the
        // previous location — and silently disabled verbose every install).
        if let Ok(home) = std::env::var("HOME") {
            let flag = std::path::Path::new(&home)
                .join(".cache")
                .join("quark")
                .join("verbose_enabled");
            if flag.exists() {
                return true;
            }
        }
        // Legacy fallback: old installs put the flag next to the binary.
        // Kept for one release cycle so freshly-built launchers still see
        // any leftover flag from a pre-fix install.
        if let Ok(exe) = std::env::current_exe() {
            if let Some(dir) = exe.parent() {
                if dir.join("verbose_enabled").exists() {
                    return true;
                }
            }
        }
        false
    })
}

pub fn timestamp() -> [u8; 10] {
    let mut tv: libc::timeval = unsafe { std::mem::zeroed() };
    unsafe { libc::gettimeofday(&mut tv, std::ptr::null_mut()) };
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    unsafe { libc::localtime_r(&tv.tv_sec, &mut tm) };
    let h = tm.tm_hour as u8;
    let m = tm.tm_min as u8;
    let s = tm.tm_sec as u8;
    let mut buf = *b"[00:00:00]";
    buf[1] = b'0' + h / 10;
    buf[2] = b'0' + h % 10;
    buf[4] = b'0' + m / 10;
    buf[5] = b'0' + m % 10;
    buf[7] = b'0' + s / 10;
    buf[8] = b'0' + s % 10;
    buf
}

#[macro_export]
macro_rules! log_info {
    ($($arg:tt)*) => {{
        let ts = $crate::log::timestamp();
        let ts = unsafe { std::str::from_utf8_unchecked(&ts) };
        eprintln!("{ts} [INFO]   {}", format_args!($($arg)*));
    }};
}

#[macro_export]
macro_rules! log_warn {
    ($($arg:tt)*) => {{
        let ts = $crate::log::timestamp();
        let ts = unsafe { std::str::from_utf8_unchecked(&ts) };
        eprintln!("{ts} [WARN]   {}", format_args!($($arg)*));
    }};
}

#[macro_export]
macro_rules! log_error {
    ($($arg:tt)*) => {{
        let ts = $crate::log::timestamp();
        let ts = unsafe { std::str::from_utf8_unchecked(&ts) };
        eprintln!("{ts} [ERROR]  {}", format_args!($($arg)*));
    }};
}

#[macro_export]
macro_rules! log_verbose {
    ($($arg:tt)*) => {{
        if $crate::log::is_verbose() {
            let ts = $crate::log::timestamp();
            let ts = unsafe { std::str::from_utf8_unchecked(&ts) };
            eprintln!("{ts} [INFO]   {}", format_args!($($arg)*));
        }
    }};
}

// trace! macro removed — montauk --trace handles per-request diagnostics now.


pub fn format_with_commas(n: u64) -> String {
    let s = n.to_string();
    let mut result = String::with_capacity(s.len() + s.len() / 3);
    for (i, ch) in s.chars().rev().enumerate() {
        if i > 0 && i % 3 == 0 {
            result.push(',');
        }
        result.push(ch);
    }
    result.chars().rev().collect()
}

// ---------------------------------------------------------------------------
// Prometheus text exposition format writer
// ---------------------------------------------------------------------------

/// Resolve the log directory: ~/.cache/quark/
pub fn log_dir() -> std::path::PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    std::path::PathBuf::from(home).join(".cache").join("quark")
}


// Prometheus generation removed — montauk handles tracing now.
