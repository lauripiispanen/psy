//! psy-core — embeddable cross-platform process supervisor.
//!
//! This crate is the supervisor logic that powers the `psy` binary. It can
//! also be embedded directly in a host process to supervise children without
//! a separate `psy` process.
//!
//! ## Quick start (embedded)
//!
//! ```no_run
//! use psy_core::{PsyRoot, RootOptions, Spawn, RestartPolicy};
//!
//! // At the very top of your `main()` — before any other initialization.
//! psy_core::dispatch_macos_cleanup_if_invoked();
//!
//! # async fn run() -> Result<(), Box<dyn std::error::Error>> {
//! let root = PsyRoot::start(RootOptions::new("my-host")).await?;
//! let _h = root
//!     .spawn(Spawn::new("worker", ["my-program", "--flag"])
//!         .with_restart(RestartPolicy::OnFailure))
//!     .await?;
//! // ... do host work ...
//! root.shutdown().await?;
//! # Ok(()) }
//! ```
//!
//! ## Modules
//!
//! The curated public API lives at the crate root via re-exports below.
//! The underlying modules (`protocol`, `process`, `root`, `psyfile`, …)
//! stay `pub` so advanced callers and the in-tree CLI / MCP / client crates
//! can reach in. Only the items re-exported here carry SemVer guarantees;
//! anything else is subject to change.

pub mod api;
pub mod macos_cleanup;
pub mod platform;
pub mod probe;
pub mod process;
pub mod protocol;
pub mod psyfile;
pub mod ring_buffer;
pub mod root;

// Curated public surface.
pub use api::{
    DependencyRef, ErrorCode, HealthCheck, LogLine, LogPage, LogsQuery, ProcessInfo, PsyError,
    PsyRoot, PsyfileSource, ReadyProbe, RestartPolicy, RootHandle, RootOptions, RunInfo,
    SocketBinding, Spawn, SpawnHandle, StreamKind, SubRootKind, SubRootOptions, WaitFor,
};
pub use macos_cleanup::{
    dispatch_macos_cleanup_if_invoked, dispatch_macos_cleanup_if_invoked_with_sentinel,
    SidecarStrategy, DEFAULT_SENTINEL,
};
