use std::collections::HashMap;
use std::io::{self, BufRead, Write};

use serde_json;

use crate::protocol::{
    LogsArgs, Request, Response, RestartPolicy, RunArgs, StdinData, StreamFilter,
};

/// Read PSY_SOCK from the environment, returning a friendly error if unset.
fn sock_path() -> Result<String, String> {
    std::env::var("PSY_SOCK")
        .map_err(|_| "PSY_SOCK not set \u{2014} are you inside a psy session?".to_string())
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
        extra_args: None,
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
