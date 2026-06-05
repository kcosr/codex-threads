use anyhow::Result;
use serde_json::{Map, Value, json};

use crate::annotations::{load_annotation, namespace_annotations};
use crate::cli::{ItemsView, MessageRole, SortKey};
use crate::config::Target;
use crate::rpc::RpcClient;

#[derive(Debug, Clone, Copy)]
pub enum ThreadProjection {
    Direct,
    SearchResult,
}

#[derive(Debug)]
pub struct ListThreadsRequest {
    pub limit: u32,
    pub cursor: Option<String>,
    pub since: Option<i64>,
    pub cwd: Option<String>,
    pub archived: bool,
    pub sort: Option<SortKey>,
    pub asc: bool,
    pub desc: bool,
}

#[derive(Debug)]
pub struct SearchThreadsRequest {
    pub query: String,
    pub limit: u32,
    pub cursor: Option<String>,
    pub since: Option<i64>,
    pub archived: bool,
}

#[derive(Debug)]
pub struct ShowThreadRequest {
    pub thread_id: String,
    pub last: u32,
    pub cursor: Option<String>,
    pub asc: bool,
    pub desc: bool,
    pub items: ItemsView,
}

#[derive(Debug)]
pub struct MessagesRequest {
    pub thread_id: String,
    pub last: Option<usize>,
    pub since: Option<i64>,
    pub role: Option<MessageRole>,
    pub max_turns: u32,
}

#[derive(Debug)]
pub struct MessagesResult {
    pub output: Value,
    pub filtered_role: Option<MessageRole>,
}

#[derive(Debug)]
pub struct ThreadStatusRequest {
    pub thread_id: String,
    pub load: bool,
    pub turn_scan_limit: u32,
}

#[derive(Debug)]
pub struct LoadedStatusRequest {
    pub limit: u32,
}

pub async fn list_threads(
    target: &Target,
    client: &mut RpcClient,
    request: ListThreadsRequest,
) -> Result<Value> {
    let mut params = Map::new();
    insert_opt(&mut params, "cursor", request.cursor.clone());
    params.insert("limit".to_string(), json!(request.limit));
    if let Some(sort) = request.sort {
        params.insert("sortKey".to_string(), json!(sort_key(sort)));
    }
    params.insert(
        "sortDirection".to_string(),
        json!(direction(request.asc, request.desc)),
    );
    if request.archived {
        params.insert("archived".to_string(), json!(true));
    }
    if let Some(cwd) = request.cwd {
        params.insert("cwd".to_string(), json!(cwd));
    }
    let mut result = if let Some(since) = request.since {
        scan_since_filtered(
            client,
            "thread/list",
            params,
            request.cursor,
            request.limit,
            since,
            ThreadProjection::Direct,
        )
        .await?
    } else {
        client
            .request("thread/list", Value::Object(params), |_| {})
            .await?
    };
    attach_thread_annotations(target, &mut result, ThreadProjection::Direct)?;
    Ok(result)
}

pub async fn search_threads(
    target: &Target,
    client: &mut RpcClient,
    request: SearchThreadsRequest,
) -> Result<Value> {
    let mut params = Map::new();
    insert_opt(&mut params, "cursor", request.cursor.clone());
    params.insert("limit".to_string(), json!(request.limit));
    params.insert("searchTerm".to_string(), json!(request.query));
    if request.archived {
        params.insert("archived".to_string(), json!(true));
    }
    let mut result = if let Some(since) = request.since {
        scan_since_filtered(
            client,
            "thread/search",
            params,
            request.cursor,
            request.limit,
            since,
            ThreadProjection::SearchResult,
        )
        .await?
    } else {
        client
            .request("thread/search", Value::Object(params), |_| {})
            .await?
    };
    attach_thread_annotations(target, &mut result, ThreadProjection::SearchResult)?;
    Ok(result)
}

pub async fn read_thread_detail(
    target: &Target,
    client: &mut RpcClient,
    request: ShowThreadRequest,
) -> Result<Value> {
    let thread = client
        .request(
            "thread/read",
            json!({"threadId": request.thread_id, "includeTurns": false}),
            |_| {},
        )
        .await?;
    let turns = client
        .request(
            "thread/turns/list",
            json!({
                "threadId": request.thread_id,
                "cursor": request.cursor,
                "limit": request.last,
                "sortDirection": direction(request.asc, request.desc),
                "itemsView": items_view(request.items)
            }),
            |_| {},
        )
        .await?;
    let mut thread = thread["thread"].clone();
    attach_annotation_to_thread(target, &mut thread)?;
    Ok(json!({"server": target.server, "thread": thread, "turns": turns}))
}

pub async fn load_messages(
    target: &Target,
    client: &mut RpcClient,
    request: MessagesRequest,
) -> Result<MessagesResult> {
    let result = client
        .request(
            "thread/turns/list",
            json!({
                "threadId": request.thread_id,
                "limit": request.max_turns,
                "sortDirection": "desc",
                "itemsView": "full"
            }),
            |_| {},
        )
        .await?;
    let mut messages = flatten_messages(&result);
    if let Some(cutoff) = request.since {
        messages.retain(|m| {
            m["turnStartedAt"]
                .as_i64()
                .or_else(|| m["turnCompletedAt"].as_i64())
                .unwrap_or(0)
                >= cutoff
        });
    }
    let filtered_role = request.role;
    if let Some(role) = filtered_role.map(message_role_name) {
        messages.retain(|m| m["role"].as_str() == Some(role));
    }
    if let Some(last) = request.last
        && messages.len() > last
    {
        messages = messages.split_off(messages.len() - last);
    }
    let output = json!({
        "server": target.server,
        "threadId": request.thread_id,
        "messages": messages,
        "truncated": result["nextCursor"].is_string(),
        "nextCursor": result["nextCursor"].clone()
    });
    Ok(MessagesResult {
        output,
        filtered_role,
    })
}

pub async fn thread_status(
    target: &Target,
    client: &mut RpcClient,
    request: ThreadStatusRequest,
) -> Result<Value> {
    if request.load {
        let _ = resume_thread_for_inspection(client, &request.thread_id).await?;
    }
    let thread = client
        .request(
            "thread/read",
            json!({"threadId": request.thread_id, "includeTurns": false}),
            |_| {},
        )
        .await?;
    let turns = client
        .request(
            "thread/turns/list",
            json!({"threadId": request.thread_id, "limit": request.turn_scan_limit, "sortDirection": "desc", "itemsView": "notLoaded"}),
            |_| {},
        )
        .await?;
    let active_turn_id = turns["data"]
        .as_array()
        .and_then(|turns| turns.iter().find(|turn| turn_status(turn) == "inProgress"))
        .and_then(|turn| turn["id"].as_str())
        .map(str::to_string);
    Ok(
        json!({"server": target.server, "threadId": request.thread_id, "thread": thread["thread"], "activeTurnId": active_turn_id, "truncated": turns["nextCursor"].is_string()}),
    )
}

pub async fn loaded_status(
    target: &Target,
    client: &mut RpcClient,
    request: LoadedStatusRequest,
) -> Result<Value> {
    let loaded = client
        .request(
            "thread/loaded/list",
            json!({"limit": request.limit}),
            |_| {},
        )
        .await?;
    Ok(
        json!({"server": target.server, "reachable": true, "loadedThreadIds": loaded["data"], "nextCursor": loaded["nextCursor"]}),
    )
}

async fn scan_since_filtered(
    client: &mut RpcClient,
    method: &str,
    mut base_params: Map<String, Value>,
    mut cursor: Option<String>,
    limit: u32,
    since: i64,
    projection: ThreadProjection,
) -> Result<Value> {
    let mut data = Vec::new();
    let mut next_cursor = Value::Null;
    let mut backwards_cursor = Value::Null;
    let mut remaining = limit;

    base_params.remove("cursor");
    base_params.remove("limit");

    while remaining > 0 {
        let mut params = base_params.clone();
        insert_opt(&mut params, "cursor", cursor.clone());
        params.insert("limit".to_string(), json!(remaining));
        let page = client
            .request(method, Value::Object(params), |_| {})
            .await?;
        next_cursor = page["nextCursor"].clone();
        backwards_cursor = page["backwardsCursor"].clone();

        for item in page["data"].as_array().cloned().unwrap_or_default() {
            if thread_updated_at(&item, projection).unwrap_or(0) >= since {
                data.push(item);
                remaining -= 1;
                if remaining == 0 {
                    break;
                }
            }
        }

        let Some(next) = next_cursor.as_str().filter(|value| !value.is_empty()) else {
            break;
        };
        if cursor.as_deref() == Some(next) {
            break;
        }
        cursor = Some(next.to_string());
    }

    Ok(json!({
        "data": data,
        "nextCursor": next_cursor,
        "backwardsCursor": backwards_cursor
    }))
}

fn thread_updated_at(item: &Value, projection: ThreadProjection) -> Option<i64> {
    match projection {
        ThreadProjection::Direct => item["updatedAt"].as_i64(),
        ThreadProjection::SearchResult => item["thread"]["updatedAt"].as_i64(),
    }
}

fn attach_thread_annotations(
    target: &Target,
    result: &mut Value,
    projection: ThreadProjection,
) -> Result<()> {
    let annotations = namespace_annotations(target)?;
    if annotations.is_empty() {
        return Ok(());
    }
    let Some(items) = result["data"].as_array_mut() else {
        return Ok(());
    };
    for item in items {
        let Some(thread) = (match projection {
            ThreadProjection::Direct => Some(item),
            ThreadProjection::SearchResult => item.get_mut("thread"),
        }) else {
            continue;
        };
        if let Some(thread_id) = thread["id"].as_str()
            && let Some(annotation) = annotations.get(thread_id)
            && let Some(thread_object) = thread.as_object_mut()
        {
            thread_object.insert("annotation".to_string(), json!(annotation));
        }
    }
    Ok(())
}

fn attach_annotation_to_thread(target: &Target, thread: &mut Value) -> Result<()> {
    if let Some(thread_id) = thread["id"].as_str()
        && let Some(annotation) = load_annotation(target, thread_id)?
        && let Some(thread_object) = thread.as_object_mut()
    {
        thread_object.insert("annotation".to_string(), json!(annotation));
    }
    Ok(())
}

fn flatten_messages(turns: &Value) -> Vec<Value> {
    let mut out = Vec::new();
    for turn in turns["data"].as_array().unwrap_or(&Vec::new()).iter().rev() {
        for item in turn["items"].as_array().unwrap_or(&Vec::new()) {
            match item["type"].as_str() {
                Some("userMessage") => {
                    let text = item["content"]
                        .as_array()
                        .unwrap_or(&Vec::new())
                        .iter()
                        .filter_map(|input| input["text"].as_str())
                        .collect::<Vec<_>>()
                        .join("\n");
                    out.push(json!({"role": "user", "text": text, "turnId": turn["id"], "itemId": item["id"], "turnStartedAt": turn["startedAt"], "turnCompletedAt": turn["completedAt"]}));
                }
                Some("agentMessage") => {
                    out.push(json!({"role": "assistant", "text": item["text"], "turnId": turn["id"], "itemId": item["id"], "turnStartedAt": turn["startedAt"], "turnCompletedAt": turn["completedAt"]}));
                }
                _ => {}
            }
        }
    }
    out
}

async fn resume_thread_for_inspection(client: &mut RpcClient, thread_id: &str) -> Result<Value> {
    let result = client
        .request(
            "thread/resume",
            json!({"threadId": thread_id, "excludeTurns": true}),
            |_| {},
        )
        .await?;
    let _ = client
        .request("thread/unsubscribe", json!({"threadId": thread_id}), |_| {})
        .await;
    Ok(result)
}

fn insert_opt(map: &mut Map<String, Value>, key: &str, value: Option<String>) {
    if let Some(value) = value {
        map.insert(key.to_string(), json!(value));
    }
}

fn sort_key(sort: SortKey) -> &'static str {
    match sort {
        SortKey::Updated => "updated_at",
        SortKey::Created => "created_at",
    }
}

fn direction(asc: bool, desc: bool) -> &'static str {
    if asc {
        "asc"
    } else {
        let _desc_requested = desc;
        "desc"
    }
}

fn items_view(view: ItemsView) -> &'static str {
    match view {
        ItemsView::Summary => "summary",
        ItemsView::Full => "full",
        ItemsView::None => "notLoaded",
    }
}

fn message_role_name(role: MessageRole) -> &'static str {
    match role {
        MessageRole::User => "user",
        MessageRole::Assistant => "assistant",
    }
}

fn turn_status(turn: &Value) -> &'static str {
    match turn["status"].as_str().unwrap_or("inProgress") {
        "completed" => "completed",
        "interrupted" => "interrupted",
        "failed" => "failed",
        _ => "inProgress",
    }
}
