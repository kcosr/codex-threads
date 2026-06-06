---
name: codex-threads
description: Use `codex-threads` to search, inspect, summarize, message, and control Codex app-server threads. Use this when the user asks about recent Codex work, project status over a time window, what was discussed in another Codex thread, or wants to send/follow up with a Codex thread.
---

# codex-threads

Use `codex-threads` to query and control Codex app-server threads.

The executable is on `PATH`:

```bash
codex-threads
```

The user normally has config at `~/.config/codex-threads/config.toml` pointing at the main app-server endpoint, so do **not** pass `--connect` unless debugging or explicitly targeting another server.

`codex-threads` requires a separate Codex CLI/runtime installation and only sees threads served by a running Codex app-server. The app-server must be started with a listener such as:

```bash
CODEX_SOCK=unix:///path/to/codex.sock
codex app-server --listen "$CODEX_SOCK"
# or
codex app-server --listen ws://127.0.0.1:8765
```

Interactive Codex CLI sessions that should be visible here should connect to the same app-server:

```bash
codex --remote "$CODEX_SOCK" --cd "$PWD"
```

If an interactive Codex session was not started with `--remote`, do not assume `codex-threads` can find or control that session.

`codex-threads` opts into Codex app-server experimental APIs during initialize by sending `capabilities.experimentalApi = true`; no separate Codex feature flag is required. If a command fails with an `experimentalApi` capability error, the running app-server is too old or rejected that capability.

## First Checks

Verify connectivity when the tool or server state is uncertain:

```bash
codex-threads servers ping
```

Expected shape:

```text
SERVER  STATUS
main    ok
```

Use JSON for precise targeting and machine-readable output:

```bash
codex-threads servers ping --json
```

## Core Commands

```bash
codex-threads list --limit 20
codex-threads search "query" --limit 20
codex-threads show <thread_id>
codex-threads messages <thread_id>
codex-threads status <thread_id>
codex-threads send <thread_id> "follow-up message"
codex-threads new --cwd /abs/path "initial prompt"
codex-threads annotate get <thread_id>
codex-threads annotate set <thread_id> "note"
```

Use `--json` whenever you need exact IDs, cwd, role, timestamps, status, cursors, or reliable parsing.

Default limits: `list` uses `--limit 50`, `show` uses `--last 20`, and `messages` uses `--max-turns 200` unless overridden.

Examples in this skill use `jq` for compact JSON projection; use another JSON tool if `jq` is not installed.

## Interactive TUI

Use `codex-threads tui` when the user wants to browse, search, inspect, annotate,
refresh, or control threads interactively from a terminal:

```bash
codex-threads tui --since 24h --cwd "$PWD"
codex-threads tui --query "release process" --limit 20
```

The TUI accepts the same initial discovery filters as `list`/`search`: `--query`,
`--since`, `--cwd`, `--archived`, `--limit`, `--sort`, `--asc`, and `--desc`.
Do not use it for machine-readable automation; use the CLI `--json` commands
instead.

Useful TUI keys:

- `?` opens the comprehensive keyboard help modal.
- `j/k`, arrow keys, or mouse wheel scrolling move through the browser and
  detail transcript; `Enter` opens a thread.
- `gg` jumps to the top and `G` jumps to the bottom in the browser or detail.
- `/` searches threads in the browser or loaded transcript lines in detail.
- Search prompts use `Enter` to apply and `Ctrl-D` to clear.
- `]` and `[` page through browser/detail cursors when available.
- `p` toggles the browser preview pane.
- `f` opens filters, `s` opens sort, and `c` opens visible columns plus the
  relative updated-time display toggle. In filters, `a` toggles the archived
  thread filter.
- `a` edits the local annotation with `Enter` save and `Ctrl-D` clear.
- `e` renames the active thread with `Enter` save; `Ctrl-D` clears the draft,
  but app-server does not expose a clear-name operation.
- `A` archives or unarchives the active thread.
- `r` refreshes; `R` resets pagination; `t` toggles auto-refresh.
- `y` copies the active thread id with OSC 52.
- `m` composes a follow-up; `Enter` sends, `Ctrl-J` inserts a newline, and
  `Tab` toggles stream/no-wait for new turns.
- Opening a detail view starts at the transcript bottom; while in detail,
  `Enter` opens the message action, `n/N` move between message-search matches,
  and `Esc` unlinks the local detail view and returns to the browser.
- In detail, `T` attaches to an active turn, `S` steers it, and `i` confirms
  interrupt. Attach refreshes readable history first, then streams new events.
- `q` quits. Local detach leaves remote turns running unless interrupted.
- Set `CODEX_THREADS_TUI_STREAM_LOG=/path/to/events.jsonl` to capture raw stream
  events for transcript debugging.

In TUI search mode, `--cwd` is a local refinement over the loaded search page.
Sort controls are disabled in search mode until app-server supports server-side
search sorting.

TUI transcript rendering is markdown-aware and preserves paragraph spacing.
Message headings show role and timestamp without repeating turn IDs. Syntax
highlighting for fenced code blocks is behind the default-off Cargo feature
`tui-syntax-highlighting`; normal release builds still show readable plain code
blocks.

## Local Annotations

`codex-threads annotate` manages local notes for threads. These annotations are
stored by `codex-threads`, keyed by selected server endpoint and thread ID; they
are not Codex app-server state.

Use annotations to mark why a thread matters, what follow-up is pending, or how
to recognize a thread later:

```bash
codex-threads annotate set <thread_id> "Waiting for review on PR #5"
codex-threads annotate get <thread_id>
codex-threads annotate clear <thread_id>
codex-threads annotate list
codex-threads annotate search "review"
```

`annotate get <thread_id>` exits with code `2` if there is no local annotation.
`annotate list` and `annotate search` exit successfully with an empty result
when there are no matches.

Annotations appear automatically in `list`, `search`, and `show` output when
present. In JSON, look for `thread.annotation.text`; in human `list` and
`search`, an `ANNOTATION` column appears only when displayed rows have
annotations.

Use prune only when intentionally cleaning stale local records:

```bash
codex-threads annotate prune --dry-run --json
codex-threads annotate prune --json
```

`annotate prune` contacts app-server and removes annotations whose threads are
reported missing. Archive/unarchive does not remove annotations.

## Recommended Investigation Workflow

For an agent, prefer this split:

1. **Discover with JSON.**
   Use `list` or `search --json` to find candidate thread IDs and disambiguate by cwd/status/preview/snippet.
2. **Read recent context with human output.**
   Once a thread is selected, use `messages` without `--json` for readable review of recent conversation context. Prefer `--last N` for the final number of messages to display and set `--max-turns M` high enough to scan the recent turn window you need.
3. **Use JSON or `show` again for exact fields, older history, or pagination.**
   Use `--json` when you need `turnId`, `activeTurnId`, `nextCursor`, `truncated`, or exact machine parsing. Use `show --asc` / `show --cursor` for beginning-of-thread or older turn review; `messages` does not have a `--first` option.

Example:

```bash
codex-threads search --json --limit 10 "agent pack" \
  | jq '{results:[.results[] | {id:.thread.id,cwd:.thread.cwd,status:.thread.status.type,updatedAt:.thread.updatedAt,preview:.thread.preview,snippet:.snippet}]}'

codex-threads messages <thread_id> --last 4 --max-turns 50
```

### Message limit and filter semantics

For `messages`, `--max-turns` and `--last` are **not aliases**:

1. `--max-turns M` fetches/scans the most recent M turns first. It is a scan window, not the final message display limit.
2. The command flattens those turns into user/assistant messages.
3. `--since` and `--role user|assistant` filter the flattened messages.
4. `--last N` is applied last and limits the final message list.

There is no `messages --first`. For the start of a thread or older exact paging, use `show --asc` and/or `show --cursor` instead of `messages`.

When role-filtering, increase `--max-turns` if the role is sparse or the messages may be older; otherwise `--role assistant --last 3` can miss older assistant messages outside the scanned recent turn window.

Use `messages --role user|assistant` to reduce output when looking for intent:

```bash
codex-threads messages <thread_id> --role user --last 10 --max-turns 100
codex-threads messages <thread_id> --role assistant --last 3 --max-turns 50
```

For custom filtering beyond the built-in role filter, use JSON + `jq`:

```bash
codex-threads messages <thread_id> --last 10 --max-turns 100 --json \
  | jq -r '.messages[] | select(.role=="user") | "--- " + (.turnId // "") + "\n" + (.text // "")'
```

## Recent Work / Last Day / Project Status Workflow

Use this skill when the user asks things like:

- "What was I working on recently?"
- "Summarize my Codex work from the last day."
- "What is the status of the projects I worked on today?"
- "Find the recent thread about <topic>."

### 1. List recent threads by time window

Prefer `--since` over paging when the user gives a time window.

Examples:

```bash
codex-threads list --since 24h --limit 100 --json
codex-threads list --since 1d --limit 100 --json
```

`--since` accepts epoch seconds or relative durations ending in `s`, `m`, `h`, or `d`; it does not accept calendar dates such as `2026-06-01`.

Then group by cwd/project and keep the output compact:

```bash
codex-threads list --since 24h --limit 100 --json \
  | jq -r '.threads[] | [.updatedAt, .id, (.status.type // ""), (.cwd // ""), ((.name // .preview // "") | gsub("\n"; " ") | .[0:140])] | @tsv'
```

For "project status" summaries, inspect the most relevant/recent threads per cwd, especially active or recently updated ones.

### 2. Check active status before summarizing or messaging

```bash
codex-threads status <thread_id> --load --json \
  | jq '{threadId, activeTurnId, status:.thread.status, cwd:.thread.cwd, preview:.thread.preview}'
```

Use `--load` when liveness matters; plain `status <thread_id>` does not resume unloaded threads.
If `activeTurnId` is non-null, the thread is running. Do not send disruptive follow-ups unless the user asked you to.

### 3. Review user intent first

For each candidate thread, skim recent user messages first:

```bash
codex-threads messages <thread_id> --role user --last 10 --max-turns 100
```

Fallback:

```bash
codex-threads messages <thread_id> --last 10 --max-turns 100 --json \
  | jq -r '.messages[] | select(.role=="user") | "---\n" + (.text // "")'
```

Then review the nearby full exchange:

```bash
codex-threads messages <thread_id> --last 6 --max-turns 50
```

### 4. Summarize by project

When reporting "last day" status, group by cwd/project and include minimal relevant columns:

| Project/CWD | Thread | Status | Summary |
|---|---|---|---|

Mention if a thread is currently active, blocked, waiting for review, or has an active subprocess according to recent messages/status.

## Paging, Cursors, and "Offsets"

The CLI is cursor-based, not numeric-offset-based. Do not invent offset numbers. Use returned cursors.

### List/search pagination

`list --json` and `search --json` return cursors such as `nextCursor` and `backwardsCursor`.

Fetch the next page by passing the cursor back:

```bash
page1=$(codex-threads list --limit 20 --json)
echo "$page1" | jq -r '.nextCursor'

codex-threads list --limit 20 --cursor "$(echo "$page1" | jq -r '.nextCursor')" --json
```

Search works similarly:

```bash
page1=$(codex-threads search "agent pack" --limit 20 --json)
codex-threads search "agent pack" --limit 20 --cursor "$(echo "$page1" | jq -r '.nextCursor')" --json
```

Use `--since` instead of paginating whenever the user asks about a recent time window.

### Thread history pagination

For a small recent slice, prefer:

```bash
codex-threads messages <thread_id> --last 5 --max-turns 50
codex-threads show <thread_id> --last 5 --items summary
```

For older history or when output says it is truncated, use `show --json` cursors. `show --json` returns turns under `.turns.data` and cursor fields under `.turns.nextCursor` / `.turns.backwardsCursor`.

```bash
page1=$(codex-threads show <thread_id> --last 10 --items summary --json)
echo "$page1" | jq '.turns.data[] | {id,status,startedAt,completedAt}'

cursor=$(echo "$page1" | jq -r '.turns.nextCursor')
codex-threads show <thread_id> --cursor "$cursor" --items summary --json
```

`messages --json` may report `truncated` and a `nextCursor`; use `show --cursor` for paging older thread history.

## Sending Follow-ups

Choose wait behavior from the user's wording:

- If the user says **ask** the agent/thread something, wait for the response.
- If the user says **tell** the agent/thread something, send it as fire-and-forget with `--no-wait`.
- If the wording is ambiguous or you are unsure whether they expect an answer, ask before sending.

Wait for a response:

```bash
codex-threads send <thread_id> "message" --json
```

Fire-and-forget:

```bash
codex-threads send <thread_id> "message" --no-wait --json
```

Blocking sends wait up to one hour. If the local wait times out, the command exits with code `3` and the remote turn may still be running.

If `send`, `steer`, or `settings set` sees Codex app-server's unloaded-thread error, it resumes/loads the thread and retries once. The resume uses yolo permissions by default unless global `--no-yolo` is passed.

Before sending, check status if there is any chance the thread is active:

```bash
codex-threads status <thread_id> --json
```

If active, tell the user and ask whether to queue/send anyway unless their intent is clear.

For multiline messages, write to a temp file and command-substitute it:

```bash
cat > /tmp/codex-followup.txt <<'MSG'
Your multiline message here.
MSG

codex-threads send <thread_id> "$(cat /tmp/codex-followup.txt)" --json
```

## Creating New Threads

Always pass an absolute cwd:

```bash
codex-threads new --cwd /home/kevin/worktrees/<repo> "Prompt here"
```

Optional flags:

```bash
--model <model>
--effort none|minimal|low|medium|high|xhigh
--service-tier <tier>
--name "Readable name"
--stream
--no-wait
--json
```

## Compact JSON Patterns

For custom JSON filtering, keep `jq` output short.

Recent thread list:

```bash
codex-threads list --limit 20 --json \
  | jq '{threads:[.threads[] | {id,name,cwd,preview,status:.status.type,updatedAt}]}'
```

Search results:

```bash
codex-threads search --limit 10 --json "query" \
  | jq '{results:[.results[] | {id:.thread.id,cwd:.thread.cwd,preview:.thread.preview,status:.thread.status.type,updatedAt:.thread.updatedAt,snippet:.snippet,annotation:.thread.annotation.text}]}'
```

Recent messages:

```bash
codex-threads messages <thread_id> --last 3 --max-turns 50 --json \
  | jq -r '.messages[] | "--- " + (.role // "?") + " turn=" + (.turnId // "") + "\n" + ((.text // "") | .[0:2000])'
```

## Command Shape Notes

- `list --json` returns `{ server, threads, nextCursor, backwardsCursor }`.
- `search --json` returns `{ server, results, nextCursor, backwardsCursor }`; each result has `thread` and `snippet`.
- `show --json` returns `{ server, thread, turns }`; turns are under `.turns.data`.
- When present, `list --json`, `search --json`, and `show --json` include `annotation` on thread objects.
- `messages --json` returns `{ server, threadId, messages, nextCursor, truncated }`.
- `status --json` returns `{ server, reachable, loadedThreadIds, nextCursor }`.
- `status <thread_id> --json` returns `thread`, `threadId`, `activeTurnId`, and `truncated`.
- `status <thread_id> --load --json` resumes/loads first, unsubscribes the probing connection, then returns the same shape.
- `settings show <thread_id> --json` returns cwd/model/effort/service tier.
- `goal get <thread_id> --json` may return `goal: null`.
- `annotate set/get/clear/list/search/prune --json` returns local annotation state for the selected server endpoint.

## Avoid

- Do not dump large raw JSON blobs to the user; summarize in tables.
- Do not rely on stale candidate IDs without checking cwd/preview/status.
- Do not send to an active thread without considering whether it will interrupt/confuse current work.
- Do not use `--connect` by default; config normally handles the main endpoint.
- Do not treat cursor strings as timestamps or offsets; pass them back exactly, quoted.
- Do not assume annotations exist in Codex app-server; they are local `codex-threads` state.
