//! Embedded-mode integration tests.
//!
//! These exercise psy-core via the public library API rather than through
//! the `psy` CLI binary. They prove that hosts which embed psy-core (e.g.
//! the Kickstart Tauri backend) get the same supervision and cleanup
//! guarantees the binary does.
//!
//! Run with `cargo test --test embedded -- --ignored`.

#![cfg(target_os = "macos")]

use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Find the embedded_fixture binary. Cargo builds examples to
/// `target/<profile>/examples/<name>` — we resolve via the test executable's
/// own location to avoid depending on the working directory.
fn embedded_fixture_bin() -> PathBuf {
    // Test exe path: target/<profile>/deps/embedded-<hash>
    // We want:        target/<profile>/examples/embedded_fixture
    let mut p = std::env::current_exe().expect("test exe path");
    p.pop(); // deps/
    p.pop(); // <profile>/
    p.push("examples");
    p.push("embedded_fixture");
    p
}

/// Wait for `cond` to return `true`, polling every 100ms up to `timeout`.
fn wait_until<F: FnMut() -> bool>(timeout: Duration, mut cond: F) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if cond() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    cond()
}

fn pid_alive(pid: u32) -> bool {
    unsafe { libc::kill(pid as i32, 0) == 0 }
}

#[test]
#[ignore]
fn test_embedded_host_sigkill_cleans_up_grandchild() {
    let bin = embedded_fixture_bin();
    assert!(
        bin.exists(),
        "embedded_fixture not built; expected at {}",
        bin.display()
    );

    // Spawn the fixture with a long-running main process.
    let mut child = Command::new(&bin)
        .args(["sleep", "600"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn embedded_fixture");

    // Read the MAIN_PID line from stdout.
    let stdout = child.stdout.take().expect("stdout piped");
    let mut reader = BufReader::new(stdout);
    let mut main_pid: Option<u32> = None;
    let deadline = Instant::now() + Duration::from_secs(8);
    while Instant::now() < deadline {
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) => break,
            Ok(_) => {
                if let Some(rest) = line.trim().strip_prefix("MAIN_PID=") {
                    if let Ok(pid) = rest.parse::<u32>() {
                        main_pid = Some(pid);
                        break;
                    }
                }
            }
            Err(_) => break,
        }
    }
    let main_pid = main_pid.expect("fixture should announce MAIN_PID");

    // Sanity: the supervised child is alive before we crash the host.
    assert!(
        pid_alive(main_pid),
        "main child should be alive before crash"
    );

    // SIGKILL the host (worst case — host can't run any code).
    unsafe {
        libc::kill(child.id() as i32, libc::SIGKILL);
    }
    let _ = child.wait();

    // Within a few seconds the cleanup sidecar must have reaped the child.
    let dead = wait_until(Duration::from_secs(15), || !pid_alive(main_pid));
    assert!(
        dead,
        "supervised child {main_pid} should be dead after embedded host SIGKILL"
    );
}
