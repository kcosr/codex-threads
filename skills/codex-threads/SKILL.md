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

The user normally has config at `~/.config/codex-threads/config.toml` pointing at the main Unix socket, so do **not** pass `--connect` unless debugging or explicitly targeting another server.

`codex-threads` only sees threads served by a running Codex app-server. The app-server must be started with a listener such as:

```bash
CODEX_SOCK=unix:///var/run/user/1000/codex.sock
codex app-server --listen "$CODEX_SOCK"
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
```

Use `--json` whenever you need exact IDs, cwd, role, timestamps, status, cursors, or reliable parsing.

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

If `messages --role user|assistant` is available, use it to reduce output when looking for intent:

```bash
codex-threads messages <thread_id> --role user --last 10 --max-turns 100
codex-threads messages <thread_id> --role assistant --last 3 --max-turns 50
```

If role filtering is not available in the installed version, fall back to JSON + `jq`:

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
codex-threads status <thread_id> --json \
  | jq '{threadId, activeTurnId, status:.thread.status, cwd:.thread.cwd, preview:.thread.preview}'
```

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

`messages --json` may report `truncated` and a `nextCursor`, but if the installed `messages` command does not accept `--cursor`, switch to `show --cursor` for paging older thread history.

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

Until all desired human filtering is available, keep `jq` output short.

Recent thread list:

```bash
codex-threads list --limit 20 --json \
  | jq '{threads:[.threads[] | {id,name,cwd,preview,status:.status.type,updatedAt}]}'
```

Search results:

```bash
codex-threads search --limit 10 --json "query" \
  | jq '{results:[.results[] | {id:.thread.id,cwd:.thread.cwd,preview:.thread.preview,status:.thread.status.type,updatedAt:.thread.updatedAt,snippet:.snippet}]}'
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
- `messages --json` returns `{ server, threadId, messages, nextCursor, truncated }`.
- `status --json` returns `{ server, reachable, loadedThreadIds, nextCursor }`.
- `status <thread_id> --json` returns `thread`, `threadId`, `activeTurnId`, and `truncated`.
- `settings show <thread_id> --json` returns cwd/model/effort/service tier.
- `goal get <thread_id> --json` may return `goal: null`.

## Avoid

- Do not dump large raw JSON blobs to the user; summarize in tables.
- Do not rely on stale candidate IDs without checking cwd/preview/status.
- Do not send to an active thread without considering whether it will interrupt/confuse current work.
- Do not use `--connect` by default; config normally handles the main socket.
- Do not treat cursor strings as timestamps or offsets; pass them back exactly, quoted.
