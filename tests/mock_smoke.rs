use std::fs;
use std::os::unix::net::UnixListener as StdUnixListener;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread;

use assert_cmd::Command;
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::net::UnixListener;
use tokio_tungstenite::accept_async;
use tokio_tungstenite::tungstenite::protocol::Message;

struct MockServer {
    _temp: TempDir,
    socket: PathBuf,
    config: PathBuf,
    received: Arc<Mutex<Vec<String>>>,
}

#[derive(Clone, Copy)]
enum TurnNotificationMode {
    Complete,
    None,
    WrongTurnCompleted,
    Failed,
    UnknownStatus,
}

impl MockServer {
    fn start() -> Self {
        Self::start_with_options(TurnNotificationMode::Complete, false)
    }

    fn start_without_turn_notifications() -> Self {
        Self::start_with_options(TurnNotificationMode::None, false)
    }

    fn start_with_malformed_turn_start() -> Self {
        Self::start_with_options(TurnNotificationMode::None, true)
    }

    fn start_with_wrong_turn_completion() -> Self {
        Self::start_with_options(TurnNotificationMode::WrongTurnCompleted, false)
    }

    fn start_with_failed_turn() -> Self {
        Self::start_with_options(TurnNotificationMode::Failed, false)
    }

    fn start_with_unknown_turn_status() -> Self {
        Self::start_with_options(TurnNotificationMode::UnknownStatus, false)
    }

    fn start_with_options(
        turn_notification_mode: TurnNotificationMode,
        malformed_turn_start: bool,
    ) -> Self {
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
        let std_listener = StdUnixListener::bind(&socket).expect("bind mock socket");
        std_listener.set_nonblocking(true).expect("nonblocking");
        let received = Arc::new(Mutex::new(Vec::new()));
        let received_for_thread = Arc::clone(&received);
        thread::spawn(move || {
            let runtime = tokio::runtime::Runtime::new().expect("runtime");
            runtime.block_on(async move {
                let listener = UnixListener::from_std(std_listener).expect("tokio listener");
                loop {
                    let (stream, _) = listener.accept().await.expect("accept");
                    let received = Arc::clone(&received_for_thread);
                    tokio::spawn(async move {
                        handle_connection(
                            stream,
                            received,
                            turn_notification_mode,
                            malformed_turn_start,
                        )
                        .await;
                    });
                }
            });
        });

        Self {
            _temp: temp,
            socket,
            config,
            received,
        }
    }

    fn endpoint(&self) -> String {
        format!("unix://{}", self.socket.display())
    }

    fn command(&self) -> Command {
        let mut command = Command::cargo_bin("codex-threads").expect("binary");
        command
            .env_remove("CODEX_THREADS_CONFIG")
            .env_remove("CODEX_THREADS_SERVER")
            .arg("--config")
            .arg(&self.config);
        command
    }

    fn methods(&self) -> Vec<String> {
        self.received.lock().expect("received").clone()
    }
}

async fn handle_connection(
    stream: tokio::net::UnixStream,
    received: Arc<Mutex<Vec<String>>>,
    turn_notification_mode: TurnNotificationMode,
    malformed_turn_start: bool,
) {
    let mut ws = accept_async(stream).await.expect("websocket accept");
    while let Some(message) = ws.next().await {
        let Ok(Message::Text(text)) = message else {
            continue;
        };
        let value: Value = serde_json::from_str(&text).expect("json request");
        if let Some(method) = value.get("method").and_then(Value::as_str) {
            received.lock().expect("received").push(method.to_string());
            if let Some(id) = value.get("id").cloned() {
                let result = mock_result(method, &value, malformed_turn_start);
                if method == "turn/start" {
                    let thread_id = value["params"]["threadId"].as_str().unwrap_or("thread_1");
                    send_turn_notifications(&mut ws, thread_id, turn_notification_mode).await;
                }
                let response = json!({ "id": id, "result": result });
                if ws
                    .send(Message::Text(response.to_string().into()))
                    .await
                    .is_err()
                {
                    return;
                }
            }
        }
    }
}

async fn send_turn_notifications(
    ws: &mut tokio_tungstenite::WebSocketStream<tokio::net::UnixStream>,
    thread_id: &str,
    mode: TurnNotificationMode,
) {
    let (turn_id, terminal_status, text) = match mode {
        TurnNotificationMode::Complete => ("turn_1", "completed", "done"),
        TurnNotificationMode::WrongTurnCompleted => ("turn_other", "failed", "done"),
        TurnNotificationMode::Failed => ("turn_1", "failed", "failed"),
        TurnNotificationMode::UnknownStatus => ("turn_1", "mystery", "mystery"),
        TurnNotificationMode::None => return,
    };
    let _ = ws
        .send(Message::Text(
            json!({
                "method": "item/agentMessage/delta",
                "params": {
                    "threadId": thread_id,
                    "turnId": "turn_1",
                    "itemId": "item_agent",
                    "delta": text
                }
            })
            .to_string()
            .into(),
        ))
        .await;
    let _ = ws
        .send(Message::Text(
            json!({
                "method": "item/completed",
                "params": {
                    "threadId": thread_id,
                    "turnId": "turn_1",
                    "item": {
                        "id": "item_agent",
                        "type": "agentMessage",
                        "text": text
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
                    "turn": { "id": turn_id, "status": terminal_status, "items": [] }
                }
            })
            .to_string()
            .into(),
        ))
        .await;
}

fn mock_result(method: &str, request: &Value, malformed_turn_start: bool) -> Value {
    match method {
        "initialize" => json!({
            "userAgent": "mock-codex",
            "codexHome": "/tmp/mock-codex",
            "platformFamily": "unix",
            "platformOs": "linux"
        }),
        "thread/list" if request["params"]["cwd"].as_str() == Some("/tmp/paged") => {
            paged_threads(request)
        }
        "thread/list" => page(json!([sample_thread("thread_1")])),
        "thread/search" if request["params"]["searchTerm"].as_str() == Some("paged") => {
            paged_search_results(request)
        }
        "thread/search" => page(json!([{ "thread": sample_thread("thread_1"), "score": 1.0 }])),
        "thread/read" => json!({ "thread": sample_thread(thread_id(request)) }),
        "thread/turns/list" => page(json!([sample_turn()])),
        "thread/start" => json!({
            "thread": sample_thread("thread_new"),
            "model": request["params"]["model"].as_str().unwrap_or("gpt-5.1-codex"),
            "reasoningEffort": request["params"]["config"]["model_reasoning_effort"].as_str().unwrap_or("medium"),
            "serviceTier": request["params"].get("serviceTier").cloned().unwrap_or(Value::Null)
        }),
        "thread/name/set" => json!({}),
        "turn/start" if malformed_turn_start => {
            json!({ "turn": { "status": "inProgress", "items": [] } })
        }
        "turn/start" => json!({ "turn": { "id": "turn_1", "status": "inProgress", "items": [] } }),
        "thread/resume" => json!({
            "threadId": thread_id(request),
            "model": "gpt-5.1-codex",
            "reasoningEffort": "medium",
            "serviceTier": Value::Null,
            "cwd": "/tmp/mock-work"
        }),
        "thread/unsubscribe" => json!({}),
        "thread/settings/update" => json!({}),
        "thread/loaded/list" => page(json!(["thread_1"])),
        "turn/steer" => {
            json!({ "turnId": request["params"]["expectedTurnId"].as_str().unwrap_or("turn_1") })
        }
        "turn/interrupt" => json!({}),
        "thread/archive" => json!({}),
        "thread/unarchive" => json!({ "thread": sample_thread(thread_id(request)) }),
        "model/list" => page(json!([{ "id": "gpt-5.5", "name": "GPT-5.5" }])),
        "thread/goal/get" => json!({ "goal": { "objective": "Finish", "status": "active" } }),
        "thread/goal/set" => json!({
            "goal": {
                "objective": request["params"]["objective"].as_str().unwrap_or("Finish"),
                "status": request["params"]["status"].as_str().unwrap_or("active")
            }
        }),
        "thread/goal/clear" => json!({ "cleared": true }),
        other => panic!("unexpected method {other}"),
    }
}

fn page(data: Value) -> Value {
    json!({ "data": data, "nextCursor": Value::Null, "backwardsCursor": Value::Null })
}

fn paged_threads(request: &Value) -> Value {
    match request["params"]["cursor"].as_str() {
        None => json!({
            "data": [sample_thread_with_updated("thread_old", 1_600_000_000)],
            "nextCursor": "page2",
            "backwardsCursor": Value::Null
        }),
        Some("page2") => json!({
            "data": [
                sample_thread_with_updated("thread_new_1", 1_700_000_100),
                sample_thread_with_updated("thread_new_2", 1_700_000_200)
            ],
            "nextCursor": "page3",
            "backwardsCursor": Value::Null
        }),
        _ => page(json!([])),
    }
}

fn paged_search_results(request: &Value) -> Value {
    let page = paged_threads(request);
    let data = page["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|thread| json!({ "thread": thread, "score": 1.0 }))
        .collect::<Vec<_>>();
    json!({
        "data": data,
        "nextCursor": page["nextCursor"].clone(),
        "backwardsCursor": page["backwardsCursor"].clone()
    })
}

fn thread_id(request: &Value) -> &str {
    request["params"]["threadId"].as_str().unwrap_or("thread_1")
}

fn sample_thread(id: &str) -> Value {
    sample_thread_with_updated(id, 1_700_000_100)
}

fn sample_thread_with_updated(id: &str, updated_at: i64) -> Value {
    json!({
        "id": id,
        "name": "Mock Thread",
        "preview": "Mock preview",
        "cwd": "/tmp/mock-work",
        "status": { "type": "idle" },
        "createdAt": 1_700_000_000_i64,
        "updatedAt": updated_at
    })
}

fn sample_turn() -> Value {
    json!({
        "id": "turn_1",
        "status": "completed",
        "startedAt": 1_700_000_050_i64,
        "completedAt": 1_700_000_060_i64,
        "items": [
            {
                "id": "item_user",
                "type": "userMessage",
                "content": [{ "type": "text", "text": "hello" }]
            },
            {
                "id": "item_agent",
                "type": "agentMessage",
                "text": "done"
            }
        ]
    })
}

fn run_json(server: &MockServer, args: &[&str]) -> Value {
    let output = server
        .command()
        .args(args)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    serde_json::from_slice(&output).expect("json output")
}

#[test]
fn connect_bypasses_config_and_lists_threads() {
    let server = MockServer::start();
    let output = Command::cargo_bin("codex-threads")
        .expect("binary")
        .env_remove("CODEX_THREADS_CONFIG")
        .env_remove("CODEX_THREADS_SERVER")
        .arg("--config")
        .arg(server.config.parent().unwrap().join("missing.toml"))
        .arg("--connect")
        .arg(server.endpoint())
        .args(["list", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let value: Value = serde_json::from_slice(&output).expect("json output");
    assert_eq!(value["server"], server.endpoint());
    assert_eq!(value["threads"][0]["id"], "thread_1");
}

#[test]
fn connect_bypasses_config_for_servers_ping() {
    let server = MockServer::start();
    Command::cargo_bin("codex-threads")
        .expect("binary")
        .env_remove("CODEX_THREADS_CONFIG")
        .env_remove("CODEX_THREADS_SERVER")
        .arg("--config")
        .arg(server.config.parent().unwrap().join("missing.toml"))
        .arg("--connect")
        .arg(server.endpoint())
        .args(["servers", "ping"])
        .assert()
        .success()
        .stdout(predicates::str::contains(format!(
            "{}\tok",
            server.endpoint()
        )));
}

#[test]
fn connect_rejects_servers_ping_all() {
    let server = MockServer::start();
    Command::cargo_bin("codex-threads")
        .expect("binary")
        .env_remove("CODEX_THREADS_CONFIG")
        .env_remove("CODEX_THREADS_SERVER")
        .arg("--connect")
        .arg(server.endpoint())
        .args(["servers", "ping", "--all"])
        .assert()
        .code(2)
        .stderr(predicates::str::contains(
            "--connect cannot be combined with servers ping --all",
        ));
}

#[test]
fn missing_server_is_an_error_when_multiple_servers_are_configured() {
    let temp = TempDir::new().expect("tempdir");
    let config = temp.path().join("config.toml");
    fs::write(
        &config,
        r#"
[servers.one]
type = "uds"
path = "/tmp/one.sock"

[servers.two]
type = "uds"
path = "/tmp/two.sock"
"#,
    )
    .expect("config");

    Command::cargo_bin("codex-threads")
        .expect("binary")
        .env_remove("CODEX_THREADS_CONFIG")
        .env_remove("CODEX_THREADS_SERVER")
        .arg("--config")
        .arg(config)
        .args(["list", "--json"])
        .assert()
        .code(2)
        .stderr(predicates::str::contains("multiple servers configured"));
}

#[test]
fn read_only_commands_return_scriptable_json() {
    let server = MockServer::start();

    assert_eq!(
        run_json(&server, &["servers", "--json"])["servers"][0]["alias"],
        "work"
    );
    assert_eq!(
        run_json(&server, &["servers", "ping", "--server", "work", "--json"])["servers"][0]["ok"],
        true
    );
    assert_eq!(
        run_json(&server, &["list", "--server", "work", "--json"])["threads"][0]["id"],
        "thread_1"
    );
    assert_eq!(
        run_json(&server, &["search", "--server", "work", "--json", "mock"])["results"][0]["thread"]
            ["id"],
        "thread_1"
    );
    assert_eq!(
        run_json(&server, &["show", "--server", "work", "--json", "thread_1"])["turns"]["data"][0]
            ["id"],
        "turn_1"
    );
    assert_eq!(
        run_json(
            &server,
            &["messages", "--server", "work", "--json", "thread_1"]
        )["messages"][1]["role"],
        "assistant"
    );
    assert_eq!(
        run_json(&server, &["status", "--server", "work", "--json"])["loadedThreadIds"][0],
        "thread_1"
    );
    assert_eq!(
        run_json(
            &server,
            &["status", "--server", "work", "--json", "thread_1"]
        )["threadId"],
        "thread_1"
    );
    assert_eq!(
        run_json(&server, &["models", "--server", "work", "--json"])["models"][0]["id"],
        "gpt-5.5"
    );
}

#[test]
fn list_since_filters_locally_across_server_pages() {
    let server = MockServer::start();
    let output = run_json(
        &server,
        &[
            "list",
            "--server",
            "work",
            "--json",
            "--cwd",
            "/tmp/paged",
            "--limit",
            "2",
            "--since",
            "1700000000",
        ],
    );
    assert_eq!(output["threads"].as_array().unwrap().len(), 2);
    assert_eq!(output["threads"][0]["id"], "thread_new_1");
    assert_eq!(output["threads"][1]["id"], "thread_new_2");
    assert_eq!(output["nextCursor"], "page3");
}

#[test]
fn search_since_filters_locally_across_server_pages() {
    let server = MockServer::start();
    let output = run_json(
        &server,
        &[
            "search",
            "--server",
            "work",
            "--json",
            "--limit",
            "2",
            "--since",
            "1700000000",
            "paged",
        ],
    );
    assert_eq!(output["results"].as_array().unwrap().len(), 2);
    assert_eq!(output["results"][0]["thread"]["id"], "thread_new_1");
    assert_eq!(output["results"][1]["thread"]["id"], "thread_new_2");
    assert_eq!(output["nextCursor"], "page3");
}

#[test]
fn new_send_and_settings_commands_return_follow_up_ids() {
    let server = MockServer::start();
    let cwd = server
        .config
        .parent()
        .unwrap()
        .to_string_lossy()
        .to_string();

    let created = run_json(
        &server,
        &[
            "new", "--server", "work", "--cwd", &cwd, "--model", "gpt-5.5", "--effort", "medium",
            "--json",
        ],
    );
    assert_eq!(created["threadId"], "thread_new");

    let completed = run_json(
        &server,
        &[
            "new", "--server", "work", "--cwd", &cwd, "--json", "say done",
        ],
    );
    assert_eq!(completed["threadId"], "thread_new");
    assert_eq!(completed["turnId"], "turn_1");
    assert_eq!(completed["finalAssistantText"], "done");

    let accepted = run_json(
        &server,
        &[
            "send",
            "--server",
            "work",
            "--json",
            "--no-wait",
            "thread_1",
            "continue",
        ],
    );
    assert_eq!(accepted["threadId"], "thread_1");
    assert_eq!(accepted["turnId"], "turn_1");

    let settings = run_json(
        &server,
        &["settings", "show", "--server", "work", "--json", "thread_1"],
    );
    assert_eq!(settings["model"], "gpt-5.1-codex");

    let updated = run_json(
        &server,
        &[
            "settings",
            "set",
            "--server",
            "work",
            "--json",
            "thread_1",
            "--effort",
            "high",
            "--clear-service-tier",
        ],
    );
    assert_eq!(updated["status"], "accepted");
}

#[test]
fn send_streams_ndjson_when_requested() {
    let server = MockServer::start();
    let output = server
        .command()
        .args([
            "send", "--server", "work", "--json", "--stream", "thread_1", "continue",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let lines = String::from_utf8(output).expect("utf8");
    let events = lines
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).expect("ndjson"))
        .collect::<Vec<_>>();
    assert_eq!(events[0]["type"], "accepted");
    assert_eq!(events[1]["delta"], "done");
    assert_eq!(events.last().unwrap()["status"], "completed");
}

#[test]
fn send_human_stream_does_not_duplicate_completed_agent_message() {
    let server = MockServer::start();
    let output = server
        .command()
        .args(["send", "--server", "work", "thread_1", "continue"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let text = String::from_utf8(output).expect("utf8");
    assert_eq!(text.matches("done").count(), 1);
    assert!(text.contains("done\nstatus\tcompleted"));
}

#[test]
fn models_human_output_uses_model_fields() {
    let server = MockServer::start();
    let output = server
        .command()
        .args(["models", "--server", "work"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let text = String::from_utf8(output).expect("utf8");
    assert!(text.contains("gpt-5.5\tGPT-5.5"));
    assert!(!text.starts_with("0\t\t\t"));
}

#[test]
fn send_falls_back_to_polling_when_turn_notifications_are_absent() {
    let server = MockServer::start_without_turn_notifications();
    let completed = run_json(
        &server,
        &["send", "--server", "work", "--json", "thread_1", "continue"],
    );
    assert_eq!(completed["status"], "completed");
    assert_eq!(completed["finalAssistantText"], "done");
    assert_eq!(
        completed["progress"].as_array().unwrap().last().unwrap()["source"],
        "poll"
    );
}

#[test]
fn send_ignores_completion_for_a_different_turn_on_the_same_thread() {
    let server = MockServer::start_with_wrong_turn_completion();
    let completed = run_json(
        &server,
        &["send", "--server", "work", "--json", "thread_1", "continue"],
    );
    assert_eq!(completed["turnId"], "turn_1");
    assert_eq!(completed["status"], "completed");
    assert_eq!(completed["finalAssistantText"], "done");
    assert_eq!(
        completed["progress"].as_array().unwrap().last().unwrap()["source"],
        "poll"
    );
}

#[test]
fn failed_turn_exits_one_and_returns_terminal_json() {
    let server = MockServer::start_with_failed_turn();
    let output = server
        .command()
        .args(["send", "--server", "work", "--json", "thread_1", "continue"])
        .assert()
        .code(1)
        .get_output()
        .stdout
        .clone();
    let failed: Value = serde_json::from_slice(&output).expect("json output");
    assert_eq!(failed["turnId"], "turn_1");
    assert_eq!(failed["status"], "failed");
    assert_eq!(failed["finalAssistantText"], "failed");
}

#[test]
fn unknown_turn_status_notification_is_app_server_error() {
    let server = MockServer::start_with_unknown_turn_status();
    server
        .command()
        .args(["send", "--server", "work", "--json", "thread_1", "continue"])
        .assert()
        .code(3)
        .stderr(predicates::str::contains(
            "app-server returned unrecognized turn status `mystery`",
        ));
}

#[test]
fn malformed_app_server_turn_start_is_exit_code_three() {
    let server = MockServer::start_with_malformed_turn_start();
    server
        .command()
        .args([
            "send",
            "--server",
            "work",
            "--json",
            "--no-wait",
            "thread_1",
            "continue",
        ])
        .assert()
        .code(3)
        .stderr(predicates::str::contains(
            "turn/start response missing turn.id",
        ));
}

#[test]
fn control_and_goal_commands_return_acknowledgements() {
    let server = MockServer::start();

    assert_eq!(
        run_json(
            &server,
            &[
                "steer", "--server", "work", "--json", "thread_1", "turn_1", "adjust"
            ]
        )["status"],
        "accepted"
    );
    assert_eq!(
        run_json(
            &server,
            &[
                "interrupt",
                "--server",
                "work",
                "--json",
                "thread_1",
                "turn_1"
            ]
        )["status"],
        "accepted"
    );
    assert_eq!(
        run_json(
            &server,
            &["name", "--server", "work", "--json", "thread_1", "New name"]
        )["name"],
        "New name"
    );
    assert_eq!(
        run_json(
            &server,
            &["archive", "--server", "work", "--json", "thread_1"]
        )["archived"],
        true
    );
    let unarchived = run_json(
        &server,
        &["unarchive", "--server", "work", "--json", "thread_1"],
    );
    assert_eq!(unarchived["archived"], false);
    assert_eq!(unarchived["thread"]["id"], "thread_1");
    assert_eq!(
        run_json(
            &server,
            &["goal", "get", "--server", "work", "--json", "thread_1"]
        )["goal"]["status"],
        "active"
    );
    assert_eq!(
        run_json(
            &server,
            &[
                "goal",
                "set",
                "--server",
                "work",
                "--json",
                "thread_1",
                "--objective",
                "Ship",
                "--status",
                "active",
                "--token-budget",
                "1000",
            ],
        )["goal"]["objective"],
        "Ship"
    );
    assert_eq!(
        run_json(
            &server,
            &["goal", "clear", "--server", "work", "--json", "thread_1"]
        )["cleared"],
        true
    );

    let methods = server.methods();
    assert!(methods.iter().any(|method| method == "turn/steer"));
    assert!(methods.iter().any(|method| method == "thread/goal/clear"));
}

#[test]
fn invalid_new_prompt_flags_fail_before_connecting() {
    let server = MockServer::start();
    let cwd = server
        .config
        .parent()
        .unwrap()
        .to_string_lossy()
        .to_string();
    server
        .command()
        .args([
            "new",
            "--server",
            "work",
            "--cwd",
            &cwd,
            "--json",
            "--no-wait",
        ])
        .assert()
        .code(2)
        .stderr(predicates::str::contains(
            "new without PROMPT cannot use --no-wait",
        ));
}
