//! Windows platform abstraction.
//!
//! Uses Win32 Job Objects for child cleanup and named pipes for IPC.

use std::io;
use std::sync::OnceLock;
use std::thread;
use std::time::Duration;

use windows::Win32::Foundation::{CloseHandle, BOOL, HANDLE};
use windows::Win32::Security::SECURITY_ATTRIBUTES;
use windows::Win32::System::Console::{GenerateConsoleCtrlEvent, CTRL_BREAK_EVENT};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32First, Process32Next, PROCESSENTRY32, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
    SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
};
use windows::Win32::System::Pipes::CreatePipe;
use windows::Win32::System::Threading::{
    GetCurrentProcess, GetProcessTimes, OpenProcess, TerminateProcess,
    PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE, PROCESS_TERMINATE,
};

// ---------------------------------------------------------------------------
// Death pipe
// ---------------------------------------------------------------------------

/// Holds the read and write ends of the death pipe (as raw pointer-sized values).
pub struct DeathPipe {
    pub read_handle: isize,
    pub write_handle: isize,
}

// ---------------------------------------------------------------------------
// Job Object (singleton)
// ---------------------------------------------------------------------------

static JOB_HANDLE: OnceLock<isize> = OnceLock::new();

pub fn setup_root() {
    let handle = unsafe { CreateJobObjectW(None, None).expect("CreateJobObjectW failed") };

    let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
    info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;

    unsafe {
        SetInformationJobObject(
            handle,
            JobObjectExtendedLimitInformation,
            &info as *const _ as *const _,
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        )
        .expect("SetInformationJobObject failed");
    }

    unsafe {
        let _ = AssignProcessToJobObject(handle, GetCurrentProcess());
    }

    JOB_HANDLE.set(handle.0 as isize).ok();
}

/// Add a child process to the root Job Object.
pub fn assign_to_job(process_handle: HANDLE) {
    if let Some(&raw) = JOB_HANDLE.get() {
        let job = HANDLE(raw as *mut _);
        unsafe {
            let _ = AssignProcessToJobObject(job, process_handle);
        }
    }
}

// ---------------------------------------------------------------------------
// Named pipe path
// ---------------------------------------------------------------------------

/// Named pipe path for IPC. Format: `\\.\pipe\psy-<pid>`
pub fn socket_path(pid: u32) -> String {
    format!(r"\\.\pipe\psy-{pid}")
}

pub fn cleanup_stale_socket(_path: &std::path::Path) -> io::Result<()> {
    // Named pipes are kernel objects — gone when the process exits.
    Ok(())
}

// ---------------------------------------------------------------------------
// Death pipe
// ---------------------------------------------------------------------------

pub fn create_death_pipe() -> io::Result<DeathPipe> {
    let mut read_handle = HANDLE::default();
    let mut write_handle = HANDLE::default();

    let sa = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        bInheritHandle: BOOL(1),
        lpSecurityDescriptor: std::ptr::null_mut(),
    };

    unsafe {
        CreatePipe(&mut read_handle, &mut write_handle, Some(&sa), 0)
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;
    }

    Ok(DeathPipe {
        read_handle: read_handle.0 as isize,
        write_handle: write_handle.0 as isize,
    })
}

pub fn spawn_watchdog_thread(_pipe: &DeathPipe) {
    // TODO: blocking ReadFile on the read handle; on EOF call exit(1).
    // For now the Job Object provides the cleanup guarantee.
}

// ---------------------------------------------------------------------------
// Graceful stop
// ---------------------------------------------------------------------------

pub fn stop_process(pid: u32, timeout: Duration) {
    unsafe {
        let _ = GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, pid);
    }

    let step = Duration::from_millis(100);
    let mut elapsed = Duration::ZERO;
    while elapsed < timeout {
        thread::sleep(step);
        elapsed += step;
        let alive = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, false, pid).is_ok() };
        if !alive {
            return;
        }
    }

    unsafe {
        if let Ok(h) = OpenProcess(PROCESS_TERMINATE, false, pid) {
            let _ = TerminateProcess(h, 1);
            let _ = CloseHandle(h);
        }
    }
}

// ---------------------------------------------------------------------------
// Pre-exec hook (no-op on Windows — Job Object handles cleanup)
// ---------------------------------------------------------------------------

pub fn pre_exec_hook() -> impl FnMut() -> io::Result<()> + Send + Sync + 'static {
    move || Ok(())
}

// ---------------------------------------------------------------------------
// PID ancestry & root discovery
// ---------------------------------------------------------------------------

use std::fs;
use std::path::PathBuf;

/// Check whether a process with the given PID is still alive.
pub fn is_pid_alive(pid: u32) -> bool {
    unsafe { OpenProcess(PROCESS_SYNCHRONIZE, false, pid).is_ok() }
}

/// Return the directory for anchor files.
///
/// Uses `%LOCALAPPDATA%\psy\roots\` with fallback to `%TEMP%\psy-roots\`.
pub fn roots_dir() -> PathBuf {
    if let Ok(local) = std::env::var("LOCALAPPDATA") {
        PathBuf::from(local).join("psy").join("roots")
    } else if let Ok(tmp) = std::env::var("TEMP") {
        PathBuf::from(tmp).join("psy-roots")
    } else {
        PathBuf::from(r"C:\Temp\psy-roots")
    }
}

/// Build the PID ancestor chain from the root of the tree down to `pid`.
///
/// Returns e.g. `[4, 423, 1500, pid]`. On Windows the top-level PID is
/// typically the System Idle Process (0) or System (4). We include whatever
/// the topmost reachable parent is.
///
/// To guard against PID reuse, each link in the chain is validated: the
/// parent's creation time must be earlier than the child's. If validation
/// fails, the chain is terminated at that point.
pub fn get_ancestor_chain(pid: u32) -> Vec<u32> {
    // Build a pid → ppid map from a process snapshot.
    let ppid_map = match build_ppid_map() {
        Some(m) => m,
        None => return vec![pid],
    };

    let mut chain = Vec::new();
    let mut current = pid;
    let mut visited = std::collections::HashSet::new();

    loop {
        if !visited.insert(current) {
            // Cycle detected (shouldn't happen, but be safe).
            break;
        }
        chain.push(current);

        if current == 0 {
            break;
        }

        match ppid_map.get(&current) {
            Some(&parent) if parent != current => {
                // Validate creation time ordering to guard against PID reuse.
                if !validate_parent_child_times(parent, current) {
                    break;
                }
                current = parent;
            }
            _ => break,
        }
    }

    chain.reverse();
    chain
}

/// Take a snapshot of all processes and build a pid → ppid map.
fn build_ppid_map() -> Option<std::collections::HashMap<u32, u32>> {
    use std::collections::HashMap;

    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0).ok()? };

    let mut map = HashMap::new();
    let mut entry = PROCESSENTRY32 {
        dwSize: std::mem::size_of::<PROCESSENTRY32>() as u32,
        ..Default::default()
    };

    unsafe {
        if Process32First(snapshot, &mut entry).is_ok() {
            loop {
                map.insert(entry.th32ProcessID, entry.th32ParentProcessID);
                if Process32Next(snapshot, &mut entry).is_err() {
                    break;
                }
            }
        }
        let _ = CloseHandle(snapshot);
    }

    Some(map)
}

/// Validate that the parent process was created before the child process.
///
/// Returns `true` if validation passes or if times cannot be retrieved
/// (we allow the link rather than breaking the chain on permission errors).
fn validate_parent_child_times(parent_pid: u32, child_pid: u32) -> bool {
    let parent_time = get_creation_time(parent_pid);
    let child_time = get_creation_time(child_pid);

    match (parent_time, child_time) {
        (Some(pt), Some(ct)) => pt <= ct,
        _ => true, // Can't determine — allow the link.
    }
}

/// Get the creation time of a process as a u64 (FILETIME as single value).
fn get_creation_time(pid: u32) -> Option<u64> {
    use windows::Win32::Foundation::FILETIME;

    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut creation = FILETIME::default();
        let mut exit = FILETIME::default();
        let mut kernel = FILETIME::default();
        let mut user = FILETIME::default();

        let ok = GetProcessTimes(handle, &mut creation, &mut exit, &mut kernel, &mut user);
        let _ = CloseHandle(handle);

        if ok.is_ok() {
            let time = (creation.dwHighDateTime as u64) << 32 | creation.dwLowDateTime as u64;
            Some(time)
        } else {
            None
        }
    }
}

/// Build the anchor filename from a PID chain. E.g. `[4, 423, 5000]` → `"4-423-5000.pipe"`.
pub fn anchor_chain_filename(chain: &[u32]) -> String {
    let parts: Vec<String> = chain.iter().map(|p| p.to_string()).collect();
    format!("{}.pipe", parts.join("-"))
}

/// Parse a PID chain from an anchor filename.
///
/// E.g. `"4-423-5000.pipe"` → `Some([4, 423, 5000])`.
pub fn parse_anchor_chain(filename: &str) -> Option<Vec<u32>> {
    let stem = filename.strip_suffix(".pipe")?;
    let pids: Option<Vec<u32>> = stem.split('-').map(|s| s.parse::<u32>().ok()).collect();
    pids
}

/// Return the anchor file path for a given PID chain.
///
/// On Windows, the anchor file is always a regular file containing the named
/// pipe path. (Named pipes cannot be enumerated on Windows, so the file
/// serves as the discoverable index.)
pub fn anchor_file_path(chain: &[u32]) -> PathBuf {
    roots_dir().join(anchor_chain_filename(chain))
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
