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

- TOML configuration for named app-server endpoints.
- Direct `--connect unix:///path/to.sock` and `--connect ws://host:port`
  debug targeting.
- Deterministic target selection with `--server`, `CODEX_THREADS_SERVER`, or a
  single configured server.
- Thread list, search, detail, status, and flattened message history commands.
- Interactive TUI browser for listing, searching, viewing, annotating,
  refreshing, sending, steering, and interrupting threads.
- Local thread annotations projected into list, search, and detail output.
- Thread creation with required `--cwd`.
- Prompted `new` and `send` commands that wait by default, stream human output,
  and support JSON final output or newline-delimited JSON (NDJSON) streaming.
- Model, reasoning effort, and service-tier settings where Codex app-server
  supports them.
- Thread naming, archive/unarchive, active-turn steer/interrupt, model listing,
  and goal get/set/clear.

## Install

Download the latest archive for your platform from GitHub Releases:

```text
https://github.com/kcosr/codex-threads/releases
```

Supported release platforms are currently:

- `linux-x86_64`
- `macos-arm64`

Install the extracted `codex-threads` binary somewhere on your `PATH`, for
example `~/.local/bin`:

```bash
mkdir -p ~/.local/bin
install -m 755 codex-threads ~/.local/bin/codex-threads
codex-threads help
```

For unsupported platforms or local development, build from source in the
Development section near the end of this document.

## Quickstart

Prerequisites:

- Install the Codex CLI/runtime separately and ensure the `codex` executable is
  on `PATH`.
- Start Codex app-server with a Unix domain socket (UDS) or WebSocket listener
  before using this CLI.

When asking another agent to use this CLI, point it at the included skill:

```text
skills/codex-threads
```

`codex-threads` talks to a running Codex app-server:

```bash
CODEX_SOCK=unix:///path/to/codex.sock
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
model = "gpt-5.5"
model_reasoning_effort = "high"

[servers.main]
endpoint = "unix:///path/to/codex.sock"
```

See `config.example.toml` for a complete starting point.

Then omit `--server`:

```bash
codex-threads servers ping
codex-threads list
codex-threads new --cwd "$PWD" "Run the tests"
```

Or configure named servers when you have multiple app-server sockets:

```toml
[servers.main]
endpoint = "unix:///path/to/main/codex.sock"

[servers.work]
endpoint = "ws://127.0.0.1:8765"
model = "gpt-5.5"
model_reasoning_effort = "low"
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

This project targets Unix-like systems. Replace `/path/to/codex.sock` and
`127.0.0.1:8765` examples with the endpoint you choose for your app-server.

## Common Workflows

The examples below assume `codex-threads` is installed on `PATH` and a server is
configured or selected with `--connect`.

Examples that pipe JSON use `jq`; install it separately or replace those
pipelines with your preferred JSON tooling.

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

Launch the interactive browser with the same initial filters:

```bash
codex-threads tui --since 24h --cwd "$PWD"
codex-threads tui --query "release process" --limit 20
```

Inside the TUI, use `j/k`, arrow keys, or mouse wheel scrolling to move in the
browser and detail transcript; use `gg` and `G` to jump to top and bottom. Use
`?` for keyboard help, `/` to search threads or loaded transcript messages,
`Enter` to open a thread, `p` to toggle the lazy recent-message preview pane,
`[` and `]` to page browser results, `f` for filters, `s` for sort, `c` for
visible columns and updated-time display, `a` to annotate, `e` to rename, `A`
to confirm archive or unarchive, `T` to attach to the selected active thread,
`i` to confirm interrupting it, `r` to refresh, `y` to copy the active thread id
with OSC 52, `o` to confirm opening the active thread in Codex's own TUI, and
`m` to compose. Use
`l` to explicitly load the selected or open thread, matching
`status THREAD_ID --load`, then refresh visible metadata and history.
Compose uses `Enter` to submit and `Ctrl-J` to insert a newline. On active
threads, compose defaults to steering the active turn; `Tab` switches to a
normal new-turn send, and `Tab` switches back to steer while the thread remains
active. On inactive threads, `Tab` toggles stream/no-wait for new turns. Browser
compose streams into the preview while the thread remains selected, follows
queued turns on that thread, and detaches locally when selection moves away. If
the initial selected browser row is active, or if an active thread is opened in
detail, the TUI attaches to it automatically.
Use `t` to toggle real browser auto-refresh; the `c` menu adjusts the persisted
refresh interval from 5-300 seconds with `-` and `+`.
Search prompts use `Enter` to apply and `Ctrl-D` to clear. Annotation editing
uses `Enter` to save and `Ctrl-D` to clear. Rename editing uses `Enter` to save
and `Ctrl-D` to clear the draft; app-server does not expose a clear-name
operation. In detail, `T` attaches to an active turn, `i` confirms interrupt,
`Enter` or `m` composes a message or steer action based on whether the thread is
active, and `q` quits. Normal send uses Codex app-server's `turn/start` path;
steer uses `turn/steer` when the thread is active and the composer is in steer
mode. Attaching resumes the thread with turns included so the active-turn
snapshot appears before new stream notifications.
Opening a thread loads a small recent turn window, orders it chronologically,
and starts at the bottom of the transcript. Scrolling up at the top loads the
next older chunk above the current transcript and preserves the current view. In
detail, `gg` and `Home` load through to the real transcript start before jumping
there; `G` and `End` load through to the real transcript end before jumping
there. Detail views refresh in place while open, and `Esc` returns to the
browser after unlinking the local detail view and detaching any local stream.
Local detach leaves remote turns running.
Opening in Codex temporarily returns terminal control to `codex resume
<thread-id> --remote <server-endpoint> --cd <thread-cwd>`, adding
`--dangerously-bypass-approvals-and-sandbox` when the codex-threads TUI was
launched with yolo enabled, then redraws and refreshes the codex-threads TUI
after Codex exits.
Transcript rendering is markdown-aware for common headings, blockquotes, lists,
paragraph spacing, and fenced code blocks. Message headings show role and
timestamp without repeating turn IDs. Fenced code blocks gain syntax-highlighted
spans when the default-off `tui-syntax-highlighting` Cargo feature is enabled.
Set `CODEX_THREADS_TUI_STREAM_LOG=/path/to/events.jsonl` to append raw stream
events while debugging live transcript rendering.

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

1. `--connect unix:///path/to.sock`, `--connect ws://host:port`, or
   `--connect wss://host:port`
2. `--server ALIAS`
3. `CODEX_THREADS_SERVER`
4. The single configured server, only when exactly one server exists
5. Error

`--connect` bypasses configured servers and reports the endpoint URI as the
`server` value in JSON output. It is mutually exclusive with `--server` and
`CODEX_THREADS_SERVER`.

Configured servers use a single endpoint string:

```toml
[servers.main]
endpoint = "unix:///path/to/codex.sock"

[servers.local_ws]
endpoint = "ws://127.0.0.1:8765"
```

`ws://` and `wss://` endpoints must include an explicit port and must not
include a path, query, or fragment.

Legacy UDS config still works, but prints a deprecation warning:

```toml
[servers.main]
type = "uds"
path = "/path/to/codex.sock"
```

Replace it with:

```toml
[servers.main]
endpoint = "unix:///path/to/codex.sock"
```

WebSocket endpoints may use bearer-token auth. Prefer env-var indirection:

```toml
[servers.remote]
endpoint = "wss://example.com:443"
auth_token_env = "CODEX_APP_SERVER_TOKEN"
```

Literal tokens are also supported for private configs:

```toml
[servers.local_ws]
endpoint = "ws://127.0.0.1:8765"
auth_token = "literal-token"
```

For direct connections, use `--connect-auth-token-env ENV_VAR` or
`--connect-auth-token TOKEN`. Prefer the env-var form on shared machines because
literal command-line tokens may be visible through process listings. Both send
`Authorization: Bearer <token>` during the WebSocket upgrade. Tokens are
accepted only for `wss://` or loopback `ws://` endpoints; non-loopback plain
`ws://` with a token is rejected to avoid
sending credentials over cleartext.

When more than one server is configured, app-server commands require an explicit
target through `--server` or `CODEX_THREADS_SERVER`.
This avoids cursor merging and prevents accidentally sending work to the wrong
server. `servers ping --all` is the only aggregate command.

New-thread model defaults:

1. `new --model MODEL` and `new --effort EFFORT`
2. The selected server's `model` and `model_reasoning_effort`
3. Top-level `model` and `model_reasoning_effort`
4. Codex app-server defaults

Config model defaults are applied only when creating a thread with `new`.
Follow-up `send` commands keep the thread's existing app-server settings unless
`--model` or `--effort` is passed explicitly.

## Commands

| Command | Purpose |
| --- | --- |
| `servers [--json]` | List configured server aliases without connecting. |
| `servers ping [--server ALIAS\|--all] [--json]` | Connect, initialize, and report reachability. |
| `list` | List threads with `--limit`, `--cursor`, `--since`, `--cwd`, `--archived`, `--sort`, `--asc`, `--desc`. Defaults to `--limit 50`. |
| `search QUERY` | Search one server with `--limit`, `--cursor`, `--since`, and `--archived`. |
| `show THREAD_ID` | Show thread detail and turns with `--last`, `--cursor`, `--asc`, `--desc`, `--items summary\|full\|none`. Defaults to `--last 20`. |
| `tui` | Launch the interactive browser with `--query`, `--since`, `--cwd`, `--archived`, `--limit`, `--sort`, `--asc`, and `--desc` initial filters. |
| `messages THREAD_ID` | Flatten messages from recent turns with `--last`, `--since`, `--role user\|assistant`, and `--max-turns`. |
| `new --cwd PATH [PROMPT]` | Create a thread and optionally start the first turn. Supports `--model`, `--effort`, `--service-tier`, `--name`, `--json`, `--stream`, `--no-wait`. |
| `send THREAD_ID PROMPT` | Start a follow-up turn. Supports `--model`, `--effort`, `--service-tier`, `--json`, `--stream`, `--no-wait`. |
| `settings show THREAD_ID` | Read model, effort, service tier, and cwd. This resumes the thread for inspection but does not force yolo permissions. |
| `settings set THREAD_ID` | Update `--model`, `--effort`, `--service-tier`, or `--clear-service-tier`; at least one setting flag is required. |
| `status [THREAD_ID]` | Show server loaded-thread status or one thread with active turn discovery. Use `--load` with a thread ID to resume/load before reporting. |
| `steer THREAD_ID TURN_ID PROMPT` | Send steering input to an active turn. |
| `interrupt THREAD_ID TURN_ID` | Interrupt an active turn. |
| `name THREAD_ID NAME` | Set a thread name. |
| `archive THREAD_ID` / `unarchive THREAD_ID` | Archive or restore a thread. |
| `models` | List available models from the app-server. |
| `usage` | Show account usage, rate-limit windows, plan, and credits from the app-server. |
| `goal get THREAD_ID` | Read the active goal. |
| `goal set THREAD_ID` | Set `--objective`, `--status`, or `--token-budget`; at least one flag is required. |
| `goal clear THREAD_ID` | Clear the active goal. |
| `annotate set THREAD_ID TEXT` | Set or replace a local annotation for a thread. |
| `annotate get THREAD_ID` | Read a local annotation. Missing annotations exit with code `2`. |
| `annotate clear THREAD_ID` | Clear a local annotation. |
| `annotate list` | List local annotations for the selected server, optionally filtered with `--query`. |
| `annotate search QUERY` | Search local annotation text for the selected server. |
| `annotate prune [--dry-run]` | Remove annotations whose threads are no longer found by app-server. |
| `completion [SHELL]` | Print shell completion setup instructions for `bash`, `zsh`, or `fish`. |

Every app-server and annotation command accepts `--server ALIAS` and `--json`.
Global
`--config PATH`, `--connect ENDPOINT`, `--connect-auth-token-env ENV_VAR`, and
`--connect-auth-token TOKEN` may be placed before or after the subcommand
because they are global options.
Global `--no-yolo` disables the default permission override for action commands
that create, resume before action, or start Codex work. `settings show` is a
read path and does not force yolo permissions even though it resumes the thread
to inspect settings.

If `send`, `steer`, or `settings set` receives Codex app-server's unloaded
thread error, `codex-threads` resumes the target thread and retries the action
once. That resume uses the same permission mode as the action: yolo permissions
by default, or app-server defaults when global `--no-yolo` is passed.

Accepted `--effort` values are `none`, `minimal`, `low`, `medium`, `high`, and
`xhigh`. Accepted `goal set --status` values are `active`, `paused`, `blocked`,
`usage-limited`, `budget-limited`, and `complete`.

## Shell Completion

Print setup instructions for the detected shell:

```bash
codex-threads completion
codex-threads completion bash
codex-threads completion zsh
codex-threads completion fish
```

Enable completion only for the current shell:

```bash
source <(codex-threads completion script bash)
source <(codex-threads completion script zsh)
codex-threads completion script fish | source
```

For permanent bash setup, generate a static completion file and source it from
`~/.bashrc`:

```bash
mkdir -p ~/.local/share/codex-threads
codex-threads completion script bash > ~/.local/share/codex-threads/completion.bash
printf '\nsource ~/.local/share/codex-threads/completion.bash\n' >> ~/.bashrc
```

For permanent zsh setup:

```bash
mkdir -p ~/.local/share/codex-threads
codex-threads completion script zsh > ~/.local/share/codex-threads/completion.zsh
printf '\nsource ~/.local/share/codex-threads/completion.zsh\n' >> ~/.zshrc
```

For permanent fish setup:

```fish
mkdir -p ~/.config/fish/completions
codex-threads completion script fish > ~/.config/fish/completions/codex-threads.fish
```

Regenerate the completion file after upgrading `codex-threads`.

Completions suggest command names, nested subcommands, option names, static
values such as `--sort updated|created`, `--items summary|full|none`,
`--role user|assistant`, `--effort none|minimal|low|medium|high|xhigh`, goal
status values, shell names for `completion`, and local configured server aliases
for `--server`. Completion does not connect to Codex app-server, so thread IDs,
turn IDs, and remote model IDs are not completed.

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

Streamed assistant progress events include Codex `itemId` when available, and
`assistantResponses` contains one entry per assistant item so clients can keep
separate assistant messages distinct.

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
If the local one-hour wait times out, the command exits with code `3`; the
remote Codex turn may still be running.

`status --json` without a thread ID returns `{ server, reachable,
loadedThreadIds, nextCursor }`. `status THREAD_ID --json` returns the selected
thread, `threadId`, `activeTurnId`, and `truncated`. Plain `status THREAD_ID`
does not resume unloaded threads; `status THREAD_ID --load` explicitly calls
`thread/resume` with `excludeTurns: true`, unsubscribes the probing connection,
then reports status from the loaded app-server view.

`usage --json` returns `{ server, rateLimits, rateLimitsByLimitId }` from
Codex app-server's `account/rateLimits/read` response. Human output summarizes
the server, plan, credits, rate-limit reached state, and primary/secondary
windows for each limit ID.

Annotations are local `codex-threads` state, not Codex app-server state. The
state file is resolved as:

1. `$CODEX_THREADS_STATE/annotations.json`
2. `$XDG_STATE_HOME/codex-threads/annotations.json`
3. `~/.local/state/codex-threads/annotations.json`

Annotations are keyed by selected server endpoint and thread ID. `annotate`
commands can set, get, clear, list, search, and prune those local records.
`list --json`, `search --json`, and `show --json` include an `annotation` object
on returned thread objects when one exists. Human `list` and `search` add an
`ANNOTATION` column only when displayed rows have annotations; human `show`
prints the annotation in the thread detail.

TUI preferences are local `codex-threads` state, separate from annotations:

1. `$CODEX_THREADS_STATE/tui.json`
2. `$XDG_STATE_HOME/codex-threads/tui.json`
3. `~/.local/state/codex-threads/tui.json`

The TUI persists disposable UI preferences such as visible columns,
auto-refresh, the 5-300 second refresh interval, preview pane, and default sort.
Corrupt or unsupported preference files are renamed to
`tui.json.corrupt.<epoch>` when possible and fall back to defaults instead of
blocking launch. In search mode, `--cwd` is a local refinement over the loaded
search page; sort controls are disabled until the app-server search API supports
server-side sorting.

Exit codes:

| Code | Meaning |
| --- | --- |
| `0` | Command succeeded, or a blocking turn completed. |
| `1` | A blocking `new` or `send` turn reached `failed` or `interrupted`. |
| `2` | Usage, argument, validation, configuration, or local lookup error. |
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
When the recent turn scan is truncated, human output prints a warning; increase
`--max-turns` or use `show --cursor` for older exact paging.
Long table cells and message previews may be shortened in human output to keep
terminal output readable; use `--json` when exact text is required.

## Development

Build the CLI during development:

```bash
cargo build
```

Build the optimized binary:

```bash
cargo build --release
```

To use the local build like a release binary, install it somewhere on your
`PATH`, for example:

```bash
mkdir -p ~/.local/bin
install -m 755 target/release/codex-threads ~/.local/bin/codex-threads
```

You can also install directly from the checkout:

```bash
cargo install --path . --root ~/.local
```

Required checks:

```bash
cargo fmt --check
cargo test
cargo clippy --all-targets --all-features
cargo build --release
```

The integration smoke tests in `tests/mock_smoke.rs` start mock UDS and TCP
WebSocket app-servers and exercise the compiled CLI binary end to end.

The opt-in PTY smoke tests in `tests/tui_pty_smoke.rs` drive the real
interactive TUI through a pseudo-terminal, including browser/detail navigation,
streaming sends, attach/detach, and CLI history/status validation against a
stateful mock app-server:

```bash
cargo test --test tui_pty_smoke -- --ignored
```

Live smoke checks are opt-in:

```bash
CODEX_ENDPOINT=unix:///path/to/codex.sock smoke/live_smoke.sh
```

Set `RUN_CODEX_TURN=1` to run a real model turn through the live app-server.
Set `RUN_ARCHIVE=1` to include live archive/unarchive checks.

## Release

Releases are driven from `Cargo.toml`, `Cargo.lock`, and `CHANGELOG.md`.
Use `patch`, `minor`, `major`, or an explicit semantic version:

```bash
node scripts/release.mjs patch
node scripts/release.mjs minor
node scripts/release.mjs major
node scripts/release.mjs 0.2.3
```

The script stamps the changelog, commits `Release vX.Y.Z`, creates and pushes a
matching git tag, creates a GitHub release with notes from the changelog,
then commits a fresh `Unreleased` section for the next cycle.

Release binaries are packaged separately after the platform binaries have been
provided or built by the release operator. Supported release platforms currently
use archive names like:

```text
codex-threads-VERSION-linux-x86_64.tar.gz
codex-threads-VERSION-macos-arm64.tar.gz
```

Each archive should contain one top-level directory named
`codex-threads-VERSION-PLATFORM` with:

- `codex-threads` - executable binary for that platform
- `README.md`
- `LICENSE`
- `CHANGELOG.md`
- `config.example.toml`
- `skills/`

Example packaging flow for one platform:

```bash
VERSION=0.2.0
PLATFORM=linux-x86_64
BINARY=/path/to/codex-threads

STAGE="$(mktemp -d)"
ROOT="codex-threads-${VERSION}-${PLATFORM}"
mkdir -p "$STAGE/$ROOT"
install -m 755 "$BINARY" "$STAGE/$ROOT/codex-threads"
cp README.md LICENSE CHANGELOG.md config.example.toml "$STAGE/$ROOT/"
cp -R skills "$STAGE/$ROOT/"
tar -C "$STAGE" -czf "${ROOT}.tar.gz" "$ROOT"
rm -rf "$STAGE"
```

Repeat that staging step for each platform, for example `linux-x86_64` and
`macos-arm64`, using the correct binary for each target. After the GitHub
Release exists, upload the archives:

```bash
RELEASE_TAG="v${VERSION}"
gh release upload "$RELEASE_TAG" \
  "codex-threads-${VERSION}-linux-x86_64.tar.gz" \
  "codex-threads-${VERSION}-macos-arm64.tar.gz"
```

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
