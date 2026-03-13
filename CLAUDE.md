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
│   ├── mcp.rs               # MCP server implementation
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
- [x] `src/main.rs` — CLI arg parsing with clap (up, run, ps, logs, stop, restart, down, mcp, version)
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
- [x] Handle `down` command — teardown all children in reverse order
- [x] Reject `run` during teardown ("shutting down")
- [x] Reject `stop main` (must use `down`)
- [x] Name validation: `[a-zA-Z0-9][a-zA-Z0-9_-]{0,62}`

### CLI Commands
- [x] `psy up` — create root, launch shell (`$SHELL` or default)
- [x] `psy up -- <command>` — create root, launch command as main
- [x] `psy up --name` — custom root name (default: `psy-<pid>`)
- [x] ~`psy up --mcp`~ — removed; agent launches `psy mcp` itself via MCP config
- [x] `psy run <name> -- <command>` — send run command to root via PSY_SOCK
- [x] `psy run --restart <policy>` — restart policy
- [x] `psy run --env KEY=VAL` — extra env vars
- [ ] `psy run --attach` — connect terminal stdin/stdout to child
- [x] `psy ps` — send ps command, format table output
- [x] `psy logs <name>` — dump captured logs
- [x] `psy logs --tail <n>` — last N lines
- [x] `psy logs -f` — follow mode (stream until Ctrl-C)
- [x] `psy logs --stdout / --stderr` — filter by stream
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
- [ ] Signal handling: SIGTERM, SIGINT → initiate teardown (not yet wired)

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
- [x] Connect to root via `PSY_SOCK` internally

### Log Output Format
- [x] Prefix lines: `[<ISO8601> stdout/stderr] <content>`
- [x] Interleaved by default, `--stdout`/`--stderr` to filter

### Edge Cases
- [x] Concurrent `psy run` same name → error "already exists"
- [x] `psy run` after main exits → error "shutting down"
- [x] Nested `psy up` inside another → works, independent roots
- [x] Large log output → ring buffer enforces limit, no unbounded growth
- [ ] Grandchildren: Linux subreaper adopts; Windows Job Object covers; macOS pipe trick only direct children (known limitation — documented)
- [ ] Tombstone replacement: if exited process name reused, replace old entry

### Unit Tests
- [x] Ring buffer: boundary conditions, eviction, line counting
- [x] Ring buffer: 2MB size limit enforcement
- [ ] Protocol: serialization/deserialization roundtrip
- [x] Process name validation
- [x] Restart backoff calculation
- [x] Restart policy logic (should_restart)

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

### CI / GitHub Actions (`.github/workflows/ci.yml`)
- [x] Matrix: ubuntu-latest, macos-latest, windows-latest
- [x] Steps: checkout, install Rust toolchain, cargo build, cargo test, cargo test --ignored (integration)
- [x] Platform-specific test adaptations (sh vs cmd, signal vs terminate)

## Conventions

- Async runtime: tokio
- Serialization: serde + serde_json, NDJSON over sockets
- CLI parsing: clap derive API
- Error handling: Box<dyn Error> / Result types (no anyhow)
- Platform code: `src/platform/unix.rs` (shared Linux+macOS with cfg), `src/platform/windows.rs`
- Socket path: `/tmp/psy-<uid>/<pid>.sock` on macOS, `$XDG_RUNTIME_DIR/psy/<pid>.sock` on Linux (fallback `/tmp/...`)
- Windows IPC: named pipe `\\.\pipe\psy-<pid>` (tokio named pipe server)
