use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Core request / response envelopes
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Request {
    pub id: String,
    pub cmd: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub args: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Response {
    pub id: String,
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Streamed log line sent during a `logs_follow` session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogLineResponse {
    pub id: String,
    pub name: String,
    pub timestamp: String,
    pub stream: StreamKind,
    pub content: String,
}

/// Stdin data sent from client to root during an attach session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StdinData {
    pub stdin: String,
}

/// Sent by root when an attach session ends because the child exited.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DetachNotice {
    pub detached: bool,
    pub reason: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
}

// ---------------------------------------------------------------------------
// Commands (as constants for easy matching)
// ---------------------------------------------------------------------------

pub const CMD_RUN: &str = "run";
pub const CMD_PS: &str = "ps";
pub const CMD_LOGS: &str = "logs";
pub const CMD_LOGS_FOLLOW: &str = "logs_follow";
pub const CMD_STOP: &str = "stop";
pub const CMD_RESTART: &str = "restart";
pub const CMD_DOWN: &str = "down";
pub const CMD_HISTORY: &str = "history";
pub const CMD_SEND: &str = "send";
pub const CMD_SEND_WAIT: &str = "send_wait";
pub const CMD_CLEAN: &str = "clean";

// ---------------------------------------------------------------------------
// Argument / payload types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunArgs {
    pub name: String,
    #[serde(default)]
    pub command: Vec<String>,
    #[serde(default)]
    pub restart: RestartPolicy,
    #[serde(default)]
    pub env: HashMap<String, String>,
    #[serde(default)]
    pub attach: bool,
    #[serde(default)]
    pub interactive: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extra_args: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wait_for: Option<WaitFor>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wait_timeout: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ports: Vec<PortDefArg>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PortDefArg {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<u16>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WaitFor {
    Ready,
    Exit,
    Log { pattern: String },
    Dependency { name: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SendArgs {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<String>,
    #[serde(default)]
    pub eof: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SendWaitArgs {
    pub name: String,
    pub input: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idle_timeout: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum RestartPolicy {
    #[default]
    No,
    OnFailure,
    Always,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogsArgs {
    pub name: String,
    #[serde(default)]
    pub tail: Option<usize>,
    #[serde(default)]
    pub stream: StreamFilter,
    #[serde(default)]
    pub since: Option<String>,
    #[serde(default)]
    pub until: Option<String>,
    #[serde(default)]
    pub grep: Option<String>,
    #[serde(default)]
    pub run: Option<u32>,
    #[serde(default)]
    pub previous: bool,
    #[serde(default)]
    pub probe: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryArgs {
    pub name: String,
}

// ---------------------------------------------------------------------------
// History response payload
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunInfo {
    pub run_id: u32,
    pub status: String,
    pub exit_code: Option<i32>,
    pub signal: Option<String>,
    pub started_at: Option<String>,
    pub duration_secs: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryResponse {
    pub name: String,
    pub runs: Vec<RunInfo>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum StreamFilter {
    #[default]
    All,
    Stdout,
    Stderr,
    Probe,
    ProbeStdout,
    ProbeStderr,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StreamKind {
    Stdout,
    Stderr,
    ProbeStdout,
    ProbeStderr,
}

impl From<crate::ring_buffer::Stream> for StreamKind {
    fn from(s: crate::ring_buffer::Stream) -> Self {
        match s {
            crate::ring_buffer::Stream::Stdout => StreamKind::Stdout,
            crate::ring_buffer::Stream::Stderr => StreamKind::Stderr,
            crate::ring_buffer::Stream::ProbeStdout => StreamKind::ProbeStdout,
            crate::ring_buffer::Stream::ProbeStderr => StreamKind::ProbeStderr,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StopArgs {
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RestartArgs {
    pub name: String,
}

// ---------------------------------------------------------------------------
// Ps response payload
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProcessInfo {
    pub name: String,
    pub pid: Option<u32>,
    pub status: String,
    pub restart_policy: RestartPolicy,
    pub started_at: Option<String>,
    pub uptime_secs: Option<u64>,
    pub exit_code: Option<i32>,
    pub signal: Option<String>,
    pub restarts: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ready: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ports: Option<HashMap<String, u16>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PsResponse {
    pub processes: Vec<ProcessInfo>,
}

// ---------------------------------------------------------------------------
// Helper constructors
// ---------------------------------------------------------------------------

impl Request {
    /// Create a new request with a random UUID.
    pub fn new(cmd: impl Into<String>, args: Option<Value>) -> Self {
        Self {
            id: Uuid::new_v4().to_string(),
            cmd: cmd.into(),
            args,
        }
    }

    pub fn run(args: RunArgs) -> Self {
        Self::new(
            CMD_RUN,
            Some(serde_json::to_value(args).expect("serialize RunArgs")),
        )
    }

    pub fn ps() -> Self {
        Self::new(CMD_PS, None)
    }

    pub fn logs(args: LogsArgs) -> Self {
        Self::new(
            CMD_LOGS,
            Some(serde_json::to_value(args).expect("serialize LogsArgs")),
        )
    }

    pub fn logs_follow(args: LogsArgs) -> Self {
        Self::new(
            CMD_LOGS_FOLLOW,
            Some(serde_json::to_value(args).expect("serialize LogsArgs")),
        )
    }

    pub fn stop(args: StopArgs) -> Self {
        Self::new(
            CMD_STOP,
            Some(serde_json::to_value(args).expect("serialize StopArgs")),
        )
    }

    pub fn restart(args: RestartArgs) -> Self {
        Self::new(
            CMD_RESTART,
            Some(serde_json::to_value(args).expect("serialize RestartArgs")),
        )
    }

    pub fn down() -> Self {
        Self::new(CMD_DOWN, None)
    }

    pub fn history(args: HistoryArgs) -> Self {
        Self::new(
            CMD_HISTORY,
            Some(serde_json::to_value(args).expect("serialize HistoryArgs")),
        )
    }

    pub fn send(args: SendArgs) -> Self {
        Self::new(
            CMD_SEND,
            Some(serde_json::to_value(args).expect("serialize SendArgs")),
        )
    }

    pub fn send_wait(args: SendWaitArgs) -> Self {
        Self::new(
            CMD_SEND_WAIT,
            Some(serde_json::to_value(args).expect("serialize SendWaitArgs")),
        )
    }

    pub fn clean() -> Self {
        Self::new(CMD_CLEAN, None)
    }
}

impl Response {
    pub fn ok(id: impl Into<String>, data: Option<Value>) -> Self {
        Self {
            id: id.into(),
            ok: true,
            data,
            error: None,
        }
    }

    pub fn err(id: impl Into<String>, error: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            ok: false,
            data: None,
            error: Some(error.into()),
        }
    }
}
