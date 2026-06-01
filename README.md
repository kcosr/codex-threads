# codex-threads

`codex-threads` is a companion CLI for inspecting and controlling Codex
app-server threads from a terminal or another agent.

It exists for workflows the Codex CLI does not currently cover well: asking
what threads were active recently, what happened in a repo, whether a thread is
still running, and sending a follow-up to an existing session. The Codex desktop
app may expose some of this interactively; `codex-threads` makes it available
through a focused command-line interface.

The main use cases are coordinating Codex work as a user or through another
agent: list recent sessions/threads, retrieve relevant transcript slices,
summarize status and where work left off, spawn new Codex threads for background
work, and relay user requests or follow-ups across those threads. Search is
available when you need it, but it is not optimized and can be very slow over
large histories; prefer recent listing, targeted transcript retrieval, thread
creation, and direct follow-ups when those fit the workflow.

It talks to Codex app-server, the local control server exposed by the Codex
agent runtime. In Codex terminology, a thread is one session and a turn is one
user request/assistant response cycle.

`codex-threads` is built for headless Codex control by users who already run
Codex in yolo-style environments where sandboxing and approval policy are handled
outside the Codex application. Yolo mode is opt-out: by default, thread
creation, resume-before-action recovery, and turn start requests force Codex
app-server to use `approvalPolicy = "never"` and full-access sandboxing
(`sandbox = "danger-full-access"` or
`sandboxPolicy.type = "dangerFullAccess"`). Pass global `--no-yolo` to use the
app-server's configured approval and sandbox defaults instead. Do not use this
CLI as a safety boundary.

## Features

- TOML configuration for named local Unix domain socket targets.
- Direct `--connect unix:///path/to.sock` debug targeting.
- Deterministic target selection with `--server`, `CODEX_THREADS_SERVER`, or a
  single configured server.
- Thread list, search, detail, status, and flattened message history commands.
- Thread creation with required `--cwd`.
- Prompted `new` and `send` commands that wait by default, stream human output,
  and support JSON final output or newline-delimited JSON (NDJSON) streaming.
- Model, reasoning effort, and service-tier settings where Codex app-server
  supports them.
- Thread naming, archive/unarchive, active-turn steer/interrupt, model listing,
  and goal get/set/clear.

## Quickstart

Build the CLI:

```bash
cargo build
```

Install it on your `PATH` for the bare `codex-threads` examples. If you use
`~/.local/bin`:

```bash
cargo install --path . --root ~/.local
```

When asking another agent to use this CLI, point it at the included skill:

```text
skills/codex-threads
```

`codex-threads` talks to a running Codex app-server. Start Codex app-server
with a Unix domain socket (UDS) listener before using this CLI:

```bash
CODEX_SOCK=unix:///var/run/user/1000/codex.sock
codex app-server --listen "$CODEX_SOCK"
```

`codex-threads` opts into Codex app-server experimental APIs during its
JSON-RPC `initialize` request by sending `capabilities.experimentalApi = true`.
No separate Codex feature flag is required in `codex-threads`. If the running
Codex app-server is too old or rejects that capability, commands that depend on
experimental methods fail with an app-server capability error.

Interactive Codex CLI sessions that should be visible to `codex-threads` should
connect to the same app-server with `--remote`:

```bash
codex --remote "$CODEX_SOCK" --cd "$PWD"
```

Without `--remote`, an interactive Codex CLI session may not be using the same
app-server that `codex-threads` queries and controls.

Use a direct socket when you do not want to create a config file:

```bash
codex-threads --connect "$CODEX_SOCK" models --json
```

For the common one-server case, configure one server at
`~/.config/codex-threads/config.toml`:

```toml
[servers.main]
type = "uds"
path = "/var/run/user/1000/codex.sock"
```

Then omit `--server`:

```bash
codex-threads servers ping
codex-threads list
codex-threads new --cwd "$PWD" "Run the tests"
```

Or configure named servers when you have multiple app-server sockets:

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
codex-threads servers ping --server main
codex-threads list --server main
codex-threads new --server main --cwd "$PWD" "Run the tests"
```

Successful `servers ping` human output is tabular:

```text
SERVER  STATUS
main    ok
```

This project targets Unix-like systems with Unix domain socket support. Linux
examples use `/var/run/user/...`; choose an appropriate socket path for other
Unix-like systems.

## Common Workflows

The examples below assume `codex-threads` is installed on `PATH` and a server is
configured or selected with `--connect`.

Find recent candidate threads, then inspect the selected thread:

```bash
codex-threads list --since 24h --limit 20 --json
codex-threads search "release process" --limit 10 --json
codex-threads messages THREAD_ID --role user --last 10 --max-turns 100
codex-threads messages THREAD_ID --last 8 --max-turns 50
```

Use `messages` for readable recent context. Use `show --items summary|full`
when you need turn IDs, exact turn structure, or cursor-based paging:

```bash
codex-threads show THREAD_ID --last 10 --items summary --json
codex-threads show THREAD_ID --asc --items full --json
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

Server target precedence for commands that target one app-server:

1. `--connect unix:///path/to.sock`
2. `--server ALIAS`
3. `CODEX_THREADS_SERVER`
4. The single configured server, only when exactly one server exists
5. Error

`--connect` bypasses configured servers and reports the endpoint URI as the
`server` value in JSON output. It is mutually exclusive with `--server` and
`CODEX_THREADS_SERVER`.

When more than one server is configured, app-server commands require an explicit
target through `--server` or `CODEX_THREADS_SERVER`.
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
| `settings show THREAD_ID` | Read model, effort, service tier, and cwd. This resumes the thread for inspection but does not force yolo permissions. |
| `settings set THREAD_ID` | Update `--model`, `--effort`, `--service-tier`, or `--clear-service-tier`; at least one setting flag is required. |
| `status [THREAD_ID]` | Show server loaded-thread status or one thread with active turn discovery. |
| `steer THREAD_ID TURN_ID PROMPT` | Send steering input to an active turn. |
| `interrupt THREAD_ID TURN_ID` | Interrupt an active turn. |
| `name THREAD_ID NAME` | Set a thread name. |
| `archive THREAD_ID` / `unarchive THREAD_ID` | Archive or restore a thread. |
| `models` | List available models from the app-server. |
| `goal get THREAD_ID` | Read the active goal. |
| `goal set THREAD_ID` | Set `--objective`, `--status`, or `--token-budget`; at least one flag is required. |
| `goal clear THREAD_ID` | Clear the active goal. |

Every app-server command accepts `--server ALIAS` and `--json`. Global
`--config PATH` and `--connect ENDPOINT` may be placed before or after the
subcommand because they are global options.
Global `--no-yolo` disables the default permission override for action commands
that create, resume before action, or start Codex work. `settings show` is a
read path and does not force yolo permissions even though it resumes the thread
to inspect settings.

Accepted `--effort` values are `none`, `minimal`, `low`, `medium`, `high`, and
`xhigh`. Accepted `goal set --status` values are `active`, `paused`, `blocked`,
`usage-limited`, `budget-limited`, and `complete`.

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

`status --json` without a thread ID returns `{ server, reachable,
loadedThreadIds, nextCursor }`. `status THREAD_ID --json` returns the selected
thread, `threadId`, `activeTurnId`, and `truncated`.

Exit codes:

| Code | Meaning |
| --- | --- |
| `0` | Command succeeded, or a blocking turn completed. |
| `1` | A blocking `new` or `send` turn reached `failed` or `interrupted`. |
| `2` | Usage, argument, validation, or configuration error. |
| `3` | App-server, connection, Unix socket, WebSocket, or capability error. |
| `130` | Local Ctrl-C while waiting on a turn; the remote turn may still be running. |

`list --since`, `search --since`, and `messages --since` accept either an epoch
timestamp in seconds or a relative duration ending in `s`, `m`, `h`, or `d`,
such as `5m`. List and search filtering is applied client-side to `updatedAt`.
When a filtered list or search does not fill `--limit` from the first server
page, the CLI keeps scanning server pages until the filtered limit is filled or
the server cursor is exhausted. Returned cursors are still raw Codex server
cursors from the last scanned page.

`messages` is a convenience projection over recent turn history. It does not
page exact whole-thread message history and it does not have `--first`. For the
beginning of a thread or older exact review, use `show --asc` and/or
`show --cursor` with the appropriate `--items` view.

Message selection order is:

1. Fetch up to `--max-turns` recent turns from Codex with full items.
2. Flatten those turns into user/assistant messages.
3. Apply `--since`, if present, using the turn timestamp.
4. Apply `--role user|assistant`, if present.
5. Apply `--last N`, if present, to the final filtered message list.

`--max-turns` defaults to `200` and is the recent turn scan window, not a final
display limit.
`--last` is the final message limit after flattening and filtering; it is not
an alias for `--max-turns`. Role filtering only sees messages inside the
scanned recent turns, so increase `--max-turns` when looking for sparse or older
messages such as `--role assistant --last 3`.

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
- `src/rpc.rs` - Unix domain socket WebSocket JSON-RPC transport and handshake.
- `src/cli.rs` - command-line parser.
- `src/app.rs` - command orchestration and rendering.
- `tests/` - deterministic binary-level mock smoke coverage.
- `scripts/` - release automation.
- `smoke/` - opt-in live smoke harness.
- `skills/` - Codex skill guidance for using this CLI from other assistant
  sessions.
