//! Embedded-mode fixture used by `tests/embedded.rs`.
//!
//! Demonstrates the host pattern: dispatch sidecar argv, build a
//! `RootHandle`, spawn a child, print its PID for the test driver to
//! locate, then idle until SIGKILL'd.

fn main() {
    // 1. Sidecar dispatch must run before any other initialization.
    psy_core::dispatch_macos_cleanup_if_invoked();

    let argv: Vec<String> = std::env::args().skip(1).collect();
    if argv.is_empty() {
        eprintln!("usage: embedded_fixture <main-cmd> [args...]");
        std::process::exit(2);
    }

    let runtime = tokio::runtime::Runtime::new().expect("create tokio runtime");
    let exit_code = runtime.block_on(async move {
        // 2. Construct the root via the public library API.
        let root =
            match psy_core::PsyRoot::start(psy_core::RootOptions::new("embedded-fixture")).await {
                Ok(r) => r,
                Err(e) => {
                    eprintln!("embedded_fixture: PsyRoot::start failed: {e}");
                    return 1;
                }
            };

        // 3. Spawn the supervised process.
        let spawn = psy_core::Spawn::new("main", argv);
        let handle = match root.spawn(spawn).await {
            Ok(h) => h,
            Err(e) => {
                eprintln!("embedded_fixture: spawn failed: {e}");
                return 1;
            }
        };

        // 4. Announce the supervised PID so the test driver can verify it
        //    dies on host SIGKILL.
        if let Some(pid) = handle.pid {
            println!("MAIN_PID={pid}");
            use std::io::Write;
            let _ = std::io::stdout().flush();
        } else {
            eprintln!("embedded_fixture: no PID in spawn response");
            return 1;
        }

        // 5. Idle. Tests SIGKILL us, which fires kqueue NOTE_EXIT in the
        //    sidecar and reaps the supervised child.
        std::future::pending::<()>().await;
        0
    });

    std::process::exit(exit_code);
}
