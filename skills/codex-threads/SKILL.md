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
codex-threads search threads "query" --limit 20
codex-threads search messages <thread_id> "query" --limit 20
codex-threads show <thread_id>
codex-threads messages <thread_id>
codex-threads status <thread_id>
codex-threads send <thread_id> "follow-up message"
codex-threads new --cwd /abs/path "initial prompt"
codex-threads fork <thread_id> --last-turn <turn_id>
codex-threads sections list
codex-threads sections create "Work"
codex-threads list --section <section_id> --sort section-position
codex-threads section <thread_id> --section <section_id>
codex-threads section <thread_id> --clear
codex-threads annotate get <thread_id>
codex-threads annotate set <thread_id> "note"
```

Use `list --parent <thread_id>` for direct spawned child threads and
`list --ancestor <thread_id>` for spawned descendants. These filters follow
app-server `parentThreadId` spawn edges, not `forkedFromId` history forks.

Use `sections list|create|rename|delete` to manage Codex-owned thread sections.
`sections list --cursor <cursor>` pages through sections. Use `list --section <id>`
or `list --unsectioned` to filter membership, and `--sort section-position` for
section order. `section <thread_id> --section <id> [--before <thread_id>]` moves
a thread; `section <thread_id> --clear` removes membership. Sections are persisted
by app-server, unlike local annotations.

Use `--json` whenever you need exact IDs, cwd, role, timestamps, status, cursors, parent IDs, or reliable parsing.

Default limits: `list` uses `--limit 50`, `show` uses `--last 20`, and `messages` uses `--max-turns 200` unless overridden.

Examples in this skill use `jq` for compact JSON projection; use another JSON tool if `jq` is not installed.

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

Annotations appear automatically in `list`, `search threads`, and `show`
output when present. In JSON, look for `thread.annotation.text`; in human
`list` and `search threads`, an `ANNOTATION` column appears only when displayed
rows have annotations.

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
   Use `list` or `search threads --json` to find candidate thread IDs and disambiguate by cwd/status/preview/snippet.
2. **Read recent context with human output.**
   Once a thread is selected, use `messages` without `--json` for readable review of recent conversation context. Prefer `--last N` for the final number of messages to display and set `--max-turns M` high enough to scan the recent turn window you need.
3. **Use JSON or `show` again for exact fields, older history, or pagination.**
   Use `--json` when you need `turnId`, `activeTurnId`, `nextCursor`, `truncated`, or exact machine parsing. Use `show --asc` / `show --cursor` for beginning-of-thread or older turn review; `messages` does not have a `--first` option.

Example:

```bash
codex-threads search threads --json --limit 10 "agent pack" \
  | jq '{results:[.results[] | {id:.thread.id,cwd:.thread.cwd,status:.thread.status.type,updatedAt:.thread.updatedAt,preview:.thread.preview,snippet:.snippet}]}'

codex-threads messages <thread_id> --last 4 --max-turns 50
```

Use `search threads` to discover candidate threads across a server. Once the
thread ID is known, use `messages` for readable recent context or `show` with
cursors for exact persisted history. Use `search messages <thread_id> "query"`
for exact occurrence snippets and turn cursors, with `--cursor` for further matches.
Codex 0.155.1 creates persisted threads with paginated history by default; older
legacy-history or ephemeral threads may return a server error for occurrence search.

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

### List/thread-search pagination

`list --json` and `search threads --json` return cursors such as `nextCursor`
and `backwardsCursor`.

Fetch the next page by passing the cursor back:

```bash
page1=$(codex-threads list --limit 20 --json)
echo "$page1" | jq -r '.nextCursor'

codex-threads list --limit 20 --cursor "$(echo "$page1" | jq -r '.nextCursor')" --json
```

Search works similarly:

```bash
page1=$(codex-threads search threads "agent pack" --limit 20 --json)
codex-threads search threads "agent pack" --limit 20 --cursor "$(echo "$page1" | jq -r '.nextCursor')" --json
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

`send` and `steer` read `canAcceptDirectInput` before submission and check it
again after an automatic resume. An explicit `false` means the thread is
read-only and the command exits with code `3` without submitting input. The TUI
uses the same safeguard. Absent or null capability data is unknown and does not
block the action.

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
--effort <effort>
--service-tier <tier>
--name "Readable name"
--stream
--no-wait
--json
```

Built-in completion suggestions for `--effort` are `none`, `minimal`, `low`,
`medium`, `high`, `xhigh`, `max`, and `ultra`; other non-empty app-server
effort values are passed through.

Use `codex-threads fork <thread_id>` to create a new thread from existing
history. Pass `--last-turn <turn_id>` to fork through a specific turn and use
the same `--model`, `--effort`, `--service-tier`, `--name`, and `--json` flags
when the fork should start with explicit settings. Without explicit model,
effort, or service-tier flags, the fork inherits those settings from the source
thread.

## Compact JSON Patterns

For custom JSON filtering, keep `jq` output short.

Recent thread list:

```bash
codex-threads list --limit 20 --json \
  | jq '{threads:[.threads[] | {id,name,cwd,preview,status:.status.type,updatedAt}]}'
```

Search results:

```bash
codex-threads search threads --limit 10 --json "query" \
  | jq '{results:[.results[] | {id:.thread.id,cwd:.thread.cwd,preview:.thread.preview,status:.thread.status.type,updatedAt:.thread.updatedAt,snippet:.snippet,annotation:.thread.annotation.text}]}'
```

Recent messages:

```bash
codex-threads messages <thread_id> --last 3 --max-turns 50 --json \
  | jq -r '.messages[] | "--- " + (.role // "?") + " turn=" + (.turnId // "") + "\n" + ((.text // "") | .[0:2000])'
```

## Command Shape Notes

- `list --json` returns `{ server, threads, nextCursor, backwardsCursor }`.
- `search threads --json` returns `{ server, results, nextCursor, backwardsCursor }`; each result has `thread` and `snippet`.
- `show --json` returns `{ server, thread, turns }`; turns are under `.turns.data`.
- When present, `list --json`, `search threads --json`, and `show --json` include `annotation` on thread objects.
- Human `list` and `search threads` output includes a `PARENT ID` column when any displayed thread has `parentThreadId`; use `--json` for a stable shape.
- `messages --json` returns `{ server, threadId, messages, nextCursor, truncated }`.
- `fork <thread_id> --json` returns `threadId`, `forkedFromThreadId`, optional `lastTurnId`, and applied model/effort/service tier fields.
- Forked thread objects expose `forkedFromId`; `list --parent` and `list --ancestor` do not find forks.
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
