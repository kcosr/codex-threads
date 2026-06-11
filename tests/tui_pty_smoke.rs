#![cfg(feature = "tui")]

use std::collections::HashMap;
use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixListener as StdUnixListener;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use assert_cmd::Command;
use futures_util::{SinkExt, StreamExt};
use portable_pty::{Child, CommandBuilder, PtySize, native_pty_system};
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::net::UnixListener;
use tokio_tungstenite::accept_async;
use tokio_tungstenite::tungstenite::protocol::Message;

const PTY_COLS: u16 = 120;
const PTY_ROWS: u16 = 32;
const WAIT_TIMEOUT: Duration = Duration::from_secs(8);

#[derive(Clone)]
struct ThreadRecord {
    id: String,
    name: String,
    preview: String,
    cwd: String,
    status: String,
    updated_at: i64,
    active_turn_id: Option<String>,
    turns: Vec<Value>,
}

struct TuiMockServer {
    _temp: TempDir,
    config: PathBuf,
    received: Arc<Mutex<Vec<Value>>>,
}

struct ServerState {
    threads: HashMap<String, ThreadRecord>,
    order: Vec<String>,
    next_turn: u64,
}

struct StartedMockTurn {
    turn_id: String,
    reply: String,
    completed_previous_turn_id: Option<String>,
    stream_now: bool,
}

struct TuiPty {
    child: Box<dyn Child + Send + Sync>,
    writer: Box<dyn Write + Send>,
    parser: Arc<Mutex<vt100::Parser>>,
    _reader: thread::JoinHandle<()>,
}

impl TuiMockServer {
    fn start() -> Self {
        let temp = TempDir::new().expect("tempdir");
        let socket = temp.path().join("codex.sock");
        let config = temp.path().join("config.toml");
        fs::write(
            &config,
            format!(
                "[servers.work]\ntype = \"uds\"\npath = \"{}\"\n",
                socket.display()
            ),
        )
        .expect("config");

        let state = Arc::new(Mutex::new(ServerState::new()));
        let received = Arc::new(Mutex::new(Vec::new()));
        spawn_mock_listener(socket, state, Arc::clone(&received));

        Self {
            _temp: temp,
            config,
            received,
        }
    }

    fn start_multi() -> Self {
        let temp = TempDir::new().expect("tempdir");
        let main_socket = temp.path().join("main.sock");
        let work_socket = temp.path().join("work.sock");
        let config = temp.path().join("config.toml");
        fs::write(
            &config,
            format!(
                "[servers.main]\ntype = \"uds\"\npath = \"{}\"\n\n[servers.work]\ntype = \"uds\"\npath = \"{}\"\n",
                main_socket.display(),
                work_socket.display()
            ),
        )
        .expect("config");

        let received = Arc::new(Mutex::new(Vec::new()));
        spawn_mock_listener(
            main_socket,
            Arc::new(Mutex::new(ServerState::named("Main"))),
            Arc::clone(&received),
        );
        spawn_mock_listener(
            work_socket,
            Arc::new(Mutex::new(ServerState::named("Work"))),
            Arc::clone(&received),
        );

        Self {
            _temp: temp,
            config,
            received,
        }
    }

    fn command(&self) -> Command {
        let mut command = Command::cargo_bin("codex-threads").expect("binary");
        command
            .env_remove("CODEX_THREADS_CONFIG")
            .env_remove("CODEX_THREADS_SERVER")
            .env_remove("CODEX_THREADS_STATE")
            .env_remove("XDG_STATE_HOME")
            .arg("--config")
            .arg(&self.config);
        command
    }

    fn method_count(&self, method: &str) -> usize {
        self.received
            .lock()
            .expect("received")
            .iter()
            .filter(|request| request["method"].as_str() == Some(method))
            .count()
    }

    fn requests_for(&self, method: &str) -> Vec<Value> {
        self.received
            .lock()
            .expect("received")
            .iter()
            .filter(|request| request["method"].as_str() == Some(method))
            .cloned()
            .collect()
    }

    fn wait_for_method_count(&self, method: &str, min: usize) {
        let started = Instant::now();
        while started.elapsed() < WAIT_TIMEOUT {
            if self.method_count(method) >= min {
                return;
            }
            thread::sleep(Duration::from_millis(25));
        }
        panic!(
            "timed out waiting for {method} count {min}; got {}",
            self.method_count(method)
        );
    }
}

fn spawn_mock_listener(
    socket: PathBuf,
    state: Arc<Mutex<ServerState>>,
    received: Arc<Mutex<Vec<Value>>>,
) {
    let std_listener = StdUnixListener::bind(&socket).expect("bind mock socket");
    std_listener.set_nonblocking(true).expect("nonblocking");
    thread::spawn(move || {
        let runtime = tokio::runtime::Runtime::new().expect("runtime");
        runtime.block_on(async move {
            let listener = UnixListener::from_std(std_listener).expect("tokio listener");
            loop {
                let (stream, _) = listener.accept().await.expect("accept");
                let state = Arc::clone(&state);
                let received = Arc::clone(&received);
                tokio::spawn(async move {
                    let ws = accept_async(stream).await.expect("websocket accept");
                    handle_websocket(ws, state, received).await;
                });
            }
        });
    });
}

impl ServerState {
    fn new() -> Self {
        let active_turn = json!({
            "id": "turn_active",
            "status": "inProgress",
            "startedAt": 1_700_000_090_i64,
            "completedAt": Value::Null,
            "items": [
                {
                    "id": "item_active_user",
                    "type": "userMessage",
                    "content": [{ "type": "text", "text": "watch the active session" }]
                },
                {
                    "id": "item_active_agent",
                    "type": "agentMessage",
                    "text": "attached history before live"
                }
            ]
        });
        let beta_turn = json!({
            "id": "turn_beta_initial",
            "status": "completed",
            "startedAt": 1_700_000_050_i64,
            "completedAt": 1_700_000_060_i64,
            "items": [
                {
                    "id": "item_beta_user",
                    "type": "userMessage",
                    "content": [{ "type": "text", "text": "beta opening prompt" }]
                },
                {
                    "id": "item_beta_agent",
                    "type": "agentMessage",
                    "text": "beta opening response"
                }
            ]
        });
        let long_turns = (1..=12)
            .map(|index| {
                json!({
                    "id": format!("turn_long_{index:02}"),
                    "status": "completed",
                    "startedAt": 1_700_000_000_i64 + index,
                    "completedAt": 1_700_000_010_i64 + index,
                    "items": [
                        {
                            "id": format!("item_long_user_{index:02}"),
                            "type": "userMessage",
                            "content": [{ "type": "text", "text": format!("long prompt {index:02}") }]
                        },
                        {
                            "id": format!("item_long_agent_{index:02}"),
                            "type": "agentMessage",
                            "text": format!("long response {index:02}")
                        }
                    ]
                })
            })
            .collect::<Vec<_>>();
        let mut threads = HashMap::new();
        threads.insert(
            "thread_active".to_string(),
            ThreadRecord {
                id: "thread_active".to_string(),
                name: "Active stream".to_string(),
                preview: "watch the active session".to_string(),
                cwd: "/tmp/tui-active".to_string(),
                status: "active".to_string(),
                updated_at: 1_700_000_200,
                active_turn_id: Some("turn_active".to_string()),
                turns: vec![active_turn],
            },
        );
        threads.insert(
            "thread_beta".to_string(),
            ThreadRecord {
                id: "thread_beta".to_string(),
                name: "Beta task".to_string(),
                preview: "beta opening prompt".to_string(),
                cwd: "/tmp/tui-beta".to_string(),
                status: "idle".to_string(),
                updated_at: 1_700_000_100,
                active_turn_id: None,
                turns: vec![beta_turn],
            },
        );
        threads.insert(
            "thread_long".to_string(),
            ThreadRecord {
                id: "thread_long".to_string(),
                name: "Long history".to_string(),
                preview: "long prompt 12".to_string(),
                cwd: "/tmp/tui-long".to_string(),
                status: "idle".to_string(),
                updated_at: 1_700_000_090,
                active_turn_id: None,
                turns: long_turns,
            },
        );
        Self {
            threads,
            order: vec![
                "thread_active".to_string(),
                "thread_beta".to_string(),
                "thread_long".to_string(),
            ],
            next_turn: 2,
        }
    }

    fn named(prefix: &str) -> Self {
        let mut state = Self::new();
        for thread in state.threads.values_mut() {
            thread.name = format!("{prefix} {}", thread.name);
            thread.cwd = format!("{}/{}", thread.cwd, prefix.to_ascii_lowercase());
        }
        state
    }

    fn thread_json(&self, id: &str) -> Value {
        let thread = self.threads.get(id).expect("thread exists");
        json!({
            "id": thread.id,
            "name": thread.name,
            "preview": thread.preview,
            "cwd": thread.cwd,
            "status": { "type": thread.status },
            "createdAt": 1_700_000_000_i64,
            "updatedAt": thread.updated_at
        })
    }

    fn thread_list(&self) -> Value {
        let threads = self
            .order
            .iter()
            .map(|id| self.thread_json(id))
            .collect::<Vec<_>>();
        page(json!(threads))
    }

    fn turns_page(
        &self,
        thread_id: &str,
        direction: &str,
        cursor: Option<&str>,
        limit: usize,
    ) -> Value {
        let mut ordered = self
            .threads
            .get(thread_id)
            .map(|thread| thread.turns.clone())
            .unwrap_or_default();
        if direction == "desc" {
            ordered.reverse();
        }
        let start = cursor
            .and_then(|cursor| {
                ordered
                    .iter()
                    .position(|turn| turn["id"].as_str() == Some(cursor))
            })
            .map(|index| index + 1)
            .unwrap_or(0);
        let page_turns = ordered
            .iter()
            .skip(start)
            .take(limit)
            .cloned()
            .collect::<Vec<_>>();
        let next_cursor = if start + page_turns.len() < ordered.len() {
            page_turns
                .last()
                .and_then(|turn| turn["id"].as_str())
                .map(str::to_string)
        } else {
            None
        };
        let backwards_cursor = if start > 0 {
            page_turns
                .first()
                .and_then(|turn| turn["id"].as_str())
                .map(str::to_string)
        } else {
            None
        };
        json!({
            "data": page_turns,
            "nextCursor": next_cursor,
            "backwardsCursor": backwards_cursor
        })
    }

    fn status_for(&self, thread_id: &str) -> Value {
        let active_turn_id = self
            .threads
            .get(thread_id)
            .and_then(|thread| thread.active_turn_id.clone());
        json!({
            "thread": self.thread_json(thread_id),
            "activeTurnId": active_turn_id,
            "truncated": false
        })
    }

    fn add_thread(&mut self, cwd: &str) -> String {
        let id = format!("thread_created_{}", self.next_turn);
        self.threads.insert(
            id.clone(),
            ThreadRecord {
                id: id.clone(),
                name: String::new(),
                preview: String::new(),
                cwd: cwd.to_string(),
                status: "idle".to_string(),
                updated_at: 1_700_000_300,
                active_turn_id: None,
                turns: Vec::new(),
            },
        );
        self.order.insert(0, id.clone());
        id
    }

    fn start_turn(&mut self, thread_id: &str, prompt: &str) -> StartedMockTurn {
        let reply = format!("stream reply for {prompt}");
        let thread = self.threads.get_mut(thread_id).expect("thread exists");
        let turn_id = format!("turn_{}", self.next_turn);
        self.next_turn += 1;
        let item_id = format!("item_agent_{}", self.next_turn);
        let completed_previous_turn_id = thread.active_turn_id.clone();
        let stream_now = completed_previous_turn_id.is_none();
        if let Some(previous_turn_id) = &completed_previous_turn_id
            && let Some(previous) = thread
                .turns
                .iter_mut()
                .find(|turn| turn["id"].as_str() == Some(previous_turn_id.as_str()))
        {
            previous["status"] = json!("completed");
            previous["completedAt"] = json!(thread.updated_at + 1);
        }
        thread.status = if stream_now { "idle" } else { "active" }.to_string();
        thread.active_turn_id = if stream_now {
            None
        } else {
            Some(turn_id.clone())
        };
        thread.preview = prompt.to_string();
        thread.updated_at += 1;
        thread.turns.push(json!({
            "id": turn_id,
            "status": if stream_now { "completed" } else { "inProgress" },
            "startedAt": thread.updated_at,
            "completedAt": if stream_now { json!(thread.updated_at + 1) } else { Value::Null },
            "items": [
                {
                    "id": format!("item_user_{}", self.next_turn),
                    "type": "userMessage",
                    "content": [{ "type": "text", "text": prompt }]
                },
                {
                    "id": item_id,
                    "type": "agentMessage",
                    "text": if stream_now { reply.clone() } else { String::new() }
                }
            ]
        }));
        StartedMockTurn {
            turn_id,
            reply,
            completed_previous_turn_id,
            stream_now,
        }
    }

    fn complete_generated_active_turn(&mut self, thread_id: &str) -> Option<(String, String)> {
        let thread = self.threads.get_mut(thread_id)?;
        let active_turn_id = thread.active_turn_id.clone()?;
        if active_turn_id == "turn_active" {
            return None;
        }
        let turn = thread
            .turns
            .iter_mut()
            .find(|turn| turn["id"].as_str() == Some(active_turn_id.as_str()))?;
        if turn["status"].as_str() != Some("inProgress") {
            return None;
        }
        let prompt = turn["items"]
            .as_array()
            .and_then(|items| {
                items
                    .iter()
                    .find(|item| item["type"].as_str() == Some("userMessage"))
            })
            .and_then(|item| item["content"].as_array())
            .and_then(|content| content.first())
            .and_then(|item| item["text"].as_str())
            .unwrap_or("")
            .to_string();
        let reply = format!("stream reply for {prompt}");
        if let Some(agent) = turn["items"].as_array_mut().and_then(|items| {
            items
                .iter_mut()
                .find(|item| item["type"].as_str() == Some("agentMessage"))
        }) {
            agent["text"] = json!(reply.clone());
        }
        turn["status"] = json!("completed");
        turn["completedAt"] = json!(thread.updated_at + 1);
        thread.status = "idle".to_string();
        thread.active_turn_id = None;
        Some((active_turn_id, reply))
    }

    fn apply_live_attach_delta(&mut self, thread_id: &str) -> Option<String> {
        let thread = self.threads.get_mut(thread_id)?;
        let turn = thread.turns.iter_mut().find(|turn| {
            turn["id"].as_str() == thread.active_turn_id.as_deref()
                && turn["status"].as_str() == Some("inProgress")
        })?;
        let items = turn["items"].as_array_mut()?;
        let item = items
            .iter_mut()
            .find(|item| item["id"].as_str() == Some("item_active_agent"))?;
        let text = item["text"].as_str().unwrap_or_default().to_string();
        let next = if text.contains("attached live update") {
            text
        } else {
            format!("{text}\nattached live update")
        };
        item["text"] = json!(next.clone());
        Some(next)
    }
}

async fn handle_websocket<S>(
    mut ws: tokio_tungstenite::WebSocketStream<S>,
    state: Arc<Mutex<ServerState>>,
    received: Arc<Mutex<Vec<Value>>>,
) where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    while let Some(message) = ws.next().await {
        let Ok(Message::Text(text)) = message else {
            continue;
        };
        let request: Value = serde_json::from_str(&text).expect("json request");
        let Some(method) = request["method"].as_str() else {
            continue;
        };
        received.lock().expect("received").push(request.clone());
        let Some(id) = request.get("id").cloned() else {
            continue;
        };

        if method == "turn/start" {
            let thread_id = request["params"]["threadId"]
                .as_str()
                .unwrap_or("thread_beta")
                .to_string();
            let prompt = request["params"]["input"]
                .as_array()
                .and_then(|items| items.first())
                .and_then(|item| item["text"].as_str())
                .unwrap_or("")
                .to_string();
            let started = {
                let mut state = state.lock().expect("state");
                state.start_turn(&thread_id, &prompt)
            };
            let response = json!({
                "id": id,
                "result": {
                    "turn": {
                        "id": started.turn_id,
                        "status": "inProgress",
                        "items": []
                    }
                }
            });
            if ws
                .send(Message::Text(response.to_string().into()))
                .await
                .is_err()
            {
                break;
            }
            if let Some(previous_turn_id) = started.completed_previous_turn_id {
                send_turn_completed(&mut ws, &thread_id, &previous_turn_id).await;
            }
            if started.stream_now {
                send_stream_notifications(&mut ws, &thread_id, &started.turn_id, &started.reply)
                    .await;
            }
            continue;
        }

        if method == "thread/resume" && request["params"]["excludeTurns"].as_bool() == Some(false) {
            let thread_id = thread_id(&request).to_string();
            let generated = {
                let mut state = state.lock().expect("state");
                state.complete_generated_active_turn(&thread_id)
            };
            if let Some((turn_id, reply)) = generated {
                send_stream_notifications(&mut ws, &thread_id, &turn_id, &reply).await;
            } else if {
                let mut state = state.lock().expect("state");
                state.apply_live_attach_delta(&thread_id)
            }
            .is_some()
            {
                send_delta(
                    &mut ws,
                    &thread_id,
                    "turn_active",
                    "item_active_agent",
                    "attached live update",
                )
                .await;
            }
        }

        let result = mock_result(method, &request, &state);
        let response = json!({ "id": id, "result": result });
        if ws
            .send(Message::Text(response.to_string().into()))
            .await
            .is_err()
        {
            break;
        }
    }
}

async fn send_stream_notifications<S>(
    ws: &mut tokio_tungstenite::WebSocketStream<S>,
    thread_id: &str,
    turn_id: &str,
    reply: &str,
) where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let mut parts = reply.splitn(3, ' ');
    let first = parts.next().unwrap_or("");
    let second = parts.next().unwrap_or("");
    let rest = parts.next().unwrap_or("");
    send_delta(
        ws,
        thread_id,
        turn_id,
        "item_agent_stream",
        &format!("{first} "),
    )
    .await;
    send_delta(
        ws,
        thread_id,
        turn_id,
        "item_agent_stream",
        &format!("{second} "),
    )
    .await;
    send_delta(ws, thread_id, turn_id, "item_agent_stream", rest).await;
    let _ = ws
        .send(Message::Text(
            json!({
                "method": "item/completed",
                "params": {
                    "threadId": thread_id,
                    "turnId": turn_id,
                    "item": {
                        "id": "item_agent_stream",
                        "type": "agentMessage",
                        "text": reply
                    }
                }
            })
            .to_string()
            .into(),
        ))
        .await;
    let _ = ws
        .send(Message::Text(
            json!({
                "method": "turn/completed",
                "params": {
                    "threadId": thread_id,
                    "turn": {
                        "id": turn_id,
                        "status": "completed",
                        "items": []
                    }
                }
            })
            .to_string()
            .into(),
        ))
        .await;
}

async fn send_turn_completed<S>(
    ws: &mut tokio_tungstenite::WebSocketStream<S>,
    thread_id: &str,
    turn_id: &str,
) where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let _ = ws
        .send(Message::Text(
            json!({
                "method": "turn/completed",
                "params": {
                    "threadId": thread_id,
                    "turn": {
                        "id": turn_id,
                        "status": "completed",
                        "items": []
                    }
                }
            })
            .to_string()
            .into(),
        ))
        .await;
}

async fn send_delta<S>(
    ws: &mut tokio_tungstenite::WebSocketStream<S>,
    thread_id: &str,
    turn_id: &str,
    item_id: &str,
    delta: &str,
) where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let _ = ws
        .send(Message::Text(
            json!({
                "method": "item/agentMessage/delta",
                "params": {
                    "threadId": thread_id,
                    "turnId": turn_id,
                    "itemId": item_id,
                    "delta": delta
                }
            })
            .to_string()
            .into(),
        ))
        .await;
}

fn mock_result(method: &str, request: &Value, state: &Arc<Mutex<ServerState>>) -> Value {
    let mut state = state.lock().expect("state");
    match method {
        "thread/start" => {
            let cwd = request["params"]["cwd"].as_str().unwrap_or("");
            let id = state.add_thread(cwd);
            json!({
                "thread": state.thread_json(&id),
                "model": "gpt-5.1-codex",
                "reasoningEffort": "medium",
                "serviceTier": Value::Null
            })
        }
        "initialize" => json!({
            "userAgent": "mock-codex",
            "codexHome": "/tmp/mock-codex",
            "platformFamily": "unix",
            "platformOs": "linux"
        }),
        "thread/list" => state.thread_list(),
        "thread/search" => {
            let data = state
                .order
                .iter()
                .map(|id| json!({"thread": state.thread_json(id), "score": 1.0}))
                .collect::<Vec<_>>();
            page(json!(data))
        }
        "thread/read" => json!({"thread": state.thread_json(thread_id(request))}),
        "thread/turns/list" => {
            let direction = request["params"]["sortDirection"]
                .as_str()
                .unwrap_or("desc");
            let cursor = request["params"]["cursor"].as_str();
            let limit = request["params"]["limit"].as_u64().unwrap_or(50) as usize;
            state.turns_page(thread_id(request), direction, cursor, limit)
        }
        "thread/resume" => {
            let thread_id = thread_id(request);
            let status = state.status_for(thread_id);
            json!({
                "threadId": thread_id,
                "thread": status["thread"].clone(),
                "model": "gpt-5.1-codex",
                "reasoningEffort": "medium",
                "serviceTier": Value::Null,
                "cwd": "/tmp/tui-work"
            })
        }
        "thread/unsubscribe" => json!({}),
        "thread/loaded/list" => page(json!(["thread_active"])),
        "turn/start" => json!({"turn": {"id": "turn_2", "status": "inProgress", "items": []}}),
        "turn/steer" => json!({"turnId": request["params"]["expectedTurnId"].clone()}),
        "turn/interrupt" => json!({}),
        "thread/name/set" => {
            let id = thread_id(request).to_string();
            if let Some(name) = request["params"]["name"].as_str()
                && let Some(thread) = state.threads.get_mut(&id)
            {
                thread.name = name.to_string();
            }
            json!({"thread": state.thread_json(&id)})
        }
        "thread/archive" => json!({"thread": state.thread_json(thread_id(request))}),
        "thread/unarchive" => json!({"thread": state.thread_json(thread_id(request))}),
        other => panic!("unexpected method {other}"),
    }
}

fn thread_id(request: &Value) -> &str {
    request["params"]["threadId"]
        .as_str()
        .unwrap_or("thread_beta")
}

fn page(data: Value) -> Value {
    json!({"data": data, "nextCursor": Value::Null, "backwardsCursor": Value::Null})
}

impl TuiPty {
    fn spawn(server: &TuiMockServer, state_dir: &TempDir, stream_log: &PathBuf) -> Self {
        Self::spawn_with_env(server, state_dir, stream_log, &[])
    }

    fn spawn_with_env(
        server: &TuiMockServer,
        state_dir: &TempDir,
        stream_log: &PathBuf,
        extra_env: &[(&str, PathBuf)],
    ) -> Self {
        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows: PTY_ROWS,
                cols: PTY_COLS,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("open pty");
        let binary = assert_cmd::cargo::cargo_bin("codex-threads");
        let mut command = CommandBuilder::new(binary);
        command.env("TERM", "xterm-256color");
        command.env("CODEX_THREADS_STATE", state_dir.path());
        command.env("CODEX_THREADS_TUI_STREAM_LOG", stream_log);
        command.env(
            "CODEX_THREADS_RPC_LOG",
            stream_log.with_extension("rpc.ndjson"),
        );
        for (key, value) in extra_env {
            command.env(key, value);
        }
        command.arg("--config");
        command.arg(&server.config);
        command.arg("tui");
        let child = pair.slave.spawn_command(command).expect("spawn tui");
        drop(pair.slave);
        let mut reader = pair.master.try_clone_reader().expect("pty reader");
        let writer = pair.master.take_writer().expect("pty writer");
        let parser = Arc::new(Mutex::new(vt100::Parser::new(PTY_ROWS, PTY_COLS, 0)));
        let parser_for_thread = Arc::clone(&parser);
        let reader_thread = thread::spawn(move || {
            let mut buffer = [0_u8; 4096];
            loop {
                match reader.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(read) => parser_for_thread
                        .lock()
                        .expect("parser")
                        .process(&buffer[..read]),
                    Err(_) => break,
                }
            }
        });
        Self {
            child,
            writer,
            parser,
            _reader: reader_thread,
        }
    }

    fn screen(&self) -> String {
        self.parser.lock().expect("parser").screen().contents()
    }

    fn write(&mut self, bytes: &[u8]) {
        self.writer.write_all(bytes).expect("write key");
        self.writer.flush().expect("flush key");
    }

    fn type_text(&mut self, text: &str) {
        self.write(text.as_bytes());
    }

    fn wait_for(&self, expected: &str) {
        self.wait_for_all(&[expected]);
    }

    fn wait_for_all(&self, expected: &[&str]) {
        let started = Instant::now();
        while started.elapsed() < WAIT_TIMEOUT {
            let screen = self.screen();
            if expected.iter().all(|text| screen.contains(text)) {
                return;
            }
            thread::sleep(Duration::from_millis(25));
        }
        panic!(
            "timed out waiting for {expected:?}\n--- screen ---\n{}",
            self.screen()
        );
    }

    fn quit(mut self) {
        self.write(b"q");
        let started = Instant::now();
        while started.elapsed() < WAIT_TIMEOUT {
            if self.child.try_wait().expect("try wait").is_some() {
                return;
            }
            thread::sleep(Duration::from_millis(25));
        }
        let _ = self.child.kill();
    }
}

fn run_cli_json(server: &TuiMockServer, state_dir: &TempDir, args: &[&str]) -> Value {
    let output = server
        .command()
        .env("CODEX_THREADS_STATE", state_dir.path())
        .args(args)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    serde_json::from_slice(&output).expect("json output")
}

fn wait_for_file_contains(path: &PathBuf, expected: &str) {
    let started = Instant::now();
    while started.elapsed() < WAIT_TIMEOUT {
        if fs::read_to_string(path)
            .unwrap_or_default()
            .contains(expected)
        {
            return;
        }
        thread::sleep(Duration::from_millis(25));
    }
    panic!(
        "timed out waiting for {path:?} to contain {expected:?}\n--- file ---\n{}",
        fs::read_to_string(path).unwrap_or_default()
    );
}

fn fake_codex_script(temp: &TempDir) -> (PathBuf, PathBuf) {
    let script = temp.path().join("fake-codex");
    let stdin_log = temp.path().join("fake-codex-stdin.log");
    fs::write(
        &script,
        "#!/bin/sh\nprintf 'fake codex ready\\n'\nIFS= read -r line\nprintf '%s\\n' \"$line\" > \"$FAKE_CODEX_STDIN_LOG\"\nprintf 'fake codex exiting\\n'\n",
    )
    .expect("fake codex script");
    let mut permissions = fs::metadata(&script).expect("fake metadata").permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&script, permissions).expect("fake chmod");
    (script, stdin_log)
}

#[test]
#[ignore = "PTY smoke; run with `cargo test --test tui_pty_smoke -- --ignored`"]
fn tui_browser_merges_all_configured_servers_by_default() {
    let server = TuiMockServer::start_multi();
    let state_dir = TempDir::new().expect("state dir");
    let stream_log = state_dir.path().join("stream.ndjson");
    let tui = TuiPty::spawn(&server, &state_dir, &stream_log);

    tui.wait_for_all(&[
        "SERVER",
        "main",
        "work",
        "Main Active stream",
        "Work Active stream",
    ]);
    tui.quit();

    assert!(
        server.method_count("thread/list") >= 2,
        "TUI should fetch browser rows from both configured servers"
    );
}

#[test]
#[ignore = "PTY smoke; run with `cargo test --test tui_pty_smoke -- --ignored`"]
fn tui_codex_launch_hands_pty_input_to_child() {
    let server = TuiMockServer::start();
    let state_dir = TempDir::new().expect("state dir");
    let stream_log = state_dir.path().join("stream.ndjson");
    let fake_dir = TempDir::new().expect("fake dir");
    let (fake_codex, fake_stdin_log) = fake_codex_script(&fake_dir);
    let mut tui = TuiPty::spawn_with_env(
        &server,
        &state_dir,
        &stream_log,
        &[
            ("CODEX_THREADS_CODEX_BIN", fake_codex),
            ("FAKE_CODEX_STDIN_LOG", fake_stdin_log.clone()),
        ],
    );

    tui.wait_for_all(&["Active stream", "Beta task"]);
    tui.write(b"j");
    tui.write(b"o");
    tui.wait_for_all(&["Open In Codex", "thread_beta"]);
    tui.write(b"\r");
    tui.wait_for("fake codex ready");
    tui.write(b"child-input\r");
    wait_for_file_contains(&fake_stdin_log, "child-input");
    tui.wait_for("codex exited");
    tui.quit();
}

#[test]
#[ignore = "PTY smoke; run with `cargo test --test tui_pty_smoke -- --ignored`"]
fn tui_detail_compose_stream_updates_screen_and_cli_history() {
    let server = TuiMockServer::start();
    let state_dir = TempDir::new().expect("state dir");
    let stream_log = state_dir.path().join("stream.ndjson");
    let mut tui = TuiPty::spawn(&server, &state_dir, &stream_log);

    tui.wait_for_all(&["Active stream", "Beta task"]);
    tui.write(b"j");
    tui.write(b"\r");
    tui.wait_for_all(&["Transcript", "beta opening prompt", "beta opening response"]);
    tui.write(b"m");
    tui.wait_for("Compose stream");
    tui.type_text("tui smoke detail");
    tui.write(b"\r");
    tui.wait_for("stream reply for tui smoke detail");
    wait_for_file_contains(&stream_log, "stream reply for tui smoke detail");
    tui.write(b"\x1b");
    tui.wait_for("Threads");
    tui.quit();

    let messages = run_cli_json(&server, &state_dir, &["messages", "--json", "thread_beta"]);
    let rendered = serde_json::to_string(&messages).expect("json");
    assert!(rendered.contains("tui smoke detail"), "{rendered}");
    assert!(
        rendered.contains("stream reply for tui smoke detail"),
        "{rendered}"
    );
    assert!(server.method_count("turn/start") >= 1);
    assert!(server.method_count("thread/turns/list") >= 2);
}

#[test]
#[ignore = "PTY smoke; run with `cargo test --test tui_pty_smoke -- --ignored`"]
fn tui_detail_enter_send_on_initial_active_thread_follows_started_turn() {
    let server = TuiMockServer::start();
    let state_dir = TempDir::new().expect("state dir");
    let stream_log = state_dir.path().join("stream.ndjson");
    let mut tui = TuiPty::spawn(&server, &state_dir, &stream_log);

    tui.wait_for_all(&["Active stream", "Beta task"]);
    tui.write(b"\r");
    tui.wait_for_all(&[
        "Transcript",
        "watch the active session",
        "attached history before live",
    ]);
    tui.write(b"\r");
    tui.wait_for("Steer active turn");
    tui.write(b"\t");
    tui.wait_for("Send new turn");
    tui.type_text("detail active followup");
    tui.write(b"\r");
    tui.wait_for("stream reply for detail active followup");
    wait_for_file_contains(&stream_log, "stream reply for detail active followup");
    tui.quit();

    assert!(server.method_count("turn/start") >= 1);
    assert_eq!(
        server.method_count("turn/steer"),
        0,
        "normal detail send should not use explicit steer"
    );
}

#[test]
#[ignore = "PTY smoke; run with `cargo test --test tui_pty_smoke -- --ignored`"]
fn tui_detail_loads_older_history_above_transcript() {
    let server = TuiMockServer::start();
    let state_dir = TempDir::new().expect("state dir");
    let stream_log = state_dir.path().join("stream.ndjson");
    let mut tui = TuiPty::spawn(&server, &state_dir, &stream_log);

    tui.wait_for_all(&["Active stream", "Beta task", "Long history"]);
    tui.write(b"jj");
    tui.write(b"\r");
    tui.wait_for_all(&["Transcript", "long prompt 12", "long response 12"]);
    let before_older_load = server.method_count("thread/turns/list");
    tui.write(b"gg");
    server.wait_for_method_count("thread/turns/list", before_older_load + 1);
    tui.wait_for_all(&["long prompt 01", "long response 01"]);
    tui.quit();
}

#[test]
#[ignore = "PTY smoke; run with `cargo test --test tui_pty_smoke -- --ignored`"]
fn tui_detail_refresh_reuses_fetch_rpc_client() {
    let server = TuiMockServer::start();
    let state_dir = TempDir::new().expect("state dir");
    let stream_log = state_dir.path().join("stream.ndjson");
    let mut tui = TuiPty::spawn(&server, &state_dir, &stream_log);

    tui.wait_for_all(&["Active stream", "Beta task"]);
    tui.write(b"j");
    tui.write(b"\r");
    tui.wait_for_all(&["Transcript", "beta opening prompt", "beta opening response"]);

    let initializations = server.method_count("initialize");
    let before_refreshes = server.method_count("thread/turns/list");
    tui.write(b"r");
    server.wait_for_method_count("thread/turns/list", before_refreshes + 1);
    tui.write(b"r");
    server.wait_for_method_count("thread/turns/list", before_refreshes + 2);
    tui.quit();

    assert_eq!(
        server.method_count("initialize"),
        initializations,
        "detail refreshes should reuse the fetch worker RPC client"
    );
}

#[test]
#[ignore = "PTY smoke; run with `cargo test --test tui_pty_smoke -- --ignored`"]
fn tui_preview_reuses_preview_rpc_client() {
    let server = TuiMockServer::start();
    let state_dir = TempDir::new().expect("state dir");
    let stream_log = state_dir.path().join("stream.ndjson");
    let mut tui = TuiPty::spawn(&server, &state_dir, &stream_log);

    tui.wait_for_all(&["Active stream", "Beta task", "Long history"]);
    let before_previews = server.method_count("thread/turns/list");
    tui.write(b"j");
    server.wait_for_method_count("thread/turns/list", before_previews + 1);
    let initializations = server.method_count("initialize");
    tui.write(b"j");
    server.wait_for_method_count("thread/turns/list", before_previews + 2);
    tui.write(b"k");
    server.wait_for_method_count("thread/turns/list", before_previews + 3);
    tui.quit();

    assert_eq!(
        server.method_count("initialize"),
        initializations,
        "preview selection changes should reuse the preview worker RPC client"
    );
}

#[test]
#[ignore = "PTY smoke; run with `cargo test --test tui_pty_smoke -- --ignored`"]
fn tui_browser_attach_detaches_when_switching_sessions() {
    let server = TuiMockServer::start();
    let state_dir = TempDir::new().expect("state dir");
    let stream_log = state_dir.path().join("stream.ndjson");
    let mut tui = TuiPty::spawn(&server, &state_dir, &stream_log);

    tui.wait_for_all(&["Active stream", "Beta task"]);
    tui.write(b"T");
    tui.wait_for("attached live update");
    wait_for_file_contains(&stream_log, "attached live update");
    server.wait_for_method_count("thread/resume", 1);
    tui.write(b"j");
    server.wait_for_method_count("thread/unsubscribe", 1);
    tui.write(b"\r");
    tui.wait_for_all(&["Transcript", "Beta task", "beta opening prompt"]);
    tui.write(b"\x1b");
    tui.wait_for("Threads");
    tui.quit();

    let active_status = run_cli_json(&server, &state_dir, &["status", "--json", "thread_active"]);
    assert_eq!(active_status["activeTurnId"], "turn_active");
}

#[test]
#[ignore = "PTY smoke; run with `cargo test --test tui_pty_smoke -- --ignored`"]
fn tui_browser_normal_send_to_active_thread_uses_turn_start() {
    let server = TuiMockServer::start();
    let state_dir = TempDir::new().expect("state dir");
    let stream_log = state_dir.path().join("stream.ndjson");
    let mut tui = TuiPty::spawn(&server, &state_dir, &stream_log);

    tui.wait_for_all(&["Active stream", "Beta task"]);
    tui.write(b"m");
    tui.wait_for("Steer active turn");
    tui.write(b"\t");
    tui.wait_for("Send new turn");
    tui.type_text("browser active followup");
    tui.write(b"\r");
    tui.wait_for("stream reply for browser active followup");
    wait_for_file_contains(&stream_log, "stream reply for browser active followup");
    tui.quit();

    assert!(server.method_count("turn/start") >= 1);
    assert_eq!(
        server.method_count("turn/steer"),
        0,
        "normal browser send should not use explicit steer"
    );
}

#[test]
#[ignore = "PTY smoke; run with `cargo test --test tui_pty_smoke -- --ignored`"]
fn tui_browser_new_session_flow_creates_named_thread_and_streams() {
    let server = TuiMockServer::start();
    let state_dir = TempDir::new().expect("state dir");
    let stream_log = state_dir.path().join("stream.ndjson");
    let mut tui = TuiPty::spawn(&server, &state_dir, &stream_log);

    tui.wait_for_all(&["Active stream", "Beta task"]);
    tui.write(b"n");
    tui.wait_for("New session cwd");
    // The prompt is prefilled with the selected row's cwd; accept it.
    tui.wait_for("/tmp/tui-active");
    tui.write(b"\r");
    tui.wait_for("New session name");
    tui.type_text("Fresh session");
    tui.write(b"\r");
    tui.wait_for("New session first message");
    tui.type_text("hello fresh session");
    tui.write(b"\r");

    server.wait_for_method_count("thread/start", 1);
    server.wait_for_method_count("thread/name/set", 1);
    server.wait_for_method_count("turn/start", 1);
    tui.wait_for("Fresh session");
    tui.wait_for("stream reply for hello fresh session");
    tui.quit();

    let starts = server.requests_for("thread/start");
    assert_eq!(starts.len(), 1);
    assert_eq!(
        starts[0]["params"]["cwd"].as_str(),
        Some("/tmp/tui-active"),
        "cwd should be prefilled from the selected row"
    );
    let names = server.requests_for("thread/name/set");
    assert_eq!(names[0]["params"]["name"].as_str(), Some("Fresh session"));
    let turn_starts = server.requests_for("turn/start");
    assert!(
        turn_starts[0]["params"]["threadId"]
            .as_str()
            .is_some_and(|id| id.starts_with("thread_created_")),
        "first turn should target the created thread"
    );

    let rpc_log = stream_log.with_extension("rpc.ndjson");
    let rpc_lines = fs::read_to_string(&rpc_log).expect("rpc log");
    assert!(
        rpc_lines.contains("\"kind\":\"send\"") && rpc_lines.contains("thread/start"),
        "rpc log should capture the thread/start exchange"
    );
    assert!(
        rpc_lines.contains("\"kind\":\"recv\""),
        "rpc log should capture received frames"
    );
}

#[test]
#[ignore = "PTY smoke; run with `cargo test --test tui_pty_smoke -- --ignored`"]
fn tui_browser_explicit_steer_and_interrupt_use_active_control_rpcs() {
    let server = TuiMockServer::start();
    let state_dir = TempDir::new().expect("state dir");
    let stream_log = state_dir.path().join("stream.ndjson");
    let mut tui = TuiPty::spawn(&server, &state_dir, &stream_log);

    tui.wait_for_all(&["Active stream", "Beta task"]);
    tui.write(b"m");
    tui.wait_for("Steer active turn");
    tui.type_text("browser explicit steer");
    tui.write(b"\r");
    server.wait_for_method_count("turn/steer", 1);
    tui.write(b"i");
    tui.wait_for("Interrupt Turn");
    tui.write(b"\r");
    server.wait_for_method_count("turn/interrupt", 1);
    tui.quit();

    assert_eq!(
        server.method_count("turn/start"),
        0,
        "explicit browser steer should not start a normal turn"
    );
}
