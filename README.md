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
psy up [--name <name>] [-- <command>]             Start a psy root session
psy run <name> [--restart <policy>] [-- <cmd>]   Launch a managed child process
psy ps                                            List managed processes
psy logs <name> [-f] [--tail <n>]                 View captured logs
psy stop <name>                                   Stop a process (SIGTERM → SIGKILL)
psy restart <name>                                Restart with same arguments
psy down                                          Tear down everything
psy mcp                                           Start MCP JSON-RPC server
psy version                                       Print version
```

### Restart policies

- `no` (default) — don't restart on exit
- `on-failure` — restart on non-zero exit, up to 5 times with exponential backoff
- `always` — restart unconditionally, same backoff and limit

## MCP Integration

psy includes a built-in MCP server. Configure `psy mcp` as an MCP server in your agent's config — it inherits `PSY_SOCK` from the psy session and relays tool calls to the root:

```bash
psy up -- claude
# Claude's MCP config launches "psy mcp" → connects back to the root via PSY_SOCK
```

Tools exposed: `psy_run`, `psy_ps`, `psy_logs`, `psy_stop`, `psy_restart`

## How It Works

psy creates a Unix domain socket (or named pipe on Windows) and manages a process table in memory. Child processes inherit `PSY_SOCK` and `PSY_ROOT_PID` environment variables, allowing any process in the tree to communicate with the root.

**Cleanup guarantees:**
- **Linux** — `PR_SET_CHILD_SUBREAPER` + `PR_SET_PDEATHSIG` ensures children die with the parent
- **macOS** — Pipe trick: children detect parent death via EOF on an inherited file descriptor
- **Windows** — Job Object with `KILL_ON_JOB_CLOSE` terminates all descendants

## License

MIT
