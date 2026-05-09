//! Minimal embedded-mode fixture used by the integration test that proves
//! the macOS cleanup sidecar reaches grandchildren when a *host* binary
//! (not the `psy` CLI) is the parent.
//!
//! Usage:
//!     embedded_fixture <main-cmd> [args...]
//!
//! The fixture:
//!   1. Calls `dispatch_macos_cleanup_if_invoked()` at the very top of
//!      `main()` — this is the contract every embedding host must follow.
//!      When the binary is re-spawned as a sidecar, this call exits the
//!      process; otherwise it returns.
//!   2. Constructs a `PsyRoot` with the default `SidecarStrategy`
//!      (`HostReDispatch` with the default sentinel) and runs `<main-cmd>`
//!      as the root's main process.
//!   3. Prints a single line to stdout: `MAIN_PID=<n>` so the test driver
//!      can locate the child process and verify it dies on host abort.
//!
//! The test driver uses `std::process::abort()` (via SIGKILL on the host)
//! to simulate the worst-case crash, then verifies `MAIN_PID` is dead
//! within a small bound — the cleanup sidecar should reap it.

fn main() {
    // Sidecar dispatch must run before any other initialization. When the
    // sidecar re-spawns this binary with the sentinel argv, this call
    // intercepts and exits the process. Otherwise it returns immediately.
    psy_core::dispatch_macos_cleanup_if_invoked();

    let argv: Vec<String> = std::env::args().skip(1).collect();
    if argv.is_empty() {
        eprintln!("usage: embedded_fixture <main-cmd> [args...]");
        std::process::exit(2);
    }

    let runtime = tokio::runtime::Runtime::new().expect("create tokio runtime");
    let exit_code = runtime.block_on(async move {
        let psy_root = psy_core::root::PsyRoot::new_with_strategy(
            "embedded-fixture".to_string(),
            None,
            psy_core::SidecarStrategy::default(),
        )
        .expect("PsyRoot::new_with_strategy");

        // Spawn a watcher task that, once the main process is in the table,
        // prints its PID so the test driver can find it.
        let shared = psy_root.shared_for_test();
        tokio::spawn(async move {
            // Poll for up to 5s for the main entry to appear with a pid.
            for _ in 0..50 {
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                let table = shared.process_table.lock().await;
                if let Some(entry) = table.get("main") {
                    if let Some(pid) = entry.pid {
                        println!("MAIN_PID={pid}");
                        use std::io::Write;
                        let _ = std::io::stdout().flush();
                        return;
                    }
                }
            }
            eprintln!("embedded_fixture: failed to capture main pid");
        });

        psy_root
            .run(Some(argv), vec![], psy_core::root::MainMode::Default, None)
            .await
            .unwrap_or(1)
    });

    std::process::exit(exit_code);
}
