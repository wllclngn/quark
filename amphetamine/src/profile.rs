// Profiling commands.
// Wraps strace and perf for syscall analysis of Wine/game processes.

use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

pub fn run_profile(app_id: &str, game_name: Option<&str>) -> i32 {
    let default_name;
    let name = match game_name {
        Some(n) => n,
        None => {
            default_name = format!("game_{app_id}");
            &default_name
        }
    };
    let out_dir = PathBuf::from(crate::gaming::LOG_DIR).join(name);
    fs::create_dir_all(&out_dir).ok();

    log_info!("Profiling app {app_id} ({name})");
    log_info!("Output: {}", out_dir.display());

    let strace_log = out_dir.join("strace_raw.log");
    log_info!("Launching game under strace (120s capture)...");
    log_info!("Start the game in Steam, play for 2 minutes, then exit.");

    let strace_log_str = strace_log.to_string_lossy().to_string();
    let steam_url = format!("steam://rungameid/{app_id}");
    crate::clone::run_cmd(&[
        "strace", "-f", "-T", "-tt",
        "-e", "trace=file,process,network,memory,ipc,signal",
        "-o", &strace_log_str,
        "timeout", "120",
        "steam", &steam_url,
    ], None);

    if strace_log.exists() && strace_log.metadata().map(|m| m.len() > 0).unwrap_or(false) {
        generate_strace_summary(&strace_log, &out_dir, "strace");
    } else {
        log_warn!("No strace data captured");
    }

    log_info!("Results in {}/", out_dir.display());
    0
}

pub fn run_profile_attach(label: Option<&str>) -> i32 {
    let dir_name = label.unwrap_or("attached");
    let out_dir = PathBuf::from(crate::gaming::LOG_DIR).join(dir_name);
    fs::create_dir_all(&out_dir).ok();

    // Find wineserver PID
    let (ret, stdout, _) = crate::clone::run_cmd_capture(&["pgrep", "-x", "wineserver"]);
    if ret != 0 {
        log_error!("No wineserver found. Launch a game first.");
        return 1;
    }
    let wineserver_pid = match stdout.trim().lines().next() {
        Some(pid) => pid.to_string(),
        None => {
            log_error!("No wineserver PID found");
            return 1;
        }
    };
    log_info!("Found wineserver PID: {wineserver_pid}");

    // Find wine game process
    let (ret, stdout, _) = crate::clone::run_cmd_capture(&["pgrep", "-f", r"wine.*\.exe"]);
    let wine_pid = if ret == 0 {
        stdout.trim().lines().next().map(|s| s.to_string())
    } else {
        None
    };
    if let Some(ref pid) = wine_pid {
        log_info!("Found Wine game PID: {pid}");
    }

    let duration_secs = 10u64;
    log_info!("Capturing {duration_secs}s of steady-state activity...");

    // Parallel strace via std::thread
    let wineserver_log = out_dir.join("wineserver_strace.log");
    let mut handles = Vec::new();

    {
        let pid = wineserver_pid.clone();
        let log_path = wineserver_log.clone();
        handles.push(std::thread::spawn(move || {
            trace_pid(&pid, &log_path, duration_secs);
        }));
    }

    let game_log = out_dir.join("game_strace.log");
    if let Some(ref pid) = wine_pid {
        let pid = pid.clone();
        let log_path = game_log.clone();
        handles.push(std::thread::spawn(move || {
            trace_pid(&pid, &log_path, duration_secs);
        }));
    }

    for h in handles {
        let _ = h.join();
    }

    // Generate summaries
    if wineserver_log.exists() {
        generate_strace_summary(&wineserver_log, &out_dir, "wineserver");
    }
    if wine_pid.is_some() && game_log.exists() {
        generate_strace_summary(&game_log, &out_dir, "game");
    }

    // Perf profile if available
    let (ret, _, _) = crate::clone::run_cmd_capture(&["which", "perf"]);
    if ret == 0 {
        log_info!("Capturing perf profile of wineserver...");
        let perf_data = out_dir.join("wineserver_perf.data");
        let perf_data_str = perf_data.to_string_lossy().to_string();
        let dur_str = duration_secs.to_string();
        crate::clone::run_cmd(&[
            "sudo", "perf", "record", "-g",
            "-p", &wineserver_pid,
            "-o", &perf_data_str,
            "--", "sleep", &dur_str,
        ], None);

        if perf_data.exists() {
            let perf_report = out_dir.join("wineserver_perf_report.txt");
            let (ret, stdout, _) = crate::clone::run_cmd_capture(&[
                "sudo", "perf", "report",
                "-i", &perf_data_str, "--stdio",
            ]);
            if ret == 0 {
                let _ = fs::write(&perf_report, &stdout);
                log_info!("Perf report: {}", perf_report.display());
            }
        }
    }

    log_info!("Results in {}/", out_dir.display());
    0
}

fn trace_pid(pid: &str, log_path: &Path, secs: u64) {
    let log_str = log_path.to_string_lossy();
    let mut child = match Command::new("strace")
        .args(["-f", "-T", "-tt", "-p", pid, "-e", "trace=all", "-o", &*log_str])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            log_error!("Failed to start strace on PID {pid}: {e}");
            return;
        }
    };

    std::thread::sleep(std::time::Duration::from_secs(secs));

    unsafe {
        libc::kill(child.id() as i32, libc::SIGINT);
    }
    let _ = child.wait();
}

fn generate_strace_summary(log_file: &Path, out_dir: &Path, prefix: &str) {
    let file_size = log_file.metadata().map(|m| m.len()).unwrap_or(0);
    log_info!("Parsing {} ({} KB)...",
        log_file.file_name().unwrap_or_default().to_string_lossy(),
        file_size / 1024);

    let f = match fs::File::open(log_file) {
        Ok(f) => f,
        Err(e) => {
            log_error!("Cannot open {}: {e}", log_file.display());
            return;
        }
    };

    let mut reader = BufReader::with_capacity(64 * 1024, f);
    let mut syscall_counts: HashMap<String, u64> = HashMap::new();
    let mut futex_count: u64 = 0;
    let mut socket_count: u64 = 0;
    let mut recvmsg_count: u64 = 0;
    let mut sendmsg_count: u64 = 0;
    let mut line_buf = String::new();

    loop {
        line_buf.clear();
        match reader.read_line(&mut line_buf) {
            Ok(0) => break,
            Ok(_) => {}
            Err(_) => break,
        }

        // strace format: PID HH:MM:SS.uuuuuu syscall_name(...)
        for part in line_buf.split_whitespace().skip(1) {
            if let Some(paren) = part.find('(') {
                let syscall = &part[..paren];
                if !syscall.is_empty() && syscall.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
                    *syscall_counts.entry(syscall.to_string()).or_insert(0) += 1;
                }
                break;
            }
        }

        if line_buf.contains("futex") {
            futex_count += 1;
        }
        if line_buf.contains("AF_UNIX") || line_buf.contains("/tmp/.wine") {
            socket_count += 1;
        }
        if line_buf.contains("recvmsg(") {
            recvmsg_count += 1;
        }
        if line_buf.contains("sendmsg(") {
            sendmsg_count += 1;
        }
    }

    // Sort by count descending
    let mut sorted: Vec<(&str, u64)> = syscall_counts.iter().map(|(k, &v)| (k.as_str(), v)).collect();
    sorted.sort_by(|a, b| b.1.cmp(&a.1));

    // Write syscall counts file
    let counts_file = out_dir.join(format!("{prefix}_syscall_counts.txt"));
    if let Ok(mut f) = fs::File::create(&counts_file) {
        for (syscall, count) in &sorted {
            let _ = writeln!(f, "{count:>8}  {syscall}");
        }
    }

    let total: u64 = syscall_counts.values().sum();
    log_info!("Syscall summary: {}", counts_file.display());
    log_info!("  Total syscalls: {}", crate::log::format_with_commas(total));
    log_info!("  Unique syscalls: {}", syscall_counts.len());
    log_info!("  Futex calls: {}", crate::log::format_with_commas(futex_count));
    log_info!("  Wineserver socket traffic: {}", crate::log::format_with_commas(socket_count));
    log_info!("  Wineserver RPCs: recvmsg={}, sendmsg={}",
        crate::log::format_with_commas(recvmsg_count),
        crate::log::format_with_commas(sendmsg_count));

    log_info!("  Top 10 syscalls:");
    for (syscall, count) in sorted.iter().take(10) {
        println!("    {:>10}  {syscall}", crate::log::format_with_commas(*count));
    }
}

pub fn run_profile_compare(dir_a: &str, dir_b: &str) -> i32 {
    let a = PathBuf::from(dir_a);
    let b = PathBuf::from(dir_b);

    log_info!("Comparing profiles:");
    log_info!("  A: {}", a.display());
    log_info!("  B: {}", b.display());

    let key_syscalls = ["recvmsg", "sendmsg", "futex", "poll", "epoll_wait",
                        "read", "write", "select", "ppoll"];

    for prefix in &["wineserver", "game"] {
        let file_a = a.join(format!("{prefix}_syscall_counts.txt"));
        let file_b = b.join(format!("{prefix}_syscall_counts.txt"));

        if !file_a.exists() || !file_b.exists() {
            continue;
        }

        let counts_a = load_syscall_counts(&file_a);
        let counts_b = load_syscall_counts(&file_b);

        let total_a: u64 = counts_a.values().sum();
        let total_b: u64 = counts_b.values().sum();

        log_info!("");
        log_info!("  {prefix} comparison:");
        println!("    {:>20}  {:>10}  {:>10}  {:>10}", "syscall", "A", "B", "delta");
        println!("    {:>20}  {:>10}  {:>10}  {:>10}",
            "TOTAL",
            crate::log::format_with_commas(total_a),
            crate::log::format_with_commas(total_b),
            format_delta(total_a, total_b));

        for &sc in &key_syscalls {
            let va = counts_a.get(sc).copied().unwrap_or(0);
            let vb = counts_b.get(sc).copied().unwrap_or(0);
            if va == 0 && vb == 0 { continue; }
            println!("    {:>20}  {:>10}  {:>10}  {:>10}",
                sc,
                crate::log::format_with_commas(va),
                crate::log::format_with_commas(vb),
                format_delta(va, vb));
        }
    }

    0
}

fn load_syscall_counts(path: &Path) -> HashMap<String, u64> {
    let mut map = HashMap::new();
    if let Ok(content) = fs::read_to_string(path) {
        for line in content.lines() {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() == 2 {
                if let Ok(count) = parts[0].parse::<u64>() {
                    map.insert(parts[1].to_string(), count);
                }
            }
        }
    }
    map
}

fn format_delta(a: u64, b: u64) -> String {
    if a == b { return "0".to_string(); }
    let diff = b as i64 - a as i64;
    let pct = if a > 0 { (diff as f64 / a as f64 * 100.0) as i64 } else { 0 };
    format!("{diff:+} ({pct:+}%)")
}

// WINEDEBUG=+server opcode trace parser
//
// Parses traces captured with AMPHETAMINE_TRACE_OPCODES=1 (which sets
// WINEDEBUG=+server). Extracts opcode names, counts frequencies,
// and identifies hot-path vs startup-only opcodes.
//
// Trace line formats:
//   Request: "0024: init_first_thread( unix_pid=141963, ... )"
//   Reply:   "0024: init_first_thread() = 0 { pid=0020, ... }"
//   Error:   "0024: create_esync() = NOT_IMPLEMENTED { ... }"
//   Misc:    "0024: *fd* 0004 -> 25"
//
// Request lines have args inside parens. Reply lines have "() = ".
// We count request lines (those with non-empty args or first occurrence).

pub fn run_profile_opcodes(trace_file: &str) -> i32 {
    let path = Path::new(trace_file);
    if !path.exists() {
        log_error!("Trace file not found: {trace_file}");
        log_error!("Capture a trace first:");
        log_error!("  touch /tmp/amphetamine/TRACE_OPCODES && steam steam://rungameid/<app_id>");
        return 1;
    }

    let file_size = path.metadata().map(|m| m.len()).unwrap_or(0);
    log_info!("Parsing opcode trace: {trace_file} ({} MB)", file_size / (1024 * 1024));

    let f = match fs::File::open(path) {
        Ok(f) => f,
        Err(e) => {
            log_error!("Cannot open {trace_file}: {e}");
            return 1;
        }
    };

    let mut reader = BufReader::with_capacity(256 * 1024, f);
    let mut opcode_counts: HashMap<String, u64> = HashMap::new();
    let mut error_counts: HashMap<String, HashMap<String, u64>> = HashMap::new();
    let mut total_calls: u64 = 0;
    let mut total_errors: u64 = 0;
    let mut line_buf = String::new();
    let mut line_number: u64 = 0;

    // Phase tracking: first N lines are startup, rest is steady-state
    // We use line count as proxy since WINEDEBUG=+server has no timestamps
    let mut startup_counts: HashMap<String, u64> = HashMap::new();
    let mut steady_counts: HashMap<String, u64> = HashMap::new();
    let mut first_call_line: Option<u64> = None;

    // Thread tracking
    let mut thread_ids: std::collections::HashSet<String> = std::collections::HashSet::new();

    loop {
        line_buf.clear();
        match reader.read_line(&mut line_buf) {
            Ok(0) => break,
            Ok(_) => {}
            Err(_) => break,
        }
        line_number += 1;

        // Format: "XXXX: opcode_name( args )" for requests
        //         "XXXX: opcode_name() = STATUS { fields }" for replies
        //         "XXXX: *fd* ..." for fd passing (skip)

        // Must start with hex TID + ": "
        let colon_space = match line_buf.find(": ") {
            Some(idx) => idx,
            None => continue,
        };
        let tid = &line_buf[..colon_space];
        if tid.is_empty() || !tid.chars().all(|c| c.is_ascii_hexdigit()) {
            continue;
        }
        thread_ids.insert(tid.to_string());

        let after_tid = &line_buf[colon_space + 2..];

        // Skip *fd* lines and other non-opcode lines
        if after_tid.starts_with('*') {
            continue;
        }

        // Extract opcode name (everything before first '(')
        let paren = match after_tid.find('(') {
            Some(idx) => idx,
            None => continue,
        };
        let opcode = after_tid[..paren].trim();
        if opcode.is_empty() || !opcode.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
            continue;
        }

        // Distinguish request vs reply:
        // Request: "opcode( args )" -- has content between parens
        // Reply:   "opcode() = STATUS" -- empty parens followed by " = "
        let after_paren = &after_tid[paren + 1..];
        let is_reply = after_paren.starts_with(')');

        if is_reply {
            // Parse error status: "() = STATUS_NAME" or "() = 0"
            if let Some(eq_idx) = after_paren.find("= ") {
                let status = after_paren[eq_idx + 2..].trim();
                let status_name = match status.find(|c: char| c == ' ' || c == '{') {
                    Some(end) => status[..end].trim(),
                    None => status,
                };
                if status_name != "0" && !status_name.is_empty() {
                    *error_counts
                        .entry(opcode.to_string())
                        .or_default()
                        .entry(status_name.to_string())
                        .or_insert(0) += 1;
                    total_errors += 1;
                }
            }
        } else {
            // Request line -- count it
            let opcode = opcode.to_string();
            *opcode_counts.entry(opcode.clone()).or_insert(0) += 1;
            total_calls += 1;

            if first_call_line.is_none() {
                first_call_line = Some(line_number);
            }

            // Phase tracking: estimate startup as first 10% of calls
            // (heuristic; real phase detection would need timestamps)
        }
    }

    // Phase split: use the call sequence to identify startup vs steady-state.
    // Re-read the file, splitting at the point where per-opcode rates stabilize.
    // Simpler heuristic: first 20K calls are startup, rest is steady-state.
    let startup_call_limit = 20_000u64;
    {
        let f = fs::File::open(path).unwrap();
        let mut reader2 = BufReader::with_capacity(256 * 1024, f);
        let mut call_idx: u64 = 0;
        let mut line_buf2 = String::new();

        loop {
            line_buf2.clear();
            match reader2.read_line(&mut line_buf2) {
                Ok(0) => break,
                Ok(_) => {}
                Err(_) => break,
            }
            let colon_space = match line_buf2.find(": ") {
                Some(idx) => idx,
                None => continue,
            };
            let tid = &line_buf2[..colon_space];
            if tid.is_empty() || !tid.chars().all(|c| c.is_ascii_hexdigit()) {
                continue;
            }
            let after_tid = &line_buf2[colon_space + 2..];
            if after_tid.starts_with('*') { continue; }
            let paren = match after_tid.find('(') {
                Some(idx) => idx,
                None => continue,
            };
            let opcode = after_tid[..paren].trim();
            if opcode.is_empty() || !opcode.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
                continue;
            }
            let after_paren = &after_tid[paren + 1..];
            if after_paren.starts_with(')') { continue; } // reply

            call_idx += 1;
            if call_idx <= startup_call_limit {
                *startup_counts.entry(opcode.to_string()).or_insert(0) += 1;
            } else {
                *steady_counts.entry(opcode.to_string()).or_insert(0) += 1;
            }
        }
    }

    if total_calls == 0 {
        log_warn!("No wineserver calls found in trace");
        log_warn!("Expected WINEDEBUG=+server format: 'XXXX: opcode_name( ... )'");
        return 1;
    }

    // Sort by count descending
    let mut sorted: Vec<(&str, u64)> = opcode_counts.iter().map(|(k, &v)| (k.as_str(), v)).collect();
    sorted.sort_by(|a, b| b.1.cmp(&a.1));

    // Summary
    log_info!("Wineserver opcode profile:");
    log_info!("  Total requests: {}", crate::log::format_with_commas(total_calls));
    log_info!("  Total errors: {}", crate::log::format_with_commas(total_errors));
    log_info!("  Unique opcodes: {}", opcode_counts.len());
    log_info!("  Wine threads: {}", thread_ids.len());
    log_info!("  Trace lines: {}", crate::log::format_with_commas(line_number));

    // Full opcode table
    let out_dir = PathBuf::from(crate::gaming::LOG_DIR);
    fs::create_dir_all(&out_dir).ok();
    let report_file = out_dir.join("opcode_profile.txt");

    if let Ok(mut f) = fs::File::create(&report_file) {
        let _ = writeln!(f, "Wineserver Opcode Profile");
        let _ = writeln!(f, "Trace: {trace_file}");
        let _ = writeln!(f, "Total requests: {total_calls}");
        let _ = writeln!(f, "Total errors: {total_errors}");
        let _ = writeln!(f, "Unique opcodes: {}", opcode_counts.len());
        let _ = writeln!(f, "Threads: {}", thread_ids.len());
        let _ = writeln!(f, "");

        // Ranked table
        let _ = writeln!(f, "{:>8}  {:>6}  {:>8}  {:>8}  {}",
            "count", "pct", "startup", "steady", "opcode");
        for (opcode, count) in &sorted {
            let pct = *count as f64 / total_calls as f64 * 100.0;
            let startup = startup_counts.get(*opcode).copied().unwrap_or(0);
            let steady = steady_counts.get(*opcode).copied().unwrap_or(0);
            let _ = writeln!(f, "{count:>8}  {pct:>5.1}%  {startup:>8}  {steady:>8}  {opcode}");
        }

        // Errors section
        if !error_counts.is_empty() {
            let _ = writeln!(f, "");
            let _ = writeln!(f, "Errors (non-zero return codes):");
            let mut err_sorted: Vec<_> = error_counts.iter().collect();
            err_sorted.sort_by(|a, b| {
                let total_a: u64 = a.1.values().sum();
                let total_b: u64 = b.1.values().sum();
                total_b.cmp(&total_a)
            });
            for (opcode, errors) in &err_sorted {
                for (err, count) in *errors {
                    let _ = writeln!(f, "  {opcode}: {err} x{count}");
                }
            }
        }
    }

    // Print top 30 to terminal
    log_info!("");
    log_info!("  {:>8}  {:>6}  {}",
        "count", "pct", "opcode");
    for (opcode, count) in sorted.iter().take(30) {
        let pct = *count as f64 / total_calls as f64 * 100.0;
        println!("  {:>8}  {:>5.1}%  {opcode}",
            crate::log::format_with_commas(*count), pct);
    }

    // Phase breakdown
    if !startup_counts.is_empty() || !steady_counts.is_empty() {
        let startup_total: u64 = startup_counts.values().sum();
        let steady_total: u64 = steady_counts.values().sum();

        log_info!("");
        log_info!("Phase breakdown (startup = first {} requests):",
            crate::log::format_with_commas(startup_call_limit));
        log_info!("  Startup: {} requests", crate::log::format_with_commas(startup_total));
        log_info!("  Steady-state: {} requests", crate::log::format_with_commas(steady_total));

        // Show opcodes that are startup-only
        let startup_only: Vec<(&str, u64)> = startup_counts.iter()
            .filter(|(k, _)| !steady_counts.contains_key(k.as_str()))
            .map(|(k, &v)| (k.as_str(), v))
            .collect();
        if !startup_only.is_empty() {
            log_info!("");
            log_info!("  Startup-only opcodes ({}):", startup_only.len());
            let mut so = startup_only;
            so.sort_by(|a, b| b.1.cmp(&a.1));
            for (opcode, count) in so.iter().take(20) {
                println!("    {:>6}  {opcode}", crate::log::format_with_commas(*count));
            }
        }

        // Hot-path opcodes (steady-state top 20)
        let mut steady_sorted: Vec<(&str, u64)> = steady_counts.iter()
            .map(|(k, &v)| (k.as_str(), v))
            .collect();
        steady_sorted.sort_by(|a, b| b.1.cmp(&a.1));
        if !steady_sorted.is_empty() {
            log_info!("");
            log_info!("  Hot-path opcodes (steady-state top 20):");
            for (opcode, count) in steady_sorted.iter().take(20) {
                println!("    {:>8}  {:>5.1}%  {opcode}",
                    crate::log::format_with_commas(*count),
                    *count as f64 / steady_total as f64 * 100.0);
            }
        }
    }

    // Top errors
    if !error_counts.is_empty() {
        log_info!("");
        log_info!("  Top errors:");
        let mut err_flat: Vec<(String, u64)> = Vec::new();
        for (opcode, errors) in &error_counts {
            for (err, count) in errors {
                err_flat.push((format!("{opcode} -> {err}"), *count));
            }
        }
        err_flat.sort_by(|a, b| b.1.cmp(&a.1));
        for (desc, count) in err_flat.iter().take(15) {
            println!("    {:>6}  {desc}", crate::log::format_with_commas(*count));
        }
    }

    log_info!("");
    log_info!("Full report: {}", report_file.display());

    // Clean up trace flag file
    let _ = fs::remove_file("/tmp/amphetamine/TRACE_OPCODES");

    0
}
