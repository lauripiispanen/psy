//! macOS cleanup sidecar.
//!
//! On macOS there is no `PR_SET_CHILD_SUBREAPER` / `PR_SET_PDEATHSIG`
//! equivalent and the existing in-process pipe-trick watchdog cannot run
//! after the parent gets SIGKILL'd. To guarantee that children of a psy
//! root die when the root is hard-killed, each root spawns a tiny sidecar
//! process at startup. The sidecar is just `psy --macos-cleanup` running in
//! a fresh session; it watches the parent psy via kqueue `NOTE_EXIT` and
//! receives child PIDs over a pipe as they're spawned. When the parent
//! dies (or the pipe EOFs because all of psy's FDs closed), the sidecar
//! sends SIGKILL to every PID it has been told about and exits.
//!
//! The PID stream is append-only: psy never tells the sidecar that a PID
//! has gone away. Killing an already-dead PID is harmless (kill returns
//! ESRCH), so we keep the protocol minimal.
//!
//! On Linux the kernel's subreaper + PDEATHSIG handle this; on Windows the
//! Job Object's `KILL_ON_JOB_CLOSE` does. This module is macOS-only.

use std::collections::HashSet;
use std::io::{BufRead, Write};
use std::os::fd::{FromRawFd, RawFd};
use std::process::Child;
use std::sync::{Arc, Mutex};
use std::thread;

/// Env var that carries the inherited pipe FD number to the sidecar.
pub const CLEANUP_FD_ENV: &str = "PSY_CLEANUP_FD";

/// Handle returned to psy after spawning the sidecar. Holds the write end of
/// the announce pipe and the sidecar's `Child` for supervision/respawn.
pub struct SidecarHandle {
    /// Write end of the pipe to the sidecar — append-only newline-delimited
    /// PIDs. Wrapped in a sync `Mutex` because writes are tiny and infrequent.
    pub pipe: Mutex<std::fs::File>,
    /// The sidecar's `Child`. Wrapped so the supervisor can `wait()` on it.
    pub child: Mutex<Child>,
}

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

/// Spawn the macOS cleanup sidecar (`psy macos-cleanup --parent-pid <pid>`).
///
/// Both ends of the announce pipe are set CLOEXEC so subsequent `psy run`
/// spawns can't inherit them. A `pre_exec` hook clears `CLOEXEC` on the
/// read end *only* in the sidecar child so it survives exec.
pub fn spawn_sidecar(parent_pid: u32) -> std::io::Result<SidecarHandle> {
    use std::os::unix::process::CommandExt;

    // Plain pipe (macOS has no `pipe2`). Both ends inherit by default.
    let mut fds: [libc::c_int; 2] = [0; 2];
    if unsafe { libc::pipe(fds.as_mut_ptr()) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    let read_fd_raw: RawFd = fds[0];
    let write_fd_raw: RawFd = fds[1];

    // Mark both ends CLOEXEC so unrelated child spawns don't inherit them.
    set_cloexec(read_fd_raw)?;
    set_cloexec(write_fd_raw)?;

    let psy_bin = std::env::current_exe()?;
    let mut cmd = std::process::Command::new(psy_bin);
    cmd.arg("macos-cleanup")
        .arg("--parent-pid")
        .arg(parent_pid.to_string())
        .env(CLEANUP_FD_ENV, read_fd_raw.to_string())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());

    let read_fd_for_hook = read_fd_raw;
    unsafe {
        cmd.pre_exec(move || {
            // Clear CLOEXEC on the read FD so the sidecar inherits it across
            // exec. The write FD stays CLOEXEC'd and is closed in the child.
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

    // Parent doesn't need the read end.
    unsafe {
        libc::close(read_fd_raw);
    }

    let pipe_file = unsafe { std::fs::File::from_raw_fd(write_fd_raw) };

    Ok(SidecarHandle {
        pipe: Mutex::new(pipe_file),
        child: Mutex::new(child),
    })
}

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

/// Entry point invoked by `psy --macos-cleanup --parent-pid <pid>`.
///
/// Never returns under normal operation: the sidecar exits with code 0
/// after delivering SIGKILLs, or exits with non-zero on setup failure.
pub fn run(parent_pid: u32) -> ! {
    // Detach from psy's session/process group so signals targeting that
    // group (Ctrl-C from a TTY, for example) don't reach us. We must still
    // be killable individually — that's fine, only the GROUP signal flow
    // changes.
    unsafe {
        // Safety: setsid is signal-safe; failure is benign here.
        let _ = libc::setsid();
    }

    // Recover the inherited pipe FD that carries child PIDs from psy.
    let fd_str = std::env::var(CLEANUP_FD_ENV).unwrap_or_else(|_| {
        eprintln!("psy --macos-cleanup: {CLEANUP_FD_ENV} not set");
        std::process::exit(2);
    });
    let fd: RawFd = match fd_str.parse() {
        Ok(n) => n,
        Err(_) => {
            eprintln!("psy --macos-cleanup: invalid {CLEANUP_FD_ENV}={fd_str}");
            std::process::exit(2);
        }
    };

    let pids: Arc<Mutex<HashSet<u32>>> = Arc::new(Mutex::new(HashSet::new()));

    // Reader thread: parses one PID per line and adds to the tracked set.
    // On EOF it triggers cleanup directly — that handles the case where
    // the parent crashed before we get the kqueue notification.
    {
        let pids = Arc::clone(&pids);
        thread::spawn(move || {
            // Safety: we own this fd for the life of the thread.
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
            // Pipe closed — psy is gone (or shutting down). Trigger cleanup
            // even though kqueue may also fire; the operation is idempotent.
            sigkill_all_and_exit(&pids);
        });
    }

    // Set up kqueue watch on the parent psy's PID.
    let kq = unsafe { libc::kqueue() };
    if kq < 0 {
        eprintln!(
            "psy --macos-cleanup: kqueue failed: {}",
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
            "psy --macos-cleanup: kevent register failed: {}",
            std::io::Error::last_os_error()
        );
        std::process::exit(2);
    }

    // Block until the parent process exits.
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

fn sigkill_all_and_exit(pids: &Arc<Mutex<HashSet<u32>>>) -> ! {
    let snapshot: Vec<u32> = pids
        .lock()
        .map(|s| s.iter().copied().collect())
        .unwrap_or_default();
    for pid in snapshot {
        // SIGKILL to each tracked PID. Already-dead PIDs return ESRCH which
        // we deliberately ignore — the protocol is append-only.
        unsafe {
            libc::kill(pid as i32, libc::SIGKILL);
        }
    }
    std::process::exit(0);
}
