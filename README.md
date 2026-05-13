# psy

> *"ps... why?"*

A cross-platform process supervisor: a single-binary CLI **and** an embeddable Rust library. Manages an isolated process tree where all child processes are guaranteed to be killed when psy exits — even on crash. Think "docker compose for raw processes" — without containers, images, or daemons.

## Why

AI coding agents (Claude Code, Codex, Cursor, etc.) often need long-running sidecar processes — dev servers, watchers, databases. These processes should share the agent's lifecycle and be discoverable from within the agent's shell. psy makes that trivial.

Other use cases:

- **Embeddable supervisor** for desktop apps (Tauri, Electron-via-FFI) that want fork+exec-free supervision of their own children. Add `psy-core` as a dependency and you get cleanup guarantees, restart policies, log buffering, and Psyfile loading without shipping a separate binary.
- **CI / parallel-test orchestration** where each test run gets a clean process tree that's reliably torn down even if the harness crashes.
- **Per-tenant isolation** via sub-roots — supervise multiple independent worlds under one umbrella, with each world's processes hidden from the others.

## Quick Start

```bash
# Launch psy with claude as the main process
psy up -- claude

# Inside the session, start sidecars
psy run dev-server -- npm run dev
psy run db -- docker run --rm -p 5432:5432 postgres
psy logs dev-server --tail 20
psy ps

# When claude exits, everything tears down automatically
```

Or use a **Psyfile** to define your stack declaratively:

```toml
# Psyfile
[db]
command = "docker run --rm -p 5432:5432 postgres:16"
restart = "always"
ready = { tcp = 5432 }

[api]
command = "cargo run --bin api-server"
restart = "on-failure"
depends_on = [{ name = "db", restart = true }]
healthcheck = { http = "http://localhost:3000/health", interval = "10s", retries = 3 }

[frontend]
command = "npm run dev"
depends_on = ["api"]
```

```bash
psy up --all -- claude
# db starts first, api waits for port 5432, frontend waits for api
# When claude exits, everything tears down automatically
```

## Install

### Homebrew (macOS / Linux)

```bash
brew tap lauripiispanen/tap
brew install psy
```

### From source

```bash
cargo install --path .
```

### Pre-built binaries

Download from [GitHub Releases](https://github.com/lauripiispanen/psy/releases) — builds available for x86_64 and aarch64 on Linux, macOS, and Windows.

## CLI

```
psy up [--name <name>] [units...] [-- <command>]   Start a psy root session
psy up --all [-- <command>]                        Start all Psyfile units
psy up --mcp [--all] [units...]                    Start root with MCP server on stdin/stdout
psy up --parent <SOCK> -- <command>                Start as a managed sub-root of an existing psy
psy run <name> [--restart <policy>] [-- <cmd>]     Launch a managed child process
psy run --attach <name> [-- <cmd>]                 Launch and attach stdin/stdout
psy run --interactive <name> [-- <cmd>]            Launch with writable stdin pipe
psy run --port <name[=port]> <name> [-- <cmd>]      Allocate named ports for ad-hoc processes
psy run --wait-for <cond> <name> [-- <cmd>]        Launch and block until condition met
psy send <name> "text"                              Write text to a process's stdin
psy send --wait <name> "text"                       Send and wait for output
psy send --raw <name> "text"                        Write without appending newline
psy send --eof <name>                               Close a process's stdin
psy send --file <path> <name>                       Pipe file contents to stdin
psy ps                                              List managed processes
psy logs <name> [-f] [--tail <n>] [--probe]         View captured logs
psy history <name>                                  Show run history
psy stop <name>                                     Stop a process (SIGTERM → SIGKILL)
psy restart <name>                                  Restart with same arguments
psy clean                                           Remove stopped/failed processes
psy down                                            Tear down everything
psy psyfile schema                                  Output JSON Schema for Psyfile
psy psyfile validate [--file <path>]                Validate a Psyfile
psy psyfile init                                    Generate a starter Psyfile
psy mcp                                             Start MCP JSON-RPC server
psy version                                         Print version
```

### Process status

```
$ psy ps
NAME                 PID      STATUS     READY    EXIT     UPTIME         RESTARTS   RESTART
--------------------------------------------------------------------------------------
main                 12345    running    -        -        2h 13m 4s      0          no
server               12350    running    ready    -        1h 58m 2s      0          on-failure
db                   12348    running    ready    -        2h 13m 1s      0          always
worker               -        stopped    -        0        -              0          no
crasher              -        failed     -        1        -              5          on-failure
```

Stopped processes remain visible as tombstones. Re-running `psy run` with the same name replaces a stopped or failed process. The READY column shows readiness probe status (`ready`, `waiting`, `failed`, or `-` if no probe).

### Restart policies

- `no` (default) — don't restart on exit
- `on-failure` — restart on non-zero exit, up to 5 times with exponential backoff
- `always` — restart unconditionally, same backoff and limit

### Logs

```bash
psy logs server                    # full log (plain text)
psy logs server --tail 20          # last 20 lines
psy logs server -f                 # follow (stream until Ctrl-C)
psy logs server --stdout           # stdout only
psy logs server --stderr           # stderr only
psy logs server --since 5m         # last 5 minutes
psy logs server --since 1h         # last hour
psy logs server --since last       # only new logs since last request
psy logs server --until 2026-03-12T20:00:00Z
psy logs server --grep "error"     # case-insensitive regex filter
psy logs server --grep "err.*timeout"  # regex pattern
psy logs server -f --grep "WARN"   # follow with filter
```

Output format: `[2025-03-12T10:15:32.123Z stdout] Server listening on :8080`

Logs are kept in a per-process ring buffer (10k lines / 2MB). Each run gets its own log buffer — logs from previous runs are preserved and queryable.

### Run history

Every time a process starts (via `psy run`, `psy restart`, or automatic restart), a new run is recorded. Use `psy history` to see all runs, then query logs by run ID:

```
$ psy history web
RUN    STATUS     EXIT     STARTED                      DURATION
--------------------------------------------------------------------
1      stopped    SIG15    2026-03-13T20:10:39+00:00    1m 47s
2      stopped    SIG15    2026-03-13T20:12:26+00:00    14s
3      running    -        2026-03-13T20:12:40+00:00    1m 2s
```

```bash
psy logs web                # current run (default)
psy logs web --run 1        # first run's logs
psy logs web --run 2        # second run's logs
psy logs web --previous     # shorthand for the run before current

# Composes with all other flags:
psy logs web --run 1 --grep "error"
psy logs web --previous --tail 5
```

### Probe logs

Readiness probes and health checks write diagnostic output to separate log streams. These are hidden by default to keep `psy logs` clean:

```bash
psy logs server --probe             # all probe output
psy logs server --probe --stderr    # probe diagnostics only
psy logs server --probe --stdout    # probe command stdout only
```

### Attach mode

```bash
psy run --attach myrepl -- python3 -i
```

Attach connects your terminal's stdin to the child process and streams its output back. Ctrl-C detaches without killing the child — it keeps running in the background and you can reattach to its logs with `psy logs -f`.

### Interactive stdin

By default, child processes have their stdin connected to `/dev/null`. The interactive mode opens a writable stdin pipe, letting you send input to a running process programmatically:

```bash
# Start a process with interactive stdin
psy run --interactive myproc -- cat

# Send a line (newline auto-appended)
psy send myproc "hello world"

# Send without trailing newline
psy send --raw myproc "no newline"

# Pipe a file's contents to stdin
psy send --file data.txt myproc

# Close stdin (EOF) — permanent, cannot reopen
psy send --eof myproc
```

Design notes:
- Uses a pipe, not a PTY — simple and cross-platform
- `psy send` appends a newline by default; use `--raw` to send exactly what you pass
- `--eof` closes the pipe permanently; further sends return an error
- If the pipe buffer is full, `psy send` blocks for up to 5 seconds before returning a backpressure error
- Sending to a process that was not started with `--interactive` (or `interactive = true` in Psyfile) returns an error

### Blocking send (`--wait`)

For REPL-like interactions (psql, python, debuggers), `--wait` sends input and collects output in a single call — no need for a sleep-then-logs dance:

```bash
# Send a command and get the output back
psy send --wait myrepl "SELECT 1;"

# With prompt detection (returns as soon as the prompt appears)
psy send --wait --wait-prompt ">>>" myrepl "print(2+2)"

# Custom timeouts
psy send --wait --wait-timeout 10s --idle-timeout 500ms myrepl "slow_query()"
```

Options:
- `--wait-timeout` (default `5s`) — overall timeout; returns collected output when reached
- `--idle-timeout` (default `200ms`) — stop collecting after this long with no new output
- `--wait-prompt` — return early when output contains this substring (case-insensitive)

Durations support `ms`, `s`, `m`, and `h` suffixes (e.g. `200ms`, `5s`, `2m`).

### Blocking run (`--wait-for`)

Launch a process and block until a condition is met — useful for build steps, migrations, or waiting for services to become ready:

```bash
# Wait for the ready probe to pass
psy run --wait-for ready db -- docker run --rm -p 5432:5432 postgres

# Wait for the process to exit (returns exit code + tail logs)
psy run --wait-for exit migration -- cargo run --bin migrate

# Wait for a log line matching a substring
psy run --wait-for-log "listening on" server -- cargo run --bin server

# Wait for a dependency's ready probe
psy run --wait-for-dep db api -- cargo run --bin api

# Custom timeout (default: 120s)
psy run --wait-for ready --wait-timeout 60s db -- docker run postgres
```

The response includes enriched status (ready state, exit code, logs) depending on the condition type. If the timeout expires, `timed_out: true` is included along with whatever status was available.

### Cleaning up

Stopped and failed processes remain in the process table as tombstones. To remove them:

```bash
psy clean    # removes all stopped/failed processes
```

## Embedding (Rust library)

psy ships as a workspace of crates. The CLI binary (`psy`) is a thin shell over the library; hosts that want supervision in-process can depend on `psy-core` directly:

```toml
[dependencies]
psy-core = "2"
tokio = { version = "1", features = ["full"] }
```

```rust
use psy_core::{
    DependencyRef, PsyRoot, ReadyProbe, RestartPolicy, RootOptions, Spawn,
};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Call this as the very first thing in main(). On macOS, when the
    // sidecar re-spawns this binary with the cleanup sentinel, this call
    // intercepts and exits the process. No-op on Linux / Windows.
    psy_core::dispatch_macos_cleanup_if_invoked();

    let root = PsyRoot::start(RootOptions::new("my-host")).await?;

    // Programmatic dependency graph — equivalent of a Psyfile, in code.
    root.spawn(
        Spawn::new("postgres", ["postgres", "-D", "/var/lib/pg"])
            .with_ready(ReadyProbe::Tcp {
                addr: "localhost:5432".into(),
                interval: None,
                timeout: None,
                retries: None,
            })
            .with_restart(RestartPolicy::Always),
    )
    .await?;

    let api = root
        .spawn(
            Spawn::new("api", ["./api-server"])
                .with_depends_on(vec![
                    DependencyRef::new("postgres").with_restart(true),
                ])
                .with_restart(RestartPolicy::OnFailure),
        )
        .await?;

    // Stream stdout / wait for exit / stop directly via SpawnHandle.
    use futures_util::StreamExt;
    let mut api_stdout = Box::pin(api.stdout().await?);
    while let Some(line) = api_stdout.next().await {
        println!("[api] {}", line.content);
    }

    root.shutdown().await?;
    Ok(())
}
```

What you get embedded:

- **Same cleanup guarantees as the CLI.** Linux uses `PR_SET_CHILD_SUBREAPER` + `PR_SET_PDEATHSIG=SIGKILL`; macOS uses a per-root cleanup sidecar (kqueue `NOTE_EXIT`); Windows uses Job Objects with `KILL_ON_JOB_CLOSE`. All applied to the **host process** — your Tauri/Axum/CLI binary becomes the supervisor.
- **Programmatic `Spawn` API** with full Psyfile-equivalent surface: `argv`, `env`, `cwd`, `restart`, `interactive`, `ports`, `ready` (TCP / HTTP / Exec / Exit probes), `healthcheck` (continuous), `depends_on` (with restart cascades), `metadata` tags, `wait_for` blocking conditions. Hosts that build supervision graphs at runtime never need to touch TOML.
- **Streaming `SpawnHandle`** — `stdout()` / `stderr()` return `Stream<LogLine>`, `wait()` returns `ExitStatus`, `stop()` and `kill()` are direct methods on the handle. No polling required for reactive UIs.
- **Bidirectional stdio for framed protocols.** `SpawnHandle::write_stdin(&[u8])` and `SpawnHandle::close_stdin()` drive an interactive child's stdin directly (takes raw bytes, not `&str`, so framed protocols like JSON-RPC with `Content-Length` headers or length-prefixed binary work without UTF-8 round-tripping). Pair with `Spawn::with_raw_stdio(true)` and `SpawnHandle::stdout_bytes()` / `stderr_bytes()` to receive child output as raw `Vec<u8>` chunks — byte-exact, no line buffering, no newline stripping. The line-tokenized ring buffer feeding `psy logs` is still populated in parallel.
- **Typed error contract** — `PsyError` variants (e.g. `AlreadyExists { name }`, `NotFound { name }`, `PortAllocationFailed { port_name }`) are backed by a wire-protocol `error_code: ErrorCode` so the typed surface stays SemVer-stable across psy-core releases regardless of how human-readable error strings evolve.
- **In-process sub-roots** for per-tenant isolation: `RootHandle::sub_root(SubRootOptions::new("instance-a"))` returns a fresh `RootHandle` with its own process table, sharing the host's runtime and cleanup sidecar. One sub-root's children are invisible to siblings.
- **Optional IPC socket.** `SocketBinding::None` (default for embedded hosts) means the host's API surface is the only way in. `SocketBinding::Auto` exposes a discoverable socket so an operator running `psy ps` in another shell can introspect.
- **Live event hooks (v2.1).** `RootOptions::with_log_sink(...)` forwards every captured stdout / stderr line to a host-supplied `LogSink` (route into `tracing`, OpenTelemetry, a file logger). `RootOptions::with_on_event(...)` fires a callback on `RootEvent` lifecycle transitions (`SpawnStarted`, `SpawnReady`, `SpawnExited`, `SpawnRestarted`, `ProbeFailed`, `Shutdown`). Per-process: `SpawnHandle::events()` returns a `Stream<RootEvent>` and `SpawnHandle::pid_watch()` returns a `tokio::sync::watch::Receiver<Option<u32>>` that updates on spawn / restart / exit.
- **Runtime injection (v2.1).** `RootOptions::with_runtime(handle)` pins psy-core's long-lived background tasks (socket listener, sidecar supervisor, per-process monitors, probe loops) to a specific tokio runtime. Default is to inherit from the runtime `PsyRoot::start` is awaited from.
- **Standalone macOS cleanup sidecar (v2.1).** Hosts that don't want their main binary re-dispatchable as the cleanup sidecar can ship the standalone `psy-macos-cleanup-sidecar` shim alongside theirs and configure `RootOptions::with_sidecar_strategy(SidecarStrategy::ExternalBinary { path, sentinel })`.
- **Aggregate shutdown exit code (v2.1).** `RootHandle::shutdown` returns an `i32` derived from the supervised children's last-known exit statuses — useful for hosts that want to forward a child's failure as their own process exit.
- **Diagnostics via `tracing` (v2.1).** psy-core's internal warn / error diagnostics (sidecar respawn, listener errors, restart failures) emit `tracing::warn!` / `tracing::error!` events under target `psy`. Hosts route these through their existing observability stack; embedded hosts that don't install a subscriber stay silent.
- **Workspace crates** if you only want a piece: `psy-client` (NDJSON wire-protocol client without the supervisor), `psy-mcp` (MCP JSON-RPC server), `psy-macos-cleanup-sidecar` (standalone macOS cleanup shim).

### Embedded-mode caveats

- **Linux subreaper is process-wide.** Once `install_host_cleanup = true` (the default), your host adopts every orphaned descendant — not just psy-spawned ones. Tauri webview helpers, native messaging hosts, anything any dependency forks internally. Either tolerate them as zombies until host exit (fine for a desktop app) or wire your own reaper. Set `install_host_cleanup = false` to opt out.
- **In-process sub-roots share address space.** A panic mid-mutation of a shared `Mutex` poisons it; a blocking task in one sub-root starves the shared runtime; OOM kills everyone. The recommended default for hosts that don't need full crash isolation; switch individual sub-roots to `SubRootKind::OutOfProcess` if you need address-space isolation. (`OutOfProcess` typed-API support is **targeted for v2.2**; v2.1 hosts use `RootHandle::spawn_psy_subroot(name, binary, extra_args)` which spawns a supervised `psy up` child and returns a regular `SpawnHandle`.)
- **macOS sidecar requires host cooperation.** Calling `dispatch_macos_cleanup_if_invoked()` at the top of `main()` is mandatory on macOS embedded hosts. If your binary is re-spawned as a sidecar (which psy does to cover hard-kill cleanup), that call is what runs the sidecar logic. Hosts that can't satisfy the re-dispatch contract — no `main()` they own (library plugins, dylibs, dynamically loaded into a host they don't control), running under a test harness whose argv they can't intercept, or simply preferring to ship a separately signed shim — have two opt-out paths: ship the standalone `psy-macos-cleanup-sidecar` binary and set `SidecarStrategy::ExternalBinary { path, sentinel }`, or set `SidecarStrategy::Disabled` to skip the sidecar entirely (you lose hard-kill cleanup on macOS — fine for in-process tests, short-lived fixtures, or hosts that install their own equivalent mechanism).

## MCP Integration

psy includes a built-in MCP server. The simplest setup is `psy up --mcp` — it starts a psy root with the MCP JSON-RPC server on stdin/stdout, so you can configure it directly as your agent's MCP server:

```json
{
  "mcpServers": {
    "psy": { "command": "psy", "args": ["up", "--mcp"] }
  }
}
```

When the agent disconnects (stdin closes), psy tears down all managed processes automatically. Boot units work too: `psy up --mcp --all` or `psy up --mcp db api`.

Alternatively, `psy mcp` (without `up`) is a lightweight relay that connects to an existing root via auto-discovery or `PSY_SOCK`:

```bash
psy up -- claude
# Claude's MCP config launches "psy mcp" → discovers the root automatically
```

Tools exposed: `psy_run` (with `interactive`, `wait_for`, `ports` params), `psy_ps`, `psy_logs` (with `format` param: `lines`/`structured`, `since: "last"` for incremental viewing, `grep` regex filter), `psy_send` (with `wait` mode), `psy_stop`, `psy_restart`, `psy_history`, `psy_psyfile_schema`, `psy_clean`

## Psyfile

A Psyfile is an optional TOML file that defines named process units. Place a file named `Psyfile` or `Psyfile.toml` in your project directory. psy discovers it by walking upward from the current directory.

```toml
[postgres]
command = "docker run --rm -p ${DB_PORT:-5432}:5432 postgres:16"
restart = "always"
ready = { tcp = 5432, interval = "1s", timeout = "30s" }

[api]
command = "cargo run --bin api-server"
restart = "on-failure"
env = { DATABASE_URL = "postgres://localhost:${DB_PORT:-5432}/dev" }
depends_on = [{ name = "postgres", restart = true }]
healthcheck = { http = "http://localhost:3000/health", interval = "10s", retries = 3 }

[worker]
command = "./worker --id $PSY_INSTANCE"
singleton = false
depends_on = ["api"]
```

### Unit fields

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `command` | string | *required* | Shell command to run |
| `restart` | `"no"` / `"on-failure"` / `"always"` | `"no"` | Restart policy |
| `env` | table | `{}` | Environment variables (supports `${VAR:-default}`) |
| `depends_on` | array | `[]` | Dependencies — strings or `{ name, restart }` tables |
| `singleton` | bool | `true` | `false` = template unit (multiple instances) |
| `interactive` | bool | `false` | Enable writable stdin pipe |
| `ports` | array | `[]` | Named port allocations (strings or `{ name, default }` tables) |
| `sub_root` | bool | `false` | Run this unit as a managed sub-root with its own scoped socket |
| `working_dir` | string | cwd | Working directory |
| `ready` | table | none | Startup readiness probe |
| `healthcheck` | table | none | Continuous health check |
| `platforms` | array | all | Restrict to specific OSes (`linux`, `macos`, `windows`) |
| `platform.<os>` | table | none | Per-platform overrides for command, env, restart, etc. |

### Extra arguments

You can pass extra arguments to Psyfile units at runtime. By default they're appended to the command; use `$@` in the command for explicit placement:

```toml
[test]
command = "cargo test"

[migrate]
command = "cargo run --bin migrate -- $@"
```

```bash
psy run test -- --release             # → cargo test --release
psy run migrate -- --dry-run          # → cargo run --bin migrate -- --dry-run
psy run test                          # no extra args → cargo test
```

When `$@` is present, it's replaced with the extra args (or removed if none). Without `$@`, args are appended to the end.

### Readiness probes

A `ready` probe runs once after process start. Dependents wait for it to pass before starting.

```toml
ready = { tcp = "localhost:5432" }                    # TCP port check
ready = { tcp = 8080 }                                # shorthand (localhost:PORT)
ready = { http = "http://localhost:3000/health" }     # HTTP GET, expects 2xx
ready = { exec = "pg_isready -h localhost" }          # command, expects exit 0
ready = { exit = 0 }                                  # process itself exits with code
```

Optional timing: `interval` (default `"1s"`), `timeout` (default `"30s"`), `retries`.

### Health checks

A `healthcheck` runs continuously after the process is ready. On failure (consecutive retries exhausted), the process is killed and restarted per its restart policy.

```toml
healthcheck = { tcp = "localhost:5432", interval = "10s", retries = 3 }
healthcheck = { http = "http://localhost:3000/health", interval = "15s", retries = 5 }
healthcheck = { exec = "curl -sf http://localhost:3000/ping", interval = "10s" }
```

### Restart cascades

When a dependency restarts, dependents with `restart = true` automatically restart too:

```toml
[api]
depends_on = [{ name = "db", restart = true }]
# If db restarts, api restarts automatically (in dependency order)
```

### Port allocation

psy can dynamically allocate non-conflicting TCP ports for your services. This is especially useful when running multiple psy roots concurrently (CI, parallel test suites, multiple developers) — each root gets unique ports with zero coordination.

```toml
[db]
command = "postgres -p ${port.pg}"
ports = [{ name = "pg", default = 5432 }]
ready = { tcp = "${port.pg}" }

[api]
command = "cargo run --bin api -- --port ${port.http}"
ports = [{ name = "http", default = 8080 }, "metrics"]
env = { PORT = "${port.http}", METRICS_PORT = "${port.metrics}" }
depends_on = ["db"]

[worker]
command = "worker --db-port ${port.pg@db} --api-port ${port.http@api}"
depends_on = ["api"]
```

**Port definition formats:**
- `"http"` — dynamic port (OS-assigned)
- `{ name = "http", default = 8080 }` — tries port 8080 first, falls back to dynamic if unavailable

**How ports reach your process:**
- **Auto env vars:** `ports = ["http"]` → child gets `PSY_PORT_HTTP=<port>`
- **Interpolation:** `${port.http}` works in `command`, `env` values, and probe configs
- **Cross-unit refs:** `${port.http@api}` references another unit's port (requires `depends_on`)

**Ad-hoc processes** can also request ports via the CLI:

```bash
psy run srv --port http --port grpc -- node server.js
# Child gets PSY_PORT_HTTP and PSY_PORT_GRPC env vars
psy run srv --port http=8080 -- node server.js
# Tries port 8080 first, falls back to dynamic
```

**Automatic restart cascade:** Cross-unit port references (`${port.x@unit}`) automatically imply `restart = true` on that dependency. If the upstream unit restarts with new ports, dependents restart too.

**Port reuse on restart:** When a process restarts, psy tries to reuse its previous ports. If a port was grabbed by something else, a fresh one is allocated.

`psy ps` shows a PORTS column when any process has allocated ports.

### Multi-platform support

Restrict units to specific operating systems with `platforms`, and override fields per-platform with `platform.<os>`:

```toml
[db]
command = "docker run --rm -p 5432:5432 postgres:16"

[redis]
command = "redis-server"
platforms = ["linux", "macos"]  # not available on Windows

[cache]
command = "echo starting cache"
platform.linux.command = "redis-cli monitor"
platform.windows.command = "echo no redis on windows"
platform.macos.command = "redis-cli monitor"

[api]
command = "cargo run --bin api"
env = { PORT = "3000" }
platform.windows.env = { PORT = "3000", RUST_LOG = "debug" }
```

Platform overrides can set: `command`, `env`, `restart`, `depends_on`, `working_dir`, `ready`, `healthcheck`. Environment variables are merged (platform wins on conflict). Units excluded by `platforms` are invisible — they won't appear in `psy ps` and can't be started.

Valid platform names: `linux`, `macos`, `windows`.

### Sub-roots

A unit declared with `sub_root = true` runs as a managed sub-root: its `command` is wrapped in a fresh `psy up --parent <parent_sock>` invocation, and the resulting psy registers itself with the umbrella psy as a unit. The sub-root has its own scoped socket and manages its own children independently — they're invisible to the umbrella's other units, and the sub-root's lifecycle is controlled from the umbrella (one place to kill everything; clean teardown on umbrella crash).

```toml
[instance-a]
command = "./worker"
sub_root = true
restart = "on-failure"
ports = [{ name = "pg", default = 5432 }]
ready = { tcp = "localhost:${port.pg}" }
```

```bash
psy up --all
psy ps                                  # umbrella's units, including instance-a
psy ps --in instance-a                  # drill in: shows instance-a's own units
psy logs --in instance-a worker         # logs of instance-a's worker, from inside its tree
psy stop instance-a                     # tears down instance-a and everything inside it
```

`--in <name>` is a global flag that resolves the named sub-root unit's socket via the umbrella and proxies the command there. It's equivalent to running `psy ps`/`psy logs`/etc. from inside the sub-root.

You can also start a sub-root externally with `psy up --parent <SOCK>` — useful for tools that supervise their own sub-tenants under one umbrella psy.

**Constraints on sub-root units:**
- Incompatible with `interactive = true` (the sub-root psy owns the inner stdin pipe).
- Incompatible with `ready = { exit = ... }` (sub-root psys are long-lived).
- Probes on a sub-root unit run from the parent's perspective (e.g. `ready = { tcp = ... }` waits for the inner workload to listen). The sub-root's own units have their own probes inside the sub-root's Psyfile, independent of the parent.

**Cleanup contract:** stopping the sub-root unit from the parent SIGTERMs the sub-root's main process; the sub-root tears down its own children and exits. Killing the parent psy itself cascades through OS-level mechanisms (subreaper / pipe trick / Job Object), so grandchildren also die.

**Authorization:** the registering sub-root's PID must be a descendant of the parent psy. Unrelated processes that happen to know the parent socket path cannot inject themselves as sub-roots.

### Hot-reload

The Psyfile is re-read from disk on every command. You can create, modify, or delete it while psy is running — changes take effect immediately.

### Psyfile utilities

```bash
psy psyfile schema      # output JSON Schema (for editor validation)
psy psyfile validate    # validate current Psyfile
psy psyfile init        # generate a starter Psyfile
```

## How It Works

psy creates a Unix domain socket (or named pipe on Windows) and manages a process table in memory. Child processes inherit `PSY_SOCK` and `PSY_ROOT_PID` environment variables, allowing any process in the tree to communicate with the root.

**Workspace layout (v2.0):** `psy-core` is the supervisor library; `psy` is a thin CLI shell over it; `psy-client` is the NDJSON wire-protocol client; `psy-mcp` is the MCP JSON-RPC server. The CLI and any embedded host build on the same `psy-core` so behaviour is identical between modes.

**Auto-discovery:** When `PSY_SOCK` is not set, psy automatically discovers the nearest running root by matching PID ancestor chains. This means you can open a new terminal window and run `psy ps`, `psy logs`, etc. without being inside the psy session — psy finds the right root automatically.

**Cleanup guarantees:**
- **Linux** — `PR_SET_CHILD_SUBREAPER` + `PR_SET_PDEATHSIG` ensures children die with the parent
- **macOS** — A cleanup sidecar per root watches the parent via kqueue `NOTE_EXIT` and SIGKILLs every tracked child if the parent is hard-killed. The sidecar is a re-dispatched copy of the host binary itself; embedding hosts call `psy_core::dispatch_macos_cleanup_if_invoked()` at the top of `main()` to recognize sidecar invocations.
- **Windows** — Job Object with `KILL_ON_JOB_CLOSE` terminates all descendants

**Signal handling:** SIGTERM and SIGINT on the root trigger graceful teardown of all children before exit.

## License

MIT
