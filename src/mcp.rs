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
    RunArgs, StopArgs, StreamFilter,
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
                "description": "Start a new managed process",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "name": {
                            "type": "string",
                            "description": "Unique name for the process"
                        },
                        "command": {
                            "type": "array",
                            "items": { "type": "string" },
                            "description": "Command and arguments to run"
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
                        }
                    },
                    "required": ["name", "command"]
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
                            "description": "Show logs since this RFC 3339 timestamp (e.g. 2026-03-12T20:00:00Z)"
                        },
                        "until": {
                            "type": "string",
                            "description": "Show logs until this RFC 3339 timestamp"
                        },
                        "grep": {
                            "type": "string",
                            "description": "Filter logs by case-insensitive substring match"
                        },
                        "run": {
                            "type": "integer",
                            "description": "Show logs from a specific run ID (see psy_history)"
                        },
                        "previous": {
                            "type": "boolean",
                            "description": "Show logs from the previous run (before the current one)"
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
                .ok_or("missing required parameter: command")?
                .iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect();
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

            let req = Request::run(RunArgs {
                name,
                command,
                restart,
                env,
                attach: false,
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
            let run = args
                .get("run")
                .and_then(|v| v.as_u64())
                .map(|n| n as u32);
            let previous = args
                .get("previous")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let req = Request::logs(LogsArgs {
                name,
                tail,
                stream,
                since,
                until,
                grep,
                run,
                previous,
            });
            let resp = client::send_command(req).map_err(|e| e.to_string())?;
            if resp.ok {
                let text = resp
                    .data
                    .map(|d| serde_json::to_string_pretty(&d).unwrap_or_default())
                    .unwrap_or_else(|| "(no output)".into());
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
        "{:<20} {:<8} {:<10} {:<8} {:<14} {:<10} {}\n",
        "NAME", "PID", "STATUS", "EXIT", "UPTIME", "RESTARTS", "RESTART"
    ));
    out.push_str(&"-".repeat(78));
    out.push('\n');
    for p in &ps.processes {
        let pid_str = p.pid.map(|p| p.to_string()).unwrap_or_else(|| "-".into());
        let exit_str = if let Some(sig) = &p.signal {
            sig.clone()
        } else {
            p.exit_code
                .map(|c| c.to_string())
                .unwrap_or_else(|| "-".into())
        };
        let uptime = p
            .uptime_secs
            .map(|s| format_uptime(s))
            .unwrap_or_else(|| "-".into());
        let restart = format!("{:?}", p.restart_policy).to_lowercase();
        out.push_str(&format!(
            "{:<20} {:<8} {:<10} {:<8} {:<14} {:<10} {}\n",
            p.name, pid_str, p.status, exit_str, uptime, p.restarts, restart
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
            .map(|s| format_uptime(s))
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
