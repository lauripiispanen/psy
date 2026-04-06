# psy

> *"ps... why?"*

A single-binary process supervisor that creates an isolated process tree. All child processes are guaranteed to be killed when psy exits, even on crash. Think "docker compose for raw processes" — without containers, images, or daemons.

## Why

AI coding agents (Claude Code, Codex, Cursor, etc.) often need long-running sidecar processes — dev servers, watchers, databases. These processes should share the agent's lifecycle and be discoverable from within the agent's shell. psy makes that trivial.

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

**Auto-discovery:** When `PSY_SOCK` is not set, psy automatically discovers the nearest running root by matching PID ancestor chains. This means you can open a new terminal window and run `psy ps`, `psy logs`, etc. without being inside the psy session — psy finds the right root automatically.

**Cleanup guarantees:**
- **Linux** — `PR_SET_CHILD_SUBREAPER` + `PR_SET_PDEATHSIG` ensures children die with the parent
- **macOS** — Pipe trick: children detect parent death via EOF on an inherited file descriptor
- **Windows** — Job Object with `KILL_ON_JOB_CLOSE` terminates all descendants

**Signal handling:** SIGTERM and SIGINT on the root trigger graceful teardown of all children before exit.

## License

MIT
