//! MCP (Model Context Protocol) JSON-RPC 2.0 server over stdin/stdout.
//!
//! On startup it connects to the psy root via PSY_SOCK so it can relay
//! tool calls as psy protocol commands.

use std::collections::HashMap;
use std::io::{self, BufRead, Write};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::client;
use crate::protocol::{
    HistoryArgs, HistoryResponse, LogsArgs, PsResponse, Request, RestartArgs, RestartPolicy,
    RunArgs, SendArgs, SendWaitArgs, StopArgs, StreamFilter,
};

// ---------------------------------------------------------------------------
// JSON-RPC types
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct JsonRpcRequest {
    #[allow(dead_code)]
    jsonrpc: String,
    #[serde(default)]
    id: Option<Value>,
    method: String,
    #[serde(default)]
    params: Option<Value>,
}

#[derive(Debug, Serialize)]
struct JsonRpcResponse {
    jsonrpc: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<JsonRpcError>,
}

#[derive(Debug, Serialize)]
struct JsonRpcError {
    code: i64,
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    data: Option<Value>,
}

impl JsonRpcResponse {
    fn success(id: Option<Value>, result: Value) -> Self {
        Self {
            jsonrpc: "2.0".into(),
            id,
            result: Some(result),
            error: None,
        }
    }

    fn error(id: Option<Value>, code: i64, message: impl Into<String>) -> Self {
        Self {
            jsonrpc: "2.0".into(),
            id,
            result: None,
            error: Some(JsonRpcError {
                code,
                message: message.into(),
                data: None,
            }),
        }
    }
}

// ---------------------------------------------------------------------------
// Tool schema definitions
// ---------------------------------------------------------------------------

fn tool_schemas() -> Value {
    json!({
        "tools": [
            {
                "name": "psy_run",
                "description": "Launch a named process. If name matches a Psyfile unit, starts that unit (with optional extra args). Otherwise, command is required.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "name": {
                            "type": "string",
                            "description": "Process name (or Psyfile unit name)"
                        },
                        "command": {
                            "type": "array",
                            "items": { "type": "string" },
                            "description": "Command and arguments. Required if name is not a Psyfile unit."
                        },
                        "args": {
                            "type": "array",
                            "items": { "type": "string" },
                            "description": "Extra arguments for Psyfile unit commands"
                        },
                        "restart": {
                            "type": "string",
                            "enum": ["no", "on_failure", "always"],
                            "description": "Restart policy (default: no)"
                        },
                        "env": {
                            "type": "object",
                            "additionalProperties": { "type": "string" },
                            "description": "Additional environment variables"
                        },
                        "interactive": {
                            "type": "boolean",
                            "description": "Enable stdin pipe (writable via psy_send)"
                        },
                        "wait_for": {
                            "description": "Block until a condition is met before returning. Use \"ready\" to wait for the ready probe, \"exit\" to wait for process exit (returns exit code + logs), or an object like {\"log\": \"pattern\"} to wait for a log line matching a substring, or {\"dependency\": \"name\"} to wait for a dependency's ready probe.",
                            "oneOf": [
                                { "type": "string", "enum": ["ready", "exit"] },
                                {
                                    "type": "object",
                                    "properties": {
                                        "log": { "type": "string", "description": "Case-insensitive substring to match in log output" }
                                    },
                                    "required": ["log"]
                                },
                                {
                                    "type": "object",
                                    "properties": {
                                        "dependency": { "type": "string", "description": "Dependency name to wait for readiness" }
                                    },
                                    "required": ["dependency"]
                                }
                            ]
                        },
                        "timeout": {
                            "type": "string",
                            "description": "Timeout for wait_for (e.g. '30s', '2m', '120s'). Default: 120s"
                        }
                    },
                    "required": ["name"]
                }
            },
            {
                "name": "psy_ps",
                "description": "List all managed processes and their status",
                "inputSchema": {
                    "type": "object",
                    "properties": {},
                    "required": []
                }
            },
            {
                "name": "psy_logs",
                "description": "Retrieve recent log output from a managed process",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "name": {
                            "type": "string",
                            "description": "Process name"
                        },
                        "tail": {
                            "type": "integer",
                            "description": "Number of lines to return (default: 50)"
                        },
                        "stream": {
                            "type": "string",
                            "enum": ["all", "stdout", "stderr"],
                            "description": "Which output stream to show (default: all)"
                        },
                        "since": {
                            "type": "string",
                            "description": "Show logs since this RFC 3339 timestamp (e.g. 2026-03-12T20:00:00Z), or 'last' to show only new logs since the previous logs request"
                        },
                        "until": {
                            "type": "string",
                            "description": "Show logs until this RFC 3339 timestamp"
                        },
                        "grep": {
                            "type": "string",
                            "description": "Filter logs by regex pattern (case-insensitive)"
                        },
                        "run": {
                            "type": "integer",
                            "description": "Show logs from a specific run ID (see psy_history)"
                        },
                        "previous": {
                            "type": "boolean",
                            "description": "Show logs from the previous run (before the current one)"
                        },
                        "probe": {
                            "type": "boolean",
                            "description": "Show probe logs instead of process output (default: false)"
                        },
                        "format": {
                            "type": "string",
                            "enum": ["lines", "structured"],
                            "description": "Output format: 'lines' returns plain text (default), 'structured' returns JSON objects with timestamp/stream/content"
                        }
                    },
                    "required": ["name"]
                }
            },
            {
                "name": "psy_stop",
                "description": "Stop a running managed process",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "name": {
                            "type": "string",
                            "description": "Process name to stop"
                        }
                    },
                    "required": ["name"]
                }
            },
            {
                "name": "psy_restart",
                "description": "Restart a managed process",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "name": {
                            "type": "string",
                            "description": "Process name to restart"
                        }
                    },
                    "required": ["name"]
                }
            },
            {
                "name": "psy_history",
                "description": "Show run history for a managed process. Use this to check if a process has been crashing repeatedly, and to find run IDs for querying past logs.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "name": {
                            "type": "string",
                            "description": "Process name"
                        }
                    },
                    "required": ["name"]
                }
            },
            {
                "name": "psy_send",
                "description": "Write to a process's stdin (must be started with interactive mode). Use wait: true to block until output is collected (ideal for REPL interactions).",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "name": {
                            "type": "string",
                            "description": "Process name"
                        },
                        "input": {
                            "type": "string",
                            "description": "Text to send (newline appended automatically)"
                        },
                        "eof": {
                            "type": "boolean",
                            "description": "Close stdin (for programs that read to EOF)"
                        },
                        "wait": {
                            "type": "boolean",
                            "description": "Wait for output after sending and return collected lines (default: false)"
                        },
                        "wait_timeout": {
                            "type": "string",
                            "description": "Overall timeout for wait mode (e.g. '5s', '200ms'). Default: '5s'"
                        },
                        "idle_timeout": {
                            "type": "string",
                            "description": "Idle timeout — stop collecting after this long with no new output (e.g. '200ms'). Default: '200ms'"
                        },
                        "wait_prompt": {
                            "type": "string",
                            "description": "Return early when output contains this substring (case-insensitive)"
                        }
                    },
                    "required": ["name"]
                }
            },
            {
                "name": "psy_psyfile_schema",
                "description": "Return the JSON Schema for the Psyfile format. Use this to discover available fields for defining process units.",
                "inputSchema": {
                    "type": "object",
                    "properties": {},
                    "required": []
                }
            },
            {
                "name": "psy_clean",
                "description": "Remove all stopped and failed processes from the process table. Useful for cleaning up after many test runs.",
                "inputSchema": {
                    "type": "object",
                    "properties": {},
                    "required": []
                }
            }
        ]
    })
}

// ---------------------------------------------------------------------------
// Tool call dispatch
// ---------------------------------------------------------------------------

fn handle_tool_call(tool_name: &str, args: &Value) -> Result<Value, String> {
    match tool_name {
        "psy_run" => {
            let name = args
                .get("name")
                .and_then(|v| v.as_str())
                .ok_or("missing required parameter: name")?
                .to_string();
            let command: Vec<String> = args
                .get("command")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default();
            let extra_args: Option<Vec<String>> =
                args.get("args").and_then(|v| v.as_array()).map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str().map(String::from))
                        .collect()
                });
            let restart = match args.get("restart").and_then(|v| v.as_str()) {
                Some("on_failure") => RestartPolicy::OnFailure,
                Some("always") => RestartPolicy::Always,
                _ => RestartPolicy::No,
            };
            let env: HashMap<String, String> = args
                .get("env")
                .and_then(|v| v.as_object())
                .map(|obj| {
                    obj.iter()
                        .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                        .collect()
                })
                .unwrap_or_default();

            let interactive = args
                .get("interactive")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);

            let wait_for = match args.get("wait_for") {
                Some(Value::String(s)) => match s.as_str() {
                    "ready" => Some(crate::protocol::WaitFor::Ready),
                    "exit" => Some(crate::protocol::WaitFor::Exit),
                    _ => return Err(format!("invalid wait_for value: {s}")),
                },
                Some(obj) if obj.is_object() => {
                    if let Some(pattern) = obj.get("log").and_then(|v| v.as_str()) {
                        Some(crate::protocol::WaitFor::Log {
                            pattern: pattern.to_string(),
                        })
                    } else if let Some(dep) = obj.get("dependency").and_then(|v| v.as_str()) {
                        Some(crate::protocol::WaitFor::Dependency {
                            name: dep.to_string(),
                        })
                    } else {
                        return Err("invalid wait_for object: expected {\"log\": \"...\"} or {\"dependency\": \"...\"}".into());
                    }
                }
                None => None,
                _ => return Err("invalid wait_for: expected string or object".into()),
            };

            let wait_timeout = args
                .get("timeout")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());

            let req = Request::run(RunArgs {
                name,
                command,
                restart,
                env,
                attach: false,
                interactive,
                extra_args,
                wait_for,
                wait_timeout,
            });
            let resp = client::send_command(req).map_err(|e| e.to_string())?;
            if resp.ok {
                Ok(json!({
                    "type": "text",
                    "text": serde_json::to_string_pretty(&resp.data).unwrap_or_default()
                }))
            } else {
                Err(resp.error.unwrap_or_else(|| "unknown error".into()))
            }
        }

        "psy_ps" => {
            let req = Request::ps();
            let resp = client::send_command(req).map_err(|e| e.to_string())?;
            if resp.ok {
                let text = if let Some(data) = &resp.data {
                    if let Ok(ps) = serde_json::from_value::<PsResponse>(data.clone()) {
                        format_ps_table(&ps)
                    } else {
                        serde_json::to_string_pretty(data).unwrap_or_default()
                    }
                } else {
                    "No processes".to_string()
                };
                Ok(json!({ "type": "text", "text": text }))
            } else {
                Err(resp.error.unwrap_or_else(|| "unknown error".into()))
            }
        }

        "psy_logs" => {
            let name = args
                .get("name")
                .and_then(|v| v.as_str())
                .ok_or("missing required parameter: name")?
                .to_string();
            let tail = args
                .get("tail")
                .and_then(|v| v.as_u64())
                .map(|n| n as usize)
                .or(Some(50));
            let stream = match args.get("stream").and_then(|v| v.as_str()) {
                Some("stdout") => StreamFilter::Stdout,
                Some("stderr") => StreamFilter::Stderr,
                _ => StreamFilter::All,
            };
            let since = args
                .get("since")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            let until = args
                .get("until")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            let grep = args
                .get("grep")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            let run = args.get("run").and_then(|v| v.as_u64()).map(|n| n as u32);
            let previous = args
                .get("previous")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let probe = args.get("probe").and_then(|v| v.as_bool()).unwrap_or(false);
            let format = args
                .get("format")
                .and_then(|v| v.as_str())
                .unwrap_or("lines");
            let req = Request::logs(LogsArgs {
                name,
                tail,
                stream,
                since,
                until,
                grep,
                run,
                previous,
                probe,
            });
            let resp = client::send_command(req).map_err(|e| e.to_string())?;
            if resp.ok {
                let text = match format {
                    "structured" => resp
                        .data
                        .map(|d| serde_json::to_string_pretty(&d).unwrap_or_default())
                        .unwrap_or_else(|| "(no output)".into()),
                    _ => {
                        // "lines" format: extract content from each log line
                        resp.data
                            .and_then(|d| d.get("lines").cloned())
                            .and_then(|v| v.as_array().cloned())
                            .map(|arr| {
                                arr.iter()
                                    .filter_map(|line| {
                                        line.get("content")
                                            .and_then(|c| c.as_str())
                                            .map(String::from)
                                    })
                                    .collect::<Vec<_>>()
                                    .join("\n")
                            })
                            .unwrap_or_else(|| "(no output)".into())
                    }
                };
                Ok(json!({ "type": "text", "text": text }))
            } else {
                Err(resp.error.unwrap_or_else(|| "unknown error".into()))
            }
        }

        "psy_stop" => {
            let name = args
                .get("name")
                .and_then(|v| v.as_str())
                .ok_or("missing required parameter: name")?
                .to_string();
            let req = Request::stop(StopArgs { name });
            let resp = client::send_command(req).map_err(|e| e.to_string())?;
            if resp.ok {
                Ok(json!({ "type": "text", "text": "stopped" }))
            } else {
                Err(resp.error.unwrap_or_else(|| "unknown error".into()))
            }
        }

        "psy_history" => {
            let name = args
                .get("name")
                .and_then(|v| v.as_str())
                .ok_or("missing required parameter: name")?
                .to_string();
            let req = Request::history(HistoryArgs { name });
            let resp = client::send_command(req).map_err(|e| e.to_string())?;
            if resp.ok {
                let text = if let Some(data) = &resp.data {
                    if let Ok(history) = serde_json::from_value::<HistoryResponse>(data.clone()) {
                        format_history_table(&history)
                    } else {
                        serde_json::to_string_pretty(data).unwrap_or_default()
                    }
                } else {
                    "No history".to_string()
                };
                Ok(json!({ "type": "text", "text": text }))
            } else {
                Err(resp.error.unwrap_or_else(|| "unknown error".into()))
            }
        }

        "psy_restart" => {
            let name = args
                .get("name")
                .and_then(|v| v.as_str())
                .ok_or("missing required parameter: name")?
                .to_string();
            let req = Request::restart(RestartArgs { name });
            let resp = client::send_command(req).map_err(|e| e.to_string())?;
            if resp.ok {
                Ok(json!({ "type": "text", "text": "restarted" }))
            } else {
                Err(resp.error.unwrap_or_else(|| "unknown error".into()))
            }
        }

        "psy_send" => {
            let name = args
                .get("name")
                .and_then(|v| v.as_str())
                .ok_or("missing required parameter: name")?
                .to_string();
            let wait = args.get("wait").and_then(|v| v.as_bool()).unwrap_or(false);

            if wait {
                let input = args
                    .get("input")
                    .and_then(|v| v.as_str())
                    .ok_or("missing required parameter: input (required for wait mode)")?
                    .to_string();
                let timeout = args
                    .get("wait_timeout")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
                let idle_timeout = args
                    .get("idle_timeout")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
                let prompt = args
                    .get("wait_prompt")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
                let req = Request::send_wait(SendWaitArgs {
                    name,
                    input,
                    timeout,
                    idle_timeout,
                    prompt,
                });
                let resp = client::send_command(req).map_err(|e| e.to_string())?;
                if resp.ok {
                    // Return lines as plain text
                    let text = resp
                        .data
                        .as_ref()
                        .and_then(|d| d.get("lines"))
                        .and_then(|v| v.as_array())
                        .map(|arr| {
                            arr.iter()
                                .filter_map(|v| v.as_str())
                                .collect::<Vec<_>>()
                                .join("\n")
                        })
                        .unwrap_or_default();
                    Ok(json!({ "type": "text", "text": text }))
                } else {
                    Err(resp.error.unwrap_or_else(|| "unknown error".into()))
                }
            } else {
                let eof = args.get("eof").and_then(|v| v.as_bool()).unwrap_or(false);
                let input = if eof {
                    None
                } else {
                    let text = args
                        .get("input")
                        .and_then(|v| v.as_str())
                        .ok_or("missing required parameter: input (or set eof: true)")?;
                    // Auto-append newline like CLI does
                    Some(format!("{text}\n"))
                };
                let req = Request::send(SendArgs { name, input, eof });
                let resp = client::send_command(req).map_err(|e| e.to_string())?;
                if resp.ok {
                    Ok(json!({
                        "type": "text",
                        "text": serde_json::to_string_pretty(&resp.data).unwrap_or_default()
                    }))
                } else {
                    Err(resp.error.unwrap_or_else(|| "unknown error".into()))
                }
            }
        }

        "psy_psyfile_schema" => {
            let schema = crate::psyfile::json_schema();
            let text = serde_json::to_string_pretty(&schema).unwrap_or_default();
            Ok(json!({ "type": "text", "text": text }))
        }

        "psy_clean" => {
            let req = Request::clean();
            let resp = client::send_command(req).map_err(|e| e.to_string())?;
            if resp.ok {
                let removed = resp
                    .data
                    .as_ref()
                    .and_then(|d| d.get("removed"))
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0);
                Ok(
                    json!({ "type": "text", "text": format!("removed {removed} stopped process(es)") }),
                )
            } else {
                Err(resp.error.unwrap_or_else(|| "unknown error".into()))
            }
        }

        _ => Err(format!("unknown tool: {tool_name}")),
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn format_ps_table(ps: &PsResponse) -> String {
    if ps.processes.is_empty() {
        return "No processes running".to_string();
    }
    let mut out = String::new();
    out.push_str(&format!(
        "{:<20} {:<8} {:<10} {:<8} {:<8} {:<14} {:<10} {}\n",
        "NAME", "PID", "STATUS", "READY", "EXIT", "UPTIME", "RESTARTS", "RESTART"
    ));
    out.push_str(&"-".repeat(86));
    out.push('\n');
    for p in &ps.processes {
        let pid_str = p.pid.map(|p| p.to_string()).unwrap_or_else(|| "-".into());
        let ready_str = p.ready.as_deref().unwrap_or("-");
        let exit_str = if let Some(sig) = &p.signal {
            sig.clone()
        } else {
            p.exit_code
                .map(|c| c.to_string())
                .unwrap_or_else(|| "-".into())
        };
        let uptime = p
            .uptime_secs
            .map(format_uptime)
            .unwrap_or_else(|| "-".into());
        let restart = format!("{:?}", p.restart_policy).to_lowercase();
        out.push_str(&format!(
            "{:<20} {:<8} {:<10} {:<8} {:<8} {:<14} {:<10} {}\n",
            p.name, pid_str, p.status, ready_str, exit_str, uptime, p.restarts, restart
        ));
    }
    out
}

fn format_history_table(history: &HistoryResponse) -> String {
    if history.runs.is_empty() {
        return format!("No runs recorded for '{}'", history.name);
    }
    let mut out = String::new();
    out.push_str(&format!(
        "{:<6} {:<10} {:<8} {:<28} {}\n",
        "RUN", "STATUS", "EXIT", "STARTED", "DURATION"
    ));
    out.push_str(&"-".repeat(68));
    out.push('\n');
    for r in &history.runs {
        let exit_str = if let Some(sig) = &r.signal {
            sig.clone()
        } else {
            r.exit_code
                .map(|c| c.to_string())
                .unwrap_or_else(|| "-".into())
        };
        let started = r.started_at.as_deref().unwrap_or("-");
        let duration = r
            .duration_secs
            .map(format_uptime)
            .unwrap_or_else(|| "-".into());
        out.push_str(&format!(
            "{:<6} {:<10} {:<8} {:<28} {}\n",
            r.run_id, r.status, exit_str, started, duration
        ));
    }
    out
}

fn format_uptime(secs: u64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m {}s", secs / 60, secs % 60)
    } else {
        format!("{}h {}m {}s", secs / 3600, (secs % 3600) / 60, secs % 60)
    }
}

// ---------------------------------------------------------------------------
// Main loop
// ---------------------------------------------------------------------------

/// Run the MCP server, reading JSON-RPC requests from stdin and writing
/// responses to stdout.
pub fn run() -> Result<(), String> {
    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut out = stdout.lock();

    let reader = stdin.lock();

    for line_result in reader.lines() {
        let line = match line_result {
            Ok(l) => l,
            Err(_) => break,
        };

        if line.trim().is_empty() {
            continue;
        }

        let req: JsonRpcRequest = match serde_json::from_str(&line) {
            Ok(r) => r,
            Err(e) => {
                let resp = JsonRpcResponse::error(None, -32700, format!("Parse error: {e}"));
                write_response(&mut out, &resp);
                continue;
            }
        };

        let resp = dispatch(&req);
        if let Some(resp) = resp {
            write_response(&mut out, &resp);
        }
    }

    Ok(())
}

fn write_response(out: &mut impl Write, resp: &JsonRpcResponse) {
    if let Ok(json) = serde_json::to_string(resp) {
        let _ = writeln!(out, "{json}");
        let _ = out.flush();
    }
}

fn dispatch(req: &JsonRpcRequest) -> Option<JsonRpcResponse> {
    match req.method.as_str() {
        "initialize" => {
            let result = json!({
                "protocolVersion": "2024-11-05",
                "serverInfo": {
                    "name": "psy",
                    "version": env!("CARGO_PKG_VERSION")
                },
                "capabilities": {
                    "tools": {}
                }
            });
            Some(JsonRpcResponse::success(req.id.clone(), result))
        }

        // `initialized` is a notification (no id) -- no response required.
        "notifications/initialized" | "initialized" => None,

        "tools/list" => {
            let schemas = tool_schemas();
            Some(JsonRpcResponse::success(req.id.clone(), schemas))
        }

        "tools/call" => {
            let params = req.params.as_ref();
            let tool_name = params
                .and_then(|p| p.get("name"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let arguments = params
                .and_then(|p| p.get("arguments"))
                .cloned()
                .unwrap_or(json!({}));

            match handle_tool_call(tool_name, &arguments) {
                Ok(content) => {
                    let result = json!({
                        "content": [content],
                        "isError": false
                    });
                    Some(JsonRpcResponse::success(req.id.clone(), result))
                }
                Err(e) => {
                    let result = json!({
                        "content": [{
                            "type": "text",
                            "text": e
                        }],
                        "isError": true
                    });
                    Some(JsonRpcResponse::success(req.id.clone(), result))
                }
            }
        }

        _ => Some(JsonRpcResponse::error(
            req.id.clone(),
            -32601,
            format!("Method not found: {}", req.method),
        )),
    }
}
