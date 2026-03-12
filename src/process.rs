use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};

use crate::platform;
use crate::protocol::{ProcessInfo, RestartPolicy};
use crate::ring_buffer::{RingBuffer, Stream};

// ---------------------------------------------------------------------------
// Process state
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProcessState {
    Running,
    Stopped,
    Failed,
}

impl std::fmt::Display for ProcessState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProcessState::Running => write!(f, "running"),
            ProcessState::Stopped => write!(f, "stopped"),
            ProcessState::Failed => write!(f, "failed"),
        }
    }
}

// ---------------------------------------------------------------------------
// Process entry
// ---------------------------------------------------------------------------

pub struct ProcessEntry {
    pub name: String,
    pub command: Vec<String>,
    pub env: HashMap<String, String>,
    pub restart_policy: RestartPolicy,
    pub state: ProcessState,
    pub pid: Option<u32>,
    pub exit_status: Option<i32>,
    pub signal: Option<String>,
    pub started_at: Option<DateTime<Utc>>,
    pub stopped_at: Option<DateTime<Utc>>,
    pub restarts: u32,
    pub stdout_buf: Arc<RingBuffer>,
    pub stderr_buf: Arc<RingBuffer>,
    pub is_main: bool,
    /// Handle to the running child — only present while Running.
    pub child: Option<Child>,
}

impl ProcessEntry {
    pub fn new(
        name: String,
        command: Vec<String>,
        env: HashMap<String, String>,
        restart_policy: RestartPolicy,
        is_main: bool,
    ) -> Self {
        Self {
            name,
            command,
            env,
            restart_policy,
            state: ProcessState::Stopped,
            pid: None,
            exit_status: None,
            signal: None,
            started_at: None,
            stopped_at: None,
            restarts: 0,
            stdout_buf: Arc::new(RingBuffer::new()),
            stderr_buf: Arc::new(RingBuffer::new()),
            is_main,
            child: None,
        }
    }

    /// Convert to the protocol's `ProcessInfo` for ps output.
    pub fn to_ps_entry(&self) -> ProcessInfo {
        let uptime_secs = if self.state == ProcessState::Running {
            self.started_at.map(|t| {
                let dur = Utc::now().signed_duration_since(t);
                dur.num_seconds().max(0) as u64
            })
        } else {
            None
        };

        ProcessInfo {
            name: self.name.clone(),
            pid: self.pid,
            status: self.state.to_string(),
            restart_policy: self.restart_policy,
            started_at: self.started_at.map(|t| t.to_rfc3339()),
            uptime_secs,
        }
    }
}

// ---------------------------------------------------------------------------
// Name validation
// ---------------------------------------------------------------------------

/// Validate a process name: must match `[a-zA-Z0-9][a-zA-Z0-9_-]{0,62}`.
pub fn validate_name(name: &str) -> bool {
    if name.is_empty() || name.len() > 63 {
        return false;
    }

    let bytes = name.as_bytes();

    // First character: alphanumeric only
    if !bytes[0].is_ascii_alphanumeric() {
        return false;
    }

    // Remaining characters: alphanumeric, underscore, or hyphen
    bytes[1..]
        .iter()
        .all(|&b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

// ---------------------------------------------------------------------------
// Spawn a child process
// ---------------------------------------------------------------------------

pub fn spawn_child(
    entry: &mut ProcessEntry,
    psy_sock: &str,
    psy_root_pid: u32,
) -> std::io::Result<Child> {
    if entry.command.is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "empty command",
        ));
    }

    let mut cmd = Command::new(&entry.command[0]);
    cmd.args(&entry.command[1..]);

    // Environment: inherit current env + PSY variables + entry-specific env
    cmd.env("PSY_SOCK", psy_sock);
    cmd.env("PSY_ROOT_PID", psy_root_pid.to_string());
    for (k, v) in &entry.env {
        cmd.env(k, v);
    }

    if entry.is_main {
        // Main process: inherit stdin/stdout/stderr (passthrough)
        cmd.stdin(std::process::Stdio::inherit());
        cmd.stdout(std::process::Stdio::inherit());
        cmd.stderr(std::process::Stdio::inherit());
    } else {
        // Non-main: stdin from /dev/null, stdout/stderr piped
        cmd.stdin(std::process::Stdio::null());
        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::piped());
    }

    // Unix-specific pre_exec hook
    #[cfg(unix)]
    {
        let mut hook = platform::pre_exec_hook();
        unsafe { cmd.pre_exec(move || hook()) };
    }

    let mut child = cmd.spawn()?;

    // Record the PID and timestamps
    entry.pid = child.id();
    entry.state = ProcessState::Running;
    entry.started_at = Some(Utc::now());
    entry.stopped_at = None;
    entry.exit_status = None;
    entry.signal = None;

    // Start output capture tasks for non-main processes
    if !entry.is_main {
        if let Some(stdout) = child.stdout.take() {
            let buf = Arc::clone(&entry.stdout_buf);
            tokio::spawn(capture_output(stdout, buf));
        }
        if let Some(stderr) = child.stderr.take() {
            let buf = Arc::clone(&entry.stderr_buf);
            tokio::spawn(capture_stderr(stderr, buf));
        }
    }

    Ok(child)
}

// ---------------------------------------------------------------------------
// Output capture
// ---------------------------------------------------------------------------

/// Read lines from a child's stdout and push them into the ring buffer.
pub async fn capture_output(
    stdout: tokio::process::ChildStdout,
    buf: Arc<RingBuffer>,
) {
    let reader = BufReader::new(stdout);
    let mut lines = reader.lines();
    while let Ok(Some(line)) = lines.next_line().await {
        buf.push(Stream::Stdout, line);
    }
}

/// Read lines from a child's stderr and push them into the ring buffer.
async fn capture_stderr(
    stderr: tokio::process::ChildStderr,
    buf: Arc<RingBuffer>,
) {
    let reader = BufReader::new(stderr);
    let mut lines = reader.lines();
    while let Ok(Some(line)) = lines.next_line().await {
        buf.push(Stream::Stderr, line);
    }
}

// ---------------------------------------------------------------------------
// Restart logic
// ---------------------------------------------------------------------------

const MAX_RESTARTS: u32 = 5;

/// Calculate exponential backoff: 1s, 2s, 4s, 8s, 16s (capped).
pub fn calculate_backoff(restarts: u32) -> Duration {
    let secs = 1u64 << restarts.min(4); // 2^0=1, 2^1=2, ..., 2^4=16
    Duration::from_secs(secs)
}

/// Determine whether a process should be restarted based on its policy and state.
pub fn should_restart(entry: &ProcessEntry, exit_code: Option<i32>) -> bool {
    if entry.restarts >= MAX_RESTARTS {
        return false;
    }

    match entry.restart_policy {
        RestartPolicy::No => false,
        RestartPolicy::Always => true,
        RestartPolicy::OnFailure => {
            // Restart only if the exit code is non-zero (or unknown/signal)
            !matches!(exit_code, Some(0))
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -- validate_name -------------------------------------------------------

    #[test]
    fn valid_names() {
        assert!(validate_name("a"));
        assert!(validate_name("myapp"));
        assert!(validate_name("my-app"));
        assert!(validate_name("my_app"));
        assert!(validate_name("App123"));
        assert!(validate_name("a1-b2_c3"));
        assert!(validate_name("X"));
        assert!(validate_name("0"));
    }

    #[test]
    fn invalid_names() {
        assert!(!validate_name(""));
        assert!(!validate_name("-start"));
        assert!(!validate_name("_start"));
        assert!(!validate_name("has space"));
        assert!(!validate_name("has.dot"));
        assert!(!validate_name("has/slash"));
        // 64 characters is too long (max 63)
        let long_name = "a".repeat(64);
        assert!(!validate_name(&long_name));
        // 63 characters is fine
        let max_name = "a".repeat(63);
        assert!(validate_name(&max_name));
    }

    // -- calculate_backoff ---------------------------------------------------

    #[test]
    fn backoff_values() {
        assert_eq!(calculate_backoff(0), Duration::from_secs(1));
        assert_eq!(calculate_backoff(1), Duration::from_secs(2));
        assert_eq!(calculate_backoff(2), Duration::from_secs(4));
        assert_eq!(calculate_backoff(3), Duration::from_secs(8));
        assert_eq!(calculate_backoff(4), Duration::from_secs(16));
        // Capped at 16s
        assert_eq!(calculate_backoff(5), Duration::from_secs(16));
        assert_eq!(calculate_backoff(100), Duration::from_secs(16));
    }

    // -- should_restart ------------------------------------------------------

    fn make_entry(policy: RestartPolicy, restarts: u32) -> ProcessEntry {
        let mut e = ProcessEntry::new(
            "test".into(),
            vec!["echo".into()],
            HashMap::new(),
            policy,
            false,
        );
        e.restarts = restarts;
        e
    }

    #[test]
    fn no_restart_policy() {
        let entry = make_entry(RestartPolicy::No, 0);
        assert!(!should_restart(&entry, Some(1)));
        assert!(!should_restart(&entry, Some(0)));
        assert!(!should_restart(&entry, None));
    }

    #[test]
    fn always_restart_policy() {
        let entry = make_entry(RestartPolicy::Always, 0);
        assert!(should_restart(&entry, Some(0)));
        assert!(should_restart(&entry, Some(1)));
        assert!(should_restart(&entry, None));
    }

    #[test]
    fn always_restart_max_reached() {
        let entry = make_entry(RestartPolicy::Always, 5);
        assert!(!should_restart(&entry, Some(1)));
    }

    #[test]
    fn on_failure_restart_policy() {
        let entry = make_entry(RestartPolicy::OnFailure, 0);
        assert!(!should_restart(&entry, Some(0)));
        assert!(should_restart(&entry, Some(1)));
        assert!(should_restart(&entry, Some(137)));
        assert!(should_restart(&entry, None));
    }

    #[test]
    fn on_failure_max_reached() {
        let entry = make_entry(RestartPolicy::OnFailure, 5);
        assert!(!should_restart(&entry, Some(1)));
    }
}
