//! Psyfile — TOML-based process unit definitions.
//!
//! A Psyfile defines named process units with commands, restart policies,
//! environment variables, dependencies, and working directories.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::protocol::RestartPolicy;

// ---------------------------------------------------------------------------
// Data model
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub struct Psyfile {
    pub units: HashMap<String, UnitDef>,
}

#[derive(Debug)]
pub struct UnitDef {
    pub command: String,
    pub restart: RestartPolicy,
    pub env: HashMap<String, String>,
    pub depends_on: Vec<Dependency>,
    pub singleton: bool,
    pub working_dir: Option<PathBuf>,
    pub ready: Option<ProbeConfig>,
    pub healthcheck: Option<ProbeConfig>,
}

#[derive(Debug, Clone)]
pub struct Dependency {
    pub name: String,
    pub restart: bool,
}

#[derive(Debug, Clone)]
pub enum ProbeKind {
    Exit(i32),
    Tcp(String),
    Http(String),
    Exec(String),
}

#[derive(Debug, Clone)]
pub struct ProbeConfig {
    pub probe: ProbeKind,
    pub interval: Duration,
    pub timeout: Duration,
    pub retries: Option<u32>,
}

impl UnitDef {
    /// Extract dependency names as a plain list of strings.
    pub fn dep_names(&self) -> Vec<String> {
        self.depends_on.iter().map(|d| d.name.clone()).collect()
    }
}

// ---------------------------------------------------------------------------
// File discovery
// ---------------------------------------------------------------------------

/// Walk upward from `from` looking for `Psyfile` or `Psyfile.toml`.
pub fn discover(from: &Path) -> Option<PathBuf> {
    let mut dir = from.to_path_buf();
    loop {
        for name in &["Psyfile", "Psyfile.toml"] {
            let candidate = dir.join(name);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
        if !dir.pop() {
            return None;
        }
    }
}

// ---------------------------------------------------------------------------
// Parsing
// ---------------------------------------------------------------------------

pub fn parse(path: &Path) -> Result<Psyfile, String> {
    let content = std::fs::read_to_string(path)
        .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    parse_str(&content)
}

pub fn parse_str(content: &str) -> Result<Psyfile, String> {
    let table: toml::Table = content
        .parse()
        .map_err(|e: toml::de::Error| format!("invalid TOML: {e}"))?;

    let mut units = HashMap::new();

    for (name, value) in &table {
        let unit_table = value
            .as_table()
            .ok_or_else(|| format!("unit '{name}' must be a table"))?;

        // Reject unknown fields
        let known_fields = [
            "command",
            "restart",
            "env",
            "depends_on",
            "singleton",
            "working_dir",
            "ready",
            "healthcheck",
        ];
        for key in unit_table.keys() {
            if !known_fields.contains(&key.as_str()) {
                return Err(format!("unknown field '{key}' in unit '{name}'"));
            }
        }

        let command = unit_table
            .get("command")
            .and_then(|v| v.as_str())
            .ok_or_else(|| format!("unit '{name}': 'command' is required and must be a string"))?
            .to_string();

        let restart = match unit_table.get("restart").and_then(|v| v.as_str()) {
            Some("on-failure") | Some("on_failure") => RestartPolicy::OnFailure,
            Some("always") => RestartPolicy::Always,
            Some("no") | None => RestartPolicy::No,
            Some(other) => {
                return Err(format!(
                    "unit '{name}': invalid restart policy '{other}', must be 'no', 'on-failure', or 'always'"
                ))
            }
        };

        let env = match unit_table.get("env") {
            Some(v) => {
                let env_table = v
                    .as_table()
                    .ok_or_else(|| format!("unit '{name}': 'env' must be a table"))?;
                let mut map = HashMap::new();
                for (k, val) in env_table {
                    let s = val.as_str().ok_or_else(|| {
                        format!("unit '{name}': env value for '{k}' must be a string")
                    })?;
                    map.insert(k.clone(), s.to_string());
                }
                map
            }
            None => HashMap::new(),
        };

        let depends_on = match unit_table.get("depends_on") {
            Some(v) => {
                let arr = v
                    .as_array()
                    .ok_or_else(|| format!("unit '{name}': 'depends_on' must be an array"))?;
                arr.iter()
                    .map(|item| {
                        if let Some(s) = item.as_str() {
                            Ok(Dependency {
                                name: s.to_string(),
                                restart: false,
                            })
                        } else if let Some(tbl) = item.as_table() {
                            let dep_name = tbl
                                .get("name")
                                .and_then(|v| v.as_str())
                                .ok_or_else(|| {
                                    format!("unit '{name}': depends_on table entry requires 'name' string")
                                })?;
                            let restart = tbl
                                .get("restart")
                                .and_then(|v| v.as_bool())
                                .unwrap_or(false);
                            // Reject unknown keys
                            for key in tbl.keys() {
                                if key != "name" && key != "restart" {
                                    return Err(format!(
                                        "unit '{name}': unknown field '{key}' in depends_on entry"
                                    ));
                                }
                            }
                            Ok(Dependency {
                                name: dep_name.to_string(),
                                restart,
                            })
                        } else {
                            Err(format!(
                                "unit '{name}': 'depends_on' entries must be strings or tables"
                            ))
                        }
                    })
                    .collect::<Result<Vec<_>, _>>()?
            }
            None => Vec::new(),
        };

        let singleton = unit_table
            .get("singleton")
            .and_then(|v| v.as_bool())
            .unwrap_or(true);

        let working_dir = unit_table
            .get("working_dir")
            .and_then(|v| v.as_str())
            .map(PathBuf::from);

        let ready = match unit_table.get("ready") {
            Some(v) => Some(parse_probe_config(v, name, "ready")?),
            None => None,
        };

        let healthcheck = match unit_table.get("healthcheck") {
            Some(v) => Some(parse_probe_config(v, name, "healthcheck")?),
            None => None,
        };

        units.insert(
            name.clone(),
            UnitDef {
                command,
                restart,
                env,
                depends_on,
                singleton,
                working_dir,
                ready,
                healthcheck,
            },
        );
    }

    Ok(Psyfile { units })
}

// ---------------------------------------------------------------------------
// Probe config parsing
// ---------------------------------------------------------------------------

fn parse_probe_config(
    value: &toml::Value,
    unit_name: &str,
    field: &str,
) -> Result<ProbeConfig, String> {
    let tbl = value
        .as_table()
        .ok_or_else(|| format!("unit '{unit_name}': '{field}' must be a table"))?;

    // Reject unknown keys
    let known_probe_fields = [
        "exit", "tcp", "http", "exec", "interval", "timeout", "retries",
    ];
    for key in tbl.keys() {
        if !known_probe_fields.contains(&key.as_str()) {
            return Err(format!(
                "unit '{unit_name}': unknown field '{key}' in '{field}'"
            ));
        }
    }

    // Parse probe kind — exactly one of exit/tcp/http/exec
    let mut probe = None;
    let mut type_count = 0;

    if let Some(v) = tbl.get("exit") {
        if field == "healthcheck" {
            return Err(format!(
                "unit '{unit_name}': 'exit' probe type is not valid for healthcheck"
            ));
        }
        let code = v
            .as_integer()
            .ok_or_else(|| format!("unit '{unit_name}': '{field}.exit' must be an integer"))?
            as i32;
        probe = Some(ProbeKind::Exit(code));
        type_count += 1;
    }
    if let Some(v) = tbl.get("tcp") {
        let addr = if let Some(s) = v.as_str() {
            s.to_string()
        } else if let Some(n) = v.as_integer() {
            format!("localhost:{n}")
        } else {
            return Err(format!(
                "unit '{unit_name}': '{field}.tcp' must be a string or integer"
            ));
        };
        probe = Some(ProbeKind::Tcp(addr));
        type_count += 1;
    }
    if let Some(v) = tbl.get("http") {
        let url = v
            .as_str()
            .ok_or_else(|| format!("unit '{unit_name}': '{field}.http' must be a string"))?;
        probe = Some(ProbeKind::Http(url.to_string()));
        type_count += 1;
    }
    if let Some(v) = tbl.get("exec") {
        let cmd = v
            .as_str()
            .ok_or_else(|| format!("unit '{unit_name}': '{field}.exec' must be a string"))?;
        probe = Some(ProbeKind::Exec(cmd.to_string()));
        type_count += 1;
    }

    if type_count == 0 {
        return Err(format!(
            "unit '{unit_name}': '{field}' must specify one of: exit, tcp, http, exec"
        ));
    }
    if type_count > 1 {
        return Err(format!(
            "unit '{unit_name}': '{field}' must specify exactly one of: exit, tcp, http, exec"
        ));
    }

    let is_ready = field == "ready";
    let default_interval = if is_ready {
        Duration::from_secs(1)
    } else {
        Duration::from_secs(10)
    };
    let default_timeout = Duration::from_secs(30);

    let interval = match tbl.get("interval") {
        Some(v) => {
            let s = v.as_str().ok_or_else(|| {
                format!("unit '{unit_name}': '{field}.interval' must be a string")
            })?;
            parse_duration(s).map_err(|e| format!("unit '{unit_name}': '{field}.interval': {e}"))?
        }
        None => default_interval,
    };

    let timeout = match tbl.get("timeout") {
        Some(v) => {
            let s = v
                .as_str()
                .ok_or_else(|| format!("unit '{unit_name}': '{field}.timeout' must be a string"))?;
            parse_duration(s).map_err(|e| format!("unit '{unit_name}': '{field}.timeout': {e}"))?
        }
        None => default_timeout,
    };

    let retries =
        match tbl.get("retries") {
            Some(v) => Some(v.as_integer().ok_or_else(|| {
                format!("unit '{unit_name}': '{field}.retries' must be an integer")
            })? as u32),
            None => None,
        };

    Ok(ProbeConfig {
        probe: probe.unwrap(),
        interval,
        timeout,
        retries,
    })
}

/// Parse a duration string like "1s", "5m", "2h".
pub fn parse_duration(s: &str) -> Result<Duration, String> {
    let s = s.trim();
    if s.is_empty() {
        return Err("empty duration string".into());
    }
    let (num_str, suffix) = s.split_at(s.len() - 1);
    let num: u64 = num_str
        .parse()
        .map_err(|_| format!("invalid duration '{s}': expected format like '1s', '5m', '2h'"))?;
    match suffix {
        "s" => Ok(Duration::from_secs(num)),
        "m" => Ok(Duration::from_secs(num * 60)),
        "h" => Ok(Duration::from_secs(num * 3600)),
        _ => Err(format!(
            "invalid duration suffix '{suffix}' in '{s}': expected 's', 'm', or 'h'"
        )),
    }
}

// ---------------------------------------------------------------------------
// Validation
// ---------------------------------------------------------------------------

pub fn validate(psyfile: &Psyfile) -> Result<(), String> {
    use crate::process::validate_name;

    for name in psyfile.units.keys() {
        if name == "main" {
            return Err("unit name 'main' is reserved".into());
        }
        if !validate_name(name) {
            return Err(format!(
                "invalid unit name '{name}': must match [a-zA-Z0-9][a-zA-Z0-9_-]{{0,62}}"
            ));
        }
    }

    // Check dependency references exist
    for (name, unit) in &psyfile.units {
        for dep in &unit.depends_on {
            if !psyfile.units.contains_key(&dep.name) {
                return Err(format!(
                    "unit '{name}': depends_on references unknown unit '{}'",
                    dep.name
                ));
            }
        }
    }

    // Circular dependency detection via topological sort (Kahn's algorithm)
    detect_cycles(psyfile)?;

    Ok(())
}

fn detect_cycles(psyfile: &Psyfile) -> Result<(), String> {
    let mut in_degree: HashMap<&str, usize> = HashMap::new();
    let mut adjacency: HashMap<&str, Vec<&str>> = HashMap::new();

    for name in psyfile.units.keys() {
        in_degree.entry(name.as_str()).or_insert(0);
        adjacency.entry(name.as_str()).or_default();
    }

    for (name, unit) in &psyfile.units {
        for dep in &unit.depends_on {
            adjacency
                .entry(dep.name.as_str())
                .or_default()
                .push(name.as_str());
            *in_degree.entry(name.as_str()).or_insert(0) += 1;
        }
    }

    let mut queue: VecDeque<&str> = in_degree
        .iter()
        .filter(|(_, &deg)| deg == 0)
        .map(|(&name, _)| name)
        .collect();

    let mut visited = 0usize;
    while let Some(node) = queue.pop_front() {
        visited += 1;
        if let Some(neighbors) = adjacency.get(node) {
            for &neighbor in neighbors {
                let deg = in_degree.get_mut(neighbor).unwrap();
                *deg -= 1;
                if *deg == 0 {
                    queue.push_back(neighbor);
                }
            }
        }
    }

    if visited != psyfile.units.len() {
        // Find nodes in the cycle for a better error message
        let in_cycle: Vec<_> = in_degree
            .iter()
            .filter(|(_, &deg)| deg > 0)
            .map(|(&name, _)| name)
            .collect();
        return Err(format!(
            "circular dependency detected among: {}",
            in_cycle.join(", ")
        ));
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Environment variable interpolation
// ---------------------------------------------------------------------------

/// Interpolate `${VAR}` and `${VAR:-default}` in a template string.
pub fn interpolate(template: &str, env: &HashMap<String, String>) -> String {
    let mut result = String::with_capacity(template.len());
    let mut chars = template.chars().peekable();

    while let Some(c) = chars.next() {
        if c == '$' && chars.peek() == Some(&'{') {
            chars.next(); // consume '{'
            let mut var_expr = String::new();
            let mut depth = 1;
            for ch in chars.by_ref() {
                if ch == '{' {
                    depth += 1;
                    var_expr.push(ch);
                } else if ch == '}' {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                    var_expr.push(ch);
                } else {
                    var_expr.push(ch);
                }
            }
            // Parse VAR or VAR:-default
            if let Some(idx) = var_expr.find(":-") {
                let var_name = &var_expr[..idx];
                let default = &var_expr[idx + 2..];
                match env.get(var_name) {
                    Some(val) if !val.is_empty() => result.push_str(val),
                    _ => result.push_str(default),
                }
            } else if let Some(val) = env.get(var_expr.as_str()) {
                result.push_str(val);
            }
        } else {
            result.push(c);
        }
    }

    result
}

// ---------------------------------------------------------------------------
// Dependency resolution
// ---------------------------------------------------------------------------

/// Given a set of unit names, return the full list including transitive
/// dependencies in topological order (dependencies first).
pub fn resolve_start_order(psyfile: &Psyfile, names: &[String]) -> Result<Vec<String>, String> {
    let mut result = Vec::new();
    let mut visited = HashSet::new();
    let mut visiting = HashSet::new(); // for cycle detection during DFS

    for name in names {
        dfs_order(psyfile, name, &mut visited, &mut visiting, &mut result)?;
    }

    Ok(result)
}

fn dfs_order(
    psyfile: &Psyfile,
    name: &str,
    visited: &mut HashSet<String>,
    visiting: &mut HashSet<String>,
    result: &mut Vec<String>,
) -> Result<(), String> {
    if visited.contains(name) {
        return Ok(());
    }
    if visiting.contains(name) {
        return Err(format!("circular dependency involving '{name}'"));
    }

    let unit = psyfile
        .units
        .get(name)
        .ok_or_else(|| format!("unknown unit '{name}'"))?;

    visiting.insert(name.to_string());

    for dep in &unit.depends_on {
        dfs_order(psyfile, &dep.name, visited, visiting, result)?;
    }

    visiting.remove(name);
    visited.insert(name.to_string());
    result.push(name.to_string());

    Ok(())
}

// ---------------------------------------------------------------------------
// Shell escaping
// ---------------------------------------------------------------------------

/// Shell-escape a single argument using single quotes.
pub fn shell_escape(arg: &str) -> String {
    if arg.is_empty() {
        return "''".to_string();
    }
    // If it's safe (only alphanumeric, dash, underscore, dot, slash), no escaping needed
    if arg
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.' || b == b'/')
    {
        return arg.to_string();
    }
    // Wrap in single quotes, escaping internal single quotes as '\''
    let mut escaped = String::from("'");
    for c in arg.chars() {
        if c == '\'' {
            escaped.push_str("'\\''");
        } else {
            escaped.push(c);
        }
    }
    escaped.push('\'');
    escaped
}

/// Shell-escape and join multiple arguments.
pub fn shell_join(args: &[String]) -> String {
    args.iter()
        .map(|a| shell_escape(a))
        .collect::<Vec<_>>()
        .join(" ")
}

// ---------------------------------------------------------------------------
// Shell command builder
// ---------------------------------------------------------------------------

/// Build a shell command for executing a command string.
pub fn build_shell_command(cmd_str: &str) -> Vec<String> {
    #[cfg(unix)]
    {
        vec!["sh".into(), "-c".into(), cmd_str.into()]
    }
    #[cfg(windows)]
    {
        vec!["cmd".into(), "/C".into(), cmd_str.into()]
    }
}

/// Build the final command string from a Psyfile unit command and extra args.
pub fn build_command_with_args(command: &str, extra_args: &[String]) -> String {
    if extra_args.is_empty() {
        if command.contains("$@") {
            // Replace $@ with empty, collapse whitespace
            let result = command.replace("$@", "");
            // Collapse multiple spaces
            let mut prev_space = false;
            let collapsed: String = result
                .chars()
                .filter(|&c| {
                    if c == ' ' {
                        if prev_space {
                            return false;
                        }
                        prev_space = true;
                    } else {
                        prev_space = false;
                    }
                    true
                })
                .collect();
            collapsed.trim().to_string()
        } else {
            command.to_string()
        }
    } else {
        let joined = shell_join(extra_args);
        if command.contains("$@") {
            command.replace("$@", &joined)
        } else {
            format!("{} {}", command, joined)
        }
    }
}

// ---------------------------------------------------------------------------
// JSON Schema
// ---------------------------------------------------------------------------

/// Return a JSON Schema describing the Psyfile format.
pub fn json_schema() -> serde_json::Value {
    serde_json::json!({
        "$schema": "http://json-schema.org/draft-07/schema#",
        "title": "Psyfile",
        "description": "psy process lifecycle manager — unit definitions",
        "type": "object",
        "additionalProperties": {
            "type": "object",
            "required": ["command"],
            "properties": {
                "command": {
                    "type": "string",
                    "description": "Shell command to run"
                },
                "restart": {
                    "type": "string",
                    "enum": ["no", "on-failure", "always"],
                    "default": "no",
                    "description": "Restart policy"
                },
                "env": {
                    "type": "object",
                    "additionalProperties": { "type": "string" },
                    "description": "Environment variables"
                },
                "depends_on": {
                    "type": "array",
                    "items": {
                        "oneOf": [
                            { "type": "string" },
                            {
                                "type": "object",
                                "required": ["name"],
                                "properties": {
                                    "name": { "type": "string" },
                                    "restart": { "type": "boolean", "default": false }
                                },
                                "additionalProperties": false
                            }
                        ]
                    },
                    "description": "Units to start before this one"
                },
                "singleton": {
                    "type": "boolean",
                    "default": true,
                    "description": "Single instance (true) or template (false)"
                },
                "working_dir": {
                    "type": "string",
                    "description": "Working directory for the process"
                },
                "ready": {
                    "type": "object",
                    "description": "Startup readiness probe — dependents wait for it",
                    "properties": {
                        "exit": { "type": "integer", "description": "Expected exit code" },
                        "tcp": {
                            "oneOf": [
                                { "type": "string" },
                                { "type": "integer" }
                            ],
                            "description": "TCP address or port to probe"
                        },
                        "http": { "type": "string", "description": "HTTP URL to probe (expects 2xx)" },
                        "exec": { "type": "string", "description": "Command to run (expects exit 0)" },
                        "interval": { "type": "string", "default": "1s", "description": "Time between attempts" },
                        "timeout": { "type": "string", "default": "30s", "description": "Give up after this duration" },
                        "retries": { "type": "integer", "description": "Max probe attempts" }
                    },
                    "additionalProperties": false
                },
                "healthcheck": {
                    "type": "object",
                    "description": "Continuous health check — failure triggers restart per policy",
                    "properties": {
                        "tcp": {
                            "oneOf": [
                                { "type": "string" },
                                { "type": "integer" }
                            ],
                            "description": "TCP address or port to probe"
                        },
                        "http": { "type": "string", "description": "HTTP URL to probe (expects 2xx)" },
                        "exec": { "type": "string", "description": "Command to run (expects exit 0)" },
                        "interval": { "type": "string", "default": "10s", "description": "Time between checks" },
                        "timeout": { "type": "string", "default": "30s", "description": "Per-check timeout" },
                        "retries": { "type": "integer", "default": 3, "description": "Consecutive failures before unhealthy" }
                    },
                    "additionalProperties": false
                }
            },
            "additionalProperties": false
        }
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -- parse ---------------------------------------------------------------

    #[test]
    fn parse_valid_psyfile() {
        let content = r#"
[server]
command = "cargo run --bin server"
restart = "on-failure"
env = { PORT = "8080" }

[db]
command = "docker run postgres"
restart = "always"
working_dir = "/tmp"
"#;
        let pf = parse_str(content).unwrap();
        assert_eq!(pf.units.len(), 2);

        let server = &pf.units["server"];
        assert_eq!(server.command, "cargo run --bin server");
        assert_eq!(server.restart, RestartPolicy::OnFailure);
        assert_eq!(server.env.get("PORT").unwrap(), "8080");
        assert!(server.singleton);
        assert!(server.working_dir.is_none());

        let db = &pf.units["db"];
        assert_eq!(db.restart, RestartPolicy::Always);
        assert_eq!(db.working_dir.as_deref(), Some(Path::new("/tmp")));
    }

    #[test]
    fn parse_minimal_psyfile() {
        let content = r#"
[echo]
command = "echo hello"
"#;
        let pf = parse_str(content).unwrap();
        let unit = &pf.units["echo"];
        assert_eq!(unit.restart, RestartPolicy::No);
        assert!(unit.env.is_empty());
        assert!(unit.depends_on.is_empty());
        assert!(unit.singleton);
        assert!(unit.working_dir.is_none());
    }

    #[test]
    fn parse_error_missing_command() {
        let content = r#"
[bad]
restart = "always"
"#;
        let err = parse_str(content).unwrap_err();
        assert!(
            err.contains("command"),
            "expected command error, got: {err}"
        );
    }

    #[test]
    fn parse_error_unknown_field() {
        let content = r#"
[unit]
command = "echo test"
depnds_on = ["other"]
"#;
        let err = parse_str(content).unwrap_err();
        assert!(
            err.contains("unknown field") && err.contains("depnds_on"),
            "expected unknown field error, got: {err}"
        );
    }

    #[test]
    fn parse_error_invalid_toml() {
        let content = "not valid toml [[[";
        let err = parse_str(content).unwrap_err();
        assert!(
            err.contains("invalid TOML"),
            "expected TOML error, got: {err}"
        );
    }

    #[test]
    fn parse_error_invalid_restart() {
        let content = r#"
[unit]
command = "echo test"
restart = "sometimes"
"#;
        let err = parse_str(content).unwrap_err();
        assert!(
            err.contains("invalid restart policy"),
            "expected restart policy error, got: {err}"
        );
    }

    #[test]
    fn parse_with_depends_on() {
        let content = r#"
[db]
command = "start-db"

[api]
command = "start-api"
depends_on = ["db"]
"#;
        let pf = parse_str(content).unwrap();
        assert_eq!(pf.units["api"].dep_names(), vec!["db"]);
    }

    #[test]
    fn parse_singleton_false() {
        let content = r#"
[client]
command = "start-client"
singleton = false
"#;
        let pf = parse_str(content).unwrap();
        assert!(!pf.units["client"].singleton);
    }

    // -- validate ------------------------------------------------------------

    #[test]
    fn validate_circular_deps() {
        let content = r#"
[a]
command = "echo a"
depends_on = ["b"]

[b]
command = "echo b"
depends_on = ["a"]
"#;
        let pf = parse_str(content).unwrap();
        let err = validate(&pf).unwrap_err();
        assert!(
            err.contains("circular"),
            "expected circular error, got: {err}"
        );
    }

    #[test]
    fn validate_unknown_dep_ref() {
        let content = r#"
[a]
command = "echo a"
depends_on = ["nonexistent"]
"#;
        let pf = parse_str(content).unwrap();
        let err = validate(&pf).unwrap_err();
        assert!(
            err.contains("unknown unit 'nonexistent'"),
            "expected unknown dep error, got: {err}"
        );
    }

    #[test]
    fn validate_reserved_name_main() {
        let content = r#"
[main]
command = "echo main"
"#;
        let pf = parse_str(content).unwrap();
        let err = validate(&pf).unwrap_err();
        assert!(
            err.contains("reserved"),
            "expected reserved name error, got: {err}"
        );
    }

    #[test]
    fn validate_invalid_name_format() {
        let content = r#"
["-bad"]
command = "echo bad"
"#;
        let pf = parse_str(content).unwrap();
        let err = validate(&pf).unwrap_err();
        assert!(
            err.contains("invalid unit name"),
            "expected invalid name error, got: {err}"
        );
    }

    #[test]
    fn validate_valid_psyfile() {
        let content = r#"
[db]
command = "start-db"

[api]
command = "start-api"
depends_on = ["db"]

[worker]
command = "start-worker"
depends_on = ["db"]
"#;
        let pf = parse_str(content).unwrap();
        assert!(validate(&pf).is_ok());
    }

    // -- interpolation -------------------------------------------------------

    #[test]
    fn interpolate_var() {
        let mut env = HashMap::new();
        env.insert("PORT".into(), "8080".into());
        assert_eq!(interpolate("--port ${PORT}", &env), "--port 8080");
    }

    #[test]
    fn interpolate_var_with_default() {
        let env = HashMap::new();
        assert_eq!(interpolate("--port ${PORT:-3000}", &env), "--port 3000");
    }

    #[test]
    fn interpolate_var_with_default_value_present() {
        let mut env = HashMap::new();
        env.insert("PORT".into(), "9090".into());
        assert_eq!(interpolate("--port ${PORT:-3000}", &env), "--port 9090");
    }

    #[test]
    fn interpolate_undefined_no_default() {
        let env = HashMap::new();
        assert_eq!(interpolate("pre-${MISSING}-post", &env), "pre--post");
    }

    #[test]
    fn interpolate_no_recursion() {
        let mut env = HashMap::new();
        env.insert("A".into(), "${B}".into());
        env.insert("B".into(), "val".into());
        // Only the outermost is substituted
        assert_eq!(interpolate("${A}", &env), "${B}");
    }

    // -- resolve_start_order -------------------------------------------------

    #[test]
    fn resolve_no_deps() {
        let content = r#"
[a]
command = "echo a"
"#;
        let pf = parse_str(content).unwrap();
        let order = resolve_start_order(&pf, &["a".into()]).unwrap();
        assert_eq!(order, vec!["a"]);
    }

    #[test]
    fn resolve_chain() {
        let content = r#"
[a]
command = "echo a"
depends_on = ["b"]

[b]
command = "echo b"
depends_on = ["c"]

[c]
command = "echo c"
"#;
        let pf = parse_str(content).unwrap();
        let order = resolve_start_order(&pf, &["a".into()]).unwrap();
        assert_eq!(order, vec!["c", "b", "a"]);
    }

    #[test]
    fn resolve_diamond() {
        let content = r#"
[a]
command = "echo a"
depends_on = ["b", "c"]

[b]
command = "echo b"
depends_on = ["d"]

[c]
command = "echo c"
depends_on = ["d"]

[d]
command = "echo d"
"#;
        let pf = parse_str(content).unwrap();
        let order = resolve_start_order(&pf, &["a".into()]).unwrap();
        // d must come first, then b and c (in some order), then a
        assert_eq!(order[0], "d");
        assert_eq!(*order.last().unwrap(), "a");
        assert_eq!(order.len(), 4);
    }

    #[test]
    fn resolve_already_included() {
        let content = r#"
[a]
command = "echo a"
depends_on = ["b"]

[b]
command = "echo b"
"#;
        let pf = parse_str(content).unwrap();
        let order = resolve_start_order(&pf, &["b".into(), "a".into()]).unwrap();
        assert_eq!(order, vec!["b", "a"]);
    }

    // -- shell escaping ------------------------------------------------------

    #[test]
    fn escape_simple() {
        assert_eq!(shell_escape("hello"), "hello");
    }

    #[test]
    fn escape_with_space() {
        assert_eq!(shell_escape("hello world"), "'hello world'");
    }

    #[test]
    fn escape_with_quote() {
        assert_eq!(shell_escape("it's"), "'it'\\''s'");
    }

    #[test]
    fn escape_empty() {
        assert_eq!(shell_escape(""), "''");
    }

    #[test]
    fn join_multiple() {
        let args = vec!["a".into(), "b c".into(), "d".into()];
        assert_eq!(shell_join(&args), "a 'b c' d");
    }

    // -- build_command_with_args ---------------------------------------------

    #[test]
    fn args_append() {
        let result = build_command_with_args("cargo test", &["--flag".into()]);
        assert_eq!(result, "cargo test --flag");
    }

    #[test]
    fn args_dollar_at_substitution() {
        let result = build_command_with_args("./cmd $@ --end", &["a".into(), "b".into()]);
        assert_eq!(result, "./cmd a b --end");
    }

    #[test]
    fn args_dollar_at_no_args() {
        let result = build_command_with_args("./cmd $@ --end", &[]);
        assert_eq!(result, "./cmd --end");
    }

    #[test]
    fn args_dollar_at_with_special() {
        let result = build_command_with_args("./cmd $@ --end", &["hello world".into()]);
        assert_eq!(result, "./cmd 'hello world' --end");
    }

    // -- discover ------------------------------------------------------------

    #[test]
    fn discover_in_current_dir() {
        let dir = std::env::temp_dir().join("psy-test-discover");
        let _ = std::fs::create_dir_all(&dir);
        let psyfile = dir.join("Psyfile");
        std::fs::write(&psyfile, "[test]\ncommand = \"echo test\"\n").unwrap();

        let found = discover(&dir);
        assert_eq!(found, Some(psyfile.clone()));

        let _ = std::fs::remove_file(&psyfile);
        let _ = std::fs::remove_dir(&dir);
    }

    #[test]
    fn discover_toml_extension() {
        let dir = std::env::temp_dir().join("psy-test-discover-toml");
        let _ = std::fs::create_dir_all(&dir);
        let psyfile = dir.join("Psyfile.toml");
        std::fs::write(&psyfile, "[test]\ncommand = \"echo test\"\n").unwrap();

        let found = discover(&dir);
        assert_eq!(found, Some(psyfile.clone()));

        let _ = std::fs::remove_file(&psyfile);
        let _ = std::fs::remove_dir(&dir);
    }

    #[test]
    fn discover_walk_upward() {
        let parent = std::env::temp_dir().join("psy-test-discover-walk");
        let child = parent.join("subdir");
        let _ = std::fs::create_dir_all(&child);
        let psyfile = parent.join("Psyfile");
        std::fs::write(&psyfile, "[test]\ncommand = \"echo test\"\n").unwrap();

        let found = discover(&child);
        assert_eq!(found, Some(psyfile.clone()));

        let _ = std::fs::remove_file(&psyfile);
        let _ = std::fs::remove_dir(&child);
        let _ = std::fs::remove_dir(&parent);
    }

    #[test]
    fn discover_not_found() {
        // Use a path unlikely to have a Psyfile
        let dir = std::env::temp_dir().join("psy-test-discover-none-xyz123");
        let _ = std::fs::create_dir_all(&dir);
        let found = discover(&dir);
        // Should be None or find one in some parent - just check it doesn't panic
        // and if found, it's a real file
        if let Some(ref p) = found {
            assert!(p.is_file());
        }
        let _ = std::fs::remove_dir(&dir);
    }

    // -- depends_on extended syntax ------------------------------------------

    #[test]
    fn parse_depends_on_mixed() {
        let content = r#"
[db]
command = "start-db"

[cache]
command = "start-cache"

[api]
command = "start-api"
depends_on = ["db", { name = "cache", restart = true }]
"#;
        let pf = parse_str(content).unwrap();
        let deps = &pf.units["api"].depends_on;
        assert_eq!(deps.len(), 2);
        assert_eq!(deps[0].name, "db");
        assert!(!deps[0].restart);
        assert_eq!(deps[1].name, "cache");
        assert!(deps[1].restart);
    }

    #[test]
    fn parse_depends_on_table_only() {
        let content = r#"
[db]
command = "start-db"

[api]
command = "start-api"
depends_on = [{ name = "db", restart = true }]
"#;
        let pf = parse_str(content).unwrap();
        let deps = &pf.units["api"].depends_on;
        assert_eq!(deps.len(), 1);
        assert_eq!(deps[0].name, "db");
        assert!(deps[0].restart);
    }

    #[test]
    fn parse_depends_on_table_default_restart() {
        let content = r#"
[db]
command = "start-db"

[api]
command = "start-api"
depends_on = [{ name = "db" }]
"#;
        let pf = parse_str(content).unwrap();
        assert!(!pf.units["api"].depends_on[0].restart);
    }

    // -- ready / healthcheck probes ------------------------------------------

    #[test]
    fn parse_ready_tcp() {
        let content = r#"
[server]
command = "start-server"
ready = { tcp = "localhost:8080" }
"#;
        let pf = parse_str(content).unwrap();
        let ready = pf.units["server"].ready.as_ref().unwrap();
        assert!(matches!(ready.probe, ProbeKind::Tcp(ref s) if s == "localhost:8080"));
        assert_eq!(ready.interval, Duration::from_secs(1));
        assert_eq!(ready.timeout, Duration::from_secs(30));
    }

    #[test]
    fn parse_ready_tcp_port_number() {
        let content = r#"
[server]
command = "start-server"
ready = { tcp = 8080 }
"#;
        let pf = parse_str(content).unwrap();
        let ready = pf.units["server"].ready.as_ref().unwrap();
        assert!(matches!(ready.probe, ProbeKind::Tcp(ref s) if s == "localhost:8080"));
    }

    #[test]
    fn parse_ready_http() {
        let content = r#"
[api]
command = "start-api"
ready = { http = "http://localhost:3000/health" }
"#;
        let pf = parse_str(content).unwrap();
        let ready = pf.units["api"].ready.as_ref().unwrap();
        assert!(
            matches!(ready.probe, ProbeKind::Http(ref s) if s == "http://localhost:3000/health")
        );
    }

    #[test]
    fn parse_ready_exec() {
        let content = r#"
[db]
command = "start-db"
ready = { exec = "pg_isready -h localhost" }
"#;
        let pf = parse_str(content).unwrap();
        let ready = pf.units["db"].ready.as_ref().unwrap();
        assert!(matches!(ready.probe, ProbeKind::Exec(ref s) if s == "pg_isready -h localhost"));
    }

    #[test]
    fn parse_ready_exit() {
        let content = r#"
[build]
command = "cargo build"
ready = { exit = 0 }
"#;
        let pf = parse_str(content).unwrap();
        let ready = pf.units["build"].ready.as_ref().unwrap();
        assert!(matches!(ready.probe, ProbeKind::Exit(0)));
    }

    #[test]
    fn parse_ready_custom_interval_timeout() {
        let content = r#"
[server]
command = "start-server"
ready = { tcp = "localhost:5432", interval = "2s", timeout = "60s", retries = 10 }
"#;
        let pf = parse_str(content).unwrap();
        let ready = pf.units["server"].ready.as_ref().unwrap();
        assert_eq!(ready.interval, Duration::from_secs(2));
        assert_eq!(ready.timeout, Duration::from_secs(60));
        assert_eq!(ready.retries, Some(10));
    }

    #[test]
    fn parse_healthcheck() {
        let content = r#"
[server]
command = "start-server"
healthcheck = { tcp = "localhost:8080", interval = "10s", retries = 3 }
"#;
        let pf = parse_str(content).unwrap();
        let hc = pf.units["server"].healthcheck.as_ref().unwrap();
        assert!(matches!(hc.probe, ProbeKind::Tcp(ref s) if s == "localhost:8080"));
        assert_eq!(hc.interval, Duration::from_secs(10));
        assert_eq!(hc.retries, Some(3));
    }

    #[test]
    fn parse_healthcheck_exit_rejected() {
        let content = r#"
[server]
command = "start-server"
healthcheck = { exit = 0 }
"#;
        let err = parse_str(content).unwrap_err();
        assert!(err.contains("not valid for healthcheck"), "got: {err}");
    }

    #[test]
    fn parse_probe_no_type() {
        let content = r#"
[server]
command = "start-server"
ready = { interval = "1s" }
"#;
        let err = parse_str(content).unwrap_err();
        assert!(err.contains("must specify one of"), "got: {err}");
    }

    #[test]
    fn parse_probe_multiple_types() {
        let content = r#"
[server]
command = "start-server"
ready = { tcp = "localhost:8080", http = "http://localhost:8080" }
"#;
        let err = parse_str(content).unwrap_err();
        assert!(err.contains("exactly one"), "got: {err}");
    }

    #[test]
    fn parse_both_ready_and_healthcheck() {
        let content = r#"
[server]
command = "start-server"
ready = { tcp = "localhost:8080" }
healthcheck = { http = "http://localhost:8080/health", interval = "15s" }
"#;
        let pf = parse_str(content).unwrap();
        assert!(pf.units["server"].ready.is_some());
        assert!(pf.units["server"].healthcheck.is_some());
    }

    // -- parse_duration ------------------------------------------------------

    #[test]
    fn duration_seconds() {
        assert_eq!(parse_duration("5s").unwrap(), Duration::from_secs(5));
    }

    #[test]
    fn duration_minutes() {
        assert_eq!(parse_duration("2m").unwrap(), Duration::from_secs(120));
    }

    #[test]
    fn duration_hours() {
        assert_eq!(parse_duration("1h").unwrap(), Duration::from_secs(3600));
    }

    #[test]
    fn duration_invalid() {
        assert!(parse_duration("abc").is_err());
        assert!(parse_duration("5x").is_err());
        assert!(parse_duration("").is_err());
    }
}
