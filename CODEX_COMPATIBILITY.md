# Codex compatibility

This file records intentional reviews of the Codex app-server API. It is a
provenance log, not a promise that older or newer app-server builds will support
every command: Codex experimental APIs can change between releases.

Use the exact upstream tag and commit that were inspected. When a
`codex-threads` change adopts API additions from a new Codex release, update the
`Unreleased` row in the same change. The release script replaces `Unreleased`
with the released `codex-threads` version in the release commit.

| codex-threads | Codex app release | Upstream reference | Reviewed integration scope |
| --- | --- | --- | --- |
| Unreleased | 0.156.1 | `rust-v0.156.1` (`b412ff32c417f855c2b2d1581b77058eed87c84b`) | Qualify existing UDS connections through the new symlink socket endpoint; no runtime changes required. Retain the 0.155.1 integration: replace removed boolean pins with persisted sections (`threadSection/*`, `thread/section/move`, tri-state `sectionId`, `section_position` sorting). Expose occurrence search for supported paginated history; persisted local threads now normally select that mode. Apply remote-TUI yolo permissions through `thread/settings/update` instead of rejected CLI flags. Fix RPC ID namespaces and deadlines, drain refusal handling, and exact turn correlation. Thread lifecycle, settings, goals, fork/history, model listing, account usage, and reset-credit requests reviewed. Native attachments, project/environment administration, active-turn settings, user verification, memory administration, section appearance editing, per-thread disabled plugin settings, access-program selection, rollout compression, and raw new tool-item rendering remain deferred. |
| 0.2.4 | 0.146 | `rust-v0.146.0` (`e363b08c9175ac1cbe5893615dd2cb9ddf95043b`) | Persisted thread pin/unpin through `thread/metadata/update`, `thread/list` pin filtering, and `Thread.isPinned` rendering; and direct-input safeguards through `Thread.canAcceptDirectInput`. Persisted occurrence search through `thread/searchOccurrences` remains internally implemented but is not exposed as a CLI command: release 0.146 returns unsupported for legacy-history threads, and legacy remains the default history mode. Experimental fork/history additions remain deferred. Peer `main` already has a post-0.146 persisted-section model that supersedes the boolean pin API; reassess it when targeting a release newer than 0.146 rather than carrying both contracts. |
| 0.2.3 | 0.143 inherited | No newer baseline was recorded | Added provider/source filters, TUI deletion, and detailed rate-limit reset redemption. This release did not document a separate Codex API review, so 0.143 remains the last evidenced baseline. |
| 0.2.2 | 0.143 | `rust-v0.143.0` (`c4d748f586a84a3ed5b6aceb82e9a1db4abb1cda`) | Explicit Codex 0.143 integration update: thread fork, parent/ancestor relationship filters, and expanded reasoning-effort pass-through. |

## Review checklist

For each intentional Codex release sync:

1. Compare the public app-server protocol and app-server README from the last
   recorded upstream reference to the new exact tag or commit.
2. Classify additions as adopted, intentionally deferred, or irrelevant to this
   CLI.
3. Update the table above, `README.md`, tests, and `CHANGELOG.md` in the same
   change as adopted user-facing behavior.
4. Keep deferred items in the newest table row so the next review does not
   rediscover them from scratch.

## 0.156.1 review and verification

Reviewed on 2026-09-24 against `rust-v0.156.1`
(`b412ff32c417f855c2b2d1581b77058eed87c84b`), comparing the public app-server
protocol with `rust-v0.155.1` (`be2951ea34f0d295ed0becf97079f92fa5f6950e`).
No runtime changes or minimum-version gate are needed for the APIs this CLI uses.

- **Socket endpoints:** the Unix transport introduced in 0.156.0 exposes the
  configured socket path as a symlink to a socket in Codex's protected daemon
  directory. Existing `UnixStream::connect` calls follow it. Keep the configured
  endpoint instead of storing the resolved target; endpoint-based local state
  continues to use that configured identity. Mock CLI coverage checks both named
  servers and direct `--connect` paths. The real-Codex smoke asserts a symlink
  resolving to a socket on initial startup and after app-server restart.
- **Protocol:** removal of `thread/rollback` does not affect this CLI, which
  never called it. Optional disabled-plugin settings, access-program metadata,
  file-backed image inputs, collaboration-mode metadata, and MCP app presentation
  fields require no changes to existing text commands. Plugin controls, access
  programs, attachment features, and the new `rollout/compress` administration
  method remain deferred, along with the features listed in the 0.155.1 review.
- **Qualification:** `smoke/offline_codex.mjs` now requires exactly 0.156.1.
  The Linux x86_64 offline fixture covers turns, messages, occurrence search,
  settings, goals, forks, sections/order, restart/resume, and archive/unarchive
  without credentials or live provider capacity. This does not qualify macOS,
  live providers, or externally operated servers.

Validation on Linux x86_64: formatting, 206 library tests, 71 mock CLI tests,
Clippy across all targets/features with warnings denied, the release build,
and the real-Codex offline smoke passed. The first real-Codex run exceeded its
five-second startup allowance while other tests ran; the standalone run passed
with two loopback model requests. The unchanged optional PTY suite had varying
timeout failures in parallel and serial runs. A serial run with
`TOKIO_WORKER_THREADS=2` passed 11 of 14 tests; history paging, detail refresh,
and preview reuse timed out. This review does not claim a clean PTY suite.

## 0.155.1 review and verification

Reviewed on 2026-09-22 from the exact 0.146.0 and 0.155.1 release trees.
GitHub's latest stable release endpoint identified 0.155.1 on that date.
No minimum-version handshake gate or older wire-shape fallback is introduced;
the current commands use the reviewed API and report unsupported operations
from other app-server versions normally.

- **Sections:** the boolean `isPinned` request/response contract is retired.
  `threadSection/list`, `create`, `update`, and `delete` manage independent
  records; `thread/section/move` requires a nullable `sectionId`. List omission
  means all threads, explicit null means unsectioned, and an ID selects a
  section. Section membership and ordering remain server-owned.
- **History:** app-server `request_processors/thread_processor.rs` defaults new
  non-ephemeral threads to paginated history when the store supports it.
  Local stores with a state database support this; occurrence search still
  rejects legacy history. No automatic history migration or local rollout
  parsing is performed. Search results retain item IDs, turn IDs, match ranges,
  and exact history cursors.
- **Turns and transport:** `turn/start` returns the authoritative started or
  steered turn ID. Polling must not substitute another turn with the same
  prompt. Unrelated notifications, including stored attachment updates, are
  accepted without changing response correlation. A draining refusal is known
  rejected; timeout or connection loss is not proof that a mutation failed.
- **Remote Codex TUI:** upstream `tui/src/app/config_persistence.rs` rejects
  permission CLI overrides on remote resume. The app-server settings mutation
  happens before launch, and failures prevent launch. `--no-yolo` leaves
  permissions untouched. This is independent of this CLI's own Ratatui browser.
- **Compatible existing APIs:** thread creation, loading, listing, search,
  naming, deletion, settings, fork, archive, turn control, goals, model listing,
  rate-limit reads, and reset-credit consumption retain the request fields this
  CLI uses. Additional metadata is preserved in JSON output where that output
  projects the corresponding server objects. User/assistant transcript views
  continue to focus on message items rather than treating tool outputs as
  assistant messages.
- **Deferred features:** stored attachment records, project/environment and
  memory administration, account verification, active-turn settings changes,
  native section appearance controls, and additional tool-item rendering are
  outside this update. Existing local annotations remain local annotations.

`smoke/offline_codex.mjs` verifies the compiled CLI against the exact reviewed
Codex executable in a disposable home with a loopback Responses fixture. It
checks real turn completion, messages and occurrence search, settings, goals,
forks, sections and their ordering, restart persistence/resume, and archiving.
It uses no account credentials or live model capacity. Deterministic unit,
mock-transport CLI, and opt-in PTY tests cover the other changed paths. This
review does not establish live-provider, macOS, or externally operated remote
server qualification.

Validation completed on Linux x86_64: formatting, 206 library tests, 70 binary
mock integration tests, all 14 opt-in offline PTY tests, Clippy across all
features/targets with warnings denied, a no-default-features build check, and
an optimized release build. The real-Codex offline smoke passed for both debug
and release binaries with exactly two loopback model requests.
