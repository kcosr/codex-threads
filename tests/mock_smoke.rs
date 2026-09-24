use std::collections::HashMap;
use std::fs;
use std::net::TcpListener as StdTcpListener;
use std::os::unix::net::UnixListener as StdUnixListener;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread;

use assert_cmd::Command;
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::net::TcpListener;
use tokio::net::UnixListener;
use tokio_tungstenite::accept_async;
use tokio_tungstenite::accept_hdr_async;
use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};
use tokio_tungstenite::tungstenite::protocol::Message;

struct MockServer {
    _temp: TempDir,
    socket: PathBuf,
    config: PathBuf,
    received: Arc<Mutex<Vec<Value>>>,
}

struct TcpMockServer {
    _temp: TempDir,
    endpoint: String,
    config: PathBuf,
}

#[derive(Clone)]
struct GoalState {
    objective: String,
    status: String,
    token_budget: i64,
}

impl TcpMockServer {
    fn start(auth_token: Option<&'static str>) -> Self {
        let temp = TempDir::new().expect("tempdir");
        let config = temp.path().join("config.toml");
        let std_listener = StdTcpListener::bind("127.0.0.1:0").expect("bind mock tcp socket");
        let addr = std_listener.local_addr().expect("local addr");
        std_listener.set_nonblocking(true).expect("nonblocking");
        let endpoint = format!("ws://{addr}");
        fs::write(
            &config,
            match auth_token {
                Some(token) => format!(
                    "[servers.work]\nendpoint = \"{}\"\nauth_token = \"{}\"\n",
                    endpoint, token
                ),
                None => format!("[servers.work]\nendpoint = \"{}\"\n", endpoint),
            },
        )
        .expect("config");
        let received = Arc::new(Mutex::new(Vec::new()));
        let received_for_thread = Arc::clone(&received);
        thread::spawn(move || {
            let runtime = tokio::runtime::Runtime::new().expect("runtime");
            runtime.block_on(async move {
                let listener = TcpListener::from_std(std_listener).expect("tokio listener");
                loop {
                    let (stream, _) = listener.accept().await.expect("accept");
                    let received = Arc::clone(&received_for_thread);
                    tokio::spawn(async move {
                        let expected_auth = auth_token.map(|token| format!("Bearer {token}"));
                        #[allow(clippy::result_large_err)]
                        let websocket = accept_hdr_async(
                            stream,
                            move |request: &Request, response: Response| {
                                let actual = request
                                    .headers()
                                    .get("authorization")
                                    .and_then(|value| value.to_str().ok())
                                    .map(ToString::to_string);
                                assert_eq!(actual, expected_auth);
                                Ok(response)
                            },
                        )
                        .await
                        .expect("websocket accept");
                        handle_websocket(
                            websocket,
                            received,
                            TurnNotificationMode::Complete,
                            false,
                            RejectFirst::none(),
                            Arc::new(Mutex::new(false)),
                            Arc::new(Mutex::new(HashMap::new())),
                        )
                        .await;
                    });
                }
            });
        });

        Self {
            _temp: temp,
            endpoint,
            config,
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
}

impl Default for GoalState {
    fn default() -> Self {
        Self {
            objective: "Finish".to_string(),
            status: "active".to_string(),
            token_budget: 1234,
        }
    }
}

#[derive(Clone, Copy)]
enum TurnNotificationMode {
    Complete,
    None,
    WrongTurnCompleted,
    Failed,
    UnknownStatus,
}

#[derive(Clone, Copy)]
enum RejectFirstMethod {
    None,
    TurnStart,
    TurnSteer,
    SettingsUpdate,
}

#[derive(Clone, Copy)]
struct RejectFirst {
    method: RejectFirstMethod,
    code: i64,
    message: Option<&'static str>,
    fail_usage_refresh_after_redemption: bool,
}

impl RejectFirst {
    const fn none() -> Self {
        Self {
            method: RejectFirstMethod::None,
            code: -32600,
            message: None,
            fail_usage_refresh_after_redemption: false,
        }
    }

    const fn method(method: RejectFirstMethod) -> Self {
        Self {
            method,
            code: -32600,
            message: None,
            fail_usage_refresh_after_redemption: false,
        }
    }

    const fn method_with_error(
        method: RejectFirstMethod,
        code: i64,
        message: &'static str,
    ) -> Self {
        Self {
            method,
            code,
            message: Some(message),
            fail_usage_refresh_after_redemption: false,
        }
    }

    const fn with_usage_refresh_failure() -> Self {
        Self {
            method: RejectFirstMethod::None,
            code: -32600,
            message: None,
            fail_usage_refresh_after_redemption: true,
        }
    }
}

impl MockServer {
    fn start() -> Self {
        Self::start_with_options(TurnNotificationMode::Complete, false, RejectFirst::none())
    }

    fn start_with_usage_refresh_failure() -> Self {
        Self::start_with_options(
            TurnNotificationMode::Complete,
            false,
            RejectFirst::with_usage_refresh_failure(),
        )
    }

    fn start_without_turn_notifications() -> Self {
        Self::start_with_options(TurnNotificationMode::None, false, RejectFirst::none())
    }

    fn start_with_malformed_turn_start() -> Self {
        Self::start_with_options(TurnNotificationMode::None, true, RejectFirst::none())
    }

    fn start_requiring_resume_for_send() -> Self {
        Self::start_with_options(
            TurnNotificationMode::None,
            false,
            RejectFirst::method(RejectFirstMethod::TurnStart),
        )
    }

    fn start_requiring_resume_for_steer() -> Self {
        Self::start_with_options(
            TurnNotificationMode::Complete,
            false,
            RejectFirst::method(RejectFirstMethod::TurnSteer),
        )
    }

    fn start_requiring_resume_for_settings_set() -> Self {
        Self::start_with_options(
            TurnNotificationMode::Complete,
            false,
            RejectFirst::method(RejectFirstMethod::SettingsUpdate),
        )
    }

    fn start_rejecting_turn_start_with(code: i64, message: &'static str) -> Self {
        Self::start_with_options(
            TurnNotificationMode::None,
            false,
            RejectFirst::method_with_error(RejectFirstMethod::TurnStart, code, message),
        )
    }

    fn start_with_wrong_turn_completion() -> Self {
        Self::start_with_options(
            TurnNotificationMode::WrongTurnCompleted,
            false,
            RejectFirst::none(),
        )
    }

    fn start_with_failed_turn() -> Self {
        Self::start_with_options(TurnNotificationMode::Failed, false, RejectFirst::none())
    }

    fn start_with_unknown_turn_status() -> Self {
        Self::start_with_options(
            TurnNotificationMode::UnknownStatus,
            false,
            RejectFirst::none(),
        )
    }

    fn start_with_options(
        turn_notification_mode: TurnNotificationMode,
        malformed_turn_start: bool,
        reject_first: RejectFirst,
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
        let goal_state = Arc::new(Mutex::new(HashMap::new()));
        let goal_state_for_thread = Arc::clone(&goal_state);
        let rejected_first_method = Arc::new(Mutex::new(false));
        let rejected_first_method_for_thread = Arc::clone(&rejected_first_method);
        thread::spawn(move || {
            let runtime = tokio::runtime::Runtime::new().expect("runtime");
            runtime.block_on(async move {
                let listener = UnixListener::from_std(std_listener).expect("tokio listener");
                loop {
                    let (stream, _) = listener.accept().await.expect("accept");
                    let received = Arc::clone(&received_for_thread);
                    let rejected_first_method = Arc::clone(&rejected_first_method_for_thread);
                    let goal_state = Arc::clone(&goal_state_for_thread);
                    tokio::spawn(async move {
                        handle_connection(
                            stream,
                            received,
                            turn_notification_mode,
                            malformed_turn_start,
                            reject_first,
                            rejected_first_method,
                            goal_state,
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
            .env_remove("CODEX_THREADS_STATE")
            .env_remove("XDG_STATE_HOME")
            .arg("--config")
            .arg(&self.config);
        command
    }

    fn allow_rate_limit_reset(&self) {
        let config = fs::read_to_string(&self.config).expect("read config");
        fs::write(
            &self.config,
            format!("{config}\nallow_rate_limit_reset = true\n"),
        )
        .expect("write config");
    }

    fn methods(&self) -> Vec<String> {
        self.received
            .lock()
            .expect("received")
            .iter()
            .filter_map(|request| request["method"].as_str().map(ToString::to_string))
            .collect()
    }

    fn params_for(&self, method: &str) -> Vec<Value> {
        self.received
            .lock()
            .expect("received")
            .iter()
            .filter(|request| request["method"].as_str() == Some(method))
            .map(|request| request["params"].clone())
            .collect()
    }
}

async fn handle_connection(
    stream: tokio::net::UnixStream,
    received: Arc<Mutex<Vec<Value>>>,
    turn_notification_mode: TurnNotificationMode,
    malformed_turn_start: bool,
    reject_first: RejectFirst,
    rejected_first_method: Arc<Mutex<bool>>,
    goal_state: Arc<Mutex<HashMap<String, GoalState>>>,
) {
    let ws = accept_async(stream).await.expect("websocket accept");
    handle_websocket(
        ws,
        received,
        turn_notification_mode,
        malformed_turn_start,
        reject_first,
        rejected_first_method,
        goal_state,
    )
    .await;
}

async fn handle_websocket<S>(
    mut ws: tokio_tungstenite::WebSocketStream<S>,
    received: Arc<Mutex<Vec<Value>>>,
    turn_notification_mode: TurnNotificationMode,
    malformed_turn_start: bool,
    reject_first: RejectFirst,
    rejected_first_method: Arc<Mutex<bool>>,
    goal_state: Arc<Mutex<HashMap<String, GoalState>>>,
) where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    while let Some(message) = ws.next().await {
        let Ok(Message::Text(text)) = message else {
            continue;
        };
        let value: Value = serde_json::from_str(&text).expect("json request");
        if let Some(method) = value.get("method").and_then(Value::as_str) {
            received.lock().expect("received").push(value.clone());
            if let Some(id) = value.get("id").cloned() {
                if method == "thread/read" && thread_id(&value) == "thread_missing" {
                    let response = json!({
                        "id": id,
                        "error": {
                            "code": -32600,
                            "message": "thread not found: thread_missing"
                        }
                    });
                    if ws
                        .send(Message::Text(response.to_string().into()))
                        .await
                        .is_err()
                    {
                        break;
                    }
                    continue;
                }
                if method == "thread/read" && thread_id(&value) == "thread_error" {
                    let response = json!({
                        "id": id,
                        "error": {
                            "code": -32603,
                            "message": "temporary read failure"
                        }
                    });
                    if ws
                        .send(Message::Text(response.to_string().into()))
                        .await
                        .is_err()
                    {
                        break;
                    }
                    continue;
                }
                if should_reject_first_method(method, reject_first.method, &rejected_first_method) {
                    let message = reject_first
                        .message
                        .map(ToString::to_string)
                        .unwrap_or_else(|| format!("thread not found: {}", thread_id(&value)));
                    let response = json!({
                        "id": id,
                        "error": {
                            "code": reject_first.code,
                            "message": message
                        }
                    });
                    if ws
                        .send(Message::Text(response.to_string().into()))
                        .await
                        .is_err()
                    {
                        return;
                    }
                    continue;
                }
                if reject_first.fail_usage_refresh_after_redemption
                    && method == "account/rateLimits/read"
                    && received.lock().expect("received").iter().any(|request| {
                        request["method"].as_str() == Some("account/rateLimitResetCredit/consume")
                    })
                {
                    let response = json!({
                        "id": id,
                        "error": {
                            "code": -32603,
                            "message": "usage refresh unavailable"
                        }
                    });
                    if ws
                        .send(Message::Text(response.to_string().into()))
                        .await
                        .is_err()
                    {
                        return;
                    }
                    continue;
                }
                let result = mock_result(method, &value, malformed_turn_start, &goal_state);
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

fn should_reject_first_method(
    method: &str,
    reject_first_method: RejectFirstMethod,
    rejected_first_method: &Arc<Mutex<bool>>,
) -> bool {
    let expected = match reject_first_method {
        RejectFirstMethod::None => return false,
        RejectFirstMethod::TurnStart => "turn/start",
        RejectFirstMethod::TurnSteer => "turn/steer",
        RejectFirstMethod::SettingsUpdate => "thread/settings/update",
    };
    if method != expected {
        return false;
    }
    let mut rejected = rejected_first_method.lock().expect("rejected first method");
    if *rejected {
        return false;
    }
    *rejected = true;
    true
}

async fn send_turn_notifications(
    ws: &mut tokio_tungstenite::WebSocketStream<
        impl tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
    >,
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

fn mock_result(
    method: &str,
    request: &Value,
    malformed_turn_start: bool,
    goal_state: &Arc<Mutex<HashMap<String, GoalState>>>,
) -> Value {
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
        "thread/list" if request["params"]["cwd"].as_str() == Some("/tmp/sorted") => {
            sorted_desc_threads(request)
        }
        "thread/list" if request["params"]["cwd"].as_str() == Some("/tmp/multiline") => {
            page(json!([sample_multiline_preview_thread("thread_multiline")]))
        }
        "thread/list" if request["params"]["parentThreadId"].is_string() => {
            page(json!([sample_thread_with_parent(
                "thread_child_1",
                request["params"]["parentThreadId"]
                    .as_str()
                    .unwrap_or("thread_parent")
            )]))
        }
        "thread/list" if request["params"]["ancestorThreadId"].is_string() => page(json!([
            sample_thread_with_parent("thread_grandchild_1", "thread_child_1"),
            sample_thread_with_parent(
                "thread_child_1",
                request["params"]["ancestorThreadId"]
                    .as_str()
                    .unwrap_or("thread_parent")
            )
        ])),
        "thread/list" if request["params"]["sectionId"].as_str() == Some("section_work") => {
            page(json!([sample_section_thread("thread_section")]))
        }
        "thread/list" if request["params"].get("sectionId") == Some(&Value::Null) => {
            page(json!([sample_thread("thread_unsectioned")]))
        }
        "thread/list" => page(json!([sample_thread("thread_1")])),
        "thread/search" if request["params"]["searchTerm"].as_str() == Some("paged") => {
            paged_search_results(request)
        }
        "thread/searchOccurrences" => json!({
            "data": [{"turnId": "turn_1", "itemId": "item_agent", "snippet": "fixture complete", "snippetMatchRange": {"start": 0, "end": 7}, "turnCursor": "turn_cursor"}],
            "nextCursor": "occurrence_cursor"
        }),
        "thread/search" => page(json!([{ "thread": sample_thread("thread_1"), "score": 1.0 }])),
        "thread/read" => {
            let mut thread = sample_thread(thread_id(request));
            if thread_id(request) == "thread_read_only" {
                thread["canAcceptDirectInput"] = json!(false);
            }
            json!({ "thread": thread })
        }
        "thread/turns/list" => page(json!([sample_turn()])),
        "thread/start" => json!({
            "thread": sample_thread("thread_new"),
            "model": request["params"]["model"].as_str().unwrap_or("gpt-5.1-codex"),
            "reasoningEffort": request["params"]["config"]["model_reasoning_effort"].as_str().unwrap_or("medium"),
            "serviceTier": request["params"].get("serviceTier").cloned().unwrap_or(Value::Null)
        }),
        "thread/fork" => json!({
            "thread": sample_forked_thread(
                "thread_fork",
                request["params"]["threadId"].as_str().unwrap_or("thread_1")
            ),
            "model": request["params"]["model"].as_str().unwrap_or("gpt-5.1-codex"),
            "reasoningEffort": request["params"]["config"]["model_reasoning_effort"].as_str().unwrap_or("medium"),
            "serviceTier": request["params"].get("serviceTier").cloned().unwrap_or(Value::Null)
        }),
        "thread/name/set" => json!({}),
        "threadSection/list" => {
            json!({"data": [{"id": "section_work", "name": "Work", "appearance": null}], "nextCursor": null})
        }
        "threadSection/create" => {
            json!({"section": {"id": "section_new", "name": request["params"]["name"], "appearance": null}})
        }
        "threadSection/update" => {
            json!({"section": {"id": request["params"]["sectionId"], "name": request["params"]["name"], "appearance": null}})
        }
        "threadSection/delete" | "thread/section/move" => json!({}),
        "turn/start" if malformed_turn_start => {
            json!({ "turn": { "status": "inProgress", "items": [] } })
        }
        "turn/start" => json!({ "turn": { "id": "turn_1", "status": "inProgress", "items": [] } }),
        "thread/resume" => {
            let mut thread = sample_thread(thread_id(request));
            if thread_id(request) == "thread_denied_after_resume" {
                thread["canAcceptDirectInput"] = json!(false);
            }
            json!({
                "thread": thread,
                "threadId": thread_id(request),
                "model": "gpt-5.1-codex",
                "reasoningEffort": "medium",
                "serviceTier": Value::Null,
                "cwd": "/tmp/mock-work"
            })
        }
        "thread/unsubscribe" => json!({}),
        "thread/settings/update" => json!({}),
        "thread/loaded/list" => page(json!(["thread_1"])),
        "turn/steer" => {
            json!({ "turnId": request["params"]["expectedTurnId"].as_str().unwrap_or("turn_1") })
        }
        "turn/interrupt" => json!({}),
        "thread/archive" => json!({}),
        "thread/unarchive" => json!({ "thread": sample_thread(thread_id(request)) }),
        "thread/delete" => json!({}),
        "model/list" => page(json!([{ "id": "gpt-5.5", "name": "GPT-5.5" }])),
        "account/rateLimits/read" => sample_usage(),
        "account/rateLimitResetCredit/consume" => json!({ "outcome": "reset" }),
        "thread/goal/get" => {
            json!({ "goal": goal_to_value(&goal_for_thread(request, goal_state)) })
        }
        "thread/goal/set" => json!({
            "goal": goal_to_value(&set_goal_for_thread(request, goal_state))
        }),
        "thread/goal/clear" => {
            if let Some(thread_id) = request["params"]["threadId"].as_str() {
                goal_state.lock().expect("goal state").remove(thread_id);
            }
            json!({ "cleared": true })
        }
        other => panic!("unexpected method {other}"),
    }
}

fn goal_for_thread(
    request: &Value,
    goal_state: &Arc<Mutex<HashMap<String, GoalState>>>,
) -> GoalState {
    let thread_id = request["params"]["threadId"].as_str().unwrap_or("thread_1");
    let mut goals = goal_state.lock().expect("goal state");
    goals.entry(thread_id.to_string()).or_default().clone()
}

fn set_goal_for_thread(
    request: &Value,
    goal_state: &Arc<Mutex<HashMap<String, GoalState>>>,
) -> GoalState {
    let thread_id = request["params"]["threadId"].as_str().unwrap_or("thread_1");
    let mut goals = goal_state.lock().expect("goal state");
    let goal = goals.entry(thread_id.to_string()).or_default();
    if let Some(objective) = request["params"]["objective"].as_str() {
        goal.objective = objective.to_string();
    }
    if let Some(status) = request["params"]["status"].as_str() {
        goal.status = status.to_string();
    }
    if let Some(token_budget) = request["params"]["tokenBudget"].as_i64() {
        goal.token_budget = token_budget;
    }
    goal.clone()
}

fn goal_to_value(goal: &GoalState) -> Value {
    json!({
        "objective": goal.objective,
        "status": goal.status,
        "tokenBudget": goal.token_budget,
    })
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

// Genuinely descending-by-updatedAt pages. `spage2` opens with a thread older
// than the test cutoff, so a sort-aware `--since` scan should stop there and
// never request `spage3` (whose "tripwire" thread is newer than the cutoff and
// would wrongly appear if paging continued past the boundary).
fn sorted_desc_threads(request: &Value) -> Value {
    match request["params"]["cursor"].as_str() {
        None => json!({
            "data": [
                sample_thread_with_updated("thread_s1", 1_700_000_300),
                sample_thread_with_updated("thread_s2", 1_700_000_200)
            ],
            "nextCursor": "spage2",
            "backwardsCursor": Value::Null
        }),
        Some("spage2") => json!({
            "data": [sample_thread_with_updated("thread_s_old", 1_600_000_000)],
            "nextCursor": "spage3",
            "backwardsCursor": Value::Null
        }),
        Some("spage3") => json!({
            "data": [sample_thread_with_updated("thread_s_tripwire", 1_700_000_999)],
            "nextCursor": Value::Null,
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

fn sample_usage() -> Value {
    json!({
        "rateLimits": {
            "limitId": "codex",
            "limitName": "Codex",
            "primary": {
                "usedPercent": 37,
                "windowDurationMins": 300,
                "resetsAt": 1700000000
            },
            "secondary": {
                "usedPercent": 12,
                "windowDurationMins": 10080,
                "resetsAt": 1700600000
            },
            "credits": {
                "hasCredits": true,
                "unlimited": false,
                "balance": "42.50"
            },
            "planType": "pro",
            "rateLimitReachedType": null
        },
        "rateLimitResetCredits": {
            "availableCount": 2,
            "credits": [
                {
                    "id": "credit_later",
                    "status": "available",
                    "grantedAt": 100,
                    "expiresAt": 1_800_000_000,
                    "title": "Later reset"
                },
                {
                    "id": "credit_soonest",
                    "status": "available",
                    "grantedAt": 200,
                    "expiresAt": 1_700_000_000,
                    "title": "Soonest reset"
                }
            ]
        },
        "rateLimitsByLimitId": {
            "codex": {
                "limitId": "codex",
                "limitName": "Codex",
                "primary": {
                    "usedPercent": 37,
                    "windowDurationMins": 300,
                    "resetsAt": 1700000000
                },
                "secondary": {
                    "usedPercent": 12,
                    "windowDurationMins": 10080,
                    "resetsAt": 1700600000
                },
                "credits": {
                    "hasCredits": true,
                    "unlimited": false,
                    "balance": "42.50"
                },
                "planType": "pro",
                "rateLimitReachedType": null
            },
            "priority": {
                "limitId": "priority",
                "limitName": "Priority",
                "primary": {
                    "usedPercent": 65,
                    "windowDurationMins": 1440,
                    "resetsAt": 1700100000
                },
                "secondary": null,
                "credits": null,
                "planType": "pro",
                "rateLimitReachedType": "rate_limit_reached"
            }
        }
    })
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
        "updatedAt": updated_at,
        "experimentalThreadField": {
            "retained": true
        }
    })
}

fn sample_thread_with_parent(id: &str, parent_id: &str) -> Value {
    let mut thread = sample_thread(id);
    thread["parentThreadId"] = json!(parent_id);
    thread
}

fn sample_section_thread(id: &str) -> Value {
    let mut thread = sample_thread(id);
    thread["section"] = json!({"id": "section_work", "name": "Work", "appearance": null});
    thread
}

fn sample_forked_thread(id: &str, source_id: &str) -> Value {
    let mut thread = sample_thread(id);
    thread["forkedFromId"] = json!(source_id);
    thread
}

fn sample_multiline_preview_thread(id: &str) -> Value {
    json!({
        "id": id,
        "name": Value::Null,
        "preview": "First line of a very long preview\nsecond line\twith a tab and enough text to force truncation because this should not spill across terminal rows",
        "cwd": "/tmp/mock-work",
        "status": { "type": "notLoaded" },
        "createdAt": 1_700_000_000_i64,
        "updatedAt": 1_700_000_100_i64
    })
}

fn sample_turn() -> Value {
    json!({
        "id": "turn_1",
        "status": "completed",
        "startedAt": 1_700_000_050_i64,
        "completedAt": 1_700_000_060_i64,
        "experimentalTurnField": "retained",
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

fn run_json_with_state(server: &MockServer, state: &TempDir, args: &[&str]) -> Value {
    let output = server
        .command()
        .env("CODEX_THREADS_STATE", state.path())
        .args(args)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    serde_json::from_slice(&output).expect("json output")
}

fn run_ndjson(server: &MockServer, args: &[&str]) -> Vec<Value> {
    let output = server
        .command()
        .args(args)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    String::from_utf8(output)
        .expect("utf8")
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).expect("ndjson"))
        .collect()
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', r#"'\''"#))
}

fn toml_string(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', r"\\").replace('"', "\\\""))
}

fn write_config(server: &MockServer, contents: impl AsRef<str>) {
    fs::write(&server.config, contents.as_ref()).expect("config");
}

fn assert_thread_yolo_params(params: &Value) {
    assert_eq!(params["approvalPolicy"], "never");
    assert_eq!(params["sandbox"], "danger-full-access");
}

fn assert_turn_yolo_params(params: &Value) {
    assert_eq!(params["approvalPolicy"], "never");
    assert_eq!(params["sandboxPolicy"], json!({"type": "dangerFullAccess"}));
}

fn assert_no_yolo_params(params: &Value) {
    assert!(params.get("approvalPolicy").is_none());
    assert!(params.get("sandbox").is_none());
    assert!(params.get("sandboxPolicy").is_none());
}

#[test]
fn symlink_socket_supports_configured_and_direct_connections() {
    let server = MockServer::start();
    let alias = server.config.parent().unwrap().join("alias.sock");
    std::os::unix::fs::symlink(&server.socket, &alias).expect("socket symlink");
    let endpoint = format!("unix://{}", alias.display());
    write_config(
        &server,
        format!("[servers.work]\nendpoint = {}\n", toml_string(&endpoint)),
    );

    let configured = run_json(&server, &["list", "--json"]);
    assert_eq!(configured["server"], "work");
    assert_eq!(configured["threads"][0]["id"], "thread_1");

    let direct = run_json(&server, &["--connect", &endpoint, "list", "--json"]);
    assert_eq!(direct["server"], endpoint);
    assert_eq!(direct["threads"][0]["id"], "thread_1");
    assert_eq!(server.params_for("initialize").len(), 2);
    assert_eq!(server.params_for("thread/list").len(), 2);
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
        .stdout(predicates::str::contains("SERVER"))
        .stdout(predicates::str::contains("STATUS"))
        .stdout(predicates::str::contains(server.endpoint()))
        .stdout(predicates::str::contains("ok"));
}

#[test]
fn connect_ws_bypasses_config_and_lists_threads() {
    let server = TcpMockServer::start(None);
    let output = Command::cargo_bin("codex-threads")
        .expect("binary")
        .env_remove("CODEX_THREADS_CONFIG")
        .env_remove("CODEX_THREADS_SERVER")
        .arg("--config")
        .arg(server.config.parent().unwrap().join("missing.toml"))
        .arg("--connect")
        .arg(&server.endpoint)
        .args(["list", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let value: Value = serde_json::from_slice(&output).expect("json output");
    assert_eq!(value["server"], server.endpoint);
    assert_eq!(value["threads"][0]["id"], "thread_1");
}

#[test]
fn configured_ws_server_lists_threads() {
    let server = TcpMockServer::start(None);
    let value = server
        .command()
        .args(["list", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let value: Value = serde_json::from_slice(&value).expect("json output");
    assert_eq!(value["server"], "work");
    assert_eq!(value["threads"][0]["id"], "thread_1");
}

#[test]
fn configured_ws_server_sends_literal_auth_token() {
    let server = TcpMockServer::start(Some("secret-token"));
    server
        .command()
        .args(["models", "--json"])
        .assert()
        .success();
}

#[test]
fn servers_listing_does_not_resolve_auth_token_env() {
    let server = MockServer::start();
    write_config(
        &server,
        format!(
            r#"[servers.local]
endpoint = "{}"

[servers.remote]
endpoint = "ws://127.0.0.1:9"
auth_token_env = "CODEX_THREADS_MISSING_TOKEN"
"#,
            server.endpoint()
        ),
    );

    let output = server
        .command()
        .env_remove("CODEX_THREADS_MISSING_TOKEN")
        .args(["servers", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let value: Value = serde_json::from_slice(&output).expect("json output");
    assert_eq!(value["servers"].as_array().unwrap().len(), 2);
    assert_eq!(value["servers"][0]["alias"], "local");
    assert_eq!(value["servers"][1]["alias"], "remote");
    assert_eq!(value["servers"][1]["endpoint"], "ws://127.0.0.1:9/");
}

#[test]
fn servers_ping_all_reports_unresolved_auth_token_env_per_server() {
    let server = MockServer::start();
    write_config(
        &server,
        format!(
            r#"[servers.local]
endpoint = "{}"

[servers.remote]
endpoint = "ws://127.0.0.1:9"
auth_token_env = "CODEX_THREADS_MISSING_TOKEN"
"#,
            server.endpoint()
        ),
    );

    let output = server
        .command()
        .env_remove("CODEX_THREADS_MISSING_TOKEN")
        .args(["servers", "ping", "--all", "--json"])
        .assert()
        .code(3)
        .get_output()
        .stdout
        .clone();
    let value: Value = serde_json::from_slice(&output).expect("json output");
    assert_eq!(value["servers"][0], json!({"server": "local", "ok": true}));
    assert_eq!(
        value["servers"][1],
        json!({"server": "remote", "ok": false})
    );
}

#[test]
fn connect_ws_sends_literal_auth_token() {
    let server = TcpMockServer::start(Some("direct-token"));
    Command::cargo_bin("codex-threads")
        .expect("binary")
        .env_remove("CODEX_THREADS_CONFIG")
        .env_remove("CODEX_THREADS_SERVER")
        .arg("--connect")
        .arg(&server.endpoint)
        .arg("--connect-auth-token")
        .arg("direct-token")
        .args(["models", "--json"])
        .assert()
        .success();
}

#[test]
fn connect_ws_sends_env_auth_token() {
    let server = TcpMockServer::start(Some("env-token"));
    Command::cargo_bin("codex-threads")
        .expect("binary")
        .env_remove("CODEX_THREADS_CONFIG")
        .env_remove("CODEX_THREADS_SERVER")
        .env("CODEX_THREADS_TEST_TOKEN", "env-token")
        .arg("--connect")
        .arg(&server.endpoint)
        .arg("--connect-auth-token-env")
        .arg("CODEX_THREADS_TEST_TOKEN")
        .args(["models", "--json"])
        .assert()
        .success();
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
fn connect_auth_flags_require_websocket_endpoint() {
    Command::cargo_bin("codex-threads")
        .expect("binary")
        .env_remove("CODEX_THREADS_CONFIG")
        .env_remove("CODEX_THREADS_SERVER")
        .arg("--connect")
        .arg("unix:///tmp/missing.sock")
        .arg("--connect-auth-token")
        .arg("secret")
        .args(["models"])
        .assert()
        .code(2)
        .stderr(predicates::str::contains(
            "auth token requires a websocket endpoint",
        ));
}

#[test]
fn connect_auth_flags_reject_non_loopback_plain_ws() {
    Command::cargo_bin("codex-threads")
        .expect("binary")
        .env_remove("CODEX_THREADS_CONFIG")
        .env_remove("CODEX_THREADS_SERVER")
        .arg("--connect")
        .arg("ws://example.com:8765")
        .arg("--connect-auth-token")
        .arg("secret")
        .args(["models"])
        .assert()
        .code(2)
        .stderr(predicates::str::contains("wss:// or loopback ws://"));
}

#[test]
fn connect_auth_flags_are_mutually_exclusive() {
    Command::cargo_bin("codex-threads")
        .expect("binary")
        .env_remove("CODEX_THREADS_CONFIG")
        .env_remove("CODEX_THREADS_SERVER")
        .arg("--connect")
        .arg("ws://127.0.0.1:8765")
        .arg("--connect-auth-token")
        .arg("secret")
        .arg("--connect-auth-token-env")
        .arg("CODEX_THREADS_TEST_TOKEN")
        .args(["models"])
        .assert()
        .code(2)
        .stderr(predicates::str::contains("cannot be used with"));
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
fn completion_commands_print_setup_scripts_and_candidates() {
    let temp = TempDir::new().expect("tempdir");
    let config = temp.path().join("config.toml");
    fs::write(
        &config,
        r#"
[servers.work]
type = "uds"
path = "/tmp/work.sock"

[servers.personal]
type = "uds"
path = "/tmp/personal.sock"
"#,
    )
    .expect("config");

    Command::cargo_bin("codex-threads")
        .expect("binary")
        .env("SHELL", "/bin/bash")
        .args(["completion"])
        .assert()
        .success()
        .stdout(predicates::str::contains("Detected shell: bash"))
        .stdout(predicates::str::contains(
            "source <(codex-threads completion script bash)",
        ));

    Command::cargo_bin("codex-threads")
        .expect("binary")
        .args(["completion", "script", "bash"])
        .assert()
        .success()
        .stdout(predicates::str::contains("mapfile -t COMPREPLY"))
        .stdout(predicates::str::contains(
            "complete -o bashdefault -o default -F _codex_threads_completion codex-threads",
        ))
        .stdout(predicates::str::contains(
            "codex-threads __complete -- \"$cur\"",
        ));

    Command::cargo_bin("codex-threads")
        .expect("binary")
        .args(["completion", "script", "zsh"])
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "compdef _codex_threads codex-threads",
        ))
        .stdout(predicates::str::contains("_files"))
        .stdout(predicates::str::contains(
            "codex-threads __complete -- \"$current\"",
        ));

    Command::cargo_bin("codex-threads")
        .expect("binary")
        .args(["completion", "script", "fish"])
        .assert()
        .success()
        .stdout(predicates::str::contains("complete -c codex-threads -a"))
        .stdout(predicates::str::contains(
            "codex-threads __complete -- \"$current\"",
        ));

    Command::cargo_bin("codex-threads")
        .expect("binary")
        .args(["__complete", "--", "l"])
        .assert()
        .success()
        .stdout(predicates::str::contains("list\n"));

    Command::cargo_bin("codex-threads")
        .expect("binary")
        .args(["__complete", "--", "p", "servers"])
        .assert()
        .success()
        .stdout("ping\n");

    Command::cargo_bin("codex-threads")
        .expect("binary")
        .args(["__complete", "--", "--so", "list"])
        .assert()
        .success()
        .stdout("--source\n--sort\n");

    Command::cargo_bin("codex-threads")
        .expect("binary")
        .args(["__complete", "--", "u", "list", "--sort"])
        .assert()
        .success()
        .stdout("updated\n");

    Command::cargo_bin("codex-threads")
        .expect("binary")
        .args([
            "__complete",
            "--",
            "wo",
            "--config",
            config.to_str().expect("utf8 path"),
            "list",
            "--server",
        ])
        .assert()
        .success()
        .stdout("work\n");

    let bash_completion = |words: &[&str], cword: usize| -> String {
        let binary = assert_cmd::cargo::cargo_bin("codex-threads");
        let binary_dir = binary.parent().expect("binary parent");
        let path = std::env::var_os("PATH").unwrap_or_default();
        let path = std::env::join_paths(
            std::iter::once(binary_dir.to_path_buf()).chain(std::env::split_paths(&path)),
        )
        .expect("join path");
        let words = words
            .iter()
            .map(|word| shell_quote(word))
            .collect::<Vec<_>>()
            .join(" ");
        let script = format!(
            "source <(codex-threads completion script bash); \
             COMP_WORDS=({words}); \
             COMP_CWORD={cword}; \
             _codex_threads_completion; \
             printf '%s\\n' \"${{COMPREPLY[@]}}\""
        );
        let output = std::process::Command::new("bash")
            .args(["--noprofile", "--norc", "-c", &script])
            .env("PATH", path)
            .env_remove("CODEX_THREADS_CONFIG")
            .env_remove("CODEX_THREADS_SERVER")
            .output()
            .expect("run bash completion smoke");
        assert!(
            output.status.success(),
            "bash completion failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).expect("utf8 stdout")
    };

    assert_eq!(bash_completion(&["codex-threads", "l"], 1), "list\n");
    assert_eq!(
        bash_completion(&["codex-threads", "servers", "p"], 2),
        "ping\n"
    );
    assert_eq!(
        bash_completion(&["codex-threads", "list", "--so"], 2),
        "--source\n--sort\n"
    );
    assert_eq!(
        bash_completion(&["codex-threads", "list", "--sort", "u"], 3),
        "updated\n"
    );
    assert_eq!(
        bash_completion(&["codex-threads", "list", "--sort=u"], 2),
        "--sort=updated\n"
    );
    assert_eq!(
        bash_completion(
            &[
                "codex-threads",
                "--config",
                config.to_str().expect("utf8 path"),
                "list",
                "--server",
                "wo",
            ],
            5,
        ),
        "work\n"
    );
    assert!(!bash_completion(&["codex-threads", ""], 1).contains("__complete"));

    let marker = temp.path().join("completion-pwned");
    let malicious_alias = format!("$(touch {})", marker.display());
    fs::write(
        &config,
        format!(
            r#"
[servers.work]
type = "uds"
path = "/tmp/work.sock"

[servers.{malicious_alias}]
type = "uds"
path = "/tmp/malicious.sock"
"#,
            malicious_alias = toml_string(&malicious_alias),
        ),
    )
    .expect("config");

    assert_eq!(
        bash_completion(
            &[
                "codex-threads",
                "--config",
                config.to_str().expect("utf8 path"),
                "list",
                "--server",
                "$",
            ],
            5,
        ),
        format!("{malicious_alias}\n")
    );
    assert!(
        !marker.exists(),
        "completion candidate executed as shell code"
    );
}

#[test]
fn clap_value_parsers_reject_empty_static_values_before_connecting() {
    Command::cargo_bin("codex-threads")
        .expect("binary")
        .args(["new", "--cwd", ".", "--effort", " "])
        .assert()
        .code(2)
        .stderr(predicates::str::contains(
            "reasoning effort cannot be empty",
        ));

    Command::cargo_bin("codex-threads")
        .expect("binary")
        .args(["goal", "set", "thread_1", "--status", "finished"])
        .assert()
        .code(2)
        .stderr(predicates::str::contains("invalid value"));
}

#[test]
fn read_only_commands_return_scriptable_json() {
    let server = MockServer::start();

    assert_eq!(
        run_json(&server, &["servers", "--json"])["servers"][0]["alias"],
        "work"
    );
    assert_eq!(
        run_json(&server, &["servers", "--json"])["servers"][0]["endpoint"],
        server.endpoint()
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
        run_json(
            &server,
            &["search", "threads", "--server", "work", "--json", "mock"]
        )["results"][0]["thread"]["id"],
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
    let user_messages = run_json(
        &server,
        &[
            "messages", "--server", "work", "--json", "--role", "user", "thread_1",
        ],
    );
    assert_eq!(user_messages["messages"].as_array().unwrap().len(), 1);
    assert_eq!(user_messages["messages"][0]["role"], "user");
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
    assert!(
        !server
            .methods()
            .iter()
            .any(|method| method == "thread/resume"),
        "plain status should not resume/load threads"
    );
    assert_eq!(
        run_json(&server, &["models", "--server", "work", "--json"])["models"][0]["id"],
        "gpt-5.5"
    );
    let usage = run_json(&server, &["usage", "--server", "work", "--json"]);
    assert_eq!(usage["server"], "work");
    assert_eq!(usage["rateLimits"]["credits"]["balance"], "42.50");
    assert_eq!(usage["rateLimitResetCredits"]["availableCount"], 2);
    assert_eq!(
        usage["rateLimitsByLimitId"]["priority"]["rateLimitReachedType"],
        "rate_limit_reached"
    );
}

#[test]
fn annotation_commands_manage_local_state_without_app_server() {
    let server = MockServer::start();
    let state = TempDir::new().expect("state");

    let set = run_json_with_state(
        &server,
        &state,
        &[
            "annotate",
            "set",
            "--server",
            "work",
            "--json",
            "thread_1",
            "Release follow-up",
        ],
    );
    assert_eq!(set["server"], "work");
    assert_eq!(set["threadId"], "thread_1");
    assert_eq!(set["annotation"]["text"], "Release follow-up");
    assert!(server.methods().is_empty());

    let get = run_json_with_state(
        &server,
        &state,
        &["annotate", "get", "--server", "work", "--json", "thread_1"],
    );
    assert_eq!(get["annotation"]["text"], "Release follow-up");

    let listed = run_json_with_state(
        &server,
        &state,
        &["annotate", "list", "--server", "work", "--json"],
    );
    assert_eq!(listed["annotations"][0]["threadId"], "thread_1");

    let searched = run_json_with_state(
        &server,
        &state,
        &[
            "annotate", "search", "--server", "work", "--json", "release",
        ],
    );
    assert_eq!(
        searched["annotations"][0]["annotation"]["text"],
        "Release follow-up"
    );

    server
        .command()
        .env("CODEX_THREADS_STATE", state.path())
        .args(["annotate", "get", "--server", "work", "--json", "missing"])
        .assert()
        .code(2)
        .stderr(predicates::str::contains("annotation not found"));

    let cleared = run_json_with_state(
        &server,
        &state,
        &[
            "annotate", "clear", "--server", "work", "--json", "thread_1",
        ],
    );
    assert_eq!(cleared["cleared"], true);
    assert!(
        run_json_with_state(
            &server,
            &state,
            &["annotate", "list", "--server", "work", "--json"]
        )["annotations"]
            .as_array()
            .unwrap()
            .is_empty()
    );
}

#[test]
fn annotations_project_into_list_search_and_show_outputs() {
    let server = MockServer::start();
    let state = TempDir::new().expect("state");
    run_json_with_state(
        &server,
        &state,
        &[
            "annotate",
            "set",
            "--server",
            "work",
            "--json",
            "thread_1",
            "Release follow-up",
        ],
    );

    let listed = run_json_with_state(&server, &state, &["list", "--server", "work", "--json"]);
    assert_eq!(
        listed["threads"][0]["annotation"]["text"],
        "Release follow-up"
    );

    let searched = run_json_with_state(
        &server,
        &state,
        &["search", "threads", "--server", "work", "--json", "mock"],
    );
    assert_eq!(
        searched["results"][0]["thread"]["annotation"]["text"],
        "Release follow-up"
    );

    let shown = run_json_with_state(
        &server,
        &state,
        &["show", "--server", "work", "--json", "thread_1"],
    );
    assert_eq!(shown["thread"]["annotation"]["text"], "Release follow-up");

    let output = server
        .command()
        .env("CODEX_THREADS_STATE", state.path())
        .args(["list", "--server", "work"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let text = String::from_utf8(output).expect("utf8");
    assert!(text.lines().next().unwrap().contains("ANNOTATION"));
    assert!(text.contains("Release follow-up"));
}

#[test]
fn annotation_prune_removes_only_missing_threads() {
    let server = MockServer::start();
    let state = TempDir::new().expect("state");
    run_json_with_state(
        &server,
        &state,
        &[
            "annotate", "set", "--server", "work", "--json", "thread_1", "Keep",
        ],
    );
    run_json_with_state(
        &server,
        &state,
        &[
            "annotate",
            "set",
            "--server",
            "work",
            "--json",
            "thread_missing",
            "Remove",
        ],
    );

    let dry_run = run_json_with_state(
        &server,
        &state,
        &[
            "annotate",
            "prune",
            "--server",
            "work",
            "--dry-run",
            "--json",
        ],
    );
    assert_eq!(dry_run["checked"], 2);
    assert_eq!(dry_run["stale"], json!(["thread_missing"]));
    assert_eq!(dry_run["removed"], 0);
    assert_eq!(
        run_json_with_state(
            &server,
            &state,
            &[
                "annotate",
                "get",
                "--server",
                "work",
                "--json",
                "thread_missing"
            ]
        )["annotation"]["text"],
        "Remove"
    );

    let pruned = run_json_with_state(
        &server,
        &state,
        &["annotate", "prune", "--server", "work", "--json"],
    );
    assert_eq!(pruned["removed"], 1);
    server
        .command()
        .env("CODEX_THREADS_STATE", state.path())
        .args([
            "annotate",
            "get",
            "--server",
            "work",
            "--json",
            "thread_missing",
        ])
        .assert()
        .code(2);
    assert_eq!(
        run_json_with_state(
            &server,
            &state,
            &["annotate", "get", "--server", "work", "--json", "thread_1"]
        )["annotation"]["text"],
        "Keep"
    );
}

#[test]
fn annotation_prune_aborts_on_unexpected_thread_read_error() {
    let server = MockServer::start();
    let state = TempDir::new().expect("state");
    run_json_with_state(
        &server,
        &state,
        &[
            "annotate",
            "set",
            "--server",
            "work",
            "--json",
            "thread_error",
            "Keep despite transient error",
        ],
    );

    server
        .command()
        .env("CODEX_THREADS_STATE", state.path())
        .args(["annotate", "prune", "--server", "work", "--json"])
        .assert()
        .code(3)
        .stderr(predicates::str::contains("temporary read failure"));

    assert_eq!(
        run_json_with_state(
            &server,
            &state,
            &[
                "annotate",
                "get",
                "--server",
                "work",
                "--json",
                "thread_error"
            ]
        )["annotation"]["text"],
        "Keep despite transient error"
    );
}

#[test]
fn status_load_requires_thread_id() {
    let server = MockServer::start();
    server
        .command()
        .args(["status", "--load"])
        .assert()
        .code(2)
        .stderr(predicates::str::contains("<THREAD_ID>"));

    assert!(server.methods().is_empty());
}

#[test]
fn status_load_resumes_then_reports_thread_status() {
    let server = MockServer::start();
    let status = run_json(
        &server,
        &["status", "--server", "work", "--json", "--load", "thread_1"],
    );

    assert_eq!(status["threadId"], "thread_1");

    let methods = server.methods();
    let status_methods = methods
        .iter()
        .filter(|method| {
            matches!(
                method.as_str(),
                "thread/resume" | "thread/unsubscribe" | "thread/read" | "thread/turns/list"
            )
        })
        .map(String::as_str)
        .collect::<Vec<_>>();
    assert_eq!(
        status_methods,
        [
            "thread/resume",
            "thread/unsubscribe",
            "thread/read",
            "thread/turns/list"
        ]
    );

    let resume_params = server.params_for("thread/resume");
    assert_eq!(resume_params.len(), 1);
    assert_eq!(resume_params[0]["threadId"], "thread_1");
    assert_eq!(resume_params[0]["excludeTurns"], true);
    assert_no_yolo_params(&resume_params[0]);
}

#[test]
fn messages_human_output_uses_readable_blocks() {
    let server = MockServer::start();
    let output = server
        .command()
        .args(["messages", "--server", "work", "thread_1"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let text = String::from_utf8(output).expect("utf8");
    assert!(text.contains(" user\nhello"));
    assert!(text.contains("\n\n"));
    assert!(text.contains(" assistant\ndone"));
}

#[test]
fn messages_role_filter_omits_redundant_role_in_human_output() {
    let server = MockServer::start();
    let output = server
        .command()
        .args(["messages", "--server", "work", "--role", "user", "thread_1"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let text = String::from_utf8(output).expect("utf8");
    assert!(text.contains("\nhello\n"));
    assert!(!text.contains(" user\n"));
    assert!(!text.contains("assistant"));
    assert!(!text.contains("done"));
}

#[test]
fn usage_human_output_shows_credits_and_limit_windows() {
    let server = MockServer::start();
    let output = server
        .command()
        .args(["usage", "--server", "work"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let text = String::from_utf8(output).expect("utf8");
    assert!(text.contains("server        work"));
    assert!(text.contains("plan          pro"));
    assert!(
        text.lines()
            .any(|line| { line.split_whitespace().collect::<Vec<_>>() == ["credits", "42.50"] })
    );
    assert!(
        text.lines()
            .any(|line| { line.split_whitespace().collect::<Vec<_>>() == ["resetCredits", "2"] })
    );
    assert!(text.contains("LIMIT"));
    assert!(text.contains("WINDOW"));
    assert!(text.contains("REACHED"));
    assert!(text.contains("Codex"));
    assert!(text.contains("primary"));
    assert!(text.contains("37%"));
    assert!(text.contains("300 mins"));
    assert!(text.contains("Priority"));
    assert!(text.contains("65%"));
    assert!(text.contains("rate_limit_reached"));
}

#[test]
fn usage_redeem_requires_server_permission_without_disclosing_how_to_enable_it() {
    let server = MockServer::start();
    let output = server
        .command()
        .args(["usage", "redeem", "--server", "work"])
        .assert()
        .code(2)
        .get_output()
        .clone();
    let stderr = String::from_utf8(output.stderr).expect("utf8");

    assert!(stderr.contains("rate-limit reset redemption is not permitted"));
    assert!(!stderr.contains("config"));
    assert!(server.methods().is_empty());
}

#[test]
fn usage_redeem_selects_and_redeems_the_soonest_expiring_credit() {
    let server = MockServer::start();
    server.allow_rate_limit_reset();

    let output = run_json(&server, &["usage", "redeem", "--server", "work", "--json"]);
    assert_eq!(output["outcome"], "reset");
    assert_eq!(output["credit"]["id"], "credit_soonest");
    assert_eq!(output["credit"]["title"], "Soonest reset");

    let params = server.params_for("account/rateLimitResetCredit/consume");
    assert_eq!(params.len(), 1);
    assert_eq!(params[0]["creditId"], "credit_soonest");
    assert!(
        params[0]["idempotencyKey"]
            .as_str()
            .is_some_and(|key| key.starts_with("codex-threads-"))
    );
}

#[test]
fn usage_redeem_reports_success_when_the_usage_refresh_fails() {
    let server = MockServer::start_with_usage_refresh_failure();
    server.allow_rate_limit_reset();

    let output = run_json(&server, &["usage", "redeem", "--server", "work", "--json"]);
    assert_eq!(output["outcome"], "reset");
    assert_eq!(output["credit"]["id"], "credit_soonest");
    assert_eq!(output["rateLimits"], Value::Null);
    assert!(
        output["refreshError"]
            .as_str()
            .is_some_and(|error| error.contains("usage refresh unavailable"))
    );
    assert_eq!(
        server
            .params_for("account/rateLimitResetCredit/consume")
            .len(),
        1
    );
}

#[test]
fn list_human_output_uses_compact_aligned_table() {
    let server = MockServer::start();
    let output = server
        .command()
        .args(["list", "--server", "work", "--cwd", "/tmp/multiline"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let text = String::from_utf8(output).expect("utf8");
    let lines = text.lines().collect::<Vec<_>>();
    assert_eq!(lines.len(), 2);
    assert!(lines[0].contains("UPDATED"));
    assert!(lines[0].contains("STATUS"));
    assert!(lines[0].contains("TITLE/PREVIEW"));
    assert!(lines[0].contains("THREAD ID"));
    assert!(lines[1].contains("2023-"));
    assert!(!lines[1].contains("1700000100"));
    assert!(lines[1].contains("First line of a very long preview second line with a ..."));
    assert!(lines[1].contains("..."));
    assert!(lines[1].contains("thread_multiline"));
    assert!(!lines[1].contains('\t'));
}

#[test]
fn messages_help_explains_scan_and_filter_order() {
    let output = Command::cargo_bin("codex-threads")
        .expect("binary")
        .args(["messages", "--help"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let text = String::from_utf8(output).expect("utf8");
    assert!(text.contains("Message selection order"));
    assert!(text.contains("--max-turns is the recent turn scan window"));
    assert!(text.contains("Use --last for the final number of messages"));
    assert!(text.contains("Role filters only see messages inside the scanned turns"));
    assert!(text.contains("There is no messages --first"));
}

#[test]
fn tui_help_exposes_interactive_filter_flags() {
    let output = Command::cargo_bin("codex-threads")
        .expect("binary")
        .args(["tui", "--help"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let text = String::from_utf8(output).expect("utf8");
    assert!(text.contains("--query"));
    assert!(text.contains("--since"));
    assert!(text.contains("--cwd"));
    assert!(text.contains("--provider"));
    assert!(text.contains("--source"));
    assert!(text.contains("--sort"));
    assert!(!text.contains("--json"));
}

#[test]
fn tui_requires_interactive_terminal_before_connecting() {
    Command::cargo_bin("codex-threads")
        .expect("binary")
        .args(["--connect", "unix:///tmp/missing.sock", "tui"])
        .assert()
        .code(2)
        .stderr(predicates::str::contains(
            "tui requires an interactive terminal",
        ));
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
fn list_since_stops_paging_at_boundary_when_sorted_updated_desc() {
    let server = MockServer::start();
    let output = run_json(
        &server,
        &[
            "list",
            "--server",
            "work",
            "--json",
            "--cwd",
            "/tmp/sorted",
            "--limit",
            "10",
            "--since",
            "1700000000",
            "--sort",
            "updated",
            "--desc",
        ],
    );
    let threads = output["threads"].as_array().unwrap();
    // Stops at the first thread older than `since`; never pages to `spage3`,
    // so the newer-but-later "tripwire" thread must not appear.
    assert_eq!(threads.len(), 2);
    assert_eq!(threads[0]["id"], "thread_s1");
    assert_eq!(threads[1]["id"], "thread_s2");
    assert!(
        !threads
            .iter()
            .any(|thread| thread["id"] == "thread_s_tripwire"),
        "early-exit should not reach the tripwire page"
    );
    assert_eq!(output["nextCursor"], "spage3");
}

#[test]
fn search_since_filters_locally_across_server_pages() {
    let server = MockServer::start();
    let output = run_json(
        &server,
        &[
            "search",
            "threads",
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
fn message_occurrence_search_requires_thread_and_query() {
    let server = MockServer::start();
    server
        .command()
        .args(["search", "messages", "thread_1"])
        .assert()
        .failure()
        .code(2)
        .stderr(predicates::str::contains("<QUERY>"));
    assert!(server.params_for("thread/searchOccurrences").is_empty());
}

#[test]
fn list_can_filter_direct_children_and_descendants() {
    let server = MockServer::start();
    let children = run_json(
        &server,
        &[
            "list",
            "--server",
            "work",
            "--json",
            "--parent",
            "thread_parent",
        ],
    );
    assert_eq!(children["threads"][0]["id"], "thread_child_1");
    assert_eq!(children["threads"][0]["parentThreadId"], "thread_parent");

    let descendants = run_json(
        &server,
        &[
            "list",
            "--server",
            "work",
            "--json",
            "--ancestor",
            "thread_parent",
        ],
    );
    assert_eq!(descendants["threads"].as_array().unwrap().len(), 2);
    assert_eq!(descendants["threads"][0]["id"], "thread_grandchild_1");
    assert_eq!(
        descendants["threads"][0]["parentThreadId"],
        "thread_child_1"
    );

    let params = server.params_for("thread/list");
    assert_eq!(params[0]["parentThreadId"], "thread_parent");
    assert!(params[0].get("ancestorThreadId").is_none());
    assert_eq!(params[1]["ancestorThreadId"], "thread_parent");
    assert!(params[1].get("parentThreadId").is_none());
}

#[test]
fn list_passes_provider_and_source_filters() {
    let server = MockServer::start();
    let _ = run_json(
        &server,
        &[
            "list",
            "--server",
            "work",
            "--json",
            "--provider",
            "openai",
            "--provider",
            "azure",
            "--source",
            "sub-agent",
            "--source",
            "sub-agent-review",
        ],
    );

    let params = server.params_for("thread/list");
    assert_eq!(params[0]["modelProviders"], json!(["openai", "azure"]));
    assert_eq!(
        params[0]["sourceKinds"],
        json!(["subAgent", "subAgentReview"])
    );
}

#[test]
fn list_filters_sections_and_distinguishes_omitted_from_null() {
    let server = MockServer::start();
    let section = run_json(&server, &["list", "--json", "--section", "section_work"]);
    assert_eq!(section["threads"][0]["section"]["id"], "section_work");
    let unsectioned = run_json(&server, &["list", "--json", "--unsectioned"]);
    assert_eq!(unsectioned["threads"][0]["id"], "thread_unsectioned");
    run_json(&server, &["list", "--json"]);
    server
        .command()
        .args([
            "list",
            "--section",
            "section_work",
            "--sort",
            "section-position",
        ])
        .assert()
        .success()
        .stdout(predicates::str::contains("SECTION"))
        .stdout(predicates::str::contains("Work"));
    let params = server.params_for("thread/list");
    assert_eq!(params[0]["sectionId"], "section_work");
    assert_eq!(params[1].get("sectionId"), Some(&Value::Null));
    assert!(params[2].get("sectionId").is_none());
    assert_eq!(params[3]["sortKey"], "section_position");
    assert!(params.iter().all(|p| p.get("isPinned").is_none()));
    server
        .command()
        .args(["list", "--section", "section_work", "--unsectioned"])
        .assert()
        .code(2);
    for args in [
        vec!["pin", "thread_1"],
        vec!["unpin", "thread_1"],
        vec!["list", "--pinned"],
        vec!["list", "--unpinned"],
    ] {
        server.command().args(args).assert().code(2);
    }
}

#[test]
fn section_management_uses_current_codex_contract() {
    let server = MockServer::start();
    let list = run_json(
        &server,
        &[
            "sections", "list", "--limit", "10", "--cursor", "page", "--json",
        ],
    );
    assert!(list.to_string().contains("section_work"));
    let created = run_json(&server, &["sections", "create", "Research", "--json"]);
    assert_eq!(created["section"]["name"], "Research");
    let renamed = run_json(
        &server,
        &["sections", "rename", "section_new", "Review", "--json"],
    );
    assert_eq!(renamed["section"]["name"], "Review");
    run_json(
        &server,
        &[
            "section",
            "thread_1",
            "--section",
            "section_work",
            "--before",
            "thread_2",
            "--json",
        ],
    );
    run_json(&server, &["section", "thread_1", "--clear", "--json"]);
    run_json(&server, &["sections", "delete", "section_new", "--json"]);
    assert_eq!(
        server.params_for("threadSection/create"),
        vec![json!({"name": "Research"})]
    );
    assert_eq!(
        server.params_for("threadSection/update"),
        vec![json!({"sectionId": "section_new", "name": "Review"})]
    );
    let moves = server.params_for("thread/section/move");
    assert_eq!(moves[0]["threadId"], "thread_1");
    assert_eq!(moves[0]["sectionId"], "section_work");
    assert_eq!(moves[0]["beforeThreadId"], "thread_2");
    assert_eq!(moves[1].get("sectionId"), Some(&Value::Null));
    assert_eq!(
        server.params_for("threadSection/delete"),
        vec![json!({"sectionId": "section_new"})]
    );
    assert!(server.params_for("thread/metadata/update").is_empty());
    server
        .command()
        .args(["section", "thread_1", "--clear", "--before", "thread_2"])
        .assert()
        .code(2);
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

    let thread_start_params = server.params_for("thread/start");
    assert_eq!(thread_start_params.len(), 2);
    assert_thread_yolo_params(&thread_start_params[0]);
    assert_thread_yolo_params(&thread_start_params[1]);

    let turn_start_params = server.params_for("turn/start");
    assert_eq!(turn_start_params.len(), 2);
    assert_turn_yolo_params(&turn_start_params[0]);
    assert_turn_yolo_params(&turn_start_params[1]);

    let thread_resume_params = server.params_for("thread/resume");
    assert_eq!(thread_resume_params.len(), 1);
    assert_no_yolo_params(&thread_resume_params[0]);
}

#[test]
fn fork_command_returns_new_thread_and_sends_cutoff_params() {
    let server = MockServer::start();
    let forked = run_json(
        &server,
        &[
            "fork",
            "--server",
            "work",
            "--json",
            "--last-turn",
            "turn_2",
            "--model",
            "gpt-5.6",
            "--effort",
            "ultra",
            "--service-tier",
            "priority",
            "--name",
            "Forked thread",
            "thread_1",
        ],
    );
    assert_eq!(forked["threadId"], "thread_fork");
    assert_eq!(forked["forkedFromThreadId"], "thread_1");
    assert_eq!(forked["lastTurnId"], "turn_2");
    assert_eq!(forked["model"], "gpt-5.6");
    assert_eq!(forked["effort"], "ultra");
    assert_eq!(forked["serviceTier"], "priority");

    let fork_params = server.params_for("thread/fork");
    assert_eq!(fork_params.len(), 1);
    assert_eq!(fork_params[0]["threadId"], "thread_1");
    assert_eq!(fork_params[0]["lastTurnId"], "turn_2");
    assert_eq!(fork_params[0]["excludeTurns"], true);
    assert_eq!(fork_params[0]["model"], "gpt-5.6");
    assert_eq!(fork_params[0]["config"]["model_reasoning_effort"], "ultra");
    assert_eq!(fork_params[0]["serviceTier"], "priority");
    assert_thread_yolo_params(&fork_params[0]);

    let name_params = server.params_for("thread/name/set");
    assert_eq!(name_params.len(), 1);
    assert_eq!(name_params[0]["threadId"], "thread_fork");
    assert_eq!(name_params[0]["name"], "Forked thread");
}

#[test]
fn custom_reasoning_effort_passes_through_to_app_server() {
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
            "new",
            "--server",
            "work",
            "--cwd",
            &cwd,
            "--effort",
            "provider-private-effort",
            "--json",
        ],
    );
    assert_eq!(created["threadId"], "thread_new");

    let thread_start_params = server.params_for("thread/start");
    assert_eq!(
        thread_start_params[0]["config"]["model_reasoning_effort"],
        "provider-private-effort"
    );
}

#[test]
fn config_model_defaults_apply_to_new_threads_not_send_or_fork() {
    let server = MockServer::start();
    let cwd = server
        .config
        .parent()
        .unwrap()
        .to_string_lossy()
        .to_string();
    write_config(
        &server,
        format!(
            r#"model = "gpt-5.5"
model_reasoning_effort = "high"

[servers.work]
type = "uds"
path = "{}"
"#,
            server.socket.display()
        ),
    );

    let completed = run_json(
        &server,
        &[
            "new", "--server", "work", "--cwd", &cwd, "--json", "say done",
        ],
    );
    assert_eq!(completed["threadId"], "thread_new");

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

    let forked = run_json(&server, &["fork", "--server", "work", "--json", "thread_1"]);
    assert_eq!(forked["threadId"], "thread_fork");

    let thread_start_params = server.params_for("thread/start");
    assert_eq!(thread_start_params.len(), 1);
    assert_eq!(thread_start_params[0]["model"], "gpt-5.5");
    assert_eq!(
        thread_start_params[0]["config"]["model_reasoning_effort"],
        "high"
    );

    let turn_start_params = server.params_for("turn/start");
    assert_eq!(turn_start_params.len(), 2);
    assert!(turn_start_params[0].get("model").is_none());
    assert!(turn_start_params[0].get("effort").is_none());
    assert!(turn_start_params[1].get("model").is_none());
    assert!(turn_start_params[1].get("effort").is_none());

    let fork_params = server.params_for("thread/fork");
    assert_eq!(fork_params.len(), 1);
    assert_eq!(fork_params[0]["threadId"], "thread_1");
    assert_eq!(fork_params[0]["excludeTurns"], true);
    assert!(fork_params[0].get("lastTurnId").is_none());
    assert!(fork_params[0].get("model").is_none());
    assert!(fork_params[0].get("config").is_none());
}

#[test]
fn server_model_defaults_override_global_and_cli_overrides_config() {
    let server = MockServer::start();
    let cwd = server
        .config
        .parent()
        .unwrap()
        .to_string_lossy()
        .to_string();
    write_config(
        &server,
        format!(
            r#"model = "gpt-global"
model_reasoning_effort = "low"

[servers.work]
type = "uds"
path = "{}"
model = "gpt-5.5"
model_reasoning_effort = "high"
"#,
            server.socket.display()
        ),
    );

    let created = run_json(
        &server,
        &["new", "--server", "work", "--cwd", &cwd, "--json"],
    );
    assert_eq!(created["threadId"], "thread_new");

    let created = run_json(
        &server,
        &[
            "new", "--server", "work", "--cwd", &cwd, "--model", "gpt-cli", "--effort", "medium",
            "--json",
        ],
    );
    assert_eq!(created["threadId"], "thread_new");

    let thread_start_params = server.params_for("thread/start");
    assert_eq!(thread_start_params.len(), 2);
    assert_eq!(thread_start_params[0]["model"], "gpt-5.5");
    assert_eq!(
        thread_start_params[0]["config"]["model_reasoning_effort"],
        "high"
    );
    assert_eq!(thread_start_params[1]["model"], "gpt-cli");
    assert_eq!(
        thread_start_params[1]["config"]["model_reasoning_effort"],
        "medium"
    );
}

#[test]
fn no_yolo_uses_app_server_permission_defaults() {
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
            "--no-yolo",
            "new",
            "--server",
            "work",
            "--cwd",
            &cwd,
            "--json",
        ],
    );
    assert_eq!(created["threadId"], "thread_new");

    let accepted = run_json(
        &server,
        &[
            "--no-yolo",
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

    let settings = run_json(
        &server,
        &[
            "--no-yolo",
            "settings",
            "show",
            "--server",
            "work",
            "--json",
            "thread_1",
        ],
    );
    assert_eq!(settings["model"], "gpt-5.1-codex");

    for params in server.params_for("thread/start") {
        assert_no_yolo_params(&params);
    }
    for params in server.params_for("turn/start") {
        assert_no_yolo_params(&params);
    }
    for params in server.params_for("thread/resume") {
        assert_no_yolo_params(&params);
    }
}

#[test]
fn golden_json_output_shapes_are_stable() {
    let server = MockServer::start();

    assert_eq!(
        run_json(&server, &["list", "--server", "work", "--json"]),
        json!({
            "server": "work",
            "threads": [
                {
                    "id": "thread_1",
                    "name": "Mock Thread",
                    "preview": "Mock preview",
                    "cwd": "/tmp/mock-work",
                    "status": { "type": "idle" },
                    "createdAt": 1_700_000_000_i64,
                    "updatedAt": 1_700_000_100_i64,
                    "experimentalThreadField": { "retained": true }
                }
            ],
            "nextCursor": Value::Null,
            "backwardsCursor": Value::Null
        })
    );

    assert_eq!(
        run_json(
            &server,
            &["search", "threads", "--server", "work", "--json", "mock"],
        ),
        json!({
            "server": "work",
            "results": [
                {
                    "thread": {
                        "id": "thread_1",
                        "name": "Mock Thread",
                        "preview": "Mock preview",
                        "cwd": "/tmp/mock-work",
                        "status": { "type": "idle" },
                        "createdAt": 1_700_000_000_i64,
                        "updatedAt": 1_700_000_100_i64,
                        "experimentalThreadField": { "retained": true }
                    },
                    "score": 1.0
                }
            ],
            "nextCursor": Value::Null,
            "backwardsCursor": Value::Null
        })
    );

    assert_eq!(
        run_json(&server, &["show", "--server", "work", "--json", "thread_1"]),
        json!({
            "server": "work",
            "thread": {
                "id": "thread_1",
                "name": "Mock Thread",
                "preview": "Mock preview",
                "cwd": "/tmp/mock-work",
                "status": { "type": "idle" },
                "createdAt": 1_700_000_000_i64,
                "updatedAt": 1_700_000_100_i64,
                "experimentalThreadField": { "retained": true }
            },
            "turns": {
                "data": [
                    {
                        "id": "turn_1",
                        "status": "completed",
                        "startedAt": 1_700_000_050_i64,
                        "completedAt": 1_700_000_060_i64,
                        "experimentalTurnField": "retained",
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
                    }
                ],
                "nextCursor": Value::Null,
                "backwardsCursor": Value::Null
            }
        })
    );

    assert_eq!(
        run_json(
            &server,
            &["messages", "--server", "work", "--json", "thread_1"]
        ),
        json!({
            "server": "work",
            "threadId": "thread_1",
            "messages": [
                {
                    "role": "user",
                    "text": "hello",
                    "turnId": "turn_1",
                    "itemId": "item_user",
                    "turnStartedAt": 1_700_000_050_i64,
                    "turnCompletedAt": 1_700_000_060_i64
                },
                {
                    "role": "assistant",
                    "text": "done",
                    "turnId": "turn_1",
                    "itemId": "item_agent",
                    "turnStartedAt": 1_700_000_050_i64,
                    "turnCompletedAt": 1_700_000_060_i64
                }
            ],
            "truncated": false,
            "nextCursor": Value::Null
        })
    );

    assert_eq!(
        run_json(
            &server,
            &["status", "--server", "work", "--json", "thread_1"]
        ),
        json!({
            "server": "work",
            "threadId": "thread_1",
            "thread": {
                "id": "thread_1",
                "name": "Mock Thread",
                "preview": "Mock preview",
                "cwd": "/tmp/mock-work",
                "status": { "type": "idle" },
                "createdAt": 1_700_000_000_i64,
                "updatedAt": 1_700_000_100_i64,
                "experimentalThreadField": { "retained": true }
            },
            "activeTurnId": Value::Null,
            "truncated": false
        })
    );
}

#[test]
fn golden_send_json_output_shapes_are_stable() {
    let server = MockServer::start();

    assert_eq!(
        run_json(
            &server,
            &["send", "--server", "work", "--json", "thread_1", "continue"]
        ),
        json!({
            "server": "work",
            "threadId": "thread_1",
            "turnId": "turn_1",
            "status": "completed",
            "progress": [
                {
                    "type": "accepted",
                    "server": "work",
                    "threadId": "thread_1",
                    "turnId": "turn_1",
                    "status": "accepted"
                },
                {
                    "type": "progress",
                    "server": "work",
                    "threadId": "thread_1",
                    "turnId": "turn_1",
                    "itemId": "item_agent",
                    "delta": "done"
                },
                {
                    "type": "completed",
                    "server": "work",
                    "threadId": "thread_1",
                    "turnId": "turn_1",
                    "status": "completed"
                }
            ],
            "assistantResponses": [{ "itemId": "item_agent", "text": "done" }],
            "finalAssistantText": "done"
        })
    );

    assert_eq!(
        run_ndjson(
            &server,
            &[
                "send", "--server", "work", "--json", "--stream", "thread_1", "continue",
            ]
        ),
        vec![
            json!({
                "type": "accepted",
                "server": "work",
                "threadId": "thread_1",
                "turnId": "turn_1",
                "status": "accepted"
            }),
            json!({
                "type": "progress",
                "server": "work",
                "threadId": "thread_1",
                "turnId": "turn_1",
                "itemId": "item_agent",
                "delta": "done"
            }),
            json!({
                "type": "completed",
                "server": "work",
                "threadId": "thread_1",
                "turnId": "turn_1",
                "status": "completed"
            }),
        ]
    );
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
    assert!(text.contains("done\nstatus"));
    assert!(text.contains("completed"));
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
    assert!(text.contains("MODEL"));
    assert!(text.contains("NAME"));
    assert!(text.contains("gpt-5.5"));
    assert!(text.contains("GPT-5.5"));
    assert!(!text.starts_with("0"));
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

    let output = server
        .command()
        .args(["send", "--server", "work", "thread_1", "continue"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let text = String::from_utf8(output).expect("utf8");
    assert_eq!(text.match_indices("done").count(), 1, "{text}");
}

#[test]
fn send_resumes_not_loaded_thread_before_retrying_turn_start() {
    let server = MockServer::start_requiring_resume_for_send();
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
    assert_eq!(accepted["status"], "accepted");
    assert_eq!(accepted["threadId"], "thread_1");

    let methods = server.methods();
    let retry_methods = methods
        .iter()
        .filter(|method| matches!(method.as_str(), "turn/start" | "thread/resume"))
        .map(String::as_str)
        .collect::<Vec<_>>();
    assert_eq!(retry_methods, ["turn/start", "thread/resume", "turn/start"]);

    let turn_start_params = server.params_for("turn/start");
    assert_eq!(turn_start_params.len(), 2);
    assert_turn_yolo_params(&turn_start_params[0]);
    assert_turn_yolo_params(&turn_start_params[1]);

    let thread_resume_params = server.params_for("thread/resume");
    assert_eq!(thread_resume_params.len(), 1);
    assert_thread_yolo_params(&thread_resume_params[0]);
}

#[test]
fn direct_input_capability_blocks_send_and_steer_before_submission() {
    let server = MockServer::start();
    server
        .command()
        .args([
            "send",
            "--server",
            "work",
            "--no-wait",
            "thread_read_only",
            "continue",
        ])
        .assert()
        .code(3)
        .stderr(predicates::str::contains(
            "thread `thread_read_only` does not accept direct input",
        ));
    server
        .command()
        .args([
            "steer",
            "--server",
            "work",
            "thread_read_only",
            "turn_1",
            "adjust",
        ])
        .assert()
        .code(3)
        .stderr(predicates::str::contains(
            "thread `thread_read_only` does not accept direct input",
        ));
    assert!(server.params_for("turn/start").is_empty());
    assert!(server.params_for("turn/steer").is_empty());
}

#[test]
fn direct_input_capability_is_rechecked_after_resume() {
    let server = MockServer::start_requiring_resume_for_send();
    server
        .command()
        .args([
            "send",
            "--server",
            "work",
            "--no-wait",
            "thread_denied_after_resume",
            "continue",
        ])
        .assert()
        .code(3)
        .stderr(predicates::str::contains(
            "thread `thread_denied_after_resume` does not accept direct input",
        ));
    assert_eq!(server.params_for("turn/start").len(), 1);
    assert_eq!(server.params_for("thread/resume").len(), 1);
}

#[test]
fn no_yolo_resume_retry_uses_app_server_permission_defaults() {
    let server = MockServer::start_requiring_resume_for_send();
    let accepted = run_json(
        &server,
        &[
            "--no-yolo",
            "send",
            "--server",
            "work",
            "--json",
            "--no-wait",
            "thread_1",
            "continue",
        ],
    );
    assert_eq!(accepted["status"], "accepted");

    for params in server.params_for("turn/start") {
        assert_no_yolo_params(&params);
    }
    for params in server.params_for("thread/resume") {
        assert_no_yolo_params(&params);
    }
}

#[test]
fn settings_set_resumes_not_loaded_thread_before_retrying_update() {
    let server = MockServer::start_requiring_resume_for_settings_set();
    let updated = run_json(
        &server,
        &[
            "settings", "set", "--server", "work", "--json", "thread_1", "--effort", "high",
        ],
    );
    assert_eq!(updated["status"], "accepted");

    let methods = server.methods();
    let retry_methods = methods
        .iter()
        .filter(|method| matches!(method.as_str(), "thread/settings/update" | "thread/resume"))
        .map(String::as_str)
        .collect::<Vec<_>>();
    assert_eq!(
        retry_methods,
        [
            "thread/settings/update",
            "thread/resume",
            "thread/settings/update"
        ]
    );

    let thread_resume_params = server.params_for("thread/resume");
    assert_eq!(thread_resume_params.len(), 1);
    assert_thread_yolo_params(&thread_resume_params[0]);
}

#[test]
fn no_yolo_settings_set_resume_uses_app_server_permission_defaults() {
    let server = MockServer::start_requiring_resume_for_settings_set();
    let updated = run_json(
        &server,
        &[
            "--no-yolo",
            "settings",
            "set",
            "--server",
            "work",
            "--json",
            "thread_1",
            "--effort",
            "high",
        ],
    );
    assert_eq!(updated["status"], "accepted");

    let thread_resume_params = server.params_for("thread/resume");
    assert_eq!(thread_resume_params.len(), 1);
    assert_no_yolo_params(&thread_resume_params[0]);
}

#[test]
fn resume_retry_requires_exact_thread_not_found_error_contract() {
    let server = MockServer::start_rejecting_turn_start_with(-32600, "missing thread: thread_1");
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
        .code(3);

    let methods = server.methods();
    assert_eq!(
        methods
            .iter()
            .filter(|method| matches!(method.as_str(), "turn/start" | "thread/resume"))
            .map(String::as_str)
            .collect::<Vec<_>>(),
        ["turn/start"]
    );
}

#[test]
fn resume_retry_requires_invalid_request_error_code() {
    let server = MockServer::start_rejecting_turn_start_with(-32603, "thread not found: thread_1");
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
        .code(3);

    let methods = server.methods();
    assert_eq!(
        methods
            .iter()
            .filter(|method| matches!(method.as_str(), "turn/start" | "thread/resume"))
            .map(String::as_str)
            .collect::<Vec<_>>(),
        ["turn/start"]
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
    let goal_set = run_json(
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
    );
    assert_eq!(goal_set["goal"]["objective"], "Ship");
    assert_eq!(goal_set["goal"]["tokenBudget"].as_i64().unwrap(), 1000);
    let goal_get = run_json(
        &server,
        &["goal", "get", "--server", "work", "--json", "thread_1"],
    );
    assert_eq!(goal_get["goal"]["tokenBudget"].as_i64().unwrap(), 1000);
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
fn steer_resumes_not_loaded_thread_before_retrying_turn_steer() {
    let server = MockServer::start_requiring_resume_for_steer();
    let accepted = run_json(
        &server,
        &[
            "steer", "--server", "work", "--json", "thread_1", "turn_1", "adjust",
        ],
    );
    assert_eq!(accepted["status"], "accepted");
    assert_eq!(accepted["threadId"], "thread_1");
    assert_eq!(accepted["turnId"], "turn_1");

    let methods = server.methods();
    let retry_methods = methods
        .iter()
        .filter(|method| matches!(method.as_str(), "turn/steer" | "thread/resume"))
        .map(String::as_str)
        .collect::<Vec<_>>();
    assert_eq!(retry_methods, ["turn/steer", "thread/resume", "turn/steer"]);

    let thread_resume_params = server.params_for("thread/resume");
    assert_eq!(thread_resume_params.len(), 1);
    assert_thread_yolo_params(&thread_resume_params[0]);
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

#[test]
fn message_search_preserves_exact_occurrences_and_cursors() {
    let server = MockServer::start();
    let result = run_json(
        &server,
        &[
            "search", "messages", "thread_1", "fixture", "--limit", "7", "--cursor", "previous",
            "--json",
        ],
    );
    assert_eq!(result["threadId"], "thread_1");
    assert_eq!(result["occurrences"][0]["turnId"], "turn_1");
    assert_eq!(result["occurrences"][0]["itemId"], "item_agent");
    assert_eq!(result["occurrences"][0]["turnCursor"], "turn_cursor");
    assert_eq!(
        result["occurrences"][0]["snippetMatchRange"],
        json!({"start": 0, "end": 7})
    );
    assert_eq!(result["nextCursor"], "occurrence_cursor");
    assert_eq!(
        server.params_for("thread/searchOccurrences"),
        vec![
            json!({"threadId": "thread_1", "searchTerm": "fixture", "limit": 7, "cursor": "previous"})
        ]
    );
    server
        .command()
        .args(["search", "messages", "thread_1", "fixture"])
        .assert()
        .success()
        .stdout(predicates::str::contains("fixture complete"))
        .stdout(predicates::str::contains("occurrence_cursor"));
}

#[test]
fn draining_refusal_does_not_resume_or_replay_a_mutation() {
    let server = MockServer::start_rejecting_turn_start_with(
        -32600,
        "Server is draining; retry after reconnecting",
    );
    server
        .command()
        .args(["send", "thread_1", "continue", "--no-wait"])
        .assert()
        .code(3)
        .stderr(predicates::str::contains(
            "rejected `turn/start` before execution",
        ));
    assert_eq!(server.params_for("turn/start").len(), 1);
    assert!(server.params_for("thread/resume").is_empty());
}
