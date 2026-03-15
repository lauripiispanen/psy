use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{watch, Mutex};

use crate::platform;
use crate::process::{
    calculate_backoff, should_restart, spawn_child, spawn_child_attached, spawn_child_interactive,
    validate_name, ProcessEntry, ProcessState,
};
use crate::protocol::*;
use crate::psyfile::{self, Psyfile};
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
    /// Explicit Psyfile path (from `--file`), or `None` for discovery.
    pub psyfile_path: Option<PathBuf>,
    /// Working directory at startup, used for Psyfile discovery.
    pub cwd: PathBuf,
    pub template_counters: Mutex<HashMap<String, u32>>,
}

impl SharedRoot {
    /// Load and validate the Psyfile, re-reading from disk each time.
    /// Returns `Ok(None)` if no Psyfile is found.
    /// Returns `Err` if the file exists but has parse/validation errors.
    pub fn load_psyfile(&self) -> Result<Option<Psyfile>, String> {
        let path = if let Some(ref p) = self.psyfile_path {
            if p.is_file() {
                Some(p.clone())
            } else {
                return Err(format!("Psyfile not found: {}", p.display()));
            }
        } else {
            psyfile::discover(&self.cwd)
        };

        match path {
            Some(p) => {
                let pf = psyfile::parse(&p)?;
                psyfile::validate(&pf)?;
                Ok(Some(pf))
            }
            None => Ok(None),
        }
    }
}

// ---------------------------------------------------------------------------
// PsyRoot
// ---------------------------------------------------------------------------

pub struct PsyRoot {
    shared: Arc<SharedRoot>,
    main_exit_rx: watch::Receiver<Option<i32>>,
}

impl PsyRoot {
    pub fn new(_name: String, psyfile_path: Option<PathBuf>) -> RootResult<Self> {
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
            psyfile_path,
            cwd: std::env::current_dir().unwrap_or_default(),
            template_counters: Mutex::new(HashMap::new()),
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
    pub async fn run(
        mut self,
        main_command: Option<Vec<String>>,
        boot_units: Vec<String>,
    ) -> RootResult<i32> {
        // Determine the main command
        let main_cmd = main_command.unwrap_or_else(|| {
            #[cfg(unix)]
            let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
            #[cfg(windows)]
            let shell = std::env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".into());
            vec![shell]
        });

        // Spawn the socket listener first (so boot units can use the socket)
        {
            let root = Arc::clone(&self.shared);
            tokio::spawn(async move {
                if let Err(e) = run_socket_listener(root).await {
                    eprintln!("psy: socket listener error: {e}");
                }
            });
        }

        // Start boot units from Psyfile (before main process)
        if !boot_units.is_empty() {
            let pf = self
                .shared
                .load_psyfile()
                .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { e.into() })?;
            if let Some(ref pf) = pf {
                let start_order = psyfile::resolve_start_order(pf, &boot_units)
                    .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { e.into() })?;
                for unit_name in &start_order {
                    let req = Request::run(RunArgs {
                        name: unit_name.clone(),
                        command: vec![],
                        restart: RestartPolicy::No,
                        env: HashMap::new(),
                        attach: false,
                        interactive: false,
                        extra_args: None,
                    });
                    let result = handle_request(&self.shared, req).await;
                    if let HandleResult::Response(resp) = result {
                        if !resp.ok {
                            eprintln!(
                                "psy: failed to start unit '{unit_name}': {}",
                                resp.error.unwrap_or_default()
                            );
                        }
                    }
                }
            }
        }

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
async fn wait_for_exit_or_signal(rx: &mut tokio::sync::watch::Receiver<Option<i32>>) -> i32 {
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
async fn wait_for_exit_or_signal(rx: &mut tokio::sync::watch::Receiver<Option<i32>>) -> i32 {
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
        CMD_SEND => HandleResult::Response(handle_send(root, &req).await),
        CMD_SEND_WAIT => HandleResult::Response(handle_send_wait(root, &req).await),
        _ => HandleResult::Response(Response::err(
            &req.id,
            format!("unknown command: {}", req.cmd),
        )),
    }
}

fn handle_run<'a>(
    root: &'a Arc<SharedRoot>,
    req: &'a Request,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = HandleResult> + Send + 'a>> {
    Box::pin(handle_run_inner(root, req))
}

async fn handle_run_inner(root: &Arc<SharedRoot>, req: &Request) -> HandleResult {
    if root.shutting_down.load(Ordering::Relaxed) {
        return HandleResult::Response(Response::err(&req.id, "server is shutting down"));
    }

    let args: RunArgs = match req
        .args
        .as_ref()
        .and_then(|v| serde_json::from_value(v.clone()).ok())
    {
        Some(a) => a,
        None => {
            return HandleResult::Response(Response::err(&req.id, "invalid or missing run args"))
        }
    };

    if !validate_name(&args.name) {
        return HandleResult::Response(Response::err(
            &req.id,
            "invalid name: must match [a-zA-Z0-9][a-zA-Z0-9_-]{0,62}",
        ));
    }

    // Load Psyfile from disk (hot-reload)
    let psyfile = match root.load_psyfile() {
        Ok(pf) => pf,
        Err(e) => {
            // If the command has an explicit command, we can still run ad-hoc
            if !args.command.is_empty() {
                None
            } else {
                return HandleResult::Response(Response::err(
                    &req.id,
                    format!("Psyfile error: {e}"),
                ));
            }
        }
    };

    // Check if this name matches a Psyfile unit
    let has_unit = psyfile
        .as_ref()
        .map(|pf| pf.units.contains_key(&args.name))
        .unwrap_or(false);

    if has_unit {
        let pf = psyfile.as_ref().unwrap();
        let unit = &pf.units[&args.name];

        // Start dependencies first
        if !unit.depends_on.is_empty() {
            let dep_names = unit.dep_names();
            let dep_order = match psyfile::resolve_start_order(pf, &dep_names) {
                Ok(o) => o,
                Err(e) => {
                    return HandleResult::Response(Response::err(
                        &req.id,
                        format!("dependency error: {e}"),
                    ))
                }
            };
            for dep_name in &dep_order {
                let already_running = {
                    let table = root.process_table.lock().await;
                    if let Some(dep_unit) = pf.units.get(dep_name.as_str()) {
                        if dep_unit.singleton {
                            table
                                .get(dep_name.as_str())
                                .map(|e| e.state == ProcessState::Running)
                                .unwrap_or(false)
                        } else {
                            table.iter().any(|(n, e)| {
                                n.starts_with(&format!("{dep_name}."))
                                    && e.state == ProcessState::Running
                            })
                        }
                    } else {
                        false
                    }
                };

                if !already_running {
                    let dep_req = Request::run(RunArgs {
                        name: dep_name.clone(),
                        command: vec![],
                        restart: RestartPolicy::No,
                        env: HashMap::new(),
                        attach: false,
                        interactive: false,
                        extra_args: None,
                    });
                    let result = handle_run(root, &dep_req).await;
                    if let HandleResult::Response(ref resp) = result {
                        if !resp.ok {
                            return HandleResult::Response(Response::err(
                                &req.id,
                                format!(
                                    "failed to start dependency '{dep_name}': {}",
                                    resp.error.as_deref().unwrap_or("unknown")
                                ),
                            ));
                        }
                    }
                }

                // Wait for dependency readiness if it has a probe
                let wait_info = {
                    let table = root.process_table.lock().await;
                    if let Some(entry) = table.get(dep_name.as_str()) {
                        if !entry.ready && entry.ready_config.is_some() {
                            let timeout = entry
                                .ready_config
                                .as_ref()
                                .map(|c| c.timeout)
                                .unwrap_or(std::time::Duration::from_secs(30));
                            Some((Arc::clone(&entry.ready_notify), timeout))
                        } else {
                            None
                        }
                    } else {
                        None
                    }
                };

                if let Some((notify, timeout)) = wait_info {
                    match tokio::time::timeout(timeout, notify.notified()).await {
                        Ok(()) => { /* dep is ready */ }
                        Err(_) => {
                            return HandleResult::Response(Response::err(
                                &req.id,
                                format!("dependency '{}' readiness probe timed out", dep_name),
                            ));
                        }
                    }
                }
            }
        }

        // Build command string
        // When a Psyfile unit is found, the -- args are treated as extra args, not a command.
        let extra_owned: Vec<String> = if let Some(ref extra) = args.extra_args {
            extra.clone()
        } else if !args.command.is_empty() {
            // CLI sends -- args as command when extra_args is None
            args.command.clone()
        } else {
            vec![]
        };
        let cmd_str = psyfile::build_command_with_args(&unit.command, &extra_owned);

        // Merge env: unit.env + args.env (args override)
        let mut resolved_env = unit.env.clone();
        for (k, v) in &args.env {
            resolved_env.insert(k.clone(), v.clone());
        }

        // Interpolate env vars in command and env values
        let mut full_env: HashMap<String, String> = std::env::vars().collect();
        full_env.insert("PSY_SOCK".into(), root.psy_sock.clone());
        full_env.insert("PSY_ROOT_PID".into(), root.psy_root_pid.to_string());
        for (k, v) in &resolved_env {
            full_env.insert(k.clone(), v.clone());
        }

        let cmd_str = psyfile::interpolate(&cmd_str, &full_env);

        let resolved_env: HashMap<String, String> = resolved_env
            .into_iter()
            .map(|(k, v)| (k, psyfile::interpolate(&v, &full_env)))
            .collect();

        // Determine process name (singleton vs template)
        let (actual_name, instance_env) = if unit.singleton {
            (args.name.clone(), HashMap::new())
        } else {
            let mut counters = root.template_counters.lock().await;
            let n = counters.entry(args.name.clone()).or_insert(0);
            *n += 1;
            let instance = *n;
            let name = format!("{}.{}", args.name, instance);
            let mut ie = HashMap::new();
            ie.insert("PSY_INSTANCE".into(), instance.to_string());
            (name, ie)
        };

        // Resolve restart policy (CLI overrides Psyfile)
        let restart = if args.restart != RestartPolicy::No {
            args.restart
        } else {
            unit.restart
        };

        let shell_cmd = psyfile::build_shell_command(&cmd_str);

        let mut final_env = resolved_env;
        for (k, v) in instance_env {
            final_env.insert(k, v);
        }

        let working_dir = unit.working_dir.as_ref().map(|d| {
            let s = d.to_string_lossy().to_string();
            std::path::PathBuf::from(psyfile::interpolate(&s, &full_env))
        });

        let interactive = args.interactive || unit.interactive;
        return spawn_process(
            root,
            &req.id,
            actual_name,
            shell_cmd,
            final_env,
            restart,
            args.attach,
            interactive,
            working_dir,
            unit.ready.clone(),
            unit.healthcheck.clone(),
        )
        .await;
    }

    // --- Ad-hoc mode (existing behavior) ---
    if args.command.is_empty() {
        return HandleResult::Response(Response::err(
            &req.id,
            format!("no command provided for ad-hoc process '{}'", args.name),
        ));
    }

    spawn_process(
        root,
        &req.id,
        args.name.clone(),
        args.command.clone(),
        args.env.clone(),
        args.restart,
        args.attach,
        args.interactive,
        None,
        None,
        None,
    )
    .await
}

/// Common process spawning logic used by both Psyfile and ad-hoc modes.
#[allow(clippy::too_many_arguments)]
async fn spawn_process(
    root: &Arc<SharedRoot>,
    req_id: &str,
    name: String,
    command: Vec<String>,
    env: HashMap<String, String>,
    restart: RestartPolicy,
    attach: bool,
    interactive: bool,
    working_dir: Option<std::path::PathBuf>,
    ready_config: Option<crate::psyfile::ProbeConfig>,
    healthcheck_config: Option<crate::psyfile::ProbeConfig>,
) -> HandleResult {
    let mut table = root.process_table.lock().await;

    // Allow replacing stopped/failed tombstones; only reject if still running
    let old_history = if let Some(existing) = table.get_mut(&name) {
        if existing.state == ProcessState::Running {
            return HandleResult::Response(Response::err(
                req_id,
                format!("process '{}' is already running", name),
            ));
        }
        existing.archive_current_run();
        let history = std::mem::take(&mut existing.run_history);
        let next_id = existing.current_run_id;
        table.remove(&name);
        Some((history, next_id))
    } else {
        None
    };

    let mut entry = ProcessEntry::new(name.clone(), command, env, restart, false);
    entry.working_dir = working_dir;

    // Configure readiness probes
    let is_exit_probe = matches!(
        ready_config.as_ref().map(|c| &c.probe),
        Some(crate::psyfile::ProbeKind::Exit(_))
    );
    if ready_config.is_some() {
        entry.ready = false;
        entry.ready_config = ready_config.clone();
    }
    entry.healthcheck_config = healthcheck_config.clone();

    if let Some((history, next_id)) = old_history {
        entry.run_history = history;
        entry.current_run_id = next_id;
    }

    let child = if attach {
        match spawn_child_attached(&mut entry, &root.psy_sock, root.psy_root_pid) {
            Ok(c) => c,
            Err(e) => {
                return HandleResult::Response(Response::err(req_id, format!("spawn failed: {e}")))
            }
        }
    } else if interactive {
        match spawn_child_interactive(&mut entry, &root.psy_sock, root.psy_root_pid) {
            Ok(c) => c,
            Err(e) => {
                return HandleResult::Response(Response::err(req_id, format!("spawn failed: {e}")))
            }
        }
    } else {
        match spawn_child(&mut entry, &root.psy_sock, root.psy_root_pid) {
            Ok(c) => c,
            Err(e) => {
                return HandleResult::Response(Response::err(req_id, format!("spawn failed: {e}")))
            }
        }
    };

    entry.child = Some(child);

    // Set up probe cancellation channel
    let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
    entry.probe_cancel = Some(cancel_tx);

    let stdout_buf = Arc::clone(&entry.stdout_buf);
    let stderr_buf = Arc::clone(&entry.stderr_buf);

    table.insert(name.clone(), entry);
    drop(table);

    // Launch readiness probe task (not for exit probes — those are handled by monitor_child)
    if let Some(ref config) = ready_config {
        if !is_exit_probe {
            let pt = Arc::clone(&root.process_table);
            let probe_name = name.clone();
            let probe_config = config.clone();
            let probe_cancel = cancel_rx.clone();
            let probe_stdout = Arc::clone(&stdout_buf);
            let probe_stderr = Arc::clone(&stderr_buf);
            tokio::spawn(async move {
                crate::probe::run_ready_probe(
                    pt,
                    probe_name,
                    probe_config,
                    probe_stdout,
                    probe_stderr,
                    probe_cancel,
                )
                .await;
            });
        }
    }

    // Launch healthcheck task
    if let Some(ref config) = healthcheck_config {
        let pt = Arc::clone(&root.process_table);
        let hc_name = name.clone();
        let hc_config = config.clone();
        let hc_cancel = cancel_rx.clone();
        let hc_stdout = Arc::clone(&stdout_buf);
        let hc_stderr = Arc::clone(&stderr_buf);
        tokio::spawn(async move {
            crate::probe::run_healthcheck(pt, hc_name, hc_config, hc_stdout, hc_stderr, hc_cancel)
                .await;
        });
    }

    let root_clone = Arc::clone(root);
    let monitor_name = name.clone();
    tokio::spawn(async move {
        monitor_child(root_clone, monitor_name).await;
    });

    let response = Response::ok(
        req_id,
        Some(serde_json::json!({ "name": name, "status": "running" })),
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
    let mut args: LogsArgs = match req
        .args
        .as_ref()
        .and_then(|v| serde_json::from_value(v.clone()).ok())
    {
        Some(a) => a,
        None => return Response::err(&req.id, "invalid or missing logs args"),
    };

    // If probe flag is set, adjust stream filter to show probe streams
    if args.probe {
        args.stream = match args.stream {
            StreamFilter::Stdout => StreamFilter::ProbeStdout,
            StreamFilter::Stderr => StreamFilter::ProbeStderr,
            StreamFilter::ProbeStdout | StreamFilter::ProbeStderr | StreamFilter::Probe => {
                args.stream // already a probe filter
            }
            _ => StreamFilter::Probe,
        };
    }

    let since = args
        .since
        .as_deref()
        .map(|s| chrono::DateTime::parse_from_rfc3339(s).map(|dt| dt.with_timezone(&chrono::Utc)))
        .transpose();
    let since = match since {
        Ok(s) => s,
        Err(e) => return Response::err(&req.id, format!("invalid since timestamp: {e}")),
    };

    let until = args
        .until
        .as_deref()
        .map(|s| chrono::DateTime::parse_from_rfc3339(s).map(|dt| dt.with_timezone(&chrono::Utc)))
        .transpose();
    let until = match until {
        Ok(u) => u,
        Err(e) => return Response::err(&req.id, format!("invalid until timestamp: {e}")),
    };

    // Check if this is a template unit name — collect logs from all instances
    let is_template = root
        .load_psyfile()
        .ok()
        .flatten()
        .and_then(|pf| pf.units.get(&args.name).map(|u| !u.singleton))
        .unwrap_or(false);

    let table = root.process_table.lock().await;

    if is_template {
        let prefix = format!("{}.", args.name);
        let grep_ref = args.grep.as_deref();
        let mut all_lines = Vec::new();

        for (instance_name, entry) in table.iter() {
            if !instance_name.starts_with(&prefix) {
                continue;
            }
            let stdout_lines = entry
                .stdout_buf
                .lines(None, args.stream, since, until, grep_ref);
            let stderr_lines = entry
                .stderr_buf
                .lines(None, args.stream, since, until, grep_ref);
            for mut line in stdout_lines.into_iter().chain(stderr_lines.into_iter()) {
                // Prefix content with instance name
                line.content = format!("[{}] {}", instance_name, line.content);
                all_lines.push(line);
            }
        }

        all_lines.sort_by_key(|l| l.timestamp);

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

        return Response::ok(&req.id, Some(serde_json::json!({ "lines": lines_json })));
    }

    let entry = match table.get(&args.name) {
        Some(e) => e,
        None => return Response::err(&req.id, format!("process '{}' not found", args.name)),
    };

    // Resolve which run's buffers to use
    let (stdout_buf, stderr_buf) = if args.previous {
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
                Some(record) => (
                    Arc::clone(&record.stdout_buf),
                    Arc::clone(&record.stderr_buf),
                ),
                None => return Response::err(&req.id, format!("run {} not found", run_id)),
            }
        }
    } else {
        (Arc::clone(&entry.stdout_buf), Arc::clone(&entry.stderr_buf))
    };

    let grep_ref = args.grep.as_deref();
    let stdout_lines = stdout_buf.lines(None, args.stream, since, until, grep_ref);
    let stderr_lines = stderr_buf.lines(None, args.stream, since, until, grep_ref);

    let mut all_lines: Vec<_> = stdout_lines
        .into_iter()
        .chain(stderr_lines.into_iter())
        .collect();
    all_lines.sort_by_key(|l| l.timestamp);

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

    // Check if this is a template unit name (non-singleton) — stop all instances
    let is_template = root
        .load_psyfile()
        .ok()
        .flatten()
        .and_then(|pf| pf.units.get(&args.name).map(|u| !u.singleton))
        .unwrap_or(false);

    if is_template {
        let prefix = format!("{}.", args.name);
        let pids: Vec<(String, Option<u32>)> = {
            let mut table = root.process_table.lock().await;
            table
                .iter_mut()
                .filter(|(n, e)| n.starts_with(&prefix) && e.state == ProcessState::Running)
                .map(|(n, e)| {
                    e.stopping.store(true, Ordering::Relaxed);
                    (n.clone(), e.pid)
                })
                .collect()
        };

        for (_, pid) in &pids {
            if let Some(pid) = pid {
                let pid = *pid;
                tokio::task::spawn_blocking(move || {
                    platform::stop_process(pid, Duration::from_secs(10));
                })
                .await
                .ok();
            }
        }

        return Response::ok(
            &req.id,
            Some(
                serde_json::json!({ "name": args.name, "status": "stopped", "instances": pids.len() }),
            ),
        );
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

fn handle_restart<'a>(
    root: &'a Arc<SharedRoot>,
    req: &'a Request,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Response> + Send + 'a>> {
    Box::pin(handle_restart_inner(root, req))
}

async fn handle_restart_inner(root: &Arc<SharedRoot>, req: &Request) -> Response {
    let args: RestartArgs = match req
        .args
        .as_ref()
        .and_then(|v| serde_json::from_value(v.clone()).ok())
    {
        Some(a) => a,
        None => return Response::err(&req.id, "invalid or missing restart args"),
    };

    // Load Psyfile for template check and command re-resolution
    let psyfile = root.load_psyfile().ok().flatten();

    // Check if this is a template unit name — restart all instances
    let is_template = psyfile
        .as_ref()
        .and_then(|pf| pf.units.get(&args.name))
        .map(|u| !u.singleton)
        .unwrap_or(false);

    if is_template {
        let prefix = format!("{}.", args.name);
        let instances: Vec<String> = {
            let table = root.process_table.lock().await;
            table
                .keys()
                .filter(|n| n.starts_with(&prefix))
                .cloned()
                .collect()
        };

        for instance_name in &instances {
            let sub_req = Request::restart(RestartArgs {
                name: instance_name.clone(),
            });
            let _ = handle_restart(root, &sub_req).await;
        }

        return Response::ok(
            &req.id,
            Some(
                serde_json::json!({ "name": args.name, "status": "restarted", "instances": instances.len() }),
            ),
        );
    }

    // Determine the unit name for Psyfile lookup.
    // For template instances like "worker.3", strip the suffix to find "worker" in the Psyfile.
    let unit_name = args
        .name
        .rsplit_once('.')
        .map(|(base, _)| base)
        .unwrap_or(&args.name);
    let unit_def = psyfile.as_ref().and_then(|pf| pf.units.get(unit_name));

    // Get info needed to stop
    let (pid, old_command, old_env, old_restart, old_working_dir) = {
        let mut table = root.process_table.lock().await;
        match table.get_mut(&args.name) {
            Some(entry) => {
                entry.stopping.store(true, Ordering::Relaxed);
                (
                    entry.pid,
                    entry.command.clone(),
                    entry.env.clone(),
                    entry.restart_policy,
                    entry.working_dir.clone(),
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
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    // Re-resolve command from Psyfile if this is a unit (hot-reload)
    let (command, env, restart_policy, working_dir) = if let Some(unit) = unit_def {
        let cmd_str = psyfile::build_command_with_args(&unit.command, &[]);
        let mut full_env: HashMap<String, String> = std::env::vars().collect();
        full_env.insert("PSY_SOCK".into(), root.psy_sock.clone());
        full_env.insert("PSY_ROOT_PID".into(), root.psy_root_pid.to_string());
        for (k, v) in &unit.env {
            full_env.insert(k.clone(), v.clone());
        }
        let cmd_str = psyfile::interpolate(&cmd_str, &full_env);
        let resolved_env: HashMap<String, String> = unit
            .env
            .iter()
            .map(|(k, v)| (k.clone(), psyfile::interpolate(v, &full_env)))
            .collect();
        let shell_cmd = psyfile::build_shell_command(&cmd_str);
        let wd = unit.working_dir.as_ref().map(|d| {
            let s = d.to_string_lossy().to_string();
            std::path::PathBuf::from(psyfile::interpolate(&s, &full_env))
        });
        (shell_cmd, resolved_env, unit.restart, wd)
    } else {
        (old_command, old_env, old_restart, old_working_dir)
    };

    // Archive current run and re-create with fresh buffers
    {
        let mut table = root.process_table.lock().await;
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
        entry.working_dir = working_dir;

        // Configure readiness probes from Psyfile unit
        let ready_config = unit_def.and_then(|u| u.ready.clone());
        let healthcheck_config = unit_def.and_then(|u| u.healthcheck.clone());

        let is_exit_probe = matches!(
            ready_config.as_ref().map(|c| &c.probe),
            Some(crate::psyfile::ProbeKind::Exit(_))
        );
        if ready_config.is_some() {
            entry.ready = false;
            entry.ready_config = ready_config.clone();
        }
        entry.healthcheck_config = healthcheck_config.clone();

        let child = match spawn_child(&mut entry, &root.psy_sock, root.psy_root_pid) {
            Ok(c) => c,
            Err(e) => return Response::err(&req.id, format!("restart spawn failed: {e}")),
        };

        entry.child = Some(child);

        // Set up probe cancellation and launch probe tasks
        let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
        entry.probe_cancel = Some(cancel_tx);
        let stdout_buf = Arc::clone(&entry.stdout_buf);
        let stderr_buf = Arc::clone(&entry.stderr_buf);

        let name = args.name.clone();
        table.insert(name.clone(), entry);
        drop(table);

        // Launch readiness probe task
        if let Some(ref config) = ready_config {
            if !is_exit_probe {
                let pt = Arc::clone(&root.process_table);
                let probe_name = name.clone();
                let probe_config = config.clone();
                let probe_cancel = cancel_rx.clone();
                let probe_stdout = Arc::clone(&stdout_buf);
                let probe_stderr = Arc::clone(&stderr_buf);
                tokio::spawn(async move {
                    crate::probe::run_ready_probe(
                        pt,
                        probe_name,
                        probe_config,
                        probe_stdout,
                        probe_stderr,
                        probe_cancel,
                    )
                    .await;
                });
            }
        }

        // Launch healthcheck task
        if let Some(ref config) = healthcheck_config {
            let pt = Arc::clone(&root.process_table);
            let hc_name = name.clone();
            let hc_config = config.clone();
            let hc_cancel = cancel_rx.clone();
            let hc_stdout = Arc::clone(&stdout_buf);
            let hc_stderr = Arc::clone(&stderr_buf);
            tokio::spawn(async move {
                crate::probe::run_healthcheck(
                    pt, hc_name, hc_config, hc_stdout, hc_stderr, hc_cancel,
                )
                .await;
            });
        }

        let root_clone = Arc::clone(root);
        let monitor_name = name.clone();
        tokio::spawn(async move {
            monitor_child(root_clone, monitor_name).await;
        });

        // Trigger restart cascades
        let root_clone = Arc::clone(root);
        let cascade_name = name.clone();
        tokio::spawn(async move {
            cascade_restarts(&root_clone, &cascade_name).await;
        });
    }

    Response::ok(
        &req.id,
        Some(serde_json::json!({ "name": args.name, "status": "running" })),
    )
}

/// Cascade restarts to dependents that have `restart = true` in their dependency
/// on the restarted unit. Uses BFS to collect all transitive dependents, then
/// restarts each in dependency order. Each restart is done via a direct restart
/// call (no recursive cascade to avoid infinite loops).
async fn cascade_restarts(root: &Arc<SharedRoot>, restarted_name: &str) {
    use std::collections::{HashSet, VecDeque};

    let psyfile = match root.load_psyfile().ok().flatten() {
        Some(pf) => pf,
        None => return,
    };

    // Build reverse dep map: unit_name -> units that depend on it with restart=true
    let mut reverse_deps: HashMap<String, Vec<String>> = HashMap::new();
    for (name, unit) in &psyfile.units {
        for dep in &unit.depends_on {
            if dep.restart {
                reverse_deps
                    .entry(dep.name.clone())
                    .or_default()
                    .push(name.clone());
            }
        }
    }

    // BFS from restarted_name to collect all transitive dependents
    let mut to_restart = Vec::new();
    let mut visited = HashSet::new();
    let mut queue = VecDeque::new();

    if let Some(direct) = reverse_deps.get(restarted_name) {
        for d in direct {
            queue.push_back(d.clone());
        }
    }

    while let Some(name) = queue.pop_front() {
        if !visited.insert(name.clone()) {
            continue;
        }
        to_restart.push(name.clone());
        if let Some(transitive) = reverse_deps.get(&name) {
            for t in transitive {
                queue.push_back(t.clone());
            }
        }
    }

    if to_restart.is_empty() {
        return;
    }

    // Wait for the restarted upstream to be ready if it has a probe
    let wait_info = {
        let table = root.process_table.lock().await;
        if let Some(entry) = table.get(restarted_name) {
            if !entry.ready && entry.ready_config.is_some() {
                let timeout = entry
                    .ready_config
                    .as_ref()
                    .map(|c| c.timeout)
                    .unwrap_or(std::time::Duration::from_secs(30));
                Some((Arc::clone(&entry.ready_notify), timeout))
            } else {
                None
            }
        } else {
            None
        }
    };

    if let Some((notify, timeout)) = wait_info {
        if tokio::time::timeout(timeout, notify.notified())
            .await
            .is_err()
        {
            // Upstream readiness timed out — skip cascading
            return;
        }
    }

    // Restart each dependent
    for dep_name in &to_restart {
        let is_running = {
            let table = root.process_table.lock().await;
            table
                .get(dep_name)
                .map(|e| e.state == ProcessState::Running)
                .unwrap_or(false)
        };

        if is_running {
            // Use the Send-safe handle_restart wrapper
            let sub_req = Request::restart(RestartArgs {
                name: dep_name.clone(),
            });
            // handle_restart_inner calls cascade_restarts at the end,
            // which naturally handles transitive cascades.
            let _ = handle_restart(root, &sub_req).await;
        }
    }
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

    // Check for template unit — if the exact name isn't in the table but is a
    // template in the Psyfile, we can't show history for the group.
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

async fn handle_send(root: &Arc<SharedRoot>, req: &Request) -> Response {
    let args: SendArgs = match req
        .args
        .as_ref()
        .and_then(|v| serde_json::from_value(v.clone()).ok())
    {
        Some(a) => a,
        None => return Response::err(&req.id, "invalid or missing send args"),
    };

    let mut table = root.process_table.lock().await;
    let entry = match table.get_mut(&args.name) {
        Some(e) => e,
        None => return Response::err(&req.id, format!("process '{}' not found", args.name)),
    };

    if entry.state != ProcessState::Running {
        return Response::err(&req.id, format!("process '{}' is not running", args.name));
    }

    if !entry.interactive {
        return Response::err(
            &req.id,
            format!(
                "process '{}' was not started with interactive mode (use --interactive or interactive = true in Psyfile)",
                args.name
            ),
        );
    }

    if entry.stdin_closed {
        return Response::err(
            &req.id,
            format!("stdin for '{}' has been closed", args.name),
        );
    }

    // Check for conflict with attach session
    if entry.stdin_handle.is_none() && !args.eof {
        return Response::err(
            &req.id,
            format!(
                "process '{}' has an attached session, detach first",
                args.name
            ),
        );
    }

    if args.eof {
        // Close stdin
        entry.stdin_handle = None;
        entry.stdin_closed = true;
        return Response::ok(
            &req.id,
            Some(serde_json::json!({ "name": args.name, "stdin": "closed" })),
        );
    }

    if let Some(ref input) = args.input {
        if let Some(ref mut stdin) = entry.stdin_handle {
            use tokio::io::AsyncWriteExt;
            match tokio::time::timeout(Duration::from_secs(5), async {
                stdin.write_all(input.as_bytes()).await?;
                stdin.flush().await
            })
            .await
            {
                Ok(Ok(())) => Response::ok(
                    &req.id,
                    Some(serde_json::json!({ "name": args.name, "bytes_written": input.len() })),
                ),
                Ok(Err(e)) => Response::err(&req.id, format!("write to stdin failed: {e}")),
                Err(_) => Response::err(&req.id, "write to stdin timed out (pipe buffer full?)"),
            }
        } else {
            Response::err(&req.id, format!("no stdin handle for '{}'", args.name))
        }
    } else {
        Response::err(&req.id, "either 'input' or 'eof' must be provided")
    }
}

// ---------------------------------------------------------------------------
// Send-and-wait
// ---------------------------------------------------------------------------

async fn handle_send_wait(root: &Arc<SharedRoot>, req: &Request) -> Response {
    let args: SendWaitArgs = match req
        .args
        .as_ref()
        .and_then(|v| serde_json::from_value(v.clone()).ok())
    {
        Some(a) => a,
        None => return Response::err(&req.id, "invalid or missing send_wait args"),
    };

    // Parse timeouts
    let timeout = match args.timeout.as_deref() {
        Some(s) => match crate::psyfile::parse_duration(s) {
            Ok(d) => d,
            Err(e) => return Response::err(&req.id, format!("invalid timeout: {e}")),
        },
        None => Duration::from_secs(5),
    };
    let idle_timeout = match args.idle_timeout.as_deref() {
        Some(s) => match crate::psyfile::parse_duration(s) {
            Ok(d) => d,
            Err(e) => return Response::err(&req.id, format!("invalid idle_timeout: {e}")),
        },
        None => Duration::from_millis(200),
    };

    // Lock table, validate, subscribe, write, then release lock
    let (mut stdout_rx, mut stderr_rx) = {
        let mut table = root.process_table.lock().await;
        let entry = match table.get_mut(&args.name) {
            Some(e) => e,
            None => return Response::err(&req.id, format!("process '{}' not found", args.name)),
        };

        if entry.state != ProcessState::Running {
            return Response::err(&req.id, format!("process '{}' is not running", args.name));
        }

        if !entry.interactive {
            return Response::err(
                &req.id,
                format!(
                    "process '{}' was not started with interactive mode",
                    args.name
                ),
            );
        }

        if entry.stdin_closed {
            return Response::err(
                &req.id,
                format!("stdin for '{}' has been closed", args.name),
            );
        }

        // Subscribe to output before writing so we don't miss lines
        let stdout_rx = entry.stdout_buf.subscribe();
        let stderr_rx = entry.stderr_buf.subscribe();

        // Write input + newline to stdin
        let input = format!("{}\n", args.input);
        if let Some(ref mut stdin) = entry.stdin_handle {
            use tokio::io::AsyncWriteExt;
            match tokio::time::timeout(Duration::from_secs(5), async {
                stdin.write_all(input.as_bytes()).await?;
                stdin.flush().await
            })
            .await
            {
                Ok(Ok(())) => {}
                Ok(Err(e)) => return Response::err(&req.id, format!("write to stdin failed: {e}")),
                Err(_) => {
                    return Response::err(&req.id, "write to stdin timed out (pipe buffer full?)")
                }
            }
        } else {
            return Response::err(&req.id, format!("no stdin handle for '{}'", args.name));
        }

        (stdout_rx, stderr_rx)
    };
    // Lock released — now collect output

    let prompt_lower = args.prompt.as_ref().map(|p| p.to_lowercase());
    let mut lines: Vec<String> = Vec::new();
    let mut matched_prompt = false;
    let deadline = tokio::time::Instant::now() + timeout;

    loop {
        let idle_sleep = tokio::time::sleep(idle_timeout);
        let overall_sleep = tokio::time::sleep_until(deadline);

        tokio::select! {
            result = stdout_rx.recv() => {
                match result {
                    Ok(log_line) => {
                        if !log_line.stream.is_probe() {
                            let content = log_line.content.clone();
                            if let Some(ref pat) = prompt_lower {
                                if content.to_lowercase().contains(pat) {
                                    matched_prompt = true;
                                    lines.push(content);
                                    break;
                                }
                            }
                            lines.push(content);
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                }
            }
            result = stderr_rx.recv() => {
                match result {
                    Ok(log_line) => {
                        if !log_line.stream.is_probe() {
                            let content = log_line.content.clone();
                            if let Some(ref pat) = prompt_lower {
                                if content.to_lowercase().contains(pat) {
                                    matched_prompt = true;
                                    lines.push(content);
                                    break;
                                }
                            }
                            lines.push(content);
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                }
            }
            _ = idle_sleep => {
                // No output for idle_timeout — done
                break;
            }
            _ = overall_sleep => {
                // Overall timeout — done
                break;
            }
        }
    }

    Response::ok(
        &req.id,
        Some(serde_json::json!({
            "name": args.name,
            "lines": lines,
            "matched_prompt": matched_prompt,
        })),
    )
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
        // holding the lock. Also record the run_id so we can detect if the
        // entry was replaced (e.g. by a restart) while we were waiting.
        let (child_handle, run_id, kill_notify) = {
            let mut table = root.process_table.lock().await;
            let entry = match table.get_mut(&name) {
                Some(e) if e.state == ProcessState::Running => e,
                _ => return,
            };
            let rid = entry.current_run_id;
            let kn = Arc::clone(&entry.kill_notify);
            match entry.child.take() {
                Some(c) => (c, rid, kn),
                None => return,
            }
        };

        // Wait for exit or a kill request from the healthcheck probe.
        // We hold the child handle here — the healthcheck signals via
        // kill_notify and we perform the actual kill.
        let mut child_handle = child_handle;
        let status = tokio::select! {
            s = child_handle.wait() => s,
            _ = kill_notify.notified() => {
                let _ = child_handle.kill().await;
                child_handle.wait().await
            }
        };

        let exit_code = status.as_ref().ok().and_then(|s| s.code());

        // Update entry state
        let (is_main, do_restart, backoff) = {
            let mut table = root.process_table.lock().await;
            let entry = match table.get_mut(&name) {
                Some(e) => e,
                None => return,
            };

            // If the entry was replaced (e.g. by handle_restart), bail out —
            // a new monitor_child is already watching the new process.
            if entry.current_run_id != run_id {
                return;
            }

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

            // Cancel any running probe tasks
            if let Some(ref cancel) = entry.probe_cancel {
                let _ = cancel.send(true);
            }

            // Handle exit readiness probe
            if let Some(ref config) = entry.ready_config {
                if let crate::psyfile::ProbeKind::Exit(expected_code) = config.probe {
                    if exit_code == Some(expected_code) {
                        entry.ready = true;
                        entry.ready_notify.notify_waiters();
                    } else {
                        entry.ready_failed = true;
                    }
                }
            }

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

            // Reset readiness state for new run
            if entry.ready_config.is_some() {
                entry.ready = false;
                entry.ready_failed = false;
            }

            match spawn_child(entry, &root.psy_sock, root.psy_root_pid) {
                Ok(child) => {
                    entry.child = Some(child);

                    // Relaunch probe tasks
                    let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
                    entry.probe_cancel = Some(cancel_tx);
                    let stdout_buf = Arc::clone(&entry.stdout_buf);
                    let stderr_buf = Arc::clone(&entry.stderr_buf);

                    let is_exit_probe = matches!(
                        entry.ready_config.as_ref().map(|c| &c.probe),
                        Some(crate::psyfile::ProbeKind::Exit(_))
                    );

                    if let Some(ref config) = entry.ready_config.clone() {
                        if !is_exit_probe {
                            let pt = Arc::clone(&root.process_table);
                            let probe_name = name.clone();
                            let probe_config = config.clone();
                            let probe_cancel = cancel_rx.clone();
                            let probe_stdout = Arc::clone(&stdout_buf);
                            let probe_stderr = Arc::clone(&stderr_buf);
                            tokio::spawn(async move {
                                crate::probe::run_ready_probe(
                                    pt,
                                    probe_name,
                                    probe_config,
                                    probe_stdout,
                                    probe_stderr,
                                    probe_cancel,
                                )
                                .await;
                            });
                        }
                    }

                    if let Some(ref config) = entry.healthcheck_config.clone() {
                        let pt = Arc::clone(&root.process_table);
                        let hc_name = name.clone();
                        let hc_config = config.clone();
                        let hc_cancel = cancel_rx.clone();
                        let hc_stdout = Arc::clone(&stdout_buf);
                        let hc_stderr = Arc::clone(&stderr_buf);
                        tokio::spawn(async move {
                            crate::probe::run_healthcheck(
                                pt, hc_name, hc_config, hc_stdout, hc_stderr, hc_cancel,
                            )
                            .await;
                        });
                    }

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
        let stdout_lines = entry
            .stdout_buf
            .lines(None, args.stream, since, None, grep_ref);
        let stderr_lines = entry
            .stderr_buf
            .lines(None, args.stream, since, None, grep_ref);

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
            let stream_kind = StreamKind::from(line.stream);
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
            StreamFilter::All => !log_line.stream.is_probe(),
            StreamFilter::Stdout => log_line.stream == RBStream::Stdout,
            StreamFilter::Stderr => log_line.stream == RBStream::Stderr,
            StreamFilter::Probe => log_line.stream.is_probe(),
            StreamFilter::ProbeStdout => log_line.stream == RBStream::ProbeStdout,
            StreamFilter::ProbeStderr => log_line.stream == RBStream::ProbeStderr,
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

        let stream_kind = StreamKind::from(log_line.stream);

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
                        let stream_kind = StreamKind::from(log_line.stream);
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
                        let stream_kind = StreamKind::from(log_line.stream);
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
