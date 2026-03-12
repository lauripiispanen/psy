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
use windows::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
    SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
};
use windows::Win32::System::Pipes::CreatePipe;
use windows::Win32::System::Threading::{
    GetCurrentProcess, OpenProcess, TerminateProcess, PROCESS_SYNCHRONIZE, PROCESS_TERMINATE,
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
