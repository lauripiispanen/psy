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
    /// Machine-readable error classification. Present on every error
    /// response from a v2.0+ root; absent on success and on responses
    /// from older roots. Hosts using the embedded API should match on
    /// this rather than parsing `error` strings.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_code: Option<ErrorCode>,
}

/// Machine-readable classification of a protocol error.
///
/// `#[non_exhaustive]` so additive evolution is non-breaking. External
/// matchers must include a `_ => …` arm.
///
/// Where to set it: handler call sites use [`Response::err`], which
/// inspects the error message via [`ErrorCode::classify_message`] and
/// sets the appropriate code. Handlers that want explicit control use
/// [`Response::err_code`] directly. The wire contract is stable for
/// hosts: changing the human-readable error message is non-breaking
/// as long as `classify_message` is updated to keep mapping the new
/// message to the same `ErrorCode`.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// A unit / process by this name is already running.
    AlreadyExists,
    /// No process / unit by this name in the table.
    NotFound,
    /// Name doesn't match `[a-zA-Z0-9][a-zA-Z0-9_-]{0,62}`.
    InvalidName,
    /// Psyfile parse, validation, or discovery error.
    PsyfileError,
    /// `spawn` / `exec` of a child process failed.
    SpawnFailed,
    /// Port allocation failed (preferred port taken; OS allocation refused).
    PortAllocationFailed,
    /// The root is shutting down and won't accept new work.
    ShuttingDown,
    /// Caller-supplied request was malformed or missing required args.
    InvalidArgs,
    /// Stop / restart targeted a process that wasn't running.
    NotRunning,
    /// Send / send_wait: process wasn't started in interactive mode.
    NotInteractive,
    /// Send / send_wait: process's stdin has been closed.
    StdinClosed,
    /// Send: process has an attached session that owns stdin.
    AttachedSessionConflict,
    /// Run history requested for a name that has no recorded runs.
    NoHistory,
    /// `--in` / sub-root register / etc. failed because the named unit
    /// is not a sub-root or hasn't yet registered.
    NotASubroot,
    /// Sub-root registration was rejected because the registering pid
    /// is not a descendant of the parent psy.
    SubrootUnauthorized,
    /// Catch-all for failures not yet classified. Hosts that rely on
    /// this for SemVer-stable matching should file an issue so the
    /// specific case gets its own variant.
    Other,
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
pub const CMD_REGISTER_SUBROOT: &str = "register_subroot";

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
    /// Capture child stdout / stderr as raw byte chunks in addition to
    /// the line-tokenized ring buffer. Used by the embedded API
    /// ([`crate::api::SpawnHandle::stdout_bytes`]) for hosts that need
    /// byte-faithful framing (Content-Length JSON-RPC, length-prefixed
    /// binary protocols, etc.). No effect on the wire CLI/MCP paths.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub raw_stdio: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extra_args: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wait_for: Option<WaitFor>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wait_timeout: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ports: Vec<PortDefArg>,
    /// Working directory for the spawned child. If `None`, inherit the
    /// host's cwd. Programmatic equivalent of Psyfile `working_dir`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// Optional readiness probe. Programmatic equivalent of Psyfile
    /// `ready = { … }`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ready: Option<ProbeArg>,
    /// Optional healthcheck. Programmatic equivalent of Psyfile
    /// `healthcheck = { … }`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub healthcheck: Option<ProbeArg>,
    /// Names of already-spawned units that must be ready before this
    /// process is started. Programmatic equivalent of Psyfile
    /// `depends_on = […]`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub depends_on: Vec<DependencyArg>,
    /// Caller-supplied tags for declarative reconciliation. psy-core
    /// stores these on the process entry but doesn't interpret them;
    /// the host's reconciliation loop reads them via `ProcessInfo`.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub metadata: HashMap<String, String>,
}

/// Wire-friendly probe configuration. Same shape on the wire whether
/// used as `ready` or `healthcheck`; the consumer interprets it
/// per-context (one-shot for `ready`, continuous for `healthcheck`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProbeArg {
    #[serde(flatten)]
    pub kind: ProbeKindArg,
    /// Duration string like `"1s"`, `"500ms"`, `"2m"`. Defaults: 1s for
    /// readiness probes, 10s for healthchecks (resolved at the handler).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interval: Option<String>,
    /// Duration string for the per-attempt or overall timeout. Default
    /// 30s (resolved at the handler).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout: Option<String>,
    /// Maximum probe attempts. `None` = unlimited (or until `timeout`
    /// elapses). For healthchecks: consecutive failures before the
    /// process is killed and restarted per its policy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retries: Option<u32>,
}

/// Probe variant. Matches the Psyfile probe table shape.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProbeKindArg {
    /// TCP connect to `addr`. `addr` accepts `"host:port"` or just a
    /// numeric port (interpreted as `localhost:<port>`).
    Tcp { addr: String },
    /// HTTP GET to `url`. Considered ready when the response is 2xx.
    Http { url: String },
    /// Run a shell command. Considered ready when it exits 0.
    Exec { command: String },
    /// The supervised process itself exits with `code`. Valid for `ready`
    /// only; not allowed for `healthcheck`.
    Exit { code: i32 },
}

/// Reference to another supervised process this one depends on.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DependencyArg {
    pub name: String,
    /// If true, restarts of the dependency cascade to this process.
    #[serde(default)]
    pub restart: bool,
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

/// Sent by a sub-root to its parent so the parent can record the sub-root's
/// socket path and authorize the connection.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegisterSubrootArgs {
    /// The unit name the sub-root expects to be registered as.
    pub name: String,
    /// The sub-root's own socket/pipe path (clients can drill in via `--in`).
    pub socket_path: String,
    /// The sub-root's own PID — must be a descendant of the parent psy.
    pub pid: u32,
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
    /// Set when this unit is a managed sub-root and registration succeeded.
    /// Clients use this for `--in <name>` proxying.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subroot_socket: Option<String>,
    /// Set when this unit was started as a sub-root (`sub_root = true` or
    /// `psy up --parent`), regardless of whether registration has completed.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub is_subroot: bool,
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

    pub fn register_subroot(args: RegisterSubrootArgs) -> Self {
        Self::new(
            CMD_REGISTER_SUBROOT,
            Some(serde_json::to_value(args).expect("serialize RegisterSubrootArgs")),
        )
    }
}

impl Response {
    pub fn ok(id: impl Into<String>, data: Option<Value>) -> Self {
        Self {
            id: id.into(),
            ok: true,
            data,
            error: None,
            error_code: None,
        }
    }

    /// Construct an error response. The error message is classified via
    /// [`ErrorCode::classify_message`] so embedded callers get a stable
    /// `error_code` without handlers having to choose one explicitly.
    pub fn err(id: impl Into<String>, error: impl Into<String>) -> Self {
        let msg = error.into();
        let code = ErrorCode::classify_message(&msg);
        Self {
            id: id.into(),
            ok: false,
            data: None,
            error: Some(msg),
            error_code: Some(code),
        }
    }

    /// Construct an error response with an explicit `ErrorCode`. Use this
    /// when the message text wouldn't classify correctly (rare) or when
    /// you want to lock the code regardless of how the message changes.
    pub fn err_code(id: impl Into<String>, code: ErrorCode, error: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            ok: false,
            data: None,
            error: Some(error.into()),
            error_code: Some(code),
        }
    }
}

impl ErrorCode {
    /// Classify a human-readable error message into a typed code. Match
    /// is by substring/prefix on stable phrases; if a handler changes
    /// its message wording, this match arm should be updated to keep
    /// the wire `ErrorCode` stable.
    ///
    /// Falls back to [`ErrorCode::Other`] for unclassified messages.
    pub fn classify_message(message: &str) -> Self {
        // Exact / unique-phrase matches first.
        if message == "server is shutting down" || message == "psy is shutting down" {
            return ErrorCode::ShuttingDown;
        }

        // Process-name'd messages: pattern is "process '<name>' <verb...>"
        if message.starts_with("process '") {
            if message.contains(" is already running") || message.contains("already running") {
                return ErrorCode::AlreadyExists;
            }
            if message.contains(" not found") {
                return ErrorCode::NotFound;
            }
            if message.contains("is not running") {
                return ErrorCode::NotRunning;
            }
            if message.contains("interactive mode")
                || message.contains("not started with interactive")
            {
                return ErrorCode::NotInteractive;
            }
            if message.contains("attached session") {
                return ErrorCode::AttachedSessionConflict;
            }
        }

        // Stdin-closed for an interactive process.
        if message.starts_with("stdin for '") || message.contains("stdin has been closed") {
            return ErrorCode::StdinClosed;
        }

        // Spawn failures.
        if message.starts_with("spawn failed:") || message.starts_with("spawn '") {
            return ErrorCode::SpawnFailed;
        }
        if message.starts_with("restart spawn failed:") {
            return ErrorCode::SpawnFailed;
        }

        // Port allocation.
        if message.starts_with("failed to allocate port") {
            return ErrorCode::PortAllocationFailed;
        }

        // Name validation.
        if message.starts_with("invalid name") || message.starts_with("invalid sub-root unit name")
        {
            return ErrorCode::InvalidName;
        }

        // Sub-root authorization.
        if message.contains("not a descendant of parent psy")
            || message.contains("does not match spawned child")
        {
            return ErrorCode::SubrootUnauthorized;
        }
        if message.contains("not a sub-root")
            || message.starts_with("no unit '")
            || message.contains("was not started as a sub-root")
        {
            return ErrorCode::NotASubroot;
        }

        // Psyfile / dependency / cycle / discovery.
        if message.starts_with("Psyfile error:")
            || message.starts_with("circular dependency")
            || message.contains("depends_on references unknown")
            || message.contains("dependency error:")
            || message.contains("readiness probe timed out")
            || message.contains("no Psyfile")
        {
            return ErrorCode::PsyfileError;
        }

        // History.
        if message == "no previous run"
            || message.starts_with("run ") && message.contains(" not found")
        {
            return ErrorCode::NoHistory;
        }

        // Args validation.
        if message.starts_with("invalid or missing")
            || message.starts_with("either '")
            || message.starts_with("invalid since timestamp")
            || message.starts_with("invalid until timestamp")
            || message.starts_with("invalid grep pattern")
            || message.starts_with("invalid timeout")
            || message.starts_with("invalid idle_timeout")
            || message == "cannot stop the main process (use 'down' instead)"
            || message.starts_with("no command provided")
            || message.starts_with("sub-root socket_path must not be empty")
            || message.starts_with("invalid JSON:")
        {
            return ErrorCode::InvalidArgs;
        }

        ErrorCode::Other
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_already_running() {
        assert_eq!(
            ErrorCode::classify_message("process 'foo' is already running"),
            ErrorCode::AlreadyExists
        );
    }

    #[test]
    fn classify_not_found() {
        assert_eq!(
            ErrorCode::classify_message("process 'bar' not found"),
            ErrorCode::NotFound
        );
    }

    #[test]
    fn classify_not_running() {
        assert_eq!(
            ErrorCode::classify_message("process 'baz' is not running"),
            ErrorCode::NotRunning
        );
    }

    #[test]
    fn classify_spawn_failed() {
        assert_eq!(
            ErrorCode::classify_message("spawn failed: ENOENT"),
            ErrorCode::SpawnFailed
        );
        assert_eq!(
            ErrorCode::classify_message("spawn 'name' failed: blah"),
            ErrorCode::SpawnFailed
        );
    }

    #[test]
    fn classify_port_allocation() {
        assert_eq!(
            ErrorCode::classify_message("failed to allocate port 'http': bind failure"),
            ErrorCode::PortAllocationFailed
        );
    }

    #[test]
    fn classify_shutting_down() {
        assert_eq!(
            ErrorCode::classify_message("server is shutting down"),
            ErrorCode::ShuttingDown
        );
    }

    #[test]
    fn classify_psyfile_error() {
        assert_eq!(
            ErrorCode::classify_message("Psyfile error: invalid TOML"),
            ErrorCode::PsyfileError
        );
        assert_eq!(
            ErrorCode::classify_message("circular dependency: a → b → a"),
            ErrorCode::PsyfileError
        );
    }

    #[test]
    fn classify_subroot_unauthorized() {
        assert_eq!(
            ErrorCode::classify_message(
                "sub-root pid 1234 is not a descendant of parent psy pid 5678"
            ),
            ErrorCode::SubrootUnauthorized
        );
    }

    #[test]
    fn classify_invalid_args() {
        assert_eq!(
            ErrorCode::classify_message("invalid or missing run args"),
            ErrorCode::InvalidArgs
        );
        assert_eq!(
            ErrorCode::classify_message("cannot stop the main process (use 'down' instead)"),
            ErrorCode::InvalidArgs
        );
    }

    #[test]
    fn classify_unknown_falls_back_to_other() {
        assert_eq!(
            ErrorCode::classify_message("something completely unexpected"),
            ErrorCode::Other
        );
    }

    #[test]
    fn err_constructor_sets_code() {
        let r = Response::err("rid", "process 'x' is already running");
        assert_eq!(r.error_code, Some(ErrorCode::AlreadyExists));
        assert_eq!(r.error.as_deref(), Some("process 'x' is already running"));
    }

    #[test]
    fn err_code_constructor_locks_explicit_code() {
        let r = Response::err_code("rid", ErrorCode::Other, "anything");
        assert_eq!(r.error_code, Some(ErrorCode::Other));
    }
}
