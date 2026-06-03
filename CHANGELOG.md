# Changelog

## [Unreleased]

### Added

- Add `status THREAD_ID --load` to explicitly resume/load a thread before
  reporting status.
- Support top-level and per-server `model` and `model_reasoning_effort` config
  defaults for new threads
  ([#1](https://github.com/kcosr/codex-threads/pull/1)).

### Changed

- Include `CHANGELOG.md` and `skills/` in documented release archive contents.

### Fixed

- Correct documented release upload tag to use the `vX.Y.Z` tag created by the
  release script.
- Correct release and changelog documentation now that `0.1.0` has shipped.
- Document live smoke goal checks in `smoke/README.md`.

## [0.1.0] - 2026-06-01

### Added

- Initial `codex-threads` release.
