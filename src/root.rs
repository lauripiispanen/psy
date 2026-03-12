use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{watch, Mutex};

use crate::platform;
use crate::process::{
    calculate_backoff, should_restart, spawn_child, validate_name, ProcessEntry, ProcessState,
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

        // Wait for the main process to exit
        let exit_code = loop {
            self.main_exit_rx.changed().await.ok();
            if let Some(code) = *self.main_exit_rx.borrow() {
                break code;
            }
        };

        // Teardown all remaining children
        teardown(Arc::clone(&self.shared)).await;

        Ok(exit_code)
    }
}

// ---------------------------------------------------------------------------
// Request handling
// ---------------------------------------------------------------------------

async fn handle_request(root: &Arc<SharedRoot>, req: Request) -> Response {
    match req.cmd.as_str() {
        CMD_RUN => handle_run(root, &req).await,
        CMD_PS => handle_ps(root, &req).await,
        CMD_LOGS => handle_logs(root, &req).await,
        CMD_STOP => handle_stop(root, &req).await,
        CMD_RESTART => handle_restart(root, &req).await,
        CMD_DOWN => handle_down(root, &req).await,
        _ => Response::err(&req.id, format!("unknown command: {}", req.cmd)),
    }
}

async fn handle_run(root: &Arc<SharedRoot>, req: &Request) -> Response {
    if root.shutting_down.load(Ordering::Relaxed) {
        return Response::err(&req.id, "server is shutting down");
    }

    let args: RunArgs = match req
        .args
        .as_ref()
        .and_then(|v| serde_json::from_value(v.clone()).ok())
    {
        Some(a) => a,
        None => return Response::err(&req.id, "invalid or missing run args"),
    };

    if !validate_name(&args.name) {
        return Response::err(
            &req.id,
            "invalid name: must match [a-zA-Z0-9][a-zA-Z0-9_-]{0,62}",
        );
    }

    let mut table = root.process_table.lock().await;

    if table.contains_key(&args.name) {
        return Response::err(&req.id, format!("process '{}' already exists", args.name));
    }

    let mut entry = ProcessEntry::new(
        args.name.clone(),
        args.command.clone(),
        args.env.clone(),
        args.restart,
        false,
    );

    let child = match spawn_child(&mut entry, &root.psy_sock, root.psy_root_pid) {
        Ok(c) => c,
        Err(e) => return Response::err(&req.id, format!("spawn failed: {e}")),
    };

    entry.child = Some(child);
    let name = args.name.clone();
    table.insert(name.clone(), entry);
    drop(table);

    // Spawn a monitor task for this child
    let root_clone = Arc::clone(root);
    tokio::spawn(async move {
        monitor_child(root_clone, name).await;
    });

    Response::ok(
        &req.id,
        Some(serde_json::json!({ "name": args.name, "status": "running" })),
    )
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

    let table = root.process_table.lock().await;
    let entry = match table.get(&args.name) {
        Some(e) => e,
        None => return Response::err(&req.id, format!("process '{}' not found", args.name)),
    };

    // Collect lines from both stdout and stderr buffers, merge by timestamp
    let stdout_lines = entry.stdout_buf.lines(None, args.stream);
    let stderr_lines = entry.stderr_buf.lines(None, args.stream);

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
        let table = root.process_table.lock().await;
        match table.get(&args.name) {
            Some(entry) if entry.state == ProcessState::Running => entry.pid,
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
        let table = root.process_table.lock().await;
        match table.get(&args.name) {
            Some(entry) => (
                entry.pid,
                entry.command.clone(),
                entry.env.clone(),
                entry.restart_policy,
            ),
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

    // Remove old entry and re-create
    {
        let mut table = root.process_table.lock().await;
        table.remove(&args.name);

        let mut entry = ProcessEntry::new(args.name.clone(), command, env, restart_policy, false);

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
    let mut lines = BufReader::new(reader).lines();

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

        let resp = handle_request(&root, req).await;
        let mut json = serde_json::to_string(&resp)?;
        json.push('\n');
        writer.write_all(json.as_bytes()).await?;
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
    let mut lines = BufReader::new(reader).lines();

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

        let resp = handle_request(&root, req).await;
        let mut json = serde_json::to_string(&resp)?;
        json.push('\n');
        writer.write_all(json.as_bytes()).await?;
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

        // Send existing tail lines first
        let stdout_lines = entry.stdout_buf.lines(None, args.stream);
        let stderr_lines = entry.stderr_buf.lines(None, args.stream);

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
