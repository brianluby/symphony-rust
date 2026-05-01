# Symphony (Rust)

Reference implementation of the [Symphony specification](https://github.com/openai/symphony/blob/main/SPEC.md) in Rust.

Symphony is an orchestrator that turns an issue tracker (Linear) into a control plane for long-running coding agents. It continuously polls the tracker, ensures each active issue has a dedicated agent run in an isolated per-issue workspace, restarts stalled agents, manages retries, and preserves workspaces for inspection.

## Features

- **Polling orchestration** — tokio-based event loop with configurable intervals
- **Linear tracker adapter** — GraphQL client for fetching candidate issues and reconciliation
- **Mock tracker** — built-in mock for local development without Linear credentials
- **Deterministic workspaces** — per-issue isolated directories with sanitized naming
- **Workspace lifecycle hooks** — `after_create`, `before_run`, `after_run`, `before_remove`
- **Agent runner** — launches coding-agent subprocess over stdio (Codex app-server protocol)
- **Mock agent** — example binary that speaks the agent protocol for testing
- **Exponential backoff retry** — configurable retry with continuation after normal exits
- **Stall detection** — terminates stuck agent sessions
- **Concurrency control** — global and per-state slot limits via tokio Semaphore
- **Liquid template rendering** — strict-mode prompt rendering with issue/attempt variables
- **Structured logging** — tracing-based JSON or text logs with issue/session context
- **HTTP API** — optional `/health`, `/api/v1/state`, and `/api/v1/refresh` endpoints
- **Dynamic WORKFLOW.md reload** — file-watch support for live config changes

## Quick Start

### Prerequisites

- Rust 1.85+ (edition 2024)

### Build

```bash
cargo build --release
```

### Run with mock tracker (no Linear credentials needed)

```bash
cargo run -- \
  --workflow-path ./examples/sample_WORKFLOW.md \
  --workspace-root ./workspaces \
  --mock-tracker \
  --mock-agent
```

This starts the orchestrator with a mock tracker containing sample issues, creates per-issue workspaces under `./workspaces/`, and simulates agent runs.

### Run with real Linear

Set your Linear API key:

```bash
export LINEAR_API_KEY="lin_api_..."
```

Then run:

```bash
cargo run -- --workflow-path ./WORKFLOW.md
```

### Enable HTTP observability

```bash
cargo run -- --port 8080 --mock-tracker --mock-agent
```

Then:
- `GET http://localhost:8080/health` — health check
- `GET http://localhost:8080/api/v1/state` — runtime state snapshot
- `POST http://localhost:8080/api/v1/refresh` — trigger immediate poll

### JSON logging

```bash
cargo run -- --json-logs
```

## Configuration

All runtime behavior is driven by `WORKFLOW.md`. See `examples/sample_WORKFLOW.md` for a documented example.

Key config sections:

| Section | Purpose |
|---------|---------|
| `tracker` | Linear API credentials, project slug, active/terminal states |
| `polling` | Poll interval in ms |
| `workspace` | Root directory, path resolution |
| `hooks` | Shell scripts for workspace lifecycle |
| `agent` | Concurrency, max turns, retry backoff |
| `codex` | Agent command, timeouts, stall detection |

Environment variable indirection is supported: use `$VAR_NAME` in config values.

## Project Structure

```
src/
├── main.rs            # CLI, bootstrap, workflow loading
├── types.rs           # Core domain model (Issue, Session, State, etc.)
├── config.rs          # Workflow loader, typed config, validation
├── template.rs        # Liquid template rendering (strict mode)
├── workspace.rs       # Workspace manager, hooks, lifecycle
├── tracker.rs         # Linear client (GraphQL) + mock tracker
├── orchestrator.rs    # Poll loop, dispatch, retry, reconciliation
├── agent_runner.rs    # Subprocess agent, JSON-RPC streaming
├── http.rs            # Optional HTTP API server
└── logging.rs         # Tracing subscriber setup

examples/
├── sample_WORKFLOW.md # Annotated workflow config
└── mock_agent.rs      # Mock agent binary for testing
```

## Testing

```bash
# Unit tests (no external services)
cargo test

# With output
cargo test -- --nocapture

# Specific module
cargo test config
```

Currently 22 unit tests covering:
- Workflow YAML parsing and validation
- Template rendering (Liquid strict mode, unknown variable rejection)
- Workspace creation, sanitization, removal
- Config defaults, env var expansion, path resolution
- Issue state normalization
- Tracker response parsing
- Orchestrator backoff computation

## Running the Mock Agent

The mock agent example simulates a coding-agent session:

```bash
cargo run --example mock_agent
```

Outputs JSON-RPC events to stdout mimicking a real Codex app-server session.

## CLI Options

```
--workflow-path     Path to WORKFLOW.md [default: ./WORKFLOW.md]
--workspace-root    Override workspace root directory
--port              HTTP server port (0 = disabled) [default: 0]
--mock-tracker      Use built-in mock tracker
--mock-agent        Use mock agent mode
--json-logs         Emit JSON-formatted logs
--verbose, -v       Verbose logging
```

## Architecture

Symphony follows the SPEC.md component model:

1. **Policy Layer** — `WORKFLOW.md` defines team rules and agent prompt
2. **Configuration Layer** — typed config with defaults and env expansion
3. **Coordination Layer** — orchestrator poll loop with state machine
4. **Execution Layer** — workspace creation + agent subprocess
5. **Integration Layer** — Linear GraphQL adapter
6. **Observability Layer** — structured logs + optional HTTP API

## SPEC Conformance

This implementation targets the MVP conformance profile from SPEC.md §18.1:

- [x] Workflow path selection (explicit + cwd default)
- [x] WORKFLOW.md loader with YAML front matter + prompt body
- [x] Typed config layer with defaults and `$` resolution
- [x] Dynamic WORKFLOW.md watch/reload (notify watcher)
- [x] Polling orchestrator with single-authority mutable state
- [x] Issue tracker client (Linear + mock)
- [x] Workspace manager with sanitized per-issue workspaces
- [x] Workspace lifecycle hooks
- [x] Hook timeout config
- [x] Coding-agent app-server subprocess client
- [x] Codex launch command config
- [x] Strict prompt rendering with `issue` and `attempt` variables
- [x] Exponential retry queue with continuation retries
- [x] Configurable retry backoff cap
- [x] Reconciliation (terminal/non-active state stops)
- [x] Workspace cleanup (startup sweep + active transition)
- [x] Structured logs with issue/session context
- [x] Operator-visible observability

## License

MIT — see the [upstream specification](https://github.com/openai/symphony) for details.
