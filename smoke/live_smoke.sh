#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="${BIN:-$ROOT/target/debug/codex-threads}"
CODEX_SOCK="${CODEX_SOCK:-unix:///var/run/user/1000/codex.sock}"
CODEX_MODEL="${CODEX_MODEL:-gpt-5.5}"
CODEX_EFFORT="${CODEX_EFFORT:-high}"
WORKDIR="${WORKDIR:-}"

if [[ "$CODEX_SOCK" != unix://* ]]; then
	echo "CODEX_SOCK must use unix://, got: $CODEX_SOCK" >&2
	exit 2
fi

SOCK_PATH="${CODEX_SOCK#unix://}"
if [[ ! -S "$SOCK_PATH" ]]; then
	echo "Codex socket does not exist or is not a socket: $SOCK_PATH" >&2
	exit 3
fi

if [[ ! -x "$BIN" ]]; then
	cargo build --manifest-path "$ROOT/Cargo.toml"
fi

CONFIG="$(mktemp)"
OWN_WORKDIR=""
if [[ -z "$WORKDIR" ]]; then
	OWN_WORKDIR="$(mktemp -d)"
	WORKDIR="$OWN_WORKDIR"
fi

cleanup() {
	rm -f "$CONFIG"
	if [[ -n "$OWN_WORKDIR" ]]; then
		rm -rf "$OWN_WORKDIR"
	fi
}
trap cleanup EXIT

cat >"$CONFIG" <<EOF
model = "$CODEX_MODEL"
model_reasoning_effort = "$CODEX_EFFORT"

[servers.live]
type = "uds"
path = "$SOCK_PATH"
EOF

run() {
	echo "+ $*" >&2
	"$@"
}

extract_thread_id() {
	node -e 'let s=""; process.stdin.on("data", d => s += d); process.stdin.on("end", () => console.log(JSON.parse(s).threadId));'
}

assert_goal_null() {
	node -e 'let s=""; process.stdin.on("data", d => s += d); process.stdin.on("end", () => { const value = JSON.parse(s); if (value.goal !== null) { console.error(`expected goal null, got ${JSON.stringify(value.goal)}`); process.exit(1); } });'
}

assert_goal_set() {
	node -e 'let s=""; process.stdin.on("data", d => s += d); process.stdin.on("end", () => { const value = JSON.parse(s); const goal = value.goal; if (!goal || goal.objective !== "codex-threads live smoke goal" || goal.status !== "active" || goal.tokenBudget !== 1234) { console.error(`unexpected goal response: ${JSON.stringify(value)}`); process.exit(1); } });'
}

assert_goal_cleared() {
	node -e 'let s=""; process.stdin.on("data", d => s += d); process.stdin.on("end", () => { const value = JSON.parse(s); if (value.cleared !== true) { console.error(`expected cleared true, got ${JSON.stringify(value)}`); process.exit(1); } });'
}

assert_settings() {
	node -e 'const expectedModel = process.argv[1]; const expectedEffort = process.argv[2]; let s=""; process.stdin.on("data", d => s += d); process.stdin.on("end", () => { const value = JSON.parse(s); if (value.model !== expectedModel || value.effort !== expectedEffort) { console.error(`unexpected settings response: ${JSON.stringify(value)}`); process.exit(1); } });' "$1" "$2"
}

run "$BIN" --config "$CONFIG" servers ping --server live --json
run "$BIN" --config "$CONFIG" models --server live --json

THREAD_JSON="$(run "$BIN" --config "$CONFIG" new --server live --cwd "$WORKDIR" --name "codex-threads live smoke" --json)"
echo "$THREAD_JSON"
THREAD_ID="$(printf '%s\n' "$THREAD_JSON" | extract_thread_id)"

run "$BIN" --config "$CONFIG" status --server live --json "$THREAD_ID"
SETTINGS_JSON="$(run "$BIN" --config "$CONFIG" settings show --server live --json "$THREAD_ID")"
echo "$SETTINGS_JSON"
printf '%s\n' "$SETTINGS_JSON" | assert_settings "$CODEX_MODEL" "$CODEX_EFFORT"
run "$BIN" --config "$CONFIG" name --server live --json "$THREAD_ID" "codex-threads live smoke"

GOAL_JSON="$(run "$BIN" --config "$CONFIG" goal get --server live --json "$THREAD_ID")"
echo "$GOAL_JSON"
printf '%s\n' "$GOAL_JSON" | assert_goal_null

GOAL_JSON="$(run "$BIN" --config "$CONFIG" goal set --server live --json "$THREAD_ID" --objective "codex-threads live smoke goal" --status active --token-budget 1234)"
echo "$GOAL_JSON"
printf '%s\n' "$GOAL_JSON" | assert_goal_set

GOAL_JSON="$(run "$BIN" --config "$CONFIG" goal get --server live --json "$THREAD_ID")"
echo "$GOAL_JSON"
printf '%s\n' "$GOAL_JSON" | assert_goal_set

GOAL_JSON="$(run "$BIN" --config "$CONFIG" goal clear --server live --json "$THREAD_ID")"
echo "$GOAL_JSON"
printf '%s\n' "$GOAL_JSON" | assert_goal_cleared

GOAL_JSON="$(run "$BIN" --config "$CONFIG" goal get --server live --json "$THREAD_ID")"
echo "$GOAL_JSON"
printf '%s\n' "$GOAL_JSON" | assert_goal_null

if [[ "${RUN_CODEX_TURN:-0}" == "1" ]]; then
	run "$BIN" \
		--config "$CONFIG" \
		send \
		--server live \
		--model "$CODEX_MODEL" \
		--effort "$CODEX_EFFORT" \
		--json \
		"$THREAD_ID" \
		"Reply with exactly: codex-threads smoke ok"
fi

if [[ "${RUN_ARCHIVE:-0}" == "1" ]]; then
	run "$BIN" --config "$CONFIG" archive --server live --json "$THREAD_ID"
	if ! run "$BIN" --config "$CONFIG" unarchive --server live --json "$THREAD_ID"; then
		echo "warning: live unarchive failed; checking final thread status" >&2
		run "$BIN" --config "$CONFIG" status --server live --json "$THREAD_ID" || true
		exit 3
	fi
fi
