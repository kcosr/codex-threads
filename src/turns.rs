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

/// How long the watched turn must stay silent on the live subscription
/// before the fallback poll runs. While notifications for the turn are
/// flowing, no polls are issued at all. Override the default of 3 seconds
/// with `CODEX_THREADS_TURN_POLL_QUIET_SECS` (clamped to 1-300).
const TURN_POLL_QUIET_SECS_DEFAULT: u64 = 3;
const TURN_POLL_QUIET_SECS_ENV: &str = "CODEX_THREADS_TURN_POLL_QUIET_SECS";

fn turn_poll_quiet_duration() -> Duration {
    static QUIET: std::sync::OnceLock<Duration> = std::sync::OnceLock::new();
    *QUIET.get_or_init(|| {
        let seconds = std::env::var(TURN_POLL_QUIET_SECS_ENV)
            .ok()
            .and_then(|value| value.trim().parse::<u64>().ok())
            .map(|seconds| seconds.clamp(1, 300))
            .unwrap_or(TURN_POLL_QUIET_SECS_DEFAULT);
        Duration::from_secs(seconds)
    })
}

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
    assistant_seed: AssistantResponses,
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

/// One agent message of the watched turn.
///
/// Codex app-server identifies the same item differently per surface: live
/// notifications use opaque ids (`msg_<hash>`), while resume snapshots and
/// `thread/turns/list` renumber items (`item-N`). Entries therefore track
/// every id observed for the item and all emitted events carry the canonical
/// (first-seen) id, so downstream consumers never see one item under two ids.
#[derive(Debug, Clone)]
struct AssistantResponse {
    /// Canonical id used in emitted events: the id this item was first seen
    /// under (snapshot id when seeded, live id otherwise).
    item_id: Option<String>,
    /// Live notification id, when it differs from `item_id`.
    live_id: Option<String>,
    /// Persisted snapshot/poll id, when it differs from `item_id`.
    poll_id: Option<String>,
    text: String,
    /// While `Some`, a live delta stream may still be replaying the seeded
    /// snapshot text from the item start; tracks how many bytes matched.
    replay_cursor: Option<usize>,
}

impl AssistantResponse {
    fn new(item_id: Option<String>) -> Self {
        Self {
            item_id,
            live_id: None,
            poll_id: None,
            text: String::new(),
            replay_cursor: None,
        }
    }

    fn matches(&self, item_id: Option<&str>) -> bool {
        match item_id {
            Some(item_id) => {
                self.item_id.as_deref() == Some(item_id)
                    || self.live_id.as_deref() == Some(item_id)
                    || self.poll_id.as_deref() == Some(item_id)
            }
            None => self.item_id.is_none(),
        }
    }

    fn alias_ids(&self) -> Vec<String> {
        let mut ids = Vec::new();
        for id in [&self.item_id, &self.live_id, &self.poll_id]
            .into_iter()
            .flatten()
        {
            if !ids.contains(id) {
                ids.push(id.clone());
            }
        }
        ids
    }
}

/// The canonical id and full alias set to stamp on an emitted event.
#[derive(Debug, Clone)]
struct AssistantItemIds {
    item_id: Option<String>,
    alias_ids: Vec<String>,
}

impl AssistantResponses {
    fn contains_item(&self, item_id: Option<&str>) -> bool {
        self.find_index(item_id).is_some()
    }

    fn text_for_item(&self, item_id: Option<&str>) -> Option<&str> {
        self.find_index(item_id)
            .map(|index| self.items[index].text.as_str())
    }

    fn find_index(&self, item_id: Option<&str>) -> Option<usize> {
        self.items.iter().position(|item| item.matches(item_id))
    }

    /// Records that the live stream declared a new item; deltas for ids that
    /// were never declared belong to the item already in progress when the
    /// subscription started (see `resolve_live`).
    fn note_started(&mut self, item_id: &str) {
        if self.find_index(Some(item_id)).is_none() {
            self.items
                .push(AssistantResponse::new(Some(item_id.to_string())));
        }
    }

    /// Finds or creates the entry a live delta/completion with `item_id`
    /// refers to.
    fn resolve_live(&mut self, item_id: Option<&str>) -> usize {
        if let Some(index) = self.find_index(item_id) {
            return index;
        }
        if let Some(item_id) = item_id {
            // An identified event upgrades a previously anonymous entry.
            if let Some(index) = self
                .items
                .iter()
                .rposition(|item| item.item_id.is_none() && item.live_id.is_none())
            {
                self.items[index].item_id = Some(item_id.to_string());
                return index;
            }
            // A live id that was never declared via item/started continues
            // the snapshot-seeded item that was in progress at attach time.
            if let Some(index) = self
                .items
                .iter()
                .rposition(|item| item.live_id.is_none() && item.replay_cursor.is_some())
            {
                self.items[index].live_id = Some(item_id.to_string());
                return index;
            }
        }
        self.items
            .push(AssistantResponse::new(item_id.map(str::to_string)));
        self.items.len() - 1
    }

    /// Applies a live delta. Returns the event ids and the fragment to emit,
    /// or `None` when the delta only replayed already-known seeded text.
    fn apply_live_delta(
        &mut self,
        item_id: Option<&str>,
        delta: &str,
    ) -> Option<(AssistantItemIds, String)> {
        let index = self.resolve_live(item_id);
        let item = &mut self.items[index];
        let mut fragment = delta;
        if let Some(cursor) = item.replay_cursor {
            let remaining = item.text.get(cursor..).unwrap_or("");
            if !remaining.is_empty() && remaining.starts_with(delta) {
                let cursor = cursor + delta.len();
                item.replay_cursor = (cursor < item.text.len()).then_some(cursor);
                return None;
            }
            if !remaining.is_empty() && delta.starts_with(remaining) {
                fragment = &delta[remaining.len()..];
            }
            item.replay_cursor = None;
        }
        item.text.push_str(fragment);
        Some((
            AssistantItemIds {
                item_id: item.item_id.clone(),
                alias_ids: item.alias_ids(),
            },
            fragment.to_string(),
        ))
    }

    /// Applies an item/completed text. Returns the event ids when the
    /// completion carries content not yet emitted.
    fn complete_live(&mut self, item_id: Option<&str>, text: &str) -> Option<AssistantItemIds> {
        let was_known = self.contains_item(item_id);
        let index = self.resolve_live(item_id);
        let item = &mut self.items[index];
        let changed = item.text != text;
        item.text = text.to_string();
        item.replay_cursor = None;
        (!was_known || changed).then(|| AssistantItemIds {
            item_id: item.item_id.clone(),
            alias_ids: item.alias_ids(),
        })
    }

    #[cfg(test)]
    fn set_text(&mut self, item_id: Option<&str>, text: &str) {
        let index = self.resolve_live(item_id);
        self.items[index].text = text.to_string();
        self.items[index].replay_cursor = None;
    }

    /// Seeds one snapshot item during attach; order of calls must follow the
    /// item order within the turn.
    fn seed_snapshot_item(&mut self, item_id: Option<&str>, text: &str) {
        let mut item = AssistantResponse::new(item_id.map(str::to_string));
        item.text = text.to_string();
        item.replay_cursor = Some(0);
        self.items.push(item);
    }

    /// Reconciles a polled turn snapshot. Poll items are joined to known
    /// entries by id alias or by position within the turn (both surfaces list
    /// the turn's agent messages in creation order), so an item streamed live
    /// as `msg_<hash>` is not re-emitted when the poll lists it as `item-N`.
    fn sync_from_turn(&mut self, turn: &Value) -> Vec<(AssistantItemIds, String)> {
        let mut updates = Vec::new();
        let mut position = 0;
        for item in turn["items"].as_array().unwrap_or(&Vec::new()) {
            if item["type"].as_str() != Some("agentMessage") {
                continue;
            }
            let poll_id = item["id"].as_str();
            let text = item["text"].as_str().unwrap_or("");
            let index = self.find_index(poll_id).or_else(|| {
                self.items
                    .get(position)
                    .filter(|item| match (poll_id, item.poll_id.as_deref()) {
                        (Some(_), None) => true,
                        (Some(poll_id), Some(known)) => poll_id == known,
                        (None, _) => false,
                    })
                    .map(|_| position)
            });
            match index {
                Some(index) => {
                    let item = &mut self.items[index];
                    if let Some(poll_id) = poll_id
                        && item.item_id.as_deref() != Some(poll_id)
                        && item.poll_id.is_none()
                    {
                        item.poll_id = Some(poll_id.to_string());
                    }
                    if !text.is_empty() && item.text != text {
                        item.text = text.to_string();
                        if item.replay_cursor.is_some() {
                            item.replay_cursor = Some(0);
                        }
                        updates.push((
                            AssistantItemIds {
                                item_id: item.item_id.clone(),
                                alias_ids: item.alias_ids(),
                            },
                            text.to_string(),
                        ));
                    }
                }
                None => {
                    let mut entry = AssistantResponse::new(poll_id.map(str::to_string));
                    entry.text = text.to_string();
                    let ids = AssistantItemIds {
                        item_id: entry.item_id.clone(),
                        alias_ids: entry.alias_ids(),
                    };
                    self.items.push(entry);
                    if !text.is_empty() {
                        updates.push((ids, text.to_string()));
                    }
                }
            }
            position += 1;
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
}

/// Builds the assistant accumulator state implied by a `thread/resume`
/// snapshot, so that turn waiting starts from the same view of the turn the
/// snapshot describes instead of an empty one. Without this, the first poll
/// re-emits the full text of every item already present in the snapshot.
#[cfg(feature = "tui")]
fn assistant_seed_from_thread_snapshot(thread: &Value, turn_id: &str) -> AssistantResponses {
    let mut assistant = AssistantResponses::default();
    for turn in thread["turns"].as_array().unwrap_or(&Vec::new()) {
        if turn["id"].as_str() != Some(turn_id) {
            continue;
        }
        for item in turn["items"].as_array().unwrap_or(&Vec::new()) {
            if item["type"].as_str() != Some("agentMessage") {
                continue;
            }
            // Empty items are seeded too: positions within the turn align
            // poll snapshots with live entries, so gaps would mismatch them.
            assistant.seed_snapshot_item(item["id"].as_str(), item["text"].as_str().unwrap_or(""));
        }
    }
    assistant
}

/// Drops or trims agent-message deltas that were buffered while the
/// `thread/resume` request was in flight and whose content the resume
/// snapshot already includes. The server generates the snapshot after sending
/// those deltas, so replaying them verbatim duplicates text downstream.
///
/// Within one item the buffered deltas are contiguous, so their concatenation
/// overlaps the snapshot text's tail by exactly the already-included portion.
/// After this pass, every delta that flows downstream is an exact, new
/// continuation of the text emitted so far.
#[cfg(feature = "tui")]
fn reconcile_replayed_deltas(
    assistant: &AssistantResponses,
    thread_id: &str,
    turn_id: &str,
    notifications: Vec<Notification>,
) -> Vec<Notification> {
    let is_replayed_delta = |notification: &Notification| {
        notification.method == "item/agentMessage/delta"
            && notification.params["threadId"] == thread_id
            && notification.params["turnId"] == turn_id
    };
    // Live ids that the buffered window itself declares as new items; deltas
    // for undeclared live ids continue the snapshot's in-progress tail item
    // under a different id namespace (live `msg_<hash>` vs snapshot `item-N`).
    let started_in_buffer: Vec<String> = notifications
        .iter()
        .filter(|notification| {
            notification.method == "item/started"
                && notification.params["threadId"] == thread_id
                && notification.params["turnId"] == turn_id
                && notification.params["item"]["type"].as_str() == Some("agentMessage")
        })
        .filter_map(|notification| notification.params["item"]["id"].as_str())
        .map(str::to_string)
        .collect();
    let seed_tail_id = assistant
        .items
        .last()
        .filter(|item| item.replay_cursor.is_some())
        .and_then(|item| item.item_id.clone());
    let resolve = |item_id: Option<&str>| -> Option<String> {
        let Some(item_id) = item_id else {
            return seed_tail_id.clone();
        };
        if assistant.contains_item(Some(item_id)) {
            return Some(item_id.to_string());
        }
        if started_in_buffer.iter().any(|id| id == item_id) {
            return Some(item_id.to_string());
        }
        seed_tail_id.clone().or(Some(item_id.to_string()))
    };
    let mut replayed: Vec<(Option<String>, String, usize)> = Vec::new();
    for notification in notifications.iter().filter(|n| is_replayed_delta(n)) {
        let item_id = resolve(notification.params["itemId"].as_str());
        let delta = notification.params["delta"].as_str().unwrap_or("");
        match replayed.iter_mut().find(|(id, _, _)| *id == item_id) {
            Some((_, text, _)) => text.push_str(delta),
            None => replayed.push((item_id, delta.to_string(), 0)),
        }
    }
    for (item_id, text, skip) in &mut replayed {
        let known = assistant.text_for_item(item_id.as_deref()).unwrap_or("");
        *skip = replayed_prefix_len(known, text);
    }
    if crate::debuglog::enabled() {
        let items = replayed
            .iter()
            .map(|(item_id, text, skip)| {
                json!({
                    "itemId": item_id,
                    "knownLen": assistant
                        .text_for_item(item_id.as_deref())
                        .map(str::len)
                        .unwrap_or(0),
                    "replayedLen": text.len(),
                    "trimmedLen": skip,
                })
            })
            .collect::<Vec<_>>();
        crate::debuglog::log(
            "attach-reconcile",
            None,
            json!({
                "threadId": thread_id,
                "turnId": turn_id,
                "bufferedNotifications": notifications.len(),
                "items": items,
            }),
        );
    }

    let mut consumed: Vec<(Option<String>, usize)> = Vec::new();
    let mut out = Vec::with_capacity(notifications.len());
    for mut notification in notifications {
        if !is_replayed_delta(&notification) {
            out.push(notification);
            continue;
        }
        let item_id = resolve(notification.params["itemId"].as_str());
        let delta_len = notification.params["delta"].as_str().unwrap_or("").len();
        let skip = replayed
            .iter()
            .find(|(id, _, _)| *id == item_id)
            .map(|(_, _, skip)| *skip)
            .unwrap_or(0);
        let start = match consumed.iter_mut().find(|(id, _)| *id == item_id) {
            Some((_, position)) => {
                let start = *position;
                *position += delta_len;
                start
            }
            None => {
                consumed.push((item_id, delta_len));
                0
            }
        };
        if start + delta_len <= skip {
            continue;
        }
        if start < skip {
            let trimmed = notification.params["delta"]
                .as_str()
                .map(|delta| delta[skip - start..].to_string())
                .unwrap_or_default();
            notification.params["delta"] = json!(trimmed);
        }
        out.push(notification);
    }
    out
}

/// Longest prefix of `replayed` that is also a suffix of `existing`, measured
/// in bytes at a char boundary of `replayed`.
#[cfg(feature = "tui")]
fn replayed_prefix_len(existing: &str, replayed: &str) -> usize {
    let max = existing.len().min(replayed.len());
    replayed
        .char_indices()
        .map(|(index, _)| index)
        .chain(std::iter::once(replayed.len()))
        .filter(|length| *length <= max)
        .rev()
        .find(|length| existing.ends_with(&replayed[..*length]))
        .unwrap_or(0)
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
        json!({"type": "accepted", "server": target.server, "threadId": thread_id, "turnId": result["turnId"].as_str().unwrap_or(&turn_id), "status": "accepted", "prompt": prompt}),
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
    let assistant_seed = assistant_seed_from_thread_snapshot(&resume["thread"], &options.turn_id);
    if crate::debuglog::enabled() {
        let items = assistant_seed
            .items
            .iter()
            .map(|item| json!({"itemId": item.item_id, "textLen": item.text.len()}))
            .collect::<Vec<_>>();
        crate::debuglog::log(
            "attach-seed",
            None,
            json!({
                "threadId": options.thread_id,
                "turnId": options.turn_id,
                "items": items,
            }),
        );
    }
    let early_notifications = reconcile_replayed_deltas(
        &assistant_seed,
        &options.thread_id,
        &options.turn_id,
        early_notifications,
    );
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
            assistant_seed,
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
    let mut assistant = started.assistant_seed;
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
    // The live subscription is the primary transport; polling is only the
    // fallback for turns whose notifications stop arriving (or never match,
    // e.g. when turn/start returned a temporary turn id).
    let mut last_turn_evidence = std::time::Instant::now();
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
                        last_turn_evidence = std::time::Instant::now();
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
                }
            }
            notification = client.next_notification_or_request() => {
                let notification = notification?;
                if notification_concerns_turn(&wait, &notification) {
                    last_turn_evidence = std::time::Instant::now();
                }
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
                if last_turn_evidence.elapsed() < turn_poll_quiet_duration() {
                    continue;
                }
                let before_len = events.len();
                let terminal = poll_turn_completion(
                    client,
                    &mut wait,
                    &mut assistant,
                    &mut events,
                    &mut on_assistant_text_from_poll,
                ).await?;
                last_turn_evidence = std::time::Instant::now();
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

/// Whether a notification proves the live subscription still delivers
/// traffic for the watched turn. Notifications for other turns do not count:
/// they leave open the possibility that `wait.turn_id` is a stale or
/// temporary id whose real turn only the fallback poll can re-align.
fn notification_concerns_turn(wait: &TurnWaitContext<'_>, notification: &Notification) -> bool {
    let params = &notification.params;
    params["threadId"] == wait.thread_id
        && (params["turnId"] == wait.turn_id.as_str()
            || params["turn"]["id"] == wait.turn_id.as_str())
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
        assistant_seed: AssistantResponses::default(),
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
    let mut assistant = started.assistant_seed;
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
    // See wait_for_turn_controlled: polls back off while turn notifications
    // are flowing on the live subscription.
    let mut last_turn_evidence = std::time::Instant::now();
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
                if notification_concerns_turn(&wait, &notification) {
                    last_turn_evidence = std::time::Instant::now();
                }
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
                if last_turn_evidence.elapsed() < turn_poll_quiet_duration() {
                    continue;
                }
                let before_len = events.len();
                let terminal = poll_turn_completion(
                    client,
                    &mut wait,
                    &mut assistant,
                    &mut events,
                    &mut on_assistant_text_from_poll,
                ).await?;
                last_turn_evidence = std::time::Instant::now();
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
    for (ids, text) in updates {
        let mut event = Map::new();
        event.insert("type".to_string(), json!("progress"));
        event.insert("server".to_string(), json!(wait.target.server));
        event.insert("threadId".to_string(), json!(wait.thread_id));
        event.insert("turnId".to_string(), json!(&wait.turn_id));
        insert_item_ids(&mut event, &ids);
        event.insert("text".to_string(), json!(text));
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
        "item/started"
            if notification.params["threadId"] == thread_id
                && notification.params["turnId"] == turn_id =>
        {
            if notification.params["item"]["type"].as_str() == Some("agentMessage")
                && let Some(item_id) = notification.params["item"]["id"].as_str()
            {
                assistant.note_started(item_id);
            }
            Ok(None)
        }
        "item/agentMessage/delta"
            if notification.params["threadId"] == thread_id
                && notification.params["turnId"] == turn_id =>
        {
            let delta = notification.params["delta"].as_str().unwrap_or("");
            let item_id = notification.params["itemId"].as_str();
            let Some((ids, fragment)) = assistant.apply_live_delta(item_id, delta) else {
                return Ok(None);
            };
            let mut event = Map::new();
            event.insert("type".to_string(), json!("progress"));
            event.insert("server".to_string(), json!(server));
            event.insert("threadId".to_string(), json!(thread_id));
            event.insert("turnId".to_string(), json!(turn_id));
            insert_item_ids(&mut event, &ids);
            event.insert("delta".to_string(), json!(fragment));
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
                if let Some(ids) = assistant.complete_live(item_id, text) {
                    let mut event = Map::new();
                    event.insert("type".to_string(), json!("assistantMessage"));
                    event.insert("server".to_string(), json!(server));
                    event.insert("threadId".to_string(), json!(thread_id));
                    event.insert("turnId".to_string(), json!(turn_id));
                    insert_item_ids(&mut event, &ids);
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

/// Stamps the canonical item id plus, when the item is known under several
/// server ids, the full alias list consumers can match against.
fn insert_item_ids(map: &mut Map<String, Value>, ids: &AssistantItemIds) {
    insert_opt(map, "itemId", ids.item_id.clone());
    if ids.alias_ids.len() > 1 {
        map.insert("itemAliases".to_string(), json!(ids.alias_ids));
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

    #[cfg(feature = "tui")]
    fn delta_notification(item_id: &str, delta: &str) -> Notification {
        Notification {
            method: "item/agentMessage/delta".to_string(),
            params: json!({
                "threadId": "thread-1",
                "turnId": "turn-1",
                "itemId": item_id,
                "delta": delta
            }),
        }
    }

    #[cfg(feature = "tui")]
    fn replayed_delta_texts(notifications: &[Notification]) -> Vec<String> {
        notifications
            .iter()
            .map(|notification| {
                notification.params["delta"]
                    .as_str()
                    .unwrap_or("")
                    .to_string()
            })
            .collect()
    }

    #[test]
    fn poll_snapshot_does_not_reemit_live_streamed_item_under_persisted_id() {
        // Codex names the same item `msg_<hash>` in live notifications but
        // `item-N` in thread snapshots; the poll must not re-emit text that
        // already streamed live under the other id.
        let mut assistant = AssistantResponses::default();
        assistant.note_started("msg_a");
        assert!(
            assistant
                .apply_live_delta(Some("msg_a"), "The CLI also ")
                .is_some()
        );
        assert!(
            assistant
                .apply_live_delta(Some("msg_a"), "exposes this.")
                .is_some()
        );
        assert!(
            assistant
                .complete_live(Some("msg_a"), "The CLI also exposes this.")
                .is_none()
        );

        let turn = json!({"items": [
            {"id": "item-3", "type": "agentMessage", "text": "The CLI also exposes this."}
        ]});
        assert!(assistant.sync_from_turn(&turn).is_empty());
        assert_eq!(
            assistant.text_for_item(Some("item-3")),
            Some("The CLI also exposes this.")
        );
    }

    #[test]
    fn undeclared_live_id_continues_seeded_snapshot_item() {
        let mut assistant = AssistantResponses::default();
        assistant.seed_snapshot_item(Some("item-7"), "Partial sn");

        // The live stream replays the item from its start under a live id
        // that was never declared via item/started.
        assert!(
            assistant
                .apply_live_delta(Some("msg_b"), "Partial ")
                .is_none()
        );
        let (ids, fragment) = assistant
            .apply_live_delta(Some("msg_b"), "snapshot text continues")
            .expect("boundary delta carries fresh tail");
        assert_eq!(ids.item_id.as_deref(), Some("item-7"));
        assert!(ids.alias_ids.contains(&"msg_b".to_string()));
        assert_eq!(fragment, "apshot text continues");
        assert_eq!(
            assistant.text_for_item(Some("item-7")),
            Some("Partial snapshot text continues")
        );
    }

    #[test]
    fn declared_live_item_stays_separate_from_seeded_tail() {
        let mut assistant = AssistantResponses::default();
        assistant.seed_snapshot_item(Some("item-7"), "Earlier paragraph.");
        assistant.note_started("msg_c");
        let (ids, fragment) = assistant
            .apply_live_delta(Some("msg_c"), "New paragraph.")
            .expect("new item delta emits");
        assert_eq!(ids.item_id.as_deref(), Some("msg_c"));
        assert_eq!(fragment, "New paragraph.");
        assert_eq!(
            assistant.text_for_item(Some("item-7")),
            Some("Earlier paragraph.")
        );

        // The poll lists both items under persisted ids: positional join
        // registers aliases without re-emitting.
        let unchanged = json!({"items": [
            {"id": "item-7", "type": "agentMessage", "text": "Earlier paragraph."},
            {"id": "item-8", "type": "agentMessage", "text": "New paragraph."}
        ]});
        assert!(assistant.sync_from_turn(&unchanged).is_empty());

        // Later growth is emitted once, under the live id plus aliases.
        let advanced = json!({"items": [
            {"id": "item-7", "type": "agentMessage", "text": "Earlier paragraph."},
            {"id": "item-8", "type": "agentMessage", "text": "New paragraph. More."}
        ]});
        let updates = assistant.sync_from_turn(&advanced);
        assert_eq!(updates.len(), 1);
        assert_eq!(updates[0].0.item_id.as_deref(), Some("msg_c"));
        assert!(updates[0].0.alias_ids.contains(&"item-8".to_string()));
        assert_eq!(updates[0].1, "New paragraph. More.");
    }

    #[cfg(feature = "tui")]
    #[test]
    fn reconcile_replayed_deltas_drops_content_already_in_snapshot() {
        // The item streamed as "The full" + " paragraph" + " with live" +
        // " suffix". The resume snapshot was generated after the third delta;
        // the deltas in flight during the resume RPC are buffered and would
        // otherwise replay content the snapshot already includes.
        let mut assistant = AssistantResponses::default();
        assistant.set_text(Some("assistant-1"), "The full paragraph with live");

        let reconciled = reconcile_replayed_deltas(
            &assistant,
            "thread-1",
            "turn-1",
            vec![
                delta_notification("assistant-1", " paragraph"),
                delta_notification("assistant-1", " with live"),
                delta_notification("assistant-1", " suffix"),
            ],
        );

        assert_eq!(replayed_delta_texts(&reconciled), vec![" suffix"]);
    }

    #[cfg(feature = "tui")]
    #[test]
    fn reconcile_trims_buffered_deltas_for_undeclared_live_id() {
        // Buffered deltas arrive under a live id while the snapshot seeded
        // the same in-progress item under its persisted id.
        let mut assistant = AssistantResponses::default();
        assistant.seed_snapshot_item(Some("item-7"), "The full paragraph with live");

        let reconciled = reconcile_replayed_deltas(
            &assistant,
            "thread-1",
            "turn-1",
            vec![
                delta_notification("msg_z", " paragraph"),
                delta_notification("msg_z", " with live"),
                delta_notification("msg_z", " suffix"),
            ],
        );

        assert_eq!(replayed_delta_texts(&reconciled), vec![" suffix"]);
    }

    #[cfg(feature = "tui")]
    #[test]
    fn reconcile_keeps_buffered_deltas_for_declared_new_item() {
        let mut assistant = AssistantResponses::default();
        assistant.seed_snapshot_item(Some("item-7"), "Earlier paragraph.");

        let started = Notification {
            method: "item/started".to_string(),
            params: json!({
                "threadId": "thread-1",
                "turnId": "turn-1",
                "item": {"id": "msg_y", "type": "agentMessage"}
            }),
        };
        let reconciled = reconcile_replayed_deltas(
            &assistant,
            "thread-1",
            "turn-1",
            vec![started, delta_notification("msg_y", "Earlier paragraph.")],
        );

        assert_eq!(reconciled.len(), 2);
        assert_eq!(reconciled[1].params["delta"], "Earlier paragraph.");
    }

    #[cfg(feature = "tui")]
    #[test]
    fn reconcile_replayed_deltas_drops_replay_from_item_start() {
        let mut assistant = AssistantResponses::default();
        assistant.set_text(Some("assistant-1"), "Hello");

        let reconciled = reconcile_replayed_deltas(
            &assistant,
            "thread-1",
            "turn-1",
            vec![
                delta_notification("assistant-1", "Hel"),
                delta_notification("assistant-1", "lo"),
                delta_notification("assistant-1", " world"),
            ],
        );

        assert_eq!(replayed_delta_texts(&reconciled), vec![" world"]);
    }

    #[cfg(feature = "tui")]
    #[test]
    fn reconcile_replayed_deltas_trims_delta_spanning_snapshot_boundary() {
        let mut assistant = AssistantResponses::default();
        assistant.set_text(Some("assistant-1"), "AB");

        let reconciled = reconcile_replayed_deltas(
            &assistant,
            "thread-1",
            "turn-1",
            vec![
                delta_notification("assistant-1", "A"),
                delta_notification("assistant-1", "BC"),
            ],
        );

        assert_eq!(replayed_delta_texts(&reconciled), vec!["C"]);
    }

    #[cfg(feature = "tui")]
    #[test]
    fn reconcile_replayed_deltas_trims_at_multibyte_boundaries() {
        let mut assistant = AssistantResponses::default();
        assistant.set_text(Some("assistant-1"), "héllo wö");

        let reconciled = reconcile_replayed_deltas(
            &assistant,
            "thread-1",
            "turn-1",
            vec![
                delta_notification("assistant-1", "héllo"),
                delta_notification("assistant-1", " wörld"),
            ],
        );

        assert_eq!(replayed_delta_texts(&reconciled), vec!["rld"]);
    }

    #[cfg(feature = "tui")]
    #[test]
    fn reconcile_replayed_deltas_keeps_unknown_items_and_other_threads() {
        let mut assistant = AssistantResponses::default();
        assistant.set_text(Some("assistant-1"), "known text");

        let other_thread = Notification {
            method: "item/agentMessage/delta".to_string(),
            params: json!({
                "threadId": "thread-2",
                "turnId": "turn-1",
                "itemId": "assistant-1",
                "delta": "known text"
            }),
        };
        let completed = Notification {
            method: "item/completed".to_string(),
            params: json!({
                "threadId": "thread-1",
                "turnId": "turn-1",
                "item": {"id": "assistant-1", "type": "agentMessage", "text": "known text"}
            }),
        };
        let reconciled = reconcile_replayed_deltas(
            &assistant,
            "thread-1",
            "turn-1",
            vec![
                delta_notification("assistant-2", "new item text"),
                other_thread,
                completed,
            ],
        );

        assert_eq!(reconciled.len(), 3);
        assert_eq!(reconciled[0].params["delta"], "new item text");
        assert_eq!(reconciled[1].params["threadId"], "thread-2");
        assert_eq!(reconciled[1].params["delta"], "known text");
        assert_eq!(reconciled[2].method, "item/completed");
    }

    #[cfg(feature = "tui")]
    #[test]
    fn assistant_seed_from_snapshot_suppresses_poll_rebroadcast() {
        let thread = json!({
            "turns": [
                {
                    "id": "turn-1",
                    "items": [
                        {"id": "user-1", "type": "userMessage", "content": [{"text": "go"}]},
                        {"id": "assistant-1", "type": "agentMessage", "text": "First paragraph"},
                        {"id": "assistant-2", "type": "agentMessage", "text": "Second part"}
                    ]
                },
                {
                    "id": "turn-0",
                    "items": [
                        {"id": "assistant-0", "type": "agentMessage", "text": "Older turn"}
                    ]
                }
            ]
        });
        let mut assistant = assistant_seed_from_thread_snapshot(&thread, "turn-1");
        assert_eq!(
            assistant.text_for_item(Some("assistant-1")),
            Some("First paragraph")
        );
        assert_eq!(assistant.text_for_item(Some("assistant-0")), None);

        // Polling the same state right after attaching must not re-emit the
        // items the snapshot already delivered.
        let unchanged = json!({
            "id": "turn-1",
            "items": [
                {"id": "assistant-1", "type": "agentMessage", "text": "First paragraph"},
                {"id": "assistant-2", "type": "agentMessage", "text": "Second part"}
            ]
        });
        assert!(assistant.sync_from_turn(&unchanged).is_empty());

        let advanced = json!({
            "id": "turn-1",
            "items": [
                {"id": "assistant-1", "type": "agentMessage", "text": "First paragraph"},
                {"id": "assistant-2", "type": "agentMessage", "text": "Second part grew"}
            ]
        });
        let updates = assistant.sync_from_turn(&advanced);
        assert_eq!(updates.len(), 1);
        assert_eq!(updates[0].0.item_id.as_deref(), Some("assistant-2"));
        assert_eq!(updates[0].1, "Second part grew");
    }

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
        assert_eq!(updates[0].0.item_id.as_deref(), Some("assistant-1"));
        assert_eq!(updates[0].1, "current active text");
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
