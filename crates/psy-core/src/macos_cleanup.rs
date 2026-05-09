//! macOS cleanup sidecar.
//!
//! On macOS there is no `PR_SET_CHILD_SUBREAPER` / `PR_SET_PDEATHSIG`
//! equivalent and the existing in-process pipe-trick watchdog cannot run
//! after the parent gets SIGKILL'd. To guarantee that children of a psy
//! root die when the root is hard-killed, each root spawns a tiny sidecar
//! process at startup. The sidecar watches the parent psy via kqueue
//! `NOTE_EXIT` and receives child PIDs over a pipe as they're spawned.
//! When the parent dies (or the pipe EOFs because all of psy's FDs closed),
//! the sidecar sends `SIGKILL` to every PID it has been told about and exits.
//!
//! The PID stream is append-only: psy never tells the sidecar that a PID
//! has gone away. Killing an already-dead PID is harmless (`kill` returns
//! `ESRCH`), so we keep the protocol minimal.
//!
//! ## Embedded-mode dispatch
//!
//! When psy-core is embedded inside a host binary (e.g. a Tauri app), the
//! sidecar can't be a separate `psy` binary because the host doesn't ship
//! one. Instead, the sidecar is a re-dispatched copy of the host binary
//! itself. The host signals its readiness to act as a sidecar by calling
//! [`dispatch_macos_cleanup_if_invoked`] at the very top of `main()` — when
//! the host's own argv matches the configured sentinel, that call runs
//! the sidecar's kqueue loop and `exit(0)`s the process. Otherwise it
//! returns immediately and `main()` continues.
//!
//! The strategy psy uses to spawn the sidecar — re-dispatch the current
//! binary, run an external binary, or disable cleanup entirely — is
//! configured via [`SidecarStrategy`]. On non-macOS targets the strategy
//! is accepted but ignored (Linux uses subreaper + PDEATHSIG; Windows uses
//! Job Objects).

#[cfg(target_os = "macos")]
use std::collections::HashSet;
#[cfg(target_os = "macos")]
use std::io::{BufRead, Write};
#[cfg(target_os = "macos")]
use std::os::fd::{FromRawFd, RawFd};
#[cfg(target_os = "macos")]
use std::process::Child;
#[cfg(target_os = "macos")]
use std::sync::{Arc, Mutex};
#[cfg(target_os = "macos")]
use std::thread;

use std::path::PathBuf;

// ---------------------------------------------------------------------------
// Public API: sentinels, strategy, dispatch
// ---------------------------------------------------------------------------

/// Default argv sentinel used to identify a sidecar invocation. Chosen to
/// be distinctive enough to never collide with a host's own subcommand
/// surface. Hosts that need a different sentinel (e.g. for back-compat with
/// existing argv) supply their own via
/// [`SidecarStrategy::HostReDispatch::sentinel`] and pass the same to
/// [`dispatch_macos_cleanup_if_invoked_with_sentinel`].
pub const DEFAULT_SENTINEL: &str = "__psy_macos_cleanup_sidecar__";

/// Env var that carries the inherited pipe FD number to the sidecar.
pub const CLEANUP_FD_ENV: &str = "PSY_CLEANUP_FD";

/// How psy spawns the macOS cleanup sidecar. Cross-platform — on Linux and
/// Windows the strategy is accepted but ignored because cleanup is handled
/// in-kernel.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub enum SidecarStrategy {
    /// Re-dispatch the current binary (`std::env::current_exe()`) with the
    /// given argv prefix as the sentinel. The host **must** call
    /// [`dispatch_macos_cleanup_if_invoked`] (or the `_with_sentinel`
    /// variant) at the top of `main()` so the re-dispatched copy
    /// recognizes the sentinel and runs the sidecar logic.
    HostReDispatch { sentinel: Vec<String> },

    /// Spawn a separate binary at this path with the given argv prefix.
    /// Useful for hosts that want a minimal-attack-surface signed sidecar
    /// (e.g. shipping the standalone `psy-macos-cleanup-sidecar` shim
    /// alongside the host).
    ExternalBinary {
        path: PathBuf,
        sentinel: Vec<String>,
    },

    /// Don't run a sidecar. Hard-kill cleanup on macOS is lost; embedded
    /// hosts that don't care (e.g. short-lived test fixtures or hosts that
    /// install their own equivalent mechanism) can opt out.
    Disabled,
}

impl Default for SidecarStrategy {
    fn default() -> Self {
        Self::HostReDispatch {
            sentinel: vec![DEFAULT_SENTINEL.to_string()],
        }
    }
}

/// Library entry point for embedded hosts. Call this **as the very first
/// thing in your `main()`** — before any logging, config loading, or other
/// initialization. If the host's argv matches the default sidecar sentinel,
/// this runs the sidecar's kqueue loop and exits the process. Otherwise it
/// returns immediately and `main()` continues.
///
/// **Contract:** reads `std::env::args()` and `PSY_CLEANUP_FD`. Does **not**
/// open files, network connections, or do anything that would fail to clean
/// up if the host's `main()` continued. Calling this is safe even if the
/// host doesn't otherwise use psy-core. No-op on non-macOS targets.
pub fn dispatch_macos_cleanup_if_invoked() {
    dispatch_macos_cleanup_if_invoked_with_sentinel(&[DEFAULT_SENTINEL]);
}

/// As [`dispatch_macos_cleanup_if_invoked`], but with a caller-supplied
/// argv sentinel. Hosts that need to interoperate with a previously-shipped
/// sidecar (or have their own existing argv conventions) can use this to
/// keep both worlds working.
pub fn dispatch_macos_cleanup_if_invoked_with_sentinel(sentinel: &[&str]) {
    #[cfg(target_os = "macos")]
    {
        if sentinel.is_empty() {
            return;
        }
        let argv: Vec<String> = std::env::args().collect();
        // argv[0] is the program name; sentinel begins at argv[1].
        if argv.len() < 1 + sentinel.len() {
            return;
        }
        for (i, want) in sentinel.iter().enumerate() {
            if argv[i + 1] != *want {
                return;
            }
        }
        // Match. Parse --parent-pid <N>; missing or unparseable → exit(2).
        let parent_pid = parse_parent_pid_from_argv(&argv).unwrap_or_else(|| {
            eprintln!("psy macos-cleanup: missing or invalid --parent-pid");
            std::process::exit(2);
        });
        // Make `ps` output identifiable so operators can distinguish a
        // sidecar from the host binary it was re-dispatched from.
        unsafe { set_proc_title(parent_pid) };
        run_sidecar(parent_pid);
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = sentinel;
    }
}

#[cfg(target_os = "macos")]
fn parse_parent_pid_from_argv(argv: &[String]) -> Option<u32> {
    let mut iter = argv.iter().peekable();
    while let Some(arg) = iter.next() {
        if arg == "--parent-pid" {
            return iter.next().and_then(|v| v.parse::<u32>().ok());
        }
        if let Some(rest) = arg.strip_prefix("--parent-pid=") {
            return rest.parse::<u32>().ok();
        }
    }
    None
}

/// Overwrite the process's argv[0] in place so `ps` shows a recognizable
/// name. Uses Apple-private `_NSGetArgv` (the same API that `setproctitle`
/// libraries use; stable across macOS versions).
///
/// Safety: caller must ensure no other code is concurrently reading argv[0].
/// Called once at sidecar startup before any other thread is spawned.
#[cfg(target_os = "macos")]
unsafe fn set_proc_title(parent_pid: u32) {
    extern "C" {
        fn _NSGetArgv() -> *mut *mut *mut libc::c_char;
    }

    let argv_ptr_ptr = _NSGetArgv();
    if argv_ptr_ptr.is_null() {
        return;
    }
    let argv = *argv_ptr_ptr;
    if argv.is_null() {
        return;
    }
    let argv0 = *argv;
    if argv0.is_null() {
        return;
    }

    let title = format!("psy-cleanup-sidecar [parent={parent_pid}]");
    let bytes = title.as_bytes();
    let orig_len = libc::strlen(argv0);
    let copy_len = std::cmp::min(bytes.len(), orig_len);

    std::ptr::copy_nonoverlapping(bytes.as_ptr(), argv0 as *mut u8, copy_len);
    // Null-fill everything from copy_len through orig_len so `ps` doesn't
    // show stale bytes from the original argv tail.
    for i in copy_len..=orig_len {
        *argv0.add(i) = 0;
    }
}

// ---------------------------------------------------------------------------
// Sidecar body (macOS only)
// ---------------------------------------------------------------------------

/// Handle returned to psy after spawning the sidecar. Holds the write end of
/// the announce pipe and the sidecar's `Child` for supervision/respawn.
#[cfg(target_os = "macos")]
pub struct SidecarHandle {
    /// Write end of the pipe to the sidecar — append-only newline-delimited
    /// PIDs. Wrapped in a sync `Mutex` because writes are tiny and infrequent.
    pub pipe: Mutex<std::fs::File>,
    /// The sidecar's `Child`. Wrapped so the supervisor can `wait()` on it.
    pub child: Mutex<Child>,
}

#[cfg(target_os = "macos")]
impl SidecarHandle {
    /// Tell the sidecar about a newly spawned child PID. Best-effort: errors
    /// are silently dropped because the supervisor is responsible for
    /// detecting a dead sidecar and respawning.
    pub fn notify(&self, pid: u32) {
        if let Ok(mut p) = self.pipe.lock() {
            let _ = writeln!(*p, "{pid}");
            let _ = p.flush();
        }
    }
}

/// Spawn the macOS cleanup sidecar according to the given strategy.
///
/// Returns `Ok(None)` for [`SidecarStrategy::Disabled`].
///
/// Both ends of the announce pipe are set CLOEXEC so subsequent psy spawns
/// can't inherit them. A `pre_exec` hook clears CLOEXEC on the read end
/// *only* in the sidecar child so it survives exec.
#[cfg(target_os = "macos")]
pub fn spawn_sidecar(
    parent_pid: u32,
    strategy: &SidecarStrategy,
) -> std::io::Result<Option<SidecarHandle>> {
    use std::os::unix::process::CommandExt;

    let (binary, sentinel): (PathBuf, &[String]) = match strategy {
        SidecarStrategy::Disabled => return Ok(None),
        SidecarStrategy::HostReDispatch { sentinel } => (std::env::current_exe()?, sentinel),
        SidecarStrategy::ExternalBinary { path, sentinel } => (path.clone(), sentinel),
    };

    if sentinel.is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "sidecar sentinel must not be empty",
        ));
    }

    let mut fds: [libc::c_int; 2] = [0; 2];
    if unsafe { libc::pipe(fds.as_mut_ptr()) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    let read_fd_raw: RawFd = fds[0];
    let write_fd_raw: RawFd = fds[1];

    set_cloexec(read_fd_raw)?;
    set_cloexec(write_fd_raw)?;

    let mut cmd = std::process::Command::new(&binary);
    for token in sentinel {
        cmd.arg(token);
    }
    cmd.arg("--parent-pid")
        .arg(parent_pid.to_string())
        .env(CLEANUP_FD_ENV, read_fd_raw.to_string())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());

    let read_fd_for_hook = read_fd_raw;
    unsafe {
        cmd.pre_exec(move || {
            let flags = libc::fcntl(read_fd_for_hook, libc::F_GETFD);
            if flags < 0 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::fcntl(read_fd_for_hook, libc::F_SETFD, flags & !libc::FD_CLOEXEC) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }

    let child = cmd.spawn()?;

    unsafe {
        libc::close(read_fd_raw);
    }

    let pipe_file = unsafe { std::fs::File::from_raw_fd(write_fd_raw) };

    Ok(Some(SidecarHandle {
        pipe: Mutex::new(pipe_file),
        child: Mutex::new(child),
    }))
}

#[cfg(target_os = "macos")]
fn set_cloexec(fd: RawFd) -> std::io::Result<()> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if flags < 0 {
        return Err(std::io::Error::last_os_error());
    }
    if unsafe { libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// Sidecar entry point. Called from [`dispatch_macos_cleanup_if_invoked`]
/// after argv-sentinel matching. Never returns: exits with code 0 after
/// SIGKILLing tracked PIDs (or non-zero on setup failure).
#[cfg(target_os = "macos")]
fn run_sidecar(parent_pid: u32) -> ! {
    unsafe {
        // Detach from psy's session/process group so signals targeting that
        // group don't reach us.
        let _ = libc::setsid();
    }

    let fd_str = std::env::var(CLEANUP_FD_ENV).unwrap_or_else(|_| {
        eprintln!("psy macos-cleanup: {CLEANUP_FD_ENV} not set");
        std::process::exit(2);
    });
    let fd: RawFd = match fd_str.parse() {
        Ok(n) => n,
        Err(_) => {
            eprintln!("psy macos-cleanup: invalid {CLEANUP_FD_ENV}={fd_str}");
            std::process::exit(2);
        }
    };

    let pids: Arc<Mutex<HashSet<u32>>> = Arc::new(Mutex::new(HashSet::new()));

    {
        let pids = Arc::clone(&pids);
        thread::spawn(move || {
            let pipe = unsafe { std::fs::File::from_raw_fd(fd) };
            let reader = std::io::BufReader::new(pipe);
            for line in reader.lines().map_while(Result::ok) {
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                if let Ok(pid) = trimmed.parse::<u32>() {
                    if let Ok(mut set) = pids.lock() {
                        set.insert(pid);
                    }
                }
            }
            sigkill_all_and_exit(&pids);
        });
    }

    let kq = unsafe { libc::kqueue() };
    if kq < 0 {
        eprintln!(
            "psy macos-cleanup: kqueue failed: {}",
            std::io::Error::last_os_error()
        );
        std::process::exit(2);
    }

    let kev_in = libc::kevent {
        ident: parent_pid as libc::uintptr_t,
        filter: libc::EVFILT_PROC,
        flags: libc::EV_ADD | libc::EV_ENABLE | libc::EV_ONESHOT,
        fflags: libc::NOTE_EXIT,
        data: 0,
        udata: std::ptr::null_mut(),
    };
    let registered = unsafe {
        libc::kevent(
            kq,
            &kev_in as *const _,
            1,
            std::ptr::null_mut(),
            0,
            std::ptr::null(),
        )
    };
    if registered < 0 {
        eprintln!(
            "psy macos-cleanup: kevent register failed: {}",
            std::io::Error::last_os_error()
        );
        std::process::exit(2);
    }

    let mut kev_out = libc::kevent {
        ident: 0,
        filter: 0,
        flags: 0,
        fflags: 0,
        data: 0,
        udata: std::ptr::null_mut(),
    };
    let _ = unsafe {
        libc::kevent(
            kq,
            std::ptr::null(),
            0,
            &mut kev_out as *mut _,
            1,
            std::ptr::null(),
        )
    };

    sigkill_all_and_exit(&pids);
}

#[cfg(target_os = "macos")]
fn sigkill_all_and_exit(pids: &Arc<Mutex<HashSet<u32>>>) -> ! {
    let snapshot: Vec<u32> = pids
        .lock()
        .map(|s| s.iter().copied().collect())
        .unwrap_or_default();
    for pid in snapshot {
        unsafe {
            libc::kill(pid as i32, libc::SIGKILL);
        }
    }
    std::process::exit(0);
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    #[test]
    fn default_strategy_is_host_re_dispatch_with_default_sentinel() {
        match super::SidecarStrategy::default() {
            super::SidecarStrategy::HostReDispatch { sentinel } => {
                assert_eq!(sentinel, vec![super::DEFAULT_SENTINEL.to_string()]);
            }
            _ => panic!("default should be HostReDispatch"),
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn parse_parent_pid_handles_separated_form() {
        let argv = vec![
            "host".to_string(),
            "__psy_macos_cleanup_sidecar__".to_string(),
            "--parent-pid".to_string(),
            "12345".to_string(),
        ];
        assert_eq!(super::parse_parent_pid_from_argv(&argv), Some(12345));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn parse_parent_pid_handles_equals_form() {
        let argv = vec![
            "host".to_string(),
            "__psy_macos_cleanup_sidecar__".to_string(),
            "--parent-pid=987".to_string(),
        ];
        assert_eq!(super::parse_parent_pid_from_argv(&argv), Some(987));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn parse_parent_pid_returns_none_when_missing() {
        let argv = vec![
            "host".to_string(),
            "__psy_macos_cleanup_sidecar__".to_string(),
        ];
        assert_eq!(super::parse_parent_pid_from_argv(&argv), None);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn parse_parent_pid_returns_none_when_unparseable() {
        let argv = vec![
            "host".to_string(),
            "__psy_macos_cleanup_sidecar__".to_string(),
            "--parent-pid".to_string(),
            "not-a-number".to_string(),
        ];
        assert_eq!(super::parse_parent_pid_from_argv(&argv), None);
    }
}
