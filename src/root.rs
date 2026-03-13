use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{watch, Mutex};

use crate::platform;
use crate::process::{
    calculate_backoff, should_restart, spawn_child, spawn_child_attached, validate_name,
    ProcessEntry, ProcessState,
};
use crate::protocol::*;
use crate::ring_buffer::Stream as RBStream;

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

pub type RootResult<T> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

// ---------------------------------------------------------------------------
// Shared root state (Arc-friendly)
// ---------------------------------------------------------------------------

pub struct SharedRoot {
    pub process_table: Arc<Mutex<HashMap<String, ProcessEntry>>>,
    pub socket_path: String,
    pub psy_sock: String,
    pub psy_root_pid: u32,
    #[allow(dead_code)]
    pub death_pipe: platform::DeathPipe,
    pub shutting_down: Arc<AtomicBool>,
    pub main_exit_tx: watch::Sender<Option<i32>>,
}

// ---------------------------------------------------------------------------
// PsyRoot
// ---------------------------------------------------------------------------

pub struct PsyRoot {
    shared: Arc<SharedRoot>,
    main_exit_rx: watch::Receiver<Option<i32>>,
}

impl PsyRoot {
    pub fn new(_name: String) -> RootResult<Self> {
        // Platform-specific root setup (setsid / subreaper / Job Object)
        platform::setup_root();

        // Create the death pipe
        let death_pipe = platform::create_death_pipe()?;

        let pid = std::process::id();
        let socket_path = platform::socket_path(pid);

        // Clean up any stale socket from a previous run
        platform::cleanup_stale_socket(std::path::Path::new(&socket_path))?;

        // Ensure the parent directory exists (Unix sockets need this)
        #[cfg(unix)]
        if let Some(parent) = std::path::Path::new(&socket_path).parent() {
            if !parent.as_os_str().is_empty() {
                let _ = std::fs::create_dir_all(parent);
            }
        }

        let psy_sock = socket_path.clone();
        let (main_exit_tx, main_exit_rx) = watch::channel(None);

        let shared = Arc::new(SharedRoot {
            process_table: Arc::new(Mutex::new(HashMap::new())),
            socket_path,
            psy_sock,
            psy_root_pid: pid,
            death_pipe,
            shutting_down: Arc::new(AtomicBool::new(false)),
            main_exit_tx,
        });

        Ok(Self {
            shared,
            main_exit_rx,
        })
    }

    /// Run the psy root server.
    ///
    /// `main_command` — the main process command (or `None` to use `$SHELL`).
    ///
    /// Returns the main process exit code.
    pub async fn run(mut self, main_command: Option<Vec<String>>) -> RootResult<i32> {
        // Determine the main command
        let main_cmd = main_command.unwrap_or_else(|| {
            #[cfg(unix)]
            let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
            #[cfg(windows)]
            let shell = std::env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".into());
            vec![shell]
        });

        // Launch the main process
        {
            let mut table = self.shared.process_table.lock().await;
            let mut entry = ProcessEntry::new(
                "main".into(),
                main_cmd,
                HashMap::new(),
                RestartPolicy::No,
                true,
            );

            let child = spawn_child(&mut entry, &self.shared.psy_sock, self.shared.psy_root_pid)?;
            entry.child = Some(child);
            table.insert("main".into(), entry);
        }

        // Spawn the main process monitor
        {
            let root = Arc::clone(&self.shared);
            tokio::spawn(async move {
                monitor_child(root, "main".into()).await;
            });
        }

        // Spawn the socket listener
        {
            let root = Arc::clone(&self.shared);
            tokio::spawn(async move {
                if let Err(e) = run_socket_listener(root).await {
                    eprintln!("psy: socket listener error: {e}");
                }
            });
        }

        // Wait for the main process to exit or a signal
        let exit_code = wait_for_exit_or_signal(&mut self.main_exit_rx).await;

        // Teardown all remaining children
        teardown(Arc::clone(&self.shared)).await;

        Ok(exit_code)
    }
}

// ---------------------------------------------------------------------------
// Signal handling
// ---------------------------------------------------------------------------

#[cfg(unix)]
async fn wait_for_exit_or_signal(
    rx: &mut tokio::sync::watch::Receiver<Option<i32>>,
) -> i32 {
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .expect("failed to register SIGTERM handler");
    loop {
        tokio::select! {
            _ = rx.changed() => {
                if let Some(code) = *rx.borrow() {
                    return code;
                }
            }
            _ = tokio::signal::ctrl_c() => {
                return 130; // 128 + SIGINT(2)
            }
            _ = sigterm.recv() => {
                return 143; // 128 + SIGTERM(15)
            }
        }
    }
}

#[cfg(windows)]
async fn wait_for_exit_or_signal(
    rx: &mut tokio::sync::watch::Receiver<Option<i32>>,
) -> i32 {
    loop {
        tokio::select! {
            _ = rx.changed() => {
                if let Some(code) = *rx.borrow() {
                    return code;
                }
            }
            _ = tokio::signal::ctrl_c() => {
                return 130;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Request handling
// ---------------------------------------------------------------------------

enum HandleResult {
    Response(Response),
    AttachSession { name: String, response: Response },
}

async fn handle_request(root: &Arc<SharedRoot>, req: Request) -> HandleResult {
    match req.cmd.as_str() {
        CMD_RUN => handle_run(root, &req).await,
        CMD_PS => HandleResult::Response(handle_ps(root, &req).await),
        CMD_LOGS => HandleResult::Response(handle_logs(root, &req).await),
        CMD_STOP => HandleResult::Response(handle_stop(root, &req).await),
        CMD_RESTART => HandleResult::Response(handle_restart(root, &req).await),
        CMD_DOWN => HandleResult::Response(handle_down(root, &req).await),
        CMD_HISTORY => HandleResult::Response(handle_history(root, &req).await),
        _ => HandleResult::Response(Response::err(&req.id, format!("unknown command: {}", req.cmd))),
    }
}

async fn handle_run(root: &Arc<SharedRoot>, req: &Request) -> HandleResult {
    if root.shutting_down.load(Ordering::Relaxed) {
        return HandleResult::Response(Response::err(&req.id, "server is shutting down"));
    }

    let args: RunArgs = match req
        .args
        .as_ref()
        .and_then(|v| serde_json::from_value(v.clone()).ok())
    {
        Some(a) => a,
        None => return HandleResult::Response(Response::err(&req.id, "invalid or missing run args")),
    };

    if !validate_name(&args.name) {
        return HandleResult::Response(Response::err(
            &req.id,
            "invalid name: must match [a-zA-Z0-9][a-zA-Z0-9_-]{0,62}",
        ));
    }

    let attach = args.attach;
    let mut table = root.process_table.lock().await;

    // Allow replacing stopped/failed tombstones; only reject if still running
    // Preserve run history from the old entry
    let old_history = if let Some(existing) = table.get_mut(&args.name) {
        if existing.state == ProcessState::Running {
            return HandleResult::Response(Response::err(
                &req.id,
                format!("process '{}' is already running", args.name),
            ));
        }
        // Archive the old entry's current run, then take its history
        existing.archive_current_run();
        let history = std::mem::take(&mut existing.run_history);
        let next_id = existing.current_run_id;
        table.remove(&args.name);
        Some((history, next_id))
    } else {
        None
    };

    let mut entry = ProcessEntry::new(
        args.name.clone(),
        args.command.clone(),
        args.env.clone(),
        args.restart,
        false,
    );

    // Restore history from previous incarnation
    if let Some((history, next_id)) = old_history {
        entry.run_history = history;
        entry.current_run_id = next_id;
    }

    let child = if attach {
        match spawn_child_attached(&mut entry, &root.psy_sock, root.psy_root_pid) {
            Ok(c) => c,
            Err(e) => return HandleResult::Response(Response::err(&req.id, format!("spawn failed: {e}"))),
        }
    } else {
        match spawn_child(&mut entry, &root.psy_sock, root.psy_root_pid) {
            Ok(c) => c,
            Err(e) => return HandleResult::Response(Response::err(&req.id, format!("spawn failed: {e}"))),
        }
    };

    entry.child = Some(child);
    let name = args.name.clone();
    table.insert(name.clone(), entry);
    drop(table);

    // Spawn a monitor task for this child
    let root_clone = Arc::clone(root);
    let monitor_name = name.clone();
    tokio::spawn(async move {
        monitor_child(root_clone, monitor_name).await;
    });

    let response = Response::ok(
        &req.id,
        Some(serde_json::json!({ "name": args.name, "status": "running" })),
    );

    if attach {
        HandleResult::AttachSession { name, response }
    } else {
        HandleResult::Response(response)
    }
}

async fn handle_ps(root: &Arc<SharedRoot>, req: &Request) -> Response {
    let table = root.process_table.lock().await;
    let processes: Vec<ProcessInfo> = table.values().map(|e| e.to_ps_entry()).collect();
    let ps_response = PsResponse { processes };
    Response::ok(&req.id, Some(serde_json::to_value(ps_response).unwrap()))
}

async fn handle_logs(root: &Arc<SharedRoot>, req: &Request) -> Response {
    let args: LogsArgs = match req
        .args
        .as_ref()
        .and_then(|v| serde_json::from_value(v.clone()).ok())
    {
        Some(a) => a,
        None => return Response::err(&req.id, "invalid or missing logs args"),
    };

    let since = args
        .since
        .as_deref()
        .map(|s| {
            chrono::DateTime::parse_from_rfc3339(s)
                .map(|dt| dt.with_timezone(&chrono::Utc))
        })
        .transpose();
    let since = match since {
        Ok(s) => s,
        Err(e) => return Response::err(&req.id, format!("invalid since timestamp: {e}")),
    };

    let until = args
        .until
        .as_deref()
        .map(|s| {
            chrono::DateTime::parse_from_rfc3339(s)
                .map(|dt| dt.with_timezone(&chrono::Utc))
        })
        .transpose();
    let until = match until {
        Ok(u) => u,
        Err(e) => return Response::err(&req.id, format!("invalid until timestamp: {e}")),
    };

    let table = root.process_table.lock().await;
    let entry = match table.get(&args.name) {
        Some(e) => e,
        None => return Response::err(&req.id, format!("process '{}' not found", args.name)),
    };

    // Resolve which run's buffers to use
    let (stdout_buf, stderr_buf) = if args.previous {
        // --previous: the run before the current one
        if entry.run_history.is_empty() {
            return Response::err(&req.id, "no previous run");
        }
        let prev = entry.run_history.last().unwrap();
        (Arc::clone(&prev.stdout_buf), Arc::clone(&prev.stderr_buf))
    } else if let Some(run_id) = args.run {
        if run_id == entry.current_run_id {
            (Arc::clone(&entry.stdout_buf), Arc::clone(&entry.stderr_buf))
        } else {
            match entry.run_history.iter().find(|r| r.run_id == run_id) {
                Some(record) => (Arc::clone(&record.stdout_buf), Arc::clone(&record.stderr_buf)),
                None => return Response::err(&req.id, format!("run {} not found", run_id)),
            }
        }
    } else {
        (Arc::clone(&entry.stdout_buf), Arc::clone(&entry.stderr_buf))
    };

    // Collect lines from both stdout and stderr buffers, merge by timestamp
    let grep_ref = args.grep.as_deref();
    let stdout_lines = stdout_buf.lines(None, args.stream, since, until, grep_ref);
    let stderr_lines = stderr_buf.lines(None, args.stream, since, until, grep_ref);

    let mut all_lines: Vec<_> = stdout_lines
        .into_iter()
        .chain(stderr_lines.into_iter())
        .collect();
    all_lines.sort_by_key(|l| l.timestamp);

    // Apply tail
    if let Some(n) = args.tail {
        let start = all_lines.len().saturating_sub(n);
        all_lines = all_lines.split_off(start);
    }

    let lines_json: Vec<serde_json::Value> = all_lines
        .iter()
        .map(|l| {
            serde_json::json!({
                "timestamp": l.timestamp.to_rfc3339(),
                "stream": l.stream,
                "content": l.content,
            })
        })
        .collect();

    Response::ok(&req.id, Some(serde_json::json!({ "lines": lines_json })))
}

async fn handle_stop(root: &Arc<SharedRoot>, req: &Request) -> Response {
    let args: StopArgs = match req
        .args
        .as_ref()
        .and_then(|v| serde_json::from_value(v.clone()).ok())
    {
        Some(a) => a,
        None => return Response::err(&req.id, "invalid or missing stop args"),
    };

    if args.name == "main" {
        return Response::err(&req.id, "cannot stop the main process (use 'down' instead)");
    }

    let pid = {
        let mut table = root.process_table.lock().await;
        match table.get_mut(&args.name) {
            Some(entry) if entry.state == ProcessState::Running => {
                entry.stopping.store(true, Ordering::Relaxed);
                entry.pid
            }
            Some(_) => {
                return Response::err(&req.id, format!("process '{}' is not running", args.name))
            }
            None => return Response::err(&req.id, format!("process '{}' not found", args.name)),
        }
    };

    if let Some(pid) = pid {
        // stop_process is synchronous and blocking, so run it on a blocking thread
        tokio::task::spawn_blocking(move || {
            platform::stop_process(pid, Duration::from_secs(10));
        })
        .await
        .ok();
    }

    Response::ok(
        &req.id,
        Some(serde_json::json!({ "name": args.name, "status": "stopped" })),
    )
}

async fn handle_restart(root: &Arc<SharedRoot>, req: &Request) -> Response {
    let args: RestartArgs = match req
        .args
        .as_ref()
        .and_then(|v| serde_json::from_value(v.clone()).ok())
    {
        Some(a) => a,
        None => return Response::err(&req.id, "invalid or missing restart args"),
    };

    // Get info needed to stop and respawn
    let (pid, command, env, restart_policy) = {
        let mut table = root.process_table.lock().await;
        match table.get_mut(&args.name) {
            Some(entry) => {
                entry.stopping.store(true, Ordering::Relaxed);
                (
                    entry.pid,
                    entry.command.clone(),
                    entry.env.clone(),
                    entry.restart_policy,
                )
            }
            None => return Response::err(&req.id, format!("process '{}' not found", args.name)),
        }
    };

    // Stop if running
    if let Some(pid) = pid {
        let timeout = Duration::from_secs(10);
        tokio::task::spawn_blocking(move || {
            platform::stop_process(pid, timeout);
        })
        .await
        .ok();
        // Wait briefly for the monitor to update state
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    // Archive current run and re-create with fresh buffers
    {
        let mut table = root.process_table.lock().await;
        // Archive the old run's state and take the history
        let (history, next_id) = if let Some(old) = table.get_mut(&args.name) {
            old.archive_current_run();
            (std::mem::take(&mut old.run_history), old.current_run_id)
        } else {
            (Vec::new(), 1)
        };
        table.remove(&args.name);

        let mut entry = ProcessEntry::new(args.name.clone(), command, env, restart_policy, false);
        entry.run_history = history;
        entry.current_run_id = next_id;

        let child = match spawn_child(&mut entry, &root.psy_sock, root.psy_root_pid) {
            Ok(c) => c,
            Err(e) => return Response::err(&req.id, format!("restart spawn failed: {e}")),
        };

        entry.child = Some(child);
        let name = args.name.clone();
        table.insert(name.clone(), entry);
        drop(table);

        // Spawn monitor for the restarted process
        let root_clone = Arc::clone(root);
        tokio::spawn(async move {
            monitor_child(root_clone, name).await;
        });
    }

    Response::ok(
        &req.id,
        Some(serde_json::json!({ "name": args.name, "status": "running" })),
    )
}

async fn handle_down(root: &Arc<SharedRoot>, req: &Request) -> Response {
    root.shutting_down.store(true, Ordering::Relaxed);
    teardown(Arc::clone(root)).await;

    // Signal main exit if not already signaled
    let _ = root.main_exit_tx.send(Some(0));

    Response::ok(&req.id, Some(serde_json::json!({ "status": "shutdown" })))
}

async fn handle_history(root: &Arc<SharedRoot>, req: &Request) -> Response {
    let args: HistoryArgs = match req
        .args
        .as_ref()
        .and_then(|v| serde_json::from_value(v.clone()).ok())
    {
        Some(a) => a,
        None => return Response::err(&req.id, "invalid or missing history args"),
    };

    let table = root.process_table.lock().await;
    let entry = match table.get(&args.name) {
        Some(e) => e,
        None => return Response::err(&req.id, format!("process '{}' not found", args.name)),
    };

    let mut runs: Vec<RunInfo> = entry.run_history.iter().map(|r| r.to_run_info()).collect();
    runs.push(entry.current_run_info());

    let history = HistoryResponse {
        name: args.name,
        runs,
    };

    Response::ok(&req.id, Some(serde_json::to_value(history).unwrap()))
}

// ---------------------------------------------------------------------------
// Teardown
// ---------------------------------------------------------------------------

async fn teardown(root: Arc<SharedRoot>) {
    root.shutting_down.store(true, Ordering::Relaxed);

    // Collect running PIDs
    let pids: Vec<(String, Option<u32>)> = {
        let table = root.process_table.lock().await;
        table
            .iter()
            .filter(|(_, e)| e.state == ProcessState::Running)
            .map(|(name, e)| (name.clone(), e.pid))
            .collect()
    };

    // Stop all running processes (reverse order)
    for (name, pid) in pids.iter().rev() {
        if let Some(pid) = pid {
            let pid = *pid;
            let name = name.clone();
            if let Err(e) = tokio::task::spawn_blocking(move || {
                platform::stop_process(pid, Duration::from_secs(10));
            })
            .await
            {
                eprintln!("psy: failed to stop {name}: {e}");
            }
        }
    }

    // Clean up the socket file
    let _ = std::fs::remove_file(&root.socket_path);
}

// ---------------------------------------------------------------------------
// Child process monitor
// ---------------------------------------------------------------------------

async fn monitor_child(root: Arc<SharedRoot>, name: String) {
    loop {
        // Take the child handle out of the table so we can await it without
        // holding the lock.
        let child_handle = {
            let mut table = root.process_table.lock().await;
            let entry = match table.get_mut(&name) {
                Some(e) if e.state == ProcessState::Running => e,
                _ => return,
            };
            match entry.child.take() {
                Some(c) => c,
                None => return,
            }
        };

        // Wait for exit (without holding the lock)
        let mut child_handle = child_handle;
        let status = child_handle.wait().await;

        let exit_code = status.as_ref().ok().and_then(|s| s.code());

        // Update entry state
        let (is_main, do_restart, backoff) = {
            let mut table = root.process_table.lock().await;
            let entry = match table.get_mut(&name) {
                Some(e) => e,
                None => return,
            };

            entry.stopped_at = Some(chrono::Utc::now());
            let was_intentional = entry.stopping.swap(false, Ordering::Relaxed);

            if was_intentional {
                // Intentional stop (psy stop / restart) — always mark as stopped
                entry.state = ProcessState::Stopped;
                entry.exit_status = exit_code;
                #[cfg(unix)]
                {
                    use std::os::unix::process::ExitStatusExt;
                    if let Ok(ref s) = status {
                        if let Some(sig) = s.signal() {
                            entry.signal = Some(format!("SIG{sig}"));
                        }
                    }
                }
            } else {
                match exit_code {
                    Some(0) => {
                        entry.state = ProcessState::Stopped;
                        entry.exit_status = Some(0);
                    }
                    Some(code) => {
                        entry.state = ProcessState::Failed;
                        entry.exit_status = Some(code);
                    }
                    None => {
                        // Killed by signal
                        entry.state = ProcessState::Failed;
                        entry.exit_status = None;
                        #[cfg(unix)]
                        {
                            use std::os::unix::process::ExitStatusExt;
                            if let Ok(ref s) = status {
                                if let Some(sig) = s.signal() {
                                    entry.signal = Some(format!("SIG{sig}"));
                                }
                            }
                        }
                    }
                }
            }

            entry.pid = None;

            let do_restart =
                !root.shutting_down.load(Ordering::Relaxed) && should_restart(entry, exit_code);
            let backoff = calculate_backoff(entry.restarts);
            let is_main = entry.is_main;

            (is_main, do_restart, backoff)
        };

        // If main process, signal exit
        if is_main {
            let _ = root.main_exit_tx.send(Some(exit_code.unwrap_or(1)));
            return;
        }

        // Handle restart
        if do_restart {
            tokio::time::sleep(backoff).await;

            let mut table = root.process_table.lock().await;
            let entry = match table.get_mut(&name) {
                Some(e) => e,
                None => return,
            };

            entry.restarts += 1;

            // Archive the completed run and start fresh buffers
            entry.archive_current_run();

            match spawn_child(entry, &root.psy_sock, root.psy_root_pid) {
                Ok(child) => {
                    entry.child = Some(child);
                    // Continue the loop to monitor the new child
                }
                Err(e) => {
                    eprintln!("psy: failed to restart {name}: {e}");
                    entry.state = ProcessState::Failed;
                    return;
                }
            }
        } else {
            return;
        }
    }
}

// ---------------------------------------------------------------------------
// Socket listener — platform-specific
// ---------------------------------------------------------------------------

#[cfg(unix)]
async fn run_socket_listener(root: Arc<SharedRoot>) -> RootResult<()> {
    let listener = tokio::net::UnixListener::bind(&root.socket_path)?;

    loop {
        let (stream, _addr) = listener.accept().await?;

        if root.shutting_down.load(Ordering::Relaxed) {
            break;
        }

        let root_clone = Arc::clone(&root);
        tokio::spawn(async move {
            if let Err(e) = handle_unix_connection(root_clone, stream).await {
                eprintln!("psy: connection error: {e}");
            }
        });
    }

    Ok(())
}

#[cfg(unix)]
async fn handle_unix_connection(
    root: Arc<SharedRoot>,
    stream: tokio::net::UnixStream,
) -> RootResult<()> {
    let (reader, mut writer) = stream.into_split();
    let mut buf_reader = BufReader::new(reader);
    let mut lines = (&mut buf_reader).lines();

    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }

        let req: Request = match serde_json::from_str(&line) {
            Ok(r) => r,
            Err(e) => {
                let err_resp = Response::err("", format!("invalid JSON: {e}"));
                let mut json = serde_json::to_string(&err_resp)?;
                json.push('\n');
                writer.write_all(json.as_bytes()).await?;
                continue;
            }
        };

        // Special case: logs_follow keeps the connection open
        if req.cmd == CMD_LOGS_FOLLOW {
            handle_logs_follow(&root, &req, &mut writer).await?;
            return Ok(());
        }

        let result = handle_request(&root, req).await;
        match result {
            HandleResult::Response(resp) => {
                let mut json = serde_json::to_string(&resp)?;
                json.push('\n');
                writer.write_all(json.as_bytes()).await?;
            }
            HandleResult::AttachSession { name, response } => {
                let mut json = serde_json::to_string(&response)?;
                json.push('\n');
                writer.write_all(json.as_bytes()).await?;
                // Enter bidirectional attach session
                handle_attach_session_impl(
                    &root,
                    &response.id,
                    &name,
                    &mut buf_reader,
                    &mut writer,
                )
                .await?;
                return Ok(());
            }
        }
    }

    Ok(())
}

#[cfg(unix)]
async fn handle_logs_follow(
    root: &Arc<SharedRoot>,
    req: &Request,
    writer: &mut tokio::net::unix::OwnedWriteHalf,
) -> RootResult<()> {
    handle_logs_follow_impl(root, req, writer).await
}

// Windows: use tokio named pipes for IPC.
#[cfg(windows)]
async fn run_socket_listener(root: Arc<SharedRoot>) -> RootResult<()> {
    use tokio::net::windows::named_pipe::ServerOptions;

    let pipe_name = &root.socket_path;

    // Create the first pipe instance.
    let mut server = ServerOptions::new()
        .first_pipe_instance(true)
        .create(pipe_name)?;

    loop {
        // Wait for a client to connect.
        server.connect().await?;
        let connected = server;

        // Create the next server instance immediately so clients don't get NotFound.
        server = ServerOptions::new().create(pipe_name)?;

        if root.shutting_down.load(Ordering::Relaxed) {
            break;
        }

        let root_clone = Arc::clone(&root);
        tokio::spawn(async move {
            if let Err(e) = handle_named_pipe_connection(root_clone, connected).await {
                eprintln!("psy: connection error: {e}");
            }
        });
    }

    Ok(())
}

#[cfg(windows)]
async fn handle_named_pipe_connection(
    root: Arc<SharedRoot>,
    pipe: tokio::net::windows::named_pipe::NamedPipeServer,
) -> RootResult<()> {
    let (reader, mut writer) = tokio::io::split(pipe);
    let mut buf_reader = BufReader::new(reader);
    let mut lines = (&mut buf_reader).lines();

    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }

        let req: Request = match serde_json::from_str(&line) {
            Ok(r) => r,
            Err(e) => {
                let err_resp = Response::err("", format!("invalid JSON: {e}"));
                let mut json = serde_json::to_string(&err_resp)?;
                json.push('\n');
                writer.write_all(json.as_bytes()).await?;
                continue;
            }
        };

        if req.cmd == CMD_LOGS_FOLLOW {
            handle_logs_follow_impl(&root, &req, &mut writer).await?;
            return Ok(());
        }

        let result = handle_request(&root, req).await;
        match result {
            HandleResult::Response(resp) => {
                let mut json = serde_json::to_string(&resp)?;
                json.push('\n');
                writer.write_all(json.as_bytes()).await?;
            }
            HandleResult::AttachSession { name, response } => {
                let mut json = serde_json::to_string(&response)?;
                json.push('\n');
                writer.write_all(json.as_bytes()).await?;
                handle_attach_session_impl(
                    &root,
                    &response.id,
                    &name,
                    &mut buf_reader,
                    &mut writer,
                )
                .await?;
                return Ok(());
            }
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Shared logs_follow implementation (generic over AsyncWrite)
// ---------------------------------------------------------------------------

async fn handle_logs_follow_impl<W: tokio::io::AsyncWrite + Unpin>(
    root: &Arc<SharedRoot>,
    req: &Request,
    writer: &mut W,
) -> RootResult<()> {
    let args: LogsArgs = match req
        .args
        .as_ref()
        .and_then(|v| serde_json::from_value(v.clone()).ok())
    {
        Some(a) => a,
        None => {
            let resp = Response::err(&req.id, "invalid or missing logs args");
            let mut json = serde_json::to_string(&resp)?;
            json.push('\n');
            writer.write_all(json.as_bytes()).await?;
            return Ok(());
        }
    };

    let since = args
        .since
        .as_deref()
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map(|dt| dt.with_timezone(&chrono::Utc));

    let grep = args.grep.clone();

    let (mut stdout_rx, mut stderr_rx) = {
        let table = root.process_table.lock().await;
        let entry = match table.get(&args.name) {
            Some(e) => e,
            None => {
                let resp = Response::err(&req.id, format!("process '{}' not found", args.name));
                let mut json = serde_json::to_string(&resp)?;
                json.push('\n');
                writer.write_all(json.as_bytes()).await?;
                return Ok(());
            }
        };

        // Send existing tail lines first (with since/grep filtering)
        let grep_ref = grep.as_deref();
        let stdout_lines = entry.stdout_buf.lines(None, args.stream, since, None, grep_ref);
        let stderr_lines = entry.stderr_buf.lines(None, args.stream, since, None, grep_ref);

        let mut all_lines: Vec<_> = stdout_lines
            .into_iter()
            .chain(stderr_lines.into_iter())
            .collect();
        all_lines.sort_by_key(|l| l.timestamp);

        if let Some(n) = args.tail {
            let start = all_lines.len().saturating_sub(n);
            all_lines = all_lines.split_off(start);
        }

        for line in &all_lines {
            let stream_kind = match line.stream {
                RBStream::Stdout => StreamKind::Stdout,
                RBStream::Stderr => StreamKind::Stderr,
            };
            let log_resp = LogLineResponse {
                id: req.id.clone(),
                name: args.name.clone(),
                timestamp: line.timestamp.to_rfc3339(),
                stream: stream_kind,
                content: line.content.clone(),
            };
            let mut json = serde_json::to_string(&log_resp)?;
            json.push('\n');
            if writer.write_all(json.as_bytes()).await.is_err() {
                return Ok(());
            }
        }

        // Subscribe to new lines
        (entry.stdout_buf.subscribe(), entry.stderr_buf.subscribe())
    };

    // Stream new lines from both stdout and stderr
    loop {
        let log_line = tokio::select! {
            result = stdout_rx.recv() => {
                match result {
                    Ok(line) => line,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                }
            }
            result = stderr_rx.recv() => {
                match result {
                    Ok(line) => line,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                }
            }
        };

        // Apply stream filter
        let passes_filter = match args.stream {
            StreamFilter::All => true,
            StreamFilter::Stdout => log_line.stream == RBStream::Stdout,
            StreamFilter::Stderr => log_line.stream == RBStream::Stderr,
        };

        if !passes_filter {
            continue;
        }

        // Apply since filter for streamed lines
        if let Some(ref s) = since {
            if log_line.timestamp < *s {
                continue;
            }
        }

        // Apply grep filter for streamed lines
        if let Some(ref pattern) = grep {
            if !pattern.is_empty()
                && !log_line
                    .content
                    .to_lowercase()
                    .contains(&pattern.to_lowercase())
            {
                continue;
            }
        }

        let stream_kind = match log_line.stream {
            RBStream::Stdout => StreamKind::Stdout,
            RBStream::Stderr => StreamKind::Stderr,
        };

        let log_resp = LogLineResponse {
            id: req.id.clone(),
            name: args.name.clone(),
            timestamp: log_line.timestamp.to_rfc3339(),
            stream: stream_kind,
            content: log_line.content,
        };

        let mut json = serde_json::to_string(&log_resp)?;
        json.push('\n');
        if writer.write_all(json.as_bytes()).await.is_err() {
            // Client disconnected
            break;
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Attach session implementation (generic over AsyncRead + AsyncWrite)
// ---------------------------------------------------------------------------

async fn handle_attach_session_impl<R, W>(
    root: &Arc<SharedRoot>,
    req_id: &str,
    name: &str,
    reader: &mut R,
    writer: &mut W,
) -> RootResult<()>
where
    R: tokio::io::AsyncBufRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    let (mut stdout_rx, mut stderr_rx) = {
        let table = root.process_table.lock().await;
        let entry = match table.get(name) {
            Some(e) => e,
            None => return Ok(()),
        };
        (entry.stdout_buf.subscribe(), entry.stderr_buf.subscribe())
    };

    let mut stdin_buf = String::new();
    loop {
        stdin_buf.clear();
        tokio::select! {
            // Read stdin from client
            result = reader.read_line(&mut stdin_buf) => {
                match result {
                    Ok(0) | Err(_) => {
                        // Client disconnected — detach but don't kill child
                        break;
                    }
                    Ok(_) => {
                        // Parse as StdinData
                        if let Ok(stdin_data) = serde_json::from_str::<crate::protocol::StdinData>(&stdin_buf) {
                            let mut table = root.process_table.lock().await;
                            if let Some(entry) = table.get_mut(name) {
                                if let Some(ref mut stdin_handle) = entry.stdin_handle {
                                    let _ = stdin_handle.write_all(stdin_data.stdin.as_bytes()).await;
                                    let _ = stdin_handle.flush().await;
                                }
                            }
                        }
                    }
                }
            }
            // Forward stdout
            result = stdout_rx.recv() => {
                match result {
                    Ok(log_line) => {
                        let stream_kind = match log_line.stream {
                            RBStream::Stdout => StreamKind::Stdout,
                            RBStream::Stderr => StreamKind::Stderr,
                        };
                        let log_resp = LogLineResponse {
                            id: req_id.to_string(),
                            name: name.to_string(),
                            timestamp: log_line.timestamp.to_rfc3339(),
                            stream: stream_kind,
                            content: log_line.content,
                        };
                        let mut json = serde_json::to_string(&log_resp).unwrap_or_default();
                        json.push('\n');
                        if writer.write_all(json.as_bytes()).await.is_err() {
                            break;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        // Child exited — send detach notice
                        let exit_code = {
                            let table = root.process_table.lock().await;
                            table.get(name).and_then(|e| e.exit_status)
                        };
                        let notice = crate::protocol::DetachNotice {
                            detached: true,
                            reason: "exited".into(),
                            exit_code,
                        };
                        let mut json = serde_json::to_string(&notice).unwrap_or_default();
                        json.push('\n');
                        let _ = writer.write_all(json.as_bytes()).await;
                        break;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                }
            }
            // Forward stderr
            result = stderr_rx.recv() => {
                match result {
                    Ok(log_line) => {
                        let stream_kind = match log_line.stream {
                            RBStream::Stdout => StreamKind::Stdout,
                            RBStream::Stderr => StreamKind::Stderr,
                        };
                        let log_resp = LogLineResponse {
                            id: req_id.to_string(),
                            name: name.to_string(),
                            timestamp: log_line.timestamp.to_rfc3339(),
                            stream: stream_kind,
                            content: log_line.content,
                        };
                        let mut json = serde_json::to_string(&log_resp).unwrap_or_default();
                        json.push('\n');
                        if writer.write_all(json.as_bytes()).await.is_err() {
                            break;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        let exit_code = {
                            let table = root.process_table.lock().await;
                            table.get(name).and_then(|e| e.exit_status)
                        };
                        let notice = crate::protocol::DetachNotice {
                            detached: true,
                            reason: "exited".into(),
                            exit_code,
                        };
                        let mut json = serde_json::to_string(&notice).unwrap_or_default();
                        json.push('\n');
                        let _ = writer.write_all(json.as_bytes()).await;
                        break;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                }
            }
        }
    }

    Ok(())
}
