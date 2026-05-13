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

    let _g = ROOT_LOCK.lock().unwrap_or_else(|e| e.into_inner());
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

    let _g = ROOT_LOCK.lock().unwrap_or_else(|e| e.into_inner());
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

/// Build a dedicated multi-threaded runtime, hand its handle to
/// `RootOptions::with_runtime`, and verify supervision still works.
/// This exercises the explicit-runtime path: psy-core's background
/// tasks (sidecar supervisor, monitor_child, etc.) are spawned via
/// `SharedRoot::spawn` which routes through the supplied handle.
#[test]
#[ignore]
fn test_embedded_runtime_injection_explicit_handle() {
    use psy_core::{PsyRoot, RootOptions, Spawn};

    let _g = ROOT_LOCK.lock().unwrap_or_else(|e| e.into_inner());

    // Driver runtime — host's "main" runtime where it awaits PsyRoot::start.
    let driver = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("driver runtime");
    // Dedicated runtime for psy-core's background tasks.
    let psy_rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .thread_name("psy-supervisor")
        .build()
        .expect("psy runtime");
    let psy_handle = psy_rt.handle().clone();

    driver.block_on(async {
        let host =
            PsyRoot::start(RootOptions::new("rt-injection-host").with_runtime(psy_handle.clone()))
                .await
                .expect("start");

        let h = host
            .spawn(Spawn::new("sleeper", ["sleep", "30"]))
            .await
            .expect("spawn");

        // pid_watch must be Some — proves spawn_process and pid_tx
        // updates landed even though monitor/sidecar tasks are on a
        // different runtime than the driver.
        let pid_rx = h.pid_watch().await.expect("pid_watch");
        assert!(pid_rx.borrow().is_some(), "pid should be set after spawn");

        h.stop().await.expect("stop");
        host.shutdown().await.expect("shutdown");
    });

    // Tear down the dedicated runtime explicitly so its background
    // workers exit before the test process ends (cleaner shutdown).
    psy_rt.shutdown_timeout(Duration::from_secs(2));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
#[allow(clippy::await_holding_lock)]
async fn test_embedded_inprocess_subroot_isolation() {
    use psy_core::{PsyRoot, RootOptions, Spawn, SubRootKind, SubRootOptions};

    let _g = ROOT_LOCK.lock().unwrap_or_else(|e| e.into_inner());
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

    let _g = ROOT_LOCK.lock().unwrap_or_else(|e| e.into_inner());
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

    let _g = ROOT_LOCK.lock().unwrap_or_else(|e| e.into_inner());
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

    let _g = ROOT_LOCK.lock().unwrap_or_else(|e| e.into_inner());
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
async fn test_embedded_spawnhandle_events_and_pid_watch() {
    use futures_util::StreamExt;
    use psy_core::{PsyRoot, RootEvent, RootOptions, Spawn};

    let _g = ROOT_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let host = PsyRoot::start(RootOptions::new("evts-host"))
        .await
        .expect("start");

    // Long-running process so we can subscribe + observe events while
    // it's alive, then stop it deterministically.
    let h = host
        .spawn(Spawn::new("transient", ["sleep", "60"]))
        .await
        .expect("spawn");

    // pid_watch should report Some immediately after spawn returns.
    let pid_watch = h.pid_watch().await.expect("pid_watch");
    assert!(
        pid_watch.borrow().is_some(),
        "pid_watch should start with Some"
    );

    // Subscribe to events BEFORE we trigger an exit so we don't miss it.
    let mut events = Box::pin(h.events().await.expect("events"));

    h.stop().await.expect("stop");

    // Drain events until we see SpawnExited (or timeout).
    let saw_exit = tokio::time::timeout(Duration::from_secs(8), async {
        while let Some(ev) = events.next().await {
            if matches!(
                ev,
                RootEvent::SpawnExited { ref name, .. } if name == "transient"
            ) {
                return true;
            }
        }
        false
    })
    .await
    .expect("timeout");
    assert!(saw_exit, "should have observed SpawnExited");

    host.shutdown().await.expect("shutdown");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
#[allow(clippy::await_holding_lock)]
async fn test_embedded_log_sink_and_on_event() {
    use psy_core::{LogSink, PsyRoot, RootEvent, RootOptions, Spawn};
    use std::sync::{Arc, Mutex};

    let _g = ROOT_LOCK.lock().unwrap_or_else(|e| e.into_inner());

    // Capture every line the sink sees.
    #[derive(Default)]
    struct VecSink(Mutex<Vec<String>>);
    impl LogSink for VecSink {
        fn on_line(
            &self,
            process: &str,
            _run_id: u32,
            _stream: psy_core::StreamKind,
            _ts: chrono::DateTime<chrono::Utc>,
            line: &str,
        ) {
            self.0.lock().unwrap().push(format!("{process}: {line}"));
        }
    }
    let sink = Arc::new(VecSink::default());

    // Capture every event.
    let events: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let events_for_cb = Arc::clone(&events);

    let root = PsyRoot::start(
        RootOptions::new("hooks-host")
            .with_log_sink(sink.clone() as Arc<dyn LogSink>)
            .with_on_event(move |ev| {
                let label = match &ev {
                    RootEvent::SpawnStarted { name, .. } => format!("started:{name}"),
                    RootEvent::SpawnExited { name, .. } => format!("exited:{name}"),
                    RootEvent::Shutdown => "shutdown".into(),
                    other => format!("{other:?}"),
                };
                events_for_cb.lock().unwrap().push(label);
            }),
    )
    .await
    .expect("start");

    let _h = root
        .spawn(Spawn::new(
            "talker",
            ["sh", "-c", "echo hello-from-sink && exit 0"],
        ))
        .await
        .expect("spawn");

    // Give the child a moment to run + exit.
    tokio::time::sleep(Duration::from_millis(800)).await;

    let captured: Vec<String> = sink.0.lock().unwrap().clone();
    assert!(
        captured.iter().any(|l| l.contains("hello-from-sink")),
        "log_sink should have seen the line; got: {captured:?}"
    );

    let evs: Vec<String> = events.lock().unwrap().clone();
    assert!(
        evs.iter().any(|s| s.starts_with("started:talker")),
        "on_event should fire SpawnStarted; got: {evs:?}"
    );
    assert!(
        evs.iter().any(|s| s.starts_with("exited:talker")),
        "on_event should fire SpawnExited; got: {evs:?}"
    );

    root.shutdown().await.expect("shutdown");

    let evs_after: Vec<String> = events.lock().unwrap().clone();
    assert!(
        evs_after.iter().any(|s| s == "shutdown"),
        "on_event should fire Shutdown; got: {evs_after:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
#[allow(clippy::await_holding_lock)]
async fn test_embedded_typed_error_already_exists() {
    use psy_core::{PsyError, PsyRoot, RootOptions, Spawn};

    let _g = ROOT_LOCK.lock().unwrap_or_else(|e| e.into_inner());
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

    let _g = ROOT_LOCK.lock().unwrap_or_else(|e| e.into_inner());
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

/// `RootHandle::shutdown` returns an aggregate exit code derived from
/// any `Failed` processes' last exit_status. Verify clean shutdown
/// returns 0 and a failed child surfaces its non-zero code.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
#[allow(clippy::await_holding_lock)]
async fn test_embedded_shutdown_exit_code_propagation() {
    use psy_core::{PsyRoot, RootOptions, Spawn};
    let _g = ROOT_LOCK.lock().unwrap_or_else(|e| e.into_inner());

    // Case 1: clean shutdown — all processes still running, no failures.
    let host = PsyRoot::start(RootOptions::new("shutdown-clean"))
        .await
        .expect("start");
    let _h = host
        .spawn(Spawn::new("idle", ["sleep", "30"]))
        .await
        .expect("spawn");
    let code = host.shutdown().await.expect("shutdown");
    assert_eq!(code, 0, "clean shutdown should return 0");

    // Case 2: a child failed before shutdown; aggregate should surface it.
    let host = PsyRoot::start(RootOptions::new("shutdown-fail"))
        .await
        .expect("start");
    let _h = host
        .spawn(Spawn::new("boom", ["sh", "-c", "exit 42"]))
        .await
        .expect("spawn");

    // Wait until the child has finished and its exit_status is recorded.
    let mut got_failed = false;
    for _ in 0..40 {
        let listing = host.list().await.expect("list");
        if listing
            .iter()
            .any(|p| p.name == "boom" && p.status == "failed")
        {
            got_failed = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(got_failed, "boom should reach Failed state before shutdown");

    let code = host.shutdown().await.expect("shutdown");
    assert_eq!(code, 42, "shutdown should surface failed child's exit code");
}

/// `spawn_psy_subroot` is the v2.1 escape hatch for hosts that need
/// process-level isolation before typed `SubRootKind::OutOfProcess`
/// lands in v2.2. Validates that the helper builds the right argv and
/// returns a working SpawnHandle.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
#[allow(clippy::await_holding_lock)]
async fn test_embedded_spawn_psy_subroot_helper() {
    use psy_core::{PsyRoot, RootOptions};
    let _g = ROOT_LOCK.lock().unwrap_or_else(|e| e.into_inner());

    // Resolve the actual psy binary built by this workspace so the
    // child invocation finds something real even if `$PATH` doesn't
    // include the cargo target directory.
    let psy_bin = {
        let mut p = std::env::current_exe().expect("test exe path");
        p.pop(); // deps/
        p.pop(); // <profile>/
        p.push("psy");
        assert!(
            p.exists(),
            "psy binary not built; expected at {}",
            p.display()
        );
        p
    };

    let host = PsyRoot::start(RootOptions::new("oop-helper-host"))
        .await
        .expect("start");

    // Spawn a psy sub-root that just sleeps as its main process.
    let h = host
        .spawn_psy_subroot("untrusted", Some(&psy_bin), ["--", "sleep", "30"])
        .await
        .expect("spawn_psy_subroot");

    // Give the child a moment to actually exec.
    tokio::time::sleep(Duration::from_millis(300)).await;

    // The helper returns a normal SpawnHandle backed by the parent
    // root's process table. Verify it's tracked there.
    let listing = host.list().await.expect("list");
    assert!(
        listing.iter().any(|p| p.name == "untrusted"),
        "spawned subroot should appear under parent's process table"
    );

    // pid_watch should report the spawned psy child's pid.
    let pid_rx = h.pid_watch().await.expect("pid_watch");
    assert!(
        pid_rx.borrow().is_some(),
        "spawn_psy_subroot child should have a pid"
    );

    h.stop().await.expect("stop");
    host.shutdown().await.expect("shutdown");
}

/// `SpawnHandle::write_stdin` writes raw bytes to a process's stdin
/// when the spawn declared `interactive = true`. Spawn `cat`, write a
/// non-newline-terminated payload followed by a marker newline, and
/// observe the echo in the line-tokenized stdout stream.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
#[allow(clippy::await_holding_lock)]
async fn test_embedded_write_stdin_roundtrip() {
    use futures_util::StreamExt;
    use psy_core::{PsyRoot, RootOptions, Spawn};

    let _g = ROOT_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let host = PsyRoot::start(RootOptions::new("stdin-host"))
        .await
        .expect("start");

    let h = host
        .spawn(Spawn::new("echo", ["cat"]).with_interactive(true))
        .await
        .expect("spawn");

    let mut stdout = Box::pin(h.stdout().await.expect("stdout"));

    // Send a non-UTF8 payload then a newline so cat flushes it.
    let payload: &[u8] = b"hello-via-write_stdin\n";
    let n = h.write_stdin(payload).await.expect("write_stdin");
    assert_eq!(n, payload.len());

    let line = tokio::time::timeout(Duration::from_secs(5), stdout.next())
        .await
        .expect("stdout timeout")
        .expect("stream closed early");
    assert!(
        line.content.contains("hello-via-write_stdin"),
        "expected echo; got: {}",
        line.content
    );

    h.stop().await.expect("stop");
    host.shutdown().await.expect("shutdown");
}

/// `write_stdin` on a non-interactive process must surface an error
/// rather than panicking or silently dropping bytes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
#[allow(clippy::await_holding_lock)]
async fn test_embedded_write_stdin_rejects_non_interactive() {
    use psy_core::{PsyRoot, RootOptions, Spawn};

    let _g = ROOT_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let host = PsyRoot::start(RootOptions::new("stdin-noninter"))
        .await
        .expect("start");

    let h = host
        .spawn(Spawn::new("idle", ["sleep", "30"]))
        .await
        .expect("spawn");

    let result = h.write_stdin(b"data").await;
    match result {
        Ok(_) => panic!("write_stdin to non-interactive spawn should fail"),
        Err(e) => {
            let s = e.to_string();
            assert!(
                s.contains("interactive"),
                "error should mention interactive mode; got: {s}"
            );
        }
    }

    h.stop().await.expect("stop");
    host.shutdown().await.expect("shutdown");
}

/// `SpawnHandle::close_stdin` sends EOF to the supervised child;
/// `cat` then exits cleanly. Verify wait() returns and subsequent
/// write_stdin calls error out.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
#[allow(clippy::await_holding_lock)]
async fn test_embedded_close_stdin_triggers_eof() {
    use psy_core::{PsyRoot, RootOptions, Spawn};

    let _g = ROOT_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let host = PsyRoot::start(RootOptions::new("eof-host"))
        .await
        .expect("start");

    let h = host
        .spawn(Spawn::new("echo", ["cat"]).with_interactive(true))
        .await
        .expect("spawn");

    h.write_stdin(b"line-before-eof\n")
        .await
        .expect("write_stdin");
    h.close_stdin().await.expect("close_stdin");

    // cat should exit when its stdin reaches EOF.
    let status = tokio::time::timeout(Duration::from_secs(5), h.wait())
        .await
        .expect("wait timeout")
        .expect("wait err");
    assert_eq!(
        status.exit_code,
        Some(0),
        "cat should exit 0 after EOF: {status:?}"
    );

    // Further write_stdin must error.
    assert!(
        h.write_stdin(b"never-arrives").await.is_err(),
        "write_stdin after close_stdin must fail"
    );

    host.shutdown().await.expect("shutdown");
}

/// Without `with_raw_stdio(true)`, `stdout_bytes()` returns an error
/// instead of silently subscribing to a never-fed channel.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
#[allow(clippy::await_holding_lock)]
async fn test_embedded_stdout_bytes_requires_opt_in() {
    use psy_core::{PsyRoot, RootOptions, Spawn};

    let _g = ROOT_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let host = PsyRoot::start(RootOptions::new("raw-optin"))
        .await
        .expect("start");

    let h = host
        .spawn(Spawn::new("idle", ["sleep", "30"]))
        .await
        .expect("spawn");

    let result = h.stdout_bytes().await;
    match result {
        Ok(_) => panic!("stdout_bytes without with_raw_stdio should fail"),
        Err(e) => {
            let s = e.to_string();
            assert!(
                s.contains("raw stdio"),
                "error should mention raw stdio; got: {s}"
            );
        }
    }

    h.stop().await.expect("stop");
    host.shutdown().await.expect("shutdown");
}

/// `with_raw_stdio(true)` surfaces partial (non-newline-terminated)
/// output verbatim on `stdout_bytes()` — proving the raw stream
/// preserves byte boundaries and isn't line-buffered like `stdout()`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
#[allow(clippy::await_holding_lock)]
async fn test_embedded_stdout_bytes_delivers_partial_chunks() {
    use futures_util::StreamExt;
    use psy_core::{PsyRoot, RootOptions, Spawn};

    let _g = ROOT_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let host = PsyRoot::start(RootOptions::new("raw-bytes-host"))
        .await
        .expect("start");

    // The child prints "partial-frame-no-newline" with no trailing \n
    // and then sleeps. A line-tokenized stream would never yield this
    // (no newline), but the raw byte stream must surface it.
    let h = host
        .spawn(
            Spawn::new(
                "framed",
                ["sh", "-c", "printf partial-frame-no-newline; sleep 30"],
            )
            .with_raw_stdio(true),
        )
        .await
        .expect("spawn");

    let mut bytes = Box::pin(h.stdout_bytes().await.expect("stdout_bytes"));

    // Collect raw chunks until we've seen the expected payload, with a
    // bounded timeout. (printf may flush in one chunk on a pipe, but
    // we don't assume that.)
    let mut accum: Vec<u8> = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while tokio::time::Instant::now() < deadline {
        let remaining = deadline - tokio::time::Instant::now();
        match tokio::time::timeout(remaining, bytes.next()).await {
            Ok(Some(chunk)) => {
                accum.extend_from_slice(&chunk);
                if accum
                    .windows(b"partial-frame-no-newline".len())
                    .any(|w| w == b"partial-frame-no-newline")
                {
                    break;
                }
            }
            Ok(None) => break,
            Err(_) => break,
        }
    }
    let as_str = String::from_utf8_lossy(&accum);
    assert!(
        as_str.contains("partial-frame-no-newline"),
        "raw stdout should surface the partial frame; got: {as_str:?}"
    );

    h.stop().await.expect("stop");
    host.shutdown().await.expect("shutdown");
}

/// With `with_raw_stdio(true)`, the line-tokenized `stdout()` ring
/// buffer must still receive newline-terminated content. Verifies the
/// chunked-read pipeline still feeds the line splitter so `psy logs`
/// keeps working for raw-stdio processes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
#[allow(clippy::await_holding_lock)]
async fn test_embedded_raw_stdio_still_feeds_line_buffer() {
    use futures_util::StreamExt;
    use psy_core::{PsyRoot, RootOptions, Spawn};

    let _g = ROOT_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let host = PsyRoot::start(RootOptions::new("raw-and-lines-host"))
        .await
        .expect("start");

    let h = host
        .spawn(
            Spawn::new("dual", ["sh", "-c", "echo line-from-dual && sleep 30"])
                .with_raw_stdio(true),
        )
        .await
        .expect("spawn");

    // The line-tokenized stream still works for raw-stdio processes.
    let mut lines = Box::pin(h.stdout().await.expect("stdout"));
    let line = tokio::time::timeout(Duration::from_secs(5), lines.next())
        .await
        .expect("stdout timeout")
        .expect("stream closed early");
    assert!(
        line.content.contains("line-from-dual"),
        "line buffer should still receive content; got: {}",
        line.content
    );

    h.stop().await.expect("stop");
    host.shutdown().await.expect("shutdown");
}

/// Find the standalone psy-macos-cleanup-sidecar binary. Mirrors
/// `embedded_fixture_bin` but points at the workspace bin target.
fn standalone_sidecar_bin() -> PathBuf {
    let mut p = std::env::current_exe().expect("test exe path");
    p.pop(); // deps/
    p.pop(); // <profile>/
    p.push("psy-macos-cleanup-sidecar");
    p
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

/// Same as `test_embedded_host_sigkill_cleans_up_grandchild`, but the
/// fixture is configured to use `SidecarStrategy::ExternalBinary`
/// pointing at the standalone `psy-macos-cleanup-sidecar` shim. Proves
/// that hosts which don't want their main binary re-dispatchable as
/// the sidecar can ship the shim alongside theirs and still get the
/// hard-kill cleanup guarantee.
#[test]
#[ignore]
fn test_embedded_external_sidecar_cleans_up_grandchild() {
    let fixture = embedded_fixture_bin();
    let sidecar = standalone_sidecar_bin();
    assert!(
        fixture.exists(),
        "embedded_fixture not built; expected at {}",
        fixture.display()
    );
    assert!(
        sidecar.exists(),
        "psy-macos-cleanup-sidecar not built; expected at {}",
        sidecar.display()
    );

    let mut child = Command::new(&fixture)
        .args(["sleep", "600"])
        .env("PSY_SIDECAR_BIN", &sidecar)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn embedded_fixture");

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
    assert!(
        pid_alive(main_pid),
        "main child should be alive before crash"
    );

    unsafe {
        libc::kill(child.id() as i32, libc::SIGKILL);
    }
    let _ = child.wait();

    let dead = wait_until(Duration::from_secs(15), || !pid_alive(main_pid));
    assert!(
        dead,
        "supervised child {main_pid} should be dead after embedded host SIGKILL \
         (external sidecar binary path)"
    );
}
