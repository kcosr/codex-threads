# Changelog

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
