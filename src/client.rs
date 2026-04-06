use std::collections::HashMap;
use std::io::{self, BufRead, Write};

use serde_json;

use crate::platform;
use crate::protocol::{
    LogsArgs, Request, Response, RestartPolicy, RunArgs, StdinData, StreamFilter,
};

/// Resolve the socket/pipe path to connect to.
///
/// 1. If `PSY_SOCK` is set in the environment, use it directly.
/// 2. Otherwise, scan anchor files to discover the nearest psy root.
fn sock_path() -> Result<String, String> {
    if let Ok(path) = std::env::var("PSY_SOCK") {
        return Ok(path);
    }
    discover_root()
}

/// Discover the nearest psy root by scanning anchor files and matching PID
/// ancestor chains.
fn discover_root() -> Result<String, String> {
    let my_chain = platform::get_ancestor_chain(std::process::id());
    let my_chain_set: std::collections::HashMap<u32, usize> = my_chain
        .iter()
        .enumerate()
        .map(|(i, &pid)| (pid, i))
        .collect();
    let my_index = my_chain.len() - 1; // index of our own PID

    let roots = platform::roots_dir();
    let entries = std::fs::read_dir(&roots).map_err(|_| {
        "no psy root found — start one with 'psy up' or ensure 'psy mcp' is running".to_string()
    })?;

    // Collect candidates: (distance, mtime, anchor_path, root_pid)
    let mut candidates: Vec<(usize, std::time::SystemTime, std::path::PathBuf, u32)> = Vec::new();

    for entry in entries.flatten() {
        let name = entry.file_name();
        let name_str = match name.to_str() {
            Some(s) => s,
            None => continue,
        };

        let chain = match parse_anchor_filename(name_str) {
            Some(c) => c,
            None => continue,
        };

        let root_pid = match chain.last() {
            Some(&p) => p,
            None => continue,
        };

        // Find the closest shared ancestor: walk the anchor's chain from leaf
        // toward root and find the first PID that is also in our chain.
        let mut best_distance = None;
        for &apid in chain.iter().rev() {
            if let Some(&idx) = my_chain_set.get(&apid) {
                let distance = my_index - idx;
                best_distance = Some(distance);
                break;
            }
        }

        if let Some(distance) = best_distance {
            let mtime = entry
                .metadata()
                .and_then(|m| m.modified())
                .unwrap_or(std::time::UNIX_EPOCH);
            candidates.push((distance, mtime, entry.path(), root_pid));
        }
    }

    if candidates.is_empty() {
        return Err(
            "no psy root found — start one with 'psy up' or ensure 'psy mcp' is running"
                .to_string(),
        );
    }

    // Sort by distance (ascending), then by mtime (descending = most recent first).
    candidates.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| b.1.cmp(&a.1)));

    // Try candidates in order, validating liveness.
    for (_, _, anchor_path, root_pid) in &candidates {
        if !platform::is_pid_alive(*root_pid) {
            // Stale anchor — clean up and skip.
            let _ = std::fs::remove_file(anchor_path);
            continue;
        }

        return resolve_socket_from_anchor(anchor_path);
    }

    Err("no psy root found — start one with 'psy up' or ensure 'psy mcp' is running".to_string())
}

/// Parse a PID chain from an anchor filename (platform-specific extension).
fn parse_anchor_filename(filename: &str) -> Option<Vec<u32>> {
    platform::parse_anchor_chain(filename)
}

/// Given an anchor file path, return the socket/pipe path to connect to.
#[cfg(unix)]
fn resolve_socket_from_anchor(anchor: &std::path::Path) -> Result<String, String> {
    use std::os::unix::fs::FileTypeExt;

    let meta = std::fs::metadata(anchor)
        .map_err(|e| format!("cannot read anchor {}: {e}", anchor.display()))?;

    if meta.file_type().is_socket() {
        // Direct mode: the anchor file IS the Unix domain socket.
        Ok(anchor.to_string_lossy().to_string())
    } else {
        // Indirect mode: the anchor file contains the socket path.
        let contents = std::fs::read_to_string(anchor)
            .map_err(|e| format!("cannot read anchor {}: {e}", anchor.display()))?;
        let sock_path = contents.trim().to_string();
        if sock_path.is_empty() {
            return Err(format!("anchor file {} is empty", anchor.display()));
        }
        Ok(sock_path)
    }
}

#[cfg(windows)]
fn resolve_socket_from_anchor(anchor: &std::path::Path) -> Result<String, String> {
    // On Windows, the anchor file always contains the named pipe path.
    let contents = std::fs::read_to_string(anchor)
        .map_err(|e| format!("cannot read anchor {}: {e}", anchor.display()))?;
    let pipe_path = contents.trim().to_string();
    if pipe_path.is_empty() {
        return Err(format!("anchor file {} is empty", anchor.display()));
    }
    Ok(pipe_path)
}

// ---------------------------------------------------------------------------
// Platform-specific transport
// ---------------------------------------------------------------------------

#[cfg(unix)]
mod transport {
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixStream;

    pub fn connect(path: &str) -> Result<(impl BufRead, impl Write), String> {
        let stream = UnixStream::connect(path)
            .map_err(|e| format!("Cannot connect to psy root at {path}: {e}"))?;
        let reader = BufReader::new(
            stream
                .try_clone()
                .map_err(|e| format!("clone error: {e}"))?,
        );
        Ok((reader, stream))
    }

    pub fn connect_streaming(path: &str) -> Result<(BufReader<UnixStream>, UnixStream), String> {
        let stream = UnixStream::connect(path)
            .map_err(|e| format!("Cannot connect to psy root at {path}: {e}"))?;
        let reader = BufReader::new(
            stream
                .try_clone()
                .map_err(|e| format!("clone error: {e}"))?,
        );
        Ok((reader, stream))
    }
}

#[cfg(windows)]
mod transport {
    use std::fs::OpenOptions;
    use std::io::{BufRead, BufReader, Write};

    pub fn connect(path: &str) -> Result<(impl BufRead, impl Write), String> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .map_err(|e| format!("Cannot connect to psy root at {path}: {e}"))?;
        let reader_file = file.try_clone().map_err(|e| format!("clone error: {e}"))?;
        let reader = BufReader::new(reader_file);
        Ok((reader, file))
    }

    pub fn connect_streaming(
        path: &str,
    ) -> Result<(BufReader<std::fs::File>, std::fs::File), String> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .map_err(|e| format!("Cannot connect to psy root at {path}: {e}"))?;
        let reader_file = file.try_clone().map_err(|e| format!("clone error: {e}"))?;
        let reader = BufReader::new(reader_file);
        Ok((reader, file))
    }
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Send a single request to the root process and return its response.
pub fn send_command(request: Request) -> Result<Response, String> {
    let path = sock_path()?;
    let (mut reader, mut writer) = transport::connect(&path)?;

    let mut payload =
        serde_json::to_string(&request).map_err(|e| format!("serialize error: {e}"))?;
    payload.push('\n');
    writer
        .write_all(payload.as_bytes())
        .map_err(|e| format!("write error: {e}"))?;
    writer.flush().map_err(|e| format!("flush error: {e}"))?;

    let mut line = String::new();
    reader
        .read_line(&mut line)
        .map_err(|e| format!("read error: {e}"))?;

    if line.is_empty() {
        return Err("Connection closed before response was received".to_string());
    }

    let response: Response =
        serde_json::from_str(&line).map_err(|e| format!("deserialize error: {e}"))?;

    Ok(response)
}

/// Follow logs for a named process, printing each line to stdout until the
/// connection is closed or the user presses Ctrl-C.
pub fn follow_logs(
    name: &str,
    stream: StreamFilter,
    since: Option<String>,
    grep: Option<String>,
) -> Result<(), String> {
    let path = sock_path()?;
    let (mut reader, mut writer) = transport::connect_streaming(&path)?;

    let request = Request::logs_follow(LogsArgs {
        name: name.to_string(),
        tail: None,
        stream,
        since,
        until: None,
        grep,
        run: None,
        previous: false,
        probe: false,
    });
    let mut payload =
        serde_json::to_string(&request).map_err(|e| format!("serialize error: {e}"))?;
    payload.push('\n');
    writer
        .write_all(payload.as_bytes())
        .map_err(|e| format!("write error: {e}"))?;
    writer.flush().map_err(|e| format!("flush error: {e}"))?;

    stream_log_lines(&mut reader)
}

/// Run a process in attach mode: forward stdin to the child, stream output back.
pub fn run_attached(
    name: &str,
    command: Vec<String>,
    restart: RestartPolicy,
    env: HashMap<String, String>,
) -> Result<(), String> {
    let path = sock_path()?;
    let (mut reader, mut writer) = transport::connect_streaming(&path)?;

    let request = Request::run(RunArgs {
        name: name.to_string(),
        command,
        restart,
        env,
        attach: true,
        interactive: false,
        extra_args: None,
        wait_for: None,
        wait_timeout: None,
        ports: vec![],
    });
    let mut payload =
        serde_json::to_string(&request).map_err(|e| format!("serialize error: {e}"))?;
    payload.push('\n');
    writer
        .write_all(payload.as_bytes())
        .map_err(|e| format!("write error: {e}"))?;
    writer.flush().map_err(|e| format!("flush error: {e}"))?;

    // Read initial response
    let mut line = String::new();
    reader
        .read_line(&mut line)
        .map_err(|e| format!("read error: {e}"))?;
    if line.is_empty() {
        return Err("Connection closed before response".to_string());
    }
    let response: Response =
        serde_json::from_str(&line).map_err(|e| format!("deserialize error: {e}"))?;
    if !response.ok {
        return Err(response.error.unwrap_or_else(|| "unknown error".into()));
    }

    // Spawn a thread to read stdin and forward to the root
    let writer_clone = writer
        .try_clone()
        .map_err(|e| format!("clone error: {e}"))?;
    let name_owned = name.to_string();
    std::thread::spawn(move || {
        stdin_forwarder(writer_clone, &name_owned);
    });

    // Read output from root and print
    let stdout = io::stdout();
    let mut out = stdout.lock();
    let mut buf = String::new();
    loop {
        buf.clear();
        match reader.read_line(&mut buf) {
            Ok(0) => break,
            Ok(_) => {
                if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&buf) {
                    // Check if it's a detach notice
                    if parsed.get("detached").is_some() {
                        let reason = parsed
                            .get("reason")
                            .and_then(|v| v.as_str())
                            .unwrap_or("unknown");
                        let exit_code = parsed.get("exit_code").and_then(|v| v.as_i64());
                        eprintln!("detached from {name}: {reason}");
                        if let Some(code) = exit_code {
                            std::process::exit(code as i32);
                        }
                        break;
                    }
                    // Regular log line
                    let ts = parsed
                        .get("timestamp")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    let stream = parsed
                        .get("stream")
                        .and_then(|v| v.as_str())
                        .unwrap_or("stdout");
                    let content = parsed.get("content").and_then(|v| v.as_str()).unwrap_or("");
                    let _ = writeln!(out, "[{ts} {stream}] {content}");
                } else {
                    let _ = out.write_all(buf.as_bytes());
                }
                let _ = out.flush();
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn stream_log_lines(reader: &mut impl BufRead) -> Result<(), String> {
    let stdout = io::stdout();
    let mut out = stdout.lock();
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => break,
            Ok(_) => {
                // Parse NDJSON log line and format as plain text
                if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&line) {
                    let ts = parsed
                        .get("timestamp")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    let stream = parsed
                        .get("stream")
                        .and_then(|v| v.as_str())
                        .unwrap_or("stdout");
                    let content = parsed.get("content").and_then(|v| v.as_str()).unwrap_or("");
                    let _ = writeln!(out, "[{ts} {stream}] {content}");
                } else {
                    let _ = out.write_all(line.as_bytes());
                }
                let _ = out.flush();
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }

    Ok(())
}

fn stdin_forwarder(mut writer: impl Write, _name: &str) {
    let stdin = io::stdin();
    let reader = stdin.lock();
    for line_result in reader.lines() {
        match line_result {
            Ok(line) => {
                let data = StdinData {
                    stdin: format!("{line}\n"),
                };
                let mut payload = match serde_json::to_string(&data) {
                    Ok(p) => p,
                    Err(_) => break,
                };
                payload.push('\n');
                if writer.write_all(payload.as_bytes()).is_err() {
                    break;
                }
                if writer.flush().is_err() {
                    break;
                }
            }
            Err(_) => break,
        }
    }
}
