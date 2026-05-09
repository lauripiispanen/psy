//! Standalone macOS cleanup sidecar for psy embedded hosts.
//!
//! This binary is what `psy-core`'s [`SidecarStrategy::ExternalBinary`]
//! spawns: it watches its parent process via kqueue + a death pipe and
//! SIGKILLs any pids the parent announced before its own death. Hosts
//! that don't want their main binary re-dispatched as the sidecar (the
//! default `HostReDispatch` strategy) ship this binary alongside theirs
//! and point `ExternalBinary { path: ... }` at it.
//!
//! Internally this is just a thin re-export of `psy-core`'s
//! [`dispatch_macos_cleanup_if_invoked`]: that function inspects argv
//! for a sentinel and runs the sidecar logic if present. `spawn_sidecar`
//! always invokes the binary with the configured sentinel as argv[1..],
//! so this binary's main is a single call.
//!
//! On non-macOS targets the binary builds but exits immediately with
//! code 0 — it has nothing useful to do (Linux's subreaper and Windows'
//! Job Object handle hard-kill cleanup in-kernel).

fn main() {
    psy_core::dispatch_macos_cleanup_if_invoked();
    // If we get here on macOS, argv didn't match a sentinel — the binary
    // was invoked directly without the expected sentinel argv. Exit
    // non-zero so misconfiguration is loud rather than silent.
    #[cfg(target_os = "macos")]
    {
        eprintln!(
            "psy-macos-cleanup-sidecar: missing argv sentinel; this binary is meant to be \
             spawned by psy-core, not invoked directly."
        );
        std::process::exit(2);
    }
}
