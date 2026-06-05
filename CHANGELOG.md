# Changelog

## [Unreleased]

### Added

- Add `codex-threads tui`, an interactive thread browser with list/search
  filters, sort/column menus, preview pane, detail viewing, loaded-message
  search, local annotation editing, refresh, send-and-stream, no-wait send,
  active-turn attach/steer/interrupt, persisted TUI preferences, mouse support,
  Vim-style navigation, and OSC 52 thread id copy.

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
