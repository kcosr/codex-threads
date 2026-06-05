use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use serde_json::{Map, Value, json};

use crate::config::Target;
use crate::errors::app_server_error;
use crate::rpc::{Notification, RpcClient, RpcRequestError};

#[derive(Debug)]
pub struct TurnStartOptions {
    pub model: Option<String>,
    pub effort: Option<String>,
    pub service_tier: Option<String>,
    pub yolo: bool,
}

#[derive(Debug)]
pub struct StartedTurn {
    pub acceptance: Value,
    pub thread_id: String,
    pub turn_id: String,
    early_notifications: Vec<Notification>,
}

#[derive(Debug)]
pub struct TurnTerminal {
    pub output: Value,
    pub exit_code: i32,
}

#[derive(Debug)]
pub enum TurnWaitOutcome {
    Terminal(TurnTerminal),
    LocalInterrupt { thread_id: String, turn_id: String },
}

#[cfg(feature = "tui")]
pub struct AttachTurnOptions {
    pub thread_id: String,
    pub turn_id: String,
    pub yolo: bool,
    pub poll_limit: u32,
    pub timeout: Duration,
}

#[cfg(feature = "tui")]
pub async fn steer_turn(
    target: &Target,
    client: &mut RpcClient,
    thread_id: String,
    turn_id: String,
    prompt: String,
    yolo: bool,
) -> Result<Value> {
    let params = json!({"threadId": thread_id, "expectedTurnId": turn_id, "input": [{"type": "text", "text": prompt, "textElements": []}]});
    let result = request_with_resume_retry(
        client,
        "turn/steer",
        params,
        &thread_id,
        yolo,
        || {},
        |_| {},
    )
    .await?;
    Ok(
        json!({"type": "accepted", "server": target.server, "threadId": thread_id, "turnId": result["turnId"].as_str().unwrap_or(&turn_id), "status": "accepted"}),
    )
}

#[cfg(feature = "tui")]
pub async fn interrupt_turn(
    target: &Target,
    client: &mut RpcClient,
    thread_id: String,
    turn_id: String,
) -> Result<Value> {
    let _ = client
        .request(
            "turn/interrupt",
            json!({"threadId": thread_id, "turnId": turn_id}),
            |_| {},
        )
        .await?;
    Ok(
        json!({"type": "accepted", "server": target.server, "threadId": thread_id, "turnId": turn_id, "status": "accepted"}),
    )
}

#[cfg(feature = "tui")]
pub async fn attach_turn<F, G>(
    target: &Target,
    client: &mut RpcClient,
    options: AttachTurnOptions,
    on_event: F,
    on_assistant_text_from_poll: G,
) -> Result<TurnWaitOutcome>
where
    F: FnMut(&Value) -> Result<()>,
    G: FnMut(&str) -> Result<()>,
{
    resume_thread_for_action(client, &options.thread_id, options.yolo).await?;
    let attached = json!({"type": "attached", "server": target.server, "threadId": options.thread_id, "turnId": options.turn_id, "status": "attached"});
    wait_for_turn(
        target,
        client,
        StartedTurn {
            acceptance: attached,
            thread_id: options.thread_id,
            turn_id: options.turn_id,
            early_notifications: Vec::new(),
        },
        options.poll_limit,
        options.timeout,
        on_event,
        on_assistant_text_from_poll,
    )
    .await
}

struct TurnWaitContext<'a> {
    target: &'a Target,
    thread_id: &'a str,
    turn_id: &'a str,
    poll_limit: u32,
}

pub async fn start_turn(
    target: &Target,
    client: &mut RpcClient,
    thread_id: String,
    prompt: String,
    options: TurnStartOptions,
) -> Result<StartedTurn> {
    let mut params = Map::new();
    params.insert("threadId".to_string(), json!(thread_id));
    params.insert(
        "input".to_string(),
        json!([{"type": "text", "text": prompt, "textElements": []}]),
    );
    if options.yolo {
        insert_turn_yolo_permissions(&mut params);
    }
    insert_opt(&mut params, "model", options.model);
    if let Some(effort) = options.effort {
        params.insert("effort".to_string(), json!(effort));
    }
    if let Some(tier) = options.service_tier {
        params.insert("serviceTier".to_string(), json!(tier));
    }
    let early_notifications = Arc::new(Mutex::new(Vec::new()));
    let params = Value::Object(params);
    let retry_notifications = early_notifications.clone();
    let captured_notifications = early_notifications.clone();
    let result = request_with_resume_retry(
        client,
        "turn/start",
        params,
        &thread_id,
        options.yolo,
        || {
            retry_notifications
                .lock()
                .expect("early notification buffer poisoned")
                .clear();
        },
        |notification| {
            captured_notifications
                .lock()
                .expect("early notification buffer poisoned")
                .push(notification);
        },
    )
    .await?;
    let turn_id = result["turn"]["id"]
        .as_str()
        .ok_or_else(|| app_server_error("turn/start response missing turn.id"))?
        .to_string();
    let acceptance = json!({"type": "accepted", "server": target.server, "threadId": thread_id, "turnId": turn_id, "status": "accepted"});
    Ok(StartedTurn {
        acceptance,
        thread_id,
        turn_id,
        early_notifications: early_notifications
            .lock()
            .expect("early notification buffer poisoned")
            .clone(),
    })
}

pub async fn wait_for_turn<F, G>(
    target: &Target,
    client: &mut RpcClient,
    started: StartedTurn,
    poll_limit: u32,
    timeout: Duration,
    mut on_event: F,
    mut on_assistant_text_from_poll: G,
) -> Result<TurnWaitOutcome>
where
    F: FnMut(&Value) -> Result<()>,
    G: FnMut(&str) -> Result<()>,
{
    let mut events = vec![started.acceptance];
    let mut assistant_text = String::new();
    let wait = TurnWaitContext {
        target,
        thread_id: &started.thread_id,
        turn_id: &started.turn_id,
        poll_limit,
    };
    for notification in started.early_notifications {
        let before_len = events.len();
        if let Some(terminal) =
            process_turn_notification(&wait, notification, &mut assistant_text, &mut events)?
        {
            if events.len() > before_len {
                on_event(events.last().expect("terminal event just pushed"))?;
            }
            return Ok(TurnWaitOutcome::Terminal(terminal));
        }
        if events.len() > before_len {
            on_event(events.last().expect("event just pushed"))?;
        }
    }
    let mut poll = tokio::time::interval(Duration::from_secs(1));
    poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let turn_timeout = tokio::time::sleep(timeout);
    tokio::pin!(turn_timeout);
    loop {
        tokio::select! {
            _ = &mut turn_timeout => {
                return Err(app_server_error(format!(
                    "timed out waiting for turn `{}` to complete",
                    started.turn_id
                )));
            }
            _ = tokio::signal::ctrl_c() => {
                return Ok(TurnWaitOutcome::LocalInterrupt {
                    thread_id: started.thread_id,
                    turn_id: started.turn_id,
                });
            }
            notification = client.next_notification_or_request() => {
                let notification = notification?;
                let before_len = events.len();
                if let Some(terminal) = process_turn_notification(
                    &wait,
                    notification,
                    &mut assistant_text,
                    &mut events,
                )? {
                    if events.len() > before_len {
                        on_event(events.last().expect("terminal event just pushed"))?;
                    }
                    return Ok(TurnWaitOutcome::Terminal(terminal));
                }
                if events.len() > before_len {
                    on_event(events.last().expect("event just pushed"))?;
                }
            }
            _ = poll.tick() => {
                let before_len = events.len();
                if let Some(terminal) = poll_turn_completion(
                    client,
                    &wait,
                    &mut assistant_text,
                    &mut events,
                    &mut on_assistant_text_from_poll,
                ).await? {
                    if events.len() > before_len {
                        on_event(events.last().expect("terminal event just pushed"))?;
                    }
                    return Ok(TurnWaitOutcome::Terminal(terminal));
                }
            }
        }
    }
}

async fn resume_thread_for_action(
    client: &mut RpcClient,
    thread_id: &str,
    yolo: bool,
) -> Result<()> {
    let mut params = Map::new();
    params.insert("threadId".to_string(), json!(thread_id));
    params.insert("excludeTurns".to_string(), json!(true));
    if yolo {
        insert_thread_yolo_permissions(&mut params);
    }
    client
        .request("thread/resume", Value::Object(params), |_| {})
        .await?;
    Ok(())
}

async fn request_with_resume_retry<F>(
    client: &mut RpcClient,
    method: &str,
    params: Value,
    thread_id: &str,
    yolo: bool,
    mut before_retry: impl FnMut(),
    mut on_notification: F,
) -> Result<Value>
where
    F: FnMut(Notification),
{
    match client
        .request(method, params.clone(), |notification| {
            on_notification(notification);
        })
        .await
    {
        Ok(result) => Ok(result),
        Err(err) if is_thread_not_found_error(&err, method, thread_id) => {
            before_retry();
            resume_thread_for_action(client, thread_id, yolo).await?;
            client
                .request(method, params, |notification| {
                    on_notification(notification);
                })
                .await
        }
        Err(err) => Err(err),
    }
}

fn is_thread_not_found_error(err: &anyhow::Error, method: &str, thread_id: &str) -> bool {
    let Some(error) = err.downcast_ref::<RpcRequestError>() else {
        return false;
    };
    error.method == method
        && error.error.code == -32600
        && error.error.message == format!("thread not found: {thread_id}")
}

async fn poll_turn_completion(
    client: &mut RpcClient,
    wait: &TurnWaitContext<'_>,
    assistant_text: &mut String,
    events: &mut Vec<Value>,
    on_assistant_text_from_poll: &mut impl FnMut(&str) -> Result<()>,
) -> Result<Option<TurnTerminal>> {
    let mut notifications = Vec::new();
    let result = client
        .request(
            "thread/turns/list",
            json!({"threadId": wait.thread_id, "limit": wait.poll_limit, "sortDirection": "desc", "itemsView": "full"}),
            |notification| notifications.push(notification),
        )
        .await?;
    for notification in notifications {
        if let Some(terminal) =
            process_turn_notification(wait, notification, assistant_text, events)?
        {
            return Ok(Some(terminal));
        }
    }

    let turn = result["data"].as_array().and_then(|turns| {
        turns
            .iter()
            .find(|turn| turn["id"].as_str() == Some(wait.turn_id))
    });
    let Some(turn) = turn else {
        return Ok(None);
    };
    reject_unknown_turn_status(turn)?;
    let status = turn_status(turn);
    if !matches!(status, "completed" | "failed" | "interrupted") {
        return Ok(None);
    }
    if assistant_text.is_empty() {
        *assistant_text = extract_assistant_text_from_turn(turn);
        if !assistant_text.is_empty() {
            on_assistant_text_from_poll(assistant_text)?;
        }
    }
    let event = json!({"type": status, "server": wait.target.server, "threadId": wait.thread_id, "turnId": wait.turn_id, "status": status, "source": "poll"});
    events.push(event);
    Ok(Some(turn_terminal(wait, status, assistant_text, events)))
}

fn process_turn_notification(
    wait: &TurnWaitContext<'_>,
    notification: Notification,
    assistant_text: &mut String,
    events: &mut Vec<Value>,
) -> Result<Option<TurnTerminal>> {
    let Some(event) = turn_event(
        &wait.target.server,
        wait.thread_id,
        wait.turn_id,
        notification,
        assistant_text,
    )?
    else {
        return Ok(None);
    };

    let status = event["status"].as_str().map(str::to_string);
    events.push(event);
    if !matches!(
        status.as_deref(),
        Some("completed" | "failed" | "interrupted")
    ) {
        return Ok(None);
    }

    let status = status.expect("status checked");
    Ok(Some(turn_terminal(wait, &status, assistant_text, events)))
}

fn turn_terminal(
    wait: &TurnWaitContext<'_>,
    status: &str,
    assistant_text: &str,
    events: &[Value],
) -> TurnTerminal {
    let output = json!({
        "server": wait.target.server,
        "threadId": wait.thread_id,
        "turnId": wait.turn_id,
        "status": status,
        "progress": events,
        "assistantResponses": if assistant_text.is_empty() { Vec::<Value>::new() } else { vec![json!({"text": assistant_text})] },
        "finalAssistantText": assistant_text
    });
    let exit_code = if output["status"].as_str() == Some("completed") {
        0
    } else {
        1
    };
    TurnTerminal { output, exit_code }
}

fn extract_assistant_text_from_turn(turn: &Value) -> String {
    turn["items"]
        .as_array()
        .unwrap_or(&Vec::new())
        .iter()
        .filter(|item| item["type"].as_str() == Some("agentMessage"))
        .filter_map(|item| item["text"].as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

fn turn_event(
    server: &str,
    thread_id: &str,
    turn_id: &str,
    notification: Notification,
    assistant_text: &mut String,
) -> Result<Option<Value>> {
    match notification.method.as_str() {
        "item/agentMessage/delta"
            if notification.params["threadId"] == thread_id
                && notification.params["turnId"] == turn_id =>
        {
            let delta = notification.params["delta"].as_str().unwrap_or("");
            assistant_text.push_str(delta);
            Ok(Some(
                json!({"type": "progress", "server": server, "threadId": thread_id, "turnId": turn_id, "delta": delta}),
            ))
        }
        "item/completed"
            if notification.params["threadId"] == thread_id
                && notification.params["turnId"] == turn_id =>
        {
            if notification.params["item"]["type"].as_str() == Some("agentMessage")
                && let Some(text) = notification.params["item"]["text"].as_str()
                && assistant_text.is_empty()
            {
                assistant_text.push_str(text);
                return Ok(Some(
                    json!({"type": "assistantMessage", "server": server, "threadId": thread_id, "turnId": turn_id, "text": text}),
                ));
            }
            Ok(None)
        }
        "turn/completed"
            if notification.params["threadId"] == thread_id
                && notification.params["turn"]["id"] == turn_id =>
        {
            reject_unknown_turn_status(&notification.params["turn"])?;
            let status = turn_status(&notification.params["turn"]);
            Ok(Some(
                json!({"type": status, "server": server, "threadId": thread_id, "turnId": turn_id, "status": status}),
            ))
        }
        _ => Ok(None),
    }
}

fn insert_opt(map: &mut Map<String, Value>, key: &str, value: Option<String>) {
    if let Some(value) = value {
        map.insert(key.to_string(), json!(value));
    }
}

fn insert_thread_yolo_permissions(map: &mut Map<String, Value>) {
    map.insert("approvalPolicy".to_string(), json!("never"));
    map.insert("sandbox".to_string(), json!("danger-full-access"));
}

fn insert_turn_yolo_permissions(map: &mut Map<String, Value>) {
    map.insert("approvalPolicy".to_string(), json!("never"));
    map.insert(
        "sandboxPolicy".to_string(),
        json!({"type": "dangerFullAccess"}),
    );
}

fn turn_status(turn: &Value) -> &'static str {
    match turn["status"].as_str().unwrap_or("inProgress") {
        "completed" => "completed",
        "interrupted" => "interrupted",
        "failed" => "failed",
        _ => "inProgress",
    }
}

fn reject_unknown_turn_status(turn: &Value) -> Result<()> {
    let Some(status) = turn["status"].as_str() else {
        return Ok(());
    };
    match status {
        "completed" | "interrupted" | "failed" | "inProgress" | "running" | "pending" => Ok(()),
        _ => Err(app_server_error(format!(
            "app-server returned unrecognized turn status `{status}`"
        ))),
    }
}
