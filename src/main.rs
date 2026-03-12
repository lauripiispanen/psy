pub mod client;
pub mod mcp;
pub mod platform;
pub mod process;
pub mod protocol;
pub mod ring_buffer;
pub mod root;

use std::collections::HashMap;

use clap::{Parser, Subcommand};

use protocol::{
    LogsArgs, PsResponse, Request, RestartArgs, RestartPolicy, RunArgs, StopArgs, StreamFilter,
};

// ---------------------------------------------------------------------------
// CLI definition
// ---------------------------------------------------------------------------

#[derive(Parser)]
#[command(
    name = "psy",
    about = "Cross-platform process lifecycle manager",
    version
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Start a new psy root session
    Up {
        /// Name for the main process
        #[arg(long)]
        name: Option<String>,
        /// Command to run as the main process (default: $SHELL)
        #[arg(last = true)]
        command: Vec<String>,
    },
    /// Run a managed child process
    Run {
        /// Unique name for the process
        name: String,
        /// Restart policy: no, on-failure, always
        #[arg(long, default_value = "no")]
        restart: String,
        /// Environment variables (KEY=VAL)
        #[arg(long = "env", value_name = "KEY=VAL")]
        envs: Vec<String>,
        /// Command to run
        #[arg(last = true, required = true)]
        command: Vec<String>,
    },
    /// List managed processes
    Ps {
        /// Show all processes including stopped
        #[arg(long)]
        all: bool,
    },
    /// View logs for a process
    Logs {
        /// Process name
        name: String,
        /// Follow log output
        #[arg(short, long)]
        follow: bool,
        /// Number of lines to show
        #[arg(long)]
        tail: Option<usize>,
        /// Show only stdout
        #[arg(long)]
        stdout: bool,
        /// Show only stderr
        #[arg(long)]
        stderr: bool,
    },
    /// Stop a managed process
    Stop {
        /// Process name
        name: String,
    },
    /// Restart a managed process
    Restart {
        /// Process name
        name: String,
    },
    /// Shut down the psy root and all managed processes
    Down,
    /// Start MCP JSON-RPC server on stdin/stdout
    Mcp,
    /// Print version information
    Version,
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

fn main() {
    let cli = Cli::parse();

    match cli.command {
        Commands::Up { name, command } => {
            let root_name = name.unwrap_or_else(|| format!("psy-{}", std::process::id()));
            let psy_root = match root::PsyRoot::new(root_name) {
                Ok(r) => r,
                Err(e) => {
                    eprintln!("psy up: {e}");
                    std::process::exit(1);
                }
            };
            let main_cmd = if command.is_empty() {
                None
            } else {
                Some(command)
            };
            let rt = tokio::runtime::Runtime::new().expect("failed to create tokio runtime");
            let exit_code = rt.block_on(async {
                match psy_root.run(main_cmd).await {
                    Ok(code) => code,
                    Err(e) => {
                        eprintln!("psy: {e}");
                        1
                    }
                }
            });
            std::process::exit(exit_code);
        }

        Commands::Version => {
            println!("psy {}", env!("CARGO_PKG_VERSION"));
        }

        Commands::Mcp => {
            if let Err(e) = mcp::run() {
                eprintln!("mcp error: {e}");
                std::process::exit(1);
            }
        }

        Commands::Run {
            name,
            restart,
            envs,
            command,
        } => {
            let restart_policy = parse_restart_policy(&restart);
            let env = parse_env_args(&envs);
            let req = Request::run(RunArgs {
                name,
                command,
                restart: restart_policy,
                env,
            });
            send_and_print(req);
        }

        Commands::Ps { all: _all } => {
            let req = Request::ps();
            match client::send_command(req) {
                Ok(resp) if resp.ok => {
                    if let Some(data) = resp.data {
                        if let Ok(ps) = serde_json::from_value::<PsResponse>(data.clone()) {
                            print_ps_table(&ps);
                        } else {
                            println!("{}", serde_json::to_string_pretty(&data).unwrap_or_default());
                        }
                    } else {
                        println!("No processes");
                    }
                }
                Ok(resp) => {
                    eprintln!("error: {}", resp.error.unwrap_or_else(|| "unknown".into()));
                    std::process::exit(1);
                }
                Err(e) => {
                    eprintln!("error: {e}");
                    std::process::exit(1);
                }
            }
        }

        Commands::Logs {
            name,
            follow,
            tail,
            stdout,
            stderr,
        } => {
            let stream = if stdout {
                StreamFilter::Stdout
            } else if stderr {
                StreamFilter::Stderr
            } else {
                StreamFilter::All
            };

            if follow {
                if let Err(e) = client::follow_logs(&name, stream) {
                    eprintln!("error: {e}");
                    std::process::exit(1);
                }
            } else {
                let req = Request::logs(LogsArgs {
                    name,
                    tail,
                    stream,
                });
                send_and_print(req);
            }
        }

        Commands::Stop { name } => {
            let req = Request::stop(StopArgs { name });
            send_and_print(req);
        }

        Commands::Restart { name } => {
            let req = Request::restart(RestartArgs { name });
            send_and_print(req);
        }

        Commands::Down => {
            let req = Request::down();
            send_and_print(req);
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn send_and_print(req: Request) {
    match client::send_command(req) {
        Ok(resp) if resp.ok => {
            if let Some(data) = resp.data {
                println!("{}", serde_json::to_string_pretty(&data).unwrap_or_default());
            }
        }
        Ok(resp) => {
            eprintln!("error: {}", resp.error.unwrap_or_else(|| "unknown".into()));
            std::process::exit(1);
        }
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
    }
}

fn parse_restart_policy(s: &str) -> RestartPolicy {
    match s {
        "on-failure" | "on_failure" => RestartPolicy::OnFailure,
        "always" => RestartPolicy::Always,
        _ => RestartPolicy::No,
    }
}

fn parse_env_args(envs: &[String]) -> HashMap<String, String> {
    let mut map = HashMap::new();
    for e in envs {
        if let Some((k, v)) = e.split_once('=') {
            map.insert(k.to_string(), v.to_string());
        }
    }
    map
}

fn print_ps_table(ps: &PsResponse) {
    if ps.processes.is_empty() {
        println!("No processes running");
        return;
    }
    println!(
        "{:<20} {:<8} {:<12} {:<12} {}",
        "NAME", "PID", "STATUS", "RESTART", "UPTIME"
    );
    println!("{}", "-".repeat(64));
    for p in &ps.processes {
        let pid_str = p
            .pid
            .map(|p| p.to_string())
            .unwrap_or_else(|| "-".into());
        let uptime = p
            .uptime_secs
            .map(|s| format!("{s}s"))
            .unwrap_or_else(|| "-".into());
        let restart = format!("{:?}", p.restart_policy).to_lowercase();
        println!(
            "{:<20} {:<8} {:<12} {:<12} {}",
            p.name, pid_str, p.status, restart, uptime
        );
    }
}
