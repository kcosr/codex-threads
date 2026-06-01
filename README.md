# codex-threads

`codex-threads` is a Rust CLI for querying and controlling Codex app-server
threads across one or more named local app-server instances.

It connects to Codex app-server over WebSocket framing on Unix domain sockets
and delegates thread state, history, active turns, settings, model listing, and
control operations to Codex. It does not parse rollout files, keep a local
thread index, or merge paginated thread data across multiple servers.

## Features

- TOML configuration for named local Unix domain socket targets.
- Direct `--connect unix:///path/to.sock` debug targeting.
- Deterministic target selection with `--server`, `CODEX_THREADS_SERVER`, or a
  single configured server.
- Thread list, search, detail, status, and flattened message history commands.
- Thread creation with required `--cwd`.
- Prompted `new` and `send` commands that wait by default, stream human output,
  and support JSON final output or NDJSON streaming.
- Model, reasoning effort, and service-tier settings where Codex app-server
  supports them.
- Thread naming, archive/unarchive, active-turn steer/interrupt, model listing,
  and goal get/set/clear.

Out of scope: HTTP API, TUI, permissions, audit logging, repository/workspace
preparation, non-Codex backends, aggregate multi-server data commands, raw
JSON-RPC passthrough, shell command execution, and thread forking.

## Quickstart

Build the CLI:

```bash
cargo build
```

Use a direct socket when you do not want to create a config file:

```bash
CODEX_SOCK=unix:///var/run/user/1000/codex.sock
cargo run -- --connect "$CODEX_SOCK" models --json
```

Or configure named servers:

```toml
[servers.main]
type = "uds"
path = "/var/run/user/1000/codex.sock"

[servers.work]
type = "uds"
path = "/home/kevin/.codex-work/app-server-control/app-server-control.sock"
```

Then run commands against a server:

```bash
codex-threads --config ./config.toml list --server main
codex-threads --config ./config.toml new --server main --cwd "$PWD" "Run the tests"
```

## Configuration

Default config path:

```text
~/.config/codex-threads/config.toml
```

Config path precedence:

1. `--config PATH`
2. `CODEX_THREADS_CONFIG`
3. `~/.config/codex-threads/config.toml`

Server target precedence for thread-data commands:

1. `--connect unix:///path/to.sock`
2. `--server ALIAS`
3. `CODEX_THREADS_SERVER`
4. The single configured server, only when exactly one server exists
5. Error

`--connect` bypasses configured servers and reports the endpoint URI as the
`server` value in JSON output. It is mutually exclusive with `--server` and
`CODEX_THREADS_SERVER`.

When more than one server is configured, commands that operate on app-server
data require an explicit target through `--server` or `CODEX_THREADS_SERVER`.
This avoids cursor merging and prevents accidentally sending work to the wrong
server. `servers ping --all` is the only aggregate command.

## Commands

| Command | Purpose |
| --- | --- |
| `servers [--json]` | List configured server aliases without connecting. |
| `servers ping [--server ALIAS\|--all] [--json]` | Connect, initialize, and report reachability. |
| `list` | List threads with `--limit`, `--cursor`, `--since`, `--cwd`, `--archived`, `--sort`, `--asc`, `--desc`. |
| `search QUERY` | Search one server with `--limit`, `--cursor`, `--since`, and `--archived`. |
| `show THREAD_ID` | Show thread detail and turns with `--last`, `--cursor`, `--asc`, `--desc`, `--items summary\|full\|none`. |
| `messages THREAD_ID` | Flatten messages from recent turns with `--last`, `--since`, `--role user\|assistant`, and `--max-turns`. |
| `new --cwd PATH [PROMPT]` | Create a thread and optionally start the first turn. Supports `--model`, `--effort`, `--service-tier`, `--name`, `--json`, `--stream`, `--no-wait`. |
| `send THREAD_ID PROMPT` | Start a follow-up turn. Supports `--model`, `--effort`, `--service-tier`, `--json`, `--stream`, `--no-wait`. |
| `settings show THREAD_ID` | Read model, effort, service tier, and cwd. |
| `settings set THREAD_ID` | Update `--model`, `--effort`, `--service-tier`, or `--clear-service-tier`. |
| `status [THREAD_ID]` | Show server loaded-thread status or one thread with active turn discovery. |
| `steer THREAD_ID TURN_ID PROMPT` | Send steering input to an active turn. |
| `interrupt THREAD_ID TURN_ID` | Interrupt an active turn. |
| `name THREAD_ID NAME` | Set a thread name. |
| `archive THREAD_ID` / `unarchive THREAD_ID` | Archive or restore a thread. |
| `models` | List available models from the app-server. |
| `goal get THREAD_ID` | Read the active goal. |
| `goal set THREAD_ID` | Set `--objective`, `--status`, or `--token-budget`. |
| `goal clear THREAD_ID` | Clear the active goal. |

Every app-server command accepts `--server ALIAS` and `--json`. Global
`--config PATH` and `--connect ENDPOINT` may be placed before or after the
subcommand because they are global options.

## Output

Human output is the default and is intended for terminal use.

`--json` emits a single pretty-printed JSON object for read commands,
acknowledgement commands, `--no-wait` turn commands, and blocking turn
commands. Blocking `new PROMPT --json` and `send --json` include:

- `server`
- `threadId`
- `turnId`
- `status`
- `progress`
- `assistantResponses`
- `finalAssistantText`

`--json --stream` is available for `new PROMPT` and `send`. It emits NDJSON:
one accepted event, zero or more progress events, and one terminal event.

Commands that create or start work always return enough follow-up identifiers:
`server`, `threadId`, and `turnId` where applicable. `new --cwd PATH` without a
prompt creates the thread and returns `threadId`; `--stream` and `--no-wait` are
invalid without a prompt.

Blocking `new PROMPT` and `send` commands wait up to one hour for the turn to
reach a terminal status. They consume realtime notifications when available and
poll recent turns as a fallback so callers still get a final JSON response if a
notification is missed.

`list --since`, `search --since`, and `messages --since` accept either an epoch
timestamp in seconds or a relative duration ending in `s`, `m`, `h`, or `d`,
such as `5m`. List and search filtering is applied client-side to `updatedAt`.
When a filtered list or search does not fill `--limit` from the first server
page, the CLI keeps scanning server pages until the filtered limit is filled or
the server cursor is exhausted. Returned cursors are still raw Codex server
cursors from the last scanned page.

`messages --since` is applied client-side after retrieving up to `--max-turns`
recent turns.

In human output, `messages` prints readable timestamped blocks. When no role
filter is set, each block header includes the role. With `--role user` or
`--role assistant`, the role is omitted from the header because every message
has the requested role. `--json` keeps the structured message array shape.

## Development

Required checks:

```bash
cargo fmt --check
cargo test
cargo clippy --all-targets --all-features
cargo build --release
```

The integration smoke tests in `tests/mock_smoke.rs` start a mock UDS
WebSocket app-server and exercise the compiled CLI binary end to end.

Live smoke checks are opt-in:

```bash
CODEX_SOCK=unix:///var/run/user/1000/codex.sock smoke/live_smoke.sh
```

Set `RUN_CODEX_TURN=1` to run a real model turn through the live app-server.
Set `RUN_ARCHIVE=1` to include live archive/unarchive checks.

## Release

Releases are driven from `Cargo.toml`, `Cargo.lock`, and `CHANGELOG.md`.
`0.1.0` is the first release version for this repository.

For the first release, after the `Unreleased` changelog section is complete and
`main` is clean:

```bash
node scripts/release.mjs current
```

For later releases, use `patch`, `minor`, `major`, or an explicit semantic
version:

```bash
node scripts/release.mjs patch
node scripts/release.mjs minor
node scripts/release.mjs major
node scripts/release.mjs 0.2.3
```

The script stamps the changelog, commits `Release vX.Y.Z`, creates and pushes a
matching git tag, creates a GitHub prerelease with notes from the changelog,
then commits a fresh `Unreleased` section for the next cycle.

## Project Structure

- `src/bin/` - binary entrypoints.
- `src/lib.rs` - shared library entrypoint.
- `src/config.rs` - TOML schema, validation, and target resolution.
- `src/rpc.rs` - UDS WebSocket JSON-RPC transport and handshake.
- `src/cli.rs` - command-line parser.
- `src/app.rs` - command orchestration and rendering.
- `tests/` - deterministic binary-level mock smoke coverage.
- `scripts/` - release automation.
- `smoke/` - opt-in live smoke harness.
