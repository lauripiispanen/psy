//! Embedded-mode integration tests.
//!
//! These exercise psy-core via the public library API rather than through
//! the `psy` CLI binary. They prove that hosts which embed psy-core (e.g.
//! the Kickstart Tauri backend) get the same supervision and cleanup
//! guarantees the binary does.
//!
//! Run with `cargo test --test embedded -- --ignored`.

#![cfg(target_os = "macos")]
//
// All tests use `cargo test ... -- --test-threads=1` semantics (each test
// constructs a `PsyRoot` and binds a per-PID socket; running concurrent
// tests in the same test binary would collide on the socket path).

use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Coarse global lock to serialize tests that build a real `PsyRoot`.
/// Without it, two tests in the same process race on the per-PID socket
/// path and the second `bind` returns `Address already in use`.
static ROOT_LOCK: Mutex<()> = Mutex::new(());

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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
#[allow(clippy::await_holding_lock)] // intentional: serializes test runs
async fn test_embedded_smoke_spawn_list_stop_shutdown() {
    use psy_core::{PsyRoot, RestartPolicy, RootOptions, Spawn};

    let _g = ROOT_LOCK.lock().unwrap();
    let root = PsyRoot::start(RootOptions::new("smoke-host"))
        .await
        .expect("PsyRoot::start");

    // Spawn a long-running child.
    let h = root
        .spawn(Spawn::new("worker", ["sleep", "60"]).with_restart(RestartPolicy::No))
        .await
        .expect("spawn");
    assert_eq!(h.name, "worker");
    assert!(h.pid.is_some());

    // list/status both visible.
    let listing = root.list().await.expect("list");
    assert!(listing.iter().any(|p| p.name == "worker"));
    let status = root.status("worker").await.expect("status");
    assert_eq!(status.status, "running");

    // stop ↦ entry stays in the table as a tombstone with status != running.
    root.stop("worker").await.expect("stop");
    tokio::time::sleep(Duration::from_millis(500)).await;
    let after = root.status("worker").await.expect("status post-stop");
    assert_ne!(after.status, "running", "worker should be stopped");

    // clean removes the tombstone.
    let removed = root.clean().await.expect("clean");
    assert!(removed >= 1, "clean should remove >=1 entry");

    // shutdown without errors.
    root.shutdown().await.expect("shutdown");
}

#[tokio::test]
#[ignore]
#[allow(clippy::await_holding_lock)] // intentional: serializes test runs
async fn test_embedded_runtime_injection_current_thread() {
    // Smoke: same as above but on a current_thread runtime, validating that
    // the library doesn't depend on the multi-threaded runtime.
    use psy_core::{PsyRoot, RootOptions, Spawn};

    let _g = ROOT_LOCK.lock().unwrap();
    let root = PsyRoot::start(RootOptions::new("ct-host"))
        .await
        .expect("PsyRoot::start");
    let _h = root
        .spawn(Spawn::new("idle", ["sleep", "30"]))
        .await
        .expect("spawn");
    let listing = root.list().await.expect("list");
    assert!(listing.iter().any(|p| p.name == "idle"));
    root.shutdown().await.expect("shutdown");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
#[allow(clippy::await_holding_lock)]
async fn test_embedded_inprocess_subroot_isolation() {
    use psy_core::{PsyRoot, RootOptions, Spawn, SubRootKind, SubRootOptions};

    let _g = ROOT_LOCK.lock().unwrap();
    let host = PsyRoot::start(RootOptions::new("isolation-host"))
        .await
        .expect("host start");

    // Spawn a child directly under the host.
    host.spawn(Spawn::new("host-child", ["sleep", "30"]))
        .await
        .expect("host spawn");

    // Spawn an in-process sub-root and a child inside it.
    let sub = host
        .sub_root(SubRootOptions::new("instance-a").with_kind(SubRootKind::InProcess))
        .await
        .expect("sub_root");
    sub.spawn(Spawn::new("sub-child", ["sleep", "30"]))
        .await
        .expect("sub spawn");

    // Host's list sees the host child but NOT the sub-root child.
    let host_listing = host.list().await.expect("host list");
    let host_names: Vec<_> = host_listing.iter().map(|p| p.name.as_str()).collect();
    assert!(host_names.contains(&"host-child"), "got: {host_names:?}");
    assert!(
        !host_names.contains(&"sub-child"),
        "host should not see sub-root child; got: {host_names:?}"
    );

    // Sub-root's list sees ITS child but NOT the host child.
    let sub_listing = sub.list().await.expect("sub list");
    let sub_names: Vec<_> = sub_listing.iter().map(|p| p.name.as_str()).collect();
    assert!(sub_names.contains(&"sub-child"), "got: {sub_names:?}");
    assert!(
        !sub_names.contains(&"host-child"),
        "sub-root should not see host child; got: {sub_names:?}"
    );

    sub.shutdown().await.expect("sub shutdown");
    host.shutdown().await.expect("host shutdown");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
#[allow(clippy::await_holding_lock)]
async fn test_embedded_programmatic_graph_with_dependency() {
    // Build a programmatic graph: a "ready" listener (TCP probe on
    // localhost:0 won't work — use exec=true which exits 0 immediately)
    // and a dependent that waits for the listener's ready probe.
    use psy_core::{DependencyRef, PsyRoot, ReadyProbe, RootOptions, Spawn};

    let _g = ROOT_LOCK.lock().unwrap();
    let host = PsyRoot::start(RootOptions::new("graph-host"))
        .await
        .expect("host start");

    // Spawn a "service" with an exec-based readiness probe (true exits 0,
    // so the probe passes immediately).
    host.spawn(
        Spawn::new("service", ["sleep", "30"]).with_ready(ReadyProbe::Exec {
            command: "true".into(),
            interval: None,
            timeout: Some(Duration::from_secs(5)),
            retries: None,
        }),
    )
    .await
    .expect("service spawn");

    // Spawn a "client" that depends on service.
    host.spawn(
        Spawn::new("client", ["sleep", "30"]).with_depends_on(vec![DependencyRef::new("service")]),
    )
    .await
    .expect("client spawn");

    // Both should be running.
    let listing = host.list().await.expect("list");
    let names: Vec<_> = listing.iter().map(|p| p.name.as_str()).collect();
    assert!(names.contains(&"service"));
    assert!(names.contains(&"client"));

    host.shutdown().await.expect("shutdown");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
#[allow(clippy::await_holding_lock)]
async fn test_embedded_dependency_not_running_errors() {
    use psy_core::{DependencyRef, PsyError, PsyRoot, RootOptions, Spawn};

    let _g = ROOT_LOCK.lock().unwrap();
    let host = PsyRoot::start(RootOptions::new("dep-err-host"))
        .await
        .expect("host start");

    let result = host
        .spawn(
            Spawn::new("dependent", ["sleep", "30"])
                .with_depends_on(vec![DependencyRef::new("never-spawned")]),
        )
        .await;
    match result {
        Ok(_) => panic!("spawn should fail when dep doesn't exist"),
        Err(PsyError::NotFound { name }) => assert_eq!(name, "never-spawned"),
        Err(other) => panic!("expected NotFound, got: {other}"),
    }

    host.shutdown().await.expect("shutdown");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
#[allow(clippy::await_holding_lock)]
async fn test_embedded_spawnhandle_streaming_and_wait() {
    // Spawn a process, subscribe to its stdout, then stop it via
    // SpawnHandle::stop(). Verify wait() returns the exit status.
    use futures_util::StreamExt;
    use psy_core::{PsyRoot, RootOptions, Spawn};

    let _g = ROOT_LOCK.lock().unwrap();
    let host = PsyRoot::start(RootOptions::new("stream-host"))
        .await
        .expect("host start");

    // Spawn a process that prints a line then sleeps. The SpawnHandle's
    // stdout stream should yield the line.
    let h = host
        .spawn(Spawn::new(
            "talker",
            ["sh", "-c", "echo hello-from-talker && sleep 30"],
        ))
        .await
        .expect("spawn");

    let mut stdout = Box::pin(h.stdout().await.expect("stdout"));
    // Read one line with a timeout (probe should appear within ~1s).
    let line = tokio::time::timeout(Duration::from_secs(5), stdout.next())
        .await
        .expect("stream timeout")
        .expect("stream closed early");
    assert!(
        line.content.contains("hello-from-talker"),
        "expected greeting; got: {}",
        line.content
    );

    // Stop via SpawnHandle.
    h.stop().await.expect("stop");

    // wait() should now return promptly with exit info.
    let status = tokio::time::timeout(Duration::from_secs(5), h.wait())
        .await
        .expect("wait timeout")
        .expect("wait err");
    // SIGTERM usually leaves exit_code None and signal Some("SIG15") on
    // Unix; assert at least one of them is set.
    assert!(
        status.exit_code.is_some() || status.signal.is_some(),
        "should have exit_code or signal: {status:?}"
    );

    host.shutdown().await.expect("shutdown");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
#[allow(clippy::await_holding_lock)]
async fn test_embedded_typed_error_already_exists() {
    use psy_core::{PsyError, PsyRoot, RootOptions, Spawn};

    let _g = ROOT_LOCK.lock().unwrap();
    let host = PsyRoot::start(RootOptions::new("dup-host"))
        .await
        .expect("host start");

    host.spawn(Spawn::new("dup", ["sleep", "30"]))
        .await
        .expect("first spawn");

    let result = host.spawn(Spawn::new("dup", ["sleep", "30"])).await;
    match result {
        Ok(_) => panic!("duplicate spawn should fail"),
        Err(PsyError::AlreadyExists { name }) => assert_eq!(name, "dup"),
        Err(other) => panic!("expected AlreadyExists, got: {other}"),
    }

    host.shutdown().await.expect("shutdown");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
#[allow(clippy::await_holding_lock)]
async fn test_embedded_inprocess_subroot_outofprocess_not_implemented() {
    use psy_core::{PsyRoot, RootOptions, SubRootKind, SubRootOptions};

    let _g = ROOT_LOCK.lock().unwrap();
    let host = PsyRoot::start(RootOptions::new("oop-host"))
        .await
        .expect("host start");

    let result = host
        .sub_root(
            SubRootOptions::new("instance-a").with_kind(SubRootKind::OutOfProcess { binary: None }),
        )
        .await;
    match result {
        Ok(_) => panic!("OutOfProcess should be rejected for now"),
        Err(e) => {
            let s = e.to_string();
            assert!(
                s.contains("OutOfProcess"),
                "error should mention OutOfProcess; got: {s}"
            );
        }
    }

    host.shutdown().await.expect("host shutdown");
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
