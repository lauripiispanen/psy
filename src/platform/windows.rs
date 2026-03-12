//! Windows platform abstraction (stub).
//!
//! This module provides the same public API surface as the Unix module but is
//! implemented using Win32 primitives.  Most functions contain only the
//! skeleton logic with TODO markers for full implementation — the project is
//! primarily developed on macOS / Linux.

use std::io;
use std::os::windows::io::RawHandle;
use std::path::PathBuf;
use std::sync::OnceLock;
use std::thread;
use std::time::Duration;

use windows::Win32::Foundation::{CloseHandle, HANDLE, BOOL};
use windows::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
    SetInformationJobObject, JOBOBJECT_BASIC_LIMIT_INFORMATION,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
};
use windows::Win32::System::Pipes::CreatePipe;
use windows::Win32::System::Threading::{GetCurrentProcess, TerminateProcess, OpenProcess, PROCESS_TERMINATE, PROCESS_QUERY_INFORMATION};
use windows::Win32::System::Console::{GenerateConsoleCtrlEvent, CTRL_BREAK_EVENT};
use windows::Win32::Security::SECURITY_ATTRIBUTES;

// ---------------------------------------------------------------------------
// Job Object (singleton)
// ---------------------------------------------------------------------------

static JOB_HANDLE: OnceLock<isize> = OnceLock::new();

/// Create a Job Object configured to kill all children when the root handle
/// is closed, and store it in a global for later use by `assign_to_job`.
pub fn setup_root() {
    let handle = unsafe {
        CreateJobObjectW(None, None).expect("CreateJobObjectW failed")
    };

    // Configure the job to kill all processes when the last handle closes.
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

    // Assign the current process to the job so that the handle stays open as
    // long as we are alive.
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
// Socket / named-pipe path
// ---------------------------------------------------------------------------

/// Return the named-pipe path for the daemon identified by `pid`.
pub fn socket_path(pid: u32) -> PathBuf {
    // Named pipes live in the kernel namespace, not the filesystem, but we
    // return a PathBuf for API consistency.
    PathBuf::from(format!(r"\\.\pipe\psy-{pid}"))
}

// ---------------------------------------------------------------------------
// Stale socket cleanup
// ---------------------------------------------------------------------------

/// On Windows named pipes are kernel objects and disappear when the owning
/// process exits, so there is nothing to clean up.
pub fn cleanup_stale_socket(_path: &std::path::Path) -> io::Result<()> {
    // No-op on Windows.
    Ok(())
}

// ---------------------------------------------------------------------------
// Death pipe
// ---------------------------------------------------------------------------

/// Create an anonymous pipe.  Returns `(read_handle, write_handle)` as raw
/// handles.
///
/// TODO: set the read handle as inheritable so children can use it.
pub fn create_death_pipe() -> io::Result<(RawHandle, RawHandle)> {
    let mut read_handle = HANDLE::default();
    let mut write_handle = HANDLE::default();

    // TODO: populate SECURITY_ATTRIBUTES to make the read end inheritable.
    let sa = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        bInheritHandle: BOOL(1),
        lpSecurityDescriptor: std::ptr::null_mut(),
    };

    unsafe {
        CreatePipe(&mut read_handle, &mut write_handle, Some(&sa), 0)
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;
    }

    Ok((read_handle.0 as RawHandle, write_handle.0 as RawHandle))
}

/// Spawn a background thread that waits for the death pipe to signal EOF.
///
/// TODO: use `ReadFile` in a blocking loop; on EOF call `ExitProcess(1)`.
pub fn spawn_watchdog_thread(read_handle: RawHandle) {
    thread::spawn(move || {
        // TODO: blocking ReadFile on `read_handle`.  When the write end is
        // closed (parent exits) ReadFile will return 0 bytes / error and we
        // should call std::process::exit(1).
        loop {
            thread::sleep(Duration::from_secs(3600));
        }
    });
}

// ---------------------------------------------------------------------------
// Graceful stop
// ---------------------------------------------------------------------------

/// Attempt a graceful stop of process `pid`.
///
/// 1. Send `CTRL_BREAK_EVENT` to the process's console group.
/// 2. Wait up to `timeout`.
/// 3. If still alive, call `TerminateProcess`.
pub fn stop_process(pid: u32, timeout: Duration) {
    // TODO: full implementation with process handle management.
    unsafe {
        // Try a console break first.
        let _ = GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, pid);
    }

    // Wait (rough polling).
    let step = Duration::from_millis(100);
    let mut elapsed = Duration::ZERO;
    while elapsed < timeout {
        thread::sleep(step);
        elapsed += step;
        // TODO: check if process is still alive via OpenProcess / WaitForSingleObject.
    }

    // Forcefully terminate.
    unsafe {
        if let Ok(h) = OpenProcess(PROCESS_TERMINATE, false, pid) {
            let _ = TerminateProcess(h, 1);
            let _ = CloseHandle(h);
        }
    }
}
