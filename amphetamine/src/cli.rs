// Command-line mode dispatch.
// Detects whether we're invoked as a proton launcher, packager, or server daemon.

pub enum Mode {
    Server,
    Launch { verb: String, args: Vec<String> },
    Package { wine_dir: String },
    Status,
    Analyze,
    Configure { wine_dir: String, execute: bool },
    Profile { app_id: String, game_name: Option<String> },
    ProfileAttach { label: Option<String> },
    ProfileCompare { dir_a: String, dir_b: String },
    ProfileOpcodes { trace_file: String },
    Clone,
}

const PROTON_VERBS: &[&str] = &[
    "waitforexitandrun",
    "run",
    "getcompatpath",
    "getnativepath",
];

pub fn parse_args() -> Mode {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        return Mode::Server;
    }

    let cmd = args[1].as_str();

    // wineserver flags -- wine64 invokes us as $WINESERVER with these
    if cmd.starts_with('-') {
        return match cmd {
            "-k" | "--kill" => {
                // Kill: signal existing triskelion to shut down (for now, no-op)
                eprintln!("[triskelion] kill requested");
                std::process::exit(0);
            }
            "-w" | "--wait" => {
                // Wait: block until wineserver terminates (for now, instant return)
                std::process::exit(0);
            }
            "-f" | "--foreground" => Mode::Server,
            _ => Mode::Server, // treat unknown flags as "start server"
        };
    }

    if PROTON_VERBS.contains(&cmd) {
        return Mode::Launch {
            verb: cmd.to_string(),
            args: args[2..].to_vec(),
        };
    }

    match cmd {
        "server" => Mode::Server,

        "status" => Mode::Status,

        "analyze" => Mode::Analyze,

        "configure" => {
            if args.len() < 3 {
                crate::log::log_error!("Usage: triskelion configure <wine_dir> [--execute]");
                std::process::exit(1);
            }
            let execute = args[3..].iter().any(|a| a == "--execute");
            Mode::Configure {
                wine_dir: args[2].clone(),
                execute,
            }
        }

        "package" => {
            if args.len() < 3 {
                crate::log::log_error!("Usage: triskelion package <wine_build_dir>");
                std::process::exit(1);
            }
            Mode::Package {
                wine_dir: args[2].clone(),
            }
        }

        "profile" => {
            if args.len() < 3 {
                crate::log::log_error!("Usage: triskelion profile <steam_app_id> [game_name]");
                std::process::exit(1);
            }
            Mode::Profile {
                app_id: args[2].clone(),
                game_name: args.get(3).cloned(),
            }
        }

        "profile-attach" => {
            let label = if args.len() >= 4 && args[2] == "--label" {
                Some(args[3].clone())
            } else {
                None
            };
            Mode::ProfileAttach { label }
        }

        "profile-compare" => {
            if args.len() < 4 {
                crate::log::log_error!("Usage: triskelion profile-compare <dir_a> <dir_b>");
                std::process::exit(1);
            }
            Mode::ProfileCompare {
                dir_a: args[2].clone(),
                dir_b: args[3].clone(),
            }
        }

        "profile-opcodes" => {
            let trace_file = if args.len() >= 3 {
                args[2].clone()
            } else {
                "/tmp/amphetamine/opcode_trace.log".to_string()
            };
            Mode::ProfileOpcodes { trace_file }
        }

        "clone" => Mode::Clone,

        _ => {
            crate::log::log_error!("Unknown command: {cmd}");
            print_usage();
            std::process::exit(1);
        }
    }
}

fn print_usage() {
    eprintln!("Usage:");
    eprintln!("  triskelion                                Server daemon");
    eprintln!("  triskelion server                         Server daemon");
    eprintln!("  triskelion status                         Show project status");
    eprintln!("  triskelion analyze                        Analyze Wine DLL surface");
    eprintln!("  triskelion configure <wine_dir> [--execute]");
    eprintln!("                                            Generate stripped ./configure");
    eprintln!("  triskelion package <wine_dir>             Package Wine as Steam compat tool");
    eprintln!("  triskelion profile <app_id> [name]        Profile a Steam game");
    eprintln!("  triskelion profile-attach [--label NAME]   Attach to running game");
    eprintln!("  triskelion profile-compare <dir_a> <dir_b> Compare two profiles");
    eprintln!("  triskelion profile-opcodes [trace_file]   Analyze wineserver opcode trace");
    eprintln!("  triskelion clone                          Clone Valve Wine + Proton");
    eprintln!("  triskelion <verb> <exe> [args...]         Proton launcher");
}
