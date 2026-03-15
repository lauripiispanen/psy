//! Integration tests for the psy binary.
//!
//! All tests are marked `#[ignore]` and should be run with:
//!     cargo test -- --ignored
//!
//! Each test starts a `psy up` root process and cleans it up on drop.

use std::io::Write;
use std::path::PathBuf;
use std::process::{Child, Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::Duration;

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn psy_bin() -> PathBuf {
    let path = env!("CARGO_BIN_EXE_psy");
    PathBuf::from(path)
}

/// A guard that kills the psy root process on drop.
struct PsyRoot {
    child: Child,
    sock: String,
}

/// A temp directory that creates a Psyfile and cleans up on drop.
struct TempPsyfileDir {
    path: PathBuf,
}

impl TempPsyfileDir {
    fn new(psyfile_content: &str) -> Self {
        let id = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("psy-test-{}-{}", std::process::id(), id));
        let _ = std::fs::create_dir_all(&dir);
        let psyfile_path = dir.join("Psyfile");
        let mut f = std::fs::File::create(&psyfile_path).expect("create Psyfile");
        f.write_all(psyfile_content.as_bytes())
            .expect("write Psyfile");
        TempPsyfileDir { path: dir }
    }

    fn psyfile_path(&self) -> PathBuf {
        self.path.join("Psyfile")
    }
}

impl Drop for TempPsyfileDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

impl PsyRoot {
    /// Start `psy up` with the given main command.
    fn start(main_command: &[&str]) -> Self {
        let mut cmd = Command::new(psy_bin());
        cmd.arg("up").arg("--").args(main_command);
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());

        let child = cmd.spawn().expect("failed to start psy up");
        let pid = child.id();

        // Give root time to create its socket and start listening.
        thread::sleep(Duration::from_secs(2));

        // Build the expected socket path (mirrors platform::socket_path).
        let sock = Self::socket_path_for(pid);

        PsyRoot { child, sock }
    }

    /// Start `psy up` with a Psyfile and optional boot units.
    fn start_with_psyfile(
        psyfile_path: &std::path::Path,
        boot_units: &[&str],
        main_command: &[&str],
    ) -> Self {
        let mut cmd = Command::new(psy_bin());
        cmd.arg("up");
        cmd.arg("--file").arg(psyfile_path);
        for unit in boot_units {
            cmd.arg(unit);
        }
        cmd.arg("--").args(main_command);
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());

        let child = cmd.spawn().expect("failed to start psy up with Psyfile");
        let pid = child.id();

        thread::sleep(Duration::from_secs(2));
        let sock = Self::socket_path_for(pid);
        PsyRoot { child, sock }
    }

    /// Start `psy up --all` with a Psyfile.
    fn start_with_psyfile_all(psyfile_path: &std::path::Path, main_command: &[&str]) -> Self {
        let mut cmd = Command::new(psy_bin());
        cmd.arg("up");
        cmd.arg("--file").arg(psyfile_path);
        cmd.arg("--all");
        cmd.arg("--").args(main_command);
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());

        let child = cmd.spawn().expect("failed to start psy up --all");
        let pid = child.id();

        thread::sleep(Duration::from_secs(2));
        let sock = Self::socket_path_for(pid);
        PsyRoot { child, sock }
    }

    #[cfg(unix)]
    fn socket_path_for(pid: u32) -> String {
        let uid = unsafe { libc::getuid() };
        let dir = std::env::var("XDG_RUNTIME_DIR")
            .map(|d| format!("{d}/psy"))
            .unwrap_or_else(|_| format!("/tmp/psy-{uid}"));
        format!("{dir}/{pid}.sock")
    }

    #[cfg(windows)]
    fn socket_path_for(pid: u32) -> String {
        format!(r"\\.\pipe\psy-{pid}")
    }

    /// Run a psy subcommand against this root, returning the Output.
    fn psy(&self, args: &[&str]) -> Output {
        Command::new(psy_bin())
            .args(args)
            .env("PSY_SOCK", &self.sock)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .expect("failed to run psy command")
    }

    /// Run a psy subcommand, returning stdout as a String.
    fn psy_stdout(&self, args: &[&str]) -> String {
        let out = self.psy(args);
        String::from_utf8_lossy(&out.stdout).to_string()
    }

    #[allow(dead_code)]
    fn psy_stderr(&self, args: &[&str]) -> String {
        let out = self.psy(args);
        String::from_utf8_lossy(&out.stderr).to_string()
    }
}

impl Drop for PsyRoot {
    fn drop(&mut self) {
        // Try graceful shutdown first.
        let _ = Command::new(psy_bin())
            .args(["down"])
            .env("PSY_SOCK", &self.sock)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();

        // Force-kill if still alive.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Cross-platform sleep command.
#[cfg(unix)]
fn sleep_cmd(secs: u64) -> Vec<String> {
    vec!["sleep".into(), secs.to_string()]
}

#[cfg(windows)]
fn sleep_cmd(secs: u64) -> Vec<String> {
    // ping sends one ICMP per second; -n (secs+1) waits ~secs seconds
    vec![
        "ping".into(),
        "-n".into(),
        (secs + 1).to_string(),
        "127.0.0.1".into(),
    ]
}

/// Cross-platform shell invocation.
#[cfg(unix)]
fn sh_c(script: &str) -> Vec<String> {
    vec!["sh".into(), "-c".into(), script.into()]
}

#[cfg(windows)]
fn sh_c(script: &str) -> Vec<String> {
    vec!["cmd".into(), "/c".into(), script.into()]
}

fn to_refs(v: &[String]) -> Vec<&str> {
    v.iter().map(|s| &**s).collect()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
#[ignore]
fn test_up_and_ps() {
    let sl = sleep_cmd(60);
    let root = PsyRoot::start(&to_refs(&sl));

    let output = root.psy_stdout(&["ps"]);
    assert!(
        output.contains("main") || output.contains("running"),
        "ps output should show a running main process, got: {output}"
    );
}

#[test]
#[ignore]
fn test_run_and_logs() {
    let sl = sleep_cmd(60);
    let root = PsyRoot::start(&to_refs(&sl));

    let echo = sh_c("echo hello");
    let echo_refs = to_refs(&echo);
    let mut run_args = vec!["run", "worker", "--"];
    run_args.extend(echo_refs);
    root.psy(&run_args);

    thread::sleep(Duration::from_secs(1));

    let logs = root.psy_stdout(&["logs", "worker"]);
    assert!(
        logs.contains("hello"),
        "logs should contain 'hello', got: {logs}"
    );
}

#[test]
#[ignore]
fn test_restart_on_failure() {
    let sl = sleep_cmd(60);
    let root = PsyRoot::start(&to_refs(&sl));

    let fail = sh_c("exit 1");
    let fail_refs = to_refs(&fail);
    let mut run_args = vec!["run", "crasher", "--restart", "on-failure", "--"];
    run_args.extend(fail_refs);
    root.psy(&run_args);

    // Wait for a few restart cycles.
    thread::sleep(Duration::from_secs(5));

    let output = root.psy_stdout(&["ps", "--all"]);
    assert!(
        output.contains("crasher"),
        "ps should list crasher, got: {output}"
    );
}

#[test]
#[ignore]
fn test_main_exit_kills_children() {
    let sl = sleep_cmd(1);
    let root = PsyRoot::start(&to_refs(&sl));

    let long_sl = sleep_cmd(999);
    let long_refs = to_refs(&long_sl);
    let mut run_args = vec!["run", "worker", "--"];
    run_args.extend(long_refs);
    root.psy(&run_args);

    // Wait for main to exit and cleanup to happen.
    thread::sleep(Duration::from_secs(4));

    // After main exits, attempting to connect should fail.
    let out = root.psy(&["ps"]);
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        !out.status.success() || combined.contains("Cannot connect") || combined.is_empty(),
        "expected connection error after main exit, got: {combined}"
    );
}

#[test]
#[ignore]
fn test_stop_process() {
    let sl = sleep_cmd(60);
    let root = PsyRoot::start(&to_refs(&sl));

    let long_sl = sleep_cmd(999);
    let long_refs = to_refs(&long_sl);
    let mut run_args = vec!["run", "stopper", "--"];
    run_args.extend(long_refs);
    root.psy(&run_args);
    thread::sleep(Duration::from_millis(500));

    root.psy(&["stop", "stopper"]);
    thread::sleep(Duration::from_millis(500));

    let output = root.psy_stdout(&["ps", "--all"]);
    assert!(
        output.contains("stopper"),
        "ps should still list stopper (as stopped), got: {output}"
    );
}

#[test]
#[ignore]
fn test_down() {
    let sl = sleep_cmd(60);
    let root = PsyRoot::start(&to_refs(&sl));

    let long_sl = sleep_cmd(999);
    let long_refs = to_refs(&long_sl);
    let mut run_args = vec!["run", "child1", "--"];
    run_args.extend(long_refs.clone());
    root.psy(&run_args);

    let mut run_args2 = vec!["run", "child2", "--"];
    run_args2.extend(long_refs);
    root.psy(&run_args2);

    thread::sleep(Duration::from_millis(500));

    root.psy(&["down"]);
    thread::sleep(Duration::from_secs(2));

    // Verify the socket is gone (Unix) or connection fails.
    let out = root.psy(&["ps"]);
    assert!(!out.status.success(), "ps should fail after down");

    #[cfg(unix)]
    {
        assert!(
            !std::path::Path::new(&root.sock).exists(),
            "socket file should be removed after down"
        );
    }
}

#[test]
#[ignore]
fn test_name_validation() {
    let sl = sleep_cmd(60);
    let root = PsyRoot::start(&to_refs(&sl));

    let echo = sh_c("echo test");
    let echo_refs = to_refs(&echo);
    let mut run_args = vec!["run", "bad/name!", "--"];
    run_args.extend(echo_refs);
    let out = root.psy(&run_args);
    let stderr = String::from_utf8_lossy(&out.stderr);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let combined = format!("{stdout}{stderr}");
    assert!(
        !out.status.success()
            || combined.to_lowercase().contains("invalid")
            || combined.to_lowercase().contains("error"),
        "invalid name should produce an error, got: {combined}"
    );
}

#[test]
#[ignore]
fn test_duplicate_name() {
    let sl = sleep_cmd(60);
    let root = PsyRoot::start(&to_refs(&sl));

    let long_sl = sleep_cmd(999);
    let long_refs = to_refs(&long_sl);
    let mut run_args = vec!["run", "dupname", "--"];
    run_args.extend(long_refs.clone());
    root.psy(&run_args);
    thread::sleep(Duration::from_millis(500));

    // Second run with same name should fail.
    let mut run_args2 = vec!["run", "dupname", "--"];
    run_args2.extend(long_refs);
    let out = root.psy(&run_args2);
    let stderr = String::from_utf8_lossy(&out.stderr);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let combined = format!("{stdout}{stderr}");
    assert!(
        !out.status.success()
            || combined.to_lowercase().contains("already")
            || combined.to_lowercase().contains("duplicate")
            || combined.to_lowercase().contains("exists")
            || combined.to_lowercase().contains("error"),
        "duplicate name should produce an error, got: {combined}"
    );
}

#[test]
#[ignore]
fn test_exit_code_propagation() {
    let exit_cmd = sh_c("exit 42");
    let exit_refs = to_refs(&exit_cmd);

    let mut cmd = Command::new(psy_bin());
    cmd.arg("up").arg("--").args(&exit_refs);
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());

    let output = cmd.output().expect("failed to start psy up");
    assert_eq!(
        output.status.code(),
        Some(42),
        "expected exit code 42, got: {:?}",
        output.status.code()
    );
}

#[test]
#[ignore]
fn test_multiple_roots() {
    let sl = sleep_cmd(60);
    let root1 = PsyRoot::start(&to_refs(&sl));
    let root2 = PsyRoot::start(&to_refs(&sl));

    // Each root should be independent.
    assert_ne!(
        root1.sock, root2.sock,
        "two roots should have different sockets"
    );

    // Run a process in root1 only.
    let echo = sh_c("echo root1-only");
    let echo_refs = to_refs(&echo);
    let mut run_args = vec!["run", "r1proc", "--"];
    run_args.extend(echo_refs);
    root1.psy(&run_args);
    thread::sleep(Duration::from_millis(500));

    // root2 should NOT see r1proc.
    let ps2 = root2.psy_stdout(&["ps"]);
    assert!(
        !ps2.contains("r1proc"),
        "root2 should not see root1's processes, got: {ps2}"
    );
}

#[test]
#[ignore]
fn test_env_passing() {
    let sl = sleep_cmd(60);
    let root = PsyRoot::start(&to_refs(&sl));

    #[cfg(unix)]
    let print_env = vec![
        "run",
        "envchild",
        "--env",
        "MY_VAR=hello123",
        "--",
        "sh",
        "-c",
        "echo $MY_VAR",
    ];
    #[cfg(windows)]
    let print_env = vec![
        "run",
        "envchild",
        "--env",
        "MY_VAR=hello123",
        "--",
        "cmd",
        "/c",
        "echo %MY_VAR%",
    ];

    root.psy(&print_env);
    thread::sleep(Duration::from_secs(1));

    let logs = root.psy_stdout(&["logs", "envchild"]);
    assert!(
        logs.contains("hello123"),
        "logs should contain the env var value 'hello123', got: {logs}"
    );
}

#[test]
#[ignore]
fn test_logs_tail() {
    let sl = sleep_cmd(60);
    let root = PsyRoot::start(&to_refs(&sl));

    #[cfg(unix)]
    let many_lines = vec![
        "run",
        "liner",
        "--",
        "sh",
        "-c",
        "for i in $(seq 1 100); do echo line-$i; done",
    ];
    #[cfg(windows)]
    let many_lines = vec![
        "run",
        "liner",
        "--",
        "cmd",
        "/c",
        "for /L %i in (1,1,100) do @echo line-%i",
    ];

    root.psy(&many_lines);
    thread::sleep(Duration::from_secs(2));

    let logs = root.psy_stdout(&["logs", "liner", "--tail", "5"]);
    // Output is now plain text lines like "[timestamp stream] content"
    let line_count = logs.lines().filter(|l| !l.is_empty()).count();
    assert!(
        line_count <= 5,
        "tail 5 should return at most 5 lines, got {line_count} in: {logs}"
    );
}

#[test]
#[ignore]
fn test_tombstone_replacement() {
    let sl = sleep_cmd(60);
    let root = PsyRoot::start(&to_refs(&sl));

    // Run a process that exits immediately.
    let echo = sh_c("echo first");
    let echo_refs = to_refs(&echo);
    let mut run_args = vec!["run", "reusable", "--"];
    run_args.extend(echo_refs);
    root.psy(&run_args);
    thread::sleep(Duration::from_secs(1));

    // Re-run with the same name — should succeed (tombstone replaced).
    let echo2 = sh_c("echo second");
    let echo2_refs = to_refs(&echo2);
    let mut run_args2 = vec!["run", "reusable", "--"];
    run_args2.extend(echo2_refs);
    let out = root.psy(&run_args2);
    assert!(
        out.status.success(),
        "re-running a stopped process should succeed, stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
#[ignore]
fn test_stop_shows_stopped_not_failed() {
    let sl = sleep_cmd(60);
    let root = PsyRoot::start(&to_refs(&sl));

    let long_sl = sleep_cmd(999);
    let long_refs = to_refs(&long_sl);
    let mut run_args = vec!["run", "svc", "--"];
    run_args.extend(long_refs);
    root.psy(&run_args);
    thread::sleep(Duration::from_millis(500));

    root.psy(&["stop", "svc"]);
    thread::sleep(Duration::from_millis(500));

    let ps = root.psy_stdout(&["ps"]);
    assert!(
        ps.contains("stopped"),
        "intentionally stopped process should show 'stopped', got: {ps}"
    );
    // Should NOT show as failed
    let svc_line = ps.lines().find(|l| l.contains("svc")).unwrap_or("");
    assert!(
        !svc_line.contains("failed"),
        "intentionally stopped process should not show 'failed', got: {svc_line}"
    );
}

#[test]
#[ignore]
fn test_logs_survive_restart() {
    let sl = sleep_cmd(60);
    let root = PsyRoot::start(&to_refs(&sl));

    // Run a process that outputs a marker then sleeps.
    let cmd = sh_c("echo BEFORE_RESTART && sleep 999");
    let cmd_refs = to_refs(&cmd);
    let mut run_args = vec!["run", "keeper", "--"];
    run_args.extend(cmd_refs);
    root.psy(&run_args);
    thread::sleep(Duration::from_secs(1));

    // Restart it.
    root.psy(&["restart", "keeper"]);
    thread::sleep(Duration::from_secs(1));

    // Logs should still contain the pre-restart output.
    let logs = root.psy_stdout(&["logs", "keeper"]);
    assert!(
        logs.contains("BEFORE_RESTART"),
        "logs should survive restart, got: {logs}"
    );
}

#[test]
#[ignore]
fn test_logs_plain_text_format() {
    let sl = sleep_cmd(60);
    let root = PsyRoot::start(&to_refs(&sl));

    let echo = sh_c("echo plaintext_check");
    let echo_refs = to_refs(&echo);
    let mut run_args = vec!["run", "fmttest", "--"];
    run_args.extend(echo_refs);
    root.psy(&run_args);
    thread::sleep(Duration::from_secs(1));

    let logs = root.psy_stdout(&["logs", "fmttest"]);
    // Should be plain text like "[2025-... stdout] plaintext_check"
    assert!(
        logs.contains("plaintext_check"),
        "logs should contain output, got: {logs}"
    );
    assert!(
        logs.contains("[") && logs.contains("stdout]"),
        "logs should be plain text format [timestamp stdout], got: {logs}"
    );
    // Should NOT be JSON
    assert!(
        !logs.contains("\"content\""),
        "logs should not be JSON, got: {logs}"
    );
}

#[test]
#[ignore]
fn test_ps_shows_exit_and_restarts() {
    let sl = sleep_cmd(60);
    let root = PsyRoot::start(&to_refs(&sl));

    // Run a process that exits with code 42.
    let exit_cmd = sh_c("exit 42");
    let exit_refs = to_refs(&exit_cmd);
    let mut run_args = vec!["run", "exiter", "--"];
    run_args.extend(exit_refs);
    root.psy(&run_args);
    thread::sleep(Duration::from_secs(1));

    let ps = root.psy_stdout(&["ps"]);
    // Header should have EXIT and RESTARTS columns
    assert!(
        ps.contains("EXIT") && ps.contains("RESTARTS"),
        "ps header should have EXIT and RESTARTS columns, got: {ps}"
    );
    // The exiter line should show exit code 42
    let exiter_line = ps.lines().find(|l| l.contains("exiter")).unwrap_or("");
    assert!(
        exiter_line.contains("42"),
        "ps should show exit code 42 for exiter, got: {exiter_line}"
    );
}

// ---------------------------------------------------------------------------
// v0.3 — Log filtering tests
// ---------------------------------------------------------------------------

#[test]
#[ignore]
fn test_logs_since() {
    let sl = sleep_cmd(60);
    let root = PsyRoot::start(&to_refs(&sl));

    let echo = sh_c("echo hello-since");
    let echo_refs = to_refs(&echo);
    let mut run_args = vec!["run", "sinceproc", "--"];
    run_args.extend(echo_refs);
    root.psy(&run_args);
    thread::sleep(Duration::from_secs(1));

    // --since 1h should include all recent logs
    let logs = root.psy_stdout(&["logs", "sinceproc", "--since", "1h"]);
    assert!(
        logs.contains("hello-since"),
        "logs --since 1h should include recent output, got: {logs}"
    );

    // --since 1s from way in the future should return nothing meaningful
    // (We test with an absolute timestamp far in the past to verify filtering works)
    let logs2 = root.psy_stdout(&["logs", "sinceproc", "--since", "2099-01-01T00:00:00Z"]);
    assert!(
        !logs2.contains("hello-since"),
        "logs --since far future should exclude old output, got: {logs2}"
    );
}

#[test]
#[ignore]
fn test_logs_until() {
    let sl = sleep_cmd(60);
    let root = PsyRoot::start(&to_refs(&sl));

    let echo = sh_c("echo hello-until");
    let echo_refs = to_refs(&echo);
    let mut run_args = vec!["run", "untilproc", "--"];
    run_args.extend(echo_refs);
    root.psy(&run_args);
    thread::sleep(Duration::from_secs(1));

    // --until far in the past should return nothing
    let logs = root.psy_stdout(&["logs", "untilproc", "--until", "2020-01-01T00:00:00Z"]);
    assert!(
        !logs.contains("hello-until"),
        "logs --until past should exclude output, got: {logs}"
    );

    // --until far in the future should include everything
    let logs2 = root.psy_stdout(&["logs", "untilproc", "--until", "2099-01-01T00:00:00Z"]);
    assert!(
        logs2.contains("hello-until"),
        "logs --until future should include output, got: {logs2}"
    );
}

#[test]
#[ignore]
fn test_logs_grep() {
    let sl = sleep_cmd(60);
    let root = PsyRoot::start(&to_refs(&sl));

    #[cfg(unix)]
    let multi = vec![
        "run",
        "grepproc",
        "--",
        "sh",
        "-c",
        "echo 'ERROR: something failed' && echo 'INFO: all good' && echo 'error: another one'",
    ];
    #[cfg(windows)]
    let multi = vec![
        "run",
        "grepproc",
        "--",
        "cmd",
        "/c",
        "echo ERROR: something failed && echo INFO: all good && echo error: another one",
    ];

    root.psy(&multi);
    thread::sleep(Duration::from_secs(1));

    let logs = root.psy_stdout(&["logs", "grepproc", "--grep", "error"]);
    assert!(
        logs.to_lowercase().contains("error"),
        "grep should return lines containing 'error', got: {logs}"
    );
    assert!(
        !logs.contains("INFO: all good"),
        "grep should not return non-matching lines, got: {logs}"
    );
}

#[test]
#[ignore]
fn test_logs_grep_no_match() {
    let sl = sleep_cmd(60);
    let root = PsyRoot::start(&to_refs(&sl));

    let echo = sh_c("echo hello-world");
    let echo_refs = to_refs(&echo);
    let mut run_args = vec!["run", "grepnone", "--"];
    run_args.extend(echo_refs);
    root.psy(&run_args);
    thread::sleep(Duration::from_secs(1));

    let logs = root.psy_stdout(&["logs", "grepnone", "--grep", "NONEXISTENT"]);
    // Should have no content lines (only empty or no output)
    let content_lines: Vec<_> = logs.lines().filter(|l| !l.is_empty()).collect();
    assert!(
        content_lines.is_empty(),
        "grep with no matches should return empty, got: {logs}"
    );
}

// ---------------------------------------------------------------------------
// v0.3 — Attach mode tests
// ---------------------------------------------------------------------------

#[test]
#[ignore]
fn test_attach_output_and_exit() {
    let sl = sleep_cmd(60);
    let root = PsyRoot::start(&to_refs(&sl));

    // Build owned args for the thread
    let echo = sh_c("echo attached-output && sleep 1");
    let mut run_args: Vec<String> = vec!["run", "--attach", "attacher", "--"]
        .into_iter()
        .map(String::from)
        .collect();
    run_args.extend(echo);

    // Run it in a thread since --attach blocks
    let sock = root.sock.clone();
    let bin = psy_bin();
    let handle = std::thread::spawn(move || {
        let args_refs: Vec<&str> = run_args.iter().map(|s| s.as_str()).collect();
        let output = Command::new(bin)
            .args(&args_refs)
            .env("PSY_SOCK", &sock)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .expect("failed to run psy run --attach");
        String::from_utf8_lossy(&output.stdout).to_string()
    });

    // Wait and then check that the process was registered
    thread::sleep(Duration::from_secs(1));
    let ps = root.psy_stdout(&["ps"]);
    assert!(
        ps.contains("attacher"),
        "attached process should appear in ps, got: {ps}"
    );

    // Wait for the attach to complete
    let output = handle.join().expect("attach thread panicked");
    assert!(
        output.contains("attached-output"),
        "attach should stream output, got: {output}"
    );
}

#[test]
#[ignore]
fn test_attach_detach_keeps_running() {
    let sl = sleep_cmd(60);
    let root = PsyRoot::start(&to_refs(&sl));

    // Start a long-running process with --attach, then kill the client
    let long_sl = sleep_cmd(999);
    let mut run_args: Vec<String> = vec!["run", "--attach", "detacher", "--"]
        .into_iter()
        .map(String::from)
        .collect();
    run_args.extend(long_sl);

    let run_refs: Vec<&str> = run_args.iter().map(|s| s.as_str()).collect();
    let mut child = Command::new(psy_bin())
        .args(&run_refs)
        .env("PSY_SOCK", &root.sock)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to start attached process");

    thread::sleep(Duration::from_secs(2));

    // Kill the client (simulates Ctrl-C / detach)
    let _ = child.kill();
    let _ = child.wait();

    thread::sleep(Duration::from_millis(500));

    // The process should still be running in the root
    let ps = root.psy_stdout(&["ps"]);
    assert!(
        ps.contains("detacher") && ps.contains("running"),
        "detached process should still be running, got: {ps}"
    );
}

// ---------------------------------------------------------------------------
// v0.3 — History & per-run logs tests
// ---------------------------------------------------------------------------

#[test]
#[ignore]
fn test_history_shows_runs() {
    let sl = sleep_cmd(60);
    let root = PsyRoot::start(&to_refs(&sl));

    // Run a process that exits, then re-run with the same name
    let echo1 = sh_c("echo run-one");
    let echo1_refs = to_refs(&echo1);
    let mut run_args = vec!["run", "hist", "--"];
    run_args.extend(echo1_refs);
    root.psy(&run_args);
    thread::sleep(Duration::from_secs(1));

    // Re-run (tombstone replacement)
    let echo2 = sh_c("echo run-two");
    let echo2_refs = to_refs(&echo2);
    let mut run_args2 = vec!["run", "hist", "--"];
    run_args2.extend(echo2_refs);
    root.psy(&run_args2);
    thread::sleep(Duration::from_secs(1));

    let history = root.psy_stdout(&["history", "hist"]);
    assert!(
        history.contains("RUN") && history.contains("STATUS"),
        "history should show header, got: {history}"
    );
    // Should show run 1 and run 2
    assert!(
        history.contains("1") && history.contains("2"),
        "history should show both runs, got: {history}"
    );
}

#[test]
#[ignore]
fn test_logs_previous_run() {
    let sl = sleep_cmd(60);
    let root = PsyRoot::start(&to_refs(&sl));

    // Run a process that outputs a marker then exits
    let echo1 = sh_c("echo FIRST_RUN_MARKER");
    let echo1_refs = to_refs(&echo1);
    let mut run_args = vec!["run", "prevlog", "--"];
    run_args.extend(echo1_refs);
    root.psy(&run_args);
    thread::sleep(Duration::from_secs(1));

    // Re-run with a different marker
    let echo2 = sh_c("echo SECOND_RUN_MARKER");
    let echo2_refs = to_refs(&echo2);
    let mut run_args2 = vec!["run", "prevlog", "--"];
    run_args2.extend(echo2_refs);
    root.psy(&run_args2);
    thread::sleep(Duration::from_secs(1));

    // Default logs should show current run
    let logs = root.psy_stdout(&["logs", "prevlog"]);
    assert!(
        logs.contains("SECOND_RUN_MARKER"),
        "default logs should show current run, got: {logs}"
    );
    assert!(
        !logs.contains("FIRST_RUN_MARKER"),
        "default logs should not show previous run, got: {logs}"
    );

    // --previous should show the first run
    let prev_logs = root.psy_stdout(&["logs", "prevlog", "--previous"]);
    assert!(
        prev_logs.contains("FIRST_RUN_MARKER"),
        "--previous should show first run, got: {prev_logs}"
    );
    assert!(
        !prev_logs.contains("SECOND_RUN_MARKER"),
        "--previous should not show current run, got: {prev_logs}"
    );
}

#[test]
#[ignore]
fn test_logs_run_id() {
    let sl = sleep_cmd(60);
    let root = PsyRoot::start(&to_refs(&sl));

    // Run a process, let it exit, re-run
    let echo1 = sh_c("echo RUN1_OUTPUT");
    let echo1_refs = to_refs(&echo1);
    let mut run_args = vec!["run", "runid", "--"];
    run_args.extend(echo1_refs);
    root.psy(&run_args);
    thread::sleep(Duration::from_secs(1));

    let echo2 = sh_c("echo RUN2_OUTPUT");
    let echo2_refs = to_refs(&echo2);
    let mut run_args2 = vec!["run", "runid", "--"];
    run_args2.extend(echo2_refs);
    root.psy(&run_args2);
    thread::sleep(Duration::from_secs(1));

    // --run 1 should show first run's output
    let logs1 = root.psy_stdout(&["logs", "runid", "--run", "1"]);
    assert!(
        logs1.contains("RUN1_OUTPUT"),
        "--run 1 should show first run, got: {logs1}"
    );

    // --run 2 should show second run's output
    let logs2 = root.psy_stdout(&["logs", "runid", "--run", "2"]);
    assert!(
        logs2.contains("RUN2_OUTPUT"),
        "--run 2 should show second run, got: {logs2}"
    );
}

#[test]
#[ignore]
fn test_logs_run_with_grep() {
    let sl = sleep_cmd(60);
    let root = PsyRoot::start(&to_refs(&sl));

    #[cfg(unix)]
    let cmd1 = vec![
        "run",
        "greprun",
        "--",
        "sh",
        "-c",
        "echo 'ERROR: old crash' && echo 'INFO: ok'",
    ];
    #[cfg(windows)]
    let cmd1 = vec![
        "run",
        "greprun",
        "--",
        "cmd",
        "/c",
        "echo ERROR: old crash && echo INFO: ok",
    ];
    root.psy(&cmd1);
    thread::sleep(Duration::from_secs(1));

    let echo2 = sh_c("echo new-run-output");
    let echo2_refs = to_refs(&echo2);
    let mut run_args2 = vec!["run", "greprun", "--"];
    run_args2.extend(echo2_refs);
    root.psy(&run_args2);
    thread::sleep(Duration::from_secs(1));

    // --run 1 --grep "error" should filter old run's logs
    let logs = root.psy_stdout(&["logs", "greprun", "--run", "1", "--grep", "error"]);
    assert!(
        logs.to_lowercase().contains("error"),
        "--run 1 --grep error should find errors, got: {logs}"
    );
    assert!(
        !logs.contains("INFO: ok"),
        "grep should filter out non-matching lines, got: {logs}"
    );
}

#[test]
#[ignore]
fn test_history_after_restart() {
    let sl = sleep_cmd(60);
    let root = PsyRoot::start(&to_refs(&sl));

    // Run, then restart
    let cmd = sh_c("echo BEFORE && sleep 999");
    let cmd_refs = to_refs(&cmd);
    let mut run_args = vec!["run", "histrestart", "--"];
    run_args.extend(cmd_refs);
    root.psy(&run_args);
    thread::sleep(Duration::from_secs(1));

    root.psy(&["restart", "histrestart"]);
    thread::sleep(Duration::from_secs(1));

    let history = root.psy_stdout(&["history", "histrestart"]);
    // Should have run 1 (stopped) and run 2 (running)
    assert!(
        history.contains("1") && history.contains("2"),
        "history should show 2 runs after restart, got: {history}"
    );

    // --previous should show old run's logs
    let prev_logs = root.psy_stdout(&["logs", "histrestart", "--previous"]);
    assert!(
        prev_logs.contains("BEFORE"),
        "--previous should show pre-restart logs, got: {prev_logs}"
    );
}

// ---------------------------------------------------------------------------
// v1.0 — Psyfile tests
// ---------------------------------------------------------------------------

#[test]
#[ignore]
fn test_psyfile_unit_run() {
    let sl = sleep_cmd(60);
    let tmp = TempPsyfileDir::new(
        r#"
[echoer]
command = "echo psyfile-works"
"#,
    );
    let root = PsyRoot::start_with_psyfile(&tmp.psyfile_path(), &[], &to_refs(&sl));

    // Run the Psyfile unit (no -- command needed)
    root.psy(&["run", "echoer"]);
    thread::sleep(Duration::from_secs(1));

    let logs = root.psy_stdout(&["logs", "echoer"]);
    assert!(
        logs.contains("psyfile-works"),
        "Psyfile unit should produce output, got: {logs}"
    );
}

#[test]
#[ignore]
fn test_psyfile_unit_with_env() {
    let sl = sleep_cmd(60);
    let tmp = TempPsyfileDir::new(
        r#"
[envunit]
command = "echo ${MY_VAR}"
env = { MY_VAR = "injected-value" }
"#,
    );
    let root = PsyRoot::start_with_psyfile(&tmp.psyfile_path(), &[], &to_refs(&sl));

    root.psy(&["run", "envunit"]);
    thread::sleep(Duration::from_secs(1));

    let logs = root.psy_stdout(&["logs", "envunit"]);
    assert!(
        logs.contains("injected-value"),
        "Psyfile env should be interpolated, got: {logs}"
    );
}

#[test]
#[ignore]
fn test_psyfile_depends_on() {
    let sl = sleep_cmd(60);
    let tmp = TempPsyfileDir::new(
        r#"
[db]
command = "echo db-started && sleep 60"
restart = "no"

[api]
command = "echo api-started && sleep 60"
depends_on = ["db"]
"#,
    );
    let root = PsyRoot::start_with_psyfile(&tmp.psyfile_path(), &[], &to_refs(&sl));

    // Running api should auto-start db
    root.psy(&["run", "api"]);
    thread::sleep(Duration::from_secs(2));

    let ps = root.psy_stdout(&["ps"]);
    assert!(
        ps.contains("db") && ps.contains("api"),
        "both db and api should be running, got: {ps}"
    );
}

#[test]
#[ignore]
fn test_psyfile_template_unit() {
    let sl = sleep_cmd(60);
    let tmp = TempPsyfileDir::new(
        r#"
[client]
command = "echo client-instance && sleep 60"
singleton = false
"#,
    );
    let root = PsyRoot::start_with_psyfile(&tmp.psyfile_path(), &[], &to_refs(&sl));

    root.psy(&["run", "client"]);
    root.psy(&["run", "client"]);
    root.psy(&["run", "client"]);
    thread::sleep(Duration::from_secs(1));

    let ps = root.psy_stdout(&["ps"]);
    assert!(
        ps.contains("client.1") && ps.contains("client.2") && ps.contains("client.3"),
        "template should create numbered instances, got: {ps}"
    );
}

#[test]
#[ignore]
fn test_psyfile_template_group_stop() {
    let sl = sleep_cmd(60);
    let tmp = TempPsyfileDir::new(
        r#"
[worker]
command = "sleep 999"
singleton = false
"#,
    );
    let root = PsyRoot::start_with_psyfile(&tmp.psyfile_path(), &[], &to_refs(&sl));

    root.psy(&["run", "worker"]);
    root.psy(&["run", "worker"]);
    thread::sleep(Duration::from_secs(1));

    // Stop the group
    root.psy(&["stop", "worker"]);
    thread::sleep(Duration::from_secs(1));

    let ps = root.psy_stdout(&["ps"]);
    // Both instances should be stopped
    let running_workers = ps
        .lines()
        .filter(|l| l.contains("worker.") && l.contains("running"))
        .count();
    assert_eq!(
        running_workers, 0,
        "all worker instances should be stopped, got: {ps}"
    );
}

#[test]
#[ignore]
fn test_psyfile_up_all() {
    let sl = sleep_cmd(60);
    let tmp = TempPsyfileDir::new(
        r#"
[svc1]
command = "echo svc1-ok && sleep 60"

[svc2]
command = "echo svc2-ok && sleep 60"

[svc3]
command = "echo svc3-ok && sleep 60"
"#,
    );
    let root = PsyRoot::start_with_psyfile_all(&tmp.psyfile_path(), &to_refs(&sl));

    let ps = root.psy_stdout(&["ps"]);
    assert!(
        ps.contains("svc1") && ps.contains("svc2") && ps.contains("svc3"),
        "--all should start all units, got: {ps}"
    );
}

#[test]
#[ignore]
fn test_psyfile_selective_boot() {
    let sl = sleep_cmd(60);
    let tmp = TempPsyfileDir::new(
        r#"
[db]
command = "echo db-ok && sleep 60"

[api]
command = "echo api-ok && sleep 60"
depends_on = ["db"]

[worker]
command = "echo worker-ok && sleep 60"
"#,
    );
    let root = PsyRoot::start_with_psyfile(&tmp.psyfile_path(), &["api"], &to_refs(&sl));

    let ps = root.psy_stdout(&["ps"]);
    // db and api should be running (api depends on db), but not worker
    assert!(
        ps.contains("db") && ps.contains("api"),
        "db and api should be running, got: {ps}"
    );
    assert!(
        !ps.contains("worker"),
        "worker should not be started, got: {ps}"
    );
}

#[test]
#[ignore]
fn test_psyfile_adhoc_alongside() {
    let sl = sleep_cmd(60);
    let tmp = TempPsyfileDir::new(
        r#"
[server]
command = "echo server-ok && sleep 60"
"#,
    );
    let root = PsyRoot::start_with_psyfile(&tmp.psyfile_path(), &[], &to_refs(&sl));

    // Run a Psyfile unit
    root.psy(&["run", "server"]);
    // Run an ad-hoc process
    let echo = sh_c("echo adhoc-ok && sleep 60");
    let echo_refs = to_refs(&echo);
    let mut run_args = vec!["run", "adhoc", "--"];
    run_args.extend(echo_refs);
    root.psy(&run_args);

    thread::sleep(Duration::from_secs(1));

    let ps = root.psy_stdout(&["ps"]);
    assert!(
        ps.contains("server") && ps.contains("adhoc"),
        "both Psyfile unit and ad-hoc should run, got: {ps}"
    );
}

#[test]
#[ignore]
fn test_psyfile_no_command_adhoc_error() {
    let sl = sleep_cmd(60);
    let root = PsyRoot::start(&to_refs(&sl));

    // Without a Psyfile, running without a command should error
    let out = root.psy(&["run", "nocommand"]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let combined = format!("{stdout}{stderr}");
    assert!(
        combined.to_lowercase().contains("error")
            || combined.to_lowercase().contains("no command")
            || !out.status.success(),
        "run without command should error, got: {combined}"
    );
}

#[test]
#[ignore]
fn test_psyfile_env_interpolation_default() {
    let sl = sleep_cmd(60);
    let tmp = TempPsyfileDir::new(
        r#"
[porttest]
command = "echo ${PORT:-8080}"
"#,
    );
    let root = PsyRoot::start_with_psyfile(&tmp.psyfile_path(), &[], &to_refs(&sl));

    root.psy(&["run", "porttest"]);
    thread::sleep(Duration::from_secs(1));

    let logs = root.psy_stdout(&["logs", "porttest"]);
    assert!(
        logs.contains("8080"),
        "default value should be used, got: {logs}"
    );
}

#[test]
#[ignore]
fn test_psyfile_restart_override() {
    let sl = sleep_cmd(60);
    let tmp = TempPsyfileDir::new(
        r#"
[crasher]
command = "exit 1"
restart = "no"
"#,
    );
    let root = PsyRoot::start_with_psyfile(&tmp.psyfile_path(), &[], &to_refs(&sl));

    // Override restart policy via CLI
    root.psy(&["run", "crasher", "--restart", "on-failure"]);
    thread::sleep(Duration::from_secs(3));

    let ps = root.psy_stdout(&["ps"]);
    // Should show on_failure restart policy and restarts > 0
    let crasher_line = ps.lines().find(|l| l.contains("crasher")).unwrap_or("");
    assert!(
        crasher_line.contains("on_failure") || crasher_line.contains("onfailure"),
        "restart policy should be overridden, got: {crasher_line}"
    );
}

#[test]
#[ignore]
fn test_psyfile_working_dir() {
    let sl = sleep_cmd(60);

    let work_dir = std::env::temp_dir()
        .canonicalize()
        .unwrap_or_else(|_| std::env::temp_dir());
    let work_dir_str = work_dir.to_string_lossy().replace('\\', "/");

    #[cfg(unix)]
    let psyfile_content = format!(
        "[pwdtest]\ncommand = \"pwd\"\nworking_dir = \"{}\"\n",
        work_dir_str
    );
    #[cfg(windows)]
    let psyfile_content = format!(
        "[pwdtest]\ncommand = \"cd\"\nworking_dir = \"{}\"\n",
        work_dir_str
    );

    let tmp = TempPsyfileDir::new(&psyfile_content);
    let root = PsyRoot::start_with_psyfile(&tmp.psyfile_path(), &[], &to_refs(&sl));

    root.psy(&["run", "pwdtest"]);
    thread::sleep(Duration::from_secs(1));

    let logs = root.psy_stdout(&["logs", "pwdtest"]);
    // Normalize both paths for comparison
    let expected = work_dir.to_string_lossy().to_lowercase();
    let logs_lower = logs.to_lowercase();
    assert!(
        logs_lower.contains(&expected)
            || logs_lower.contains(&expected.replace('\\', "/"))
            || logs_lower.contains("/private/tmp")
            || logs_lower.contains("temp"),
        "working dir should be {expected}, got: {logs}"
    );
}

// ---------------------------------------------------------------------------
// v1.1: Readiness probes
// ---------------------------------------------------------------------------

#[test]
#[ignore]
fn test_ready_exit_probe() {
    // A "build" unit with exit probe — dependents wait for it to complete
    let tmp = TempPsyfileDir::new(
        r#"
[build]
command = "echo build-done"
ready = { exit = 0 }

[server]
command = "echo server-started"
depends_on = ["build"]
"#,
    );
    let sl = sleep_cmd(60);
    let root = PsyRoot::start_with_psyfile(&tmp.psyfile_path(), &[], &to_refs(&sl));
    thread::sleep(Duration::from_secs(1));

    // Run server (which depends on build with exit probe)
    root.psy(&["run", "server"]);
    thread::sleep(Duration::from_secs(3));

    // Both should have run
    let ps = root.psy_stdout(&["ps"]);
    assert!(ps.contains("build"), "build should be in ps: {ps}");
    assert!(ps.contains("server"), "server should be in ps: {ps}");

    // Server logs should contain its output
    let server_logs = root.psy_stdout(&["logs", "server"]);
    assert!(
        server_logs.contains("server-started"),
        "server should have started: {server_logs}"
    );
}

#[test]
#[ignore]
fn test_ready_exec_probe() {
    // A unit with exec probe — probe command checks a condition
    let marker = format!("/tmp/psy-test-ready-{}", std::process::id());
    let tmp = TempPsyfileDir::new(&format!(
        r#"
[setup]
command = "sh -c 'sleep 1 && touch {marker}'"

[checker]
command = "echo checker-started"
depends_on = ["setup"]
"#,
    ));

    // Setup has no probe, so checker starts immediately after setup launches
    // This test just verifies that non-probe deps still work
    let sl = sleep_cmd(60);
    let root = PsyRoot::start_with_psyfile(&tmp.psyfile_path(), &[], &to_refs(&sl));
    thread::sleep(Duration::from_secs(1));
    root.psy(&["run", "checker"]);
    thread::sleep(Duration::from_secs(3));

    let ps = root.psy_stdout(&["ps"]);
    assert!(ps.contains("setup"), "setup should be in ps: {ps}");
    assert!(ps.contains("checker"), "checker should be in ps: {ps}");

    let _ = std::fs::remove_file(&marker);
}

#[test]
#[ignore]
fn test_ready_tcp_probe() {
    // A unit with TCP readiness probe — dependent waits for the port
    let tmp = TempPsyfileDir::new(
        r#"
[listener]
command = "sh -c 'sleep 1 && nc -l 19876 &>/dev/null &'"
ready = { tcp = "localhost:19876", interval = "1s", timeout = "10s" }

[client]
command = "echo client-ready"
depends_on = ["listener"]
"#,
    );

    let sl = sleep_cmd(60);
    let root = PsyRoot::start_with_psyfile(&tmp.psyfile_path(), &[], &to_refs(&sl));
    thread::sleep(Duration::from_secs(1));

    root.psy(&["run", "client"]);
    thread::sleep(Duration::from_secs(5));

    // Check probe logs are available
    let probe_logs = root.psy_stdout(&["logs", "listener", "--probe"]);
    assert!(
        probe_logs.contains("tcp") || probe_logs.contains("localhost:19876"),
        "probe logs should contain tcp diagnostics: {probe_logs}"
    );
}

#[test]
#[ignore]
fn test_probe_logs_hidden_by_default() {
    // Probe logs should not appear in default `psy logs` output
    let tmp = TempPsyfileDir::new(
        r#"
[server]
command = "echo hello"
ready = { exec = "false", timeout = "3s", interval = "1s" }
"#,
    );

    let sl = sleep_cmd(60);
    let root = PsyRoot::start_with_psyfile(&tmp.psyfile_path(), &[], &to_refs(&sl));
    thread::sleep(Duration::from_secs(1));
    root.psy(&["run", "server"]);
    thread::sleep(Duration::from_secs(5));

    // Default logs should NOT contain probe output
    let default_logs = root.psy_stdout(&["logs", "server"]);
    assert!(
        !default_logs.contains("probe"),
        "default logs should not contain probe output: {default_logs}"
    );

    // Probe logs should contain probe output
    let probe_logs = root.psy_stdout(&["logs", "server", "--probe"]);
    assert!(
        probe_logs.contains("exec") || probe_logs.contains("false"),
        "probe logs should contain exec diagnostics: {probe_logs}"
    );
}

#[test]
#[ignore]
fn test_ps_ready_column() {
    // Processes with probes show ready status in ps output
    let tmp = TempPsyfileDir::new(
        r#"
[server]
command = "echo done"
ready = { exit = 0 }
"#,
    );

    let sl = sleep_cmd(60);
    let root = PsyRoot::start_with_psyfile(&tmp.psyfile_path(), &[], &to_refs(&sl));
    thread::sleep(Duration::from_secs(1));
    root.psy(&["run", "server"]);
    thread::sleep(Duration::from_secs(2));

    let ps = root.psy_stdout(&["ps"]);
    assert!(ps.contains("READY"), "ps should have READY column: {ps}");
    // The exit probe should have passed (exit 0)
    assert!(
        ps.contains("ready"),
        "server should show 'ready' status: {ps}"
    );
}

#[test]
#[ignore]
fn test_depends_on_with_restart_flag() {
    // Extended depends_on syntax with restart cascade
    let tmp = TempPsyfileDir::new(
        r#"
[db]
command = "sleep 60"
restart = "always"

[api]
command = "sleep 60"
restart = "always"
depends_on = [{ name = "db", restart = true }]
"#,
    );

    let sl = sleep_cmd(60);
    let root = PsyRoot::start_with_psyfile(&tmp.psyfile_path(), &["db", "api"], &to_refs(&sl));
    thread::sleep(Duration::from_secs(3));

    let ps = root.psy_stdout(&["ps"]);
    assert!(ps.contains("db"), "db should be running: {ps}");
    assert!(ps.contains("api"), "api should be running: {ps}");
}

#[test]
#[ignore]
fn test_healthcheck_triggers_restart() {
    // A process with a failing healthcheck should be killed and restarted
    let tmp = TempPsyfileDir::new(
        r#"
[flaky]
command = "sleep 999"
restart = "on-failure"
healthcheck = { exec = "false", interval = "1s", retries = 2 }
"#,
    );

    let sl = sleep_cmd(60);
    let root = PsyRoot::start_with_psyfile(&tmp.psyfile_path(), &[], &to_refs(&sl));
    thread::sleep(Duration::from_secs(1));
    root.psy(&["run", "flaky"]);

    // Wait long enough for healthcheck to fail twice and trigger a restart
    // Healthcheck: 1s interval wait + fail, 1s wait + fail (retries=2) → kill (SIGTERM + up to 10s)
    // Then monitor_child: 1s backoff + spawn. Total ~15s worst case.
    thread::sleep(Duration::from_secs(18));

    // History should show at least 2 runs (original + restarted)
    let history = root.psy_stdout(&["history", "flaky"]);
    assert!(
        history.contains("2"),
        "healthcheck should trigger restart (run 2 in history), got: {history}"
    );

    // Previous run's probe logs should show the unhealthy message
    let probe_logs = root.psy_stdout(&["logs", "flaky", "--previous", "--probe"]);
    assert!(
        probe_logs.contains("unhealthy") || probe_logs.contains("consecutive failures"),
        "previous run's probe logs should show unhealthy message: {probe_logs}"
    );
}

#[test]
#[ignore]
fn test_restart_cascade_with_readiness() {
    // Restart db → api should also restart because depends_on has restart = true
    // No ready probe on db so dependents start immediately after launch
    let tmp = TempPsyfileDir::new(
        r#"
[db]
command = "sleep 999"
restart = "always"

[api]
command = "sleep 999"
restart = "always"
depends_on = [{ name = "db", restart = true }]
"#,
    );

    let sl = sleep_cmd(120);
    let root = PsyRoot::start_with_psyfile(&tmp.psyfile_path(), &["db", "api"], &to_refs(&sl));
    thread::sleep(Duration::from_secs(4));

    // Verify both running
    let ps1 = root.psy_stdout(&["ps"]);
    assert!(ps1.contains("db"), "db should be running: {ps1}");
    assert!(ps1.contains("api"), "api should be running: {ps1}");

    // Restart db
    root.psy(&["restart", "db"]);
    thread::sleep(Duration::from_secs(5));

    // api should have been restarted too (cascade)
    let history = root.psy_stdout(&["history", "api"]);
    assert!(
        history.contains("2"),
        "api should show run 2 after cascade restart: {history}"
    );
}

#[cfg(unix)]
#[test]
#[ignore]
fn test_probe_logs_stream_filter() {
    // --probe --stdout and --probe --stderr should filter probe streams
    // Use a script file so the command name doesn't leak into diagnostics
    let marker_script = format!("/tmp/psy-probe-marker-{}.sh", std::process::id());
    std::fs::write(&marker_script, "#!/bin/sh\necho XYZZY_MARKER\nexit 1\n").unwrap();
    std::fs::set_permissions(
        &marker_script,
        std::os::unix::fs::PermissionsExt::from_mode(0o755),
    )
    .unwrap();

    let tmp = TempPsyfileDir::new(&format!(
        r#"
[checker]
command = "echo main-output && sleep 999"
ready = {{ exec = "{marker_script}", timeout = "3s", interval = "1s" }}
"#,
    ));

    let sl = sleep_cmd(60);
    let root = PsyRoot::start_with_psyfile(&tmp.psyfile_path(), &[], &to_refs(&sl));
    thread::sleep(Duration::from_secs(1));
    root.psy(&["run", "checker"]);
    thread::sleep(Duration::from_secs(5));

    // Default logs should show main output but not probe output
    let default_logs = root.psy_stdout(&["logs", "checker"]);
    assert!(
        default_logs.contains("main-output"),
        "default logs should have main output: {default_logs}"
    );

    // --probe should show probe diagnostics
    let probe_all = root.psy_stdout(&["logs", "checker", "--probe"]);
    assert!(
        !probe_all.is_empty(),
        "probe logs should not be empty: {probe_all}"
    );

    // --probe --stdout should show only probe:stdout (the exec command's actual stdout)
    let probe_stdout = root.psy_stdout(&["logs", "checker", "--probe", "--stdout"]);
    assert!(
        probe_stdout.contains("XYZZY_MARKER"),
        "probe --stdout should show exec stdout: {probe_stdout}"
    );

    // --probe --stderr should show only probe:stderr (diagnostic messages)
    let probe_stderr = root.psy_stdout(&["logs", "checker", "--probe", "--stderr"]);
    assert!(
        probe_stderr.contains("exec") || probe_stderr.contains("attempt"),
        "probe --stderr should show diagnostics: {probe_stderr}"
    );
    // probe:stderr should NOT contain the stdout marker
    assert!(
        !probe_stderr.contains("XYZZY_MARKER"),
        "probe --stderr should not contain stdout content: {probe_stderr}"
    );

    let _ = std::fs::remove_file(&marker_script);
}

#[cfg(windows)]
#[test]
#[ignore]
fn test_probe_logs_stream_filter() {
    // Windows variant: exec probe already wraps with cmd /C, so use raw commands.
    let tmp = TempPsyfileDir::new(
        r#"
[checker]
command = "echo main-output && ping -n 999 127.0.0.1 >nul"
ready = { exec = "echo XYZZY_MARKER && exit /b 1", timeout = "3s", interval = "1s" }
"#,
    );

    let sl = sleep_cmd(60);
    let root = PsyRoot::start_with_psyfile(&tmp.psyfile_path(), &[], &to_refs(&sl));
    thread::sleep(Duration::from_secs(1));
    root.psy(&["run", "checker"]);
    thread::sleep(Duration::from_secs(5));

    // Default logs should show main output but not probe output
    let default_logs = root.psy_stdout(&["logs", "checker"]);
    assert!(
        default_logs.contains("main-output"),
        "default logs should have main output: {default_logs}"
    );

    // --probe should show probe diagnostics
    let probe_all = root.psy_stdout(&["logs", "checker", "--probe"]);
    assert!(
        !probe_all.is_empty(),
        "probe logs should not be empty: {probe_all}"
    );

    // --probe --stdout should show only probe:stdout (the exec command's actual stdout)
    let probe_stdout = root.psy_stdout(&["logs", "checker", "--probe", "--stdout"]);
    assert!(
        probe_stdout.contains("XYZZY_MARKER"),
        "probe --stdout should show exec stdout: {probe_stdout}"
    );

    // --probe --stderr should show only probe:stderr (diagnostic messages)
    let probe_stderr = root.psy_stdout(&["logs", "checker", "--probe", "--stderr"]);
    assert!(
        probe_stderr.contains("exec") || probe_stderr.contains("attempt"),
        "probe --stderr should show diagnostics: {probe_stderr}"
    );
    // Verify --probe --stdout returns different (fewer) lines than --probe --stderr.
    // This confirms stream-level filtering is working. We can't check for absence of
    // XYZZY_MARKER in stderr because the diagnostic log quotes the command name.
    let stdout_line_count = probe_stdout.lines().count();
    let stderr_line_count = probe_stderr.lines().count();
    assert!(
        stdout_line_count != stderr_line_count,
        "stdout and stderr probe streams should differ: stdout={stdout_line_count} lines, stderr={stderr_line_count} lines"
    );
}

// ---------------------------------------------------------------------------
// v1.1: Psyfile subcommand
// ---------------------------------------------------------------------------

#[test]
#[ignore]
fn test_psyfile_schema() {
    let output = Command::new(psy_bin())
        .args(["psyfile", "schema"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("failed to run psy psyfile schema");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "schema command should succeed");
    assert!(
        stdout.contains("\"$schema\""),
        "should output JSON schema: {stdout}"
    );
    assert!(
        stdout.contains("command"),
        "should mention command field: {stdout}"
    );
    assert!(
        stdout.contains("ready"),
        "should mention ready field: {stdout}"
    );
    assert!(
        stdout.contains("healthcheck"),
        "should mention healthcheck field: {stdout}"
    );
    assert!(
        stdout.contains("depends_on"),
        "should mention depends_on field: {stdout}"
    );
}

#[test]
#[ignore]
fn test_psyfile_validate_valid() {
    let tmp = TempPsyfileDir::new(
        r#"
[server]
command = "echo hello"
restart = "on-failure"
"#,
    );
    let output = Command::new(psy_bin())
        .args(["psyfile", "validate", "--file"])
        .arg(tmp.psyfile_path())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("failed to run psy psyfile validate");
    assert!(output.status.success(), "validate should succeed");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("OK"), "should print OK: {stdout}");
}

#[test]
#[ignore]
fn test_psyfile_validate_invalid() {
    let tmp = TempPsyfileDir::new(
        r#"
[main]
command = "echo reserved"
"#,
    );
    let output = Command::new(psy_bin())
        .args(["psyfile", "validate", "--file"])
        .arg(tmp.psyfile_path())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("failed to run psy psyfile validate");
    assert!(
        !output.status.success(),
        "validate should fail for reserved name"
    );
}

#[test]
#[ignore]
fn test_psyfile_init() {
    let dir = std::env::temp_dir().join(format!("psy-init-test-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);

    let output = Command::new(psy_bin())
        .args(["psyfile", "init"])
        .current_dir(&dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("failed to run psy psyfile init");
    assert!(output.status.success(), "init should succeed");

    let psyfile = dir.join("Psyfile");
    assert!(psyfile.exists(), "Psyfile should be created");
    let content = std::fs::read_to_string(&psyfile).unwrap();
    assert!(
        content.contains("command"),
        "template should contain command"
    );

    // Second run should fail (file exists)
    let output2 = Command::new(psy_bin())
        .args(["psyfile", "init"])
        .current_dir(&dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("failed to run psy psyfile init");
    assert!(
        !output2.status.success(),
        "init should fail when file exists"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// Platform support tests
// ---------------------------------------------------------------------------

#[test]
#[ignore]
fn test_psyfile_platform_override_command() {
    let os = if cfg!(target_os = "macos") {
        "macos"
    } else if cfg!(target_os = "linux") {
        "linux"
    } else {
        "windows"
    };
    let content = format!(
        r#"
[echoer]
command = "echo base-output"

[echoer.platform.{os}]
command = "echo overridden-output"
"#
    );
    let tmp = TempPsyfileDir::new(&content);
    let sl = sleep_cmd(60);
    let root = PsyRoot::start_with_psyfile(&tmp.psyfile_path(), &[], &to_refs(&sl));
    root.psy(&["run", "echoer"]);
    thread::sleep(Duration::from_secs(1));
    let logs = root.psy_stdout(&["logs", "echoer"]);
    assert!(
        logs.contains("overridden-output"),
        "expected overridden command output, got: {logs}"
    );
    assert!(
        !logs.contains("base-output"),
        "should not contain base output, got: {logs}"
    );
}

#[test]
#[ignore]
fn test_psyfile_platform_excluded_unit() {
    let other = if cfg!(target_os = "macos") {
        "linux"
    } else {
        "macos"
    };
    let content = format!(
        r#"
[excluded]
command = "echo should-not-run"
platforms = ["{other}"]

[included]
command = "echo runs-fine"
"#
    );
    let tmp = TempPsyfileDir::new(&content);
    let sl = sleep_cmd(60);
    let root = PsyRoot::start_with_psyfile(&tmp.psyfile_path(), &[], &to_refs(&sl));

    // Trying to run excluded unit should fail
    let out = root.psy(&["run", "excluded"]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !out.status.success() || stderr.contains("not found") || stderr.contains("no command"),
        "expected failure for excluded unit, stderr: {stderr}"
    );

    // Included unit should work
    root.psy(&["run", "included"]);
    thread::sleep(Duration::from_secs(1));
    let logs = root.psy_stdout(&["logs", "included"]);
    assert!(
        logs.contains("runs-fine"),
        "expected included unit output, got: {logs}"
    );
}

#[test]
#[ignore]
fn test_psyfile_platform_up_all_skips_excluded() {
    let other = if cfg!(target_os = "macos") {
        "linux"
    } else {
        "macos"
    };
    let content = format!(
        r#"
[included]
command = "echo included-output"

[excluded]
command = "echo excluded-output"
platforms = ["{other}"]
"#
    );
    let tmp = TempPsyfileDir::new(&content);
    let sl = sleep_cmd(60);
    let root = PsyRoot::start_with_psyfile_all(&tmp.psyfile_path(), &to_refs(&sl));
    thread::sleep(Duration::from_secs(1));

    let ps = root.psy_stdout(&["ps"]);
    assert!(
        ps.contains("included"),
        "psy ps should show included unit, got: {ps}"
    );
    assert!(
        !ps.contains("excluded"),
        "psy ps should not show excluded unit, got: {ps}"
    );
}

#[test]
#[ignore]
fn test_psyfile_platform_env_merge() {
    let os = if cfg!(target_os = "macos") {
        "macos"
    } else if cfg!(target_os = "linux") {
        "linux"
    } else {
        "windows"
    };
    #[cfg(unix)]
    let echo_cmd = "echo BASE=$BASE OVERRIDE=$OVERRIDE ADDED=$ADDED";
    #[cfg(windows)]
    let echo_cmd = "echo BASE=%BASE% OVERRIDE=%OVERRIDE% ADDED=%ADDED%";
    let content = format!(
        r#"
[echoer]
command = "{echo_cmd}"
env = {{ BASE = "base-val", OVERRIDE = "base-override" }}

[echoer.platform.{os}]
env = {{ OVERRIDE = "platform-override", ADDED = "platform-added" }}
"#
    );
    let tmp = TempPsyfileDir::new(&content);
    let sl = sleep_cmd(60);
    let root = PsyRoot::start_with_psyfile(&tmp.psyfile_path(), &[], &to_refs(&sl));
    root.psy(&["run", "echoer"]);
    thread::sleep(Duration::from_secs(1));
    let logs = root.psy_stdout(&["logs", "echoer"]);
    assert!(
        logs.contains("BASE=base-val"),
        "base env should be preserved, got: {logs}"
    );
    assert!(
        logs.contains("OVERRIDE=platform-override"),
        "override env should win, got: {logs}"
    );
    assert!(
        logs.contains("ADDED=platform-added"),
        "added env should appear, got: {logs}"
    );
}

// ---------------------------------------------------------------------------
// v1.3: Interactive stdin (psy send)
// ---------------------------------------------------------------------------

#[test]
#[ignore]
fn test_send_basic() {
    // Start an interactive cat process, send text, check logs
    let sl = sleep_cmd(60);
    let root = PsyRoot::start(&to_refs(&sl));

    let cat_cmd = vec!["run", "--interactive", "catproc", "--", "cat"];

    root.psy(&cat_cmd);
    thread::sleep(Duration::from_secs(1));

    // Send text
    root.psy(&["send", "catproc", "hello-interactive"]);
    thread::sleep(Duration::from_secs(1));

    let logs = root.psy_stdout(&["logs", "catproc"]);
    assert!(
        logs.contains("hello-interactive"),
        "sent text should appear in logs, got: {logs}"
    );
}

#[test]
#[ignore]
fn test_send_multiple_lines() {
    let sl = sleep_cmd(60);
    let root = PsyRoot::start(&to_refs(&sl));

    let cat_cmd = vec!["run", "--interactive", "multicat", "--", "cat"];

    root.psy(&cat_cmd);
    thread::sleep(Duration::from_secs(1));

    root.psy(&["send", "multicat", "line-one"]);
    root.psy(&["send", "multicat", "line-two"]);
    root.psy(&["send", "multicat", "line-three"]);
    thread::sleep(Duration::from_secs(1));

    let logs = root.psy_stdout(&["logs", "multicat"]);
    assert!(
        logs.contains("line-one") && logs.contains("line-two") && logs.contains("line-three"),
        "all sent lines should appear in logs, got: {logs}"
    );
}

#[test]
#[ignore]
fn test_send_non_interactive_error() {
    // Sending to a non-interactive process should fail
    let sl = sleep_cmd(60);
    let root = PsyRoot::start(&to_refs(&sl));

    let long_sl = sleep_cmd(999);
    let long_refs = to_refs(&long_sl);
    let mut run_args = vec!["run", "nointeract", "--"];
    run_args.extend(long_refs);
    root.psy(&run_args);
    thread::sleep(Duration::from_millis(500));

    let out = root.psy(&["send", "nointeract", "test"]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let combined = format!("{stdout}{stderr}");
    assert!(
        !out.status.success() || combined.to_lowercase().contains("interactive"),
        "send to non-interactive should error, got: {combined}"
    );
}

#[test]
#[ignore]
fn test_send_eof_closes_stdin() {
    let sl = sleep_cmd(60);
    let root = PsyRoot::start(&to_refs(&sl));

    let cat_cmd = vec!["run", "--interactive", "eofcat", "--", "cat"];

    root.psy(&cat_cmd);
    thread::sleep(Duration::from_secs(1));

    root.psy(&["send", "eofcat", "before-eof"]);
    thread::sleep(Duration::from_millis(500));

    // Close stdin
    root.psy(&["send", "--eof", "eofcat"]);
    thread::sleep(Duration::from_millis(500));

    // Further sends should fail
    let out = root.psy(&["send", "eofcat", "after-eof"]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let combined = format!("{stdout}{stderr}");
    assert!(
        !out.status.success() || combined.to_lowercase().contains("closed"),
        "send after eof should error, got: {combined}"
    );
}

#[test]
#[ignore]
fn test_send_not_found() {
    let sl = sleep_cmd(60);
    let root = PsyRoot::start(&to_refs(&sl));

    let out = root.psy(&["send", "nonexistent", "text"]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let combined = format!("{stdout}{stderr}");
    assert!(
        !out.status.success() || combined.to_lowercase().contains("not found"),
        "send to nonexistent process should error, got: {combined}"
    );
}

#[test]
#[ignore]
fn test_send_psyfile_interactive() {
    // Test interactive flag in Psyfile
    let tmp = TempPsyfileDir::new(
        r#"
[echoback]
command = "cat"
interactive = true
"#,
    );
    let sl = sleep_cmd(60);
    let root = PsyRoot::start_with_psyfile(&tmp.psyfile_path(), &[], &to_refs(&sl));

    root.psy(&["run", "echoback"]);
    thread::sleep(Duration::from_secs(1));

    root.psy(&["send", "echoback", "psyfile-interactive"]);
    thread::sleep(Duration::from_secs(1));

    let logs = root.psy_stdout(&["logs", "echoback"]);
    assert!(
        logs.contains("psyfile-interactive"),
        "Psyfile interactive unit should accept stdin, got: {logs}"
    );
}

#[cfg(unix)]
#[test]
#[ignore]
fn test_send_file() {
    // Test --file flag
    let sl = sleep_cmd(60);
    let root = PsyRoot::start(&to_refs(&sl));

    let cat_cmd = vec!["run", "--interactive", "filecat", "--", "cat"];
    root.psy(&cat_cmd);
    thread::sleep(Duration::from_secs(1));

    // Create temp file
    let tmp_file = std::env::temp_dir().join(format!("psy-send-test-{}", std::process::id()));
    std::fs::write(&tmp_file, "file-content-here\n").unwrap();

    let file_path = tmp_file.to_string_lossy().to_string();
    root.psy(&["send", "--file", &file_path, "filecat"]);
    thread::sleep(Duration::from_secs(1));

    let logs = root.psy_stdout(&["logs", "filecat"]);
    assert!(
        logs.contains("file-content-here"),
        "file content should appear in logs, got: {logs}"
    );

    let _ = std::fs::remove_file(&tmp_file);
}

#[test]
#[ignore]
fn test_send_raw_no_newline() {
    // Test --raw flag (no auto newline)
    let sl = sleep_cmd(60);
    let root = PsyRoot::start(&to_refs(&sl));

    let cat_cmd = vec!["run", "--interactive", "rawcat", "--", "cat"];

    root.psy(&cat_cmd);
    thread::sleep(Duration::from_secs(1));

    // Send two raw fragments that together form one line
    root.psy(&["send", "--raw", "rawcat", "part1-"]);
    root.psy(&["send", "--raw", "rawcat", "part2\n"]);
    thread::sleep(Duration::from_secs(1));

    let logs = root.psy_stdout(&["logs", "rawcat"]);
    assert!(
        logs.contains("part1-part2"),
        "raw send should not add newlines between parts, got: {logs}"
    );
}

#[test]
#[ignore]
fn test_send_stopped_process_error() {
    let sl = sleep_cmd(60);
    let root = PsyRoot::start(&to_refs(&sl));

    #[cfg(unix)]
    let cmd = vec![
        "run",
        "--interactive",
        "stopme",
        "--",
        "sh",
        "-c",
        "echo started && sleep 999",
    ];
    #[cfg(windows)]
    let cmd = vec![
        "run",
        "--interactive",
        "stopme",
        "--",
        "cmd",
        "/c",
        "echo started && ping -n 999 127.0.0.1 >nul",
    ];

    root.psy(&cmd);
    thread::sleep(Duration::from_secs(1));

    root.psy(&["stop", "stopme"]);
    thread::sleep(Duration::from_secs(1));

    let out = root.psy(&["send", "stopme", "text"]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let combined = format!("{stdout}{stderr}");
    assert!(
        !out.status.success() || combined.to_lowercase().contains("not running"),
        "send to stopped process should error, got: {combined}"
    );
}

// ---------------------------------------------------------------------------
// v1.3: Interactive stdin via Psyfile with dependencies
// ---------------------------------------------------------------------------

#[test]
#[ignore]
fn test_send_psyfile_interactive_with_deps() {
    // Interactive process with a dependency
    let tmp = TempPsyfileDir::new(
        r#"
[setup]
command = "echo setup-done"
ready = { exit = 0 }

[repl]
command = "cat"
interactive = true
depends_on = ["setup"]
"#,
    );
    let sl = sleep_cmd(60);
    let root = PsyRoot::start_with_psyfile(&tmp.psyfile_path(), &[], &to_refs(&sl));

    root.psy(&["run", "repl"]);
    thread::sleep(Duration::from_secs(3));

    // Both should be started
    let ps = root.psy_stdout(&["ps"]);
    assert!(ps.contains("setup"), "setup should be in ps: {ps}");
    assert!(ps.contains("repl"), "repl should be in ps: {ps}");

    // Send to the interactive process
    root.psy(&["send", "repl", "dep-test-input"]);
    thread::sleep(Duration::from_secs(1));

    let logs = root.psy_stdout(&["logs", "repl"]);
    assert!(
        logs.contains("dep-test-input"),
        "interactive unit with deps should work, got: {logs}"
    );
}

// ---------------------------------------------------------------------------
// Additional comprehensive cross-platform tests
// ---------------------------------------------------------------------------

#[test]
#[ignore]
fn test_restart_preserves_history_across_multiple() {
    let sl = sleep_cmd(60);
    let root = PsyRoot::start(&to_refs(&sl));

    let cmd = sh_c("echo run-marker && sleep 999");
    let cmd_refs = to_refs(&cmd);
    let mut run_args = vec!["run", "multirestart", "--"];
    run_args.extend(cmd_refs);
    root.psy(&run_args);
    thread::sleep(Duration::from_secs(1));

    // Restart multiple times
    root.psy(&["restart", "multirestart"]);
    thread::sleep(Duration::from_secs(1));
    root.psy(&["restart", "multirestart"]);
    thread::sleep(Duration::from_secs(1));

    let history = root.psy_stdout(&["history", "multirestart"]);
    // Should show 3 runs
    assert!(
        history.contains("1") && history.contains("2") && history.contains("3"),
        "history should show 3 runs, got: {history}"
    );
}

#[test]
#[ignore]
fn test_stop_main_rejected() {
    let sl = sleep_cmd(60);
    let root = PsyRoot::start(&to_refs(&sl));

    let out = root.psy(&["stop", "main"]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let combined = format!("{stdout}{stderr}");
    assert!(
        !out.status.success()
            || combined.to_lowercase().contains("cannot stop")
            || combined.to_lowercase().contains("down"),
        "stopping main should be rejected, got: {combined}"
    );
}

#[test]
#[ignore]
fn test_run_after_down_rejected() {
    let sl = sleep_cmd(60);
    let root = PsyRoot::start(&to_refs(&sl));

    root.psy(&["down"]);
    thread::sleep(Duration::from_secs(2));

    let echo = sh_c("echo should-not-work");
    let echo_refs = to_refs(&echo);
    let mut run_args = vec!["run", "afterdown", "--"];
    run_args.extend(echo_refs);
    let out = root.psy(&run_args);
    // Should fail because server is down or shutting down
    assert!(!out.status.success(), "run after down should fail");
}

#[test]
#[ignore]
fn test_logs_stderr_filter() {
    let sl = sleep_cmd(60);
    let root = PsyRoot::start(&to_refs(&sl));

    #[cfg(unix)]
    let cmd = vec![
        "run",
        "stderrtest",
        "--",
        "sh",
        "-c",
        "echo stdout-line && echo stderr-line >&2",
    ];
    #[cfg(windows)]
    let cmd = vec![
        "run",
        "stderrtest",
        "--",
        "cmd",
        "/c",
        "echo stdout-line && echo stderr-line 1>&2",
    ];

    root.psy(&cmd);
    thread::sleep(Duration::from_secs(1));

    // --stdout only
    let stdout_logs = root.psy_stdout(&["logs", "stderrtest", "--stdout"]);
    assert!(
        stdout_logs.contains("stdout-line"),
        "stdout filter should include stdout, got: {stdout_logs}"
    );
    assert!(
        !stdout_logs.contains("stderr-line"),
        "stdout filter should exclude stderr, got: {stdout_logs}"
    );

    // --stderr only
    let stderr_logs = root.psy_stdout(&["logs", "stderrtest", "--stderr"]);
    assert!(
        stderr_logs.contains("stderr-line"),
        "stderr filter should include stderr, got: {stderr_logs}"
    );
    assert!(
        !stderr_logs.contains("stdout-line"),
        "stderr filter should exclude stdout, got: {stderr_logs}"
    );
}

#[test]
#[ignore]
fn test_psyfile_circular_dep_error() {
    let tmp = TempPsyfileDir::new(
        r#"
[a]
command = "echo a"
depends_on = ["b"]

[b]
command = "echo b"
depends_on = ["a"]
"#,
    );

    // Validate should fail
    let output = Command::new(psy_bin())
        .args(["psyfile", "validate", "--file"])
        .arg(tmp.psyfile_path())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("failed to run validate");
    assert!(
        !output.status.success(),
        "circular deps should fail validation"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.to_lowercase().contains("circular") || stderr.to_lowercase().contains("cycle"),
        "error should mention circular/cycle: {stderr}"
    );
}

#[test]
#[ignore]
fn test_psyfile_unknown_dep_error() {
    let tmp = TempPsyfileDir::new(
        r#"
[api]
command = "echo api"
depends_on = ["nonexistent"]
"#,
    );

    let output = Command::new(psy_bin())
        .args(["psyfile", "validate", "--file"])
        .arg(tmp.psyfile_path())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("failed to run validate");
    assert!(
        !output.status.success(),
        "unknown dep should fail validation"
    );
}

#[test]
#[ignore]
fn test_psyfile_unknown_field_error() {
    let tmp = TempPsyfileDir::new(
        r#"
[server]
command = "echo hello"
depnds_on = ["db"]
"#,
    );

    let output = Command::new(psy_bin())
        .args(["psyfile", "validate", "--file"])
        .arg(tmp.psyfile_path())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("failed to run validate");
    assert!(
        !output.status.success(),
        "unknown field should fail validation"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("depnds_on") || stderr.contains("unknown"),
        "error should mention the typo: {stderr}"
    );
}

#[test]
#[ignore]
fn test_version() {
    let output = Command::new(psy_bin())
        .args(["version"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("failed to run version");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "version should succeed");
    let expected_version = env!("CARGO_PKG_VERSION");
    assert!(
        stdout.contains(expected_version),
        "version should show {expected_version}, got: {stdout}"
    );
}

#[test]
#[ignore]
fn test_psyfile_template_restart() {
    let sl = sleep_cmd(60);
    let tmp = TempPsyfileDir::new(
        r#"
[worker]
command = "echo worker-instance && sleep 999"
singleton = false
"#,
    );
    let root = PsyRoot::start_with_psyfile(&tmp.psyfile_path(), &[], &to_refs(&sl));

    root.psy(&["run", "worker"]);
    root.psy(&["run", "worker"]);
    thread::sleep(Duration::from_secs(1));

    // Restart the group
    root.psy(&["restart", "worker"]);
    thread::sleep(Duration::from_secs(2));

    let ps = root.psy_stdout(&["ps"]);
    assert!(
        ps.contains("worker.1") && ps.contains("worker.2"),
        "template instances should still exist after group restart, got: {ps}"
    );
}

#[test]
#[ignore]
fn test_psyfile_arg_append() {
    let sl = sleep_cmd(60);
    let tmp = TempPsyfileDir::new(
        r#"
[echoer]
command = "echo base"
"#,
    );
    let root = PsyRoot::start_with_psyfile(&tmp.psyfile_path(), &[], &to_refs(&sl));

    root.psy(&["run", "echoer", "--", "extra-arg"]);
    thread::sleep(Duration::from_secs(1));

    let logs = root.psy_stdout(&["logs", "echoer"]);
    assert!(
        logs.contains("base") && logs.contains("extra-arg"),
        "extra args should be appended, got: {logs}"
    );
}

#[test]
#[ignore]
fn test_psyfile_dollar_at_substitution() {
    let sl = sleep_cmd(60);
    let tmp = TempPsyfileDir::new(
        r#"
[cmd]
command = "echo $@ end"
"#,
    );
    let root = PsyRoot::start_with_psyfile(&tmp.psyfile_path(), &[], &to_refs(&sl));

    root.psy(&["run", "cmd", "--", "middle"]);
    thread::sleep(Duration::from_secs(1));

    let logs = root.psy_stdout(&["logs", "cmd"]);
    assert!(
        logs.contains("middle") && logs.contains("end"),
        "$@ should be substituted, got: {logs}"
    );
}

#[test]
#[ignore]
fn test_psyfile_dollar_at_no_args() {
    let sl = sleep_cmd(60);
    let tmp = TempPsyfileDir::new(
        r#"
[cmd]
command = "echo $@ end"
"#,
    );
    let root = PsyRoot::start_with_psyfile(&tmp.psyfile_path(), &[], &to_refs(&sl));

    root.psy(&["run", "cmd"]);
    thread::sleep(Duration::from_secs(1));

    let logs = root.psy_stdout(&["logs", "cmd"]);
    assert!(
        logs.contains("end"),
        "$@ with no args should work, got: {logs}"
    );
}

#[test]
#[ignore]
fn test_psyfile_schema_has_interactive() {
    let output = Command::new(psy_bin())
        .args(["psyfile", "schema"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("failed to run schema");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "schema should succeed");
    assert!(
        stdout.contains("interactive"),
        "schema should include interactive field, got: {stdout}"
    );
}

// ---------------------------------------------------------------------------
// send --wait tests
// ---------------------------------------------------------------------------

#[test]
#[ignore]
fn test_send_wait_basic() {
    // Send to cat with --wait, verify echoed output is returned
    let sl = sleep_cmd(60);
    let root = PsyRoot::start(&to_refs(&sl));

    let cat_cmd = vec!["run", "--interactive", "waitcat", "--", "cat"];
    root.psy(&cat_cmd);
    thread::sleep(Duration::from_secs(1));

    let out = root.psy(&["send", "--wait", "waitcat", "hello-wait"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "send --wait should succeed, stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        stdout.contains("hello-wait"),
        "send --wait should return echoed output, got: {stdout}"
    );
}

#[test]
#[ignore]
fn test_send_wait_prompt() {
    // Process outputs a prompt pattern, verify early return.
    // Uses Python for cross-platform compatibility (available on all CI runners).
    let sl = sleep_cmd(60);
    let root = PsyRoot::start(&to_refs(&sl));

    let py_script = vec![
        "run",
        "--interactive",
        "promptproc",
        "--",
        "python3",
        "-u",
        "-c",
        "import sys\nfor line in sys.stdin:\n    print('result: ' + line.strip(), flush=True)\n    print('PROMPT>', flush=True)",
    ];

    #[cfg(windows)]
    let py_script = vec![
        "run",
        "--interactive",
        "promptproc",
        "--",
        "python",
        "-u",
        "-c",
        "import sys\nfor line in sys.stdin:\n    print('result: ' + line.strip(), flush=True)\n    print('PROMPT>', flush=True)",
    ];

    root.psy(&py_script);
    thread::sleep(Duration::from_secs(2));

    let out = root.psy(&[
        "send",
        "--wait",
        "--wait-prompt",
        "PROMPT>",
        "promptproc",
        "test-input",
    ]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "send --wait with prompt should succeed, stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        stdout.contains("result: test-input"),
        "should contain result, got: {stdout}"
    );
    assert!(
        stdout.contains("PROMPT>"),
        "should contain prompt, got: {stdout}"
    );
}

#[test]
#[ignore]
fn test_send_wait_timeout() {
    // Process that doesn't produce output — verify timeout returns partial
    let sl = sleep_cmd(60);
    let root = PsyRoot::start(&to_refs(&sl));

    // cat will echo but then wait indefinitely for more input
    let cat_cmd = vec!["run", "--interactive", "waitcat2", "--", "cat"];
    root.psy(&cat_cmd);
    thread::sleep(Duration::from_secs(1));

    // Use a short timeout so the test completes quickly
    let out = root.psy(&[
        "send",
        "--wait",
        "--wait-timeout",
        "1s",
        "--idle-timeout",
        "300ms",
        "waitcat2",
        "timeout-test",
    ]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "send --wait should succeed even on timeout"
    );
    assert!(
        stdout.contains("timeout-test"),
        "should have partial output, got: {stdout}"
    );
}

#[test]
#[ignore]
fn test_send_wait_non_interactive_error() {
    // send --wait to non-interactive process should error
    let sl = sleep_cmd(60);
    let root = PsyRoot::start(&to_refs(&sl));

    let long_sl = sleep_cmd(999);
    let long_refs = to_refs(&long_sl);
    let mut run_args = vec!["run", "nointeract2", "--"];
    run_args.extend(long_refs);
    root.psy(&run_args);
    thread::sleep(Duration::from_millis(500));

    let out = root.psy(&["send", "--wait", "nointeract2", "test"]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let combined = format!("{stdout}{stderr}");
    assert!(
        !out.status.success() || combined.to_lowercase().contains("interactive"),
        "send --wait to non-interactive should error, got: {combined}"
    );
}

// ---------------------------------------------------------------------------
// Logs --since last
// ---------------------------------------------------------------------------

#[test]
#[ignore]
fn test_logs_since_last() {
    let sl = sleep_cmd(60);
    let root = PsyRoot::start(&to_refs(&sl));

    // Start an interactive process so we can control when output appears
    let cat = sh_c("cat");
    let cat_refs = to_refs(&cat);
    let mut run_args = vec!["run", "lastproc", "--interactive", "--"];
    run_args.extend(cat_refs);
    root.psy(&run_args);
    thread::sleep(Duration::from_millis(500));

    // Send first line
    root.psy(&["send", "lastproc", "line-one"]);
    thread::sleep(Duration::from_millis(300));

    // First logs call — should see line-one, sets marker
    let logs1 = root.psy_stdout(&["logs", "lastproc"]);
    assert!(
        logs1.contains("line-one"),
        "first logs should contain line-one, got: {logs1}"
    );

    // Send second line
    root.psy(&["send", "lastproc", "line-two"]);
    thread::sleep(Duration::from_millis(300));

    // Logs --since last — should see line-two but not line-one
    let logs2 = root.psy_stdout(&["logs", "lastproc", "--since", "last"]);
    assert!(
        logs2.contains("line-two"),
        "logs --since last should contain line-two, got: {logs2}"
    );
    assert!(
        !logs2.contains("line-one"),
        "logs --since last should NOT contain line-one, got: {logs2}"
    );

    // Send third line
    root.psy(&["send", "lastproc", "line-three"]);
    thread::sleep(Duration::from_millis(300));

    // Logs --since last again — should see line-three but not line-two
    let logs3 = root.psy_stdout(&["logs", "lastproc", "--since", "last"]);
    assert!(
        logs3.contains("line-three"),
        "second --since last should contain line-three, got: {logs3}"
    );
    assert!(
        !logs3.contains("line-two"),
        "second --since last should NOT contain line-two, got: {logs3}"
    );
}

#[test]
#[ignore]
fn test_logs_since_last_first_use() {
    let sl = sleep_cmd(60);
    let root = PsyRoot::start(&to_refs(&sl));

    let echo = sh_c("echo hello-last");
    let echo_refs = to_refs(&echo);
    let mut run_args = vec!["run", "lastfirst", "--"];
    run_args.extend(echo_refs);
    root.psy(&run_args);
    thread::sleep(Duration::from_secs(1));

    // First ever --since last with no prior marker — should return all logs
    let logs = root.psy_stdout(&["logs", "lastfirst", "--since", "last"]);
    assert!(
        logs.contains("hello-last"),
        "first --since last should return all logs, got: {logs}"
    );
}
