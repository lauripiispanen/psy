use std::fs;
use std::io::{self, Read as _};
use std::os::unix::io::{FromRawFd, IntoRawFd, RawFd};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

use nix::sys::signal::{self, Signal};
use nix::unistd::{self, Pid};

/// Holds the read and write ends of the death pipe.
pub struct DeathPipe {
    pub read_fd: RawFd,
    pub write_fd: RawFd,
}

// ---------------------------------------------------------------------------
// Root process setup
// ---------------------------------------------------------------------------

/// Perform platform-specific root-process initialisation.
///
/// * **Linux** — set the calling process as a *child sub-reaper* so that
///   orphaned grandchildren are re-parented to us rather than PID 1.
/// * **macOS** — no kernel-level equivalent; orphan cleanup relies on the
///   death-pipe mechanism instead.
pub fn setup_root() {
    #[cfg(target_os = "linux")]
    {
        // PR_SET_CHILD_SUBREAPER = 36
        let ret = unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0) };
        if ret != 0 {
            eprintln!(
                "warning: prctl(PR_SET_CHILD_SUBREAPER) failed: {}",
                io::Error::last_os_error()
            );
        }
    }

    #[cfg(target_os = "macos")]
    {
        // No sub-reaper equivalent on macOS — the death-pipe handles cleanup.
    }
}

// ---------------------------------------------------------------------------
// Pre-exec hook (used with Command::pre_exec)
// ---------------------------------------------------------------------------

/// Return a closure suitable for [`std::process::Command::pre_exec`].
///
/// * **Linux** — calls `prctl(PR_SET_PDEATHSIG, SIGKILL)` so the child is
///   killed immediately if the parent dies.
/// * **macOS** — returns a no-op; the pipe-based watchdog covers this case.
pub fn pre_exec_hook() -> impl FnMut() -> io::Result<()> + Send + Sync + 'static {
    move || {
        #[cfg(target_os = "linux")]
        {
            let ret = unsafe { libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL, 0, 0, 0) };
            if ret != 0 {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Death pipe — parent-death detection for child processes
// ---------------------------------------------------------------------------

/// Create a pipe used to detect parent death.
///
/// Returns `(read_fd, write_fd)`. The root process keeps the write end open;
/// child processes inherit the read end and watch for EOF.
pub fn create_death_pipe() -> io::Result<DeathPipe> {
    let (read_fd, write_fd) = unistd::pipe().map_err(|e| io::Error::from_raw_os_error(e as i32))?;
    Ok(DeathPipe {
        read_fd: read_fd.into_raw_fd(),
        write_fd: write_fd.into_raw_fd(),
    })
}

/// Spawn a background thread that blocks on `read_fd`.
///
/// When the write end of the pipe is closed (parent death), read returns
/// EOF / 0 bytes and the thread calls `std::process::exit(1)`.
pub fn spawn_watchdog_thread(read_fd: RawFd) {
    thread::spawn(move || {
        // Safety: we own this fd for the lifetime of the thread.
        let mut file = unsafe { std::fs::File::from_raw_fd(read_fd) };
        let mut buf = [0u8; 1];
        loop {
            match file.read(&mut buf) {
                Ok(0) => {
                    // EOF — parent closed the write end (or exited).
                    eprintln!("psy: parent died, terminating");
                    std::process::exit(1);
                }
                Ok(_) => {
                    // Unexpected data — ignore and keep reading.
                    continue;
                }
                Err(ref e) if e.kind() == io::ErrorKind::Interrupted => {
                    continue;
                }
                Err(_) => {
                    // Pipe error — treat as parent death.
                    std::process::exit(1);
                }
            }
        }
    });
}

// ---------------------------------------------------------------------------
// Socket path
// ---------------------------------------------------------------------------

/// Maximum length of a Unix domain socket path (struct sockaddr_un.sun_path).
const UNIX_SOCK_MAX: usize = 104;

/// Return the filesystem path for the daemon socket for a given PID.
///
/// * **Linux** — prefers `$XDG_RUNTIME_DIR/psy/<pid>.sock`, falling back to
///   `/tmp/psy-<uid>/<pid>.sock`.
/// * **macOS** — always uses `/tmp/psy-<uid>/<pid>.sock`.
///
/// If the resulting path is too long for a Unix domain socket (>= 104 bytes)
/// we fall back to a shorter `/tmp/psy-<uid>/<pid>.sock` path.
pub fn socket_path(pid: u32) -> String {
    let uid = unistd::getuid();
    let candidate = primary_socket_dir(uid).join(format!("{pid}.sock"));

    if candidate.as_os_str().len() < UNIX_SOCK_MAX {
        candidate.to_string_lossy().to_string()
    } else {
        let fallback = format!("/tmp/psy-{uid}/{pid}.sock");
        if fallback.len() < UNIX_SOCK_MAX {
            fallback
        } else {
            format!("/tmp/p-{pid}.sock")
        }
    }
}

/// Return the preferred directory for sockets.
fn primary_socket_dir(uid: unistd::Uid) -> PathBuf {
    #[cfg(target_os = "linux")]
    {
        if let Ok(xdg) = std::env::var("XDG_RUNTIME_DIR") {
            return PathBuf::from(xdg).join("psy");
        }
    }
    PathBuf::from(format!("/tmp/psy-{uid}"))
}

// ---------------------------------------------------------------------------
// Stale socket cleanup
// ---------------------------------------------------------------------------

/// Remove `path` if the PID encoded in its filename is no longer alive.
///
/// The filename is expected to be `<pid>.sock`. If the PID is still running
/// the socket is left in place.
pub fn cleanup_stale_socket(path: &Path) -> io::Result<()> {
    if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
        if let Ok(pid) = stem.parse::<i32>() {
            // signal::kill with None is a no-op that merely checks whether the
            // process exists and we have permission to signal it.
            let alive = signal::kill(Pid::from_raw(pid), None).is_ok();
            if !alive {
                fs::remove_file(path)?;
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Graceful stop with timeout
// ---------------------------------------------------------------------------

/// Send SIGTERM to `pid`, wait up to `timeout`, then SIGKILL if still alive.
pub fn stop_process(pid: u32, timeout: Duration) {
    let nix_pid = Pid::from_raw(pid as i32);

    // Send SIGTERM.
    if signal::kill(nix_pid, Signal::SIGTERM).is_err() {
        // Process may already be dead — nothing more to do.
        return;
    }

    // Poll in small increments up to `timeout`.
    let step = Duration::from_millis(50);
    let mut elapsed = Duration::ZERO;
    while elapsed < timeout {
        thread::sleep(step);
        elapsed += step;
        // Check whether the process is still around.
        if signal::kill(nix_pid, None).is_err() {
            return; // Gone.
        }
    }

    // Still alive — escalate to SIGKILL.
    let _ = signal::kill(nix_pid, Signal::SIGKILL);
}
