# codex-threads Annotations Design

## Summary

Add a local, stateful `annotate` command group for user-maintained notes on
Codex threads. Codex app-server does not provide annotations, so
`codex-threads` should persist them in its own JSON state file and project them
into read commands where they are useful.

This is a local metadata feature. It should not change app-server thread state,
thread history, archive state, or settings. The implementation should remain
end-state oriented: one storage shape, one keying model, and no compatibility
fallbacks unless a migration is explicitly requested later.

## Goals

- Let users set, get, clear, list, search, and prune local annotations for
  Codex threads.
- Make annotations visible in `list`, `search`, and `show` without changing
  app-server RPC contracts.
- Avoid annotation collisions across configured servers and direct connection
  targets.
- Keep stale annotations visible through annotation-specific commands unless the
  user explicitly prunes or clears them.
- Provide deterministic offline tests for state behavior and mock app-server
  tests for command integration.

## Non-Goals

- Do not store annotations in Codex app-server.
- Do not parse local Codex rollout/session files.
- Do not make regular `search QUERY` search local annotation text.
- Do not add partial edit, append, tags, or rich metadata in the initial feature.
- Do not silently migrate or support multiple annotation file shapes.

## Relevant Current Code

- CLI commands live in `src/cli.rs`; the top-level command enum is currently
  defined around `Command::{List, Search, Show, ...}`.
- Command dispatch and orchestration live in `src/app.rs`; `list_command`,
  `search_command`, and `show_command` fetch app-server data and then render it.
- `emit_threads_result` in `src/app.rs` renders human and JSON output for
  `list` and `search`.
- `print_thread_detail` in `src/app.rs` renders human `show` output.
- Target resolution lives in `src/config.rs`; configured servers use an alias as
  `Target.server`, while direct `--connect` targets use the endpoint string.
- Current config path resolution points at
  `~/.config/codex-threads/config.toml`; annotation state should not reuse this
  config file location.
- Mock integration tests in `tests/mock_smoke.rs` already assert JSON and human
  output behavior for read commands.

## Command Design

Add a top-level command group:

```text
codex-threads annotate <subcommand>
```

All subcommands that operate within one target accept `--server ALIAS` and
`--json`. Direct `--connect ENDPOINT` continues to work through existing global
target resolution.

Recommended initial subcommands:

```text
codex-threads annotate set THREAD_ID TEXT [--server ALIAS] [--json]
codex-threads annotate get THREAD_ID [--server ALIAS] [--json]
codex-threads annotate clear THREAD_ID [--server ALIAS] [--json]
codex-threads annotate list [--server ALIAS] [--query TEXT] [--json]
codex-threads annotate search QUERY [--server ALIAS] [--json]
codex-threads annotate prune [--server ALIAS] [--dry-run] [--json]
```

### `annotate set`

Sets or replaces the full annotation text for one thread.

Behavior:

- Requires non-empty `TEXT` after trimming.
- Does not contact app-server by default; users can annotate a thread id even if
  the thread is not currently listable.
- Updates `updatedAt`; sets `createdAt` only on initial creation.
- Human output uses key-value status output: `server`, `threadId`, `status`.
- JSON output returns `{ server, threadId, annotation, status }`.

### `annotate get`

Reads one annotation.

Behavior:

- Does not contact app-server.
- If missing, exit with code `2` and a clear error message. This matches the
  existing command convention that a specific missing lookup is not success,
  while keeping code `1` reserved for failed or interrupted blocking turns.
- JSON output for a hit returns `{ server, threadId, annotation }`.
- JSON output for a miss does not emit a success object.

### `annotate clear`

Deletes one annotation if present.

Behavior:

- Does not contact app-server.
- Idempotent clear is acceptable: clearing a missing annotation returns
  `{ cleared: false }` rather than failing.
- JSON output returns `{ server, threadId, cleared, status }`.

### `annotate list`

Lists local annotations.

Behavior:

- Does not require app-server reachability.
- With `--server`, lists one endpoint namespace.
- Without `--server`, follows existing target rules only if a single configured
  server or direct `--connect` is selected. Avoid defaulting to all namespaces,
  because most app-server commands operate on a single target.
- `--query TEXT` filters annotation text locally, case-insensitively.
- Human table columns: `UPDATED`, `SERVER`, `THREAD ID`, `ANNOTATION`.
- JSON output returns `{ annotations: [...] }`.

### `annotate search`

Alias for annotation-text search with a required query.

Behavior:

- Searches local annotation text only.
- Same output shape as `annotate list --query QUERY`.
- This keeps regular `search QUERY` as an app-server transcript search.

### `annotate prune`

Removes stale annotations whose underlying threads can no longer be found.

Behavior:

- Contacts app-server.
- With `--dry-run`, reports what would be removed without writing.
- Should only inspect the selected server namespace.
- A conservative implementation can check each annotated thread with
  `thread/read` and treat app-server "thread not found" as stale.
- If app-server is unreachable, return an error and do not prune.
- JSON output returns `{ server, checked, stale, removed, dryRun }`.

## State File Location

Use an XDG-style state directory, not the config file directory:

```text
$CODEX_THREADS_STATE/annotations.json
$XDG_STATE_HOME/codex-threads/annotations.json
~/.local/state/codex-threads/annotations.json
```

`CODEX_THREADS_STATE` should be a directory override, not a full file path. This
keeps test setup simple and leaves room for future state files.

Do not place mutable annotation state under
`~/.config/codex-threads/config.toml`. Config is declarative; annotations are
local runtime/user state.

## State Schema

Recommended schema:

```json
{
  "version": 1,
  "namespaces": {
    "unix:///tmp/codex.sock": {
      "displayServer": "work",
      "endpoint": "unix:///tmp/codex.sock",
      "threads": {
        "thread_1": {
          "text": "Follow up on release notes",
          "createdAt": 1791240000,
          "updatedAt": 1791240300
        }
      }
    }
  }
}
```

Rules:

- `version` is required and must be `1`.
- Namespace keys are canonical endpoint strings.
- `displayServer` is informational and may change when aliases change.
- `endpoint` duplicates the namespace key for easier manual inspection.
- Thread keys are app-server thread ids.
- Annotation text is plain UTF-8 text.
- Timestamps are epoch seconds to match existing app-server-style JSON output.

Rust structs should use `serde(deny_unknown_fields)` for strict validation.

## Keying Model

Key annotations by:

```text
canonical server endpoint + thread id
```

Do not key by only thread id, only server alias, cwd, or local session path.

Rationale:

- Thread ids may collide across app-server instances.
- Server aliases can collide across different config files or be renamed.
- Direct `--connect` already identifies targets by endpoint string.
- Cwd is mutable/filter metadata, not thread identity.
- Session paths are not part of the public `codex-threads` contract.

Endpoint canonicalization should mirror existing endpoint display/validation
behavior:

- `unix://` namespaces use normalized `unix://PATH` display.
- `ws://` and `wss://` namespaces use parsed URL string form.
- Auth tokens are never included in namespace keys.

Implementation may need a small public helper on `Target` or `Endpoint` to
return this canonical annotation namespace.

## Integration With `list`

Current `list` calls `thread/list`, optionally filters by `--since`, then
passes the result to `emit_threads_result`.

Recommended behavior:

- Load annotation state after obtaining the app-server result.
- For each returned thread, look up annotation by endpoint namespace and
  `thread.id`.
- JSON output: add `annotation` to each thread object only when present.
- Human output: add an `ANNOTATION` column only when at least one displayed row
  has an annotation.
- Annotation column should use existing whitespace sanitization and capped-cell
  truncation. A width around `40` is reasonable.

Do not let annotations affect app-server pagination or `--since` filtering.

## Integration With `search`

Current `search` calls `thread/search` and renders app-server snippets.

Recommended behavior:

- Load annotations after app-server search results are returned.
- JSON output: attach `annotation` to `result.thread` only when present.
- Human output: add an `ANNOTATION` column only when at least one returned
  result has an annotation.
- Keep the existing app-server `SNIPPET` column.
- Do not include annotation-only matches in `search QUERY`; use
  `annotate search QUERY` for local annotation text.

This preserves `search` as transcript/server search and avoids mixing local
state into app-server result counts or cursors.

## Integration With `show`

Current `show` calls `thread/read` and `thread/turns/list`, then renders thread
metadata and turns.

Recommended behavior:

- Load annotation state after `thread/read`.
- JSON output: add `annotation` to `thread` only when present.
- Human output: include an `annotation` key in the initial key-value metadata
  for single-line annotations.
- For multiline annotations, print:

```text
annotation
  first line
  second line
```

Then continue with the turns table. This avoids collapsing meaningful note
formatting in detail view.

## Archive and Unarchive Behavior

Annotations should be independent of Codex archive state.

Behavior:

- `archive THREAD_ID` does not change annotations.
- `unarchive THREAD_ID` does not change annotations.
- If a thread appears in `list --archived`, `search --archived`, or later
  unarchived views, its annotation is projected normally.

Rationale:

- Archive state lives in app-server.
- Annotation state is local metadata keyed by stable endpoint/thread id.
- Automatically deleting annotations on archive would make archive unexpectedly
  destructive.

## Deleted, Missing, and Stale Threads

If the underlying Codex thread is deleted or cannot be found:

- Keep the local annotation.
- Do not show it in normal `list`, `search`, or `show` unless app-server returns
  the thread.
- `annotate get`, `annotate list`, and `annotate search` still show local
  annotation records.
- `annotate prune` is the explicit cleanup mechanism.

If `show THREAD_ID` fails because the thread is missing, do not fall back to
displaying only the annotation. That would blur the distinction between
app-server thread commands and local annotation commands. Users can run
`annotate get THREAD_ID`.

## File Locking and Atomic Writes

State operations should be safe for multiple `codex-threads` processes.

Recommended write flow:

1. Create the state directory if needed.
2. Acquire an exclusive lock for mutating operations.
3. Read and validate the current file while holding the lock.
4. Apply the mutation in memory.
5. Write JSON to a temp file in the same directory.
6. Flush and fsync the temp file where practical.
7. Atomically rename temp file over `annotations.json`.
8. Fsync the parent directory where supported.
9. Release the lock.

Read-only operations should acquire a shared lock if the selected locking crate
supports it. Otherwise, a brief exclusive lock is acceptable for simplicity.

Use a sidecar lock file such as:

```text
annotations.json.lock
```

Use the `fd-lock` crate for the sidecar lock file. It provides a focused
cross-platform advisory file lock API for this read-modify-write state file.

## Corruption Handling

If `annotations.json` exists but cannot be parsed or validated:

- Return a clear error with the path.
- Do not overwrite the file.
- Do not silently create a new empty state file.
- Do not attempt best-effort partial repair in normal commands.

Users can move the corrupt file aside manually. Normal commands should not
attempt partial repair or automatic repair.

## Schema Versioning

Version `1` is the only accepted schema for the initial feature.

Behavior:

- Missing `version`: error.
- Unknown version: error.
- Unknown fields: error.
- Future migrations should be explicit commands or explicit versioned migration
  paths, not silent dual-shape parsing.

This follows the repo guidance to avoid backward-compatibility fallbacks and
dual-shape parsers unless explicitly requested.

## JSON Output Shapes

### `annotate set --json`

```json
{
  "server": "work",
  "threadId": "thread_1",
  "annotation": {
    "text": "Follow up on release notes",
    "createdAt": 1791240000,
    "updatedAt": 1791240300
  },
  "status": "accepted"
}
```

### `annotate get --json`

```json
{
  "server": "work",
  "threadId": "thread_1",
  "annotation": {
    "text": "Follow up on release notes",
    "createdAt": 1791240000,
    "updatedAt": 1791240300
  }
}
```

### `annotate clear --json`

```json
{
  "server": "work",
  "threadId": "thread_1",
  "cleared": true,
  "status": "accepted"
}
```

### `list --json`

```json
{
  "server": "work",
  "threads": [
    {
      "id": "thread_1",
      "annotation": {
        "text": "Follow up on release notes",
        "createdAt": 1791240000,
        "updatedAt": 1791240300
      }
    }
  ],
  "nextCursor": null,
  "backwardsCursor": null
}
```

The thread object also includes all app-server-provided fields; this example is
truncated to show annotation placement.

### `search --json`

```json
{
  "server": "work",
  "results": [
    {
      "thread": {
        "id": "thread_1",
        "annotation": {
          "text": "Follow up on release notes",
          "createdAt": 1791240000,
          "updatedAt": 1791240300
        }
      },
      "score": 1.0,
      "snippet": "release process"
    }
  ],
  "nextCursor": null,
  "backwardsCursor": null
}
```

### `show --json`

```json
{
  "server": "work",
  "thread": {
    "id": "thread_1",
    "annotation": {
      "text": "Follow up on release notes",
      "createdAt": 1791240000,
      "updatedAt": 1791240300
    }
  },
  "turns": {
    "data": []
  }
}
```

## Human Output

### `annotate get`

```text
server      work
threadId    thread_1
annotation  Follow up on release notes
updated     2026-06-05 10:05:00
```

### `annotate list`

```text
UPDATED              SERVER  THREAD ID  ANNOTATION
2026-06-05 10:05:00  work    thread_1   Follow up on release notes
```

### `list`

Without annotations, keep the current columns unchanged.

With annotations present in displayed rows:

```text
UPDATED              STATUS  TITLE/PREVIEW  ANNOTATION                    THREAD ID
2026-06-05 10:05:00  idle    Release notes  Follow up on release notes    thread_1
```

### `search`

Keep app-server snippet and add annotation only when needed:

```text
UPDATED              STATUS  TITLE/PREVIEW  SNIPPET          ANNOTATION                  THREAD ID
2026-06-05 10:05:00  idle    Release notes  release process  Follow up on release notes  thread_1
```

### `show`

```text
server      work
id          thread_1
name        Release notes
cwd         /repo
status      idle
annotation  Follow up on release notes
```

## Tests

### Unit Tests

Add tests for a new annotation state module:

- Resolves default state path from `HOME`.
- Honors `XDG_STATE_HOME`.
- Honors `CODEX_THREADS_STATE`.
- Creates empty state when the file is absent.
- Round-trips schema version `1`.
- Rejects missing version.
- Rejects unknown version.
- Rejects unknown fields.
- Rejects corrupt JSON without overwriting it.
- Sets a new annotation with `createdAt` and `updatedAt`.
- Replaces existing annotation while preserving `createdAt`.
- Gets an existing annotation.
- Clears an existing annotation.
- Clears a missing annotation idempotently.
- Lists by namespace.
- Searches annotation text case-insensitively.

### Integration/Mock Smoke Tests

Extend `tests/mock_smoke.rs`:

- `annotate set --json` writes state and returns annotation.
- `annotate get --json` reads it without app-server interaction if practical to
  assert.
- `annotate clear --json` removes it.
- `annotate list --json` returns local records.
- `annotate search --json` finds local annotation text.
- `list --json` projects `thread.annotation`.
- `search --json` projects `results[0].thread.annotation`.
- `show --json` projects `thread.annotation`.
- Human `list` keeps existing columns when no displayed annotations exist.
- Human `list` adds `ANNOTATION` when at least one displayed annotation exists.
- Human `search` keeps app-server `SNIPPET` and adds `ANNOTATION` when needed.
- Archive/unarchive commands do not remove annotations.
- `prune --dry-run --json` reports stale annotations without deleting.
- `prune --json` removes annotations for thread ids that app-server reports as
  missing.

Use `CODEX_THREADS_STATE` pointed at a temp directory for deterministic tests.

### Concurrency Tests

If practical, add a deterministic test that runs several state mutations through
the state module and verifies final JSON validity. Full multi-process locking
tests may be brittle; keep them focused if added.

## Implementation Steps

1. Add `src/annotations.rs` with strict serde structs, path resolution, endpoint
   namespace helpers, load/save, mutation, query, and prune support helpers.
2. Add a canonical annotation namespace helper on `Target` or `Endpoint`.
3. Add `AnnotateCommand` and subcommands in `src/cli.rs`.
4. Wire `Command::Annotate` dispatch in `src/app.rs`.
5. Implement `annotate set/get/clear/list/search`.
6. Implement file locking and atomic writes for state mutations.
7. Implement corruption and schema-version errors.
8. Add projection helpers that attach annotations to list/search/show JSON
   values after app-server data is fetched.
9. Update human renderers for conditional annotation columns and `show`
   metadata.
10. Implement `annotate prune` using app-server thread existence checks.
11. Add unit tests for the annotation state module.
12. Add mock smoke tests for command behavior and projection.
13. Update `README.md` command and JSON documentation.
14. Update `CHANGELOG.md` under `## [Unreleased]`.
15. Run `cargo fmt --check`, `cargo test`, and
   `cargo clippy --all-targets --all-features`.

## Resolved Decisions

- Do not support `--stdin` in the first annotations release.
- `annotate get THREAD_ID` exits with code `2` when no local annotation exists.
- Use `fd-lock` for the sidecar lock file.
