//! Opt-in NDJSON diagnostic logging for app-server traffic.
//!
//! Set `CODEX_THREADS_RPC_LOG=/path/to/rpc.ndjson` to append one line per
//! JSON-RPC frame sent or received, tagged with a millisecond timestamp and a
//! per-connection id, plus producer-side decisions such as attach replay
//! reconciliation. The file is shared by every connection in the process;
//! line order reflects observation order.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

const ENV_VAR: &str = "CODEX_THREADS_RPC_LOG";

static LOG_FILE: OnceLock<Option<Mutex<File>>> = OnceLock::new();
static NEXT_CONNECTION_ID: AtomicU64 = AtomicU64::new(1);

pub fn enabled() -> bool {
    log_file().is_some()
}

pub fn next_connection_id() -> u64 {
    NEXT_CONNECTION_ID.fetch_add(1, Ordering::Relaxed)
}

/// Appends one NDJSON line. `kind` is e.g. "connect", "send", "recv",
/// "attach-seed", or "attach-reconcile"; `connection` is None for entries not
/// tied to one connection.
pub fn log(kind: &str, connection: Option<u64>, payload: Value) {
    let Some(file) = log_file() else {
        return;
    };
    let timestamp_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    let line = json!({
        "tsMs": timestamp_ms,
        "conn": connection,
        "kind": kind,
        "payload": payload,
    });
    if let Ok(mut file) = file.lock() {
        let _ = writeln!(file, "{line}");
    }
}

/// Parses a wire frame for structured logging, falling back to the raw text
/// when it is not valid JSON.
pub fn frame_payload(text: &str) -> Value {
    serde_json::from_str(text).unwrap_or_else(|_| json!(text))
}

fn log_file() -> Option<&'static Mutex<File>> {
    LOG_FILE
        .get_or_init(|| {
            let path = std::env::var(ENV_VAR).ok()?;
            if path.trim().is_empty() {
                return None;
            }
            OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .ok()
                .map(Mutex::new)
        })
        .as_ref()
}
