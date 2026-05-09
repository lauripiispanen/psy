//! Probe execution engine for readiness and health checks.
//!
//! Probes run as background tasks and write diagnostic output to the process's
//! ring buffer using `probe:stdout` and `probe:stderr` streams.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{watch, Mutex};

use crate::process::{ProcessEntry, ProcessState};
use crate::psyfile::{ProbeConfig, ProbeKind};
use crate::ring_buffer::{RingBuffer, Stream};

/// Run a startup readiness probe. Polls at `config.interval` until the probe
/// succeeds or the timeout/retries are exhausted.
pub async fn run_ready_probe(
    process_table: Arc<Mutex<HashMap<String, ProcessEntry>>>,
    name: String,
    config: ProbeConfig,
    stdout_buf: Arc<RingBuffer>,
    stderr_buf: Arc<RingBuffer>,
    mut cancel: watch::Receiver<bool>,
) {
    let max_retries = config
        .retries
        .unwrap_or_else(|| (config.timeout.as_secs() / config.interval.as_secs().max(1)) as u32);

    for attempt in 1..=max_retries {
        // Check cancellation
        if *cancel.borrow() {
            return;
        }

        let success = execute_probe(
            &config.probe,
            &name,
            &stdout_buf,
            &stderr_buf,
            attempt,
            max_retries,
        )
        .await;

        if success {
            probe_log(
                &stderr_buf,
                &format!("{} — ready", probe_label(&config.probe)),
            );
            // Mark process as ready
            let mut table = process_table.lock().await;
            if let Some(entry) = table.get_mut(&name) {
                entry.ready = true;
                entry.ready_notify.notify_waiters();
            }
            return;
        }

        if attempt == max_retries {
            probe_log(
                &stderr_buf,
                &format!(
                    "{} — timed out after {}s ({}/{} attempts failed)",
                    probe_label(&config.probe),
                    config.timeout.as_secs(),
                    attempt,
                    max_retries,
                ),
            );
            // Mark probe as failed
            let mut table = process_table.lock().await;
            if let Some(entry) = table.get_mut(&name) {
                entry.ready_failed = true;
            }
            return;
        }

        // Wait for interval or cancellation
        tokio::select! {
            _ = tokio::time::sleep(config.interval) => {}
            _ = cancel.changed() => {
                if *cancel.borrow() {
                    return;
                }
            }
        }
    }
}

/// Run a continuous health check. Starts after the process is ready (waits for
/// `ready_notify` if the process has a readiness probe). On consecutive failures
/// exceeding `retries`, kills the process to trigger restart via monitor_child.
pub async fn run_healthcheck(
    process_table: Arc<Mutex<HashMap<String, ProcessEntry>>>,
    name: String,
    config: ProbeConfig,
    stdout_buf: Arc<RingBuffer>,
    stderr_buf: Arc<RingBuffer>,
    mut cancel: watch::Receiver<bool>,
) {
    // Wait for the process to become ready first
    {
        let notify = {
            let table = process_table.lock().await;
            if let Some(entry) = table.get(&name) {
                if !entry.ready {
                    Some(Arc::clone(&entry.ready_notify))
                } else {
                    None
                }
            } else {
                return;
            }
        };

        if let Some(notify) = notify {
            tokio::select! {
                _ = notify.notified() => {}
                _ = cancel.changed() => {
                    if *cancel.borrow() {
                        return;
                    }
                }
            }
        }
    }

    let max_failures = config.retries.unwrap_or(3);
    let mut consecutive_failures: u32 = 0;
    let mut check_num: u32 = 0;

    loop {
        // Wait for interval or cancellation
        tokio::select! {
            _ = tokio::time::sleep(config.interval) => {}
            _ = cancel.changed() => {
                if *cancel.borrow() {
                    return;
                }
            }
        }

        if *cancel.borrow() {
            return;
        }

        check_num += 1;

        let success = execute_probe(
            &config.probe,
            &name,
            &stdout_buf,
            &stderr_buf,
            check_num,
            0, // no max for healthcheck display
        )
        .await;

        if success {
            consecutive_failures = 0;
        } else {
            consecutive_failures += 1;
            if consecutive_failures >= max_failures {
                probe_log(
                    &stderr_buf,
                    &format!(
                        "{} — unhealthy ({}/{} consecutive failures), triggering restart",
                        probe_label(&config.probe),
                        consecutive_failures,
                        max_failures,
                    ),
                );

                // Request monitor_child to kill the process. We don't kill
                // directly because monitor_child owns the child handle.
                let kill_notify = {
                    let table = process_table.lock().await;
                    table
                        .get(&name)
                        .filter(|e| e.state == ProcessState::Running)
                        .map(|e| Arc::clone(&e.kill_notify))
                };
                if let Some(notify) = kill_notify {
                    notify.notify_one();
                }
                return;
            } else {
                probe_log(
                    &stderr_buf,
                    &format!(
                        "{} — check failed ({}/{} consecutive failures)",
                        probe_label(&config.probe),
                        consecutive_failures,
                        max_failures,
                    ),
                );
            }
        }
    }
}

/// Execute a single probe attempt. Returns true on success.
async fn execute_probe(
    kind: &ProbeKind,
    _name: &str,
    stdout_buf: &Arc<RingBuffer>,
    stderr_buf: &Arc<RingBuffer>,
    attempt: u32,
    max_attempts: u32,
) -> bool {
    match kind {
        ProbeKind::Tcp(addr) => {
            let attempt_label = if max_attempts > 0 {
                format!("{} — attempt {}/{}", addr, attempt, max_attempts)
            } else {
                format!("{} — check {}", addr, attempt)
            };

            match tokio::time::timeout(
                Duration::from_secs(1),
                tokio::net::TcpStream::connect(addr.as_str()),
            )
            .await
            {
                Ok(Ok(_)) => true,
                Ok(Err(e)) => {
                    probe_log(stderr_buf, &format!("tcp {} — {}", attempt_label, e));
                    false
                }
                Err(_) => {
                    probe_log(
                        stderr_buf,
                        &format!("tcp {} — connection timed out", attempt_label),
                    );
                    false
                }
            }
        }

        ProbeKind::Http(url) => {
            let attempt_label = if max_attempts > 0 {
                format!("attempt {}/{}", attempt, max_attempts)
            } else {
                format!("check {}", attempt)
            };

            match http_probe(url).await {
                Ok(status) => {
                    if (200..300).contains(&status) {
                        true
                    } else {
                        probe_log(
                            stderr_buf,
                            &format!("http {} — {} — HTTP {}", url, attempt_label, status),
                        );
                        false
                    }
                }
                Err(e) => {
                    probe_log(
                        stderr_buf,
                        &format!("http {} — {} — {}", url, attempt_label, e),
                    );
                    false
                }
            }
        }

        ProbeKind::Exec(cmd) => {
            let attempt_label = if max_attempts > 0 {
                format!("attempt {}/{}", attempt, max_attempts)
            } else {
                format!("check {}", attempt)
            };

            #[cfg(unix)]
            let result = tokio::process::Command::new("sh")
                .args(["-c", cmd])
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .output()
                .await;
            #[cfg(windows)]
            let result = tokio::process::Command::new("cmd")
                .args(["/C", cmd])
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .output()
                .await;

            match result {
                Ok(output) => {
                    // Capture stdout to probe:stdout (up to 256 bytes)
                    let stdout_str = String::from_utf8_lossy(&output.stdout);
                    let stdout_trimmed = stdout_str.trim();
                    if !stdout_trimmed.is_empty() {
                        let truncated = if stdout_trimmed.len() > 256 {
                            &stdout_trimmed[..256]
                        } else {
                            stdout_trimmed
                        };
                        for line in truncated.lines() {
                            stdout_buf.push(Stream::ProbeStdout, line.to_string());
                        }
                    }

                    // Capture stderr to probe:stderr (up to 256 bytes)
                    let stderr_str = String::from_utf8_lossy(&output.stderr);
                    let stderr_trimmed = stderr_str.trim();
                    if !stderr_trimmed.is_empty() {
                        let truncated = if stderr_trimmed.len() > 256 {
                            &stderr_trimmed[..256]
                        } else {
                            stderr_trimmed
                        };
                        for line in truncated.lines() {
                            stderr_buf.push(Stream::ProbeStderr, line.to_string());
                        }
                    }

                    let exit_code = output.status.code().unwrap_or(-1);
                    if output.status.success() {
                        true
                    } else {
                        probe_log(
                            stderr_buf,
                            &format!("exec {:?} — {} — exit {}", cmd, attempt_label, exit_code),
                        );
                        false
                    }
                }
                Err(e) => {
                    probe_log(
                        stderr_buf,
                        &format!("exec {:?} — {} — {}", cmd, attempt_label, e),
                    );
                    false
                }
            }
        }

        ProbeKind::Exit(_) => {
            // Exit probes are handled by monitor_child, not by polling.
            // This should not be called for exit probes.
            false
        }
    }
}

/// Perform a minimal HTTP GET and return the status code.
async fn http_probe(url: &str) -> Result<u16, String> {
    // Parse URL: http://host:port/path
    let url = url
        .strip_prefix("http://")
        .ok_or_else(|| "only http:// URLs are supported".to_string())?;

    let (host_port, path) = match url.find('/') {
        Some(i) => (&url[..i], &url[i..]),
        None => (url, "/"),
    };

    let stream = tokio::time::timeout(
        Duration::from_secs(2),
        tokio::net::TcpStream::connect(host_port),
    )
    .await
    .map_err(|_| "connection timed out".to_string())?
    .map_err(|e| e.to_string())?;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let (mut reader, mut writer) = stream.into_split();

    let request = format!("GET {} HTTP/1.0\r\nHost: {}\r\n\r\n", path, host_port);
    writer
        .write_all(request.as_bytes())
        .await
        .map_err(|e| e.to_string())?;

    let mut buf = vec![0u8; 256];
    let n = reader.read(&mut buf).await.map_err(|e| e.to_string())?;
    let response = String::from_utf8_lossy(&buf[..n]);

    // Parse status line: "HTTP/1.x NNN ..."
    let status_line = response.lines().next().unwrap_or("");
    let parts: Vec<&str> = status_line.split_whitespace().collect();
    if parts.len() >= 2 {
        parts[1]
            .parse::<u16>()
            .map_err(|_| format!("invalid status code: {}", parts[1]))
    } else {
        Err(format!("invalid HTTP response: {}", status_line))
    }
}

/// Write a diagnostic log line to the probe:stderr stream.
fn probe_log(buf: &Arc<RingBuffer>, msg: &str) {
    buf.push(Stream::ProbeStderr, msg.to_string());
}

fn probe_label(kind: &ProbeKind) -> String {
    match kind {
        ProbeKind::Tcp(addr) => format!("tcp {}", addr),
        ProbeKind::Http(url) => format!("http {}", url),
        ProbeKind::Exec(cmd) => format!("exec {:?}", cmd),
        ProbeKind::Exit(code) => format!("exit {}", code),
    }
}
