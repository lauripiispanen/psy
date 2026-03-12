//! Integration tests for the psy binary.
//!
//! All tests are marked `#[ignore]` and should be run with:
//!     cargo test -- --ignored
//!
//! Each test starts a `psy up` root process and cleans it up on drop.

use std::path::PathBuf;
use std::process::{Child, Command, Output, Stdio};
use std::thread;
use std::time::Duration;

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
        thread::sleep(Duration::from_secs(1));

        // Build the expected socket path (mirrors platform::socket_path).
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
        format!("\\\\.\\pipe\\psy-{pid}")
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
    vec![
        "cmd".into(), "/c".into(), "timeout".into(),
        "/t".into(), secs.to_string(), "/nobreak".into(),
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
    assert!(
        !out.status.success(),
        "ps should fail after down"
    );

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
        !out.status.success() || combined.to_lowercase().contains("invalid")
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
    let print_env = vec!["run", "envchild", "--env", "MY_VAR=hello123", "--", "sh", "-c", "echo $MY_VAR"];
    #[cfg(windows)]
    let print_env = vec!["run", "envchild", "--env", "MY_VAR=hello123", "--", "cmd", "/c", "echo %MY_VAR%"];

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
        "run", "liner", "--", "sh", "-c",
        "for i in $(seq 1 100); do echo line-$i; done",
    ];
    #[cfg(windows)]
    let many_lines = vec![
        "run", "liner", "--", "cmd", "/c",
        "for /L %i in (1,1,100) do @echo line-%i",
    ];

    root.psy(&many_lines);
    thread::sleep(Duration::from_secs(2));

    let logs = root.psy_stdout(&["logs", "liner", "--tail", "5"]);
    // Output is JSON with a "lines" array. Count the "content" entries.
    let content_count = logs.matches("\"content\"").count();
    assert!(
        content_count <= 5,
        "tail 5 should return at most 5 content lines, got {content_count} in: {logs}"
    );
}
