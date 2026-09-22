# Changelog

## [Unreleased]

### Breaking Changes

- Update the reviewed Codex baseline to 0.155.1. Replace removed pin/unpin
  commands and pinned filters with server-owned section commands and filters;
  older Codex servers do not support the new section contract
  ([#13](https://github.com/kcosr/codex-threads/pull/13)).

### Added

- Add section list/create/rename/delete, thread movement and ordering, section
  filters, section-position sorting, and section display in CLI and TUI output.
- Expose `search messages THREAD_ID QUERY` with occurrence IDs, match ranges,
  and history cursors for Codex paginated-history threads.
- Add an isolated real-Codex offline smoke harness covering thread lifecycle,
  settings, goals, history, sections, and persisted resume without provider usage
  ([#13](https://github.com/kcosr/codex-threads/pull/13)).

### Fixed

- Apply yolo permissions through the app-server before opening a remote Codex
  TUI, supporting 0.155.1's remote-resume restrictions.
- Separate server-request IDs from client-response IDs and bound RPC deadlines
  while unrelated notifications arrive.
- Report daemon-draining refusals without automatically replaying work, and
  correlate polled turns by their exact IDs instead of prompt/time heuristics
  ([#13](https://github.com/kcosr/codex-threads/pull/13)).

## [0.2.4] - 2026-07-30

### Breaking Changes

- Replace `search QUERY` with the explicit `search threads QUERY` command
  shape. The removed form is not retained as an alias
  ([#12](https://github.com/kcosr/codex-threads/pull/12)).

### Added

- Add persisted Codex thread `pin` and `unpin` commands, `list --pinned` and
  `list --unpinned` filters, and pin state in human list output
  ([#12](https://github.com/kcosr/codex-threads/pull/12)).
- Add `CODEX_COMPATIBILITY.md` to record exact upstream Codex references,
  adopted app-server features, and reviewed follow-up candidates for each
  integration update
  ([#12](https://github.com/kcosr/codex-threads/pull/12)).

### Changed

- Reject send and steer operations when a thread explicitly reports
  `canAcceptDirectInput: false`, including when that state is learned after an
  automatic resume. Apply the same safeguard in the TUI composer
  ([#12](https://github.com/kcosr/codex-threads/pull/12)).

## [0.2.3] - 2026-07-19

### Added

- Add repeatable `list` and `tui` provider/source thread filters, and a confirmed TUI-only thread delete action
  ([#11](https://github.com/kcosr/codex-threads/pull/11)).
- Add per-server, opt-in `usage redeem`, which automatically redeems the best detailed rate-limit reset credit; use the same deterministic selection in the TUI
  ([#11](https://github.com/kcosr/codex-threads/pull/11)).

### Fixed

- Preserve a successful CLI reset redemption result when its follow-up usage refresh fails
  ([#11](https://github.com/kcosr/codex-threads/pull/11)).

## [0.2.2] - 2026-07-09

### Added

- Add `fork THREAD_ID` for creating Codex app-server thread forks, with
  `--last-turn` support for forking through a specific completed turn and
  explicit model, effort, and service-tier overrides when needed
  ([#10](https://github.com/kcosr/codex-threads/pull/10)).
- Add `list --parent THREAD_ID` and `list --ancestor THREAD_ID` filters for
  browsing spawned direct child threads or all spawned descendant threads
  ([#10](https://github.com/kcosr/codex-threads/pull/10)).
- Accept `max` and `ultra` as model reasoning effort suggestions, and pass
  through other non-empty app-server-supported effort values
  ([#10](https://github.com/kcosr/codex-threads/pull/10)).

## [0.2.1] - 2026-06-22

### Added

- Show Codex rate-limit reset-credit availability in `usage` output when
  provided by app-server.
- Add a TUI usage modal with reset-credit display and explicit confirmation
  before redeeming a banked Codex rate-limit reset.

## [0.2.0] - 2026-06-15

### Added

- Add `codex-threads tui`, an interactive terminal UI for browsing, viewing,
  searching, streaming, and controlling threads
  ([#6](https://github.com/kcosr/codex-threads/pull/6)).

## [0.1.5] - 2026-06-05

### Added

- Add local thread annotations with `annotate set/get/clear/list/search/prune`,
  endpoint-scoped JSON state, and annotation projection in `list`, `search`, and
  `show` output ([#5](https://github.com/kcosr/codex-threads/pull/5)).

## [0.1.4] - 2026-06-04

### Added

- Add endpoint-based server configuration for `unix://`, `ws://`, and
  `wss://` Codex app-server targets
  ([#4](https://github.com/kcosr/codex-threads/pull/4)).
- Add WebSocket-over-TCP app-server connections with optional bearer-token auth
  from `auth_token_env`, `auth_token`, `--connect-auth-token-env`, or
  `--connect-auth-token`
  ([#4](https://github.com/kcosr/codex-threads/pull/4)).

### Changed

- Normalize `servers` output around endpoint strings
  ([#4](https://github.com/kcosr/codex-threads/pull/4)).
- Deprecated legacy `type = "uds"` plus `path` server config; existing configs
  continue to work with a warning
  ([#4](https://github.com/kcosr/codex-threads/pull/4)).

### Fixed

- Keep `servers` listing from resolving auth token environment variables, and
  report unresolved auth for `servers ping --all` as a per-server failure
  ([#4](https://github.com/kcosr/codex-threads/pull/4)).
- Reject unknown config fields so misspelled auth keys do not silently drop
  credentials
  ([#4](https://github.com/kcosr/codex-threads/pull/4)).

## [0.1.3] - 2026-06-04

### Added

- Add shell completion setup and generated bash, zsh, and fish completion
  scripts for commands, options, static values, and configured server aliases
  ([#3](https://github.com/kcosr/codex-threads/pull/3)).

## [0.1.2] - 2026-06-03

### Added

- Add `usage` to show account plan, credits, and rate-limit windows from Codex
  app-server
  ([#2](https://github.com/kcosr/codex-threads/pull/2)).

### Fixed

- Improved release-script preflight checks, diagnostics, and changelog validation edge cases.

## [0.1.1] - 2026-06-03

### Added

- Add `status THREAD_ID --load` to explicitly resume/load a thread before
  reporting status
  ([#1](https://github.com/kcosr/codex-threads/pull/1)).
- Support top-level and per-server `model` and `model_reasoning_effort` config
  defaults for new threads
  ([#1](https://github.com/kcosr/codex-threads/pull/1)).

### Changed

- Include `CHANGELOG.md` and `skills/` in documented release archive contents
  ([#1](https://github.com/kcosr/codex-threads/pull/1)).

### Fixed

- Correct documented release upload tag to use the `vX.Y.Z` tag created by the
  release script
  ([#1](https://github.com/kcosr/codex-threads/pull/1)).
- Correct release and changelog documentation now that `0.1.0` has shipped
  ([#1](https://github.com/kcosr/codex-threads/pull/1)).
- Document live smoke goal checks in `smoke/README.md`
  ([#1](https://github.com/kcosr/codex-threads/pull/1)).

## [0.1.0] - 2026-06-01

### Added

- Initial `codex-threads` release.
