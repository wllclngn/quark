// Logging macros: [HH:MM:SS] [LEVEL]   message
// Uses libc::localtime_r for local time without pulling in chrono.
//
// Metrics output (timing, opcode stats, per-thread init) is gated behind
// AMPHETAMINE_VERBOSE=1.  Use log_verbose!() for these.

use std::sync::OnceLock;

static VERBOSE: OnceLock<bool> = OnceLock::new();

pub fn is_verbose() -> bool {
    *VERBOSE.get_or_init(|| {
        // Environment variable (Steam launch options: AMPHETAMINE_VERBOSE=1 %command%)
        if std::env::var("AMPHETAMINE_VERBOSE").is_ok() {
            return true;
        }
        // Flag file written by ./install.py --verbose
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

// ---------------------------------------------------------------------------
// Prometheus text exposition format writer
// ---------------------------------------------------------------------------

/// Timestamp for log filenames: "YYYYMMDD-HHMMSS" (matches PANDEMONIUM convention)
pub fn filename_timestamp() -> String {
    let mut tv: libc::timeval = unsafe { std::mem::zeroed() };
    unsafe { libc::gettimeofday(&mut tv, std::ptr::null_mut()) };
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    unsafe { libc::localtime_r(&tv.tv_sec, &mut tm) };
    format!(
        "{:04}{:02}{:02}-{:02}{:02}{:02}",
        tm.tm_year + 1900, tm.tm_mon + 1, tm.tm_mday,
        tm.tm_hour, tm.tm_min, tm.tm_sec,
    )
}

/// Resolve the log directory: ~/.cache/amphetamine/
pub fn log_dir() -> std::path::PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    std::path::PathBuf::from(home).join(".cache").join("amphetamine")
}

/// Epoch milliseconds for the scrape timestamp header.
pub fn epoch_ms() -> u64 {
    let mut tv: libc::timeval = unsafe { std::mem::zeroed() };
    unsafe { libc::gettimeofday(&mut tv, std::ptr::null_mut()) };
    (tv.tv_sec as u64) * 1000 + (tv.tv_usec as u64) / 1000
}

// ---------------------------------------------------------------------------
// System info collectors (zero external dependencies)
// ---------------------------------------------------------------------------

pub fn kernel_version() -> String {
    let mut uts: libc::utsname = unsafe { std::mem::zeroed() };
    if unsafe { libc::uname(&mut uts) } == 0 {
        let release = unsafe { std::ffi::CStr::from_ptr(uts.release.as_ptr()) };
        release.to_string_lossy().into_owned()
    } else {
        "unknown".into()
    }
}

pub fn cpu_count() -> u64 {
    let n = unsafe { libc::sysconf(libc::_SC_NPROCESSORS_ONLN) };
    if n > 0 { n as u64 } else { 0 }
}

pub fn total_ram_bytes() -> u64 {
    if let Ok(contents) = std::fs::read_to_string("/proc/meminfo") {
        if let Some(line) = contents.lines().next() {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() >= 2 {
                if let Ok(kb) = parts[1].parse::<u64>() {
                    return kb * 1024;
                }
            }
        }
    }
    0
}

pub fn gpu_vendor() -> &'static str {
    if std::path::Path::new("/proc/driver/nvidia/version").exists() {
        "nvidia"
    } else if std::path::Path::new("/sys/module/amdgpu").exists() {
        "amd"
    } else if std::path::Path::new("/sys/module/i915").exists() {
        "intel"
    } else {
        "unknown"
    }
}

pub fn distro_name() -> String {
    if let Ok(contents) = std::fs::read_to_string("/etc/os-release") {
        for line in contents.lines() {
            if let Some(val) = line.strip_prefix("PRETTY_NAME=") {
                return val.trim_matches('"').to_string();
            }
        }
    }
    "unknown".into()
}

// ---------------------------------------------------------------------------
// PromWriter — Prometheus text exposition format builder
// ---------------------------------------------------------------------------

pub struct PromWriter {
    buf: String,
}

impl PromWriter {
    pub fn new() -> Self {
        Self { buf: String::with_capacity(8192) }
    }

    pub fn timestamp_header(&mut self) {
        let ms = epoch_ms();
        self.buf.push_str("# amphetamine_scrape_timestamp_ms ");
        self.buf.push_str(&ms.to_string());
        self.buf.push('\n');
    }

    pub fn header(&mut self, name: &str, help: &str, metric_type: &str) {
        self.buf.push_str("# HELP ");
        self.buf.push_str(name);
        self.buf.push(' ');
        self.buf.push_str(help);
        self.buf.push('\n');
        self.buf.push_str("# TYPE ");
        self.buf.push_str(name);
        self.buf.push(' ');
        self.buf.push_str(metric_type);
        self.buf.push('\n');
    }

    pub fn gauge(&mut self, name: &str, value: u64) {
        self.buf.push_str(name);
        self.buf.push(' ');
        self.buf.push_str(&value.to_string());
        self.buf.push('\n');
    }

    pub fn gauge_labeled(&mut self, name: &str, key: &str, val: &str, value: u64) {
        self.buf.push_str(name);
        self.buf.push('{');
        self.buf.push_str(key);
        self.buf.push_str("=\"");
        self.push_escaped(val);
        self.buf.push_str("\"} ");
        self.buf.push_str(&value.to_string());
        self.buf.push('\n');
    }

    pub fn gauge_labeled2(
        &mut self, name: &str,
        k1: &str, v1: &str,
        k2: &str, v2: &str,
        value: u64,
    ) {
        self.buf.push_str(name);
        self.buf.push('{');
        self.buf.push_str(k1);
        self.buf.push_str("=\"");
        self.push_escaped(v1);
        self.buf.push_str("\",");
        self.buf.push_str(k2);
        self.buf.push_str("=\"");
        self.push_escaped(v2);
        self.buf.push_str("\"} ");
        self.buf.push_str(&value.to_string());
        self.buf.push('\n');
    }

    /// Info-style metric: encodes a string as a label on gauge=1.
    pub fn info(&mut self, name: &str, key: &str, val: &str) {
        self.gauge_labeled(name, key, val, 1);
    }

    pub fn separator(&mut self) {
        self.buf.push('\n');
    }

    fn push_escaped(&mut self, s: &str) {
        for c in s.chars() {
            match c {
                '\\' => self.buf.push_str("\\\\"),
                '"' => self.buf.push_str("\\\""),
                '\n' => self.buf.push_str("\\n"),
                _ => self.buf.push(c),
            }
        }
    }

    /// Write buffer to file and create a "latest" symlink.
    pub fn write_to(
        &self,
        dir: &std::path::Path,
        filename: &str,
        symlink_name: &str,
    ) -> Result<std::path::PathBuf, std::io::Error> {
        std::fs::create_dir_all(dir)?;
        let path = dir.join(filename);
        std::fs::write(&path, &self.buf)?;

        let link = dir.join(symlink_name);
        let _ = std::fs::remove_file(&link);
        let _ = std::os::unix::fs::symlink(filename, &link);

        Ok(path)
    }
}
