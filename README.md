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

Or use it as a local dev environment:

```bash
psy up
psy run api -- cargo run --bin api-server
psy run frontend -- npm run dev
psy run worker -- python worker.py
psy ps
# Exit the shell → everything cleaned up
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
psy up [--name <name>] [-- <command>]              Start a psy root session
psy run <name> [--restart <policy>] [-- <cmd>]     Launch a managed child process
psy run --attach <name> [-- <cmd>]                 Launch and attach stdin/stdout
psy ps                                              List managed processes
psy logs <name> [-f] [--tail <n>]                   View captured logs
psy history <name>                                  Show run history
psy stop <name>                                     Stop a process (SIGTERM → SIGKILL)
psy restart <name>                                  Restart with same arguments
psy down                                            Tear down everything
psy mcp                                             Start MCP JSON-RPC server
psy version                                         Print version
```

### Process status

```
$ psy ps
NAME                 PID      STATUS     EXIT     UPTIME         RESTARTS   RESTART
------------------------------------------------------------------------------
main                 12345    running    -        2h 13m 4s      0          no
server               12350    running    -        1h 58m 2s      0          on-failure
worker               -        stopped    0        -              0          no
crasher              -        failed     1        -              5          on-failure
```

Stopped processes remain visible as tombstones. Re-running `psy run` with the same name replaces a stopped or failed process.

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
psy logs server --until 2026-03-12T20:00:00Z
psy logs server --grep "error"     # case-insensitive filter
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

### Attach mode

```bash
psy run --attach myrepl -- python3 -i
```

Attach connects your terminal's stdin to the child process and streams its output back. Ctrl-C detaches without killing the child — it keeps running in the background and you can reattach to its logs with `psy logs -f`.

## MCP Integration

psy includes a built-in MCP server. Configure `psy mcp` as an MCP server in your agent's config — it inherits `PSY_SOCK` from the psy session and relays tool calls to the root:

```bash
psy up -- claude
# Claude's MCP config launches "psy mcp" → connects back to the root via PSY_SOCK
```

Tools exposed: `psy_run`, `psy_ps`, `psy_logs`, `psy_stop`, `psy_restart`, `psy_history`

## How It Works

psy creates a Unix domain socket (or named pipe on Windows) and manages a process table in memory. Child processes inherit `PSY_SOCK` and `PSY_ROOT_PID` environment variables, allowing any process in the tree to communicate with the root.

**Cleanup guarantees:**
- **Linux** — `PR_SET_CHILD_SUBREAPER` + `PR_SET_PDEATHSIG` ensures children die with the parent
- **macOS** — Pipe trick: children detect parent death via EOF on an inherited file descriptor
- **Windows** — Job Object with `KILL_ON_JOB_CLOSE` terminates all descendants

**Signal handling:** SIGTERM and SIGINT on the root trigger graceful teardown of all children before exit.

## License

MIT
