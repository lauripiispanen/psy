//! Curated public API for embedded use of psy-core.
//!
//! This is the surface hosts target: `PsyRoot::start` returns a
//! [`RootHandle`] from which all supervision happens. The handle is
//! `Send + Sync + 'static`-friendly so it can live wherever a host
//! wants to keep it (in a Tauri state, an Axum extension, an Arc field,
//! a Lazy static, etc.).
//!
//! Phase B: this is the first stable surface. Internals in
//! `psy_core::root`, `psy_core::process`, etc. remain `pub` so advanced
//! callers can reach in, but the curated API here is what we'll evolve
//! with SemVer guarantees.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use crate::macos_cleanup::SidecarStrategy;
use crate::process::ProcessState;
use crate::protocol::{
    self, DependencyArg, HistoryArgs, HistoryResponse, LogsArgs, PortDefArg, ProbeArg,
    ProbeKindArg, PsResponse, Request, RestartArgs, RunArgs, StopArgs, StreamFilter,
    WaitFor as ProtoWaitFor,
};
pub use crate::protocol::{ErrorCode, ProcessInfo, RestartPolicy, RunInfo, StreamKind};
use crate::root::{
    handle_request, prepare_root_runtime, teardown, HandleResult, PsyRoot as _Inner,
};

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Library error type for embedded callers.
///
/// Stable variants are `#[non_exhaustive]` so we can add new ones without
/// breaking host code that matches on this. Match with a `_ => …` arm.
#[derive(Debug)]
#[non_exhaustive]
pub enum PsyError {
    /// A unit / process by this name is already running.
    AlreadyExists { name: String },
    /// No process / unit by this name in the table.
    NotFound { name: String },
    /// Name doesn't match `[a-zA-Z0-9][a-zA-Z0-9_-]{0,62}`.
    InvalidName { name: String },
    /// Psyfile parse / validation error.
    PsyfileError(String),
    /// `spawn`/`exec` failed for the given unit.
    SpawnFailed { name: String, message: String },
    /// Port allocation failed.
    PortAllocationFailed { port_name: String },
    /// The root is shutting down and won't accept new work.
    ShuttingDown,
    /// Underlying I/O error (socket bind, file write, etc.).
    Io(std::io::Error),
    /// Any other error surfaced from internals; the wire protocol reports
    /// some failures as plain strings that don't fit the typed variants.
    Other(String),
}

impl std::fmt::Display for PsyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PsyError::AlreadyExists { name } => write!(f, "process '{name}' is already running"),
            PsyError::NotFound { name } => write!(f, "process '{name}' not found"),
            PsyError::InvalidName { name } => {
                write!(
                    f,
                    "invalid name '{name}': must match [a-zA-Z0-9][a-zA-Z0-9_-]{{0,62}}"
                )
            }
            PsyError::PsyfileError(s) => write!(f, "Psyfile error: {s}"),
            PsyError::SpawnFailed { name, message } => {
                write!(f, "spawn '{name}' failed: {message}")
            }
            PsyError::PortAllocationFailed { port_name } => {
                write!(f, "port allocation '{port_name}' failed")
            }
            PsyError::ShuttingDown => f.write_str("psy is shutting down"),
            PsyError::Io(e) => write!(f, "io error: {e}"),
            PsyError::Other(s) => f.write_str(s),
        }
    }
}

impl std::error::Error for PsyError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            PsyError::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<std::io::Error> for PsyError {
    fn from(e: std::io::Error) -> Self {
        PsyError::Io(e)
    }
}

impl PsyError {
    /// Construct a `PsyError` from a protocol response's error fields.
    ///
    /// The wire protocol carries both a typed `ErrorCode` and a human-
    /// readable message; we use the code as the source of truth and the
    /// message for context (e.g. extracting the unit name). Hosts can
    /// match on typed `PsyError` variants stably across psy-core
    /// releases — wire-protocol message wording can change without
    /// affecting which variant fires.
    fn from_response(code: Option<ErrorCode>, message: String) -> Self {
        match code.unwrap_or(ErrorCode::Other) {
            ErrorCode::AlreadyExists => PsyError::AlreadyExists {
                name: extract_name(&message),
            },
            ErrorCode::NotFound => PsyError::NotFound {
                name: extract_name(&message),
            },
            ErrorCode::InvalidName => PsyError::InvalidName {
                name: extract_quoted(&message).unwrap_or_default(),
            },
            ErrorCode::PsyfileError => PsyError::PsyfileError(message),
            ErrorCode::SpawnFailed => PsyError::SpawnFailed {
                name: extract_quoted(&message).unwrap_or_default(),
                message,
            },
            ErrorCode::PortAllocationFailed => PsyError::PortAllocationFailed {
                port_name: extract_quoted(&message).unwrap_or_default(),
            },
            ErrorCode::ShuttingDown => PsyError::ShuttingDown,
            // Codes without a dedicated typed variant fall through as
            // `Other` for now; the message preserves context.
            ErrorCode::InvalidArgs
            | ErrorCode::NotRunning
            | ErrorCode::NotInteractive
            | ErrorCode::StdinClosed
            | ErrorCode::AttachedSessionConflict
            | ErrorCode::NoHistory
            | ErrorCode::NotASubroot
            | ErrorCode::SubrootUnauthorized
            | ErrorCode::Other => PsyError::Other(message),
        }
    }
}

/// Pull a name from `process 'name' …` style messages. Returns "" if
/// the message doesn't have the expected shape; caller falls back to
/// the verbatim message via the `name` field being empty.
fn extract_name(message: &str) -> String {
    extract_quoted(message).unwrap_or_default()
}

/// Pull the first single-quoted substring from a message. Used to
/// recover unit / process / port names embedded in human-readable
/// messages without re-encoding them on the wire.
fn extract_quoted(message: &str) -> Option<String> {
    let start = message.find('\'')? + 1;
    let end = message[start..].find('\'')? + start;
    Some(message[start..end].to_string())
}

// ---------------------------------------------------------------------------
// Options
// ---------------------------------------------------------------------------

/// Configuration for [`PsyRoot::start`].
///
/// `RootOptions::new("name")` returns a sensible default; chain setters /
/// public field assignment to customize.
#[non_exhaustive]
pub struct RootOptions {
    /// Identifier used as the unit name when this root registers as a
    /// sub-root, in log lines, and in the anchor file path.
    pub name: String,
    /// Where to load Psyfile units from. `None` means no Psyfile.
    pub psyfile: Option<PsyfileSource>,
    /// Names of Psyfile units to start immediately at root startup.
    pub boot_units: Vec<String>,
    /// Start every unit in the loaded Psyfile at root startup.
    pub boot_all: bool,
    /// Whether to expose an IPC socket for out-of-process clients.
    pub bind_socket: SocketBinding,
    /// Whether to install host-process cleanup machinery (subreaper /
    /// pdeathsig / Job Object / macOS sidecar). Most embedded hosts want
    /// this on. Hosts already running their own subreaper or inside a
    /// strict Job Object hierarchy may opt out.
    pub install_host_cleanup: bool,
    /// How the macOS cleanup sidecar is spawned. Cross-platform: ignored
    /// on Linux / Windows. Default uses `HostReDispatch` with the default
    /// sentinel; hosts must call
    /// [`crate::dispatch_macos_cleanup_if_invoked`] at the top of `main()`.
    pub sidecar_strategy: SidecarStrategy,
}

impl RootOptions {
    /// Construct options with sensible defaults for an embedded host.
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            psyfile: None,
            boot_units: vec![],
            boot_all: false,
            bind_socket: SocketBinding::None,
            install_host_cleanup: true,
            sidecar_strategy: SidecarStrategy::default(),
        }
    }

    pub fn with_psyfile(mut self, src: PsyfileSource) -> Self {
        self.psyfile = Some(src);
        self
    }

    pub fn with_boot_units(mut self, units: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.boot_units = units.into_iter().map(Into::into).collect();
        self
    }

    pub fn with_boot_all(mut self, all: bool) -> Self {
        self.boot_all = all;
        self
    }

    pub fn with_bind_socket(mut self, b: SocketBinding) -> Self {
        self.bind_socket = b;
        self
    }

    pub fn with_install_host_cleanup(mut self, on: bool) -> Self {
        self.install_host_cleanup = on;
        self
    }

    pub fn with_sidecar_strategy(mut self, s: SidecarStrategy) -> Self {
        self.sidecar_strategy = s;
        self
    }
}

// ---------------------------------------------------------------------------
// Sub-roots
// ---------------------------------------------------------------------------

/// Configuration for a sub-root spawned via [`RootHandle::sub_root`].
#[non_exhaustive]
pub struct SubRootOptions {
    pub name: String,
    pub kind: SubRootKind,
    pub psyfile: Option<PsyfileSource>,
    pub boot_units: Vec<String>,
    pub boot_all: bool,
    pub bind_socket: SocketBinding,
}

impl SubRootOptions {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            kind: SubRootKind::default(),
            psyfile: None,
            boot_units: vec![],
            boot_all: false,
            bind_socket: SocketBinding::Auto,
        }
    }

    pub fn with_kind(mut self, kind: SubRootKind) -> Self {
        self.kind = kind;
        self
    }

    pub fn with_psyfile(mut self, src: PsyfileSource) -> Self {
        self.psyfile = Some(src);
        self
    }

    pub fn with_boot_units(mut self, units: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.boot_units = units.into_iter().map(Into::into).collect();
        self
    }

    pub fn with_boot_all(mut self, all: bool) -> Self {
        self.boot_all = all;
        self
    }

    pub fn with_bind_socket(mut self, b: SocketBinding) -> Self {
        self.bind_socket = b;
        self
    }
}

/// Whether the sub-root runs inside the host's own process or as a
/// separate `psy` invocation. See the libpsy proposal's "in-process vs
/// out-of-process" discussion for the trade-offs.
#[non_exhaustive]
#[derive(Clone, Default)]
pub enum SubRootKind {
    /// Sub-root supervised in the host's own process. Cheap (no extra
    /// process), cheap IPC (none), shares the host's tokio runtime and
    /// macOS cleanup sidecar. Crash isolation: address space is shared,
    /// so a panic in one in-process sub-root may affect siblings. The
    /// recommended default unless host-internal supervisor bugs
    /// crossing sub-root boundaries are a real concern.
    #[default]
    InProcess,
    /// Sub-root is a separate `psy` process registered with the parent
    /// via the `psy up --parent <sock>` mechanism (v1.9). Use when the
    /// sub-root needs full address-space isolation. Costs one extra
    /// process per sub-root and adds an IPC round-trip per spawn.
    ///
    /// **Targeted for v2.1.** v2.0 returns a typed error from
    /// [`RootHandle::sub_root`] when this variant is used; until v2.1
    /// ships, hosts that genuinely need address-space isolation can
    /// shell out via `Spawn::new("instance", ["psy", "up", "--parent",
    /// &parent_sock, "--", ...])` from a [`Spawn`] on the host. The
    /// switch to the typed API in v2.1 will be source-compatible —
    /// just remove the `with_kind(SubRootKind::OutOfProcess { … })`
    /// hint when it lands.
    OutOfProcess { binary: Option<PathBuf> },
}

/// Whether to expose an IPC socket so out-of-process clients (`psy ps`,
/// MCP relays, sibling shells) can find this root.
#[non_exhaustive]
#[derive(Clone, Debug)]
pub enum SocketBinding {
    /// No IPC socket. Host's API surface is the only way in. Recommended
    /// default for embedded hosts that supervise only their own children.
    None,
    /// Default psy paths (PID-keyed in `$XDG_RUNTIME_DIR/psy/` or
    /// `/tmp/psy-<uid>/` on Unix; named pipe on Windows). Anchor files
    /// allow auto-discovery from sibling shells.
    Auto,
    /// Caller-chosen socket / pipe path. Useful for tests, sandboxes,
    /// daemon-style deployments.
    Path(PathBuf),
}

/// Where to load a Psyfile from.
#[non_exhaustive]
pub enum PsyfileSource {
    /// Walk upward from the current directory looking for `Psyfile` /
    /// `Psyfile.toml` (the same discovery the CLI does).
    Auto,
    /// Caller-supplied path; no discovery.
    Path(PathBuf),
}

// ---------------------------------------------------------------------------
// Spawn
// ---------------------------------------------------------------------------

/// Programmatic equivalent of a Psyfile unit. Construct with
/// [`Spawn::new`], chain setters, pass to [`RootHandle::spawn`].
#[non_exhaustive]
pub struct Spawn {
    pub name: String,
    pub argv: Vec<String>,
    pub env: HashMap<String, String>,
    pub restart: RestartPolicy,
    pub interactive: bool,
    /// Named ports to allocate. `(name, default_port)`; `default_port` is
    /// the preferred number, with fallback to OS-assigned.
    pub ports: Vec<(String, Option<u16>)>,
    /// Working directory for the child. `None` inherits the host's cwd.
    pub cwd: Option<PathBuf>,
    /// One-shot readiness probe. Dependents wait for this to pass.
    pub ready: Option<ReadyProbe>,
    /// Continuous health check. Failure triggers restart per
    /// [`Spawn::restart`].
    pub healthcheck: Option<HealthCheck>,
    /// Other supervised processes this one depends on. They must be
    /// already spawned (running or ready); psy waits for each to pass
    /// its `ready` probe before starting this one.
    pub depends_on: Vec<DependencyRef>,
    /// Caller-supplied tags for declarative reconciliation. Stored on
    /// the process entry; psy-core doesn't interpret them.
    pub metadata: HashMap<String, String>,
    /// If set, wait until this condition is met before returning the
    /// `SpawnHandle`. Mirrors `psy run --wait-for`.
    pub wait_for: Option<WaitFor>,
    /// Timeout applied to `wait_for`. Default: 120 seconds.
    pub wait_timeout: Option<Duration>,
}

/// One-shot readiness probe. The supervisor runs this after spawning
/// the process; dependents wait for it to pass before starting.
#[non_exhaustive]
#[derive(Clone, Debug)]
pub enum ReadyProbe {
    /// Connect to a TCP address. `addr` accepts `"host:port"` or just
    /// `"<port>"` (interpreted as `localhost:<port>`).
    Tcp {
        addr: String,
        interval: Option<Duration>,
        timeout: Option<Duration>,
        retries: Option<u32>,
    },
    /// HTTP GET. Ready when the response status is 2xx.
    Http {
        url: String,
        interval: Option<Duration>,
        timeout: Option<Duration>,
        retries: Option<u32>,
    },
    /// Run a shell command. Ready when it exits with code 0.
    Exec {
        command: String,
        interval: Option<Duration>,
        timeout: Option<Duration>,
        retries: Option<u32>,
    },
    /// The supervised process itself exits with `code`. Useful for
    /// build-step / migration units.
    Exit {
        code: i32,
        timeout: Option<Duration>,
    },
}

/// Continuous health check. Runs after the process is ready; on
/// `retries` consecutive failures the process is killed and restarted
/// per its [`RestartPolicy`].
#[non_exhaustive]
#[derive(Clone, Debug)]
pub enum HealthCheck {
    Tcp {
        addr: String,
        interval: Option<Duration>,
        timeout: Option<Duration>,
        retries: Option<u32>,
    },
    Http {
        url: String,
        interval: Option<Duration>,
        timeout: Option<Duration>,
        retries: Option<u32>,
    },
    Exec {
        command: String,
        interval: Option<Duration>,
        timeout: Option<Duration>,
        retries: Option<u32>,
    },
}

/// Reference to a dependency. The dependency's name must match an
/// already-supervised process.
#[non_exhaustive]
#[derive(Clone, Debug)]
pub struct DependencyRef {
    pub name: String,
    /// If true, when the dependency restarts this process restarts too.
    pub restart: bool,
}

impl DependencyRef {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            restart: false,
        }
    }

    pub fn with_restart(mut self, restart: bool) -> Self {
        self.restart = restart;
        self
    }
}

impl Spawn {
    /// New spawn with the given name and argv, all other fields default.
    pub fn new(name: impl Into<String>, argv: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self {
            name: name.into(),
            argv: argv.into_iter().map(Into::into).collect(),
            env: HashMap::new(),
            restart: RestartPolicy::No,
            interactive: false,
            ports: vec![],
            cwd: None,
            ready: None,
            healthcheck: None,
            depends_on: vec![],
            metadata: HashMap::new(),
            wait_for: None,
            wait_timeout: None,
        }
    }

    pub fn with_cwd(mut self, cwd: impl Into<PathBuf>) -> Self {
        self.cwd = Some(cwd.into());
        self
    }

    pub fn with_ready(mut self, ready: ReadyProbe) -> Self {
        self.ready = Some(ready);
        self
    }

    pub fn with_healthcheck(mut self, hc: HealthCheck) -> Self {
        self.healthcheck = Some(hc);
        self
    }

    pub fn with_depends_on(mut self, deps: impl IntoIterator<Item = DependencyRef>) -> Self {
        self.depends_on = deps.into_iter().collect();
        self
    }

    pub fn with_metadata(
        mut self,
        meta: impl IntoIterator<Item = (impl Into<String>, impl Into<String>)>,
    ) -> Self {
        self.metadata = meta
            .into_iter()
            .map(|(k, v)| (k.into(), v.into()))
            .collect();
        self
    }

    pub fn with_env(
        mut self,
        env: impl IntoIterator<Item = (impl Into<String>, impl Into<String>)>,
    ) -> Self {
        self.env = env.into_iter().map(|(k, v)| (k.into(), v.into())).collect();
        self
    }

    pub fn with_restart(mut self, policy: RestartPolicy) -> Self {
        self.restart = policy;
        self
    }

    pub fn with_interactive(mut self, on: bool) -> Self {
        self.interactive = on;
        self
    }

    pub fn with_ports(
        mut self,
        ports: impl IntoIterator<Item = (impl Into<String>, Option<u16>)>,
    ) -> Self {
        self.ports = ports.into_iter().map(|(n, p)| (n.into(), p)).collect();
        self
    }

    pub fn with_wait_for(mut self, condition: WaitFor) -> Self {
        self.wait_for = Some(condition);
        self
    }

    pub fn with_wait_timeout(mut self, d: Duration) -> Self {
        self.wait_timeout = Some(d);
        self
    }
}

/// Block-until-ready conditions for [`Spawn::wait_for`].
#[non_exhaustive]
#[derive(Clone, Debug)]
pub enum WaitFor {
    /// Wait until the process passes its `ready` probe.
    Ready,
    /// Wait until the process exits.
    Exit,
    /// Wait until a log line contains `pattern` (case-insensitive substring).
    Log { pattern: String },
}

/// Returned by [`RootHandle::spawn`] on success.
///
/// Public fields snapshot the spawn outcome. Streaming methods
/// ([`SpawnHandle::stdout`], [`SpawnHandle::stderr`]) and lifecycle
/// methods ([`SpawnHandle::wait`], [`SpawnHandle::stop`],
/// [`SpawnHandle::kill`]) reach back into the supervisor for live data.
#[non_exhaustive]
pub struct SpawnHandle {
    pub name: String,
    /// PID at spawn time. Across restarts the live PID changes; query
    /// `status()` on the parent `RootHandle` for the current value, or
    /// stream lifecycle changes via `events()` (Phase v2.1).
    pub pid: Option<u32>,
    /// Allocated ports keyed by port name. Empty unless the spawn
    /// declared `ports`.
    pub ports: HashMap<String, u16>,
    /// Reference to the supervisor — used by streaming/lifecycle methods.
    pub(crate) shared: Arc<crate::root::SharedRoot>,
}

impl SpawnHandle {
    /// Subscribe to this process's stdout. Returns a `Stream` of
    /// `LogLine`s; each item is a captured stdout line with timestamp.
    /// The stream closes when the process exits and the buffer is
    /// dropped (during `RootHandle::clean` or root shutdown).
    pub async fn stdout(
        &self,
    ) -> Result<impl futures_core::Stream<Item = LogLine> + Send, PsyError> {
        self.subscribe_stream(crate::ring_buffer::Stream::Stdout)
            .await
    }

    /// Subscribe to this process's stderr.
    pub async fn stderr(
        &self,
    ) -> Result<impl futures_core::Stream<Item = LogLine> + Send, PsyError> {
        self.subscribe_stream(crate::ring_buffer::Stream::Stderr)
            .await
    }

    /// Wait for the process to exit. Returns once the supervisor records
    /// the exit; survives across restarts (waits for the *current* run
    /// to exit, then the next, etc. — call once per run).
    pub async fn wait(&self) -> Result<ExitStatus, PsyError> {
        let notify = {
            let table = self.shared.process_table.lock().await;
            let entry = table.get(&self.name).ok_or_else(|| PsyError::NotFound {
                name: self.name.clone(),
            })?;
            if entry.state != crate::process::ProcessState::Running {
                // Already exited; return immediately with what we know.
                return Ok(ExitStatus {
                    exit_code: entry.exit_status,
                    signal: entry.signal.clone(),
                });
            }
            Arc::clone(&entry.exit_notify)
        };
        notify.notified().await;
        let table = self.shared.process_table.lock().await;
        let entry = table.get(&self.name).ok_or_else(|| PsyError::NotFound {
            name: self.name.clone(),
        })?;
        Ok(ExitStatus {
            exit_code: entry.exit_status,
            signal: entry.signal.clone(),
        })
    }

    /// Send SIGTERM (with the supervisor's standard 10s grace before
    /// SIGKILL). Equivalent to `RootHandle::stop(name)`.
    pub async fn stop(&self) -> Result<(), PsyError> {
        let req = Request::stop(crate::protocol::StopArgs {
            name: self.name.clone(),
        });
        match crate::root::handle_request(&self.shared, req).await {
            crate::root::HandleResult::Response(r) => {
                if r.ok {
                    Ok(())
                } else {
                    Err(PsyError::from_response(
                        r.error_code,
                        r.error.unwrap_or_default(),
                    ))
                }
            }
            crate::root::HandleResult::AttachSession { .. } => Err(PsyError::Other(
                "unexpected attach response from stop".into(),
            )),
        }
    }

    /// Send SIGKILL immediately, bypassing the SIGTERM grace period.
    /// Use only when graceful stop is inappropriate (process is wedged,
    /// host is crashing, etc.).
    pub async fn kill(&self) -> Result<(), PsyError> {
        let pid = {
            let table = self.shared.process_table.lock().await;
            table.get(&self.name).and_then(|e| e.pid)
        };
        let pid = pid.ok_or_else(|| PsyError::NotFound {
            name: self.name.clone(),
        })?;
        #[cfg(unix)]
        unsafe {
            libc::kill(pid as i32, libc::SIGKILL);
        }
        #[cfg(windows)]
        {
            // On Windows there's no per-PID SIGKILL; route through stop()
            // which uses TerminateProcess as the SIGTERM-grace fallback.
            let _ = pid;
            return self.stop().await;
        }
        #[cfg(unix)]
        Ok(())
    }

    async fn subscribe_stream(
        &self,
        which: crate::ring_buffer::Stream,
    ) -> Result<impl futures_core::Stream<Item = LogLine> + Send, PsyError> {
        let buf = {
            let table = self.shared.process_table.lock().await;
            let entry = table.get(&self.name).ok_or_else(|| PsyError::NotFound {
                name: self.name.clone(),
            })?;
            match which {
                crate::ring_buffer::Stream::Stdout => Arc::clone(&entry.stdout_buf),
                crate::ring_buffer::Stream::Stderr => Arc::clone(&entry.stderr_buf),
                _ => return Err(PsyError::Other("invalid stream selector".into())),
            }
        };
        let rx = buf.subscribe();
        Ok(async_stream::stream! {
            let mut rx = rx;
            loop {
                match rx.recv().await {
                    Ok(line) => yield LogLine {
                        timestamp: line.timestamp,
                        stream: line.stream.into(),
                        content: line.content,
                    },
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        })
    }
}

/// What the supervisor recorded when a process exited.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct ExitStatus {
    pub exit_code: Option<i32>,
    pub signal: Option<String>,
}

// ---------------------------------------------------------------------------
// Logs
// ---------------------------------------------------------------------------

/// Filter set for [`RootHandle::logs`]. All fields are optional.
#[derive(Default, Clone, Debug)]
#[non_exhaustive]
pub struct LogsQuery {
    pub tail: Option<usize>,
    pub stream: StreamFilter,
    pub since: Option<chrono::DateTime<chrono::Utc>>,
    pub until: Option<chrono::DateTime<chrono::Utc>>,
    pub grep: Option<String>,
    pub run: Option<u32>,
    pub previous: bool,
    pub probe: bool,
}

#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct LogPage {
    pub lines: Vec<LogLine>,
}

#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct LogLine {
    pub timestamp: chrono::DateTime<chrono::Utc>,
    pub stream: StreamKind,
    pub content: String,
}

// ---------------------------------------------------------------------------
// Root entry point
// ---------------------------------------------------------------------------

/// Library entry point. Construct via the static method
/// [`PsyRoot::start`]; you don't construct this struct directly.
pub struct PsyRoot;

impl PsyRoot {
    /// Bring up a psy supervisor in this process, async. Returns a
    /// [`RootHandle`] from which spawning, listing, logs, etc. are driven.
    ///
    /// Semantics:
    /// - Performs platform cleanup setup (subreaper / pdeathsig / Job
    ///   Object / macOS sidecar) when `install_host_cleanup` is true.
    /// - Binds an IPC socket if `bind_socket` is `Auto` or `Path`.
    /// - Loads a Psyfile if `psyfile` is set; honors `boot_units` /
    ///   `boot_all`.
    /// - Returns immediately; the host owns its own lifecycle. Call
    ///   [`RootHandle::shutdown`] when the host wants supervision to end.
    pub async fn start(options: RootOptions) -> Result<RootHandle, PsyError> {
        let RootOptions {
            name,
            psyfile,
            boot_units,
            boot_all,
            bind_socket: _bind_socket,
            install_host_cleanup: _install_host_cleanup,
            sidecar_strategy,
        } = options;

        // Resolve the Psyfile path before constructing the root — the
        // existing PsyRoot::new takes an Option<PathBuf>.
        let psyfile_path: Option<PathBuf> = match psyfile {
            None => None,
            Some(PsyfileSource::Path(p)) => Some(p),
            Some(PsyfileSource::Auto) => {
                crate::psyfile::discover(&std::env::current_dir().unwrap_or_default())
            }
        };

        let inner = _Inner::new_with_strategy(name, psyfile_path, sidecar_strategy)
            .map_err(|e| PsyError::Other(e.to_string()))?;

        // Resolve `boot_all`: if requested, expand to all unit names.
        let boot_units_resolved = if boot_all {
            let pf = inner.shared().load_psyfile();
            match pf {
                Ok(Some(pf)) => pf.units.keys().cloned().collect(),
                Ok(None) => return Err(PsyError::PsyfileError("no Psyfile to boot --all".into())),
                Err(e) => return Err(PsyError::PsyfileError(e)),
            }
        } else {
            boot_units
        };

        let shared = inner.shared();
        prepare_root_runtime(Arc::clone(&shared), boot_units_resolved, None)
            .await
            .map_err(|e| PsyError::Other(e.to_string()))?;

        Ok(RootHandle {
            shared,
            main_exit_tx: inner.main_exit_tx(),
        })
    }
}

// ---------------------------------------------------------------------------
// Root handle
// ---------------------------------------------------------------------------

/// Cloneable host-facing handle to a running psy root. All async methods
/// return immediately if the underlying tokio task can complete, otherwise
/// await the underlying operation. The handle is `Send + Sync`.
#[derive(Clone)]
pub struct RootHandle {
    shared: Arc<crate::root::SharedRoot>,
    main_exit_tx: tokio::sync::watch::Sender<Option<i32>>,
}

impl RootHandle {
    /// Spawn a programmatic process. Returns once the spawn is recorded
    /// (and, if `wait_for` was set, once the wait condition is satisfied
    /// or its timeout expires).
    pub async fn spawn(&self, spawn: Spawn) -> Result<SpawnHandle, PsyError> {
        let ports = spawn
            .ports
            .into_iter()
            .map(|(name, default)| PortDefArg { name, default })
            .collect();
        let wait_for = spawn.wait_for.map(|w| match w {
            WaitFor::Ready => ProtoWaitFor::Ready,
            WaitFor::Exit => ProtoWaitFor::Exit,
            WaitFor::Log { pattern } => ProtoWaitFor::Log { pattern },
        });
        let wait_timeout = spawn.wait_timeout.map(format_duration);

        let req = Request::run(RunArgs {
            name: spawn.name.clone(),
            command: spawn.argv,
            restart: spawn.restart,
            env: spawn.env,
            attach: false,
            interactive: spawn.interactive,
            extra_args: None,
            wait_for,
            wait_timeout,
            ports,
            cwd: spawn.cwd.map(|p| p.to_string_lossy().to_string()),
            ready: spawn.ready.map(ready_probe_to_arg),
            healthcheck: spawn.healthcheck.map(healthcheck_to_arg),
            depends_on: spawn
                .depends_on
                .into_iter()
                .map(|d| DependencyArg {
                    name: d.name,
                    restart: d.restart,
                })
                .collect(),
            metadata: spawn.metadata,
        });
        let resp = self.dispatch(req).await?;
        let pid = resp
            .data
            .as_ref()
            .and_then(|d| d.get("pid"))
            .and_then(|v| v.as_u64())
            .map(|n| n as u32);
        let allocated_ports: HashMap<String, u16> = resp
            .data
            .as_ref()
            .and_then(|d| d.get("ports"))
            .and_then(|v| v.as_object())
            .map(|m| {
                m.iter()
                    .filter_map(|(k, v)| v.as_u64().map(|n| (k.clone(), n as u16)))
                    .collect()
            })
            .unwrap_or_default();

        // PID isn't always in the run response; fall back to the table.
        let pid = if pid.is_some() {
            pid
        } else {
            let table = self.shared.process_table.lock().await;
            table.get(&spawn.name).and_then(|e| e.pid)
        };

        Ok(SpawnHandle {
            name: spawn.name,
            pid,
            ports: allocated_ports,
            shared: Arc::clone(&self.shared),
        })
    }

    /// Run a Psyfile-defined unit (analog of `psy run <name>` for a unit
    /// with no extra arguments).
    pub async fn run_unit(&self, name: &str) -> Result<SpawnHandle, PsyError> {
        let req = Request::run(RunArgs {
            name: name.to_string(),
            command: vec![],
            restart: RestartPolicy::No,
            env: HashMap::new(),
            attach: false,
            interactive: false,
            extra_args: None,
            wait_for: None,
            wait_timeout: None,
            ports: vec![],
            cwd: None,
            ready: None,
            healthcheck: None,
            depends_on: vec![],
            metadata: HashMap::new(),
        });
        let _resp = self.dispatch(req).await?;
        let table = self.shared.process_table.lock().await;
        let pid = table.get(name).and_then(|e| e.pid);
        let port_allocs = self.shared.port_allocations.lock().await;
        let ports = port_allocs.get(name).cloned().unwrap_or_default();
        Ok(SpawnHandle {
            name: name.to_string(),
            pid,
            ports,
            shared: Arc::clone(&self.shared),
        })
    }

    /// Snapshot the process table.
    pub async fn list(&self) -> Result<Vec<ProcessInfo>, PsyError> {
        let resp = self.dispatch(Request::ps()).await?;
        let data = resp
            .data
            .ok_or_else(|| PsyError::Other("empty ps".into()))?;
        let ps: PsResponse =
            serde_json::from_value(data).map_err(|e| PsyError::Other(format!("bad ps: {e}")))?;
        Ok(ps.processes)
    }

    /// Status of a single named process.
    pub async fn status(&self, name: &str) -> Result<ProcessInfo, PsyError> {
        let mut all = self.list().await?;
        all.retain(|p| p.name == name);
        all.into_iter().next().ok_or_else(|| PsyError::NotFound {
            name: name.to_string(),
        })
    }

    /// Run history for a process.
    pub async fn history(&self, name: &str) -> Result<Vec<RunInfo>, PsyError> {
        let resp = self
            .dispatch(Request::history(HistoryArgs {
                name: name.to_string(),
            }))
            .await?;
        let data = resp
            .data
            .ok_or_else(|| PsyError::Other("empty history".into()))?;
        let h: HistoryResponse =
            serde_json::from_value(data).map_err(|e| PsyError::Other(format!("bad hist: {e}")))?;
        Ok(h.runs)
    }

    /// One-shot log query.
    pub async fn logs(&self, name: &str, query: LogsQuery) -> Result<LogPage, PsyError> {
        let req = Request::logs(LogsArgs {
            name: name.to_string(),
            tail: query.tail,
            stream: query.stream,
            since: query.since.map(|t| t.to_rfc3339()),
            until: query.until.map(|t| t.to_rfc3339()),
            grep: query.grep,
            run: query.run,
            previous: query.previous,
            probe: query.probe,
        });
        let resp = self.dispatch(req).await?;
        let data = resp
            .data
            .ok_or_else(|| PsyError::Other("empty logs".into()))?;
        let lines_json = data
            .get("lines")
            .and_then(|v| v.as_array())
            .ok_or_else(|| PsyError::Other("logs missing 'lines'".into()))?;
        let mut lines = Vec::with_capacity(lines_json.len());
        for line in lines_json {
            let timestamp = line
                .get("timestamp")
                .and_then(|v| v.as_str())
                .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                .map(|dt| dt.with_timezone(&chrono::Utc))
                .unwrap_or_else(chrono::Utc::now);
            let stream = line
                .get("stream")
                .and_then(|v| v.as_str())
                .and_then(stream_kind_from_str)
                .unwrap_or(StreamKind::Stdout);
            let content = line
                .get("content")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            lines.push(LogLine {
                timestamp,
                stream,
                content,
            });
        }
        Ok(LogPage { lines })
    }

    /// Stop a single process (SIGTERM → 10s grace → SIGKILL).
    pub async fn stop(&self, name: &str) -> Result<(), PsyError> {
        self.dispatch(Request::stop(StopArgs {
            name: name.to_string(),
        }))
        .await
        .map(|_| ())
    }

    /// Restart a process with the same arguments.
    pub async fn restart(&self, name: &str) -> Result<(), PsyError> {
        self.dispatch(Request::restart(RestartArgs {
            name: name.to_string(),
        }))
        .await
        .map(|_| ())
    }

    /// Remove stopped/failed entries from the process table. Returns the
    /// number of entries removed.
    pub async fn clean(&self) -> Result<usize, PsyError> {
        let resp = self.dispatch(Request::clean()).await?;
        let n = resp
            .data
            .as_ref()
            .and_then(|d| d.get("removed"))
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        Ok(n as usize)
    }

    /// Tear down all supervised processes and the IPC socket. Consumes the
    /// handle. Returns 0 today; future versions may surface a meaningful
    /// exit code (e.g. propagated from a configured "main" unit).
    pub async fn shutdown(self) -> Result<i32, PsyError> {
        self.shared
            .shutting_down
            .store(true, std::sync::atomic::Ordering::Relaxed);
        teardown(Arc::clone(&self.shared)).await;
        let _ = self.main_exit_tx.send(Some(0));
        Ok(0)
    }

    /// Borrow the underlying `SharedRoot`. Escape hatch for advanced
    /// callers that want to reach into internals (the same way the CLI
    /// binary does today). Most hosts should use the typed API above.
    pub fn shared(&self) -> Arc<crate::root::SharedRoot> {
        Arc::clone(&self.shared)
    }

    /// Construct a sub-root: a fresh `RootHandle` that supervises its own
    /// independent process table. In-process sub-roots share the host's
    /// runtime and macOS cleanup sidecar; out-of-process sub-roots spawn
    /// a separate `psy` process registered with the host (not yet
    /// implemented in v2.0).
    pub async fn sub_root(&self, opts: SubRootOptions) -> Result<RootHandle, PsyError> {
        match opts.kind {
            SubRootKind::InProcess => self.inprocess_subroot(opts).await,
            SubRootKind::OutOfProcess { .. } => Err(PsyError::Other(
                "OutOfProcess sub-roots are targeted for v2.1; \
                 use SubRootKind::InProcess in v2.0, or shell out via \
                 `Spawn::new(\"instance\", [\"psy\", \"up\", \"--parent\", parent_sock, \"--\", ...])`"
                    .into(),
            )),
        }
    }

    async fn inprocess_subroot(&self, opts: SubRootOptions) -> Result<RootHandle, PsyError> {
        if opts.name.is_empty() {
            return Err(PsyError::InvalidName { name: opts.name });
        }
        if !crate::process::validate_name(&opts.name) {
            return Err(PsyError::InvalidName { name: opts.name });
        }

        let psyfile_path: Option<PathBuf> = match opts.psyfile {
            None => None,
            Some(PsyfileSource::Path(p)) => Some(p),
            Some(PsyfileSource::Auto) => {
                crate::psyfile::discover(&std::env::current_dir().unwrap_or_default())
            }
        };

        let socket_override: Option<PathBuf> = match opts.bind_socket {
            SocketBinding::None => None, // path is auto-derived (see below)
            SocketBinding::Auto => None, // ditto
            SocketBinding::Path(p) => Some(p),
        };

        let (shared, _exit_rx) = crate::root::PsyRoot::build_inprocess_subroot(
            &self.shared,
            opts.name.clone(),
            psyfile_path,
            socket_override,
        )
        .map_err(|e| PsyError::Other(e.to_string()))?;

        let main_exit_tx = shared.main_exit_tx.clone();

        // Resolve boot_all → boot_units by inspecting the sub-root's Psyfile.
        let boot_units_resolved = if opts.boot_all {
            match shared.load_psyfile() {
                Ok(Some(pf)) => pf.units.keys().cloned().collect(),
                Ok(None) => {
                    return Err(PsyError::PsyfileError(
                        "no Psyfile to boot --all in sub-root".into(),
                    ))
                }
                Err(e) => return Err(PsyError::PsyfileError(e)),
            }
        } else {
            opts.boot_units
        };

        // Wire up the listener / boot units. No parent_sock — this is an
        // in-process sub-root; it doesn't register with anything.
        crate::root::prepare_root_runtime(Arc::clone(&shared), boot_units_resolved, None)
            .await
            .map_err(|e| PsyError::Other(e.to_string()))?;

        Ok(RootHandle {
            shared,
            main_exit_tx,
        })
    }

    async fn dispatch(&self, req: Request) -> Result<protocol::Response, PsyError> {
        match handle_request(&self.shared, req).await {
            HandleResult::Response(r) => {
                if r.ok {
                    Ok(r)
                } else {
                    Err(PsyError::from_response(
                        r.error_code,
                        r.error.unwrap_or_else(|| "unknown".into()),
                    ))
                }
            }
            HandleResult::AttachSession { .. } => Err(PsyError::Other(
                "attach mode not supported via embedded API".into(),
            )),
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn format_duration(d: Duration) -> String {
    // Use the most natural unit. The protocol's parse_duration handles
    // ms/s/m/h suffixes; pick whichever is exact.
    let total_ms = d.as_millis();
    if total_ms == 0 {
        return "0ms".into();
    }
    if total_ms.is_multiple_of(3_600_000) {
        return format!("{}h", total_ms / 3_600_000);
    }
    if total_ms.is_multiple_of(60_000) {
        return format!("{}m", total_ms / 60_000);
    }
    if total_ms.is_multiple_of(1_000) {
        return format!("{}s", total_ms / 1_000);
    }
    format!("{total_ms}ms")
}

fn ready_probe_to_arg(p: ReadyProbe) -> ProbeArg {
    match p {
        ReadyProbe::Tcp {
            addr,
            interval,
            timeout,
            retries,
        } => ProbeArg {
            kind: ProbeKindArg::Tcp { addr },
            interval: interval.map(format_duration),
            timeout: timeout.map(format_duration),
            retries,
        },
        ReadyProbe::Http {
            url,
            interval,
            timeout,
            retries,
        } => ProbeArg {
            kind: ProbeKindArg::Http { url },
            interval: interval.map(format_duration),
            timeout: timeout.map(format_duration),
            retries,
        },
        ReadyProbe::Exec {
            command,
            interval,
            timeout,
            retries,
        } => ProbeArg {
            kind: ProbeKindArg::Exec { command },
            interval: interval.map(format_duration),
            timeout: timeout.map(format_duration),
            retries,
        },
        ReadyProbe::Exit { code, timeout } => ProbeArg {
            kind: ProbeKindArg::Exit { code },
            interval: None,
            timeout: timeout.map(format_duration),
            retries: None,
        },
    }
}

fn healthcheck_to_arg(p: HealthCheck) -> ProbeArg {
    match p {
        HealthCheck::Tcp {
            addr,
            interval,
            timeout,
            retries,
        } => ProbeArg {
            kind: ProbeKindArg::Tcp { addr },
            interval: interval.map(format_duration),
            timeout: timeout.map(format_duration),
            retries,
        },
        HealthCheck::Http {
            url,
            interval,
            timeout,
            retries,
        } => ProbeArg {
            kind: ProbeKindArg::Http { url },
            interval: interval.map(format_duration),
            timeout: timeout.map(format_duration),
            retries,
        },
        HealthCheck::Exec {
            command,
            interval,
            timeout,
            retries,
        } => ProbeArg {
            kind: ProbeKindArg::Exec { command },
            interval: interval.map(format_duration),
            timeout: timeout.map(format_duration),
            retries,
        },
    }
}

fn stream_kind_from_str(s: &str) -> Option<StreamKind> {
    match s {
        "stdout" => Some(StreamKind::Stdout),
        "stderr" => Some(StreamKind::Stderr),
        "probe_stdout" | "probe:stdout" => Some(StreamKind::ProbeStdout),
        "probe_stderr" | "probe:stderr" => Some(StreamKind::ProbeStderr),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Internal accessor — see `shared` and `main_exit_tx`
// declared on `crate::root::PsyRoot`. Kept module-private so the public
// API stays clean.
// ---------------------------------------------------------------------------

#[allow(dead_code)]
fn _ensure_state_used(state: &ProcessState) {
    let _ = state;
}
