pub mod client;
pub mod mcp;
pub mod platform;
pub mod probe;
pub mod process;
pub mod protocol;
pub mod psyfile;
pub mod ring_buffer;
pub mod root;

use std::collections::HashMap;
use std::path::PathBuf;

use chrono::{DateTime, Utc};
use clap::{Parser, Subcommand};

use protocol::{
    HistoryArgs, HistoryResponse, LogsArgs, PsResponse, Request, RestartArgs, RestartPolicy,
    RunArgs, SendArgs, SendWaitArgs, StopArgs, StreamFilter,
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
        /// Psyfile unit names to start on boot
        #[arg(value_name = "UNITS")]
        units: Vec<String>,
        /// Start all Psyfile units
        #[arg(long)]
        all: bool,
        /// Path to Psyfile (overrides discovery)
        #[arg(long)]
        file: Option<PathBuf>,
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
        /// Attach terminal stdin/stdout to the child process
        #[arg(long)]
        attach: bool,
        /// Enable stdin pipe (writable via psy send)
        #[arg(long)]
        interactive: bool,
        /// Command to run (required for ad-hoc processes, optional for Psyfile units)
        #[arg(last = true)]
        command: Vec<String>,
    },
    /// Write to a process's stdin (requires --interactive)
    Send {
        /// Process name
        name: String,
        /// Text to send (newline auto-appended unless --raw)
        #[arg(value_name = "TEXT")]
        input: Option<String>,
        /// Don't append a newline
        #[arg(long)]
        raw: bool,
        /// Close stdin (EOF)
        #[arg(long)]
        eof: bool,
        /// Read input from file
        #[arg(long, value_name = "PATH")]
        file: Option<PathBuf>,
        /// Wait for output after sending (blocking send)
        #[arg(long)]
        wait: bool,
        /// Overall timeout for --wait (e.g. 5s, 10s, 200ms)
        #[arg(long, default_value = "5s")]
        wait_timeout: String,
        /// Idle timeout for --wait — stop after this long with no output
        #[arg(long, default_value = "200ms")]
        idle_timeout: String,
        /// Prompt pattern for --wait — return early when output matches
        #[arg(long)]
        wait_prompt: Option<String>,
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
        /// Show logs since time (e.g. 5m, 1h, 2026-03-12T20:00:00Z, or "last")
        #[arg(long)]
        since: Option<String>,
        /// Show logs until time (e.g. 1m, 2026-03-12T21:00:00Z)
        #[arg(long)]
        until: Option<String>,
        /// Filter logs by substring (case-insensitive)
        #[arg(long)]
        grep: Option<String>,
        /// Show logs from a specific run ID
        #[arg(long)]
        run: Option<u32>,
        /// Show logs from the previous run
        #[arg(long)]
        previous: bool,
        /// Show probe logs instead of process output
        #[arg(long)]
        probe: bool,
    },
    /// Show run history for a process
    History {
        /// Process name
        name: String,
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
    /// Psyfile utilities
    Psyfile {
        #[command(subcommand)]
        command: PsyfileCommands,
    },
    /// Print version information
    Version,
}

#[derive(Subcommand)]
enum PsyfileCommands {
    /// Output JSON Schema for Psyfile format
    Schema,
    /// Validate the current Psyfile
    Validate {
        /// Path to Psyfile (overrides discovery)
        #[arg(long)]
        file: Option<PathBuf>,
    },
    /// Generate a starter Psyfile in the current directory
    Init,
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

fn main() {
    let cli = Cli::parse();

    match cli.command {
        Commands::Up {
            name,
            units,
            all,
            file,
            command,
        } => {
            // Resolve the Psyfile path
            let psyfile_path: Option<std::path::PathBuf> = if let Some(ref path) = file {
                // Explicit --file: validate it exists and parses
                match psyfile::parse(path) {
                    Ok(pf) => {
                        if let Err(e) = psyfile::validate(&pf) {
                            eprintln!("psy up: Psyfile error: {e}");
                            std::process::exit(1);
                        }
                    }
                    Err(e) => {
                        eprintln!("psy up: {e}");
                        std::process::exit(1);
                    }
                }
                Some(path.clone())
            } else if !units.is_empty() || all {
                // Need a Psyfile but none explicitly given — discover
                match psyfile::discover(&std::env::current_dir().unwrap_or_default()) {
                    Some(path) => {
                        match psyfile::parse(&path) {
                            Ok(pf) => {
                                if let Err(e) = psyfile::validate(&pf) {
                                    eprintln!("psy up: Psyfile error: {e}");
                                    std::process::exit(1);
                                }
                            }
                            Err(e) => {
                                eprintln!("psy up: {e}");
                                std::process::exit(1);
                            }
                        }
                        Some(path)
                    }
                    None => {
                        eprintln!("psy up: no Psyfile found");
                        std::process::exit(1);
                    }
                }
            } else {
                // No explicit --file, no units requested — discovery is optional.
                // The root will re-discover on each request (hot-reload).
                None
            };

            // Determine which units to start
            let boot_units = if all {
                // Parse the Psyfile to get all unit names
                let pf_path = psyfile_path.as_ref().unwrap(); // validated above
                let pf = psyfile::parse(pf_path).unwrap();
                pf.units.keys().cloned().collect::<Vec<_>>()
            } else if !units.is_empty() {
                // Validate unit names exist in the Psyfile
                let pf_path = psyfile_path.as_ref().unwrap(); // validated above
                let pf = psyfile::parse(pf_path).unwrap();
                for u in &units {
                    if !pf.units.contains_key(u) {
                        eprintln!("psy up: unknown unit '{u}' in Psyfile");
                        std::process::exit(1);
                    }
                }
                units
            } else {
                Vec::new()
            };

            let root_name = name.unwrap_or_else(|| format!("psy-{}", std::process::id()));
            let psy_root = match root::PsyRoot::new(root_name, psyfile_path) {
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
                match psy_root.run(main_cmd, boot_units).await {
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

        Commands::Psyfile { command: pf_cmd } => match pf_cmd {
            PsyfileCommands::Schema => {
                let schema = psyfile::json_schema();
                println!(
                    "{}",
                    serde_json::to_string_pretty(&schema).unwrap_or_default()
                );
            }
            PsyfileCommands::Validate { file } => {
                let path = if let Some(p) = file {
                    p
                } else {
                    match psyfile::discover(&std::env::current_dir().unwrap_or_default()) {
                        Some(p) => p,
                        None => {
                            eprintln!("error: no Psyfile found");
                            std::process::exit(1);
                        }
                    }
                };
                match psyfile::parse(&path) {
                    Ok(pf) => match psyfile::validate(&pf) {
                        Ok(()) => println!("OK: {}", path.display()),
                        Err(e) => {
                            eprintln!("error: {e}");
                            std::process::exit(1);
                        }
                    },
                    Err(e) => {
                        eprintln!("error: {e}");
                        std::process::exit(1);
                    }
                }
            }
            PsyfileCommands::Init => {
                let path = std::path::Path::new("Psyfile");
                if path.exists() {
                    eprintln!("error: Psyfile already exists");
                    std::process::exit(1);
                }
                let template = r#"# Psyfile - psy process definitions
# See: psy psyfile schema

[server]
command = "echo 'hello world'"
# restart = "on-failure"
# env = { PORT = "8080" }
# depends_on = ["db"]
# ready = { tcp = 8080 }
# healthcheck = { http = "http://localhost:8080/health", interval = "10s", retries = 3 }
"#;
                if let Err(e) = std::fs::write(path, template) {
                    eprintln!("error: {e}");
                    std::process::exit(1);
                }
                println!("Created Psyfile");
            }
        },

        Commands::Run {
            name,
            restart,
            envs,
            attach,
            interactive,
            command,
        } => {
            let restart_policy = parse_restart_policy(&restart);
            let env = parse_env_args(&envs);
            if attach {
                if let Err(e) = client::run_attached(&name, command, restart_policy, env) {
                    eprintln!("error: {e}");
                    std::process::exit(1);
                }
            } else {
                // If command is empty, this might be a Psyfile unit — send with
                // empty command and let the root resolve it.
                let (cmd, extra) = if command.is_empty() {
                    (vec![], None)
                } else {
                    (command, None)
                };
                let req = Request::run(RunArgs {
                    name,
                    command: cmd,
                    restart: restart_policy,
                    env,
                    attach: false,
                    interactive,
                    extra_args: extra,
                });
                send_and_print(req);
            }
        }

        Commands::Send {
            name,
            input,
            raw,
            eof,
            file,
            wait,
            wait_timeout,
            idle_timeout,
            wait_prompt,
        } => {
            if wait && (eof || file.is_some() || raw) {
                eprintln!("error: --wait is incompatible with --eof, --file, and --raw");
                std::process::exit(1);
            }

            if wait {
                let text = match input {
                    Some(t) => t,
                    None => {
                        eprintln!("error: provide text to send with --wait");
                        std::process::exit(1);
                    }
                };
                let req = Request::send_wait(SendWaitArgs {
                    name,
                    input: text,
                    timeout: Some(wait_timeout),
                    idle_timeout: Some(idle_timeout),
                    prompt: wait_prompt,
                });
                match client::send_command(req) {
                    Ok(resp) if resp.ok => {
                        if let Some(data) = resp.data {
                            if let Some(lines) = data.get("lines").and_then(|v| v.as_array()) {
                                for line in lines {
                                    if let Some(s) = line.as_str() {
                                        println!("{s}");
                                    }
                                }
                            }
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
            } else if eof {
                let req = Request::send(SendArgs {
                    name,
                    input: None,
                    eof: true,
                });
                send_and_print(req);
            } else {
                let text = if let Some(path) = file {
                    match std::fs::read_to_string(&path) {
                        Ok(contents) => contents,
                        Err(e) => {
                            eprintln!("error: cannot read {}: {e}", path.display());
                            std::process::exit(1);
                        }
                    }
                } else if let Some(text) = input {
                    if raw {
                        text
                    } else {
                        format!("{text}\n")
                    }
                } else {
                    eprintln!("error: provide text to send, --file, or --eof");
                    std::process::exit(1);
                };
                let req = Request::send(SendArgs {
                    name,
                    input: Some(text),
                    eof: false,
                });
                send_and_print(req);
            }
        }

        Commands::Ps { all: _all } => {
            let req = Request::ps();
            match client::send_command(req) {
                Ok(resp) if resp.ok => {
                    if let Some(data) = resp.data {
                        if let Ok(ps) = serde_json::from_value::<PsResponse>(data.clone()) {
                            print_ps_table(&ps);
                        } else {
                            println!(
                                "{}",
                                serde_json::to_string_pretty(&data).unwrap_or_default()
                            );
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
            since,
            until,
            grep,
            run,
            previous,
            probe,
        } => {
            let stream = if probe && stdout {
                StreamFilter::ProbeStdout
            } else if probe && stderr {
                StreamFilter::ProbeStderr
            } else if probe {
                StreamFilter::Probe
            } else if stdout {
                StreamFilter::Stdout
            } else if stderr {
                StreamFilter::Stderr
            } else {
                StreamFilter::All
            };

            // Parse time specs on the client side, send absolute timestamps.
            // Special case: "last" is passed through as-is for the root to resolve.
            let since_str = since.map(|s| {
                if s == "last" {
                    s
                } else {
                    parse_time_spec(&s)
                        .unwrap_or_else(|e| {
                            eprintln!("error: invalid --since: {e}");
                            std::process::exit(1);
                        })
                        .to_rfc3339()
                }
            });
            let until_str = until.map(|s| {
                parse_time_spec(&s)
                    .unwrap_or_else(|e| {
                        eprintln!("error: invalid --until: {e}");
                        std::process::exit(1);
                    })
                    .to_rfc3339()
            });

            if follow {
                if let Err(e) = client::follow_logs(&name, stream, since_str.clone(), grep.clone())
                {
                    eprintln!("error: {e}");
                    std::process::exit(1);
                }
            } else {
                let req = Request::logs(LogsArgs {
                    name,
                    tail,
                    stream,
                    since: since_str,
                    until: until_str,
                    grep,
                    run,
                    previous,
                    probe,
                });
                match client::send_command(req) {
                    Ok(resp) if resp.ok => {
                        if let Some(data) = resp.data {
                            if let Some(lines) = data.get("lines").and_then(|v| v.as_array()) {
                                for line in lines {
                                    let ts = line
                                        .get("timestamp")
                                        .and_then(|v| v.as_str())
                                        .unwrap_or("");
                                    let s = line
                                        .get("stream")
                                        .and_then(|v| v.as_str())
                                        .unwrap_or("stdout");
                                    let content =
                                        line.get("content").and_then(|v| v.as_str()).unwrap_or("");
                                    println!("[{ts} {s}] {content}");
                                }
                            }
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
        }

        Commands::History { name } => {
            let req = Request::history(HistoryArgs { name });
            match client::send_command(req) {
                Ok(resp) if resp.ok => {
                    if let Some(data) = resp.data {
                        if let Ok(history) = serde_json::from_value::<HistoryResponse>(data.clone())
                        {
                            print_history_table(&history);
                        } else {
                            println!(
                                "{}",
                                serde_json::to_string_pretty(&data).unwrap_or_default()
                            );
                        }
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
                println!(
                    "{}",
                    serde_json::to_string_pretty(&data).unwrap_or_default()
                );
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

/// Parse a time specification: either a relative duration (e.g. "5m", "1h", "30s", "2d")
/// or an absolute RFC 3339 / ISO 8601 timestamp.
fn parse_time_spec(s: &str) -> Result<DateTime<Utc>, String> {
    // Try relative duration first: <N>s, <N>m, <N>h, <N>d
    let s_trimmed = s.trim();
    if let Some(num_str) = s_trimmed.strip_suffix('s') {
        if let Ok(n) = num_str.parse::<u64>() {
            return Ok(Utc::now() - chrono::Duration::seconds(n as i64));
        }
    }
    if let Some(num_str) = s_trimmed.strip_suffix('m') {
        if let Ok(n) = num_str.parse::<u64>() {
            return Ok(Utc::now() - chrono::Duration::minutes(n as i64));
        }
    }
    if let Some(num_str) = s_trimmed.strip_suffix('h') {
        if let Ok(n) = num_str.parse::<u64>() {
            return Ok(Utc::now() - chrono::Duration::hours(n as i64));
        }
    }
    if let Some(num_str) = s_trimmed.strip_suffix('d') {
        if let Ok(n) = num_str.parse::<u64>() {
            return Ok(Utc::now() - chrono::Duration::days(n as i64));
        }
    }

    // Try RFC 3339 with timezone
    if let Ok(dt) = DateTime::parse_from_rfc3339(s_trimmed) {
        return Ok(dt.with_timezone(&Utc));
    }

    // Try ISO 8601 without timezone (assume UTC)
    if let Ok(naive) = chrono::NaiveDateTime::parse_from_str(s_trimmed, "%Y-%m-%dT%H:%M:%S") {
        return Ok(naive.and_utc());
    }

    Err(format!(
        "expected relative duration (e.g. 5s, 10m, 1h, 2d) or RFC 3339 timestamp, got: {s}"
    ))
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
        "{:<20} {:<8} {:<10} {:<8} {:<8} {:<14} {:<10} RESTART",
        "NAME", "PID", "STATUS", "READY", "EXIT", "UPTIME", "RESTARTS"
    );
    println!("{}", "-".repeat(86));
    for p in &ps.processes {
        let pid_str = p.pid.map(|p| p.to_string()).unwrap_or_else(|| "-".into());
        let ready_str = p.ready.as_deref().unwrap_or("-");
        let exit_str = if let Some(sig) = &p.signal {
            sig.clone()
        } else {
            p.exit_code
                .map(|c| c.to_string())
                .unwrap_or_else(|| "-".into())
        };
        let uptime = p
            .uptime_secs
            .map(format_uptime)
            .unwrap_or_else(|| "-".into());
        let restart = format!("{:?}", p.restart_policy).to_lowercase();
        println!(
            "{:<20} {:<8} {:<10} {:<8} {:<8} {:<14} {:<10} {}",
            p.name, pid_str, p.status, ready_str, exit_str, uptime, p.restarts, restart
        );
    }
}

fn print_history_table(history: &HistoryResponse) {
    if history.runs.is_empty() {
        println!("No runs recorded for '{}'", history.name);
        return;
    }
    println!(
        "{:<6} {:<10} {:<8} {:<28} DURATION",
        "RUN", "STATUS", "EXIT", "STARTED"
    );
    println!("{}", "-".repeat(68));
    for r in &history.runs {
        let exit_str = if let Some(sig) = &r.signal {
            sig.clone()
        } else {
            r.exit_code
                .map(|c| c.to_string())
                .unwrap_or_else(|| "-".into())
        };
        let started = r.started_at.as_deref().unwrap_or("-");
        let duration = r
            .duration_secs
            .map(format_uptime)
            .unwrap_or_else(|| "-".into());
        println!(
            "{:<6} {:<10} {:<8} {:<28} {}",
            r.run_id, r.status, exit_str, started, duration
        );
    }
}

fn format_uptime(secs: u64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m {}s", secs / 60, secs % 60)
    } else {
        format!("{}h {}m {}s", secs / 3600, (secs % 3600) / 60, secs % 60)
    }
}
