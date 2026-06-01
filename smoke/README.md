# codex-threads Smoke Tests

Deterministic mock smoke coverage lives in `tests/mock_smoke.rs` and runs as
part of `cargo test`. Those tests launch a mock Codex app-server over a Unix
domain socket and exercise the compiled CLI binary.

This directory contains opt-in live smoke checks against a real Codex
app-server.

## Live Smoke

Use the running app-server socket:

```bash
CODEX_SOCK=unix:///var/run/user/1000/codex.sock smoke/live_smoke.sh
```

The script:

- builds the CLI if needed;
- writes a temporary config with one `live` server;
- runs `servers ping`, `models`, promptless `new`, `status`, `settings show`,
  and `name`;
- uses a disposable working directory;
- avoids model work by default.

To include a real turn:

```bash
RUN_CODEX_TURN=1 \
CODEX_MODEL=gpt-5.5 \
CODEX_EFFORT=medium \
CODEX_SOCK=unix:///var/run/user/1000/codex.sock \
smoke/live_smoke.sh
```

The live turn sends a small prompt to the created thread and waits for the final
JSON response. This requires model access and may incur usage.

Set `RUN_ARCHIVE=1` to include `archive` and `unarchive`. Those commands are
covered by the mock smoke suite by default; the live app-server archive path can
be sensitive to local session-store state.
