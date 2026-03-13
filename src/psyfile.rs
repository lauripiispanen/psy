//! Psyfile — TOML-based process unit definitions.
//!
//! A Psyfile defines named process units with commands, restart policies,
//! environment variables, dependencies, and working directories.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};

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
    pub depends_on: Vec<String>,
    pub singleton: bool,
    pub working_dir: Option<PathBuf>,
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
                        item.as_str()
                            .ok_or_else(|| {
                                format!("unit '{name}': 'depends_on' entries must be strings")
                            })
                            .map(|s| s.to_string())
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

        units.insert(
            name.clone(),
            UnitDef {
                command,
                restart,
                env,
                depends_on,
                singleton,
                working_dir,
            },
        );
    }

    Ok(Psyfile { units })
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
            if !psyfile.units.contains_key(dep) {
                return Err(format!(
                    "unit '{name}': depends_on references unknown unit '{dep}'"
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
                .entry(dep.as_str())
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
        dfs_order(psyfile, dep, visited, visiting, result)?;
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
        assert_eq!(pf.units["api"].depends_on, vec!["db"]);
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
}
