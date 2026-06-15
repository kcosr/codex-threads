use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::Result;
use serde_json::{Map, Value, json};
#[cfg(feature = "tui")]
use tokio::sync::mpsc;

use crate::config::Target;
use crate::errors::app_server_error;
use crate::rpc::{Notification, RpcClient};
use crate::session::request_with_resume_retry;
#[cfg(feature = "tui")]
use crate::session::resume_thread_for_action_with_notifications;

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
    prompt: Option<String>,
    started_after_epoch: Option<i64>,
    early_notifications: Vec<Notification>,
}

impl StartedTurn {
    #[cfg(feature = "tui")]
    pub fn prompt(&self) -> Option<&str> {
        self.prompt.as_deref()
    }
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

    fn sync_from_turn(&mut self, turn: &Value) -> Vec<AssistantResponse> {
        let mut updates = Vec::new();
        for item in turn["items"].as_array().unwrap_or(&Vec::new()) {
            if item["type"].as_str() != Some("agentMessage") {
                continue;
            }
            let Some(text) = item["text"].as_str() else {
                continue;
            };
            if text.is_empty() {
                continue;
            }
            let item_id = item["id"].as_str();
            if self.text_for_item(item_id) == Some(text) {
                continue;
            }
            self.set_text(item_id, text);
            updates.push(AssistantResponse {
                item_id: item_id.map(str::to_string),
                text: text.to_string(),
            });
        }
        updates
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
        if let Some(item_id) = item_id
            && let Some(index) = self.items.iter().rposition(|item| item.item_id.is_none())
        {
            self.items[index].item_id = Some(item_id.to_string());
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
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TurnControl {
    PollNow,
    Submit { prompt: String, yolo: bool },
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
    let mut early_notifications = Vec::new();
    let resume = resume_thread_for_action_with_notifications(
        client,
        &options.thread_id,
        options.yolo,
        /*exclude_turns*/ false,
        |notification| early_notifications.push(notification),
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
            prompt: None,
            started_after_epoch: None,
            early_notifications,
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
    let mut wait = TurnWaitContext {
        target,
        thread_id: &started.thread_id,
        turn_id: started.turn_id.clone(),
        prompt: started.prompt.as_deref(),
        started_after_epoch: started.started_after_epoch,
        poll_limit: options.poll_limit,
    };
    for notification in started.early_notifications {
        let before_len = events.len();
        if let Some(terminal) =
            process_turn_notification(&wait, notification, &mut assistant, &mut events)?
        {
            emit_new_events(&events, before_len, &mut on_event)?;
            return Ok(TurnWaitOutcome::Terminal(terminal));
        }
        emit_new_events(&events, before_len, &mut on_event)?;
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
                        let terminal = poll_turn_completion(
                            client,
                            &mut wait,
                            &mut assistant,
                            &mut events,
                            &mut on_assistant_text_from_poll,
                        ).await?;
                        emit_new_events(&events, before_len, &mut on_event)?;
                        if let Some(terminal) = terminal {
                            return Ok(TurnWaitOutcome::Terminal(terminal));
                        }
                    }
                    Some(TurnControl::Submit { prompt, yolo }) => {
                        let queued = start_turn(
                            target,
                            client,
                            started.thread_id.clone(),
                            prompt.clone(),
                            TurnStartOptions {
                                model: None,
                                effort: None,
                                service_tier: None,
                                yolo,
                            },
                        )
                        .await?;
                        let mut event = queued.acceptance.clone();
                        event["type"] = json!("queued");
                        event["prompt"] = json!(prompt);
                        on_event(&event)?;
                    }
                    Some(TurnControl::Detach) | None => {
                        if options.unsubscribe_on_detach {
                            let _ = client
                                .request("thread/unsubscribe", json!({"threadId": started.thread_id}), |_| {})
                                .await;
                        }
                        return Ok(TurnWaitOutcome::LocalInterrupt {
                            thread_id: started.thread_id.clone(),
                            turn_id: wait.turn_id.clone(),
                        });
                    }
                    Some(TurnControl::Interrupt) => {
                        let _ = client
                            .request(
                                "turn/interrupt",
                                json!({"threadId": started.thread_id, "turnId": &wait.turn_id}),
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
                    emit_new_events(&events, before_len, &mut on_event)?;
                    return Ok(TurnWaitOutcome::Terminal(terminal));
                }
                emit_new_events(&events, before_len, &mut on_event)?;
            }
            _ = poll.tick() => {
                let before_len = events.len();
                let terminal = poll_turn_completion(
                    client,
                    &mut wait,
                    &mut assistant,
                    &mut events,
                    &mut on_assistant_text_from_poll,
                ).await?;
                emit_new_events(&events, before_len, &mut on_event)?;
                if let Some(terminal) = terminal {
                    return Ok(TurnWaitOutcome::Terminal(terminal));
                }
            }
        }
    }
}

struct TurnWaitContext<'a> {
    target: &'a Target,
    thread_id: &'a str,
    turn_id: String,
    prompt: Option<&'a str>,
    started_after_epoch: Option<i64>,
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
    let prompt_for_match = prompt.clone();
    let started_after_epoch = Some(current_epoch_seconds().saturating_sub(1));
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
        prompt: Some(prompt_for_match),
        started_after_epoch,
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
    let mut wait = TurnWaitContext {
        target,
        thread_id: &started.thread_id,
        turn_id: started.turn_id.clone(),
        prompt: started.prompt.as_deref(),
        started_after_epoch: started.started_after_epoch,
        poll_limit,
    };
    for notification in started.early_notifications {
        let before_len = events.len();
        if let Some(terminal) =
            process_turn_notification(&wait, notification, &mut assistant, &mut events)?
        {
            emit_new_events(&events, before_len, &mut on_event)?;
            return Ok(TurnWaitOutcome::Terminal(terminal));
        }
        emit_new_events(&events, before_len, &mut on_event)?;
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
                    thread_id: started.thread_id.clone(),
                    turn_id: wait.turn_id.clone(),
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
                    emit_new_events(&events, before_len, &mut on_event)?;
                    return Ok(TurnWaitOutcome::Terminal(terminal));
                }
                emit_new_events(&events, before_len, &mut on_event)?;
            }
            _ = poll.tick() => {
                let before_len = events.len();
                let terminal = poll_turn_completion(
                    client,
                    &mut wait,
                    &mut assistant,
                    &mut events,
                    &mut on_assistant_text_from_poll,
                ).await?;
                emit_new_events(&events, before_len, &mut on_event)?;
                if let Some(terminal) = terminal {
                    return Ok(TurnWaitOutcome::Terminal(terminal));
                }
            }
        }
    }
}

async fn poll_turn_completion(
    client: &mut RpcClient,
    wait: &mut TurnWaitContext<'_>,
    assistant: &mut AssistantResponses,
    events: &mut Vec<Value>,
    _on_assistant_text_from_poll: &mut impl FnMut(&str) -> Result<()>,
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

    let turn = poll_result_turn(wait, &result);
    let Some(turn) = turn else {
        return Ok(None);
    };
    reject_unknown_turn_status(turn)?;
    let status = turn_status(turn);
    let updates = assistant.sync_from_turn(turn);
    for update in updates {
        let mut event = Map::new();
        event.insert("type".to_string(), json!("progress"));
        event.insert("server".to_string(), json!(wait.target.server));
        event.insert("threadId".to_string(), json!(wait.thread_id));
        event.insert("turnId".to_string(), json!(&wait.turn_id));
        insert_opt(&mut event, "itemId", update.item_id);
        event.insert("text".to_string(), json!(update.text));
        event.insert("source".to_string(), json!("poll"));
        events.push(Value::Object(event));
    }
    if !matches!(status, "completed" | "failed" | "interrupted") {
        return Ok(None);
    }
    let event = json!({"type": status, "server": wait.target.server, "threadId": wait.thread_id, "turnId": &wait.turn_id, "status": status, "source": "poll"});
    events.push(event);
    Ok(Some(turn_terminal(wait, status, assistant, events)))
}

fn poll_result_turn<'a>(wait: &mut TurnWaitContext<'_>, result: &'a Value) -> Option<&'a Value> {
    let turns = result["data"].as_array()?;
    if let Some(turn) = turns
        .iter()
        .find(|turn| turn["id"].as_str() == Some(wait.turn_id.as_str()))
    {
        return Some(turn);
    }
    let prompt = wait.prompt?;
    let turn = turns.first()?;
    if !turn_matches_prompt(turn, prompt) || !turn_started_after(turn, wait.started_after_epoch) {
        return None;
    }
    if let Some(turn_id) = turn["id"].as_str() {
        wait.turn_id = turn_id.to_string();
    }
    Some(turn)
}

fn turn_matches_prompt(turn: &Value, prompt: &str) -> bool {
    let Some(items) = turn["items"].as_array() else {
        return false;
    };
    items.iter().any(|item| {
        item["type"].as_str() == Some("userMessage")
            && user_message_text(item).as_deref() == Some(prompt)
    })
}

fn user_message_text(item: &Value) -> Option<String> {
    let content = item["content"].as_array()?;
    Some(
        content
            .iter()
            .filter_map(|input| input["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n"),
    )
}

fn turn_started_after(turn: &Value, started_after_epoch: Option<i64>) -> bool {
    let Some(started_after_epoch) = started_after_epoch else {
        return false;
    };
    turn["startedAt"]
        .as_i64()
        .or_else(|| turn["completedAt"].as_i64())
        .is_some_and(|timestamp| timestamp >= started_after_epoch)
}

fn emit_new_events(
    events: &[Value],
    before_len: usize,
    on_event: &mut impl FnMut(&Value) -> Result<()>,
) -> Result<()> {
    for event in events.iter().skip(before_len) {
        on_event(event)?;
    }
    Ok(())
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
        &wait.turn_id,
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
        "turnId": &wait.turn_id,
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

fn insert_turn_yolo_permissions(map: &mut Map<String, Value>) {
    map.insert("approvalPolicy".to_string(), json!("never"));
    map.insert(
        "sandboxPolicy".to_string(),
        json!({"type": "dangerFullAccess"}),
    );
}

fn current_epoch_seconds() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
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
            turn_id: "turn-1".to_string(),
            prompt: None,
            started_after_epoch: None,
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

    #[test]
    fn assistant_response_adopts_item_id_for_provisional_delta() {
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
            turn_id: "turn-1".to_string(),
            prompt: None,
            started_after_epoch: None,
            poll_limit: 50,
        };
        let mut assistant = AssistantResponses::default();
        let mut events = Vec::new();

        process_turn_notification(
            &wait,
            Notification {
                method: "item/agentMessage/delta".to_string(),
                params: json!({
                    "threadId": "thread-1",
                    "turnId": "turn-1",
                    "delta": "draft"
                }),
            },
            &mut assistant,
            &mut events,
        )
        .unwrap();
        process_turn_notification(
            &wait,
            Notification {
                method: "item/completed".to_string(),
                params: json!({
                    "threadId": "thread-1",
                    "turnId": "turn-1",
                    "item": {
                        "id": "assistant-1",
                        "type": "agentMessage",
                        "text": "draft final"
                    }
                }),
            },
            &mut assistant,
            &mut events,
        )
        .unwrap();

        assert_eq!(assistant.final_text(), "draft final");
        assert_eq!(
            assistant.to_json(),
            vec![json!({"itemId": "assistant-1", "text": "draft final"})]
        );
    }

    #[test]
    fn assistant_response_sync_from_turn_reports_changes_once() {
        let mut assistant = AssistantResponses::default();
        let turn = json!({
            "items": [
                {
                    "id": "assistant-1",
                    "type": "agentMessage",
                    "text": "current active text"
                }
            ]
        });

        let updates = assistant.sync_from_turn(&turn);

        assert_eq!(updates.len(), 1);
        assert_eq!(updates[0].item_id.as_deref(), Some("assistant-1"));
        assert_eq!(updates[0].text, "current active text");
        assert_eq!(assistant.final_text(), "current active text");
        assert!(assistant.sync_from_turn(&turn).is_empty());
    }

    #[test]
    fn poll_result_turn_adopts_persisted_turn_id_by_prompt_when_start_id_is_absent() {
        let target = Target {
            server: "work".to_string(),
            endpoint: Endpoint::Unix {
                path: PathBuf::from("/tmp/mock.sock"),
            },
            model: None,
            model_reasoning_effort: None,
        };
        let mut wait = TurnWaitContext {
            target: &target,
            thread_id: "thread-1",
            turn_id: "returned-id".to_string(),
            prompt: Some("Reply with exactly: ok"),
            started_after_epoch: Some(1_700_000_000),
            poll_limit: 50,
        };
        let result = json!({
            "data": [
                {
                    "id": "persisted-id",
                    "status": "completed",
                    "startedAt": 1_700_000_001_i64,
                    "items": [
                        {
                            "id": "item-user",
                            "type": "userMessage",
                            "content": [{"type": "text", "text": "Reply with exactly: ok"}]
                        },
                        {
                            "id": "item-agent",
                            "type": "agentMessage",
                            "text": "ok"
                        }
                    ]
                }
            ]
        });

        let turn = poll_result_turn(&mut wait, &result).expect("aliased turn");

        assert_eq!(turn["id"], "persisted-id");
        assert_eq!(wait.turn_id, "persisted-id");
    }

    #[test]
    fn poll_result_turn_does_not_alias_to_older_repeated_prompt() {
        let target = Target {
            server: "work".to_string(),
            endpoint: Endpoint::Unix {
                path: PathBuf::from("/tmp/mock.sock"),
            },
            model: None,
            model_reasoning_effort: None,
        };
        let mut wait = TurnWaitContext {
            target: &target,
            thread_id: "thread-1",
            turn_id: "returned-id".to_string(),
            prompt: Some("repeat prompt"),
            started_after_epoch: Some(1_700_000_000),
            poll_limit: 50,
        };
        let result = json!({
            "data": [
                {
                    "id": "newest-other-turn",
                    "status": "completed",
                    "startedAt": 1_700_000_010_i64,
                    "items": [
                        {
                            "id": "item-user-new",
                            "type": "userMessage",
                            "content": [{"type": "text", "text": "different prompt"}]
                        }
                    ]
                },
                {
                    "id": "older-repeated-turn",
                    "status": "completed",
                    "startedAt": 1_699_999_000_i64,
                    "items": [
                        {
                            "id": "item-user-old",
                            "type": "userMessage",
                            "content": [{"type": "text", "text": "repeat prompt"}]
                        }
                    ]
                }
            ]
        });

        assert!(poll_result_turn(&mut wait, &result).is_none());
        assert_eq!(wait.turn_id, "returned-id");
    }
}
