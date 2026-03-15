# psy — Cross-Platform Process Lifecycle Manager

## Build & Run

```bash
cargo build                    # debug build
cargo build --release          # release build
cargo test                     # unit tests
cargo test -- --ignored        # integration tests (require built binary)
cargo build && cargo test -- --ignored  # full test run
```

## Project Structure

```
psy/
├── Cargo.toml
├── src/
│   ├── main.rs              # CLI entry point, argument parsing (clap)
│   ├── root.rs              # psy root: socket server, process table, lifecycle
│   ├── client.rs            # CLI client: connect to root via PSY_SOCK
│   ├── process.rs           # child process management, stdio capture, restart logic
│   ├── ring_buffer.rs       # log ring buffer (line-oriented, bounded)
│   ├── protocol.rs          # NDJSON request/response types, serde
│   ├── psyfile.rs           # Psyfile TOML parsing, validation, interpolation, deps
│   ├── mcp.rs               # MCP server implementation
│   ├── probe.rs             # readiness + healthcheck probe execution engine
│   └── platform/
│       ├── mod.rs           # platform trait + conditional re-exports
│       ├── unix.rs          # pipe trick, subreaper (Linux), signal handling
│       └── windows.rs       # Job Objects, named pipes, console ctrl
├── tests/
│   └── integration.rs       # cross-platform integration tests
└── .github/
    └── workflows/
        └── ci.yml           # GitHub Actions: Linux, macOS, Windows
```

## Implementation Status

### Core Infrastructure
- [x] `Cargo.toml` — dependencies, profile settings
- [x] `src/main.rs` — CLI arg parsing with clap (up, run, ps, logs, history, stop, restart, down, send, mcp, psyfile, version)
- [x] `src/protocol.rs` — NDJSON request/response types (Request, Response, serde)

### Process Management
- [x] `src/ring_buffer.rs` — line-oriented ring buffer (10k lines / 2MB cap, eviction)
- [x] `src/process.rs` — ProcessEntry, ProcessState, RestartPolicy structs
- [x] `src/process.rs` — child spawning with stdio capture (stdout/stderr → ring buffer)
- [x] `src/process.rs` — restart logic: no, on-failure, always policies
- [x] `src/process.rs` — exponential backoff (1s, 2s, 4s, 8s, 16s), max 5 retries → failed state

### Root Server (`src/root.rs`)
- [x] Unix domain socket listener (Linux/macOS)
- [x] Socket path convention: `$XDG_RUNTIME_DIR/psy/<pid>.sock` or `/tmp/psy-<uid>/<pid>.sock`
- [x] Stale socket cleanup (check PID liveness)
- [x] Socket path length validation (Unix ~104 byte limit)
- [x] Set `PSY_SOCK` and `PSY_ROOT_PID` env vars
- [x] Process table: in-memory, serialized mutations (tokio::sync::Mutex)
- [x] Handle `run` command — launch named process, unique name check
- [x] Handle `ps` command — list all processes with status
- [x] Handle `logs` command — retrieve ring buffer contents
- [x] Handle `logs_follow` command — streaming log lines
- [x] Handle `stop` command — SIGTERM → 10s wait → SIGKILL
- [x] Handle `restart` command — stop + re-run with same args
- [x] Handle `send` command — write to interactive process stdin, --eof support, backpressure timeout
- [x] Handle `send_wait` command — blocking send + output collection with idle/overall timeout and prompt matching
- [x] Handle `down` command — teardown all children in reverse order
- [x] Reject `run` during teardown ("shutting down")
- [x] Reject `stop main` (must use `down`)
- [x] Name validation: `[a-zA-Z0-9][a-zA-Z0-9_-]{0,62}`

### CLI Commands
- [x] `psy up` — create root, launch shell (`$SHELL` or default)
- [x] `psy up -- <command>` — create root, launch command as main
- [x] `psy up --name` — custom root name (default: `psy-<pid>`)
- [x] `psy up --file <path>` — explicit Psyfile path
- [x] `psy up <units>` — start specified Psyfile units on boot
- [x] `psy up --all` — start all Psyfile units on boot
- [x] ~`psy up --mcp`~ — removed; agent launches `psy mcp` itself via MCP config
- [x] `psy run <name> -- <command>` — send run command to root via PSY_SOCK
- [x] `psy run <name>` — run Psyfile unit (no `--` needed)
- [x] `psy run --restart <policy>` — restart policy
- [x] `psy run --env KEY=VAL` — extra env vars
- [x] `psy run --attach` — connect terminal stdin/stdout to child
- [x] `psy run --interactive` — enable stdin pipe (writable via psy send)
- [x] `psy ps` — send ps command, format table output
- [x] `psy logs <name>` — dump captured logs
- [x] `psy logs --tail <n>` — last N lines
- [x] `psy logs -f` — follow mode (stream until Ctrl-C)
- [x] `psy logs --stdout / --stderr` — filter by stream
- [x] `psy logs --since` / `--until` — time-based log filtering (relative or RFC 3339)
- [x] `psy logs --grep` — case-insensitive substring filtering
- [x] `psy logs --run <id>` — view logs from a specific run
- [x] `psy logs --previous` — view logs from the run before current
- [x] `psy logs --probe` — show probe logs instead of process logs
- [x] `psy history <name>` — show run history table
- [x] `psy psyfile schema` — output JSON Schema for Psyfile format
- [x] `psy psyfile validate [--file]` — validate Psyfile
- [x] `psy psyfile init` — generate starter Psyfile
- [x] `psy send <name> "text"` — write to process stdin (newline auto-appended)
- [x] `psy send --raw <name> "text"` — write without appending newline
- [x] `psy send --eof <name>` — close process stdin
- [x] `psy send --file <path> <name>` — pipe file contents to stdin
- [x] `psy send --wait <name> "text"` — blocking send, collect output until idle/timeout/prompt
- [x] `psy send --wait-timeout` / `--idle-timeout` / `--wait-prompt` — wait mode options
- [x] `psy stop <name>` — send stop command
- [x] `psy restart <name>` — send restart command
- [x] `psy down` — send down command
- [x] `psy version` — print version

### Client Mode (`src/client.rs`)
- [x] Detect client mode: `PSY_SOCK` set + command is not `up`
- [x] Connect to Unix socket / named pipe
- [x] Send NDJSON request, read response
- [x] Handle `logs_follow` streaming responses
- [x] Error handling: socket not found, connection refused

### Main Process Lifecycle
- [x] Main process stdin/stdout/stderr passthrough (not captured)
- [x] When main exits → teardown all children → exit with main's exit code
- [x] Main fails to start → print error, exit 1, no event loop
- [x] Signal handling: SIGTERM, SIGINT → initiate teardown

### Platform: Shared (`src/platform/mod.rs`)
- [x] Pipe trick: root creates pipe, holds write end, children inherit read end
- [x] Child watchdog thread: blocking read on pipe FD, self-terminate on EOF

### Platform: Linux (`src/platform/unix.rs`)
- [x] `prctl(PR_SET_CHILD_SUBREAPER, 1)` on root
- [x] `prctl(PR_SET_PDEATHSIG, SIGKILL)` in child pre_exec

### Platform: macOS (`src/platform/unix.rs`)
- [x] Pipe trick as primary mechanism (shared with Linux)
- [ ] kqueue `EVFILT_PROC` + `NOTE_EXIT` watchdog (optional secondary, not implemented)

### Platform: Windows (`src/platform/windows.rs`)
- [x] Job Object with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`
- [x] `AssignProcessToJobObject` for each child
- [x] Named pipe at `\\.\pipe\psy-<pid>` (tokio named pipe server in root.rs)
- [x] `CTRL_BREAK_EVENT` for graceful stop
- [x] `TerminateProcess` fallback
- [x] `CREATE_NEW_PROCESS_GROUP` for child spawning
- [x] `CreatePipe` for death pipe (inheritable handles)
- [x] Death pipe created; watchdog thread is no-op (Job Object is primary mechanism)

### MCP Server (`src/mcp.rs`)
- [x] JSON-RPC over stdin/stdout transport
- [x] `psy_run` tool — launch process, return status/name/pid
- [x] `psy_ps` tool — list processes as plain text table
- [x] `psy_logs` tool — get logs with tail/stream params
- [x] `psy_stop` tool — stop named process
- [x] `psy_restart` tool — restart named process
- [x] `psy_history` tool — show run history for a process
- [x] `psy_psyfile_schema` tool — return Psyfile JSON Schema
- [x] `psy_logs` tool — `probe` parameter for probe log streams
- [x] `psy_send` tool — write to process stdin (interactive mode)
- [x] `psy_send` tool — `wait` parameter for blocking send with output collection
- [x] `psy_logs` tool — `format` parameter: `lines` (default, compact) or `structured` (JSON objects)
- [x] `psy_run` tool — `interactive` parameter for stdin pipe
- [x] Connect to root via `PSY_SOCK` internally

### Log Output Format
- [x] Prefix lines: `[<ISO8601> stdout/stderr] <content>`
- [x] Interleaved by default, `--stdout`/`--stderr` to filter
- [x] Probe log streams: `probe:stdout`, `probe:stderr` (hidden by default, shown with `--probe`)

### Readiness Probes & Health Checks (`src/probe.rs`)
- [x] Probe execution engine: tcp, http, exec, exit probe types
- [x] Readiness probes (`ready`): one-time startup check, dependents wait for success
- [x] Health checks (`healthcheck`): continuous monitoring, failure triggers restart per policy
- [x] Probe cancellation via watch channel on process stop/restart
- [x] Dependency readiness waiting: dependents block until upstream ready probe passes
- [x] Restart cascades: `depends_on = [{ name = "x", restart = true }]` propagates restarts
- [x] Exit probes handled in monitor_child (not polling-based)
- [x] TCP probe: `tokio::net::TcpStream::connect` with 1s per-attempt timeout
- [x] HTTP probe: raw TCP + HTTP/1.0 GET, check for 2xx status
- [x] Exec probe: shell command, check exit 0, capture stdout/stderr (up to 256 bytes)
- [x] Probe logging to ring buffer using `probe:stdout`/`probe:stderr` streams
- [x] `psy ps` READY column: `-`/`waiting`/`ready`/`failed`
- [x] `psy logs --probe` flag to view probe logs

### Psyfile (`src/psyfile.rs`)
- [x] TOML parsing with field validation (reject unknown fields)
- [x] File discovery: walk upward from cwd for `Psyfile` or `Psyfile.toml`
- [x] Unit definition: command, restart, env, depends_on, singleton, working_dir, ready, healthcheck, interactive
- [x] Environment variable interpolation: `${VAR}` and `${VAR:-default}`
- [x] Circular dependency detection (Kahn's algorithm)
- [x] Dependency resolution: topological sort for start order
- [x] Shell escaping and `$@` substitution for extra args
- [x] Singleton units: single instance, tombstone replacement
- [x] Template units: `singleton = false`, numbered instances (name.1, name.2, ...)
- [x] Template group operations: stop/restart/logs apply to all instances
- [x] Boot-time unit startup via `psy up <units>` or `psy up --all`
- [x] Working directory support per unit
- [x] CLI restart policy override (`--restart` overrides Psyfile default)
- [x] Ad-hoc processes work alongside Psyfile units
- [x] MCP `psy_run` updated for optional command + extra args
- [x] Hot-reload: Psyfile re-read from disk on every run/stop/restart/logs command
- [x] Psyfile can be created/modified after `psy up` — changes take effect immediately
- [x] Extended `depends_on` syntax: string or `{ name, restart }` table entries
- [x] Probe config parsing: `ready` and `healthcheck` tables with type/interval/timeout/retries
- [x] Duration parsing helper: `Nms`, `Ns`, `Nm`, `Nh` format
- [x] `exit` probe type restricted to `ready` only (not `healthcheck`)
- [x] JSON Schema generation for Psyfile format (`json_schema()`)
- [x] `psy psyfile schema` — output JSON Schema
- [x] `psy psyfile validate [--file]` — validate Psyfile
- [x] `psy psyfile init` — generate starter Psyfile
- [x] Multi-platform support: `platforms` field to restrict units to specific OSes
- [x] Multi-platform support: `platform.<os>` overrides for command, env, restart, depends_on, working_dir, ready, healthcheck
- [x] Platform override env merge: platform env merged on top of base (platform wins on conflict)
- [x] Platform filtering at parse time: excluded units removed, downstream sees resolved units
- [x] Platform validation: reject unknown platform names and override fields

### Edge Cases
- [x] Concurrent `psy run` same name → error "already exists"
- [x] `psy run` after main exits → error "shutting down"
- [x] Nested `psy up` inside another → works, independent roots
- [x] Large log output → ring buffer enforces limit, no unbounded growth
- [ ] Grandchildren: Linux subreaper adopts; Windows Job Object covers; macOS pipe trick only direct children (known limitation — documented)
- [ ] Tombstone replacement: if exited process name reused, replace old entry
- [x] No Psyfile + `psy run` without command → error "no command provided"
- [x] Psyfile unit name `main` → rejected at validation time

### Unit Tests
- [x] Ring buffer: boundary conditions, eviction, line counting
- [x] Ring buffer: 2MB size limit enforcement
- [ ] Protocol: serialization/deserialization roundtrip
- [x] Process name validation
- [x] Restart backoff calculation
- [x] Restart policy logic (should_restart)
- [x] Psyfile parsing: valid, minimal, missing command, unknown field, invalid TOML
- [x] Psyfile validation: circular deps, unknown dep, reserved name, invalid name
- [x] Psyfile interpolation: vars, defaults, undefined, no recursion
- [x] Psyfile dependency resolution: no deps, chain, diamond, already included
- [x] Shell escaping: simple, spaces, quotes, empty, join
- [x] Command building: append, `$@` substitution, `$@` no args
- [x] Psyfile `depends_on` extended syntax: mixed string/table, table-only, default restart
- [x] Psyfile probe parsing: tcp, tcp port number, http, exec, exit variants
- [x] Psyfile probe validation: no type, multiple types, exit rejected for healthcheck
- [x] Psyfile probes: custom interval/timeout/retries, both ready+healthcheck
- [x] Duration parsing: milliseconds, seconds, minutes, hours, invalid input
- [x] Platform overrides: command, env merge, restart, depends_on, working_dir, ready probe
- [x] Platform filtering: excludes unit, includes current, omitted includes all
- [x] Platform validation: invalid platform name, invalid override key, invalid override field
- [x] Platform with platforms restriction: combined platforms + platform override

### Integration Tests (`tests/integration.rs`)
All integration tests pass on macOS. Must also pass on Linux and Windows via GitHub Actions.

- [x] `psy up -- sleep 60` + `psy ps` → process listed
- [x] `psy run worker -- echo hello` + `psy logs worker` → output captured
- [x] `psy run crasher -- sh -c "exit 1"` with `--restart on-failure` → restart count increments, eventually fails
- [x] `psy up -- sleep 1` → all children killed after main exits
- [ ] Kill psy root with SIGKILL → children also dead (platform cleanup test — not yet written)
- [x] Multiple concurrent psy roots → no cross-talk
- [x] `psy stop worker` → SIGTERM then SIGKILL sequence
- [ ] `psy logs worker -f` → streaming works, terminates on disconnect (not yet written)
- [x] `psy down` → all processes terminated, socket removed
- [x] Name validation: reject invalid names
- [x] Duplicate name rejection
- [x] Main process exit code propagation
- [x] Environment variable passing
- [x] Log tail limiting
- [x] Psyfile unit run: command resolved from file
- [x] Psyfile unit with env interpolation
- [x] Psyfile depends_on: auto-starts dependencies
- [x] Psyfile template unit: creates numbered instances
- [x] Psyfile template group stop
- [x] Psyfile `psy up --all`: starts all units
- [x] Psyfile selective boot: `psy up db api`
- [x] Psyfile ad-hoc alongside Psyfile units
- [x] Psyfile env interpolation with default values
- [x] Psyfile restart policy override
- [x] Psyfile working_dir support
- [x] No command without Psyfile → error
- [x] Ready exit probe: build step with `ready = { exit = 0 }`, dependent waits
- [x] Ready exec probe: exec-based readiness with dependency
- [x] Ready TCP probe: TCP readiness with probe logs
- [x] Probe logs hidden by default, shown with `--probe`
- [x] Probe log stream filters: `--probe --stdout`, `--probe --stderr`
- [x] `psy ps` READY column display
- [x] Extended `depends_on` with `restart = true` flag
- [x] Healthcheck failure triggers restart
- [x] Restart cascade with readiness waiting
- [x] `psy psyfile schema` outputs valid JSON
- [x] `psy psyfile validate` succeeds/fails appropriately
- [x] `psy psyfile init` creates file, fails on existing
- [x] Platform override command: overridden command runs instead of base
- [x] Platform excluded unit: `psy run` fails, not visible
- [x] Platform `psy up --all` skips excluded units
- [x] Platform env merge: base preserved, platform overrides/adds
- [x] `psy send` basic: send text, appears in logs
- [x] `psy send` multiple lines
- [x] `psy send` on non-interactive process → error
- [x] `psy send --eof` closes stdin, further sends error
- [x] `psy send` to nonexistent process → error
- [x] `psy send` Psyfile `interactive = true` works
- [x] `psy send --file` pipes file contents
- [x] `psy send --raw` no newline appended
- [x] `psy send` to stopped process → error
- [x] `psy send --wait` basic: send to cat, verify echoed output returned
- [x] `psy send --wait` prompt: early return when prompt pattern matched
- [x] `psy send --wait` timeout: partial output returned on timeout
- [x] `psy send --wait` non-interactive error
- [x] Interactive process with dependencies
- [x] Psyfile arg append: `psy run unit -- extra-arg`
- [x] Psyfile `$@` substitution and no-args
- [x] `psy psyfile schema` includes `interactive` field
- [x] `psy version` shows current version
- [x] Stop main rejected
- [x] Run after down rejected
- [x] Logs stderr filter
- [x] Psyfile circular dep error
- [x] Psyfile unknown dep error
- [x] Psyfile unknown field error
- [x] Template group restart
- [x] Multiple restarts preserve history

### CI / GitHub Actions (`.github/workflows/ci.yml`)
- [x] Matrix: ubuntu-latest, macos-latest, windows-latest
- [x] Steps: checkout, install Rust toolchain, cargo build, cargo test, cargo test --ignored (integration)
- [x] Platform-specific test adaptations (sh vs cmd, signal vs terminate)

## Release Checklist

Before tagging a release:

1. **Check for uncommitted changes:** `git status` — everything that should be committed must be committed before pushing/tagging
2. **Version bump:** Update `version` in `Cargo.toml` to match the release tag
3. **Formatting:** `cargo fmt -- --check` — must pass with no diffs
4. **Linting:** `cargo clippy -- -D warnings` — must pass with no warnings
5. **Unit tests:** `cargo test` — all must pass
6. **Integration tests:** `cargo build && cargo test -- --ignored` — all must pass
7. **README.md:** Must document all user-facing features for the release. Check new CLI commands, Psyfile fields, MCP tools, and behavioral changes are covered
8. **CLAUDE.md:** Implementation status checkboxes must be up-to-date
9. **CI:** Push to main first, verify GitHub Actions pass on all platforms (Linux, macOS, Windows) before tagging
10. **Tag:** `git tag vX.Y.Z && git push origin vX.Y.Z`

**Important:** Never amend a commit after it has been tagged as a release. All checks above must pass *before* tagging. If something was missed, make a new commit (and a patch release if needed).

## Conventions

- Async runtime: tokio
- Serialization: serde + serde_json, NDJSON over sockets
- CLI parsing: clap derive API
- Error handling: Box<dyn Error> / Result types (no anyhow)
- Platform code: `src/platform/unix.rs` (shared Linux+macOS with cfg), `src/platform/windows.rs`
- Socket path: `/tmp/psy-<uid>/<pid>.sock` on macOS, `$XDG_RUNTIME_DIR/psy/<pid>.sock` on Linux (fallback `/tmp/...`)
- Windows IPC: named pipe `\\.\pipe\psy-<pid>` (tokio named pipe server)
