use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use serde_json::{Map, Value, json};
#[cfg(feature = "tui")]
use tokio::sync::mpsc;

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

#[derive(Debug, Clone, Default)]
struct AssistantResponses {
    items: Vec<AssistantResponse>,
}

#[derive(Debug, Clone)]
struct AssistantResponse {
    item_id: Option<String>,
    text: String,
}

impl AssistantResponses {
    fn is_empty(&self) -> bool {
        self.items.iter().all(|item| item.text.is_empty())
    }

    fn contains_item(&self, item_id: Option<&str>) -> bool {
        self.items.iter().any(|item| match item_id {
            Some(item_id) => item.item_id.as_deref() == Some(item_id),
            None => item.item_id.is_none(),
        })
    }

    fn text_for_item(&self, item_id: Option<&str>) -> Option<&str> {
        self.items
            .iter()
            .find(|item| match item_id {
                Some(item_id) => item.item_id.as_deref() == Some(item_id),
                None => item.item_id.is_none(),
            })
            .map(|item| item.text.as_str())
    }

    fn append_delta(&mut self, item_id: Option<&str>, delta: &str) {
        self.item_mut(item_id).text.push_str(delta);
    }

    fn set_text(&mut self, item_id: Option<&str>, text: &str) {
        self.item_mut(item_id).text = text.to_string();
    }

    fn replace_from_turn(&mut self, turn: &Value) {
        self.items = turn["items"]
            .as_array()
            .unwrap_or(&Vec::new())
            .iter()
            .filter(|item| item["type"].as_str() == Some("agentMessage"))
            .filter_map(|item| {
                Some(AssistantResponse {
                    item_id: item["id"].as_str().map(str::to_string),
                    text: item["text"].as_str()?.to_string(),
                })
            })
            .collect();
    }

    fn final_text(&self) -> String {
        self.items
            .iter()
            .filter(|item| !item.text.is_empty())
            .map(|item| item.text.as_str())
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn to_json(&self) -> Vec<Value> {
        self.items
            .iter()
            .filter(|item| !item.text.is_empty())
            .map(|item| {
                let mut map = Map::new();
                if let Some(item_id) = &item.item_id {
                    map.insert("itemId".to_string(), json!(item_id));
                }
                map.insert("text".to_string(), json!(item.text));
                Value::Object(map)
            })
            .collect()
    }

    fn item_mut(&mut self, item_id: Option<&str>) -> &mut AssistantResponse {
        if let Some(index) = self.items.iter().position(|item| match item_id {
            Some(item_id) => item.item_id.as_deref() == Some(item_id),
            None => item.item_id.is_none(),
        }) {
            return &mut self.items[index];
        }
        self.items.push(AssistantResponse {
            item_id: item_id.map(str::to_string),
            text: String::new(),
        });
        self.items
            .last_mut()
            .expect("assistant response just pushed")
    }
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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnControl {
    PollNow,
    Detach,
    Interrupt,
}

#[cfg(feature = "tui")]
pub struct ControlledTurnWaitOptions {
    pub poll_limit: u32,
    pub timeout: Duration,
    pub unsubscribe_on_detach: bool,
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
    control_rx: mpsc::UnboundedReceiver<TurnControl>,
    mut on_event: F,
    on_assistant_text_from_poll: G,
) -> Result<TurnWaitOutcome>
where
    F: FnMut(&Value) -> Result<()>,
    G: FnMut(&str) -> Result<()>,
{
    let resume = resume_thread_for_action(
        client,
        &options.thread_id,
        options.yolo,
        /*exclude_turns*/ false,
    )
    .await?;
    let attached = json!({
        "type": "attached",
        "server": target.server,
        "threadId": options.thread_id,
        "turnId": options.turn_id,
        "status": "attached",
        "thread": resume["thread"].clone()
    });
    on_event(&attached)?;
    wait_for_turn_controlled(
        target,
        client,
        StartedTurn {
            acceptance: attached,
            thread_id: options.thread_id,
            turn_id: options.turn_id,
            early_notifications: Vec::new(),
        },
        ControlledTurnWaitOptions {
            poll_limit: options.poll_limit,
            timeout: options.timeout,
            unsubscribe_on_detach: true,
        },
        control_rx,
        on_event,
        on_assistant_text_from_poll,
    )
    .await
}

#[cfg(feature = "tui")]
pub async fn wait_for_turn_controlled<F, G>(
    target: &Target,
    client: &mut RpcClient,
    started: StartedTurn,
    options: ControlledTurnWaitOptions,
    mut control_rx: mpsc::UnboundedReceiver<TurnControl>,
    mut on_event: F,
    mut on_assistant_text_from_poll: G,
) -> Result<TurnWaitOutcome>
where
    F: FnMut(&Value) -> Result<()>,
    G: FnMut(&str) -> Result<()>,
{
    let mut events = vec![started.acceptance];
    let mut assistant = AssistantResponses::default();
    let wait = TurnWaitContext {
        target,
        thread_id: &started.thread_id,
        turn_id: &started.turn_id,
        poll_limit: options.poll_limit,
    };
    for notification in started.early_notifications {
        let before_len = events.len();
        if let Some(terminal) =
            process_turn_notification(&wait, notification, &mut assistant, &mut events)?
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
    let turn_timeout = tokio::time::sleep(options.timeout);
    tokio::pin!(turn_timeout);
    loop {
        tokio::select! {
            _ = &mut turn_timeout => {
                return Err(app_server_error(format!(
                    "timed out waiting for turn `{}` to complete",
                    started.turn_id
                )));
            }
            control = control_rx.recv() => {
                match control {
                    Some(TurnControl::PollNow) => {
                        let before_len = events.len();
                        if let Some(terminal) = poll_turn_completion(
                            client,
                            &wait,
                            &mut assistant,
                            &mut events,
                            &mut on_assistant_text_from_poll,
                        ).await? {
                            if events.len() > before_len {
                                on_event(events.last().expect("terminal event just pushed"))?;
                            }
                            return Ok(TurnWaitOutcome::Terminal(terminal));
                        }
                    }
                    Some(TurnControl::Detach) | None => {
                        if options.unsubscribe_on_detach {
                            let _ = client
                                .request("thread/unsubscribe", json!({"threadId": started.thread_id}), |_| {})
                                .await;
                        }
                        return Ok(TurnWaitOutcome::LocalInterrupt {
                            thread_id: started.thread_id,
                            turn_id: started.turn_id,
                        });
                    }
                    Some(TurnControl::Interrupt) => {
                        let _ = client
                            .request(
                                "turn/interrupt",
                                json!({"threadId": started.thread_id, "turnId": started.turn_id}),
                                |_| {},
                            )
                            .await?;
                    }
                }
            }
            notification = client.next_notification_or_request() => {
                let notification = notification?;
                let before_len = events.len();
                if let Some(terminal) = process_turn_notification(
                    &wait,
                    notification,
                    &mut assistant,
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
                    &mut assistant,
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
    let mut assistant = AssistantResponses::default();
    let wait = TurnWaitContext {
        target,
        thread_id: &started.thread_id,
        turn_id: &started.turn_id,
        poll_limit,
    };
    for notification in started.early_notifications {
        let before_len = events.len();
        if let Some(terminal) =
            process_turn_notification(&wait, notification, &mut assistant, &mut events)?
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
                    &mut assistant,
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
                    &mut assistant,
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
    exclude_turns: bool,
) -> Result<Value> {
    let mut params = Map::new();
    params.insert("threadId".to_string(), json!(thread_id));
    params.insert("excludeTurns".to_string(), json!(exclude_turns));
    if yolo {
        insert_thread_yolo_permissions(&mut params);
    }
    let result = client
        .request("thread/resume", Value::Object(params), |_| {})
        .await?;
    Ok(result)
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
            resume_thread_for_action(client, thread_id, yolo, /*exclude_turns*/ true).await?;
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
    assistant: &mut AssistantResponses,
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
        if let Some(terminal) = process_turn_notification(wait, notification, assistant, events)? {
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
    if assistant.is_empty() {
        assistant.replace_from_turn(turn);
        let final_text = assistant.final_text();
        if !final_text.is_empty() {
            on_assistant_text_from_poll(&final_text)?;
        }
    }
    let event = json!({"type": status, "server": wait.target.server, "threadId": wait.thread_id, "turnId": wait.turn_id, "status": status, "source": "poll"});
    events.push(event);
    Ok(Some(turn_terminal(wait, status, assistant, events)))
}

fn process_turn_notification(
    wait: &TurnWaitContext<'_>,
    notification: Notification,
    assistant: &mut AssistantResponses,
    events: &mut Vec<Value>,
) -> Result<Option<TurnTerminal>> {
    let Some(event) = turn_event(
        &wait.target.server,
        wait.thread_id,
        wait.turn_id,
        notification,
        assistant,
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
    Ok(Some(turn_terminal(wait, &status, assistant, events)))
}

fn turn_terminal(
    wait: &TurnWaitContext<'_>,
    status: &str,
    assistant: &AssistantResponses,
    events: &[Value],
) -> TurnTerminal {
    let final_text = assistant.final_text();
    let output = json!({
        "server": wait.target.server,
        "threadId": wait.thread_id,
        "turnId": wait.turn_id,
        "status": status,
        "progress": events,
        "assistantResponses": assistant.to_json(),
        "finalAssistantText": final_text
    });
    let exit_code = if output["status"].as_str() == Some("completed") {
        0
    } else {
        1
    };
    TurnTerminal { output, exit_code }
}

fn turn_event(
    server: &str,
    thread_id: &str,
    turn_id: &str,
    notification: Notification,
    assistant: &mut AssistantResponses,
) -> Result<Option<Value>> {
    match notification.method.as_str() {
        "item/agentMessage/delta"
            if notification.params["threadId"] == thread_id
                && notification.params["turnId"] == turn_id =>
        {
            let delta = notification.params["delta"].as_str().unwrap_or("");
            let item_id = notification.params["itemId"].as_str();
            assistant.append_delta(item_id, delta);
            let mut event = Map::new();
            event.insert("type".to_string(), json!("progress"));
            event.insert("server".to_string(), json!(server));
            event.insert("threadId".to_string(), json!(thread_id));
            event.insert("turnId".to_string(), json!(turn_id));
            insert_opt(&mut event, "itemId", item_id.map(str::to_string));
            event.insert("delta".to_string(), json!(delta));
            Ok(Some(Value::Object(event)))
        }
        "item/completed"
            if notification.params["threadId"] == thread_id
                && notification.params["turnId"] == turn_id =>
        {
            if notification.params["item"]["type"].as_str() == Some("agentMessage")
                && let Some(text) = notification.params["item"]["text"].as_str()
            {
                let item_id = notification.params["item"]["id"].as_str();
                let previous_text = assistant.text_for_item(item_id).map(str::to_string);
                let was_known = assistant.contains_item(item_id);
                assistant.set_text(item_id, text);
                if !was_known || previous_text.as_deref() != Some(text) {
                    let mut event = Map::new();
                    event.insert("type".to_string(), json!("assistantMessage"));
                    event.insert("server".to_string(), json!(server));
                    event.insert("threadId".to_string(), json!(thread_id));
                    event.insert("turnId".to_string(), json!(turn_id));
                    insert_opt(&mut event, "itemId", item_id.map(str::to_string));
                    event.insert("text".to_string(), json!(text));
                    return Ok(Some(Value::Object(event)));
                }
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

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::config::Endpoint;

    #[test]
    fn turn_terminal_preserves_multiple_assistant_item_responses() {
        let target = Target {
            server: "work".to_string(),
            endpoint: Endpoint::Unix {
                path: PathBuf::from("/tmp/mock.sock"),
            },
            model: None,
            model_reasoning_effort: None,
        };
        let wait = TurnWaitContext {
            target: &target,
            thread_id: "thread-1",
            turn_id: "turn-1",
            poll_limit: 50,
        };
        let mut assistant = AssistantResponses::default();
        let mut events = Vec::new();

        for notification in [
            Notification {
                method: "item/agentMessage/delta".to_string(),
                params: json!({
                    "threadId": "thread-1",
                    "turnId": "turn-1",
                    "itemId": "assistant-1",
                    "delta": "first"
                }),
            },
            Notification {
                method: "item/agentMessage/delta".to_string(),
                params: json!({
                    "threadId": "thread-1",
                    "turnId": "turn-1",
                    "itemId": "assistant-1",
                    "delta": " response"
                }),
            },
            Notification {
                method: "item/agentMessage/delta".to_string(),
                params: json!({
                    "threadId": "thread-1",
                    "turnId": "turn-1",
                    "itemId": "assistant-2",
                    "delta": "second"
                }),
            },
            Notification {
                method: "item/completed".to_string(),
                params: json!({
                    "threadId": "thread-1",
                    "turnId": "turn-1",
                    "item": {
                        "id": "assistant-2",
                        "type": "agentMessage",
                        "text": "second corrected"
                    }
                }),
            },
        ] {
            assert!(
                process_turn_notification(&wait, notification, &mut assistant, &mut events)
                    .unwrap()
                    .is_none()
            );
        }

        let terminal = process_turn_notification(
            &wait,
            Notification {
                method: "turn/completed".to_string(),
                params: json!({
                    "threadId": "thread-1",
                    "turn": {"id": "turn-1", "status": "completed", "items": []}
                }),
            },
            &mut assistant,
            &mut events,
        )
        .unwrap()
        .expect("terminal turn");

        assert_eq!(
            terminal.output["finalAssistantText"],
            "first response\nsecond corrected"
        );
        assert_eq!(
            terminal.output["assistantResponses"],
            json!([
                {"itemId": "assistant-1", "text": "first response"},
                {"itemId": "assistant-2", "text": "second corrected"}
            ])
        );
        assert_eq!(terminal.output["progress"][0]["itemId"], "assistant-1");
        assert_eq!(terminal.output["progress"][2]["itemId"], "assistant-2");
        assert_eq!(terminal.output["progress"][3]["type"], "assistantMessage");
        assert_eq!(terminal.output["progress"][3]["itemId"], "assistant-2");
    }
}
