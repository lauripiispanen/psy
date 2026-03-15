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
pub(crate) fn primary_socket_dir(uid: unistd::Uid) -> PathBuf {
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
        if let Ok(pid) = stem.parse::<u32>() {
            if !is_pid_alive(pid) {
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

// ---------------------------------------------------------------------------
// PID ancestry & root discovery
// ---------------------------------------------------------------------------

/// Check whether a process with the given PID is still alive.
pub fn is_pid_alive(pid: u32) -> bool {
    signal::kill(Pid::from_raw(pid as i32), None).is_ok()
}

/// Return the directory for anchor files.
///
/// This is `<primary_socket_dir>/roots/`.
pub fn roots_dir() -> PathBuf {
    let uid = unistd::getuid();
    primary_socket_dir(uid).join("roots")
}

/// Build the PID ancestor chain from PID 1 (or init) down to `pid`.
///
/// Returns e.g. `[1, 423, 1500, pid]`. If the chain cannot be built
/// (permission denied, zombie, etc.) the chain may be shorter — it always
/// includes `pid` itself.
pub fn get_ancestor_chain(pid: u32) -> Vec<u32> {
    let mut chain = Vec::new();
    let mut current = pid;
    loop {
        chain.push(current);
        if current <= 1 {
            break;
        }
        match read_ppid(current) {
            Some(ppid) if ppid != current && ppid != 0 => current = ppid,
            _ => break,
        }
    }
    chain.reverse();
    chain
}

/// Read the parent PID of `pid`.
#[cfg(target_os = "linux")]
fn read_ppid(pid: u32) -> Option<u32> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // Format: "<pid> (<comm>) <state> <ppid> ..."
    // <comm> can contain spaces and parens, so find the LAST ')'.
    let after_comm = stat.rfind(')')? + 1;
    let remainder = stat[after_comm..].trim_start();
    // Fields after ')': state ppid ...
    let ppid_str = remainder.split_whitespace().nth(1)?;
    ppid_str.parse::<u32>().ok()
}

#[cfg(target_os = "macos")]
fn read_ppid(pid: u32) -> Option<u32> {
    // Use proc_pidinfo with PROC_PIDTBSDINFO to get ppid.
    // struct proc_bsdinfo has ppid at a known offset.

    // PROC_PIDTBSDINFO = 3, struct proc_bsdinfo size = 136 bytes
    const PROC_PIDTBSDINFO: libc::c_int = 3;
    let mut buf = [0u8; 136];

    extern "C" {
        fn proc_pidinfo(
            pid: libc::c_int,
            flavor: libc::c_int,
            arg: u64,
            buffer: *mut libc::c_void,
            buffersize: libc::c_int,
        ) -> libc::c_int;
    }

    let ret = unsafe {
        proc_pidinfo(
            pid as libc::c_int,
            PROC_PIDTBSDINFO,
            0,
            buf.as_mut_ptr() as *mut libc::c_void,
            buf.len() as libc::c_int,
        )
    };

    if ret <= 0 {
        return None;
    }

    // pbi_ppid is at offset 16 (after flags:4, status:4, xstatus:4, pid:4)
    let ppid = u32::from_ne_bytes([buf[16], buf[17], buf[18], buf[19]]);
    Some(ppid)
}

/// Compute the anchor file path and the actual socket path.
///
/// Returns `(anchor_path, socket_path)`. When the anchor filename is short
/// enough, the anchor file IS the Unix domain socket and both paths are the
/// same. When the chain is too long, the anchor is a regular file and the
/// socket lives at a shorter path.
pub fn anchor_socket_path(chain: &[u32]) -> (PathBuf, PathBuf) {
    let chain_filename = anchor_chain_filename(chain);
    let anchor = roots_dir().join(&chain_filename);

    if anchor.as_os_str().len() < UNIX_SOCK_MAX {
        // Direct mode: anchor IS the socket.
        (anchor.clone(), anchor)
    } else {
        // Indirect mode: anchor is a regular file, socket at a shorter path.
        let uid = unistd::getuid();
        let root_pid = chain.last().copied().unwrap_or(0);
        let sock = primary_socket_dir(uid)
            .join("s")
            .join(format!("{root_pid}.sock"));
        (anchor, sock)
    }
}

/// Build the anchor filename from a PID chain. E.g. `[1, 423, 5000]` → `"1-423-5000.sock"`.
pub fn anchor_chain_filename(chain: &[u32]) -> String {
    let parts: Vec<String> = chain.iter().map(|p| p.to_string()).collect();
    format!("{}.sock", parts.join("-"))
}

/// Parse a PID chain from an anchor filename.
///
/// E.g. `"1-423-5000.sock"` → `Some([1, 423, 5000])`.
pub fn parse_anchor_chain(filename: &str) -> Option<Vec<u32>> {
    let stem = filename.strip_suffix(".sock")?;
    let pids: Option<Vec<u32>> = stem.split('-').map(|s| s.parse::<u32>().ok()).collect();
    pids
}

/// Remove stale anchor files from the roots directory.
///
/// An anchor is stale if its root PID (last element in the chain) is no longer alive.
pub fn cleanup_stale_anchors() {
    let dir = roots_dir();
    let entries = match fs::read_dir(&dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name_str = match name.to_str() {
            Some(s) => s,
            None => continue,
        };
        if let Some(chain) = parse_anchor_chain(name_str) {
            if let Some(&root_pid) = chain.last() {
                if !is_pid_alive(root_pid) {
                    let _ = fs::remove_file(entry.path());
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_get_ancestor_chain_self() {
        let pid = std::process::id();
        let chain = get_ancestor_chain(pid);
        // Chain must include at least our own PID and end with it.
        assert!(!chain.is_empty(), "chain must not be empty");
        assert_eq!(*chain.last().unwrap(), pid, "chain must end with our PID");
        // On most systems the chain has at least 2 entries (parent + self),
        // but permission restrictions may shorten it. The chain must be
        // monotonically ordered (ancestors before descendants).
        assert!(chain.len() >= 2, "chain too short: {chain:?}");
    }

    #[test]
    fn test_get_ancestor_chain_init() {
        let chain = get_ancestor_chain(1);
        assert_eq!(chain, vec![1]);
    }

    #[test]
    fn test_is_pid_alive_self() {
        assert!(is_pid_alive(std::process::id()));
    }

    #[test]
    fn test_is_pid_alive_bogus() {
        // PID 4_000_000 is extremely unlikely to exist.
        assert!(!is_pid_alive(4_000_000));
    }

    #[test]
    fn test_anchor_chain_filename() {
        assert_eq!(anchor_chain_filename(&[1, 423, 5000]), "1-423-5000.sock");
        assert_eq!(anchor_chain_filename(&[1]), "1.sock");
    }

    #[test]
    fn test_parse_anchor_chain() {
        assert_eq!(
            parse_anchor_chain("1-423-5000.sock"),
            Some(vec![1, 423, 5000])
        );
        assert_eq!(parse_anchor_chain("1.sock"), Some(vec![1]));
        assert_eq!(parse_anchor_chain("not-a-number.sock"), None);
        assert_eq!(parse_anchor_chain("1-423-5000.pipe"), None); // wrong extension
        assert_eq!(parse_anchor_chain("1-423-5000"), None); // no extension
    }

    #[test]
    fn test_anchor_socket_path_direct() {
        // A short chain should produce a direct socket (anchor == socket).
        let chain = vec![1, 100, 200];
        let (anchor, socket) = anchor_socket_path(&chain);
        assert_eq!(anchor, socket, "short chain should be direct socket");
        assert!(anchor.to_string_lossy().contains("roots/"));
        assert!(anchor.to_string_lossy().ends_with(".sock"));
    }

    #[test]
    fn test_anchor_socket_path_indirect() {
        // Build a chain long enough to exceed UNIX_SOCK_MAX.
        let chain: Vec<u32> = (0..30).map(|i| 10000 + i).collect();
        let (anchor, socket) = anchor_socket_path(&chain);
        // The anchor path will exceed 104 bytes, so socket should be different.
        if anchor.as_os_str().len() >= UNIX_SOCK_MAX {
            assert_ne!(anchor, socket, "long chain should use indirect socket");
            assert!(socket.to_string_lossy().contains("/s/"));
        }
    }

    #[test]
    fn test_roots_dir() {
        let dir = roots_dir();
        assert!(dir.to_string_lossy().contains("roots"));
    }
}
