// PARALLAX — session-root compositor, display driver replacement, and DSR for Linux
//
// Phase 2: Display info daemon
//   Enumerates real display hardware via DRM/KMS, writes structured
//   display info to shared memory for triskelion to read. Optionally
//   launches a child compositor.
//
// Usage:
//   parallax                              # enumerate and write display info
//   parallax -- kwin_wayland              # also launch child compositor
//   parallax --multiplier 2.0 -- sway    # with DSR multiplier
//   parallax --config path.toml -- ...   # custom config

mod config;
mod display_info;
mod output;

use std::os::unix::fs::MetadataExt;

fn main() {
    let config = config::Config::load();

    eprintln!("[PARALLAX] enumerating display hardware");
    let hw = output::enumerate();

    // Report what we found
    for gpu in &hw.gpus {
        eprintln!("[PARALLAX] GPU: {} (PCI {:04x}:{:04x}) bus={}",
            gpu.driver, gpu.pci_vendor, gpu.pci_device, gpu.pci_bus_id);
    }
    for conn in &hw.connectors {
        let status = if conn.connected { "connected" } else { "disconnected" };
        let monitor_name = output::edid_monitor_name(&conn.edid)
            .unwrap_or_else(|| "unknown".to_string());
        let mfr = output::edid_manufacturer(&conn.edid)
            .unwrap_or_else(|| "???".to_string());
        let mode_str = if let Some(m) = &conn.current_mode {
            format!("{}x{}@{}Hz", m.width, m.height, m.refresh)
        } else {
            "no mode".to_string()
        };
        let mult = config.multiplier_for(&conn.name, Some(&monitor_name));

        eprintln!("[PARALLAX] {}: {} ({} {}) {} {}mm x {}mm  DSR={:.1}x",
            conn.name, status, mfr, monitor_name, mode_str,
            conn.mm_width, conn.mm_height, mult);

        if mult > 1.0 {
            if let Some(m) = &conn.current_mode {
                let vw = (m.width as f64 * mult) as u32;
                let vh = (m.height as f64 * mult) as u32;
                eprintln!("[PARALLAX]   virtual: {}x{}@{}Hz -> native {}x{}",
                    vw, vh, m.refresh, m.width, m.height);
            }
        }

        for mode in &conn.modes {
            let pref = if mode.is_preferred() { " *" } else { "" };
            eprintln!("[PARALLAX]   mode: {}x{}@{}Hz{}", mode.width, mode.height, mode.refresh, pref);
        }
    }

    // Write to shared memory for triskelion
    let prefix_hash = compute_prefix_hash();
    match display_info::DisplayShm::create(&prefix_hash) {
        Some(shm) => {
            shm.write_hardware(&hw);
            eprintln!("[PARALLAX] display info written to {}", shm.shm_name());

            // If child command given, launch it and wait
            let child_cmd = get_child_command(&config);
            if let Some(cmd_str) = child_cmd {
                eprintln!("[PARALLAX] launching child: {cmd_str}");
                run_child(&cmd_str);
            }
            // Exit immediately. Triskelion reads the SHM during its
            // startup and shm_unlinks when done. No need to hold open.
        }
        None => {
            eprintln!("[PARALLAX] failed to create shared memory segment");
            std::process::exit(1);
        }
    }
}

fn compute_prefix_hash() -> String {
    let pfx = std::env::var("WINEPREFIX").unwrap_or_else(|_| {
        let h = std::env::var("HOME").unwrap_or_default();
        format!("{h}/.wine")
    });
    if let Ok(st) = std::fs::metadata(&pfx) {
        format!("{:x}{:x}", st.dev(), st.ino())
    } else {
        String::new()
    }
}

fn get_child_command(config: &config::Config) -> Option<String> {
    // Check for -- separator in args first
    let args: Vec<String> = std::env::args().collect();
    if let Some(pos) = args.iter().position(|a| a == "--") {
        let child_args = &args[pos + 1..];
        if !child_args.is_empty() {
            return Some(child_args.join(" "));
        }
    }
    config.child.clone()
}

fn run_child(cmd: &str) {
    let parts: Vec<&str> = cmd.split_whitespace().collect();
    if parts.is_empty() { return; }

    let mut child = match std::process::Command::new(parts[0])
        .args(&parts[1..])
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[PARALLAX] failed to launch child: {e}");
            return;
        }
    };

    match child.wait() {
        Ok(status) => {
            eprintln!("[PARALLAX] child exited: {status}");
        }
        Err(e) => {
            eprintln!("[PARALLAX] child wait error: {e}");
        }
    }
}
