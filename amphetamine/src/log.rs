// Logging macros: [HH:MM:SS] [LEVEL]   message
// Uses libc::localtime_r for local time without pulling in chrono.
//
// Metrics output (timing, opcode stats, per-thread init) is gated behind
// AMPHETAMINE_VERBOSE=1.  Use log_verbose!() for these.

use std::sync::OnceLock;

static VERBOSE: OnceLock<bool> = OnceLock::new();

pub fn is_verbose() -> bool {
    *VERBOSE.get_or_init(|| std::env::var("AMPHETAMINE_VERBOSE").is_ok())
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

macro_rules! log_info {
    ($($arg:tt)*) => {{
        let ts = $crate::log::timestamp();
        let ts = unsafe { std::str::from_utf8_unchecked(&ts) };
        eprintln!("{ts} [INFO]   {}", format_args!($($arg)*));
    }};
}

macro_rules! log_warn {
    ($($arg:tt)*) => {{
        let ts = $crate::log::timestamp();
        let ts = unsafe { std::str::from_utf8_unchecked(&ts) };
        eprintln!("{ts} [WARN]   {}", format_args!($($arg)*));
    }};
}

macro_rules! log_error {
    ($($arg:tt)*) => {{
        let ts = $crate::log::timestamp();
        let ts = unsafe { std::str::from_utf8_unchecked(&ts) };
        eprintln!("{ts} [ERROR]  {}", format_args!($($arg)*));
    }};
}

macro_rules! log_verbose {
    ($($arg:tt)*) => {{
        if $crate::log::is_verbose() {
            let ts = $crate::log::timestamp();
            let ts = unsafe { std::str::from_utf8_unchecked(&ts) };
            eprintln!("{ts} [INFO]   {}", format_args!($($arg)*));
        }
    }};
}

#[allow(unused_macros)]
macro_rules! log_debug {
    ($($arg:tt)*) => {{
        let ts = $crate::log::timestamp();
        let ts = unsafe { std::str::from_utf8_unchecked(&ts) };
        eprintln!("{ts} [DEBUG]  {}", format_args!($($arg)*));
    }};
}

#[allow(unused_imports)]
pub(crate) use log_info;
#[allow(unused_imports)]
pub(crate) use log_warn;
#[allow(unused_imports)]
pub(crate) use log_error;
#[allow(unused_imports)]
pub(crate) use log_verbose;
#[allow(unused_imports)]
pub(crate) use log_debug;

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
