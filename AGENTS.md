# codex-threads Agent Instructions

This repository contains `codex-threads`, a Rust CLI for querying and
controlling Codex app-server threads through named local Unix domain socket
targets.

## What This Repo Is

`codex-threads` is a focused control surface over Codex app-server. It should
delegate thread state, history, settings, and control operations to Codex
app-server instead of parsing local rollout files or maintaining its own thread
index.

## Fast Bootstrap

1. Build: `cargo build`
2. Format: `cargo fmt`
3. Check formatting: `cargo fmt --check`
4. Test: `cargo test`
5. Lint: `cargo clippy --all-targets --all-features`
6. Release build: `cargo build --release`

## Development

- Use `cargo fmt --check`, `cargo test`, and
  `cargo clippy --all-targets --all-features` before handing off substantial
  changes.
- Run `cargo build --release` before release-oriented or packaging changes.
- Update `README.md` for user-facing behavior, config, command, output, or
  workflow changes.
- Keep CLI entrypoints thin; put behavior behind focused library modules.
- Prefer deterministic offline tests for config, target resolution, protocol
  parsing, rendering, and error mapping.
- Keep live Codex smoke tests opt-in and documented under `smoke/`.
- Prefer end-state implementations over transitional ones.
- Do not add backward-compatibility fallbacks, alias fields, bridge routes, or
  dual-shape parsers unless explicitly requested.
- When redesigning config or APIs, remove obsolete shapes instead of silently
  supporting both old and new contracts.

## Layout

- `src/bin/` contains binary entrypoints.
- `src/lib.rs` is the shared library entrypoint.
- Current modules should keep responsibilities focused:
  - `config` for TOML schema, defaults, validation, and server/config resolution.
  - `rpc` for UDS WebSocket connection handling, JSON-RPC correlation, and the
    app-server handshake.
  - `cli` for command-line parsing.
  - `app` for command orchestration, event normalization, and rendering.
  - `tests` for deterministic integration coverage.
- `smoke/` contains the opt-in live smoke harness.
- `skills/` contains packaged assistant guidance for using the CLI from other
  Codex sessions.

## Changelog

Location: `CHANGELOG.md` at the repository root.

### Format

Use these sections under `## [Unreleased]`:

- `### Breaking Changes` - API/config changes requiring migration.
- `### Added` - New features.
- `### Changed` - Changes to existing behavior.
- `### Fixed` - Bug fixes.
- `### Removed` - Removed features.

### Rules

- New entries always go under `## [Unreleased]`.
- Append to existing subsections; do not create duplicate subsection headers.
- Do not edit already released version sections.
- Use inline PR links when a PR exists:
  `([#123](https://github.com/kcosr/codex-threads/pull/123))`.

## Releasing

The first release version is `0.1.0`, matching `Cargo.toml`. Since the crate is
already set to that version, release it with:

```bash
node scripts/release.mjs current
```

For later releases:

```bash
node scripts/release.mjs patch    # Bug fixes, e.g. 0.1.0 -> 0.1.1
node scripts/release.mjs minor    # New features, e.g. 0.1.1 -> 0.2.0
node scripts/release.mjs major    # Breaking changes, e.g. 0.2.0 -> 1.0.0
node scripts/release.mjs 0.2.3    # Explicit version
```

The release script verifies a clean `main` branch, optionally bumps
`Cargo.toml` and `Cargo.lock`, stamps `CHANGELOG.md`, commits and tags the
release, pushes to origin, creates a GitHub prerelease from changelog notes,
then opens a new `## [Unreleased]` section for the next cycle.
