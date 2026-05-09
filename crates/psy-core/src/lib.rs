//! psy-core — embeddable cross-platform process supervisor.
//!
//! This crate is the supervisor logic that powers the `psy` binary. It can
//! also be embedded directly in a host process to supervise children without
//! a separate `psy` process.
//!
//! Today the module surface is `pub` so the in-tree CLI binary, MCP server,
//! and wire-protocol client can use it. The Phase B work in this release
//! introduces the curated `RootOptions` / `RootHandle` / `Spawn` API on top
//! of these modules; the modules themselves stay pub for advanced use.

#[cfg(target_os = "macos")]
pub mod macos_cleanup;
pub mod platform;
pub mod probe;
pub mod process;
pub mod protocol;
pub mod psyfile;
pub mod ring_buffer;
pub mod root;
