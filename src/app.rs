use std::cell::RefCell;
use std::ffi::OsString;
use std::io::{self, Write};

use anyhow::{Context, Result};
use clap::Parser;
use serde_json::{Map, Value, json};

use crate::cli::*;
use crate::config::{
    AppConfig, Target, is_valid_reasoning_effort, load_config, resolve_config_path, resolve_target,
};
use crate::rpc::{Notification, RpcClient, RpcRequestError};

const DEFAULT_LIST_LIMIT: u32 = 50;
const DEFAULT_SHOW_LAST: u32 = 20;
const TURN_SCAN_LIMIT: u32 = 200;
const TURN_WAIT_TIMEOUT_SECS: u64 = 60 * 60;
const THREAD_LABEL_WIDTH: usize = 56;
const SEARCH_SNIPPET_WIDTH: usize = 48;

#[derive(Debug, thiserror::Error)]
#[error("{message}")]
struct ExitError {
    code: i32,
    message: String,
}

pub async fn run_cli<I, T>(args: I) -> i32
where
    I: IntoIterator<Item = T>,
    T: Into<OsString> + Clone,
{
    let cli = match Cli::try_parse_from(args) {
        Ok(cli) => cli,
        Err(err) => {
            let _ = err.print();
            return err.exit_code();
        }
    };

    match run(cli).await {
        Ok(code) => code,
        Err(err) => {
            eprintln!("error: {err:#}");
            classify_error(&err)
        }
    }
}

async fn run(cli: Cli) -> Result<i32> {
    let config_path = resolve_config_path(cli.config.clone());
    let yolo = !cli.no_yolo;
    if let Command::Servers(command) = &cli.command {
        return servers_command(&config_path, cli.connect.as_deref(), command).await;
    }
    let config = if cli.connect.is_some() {
        AppConfig::default()
    } else {
        load_config(&config_path)?
    };
    match cli.command {
        Command::Servers(_) => unreachable!(),
        Command::List(command) => {
            with_client(
                &config,
                cli.connect.as_deref(),
                command.server.server.clone(),
                |target, client| async move { list_command(target, client, command).await },
            )
            .await
        }
        Command::Search(command) => {
            with_client(
                &config,
                cli.connect.as_deref(),
                command.server.server.clone(),
                |target, client| async move { search_command(target, client, command).await },
            )
            .await
        }
        Command::Show(command) => {
            with_client(
                &config,
                cli.connect.as_deref(),
                command.server.server.clone(),
                |target, client| async move { show_command(target, client, command).await },
            )
            .await
        }
        Command::Messages(command) => {
            with_client(
                &config,
                cli.connect.as_deref(),
                command.server.server.clone(),
                |target, client| async move { messages_command(target, client, command).await },
            )
            .await
        }
        Command::New(command) => {
            with_client(
                &config,
                cli.connect.as_deref(),
                command.server.server.clone(),
                |target, client| async move { new_command(target, client, command, yolo).await },
            )
            .await
        }
        Command::Send(command) => {
            with_client(
                &config,
                cli.connect.as_deref(),
                command.server.server.clone(),
                |target, client| async move { send_command(target, client, command, yolo).await },
            )
            .await
        }
        Command::Settings(command) => {
            match command.command {
                SettingsSubcommand::Show(command) => {
                    with_client(
                        &config,
                        cli.connect.as_deref(),
                        command.server.server.clone(),
                        |target, client| async move {
                            settings_show_command(target, client, command).await
                        },
                    )
                    .await
                }
                SettingsSubcommand::Set(command) => {
                    with_client(
                        &config,
                        cli.connect.as_deref(),
                        command.server.server.clone(),
                        |target, client| async move {
                            settings_set_command(target, client, command, yolo).await
                        },
                    )
                    .await
                }
            }
        }
        Command::Status(command) => {
            with_client(
                &config,
                cli.connect.as_deref(),
                command.server.server.clone(),
                |target, client| async move { status_command(target, client, command).await },
            )
            .await
        }
        Command::Steer(command) => {
            with_client(
                &config,
                cli.connect.as_deref(),
                command.server.server.clone(),
                |target, client| async move { steer_command(target, client, command, yolo).await },
            )
            .await
        }
        Command::Interrupt(command) => {
            with_client(
                &config,
                cli.connect.as_deref(),
                command.server.server.clone(),
                |target, client| async move { interrupt_command(target, client, command).await },
            )
            .await
        }
        Command::Name(command) => {
            with_client(
                &config,
                cli.connect.as_deref(),
                command.server.server.clone(),
                |target, client| async move { name_command(target, client, command).await },
            )
            .await
        }
        Command::Archive(command) => with_client(
            &config,
            cli.connect.as_deref(),
            command.server.server.clone(),
            |target, client| async move { archive_command(target, client, command, true).await },
        )
        .await,
        Command::Unarchive(command) => with_client(
            &config,
            cli.connect.as_deref(),
            command.server.server.clone(),
            |target, client| async move { archive_command(target, client, command, false).await },
        )
        .await,
        Command::Models(command) => {
            with_client(
                &config,
                cli.connect.as_deref(),
                command.server.server.clone(),
                |target, client| async move { models_command(target, client, command).await },
            )
            .await
        }
        Command::Goal(command) => match command.command {
            GoalSubcommand::Get(command) => {
                with_client(
                    &config,
                    cli.connect.as_deref(),
                    command.server.server.clone(),
                    |target, client| async move { goal_get_command(target, client, command).await },
                )
                .await
            }
            GoalSubcommand::Set(command) => {
                with_client(
                    &config,
                    cli.connect.as_deref(),
                    command.server.server.clone(),
                    |target, client| async move { goal_set_command(target, client, command).await },
                )
                .await
            }
            GoalSubcommand::Clear(command) => with_client(
                &config,
                cli.connect.as_deref(),
                command.server.server.clone(),
                |target, client| async move { goal_clear_command(target, client, command).await },
            )
            .await,
        },
    }
}

async fn with_client<F, Fut>(
    config: &AppConfig,
    connect: Option<&str>,
    server: Option<String>,
    f: F,
) -> Result<i32>
where
    F: FnOnce(Target, RpcClient) -> Fut,
    Fut: std::future::Future<Output = Result<i32>>,
{
    let target = resolve_target(config, connect, server.as_deref())?;
    let client = RpcClient::connect(&target.path).await?;
    f(target, client).await
}

async fn servers_command(
    config_path: &std::path::Path,
    connect: Option<&str>,
    command: &ServersCommand,
) -> Result<i32> {
    let config = if connect.is_some() {
        AppConfig::default()
    } else {
        load_config(config_path)?
    };
    match &command.command {
        None => {
            let rows: Vec<_> = config
                .servers
                .iter()
                .map(|(alias, server)| json!({"alias": alias, "type": server.kind, "path": server.path}))
                .collect();
            if command.json {
                print_json(&json!({ "servers": rows }))?;
            } else {
                print_table(
                    &["ALIAS", "TYPE", "PATH"],
                    rows.iter()
                        .map(|row| {
                            vec![
                                table_cell(row["alias"].as_str().unwrap_or("")),
                                table_cell(row["type"].as_str().unwrap_or("")),
                                table_cell(row["path"].as_str().unwrap_or("")),
                            ]
                        })
                        .collect(),
                );
            }
            Ok(0)
        }
        Some(ServersSubcommand::Ping(ping)) => {
            if connect.is_some() && ping.all {
                return Err(usage_error(
                    "--connect cannot be combined with servers ping --all",
                ));
            }
            let targets = if ping.all {
                config
                    .servers
                    .iter()
                    .map(|(server, cfg)| Target::configured(server, cfg, &config))
                    .collect::<Vec<_>>()
            } else {
                vec![resolve_target(&config, connect, ping.server.as_deref())?]
            };
            let mut results = Vec::new();
            for target in targets {
                let ok = RpcClient::connect(&target.path).await.is_ok();
                results.push(json!({"server": target.server, "ok": ok}));
            }
            if ping.json {
                print_json(&json!({"servers": results}))?;
            } else {
                print_table(
                    &["SERVER", "STATUS"],
                    results
                        .iter()
                        .map(|row| {
                            vec![
                                table_cell(row["server"].as_str().unwrap_or("")),
                                table_cell(if row["ok"].as_bool() == Some(true) {
                                    "ok"
                                } else {
                                    "error"
                                }),
                            ]
                        })
                        .collect(),
                );
            }
            Ok(if results.iter().all(|r| r["ok"].as_bool() == Some(true)) {
                0
            } else {
                3
            })
        }
    }
}

async fn list_command(target: Target, mut client: RpcClient, command: ListCommand) -> Result<i32> {
    let since = command.since.as_deref().map(parse_since).transpose()?;
    let mut params = Map::new();
    insert_opt(&mut params, "cursor", command.cursor.clone());
    let limit = command.limit.unwrap_or(DEFAULT_LIST_LIMIT);
    params.insert("limit".to_string(), json!(limit));
    if let Some(sort) = command.sort {
        params.insert("sortKey".to_string(), json!(sort_key(sort)));
    }
    params.insert(
        "sortDirection".to_string(),
        json!(direction(command.asc, command.desc)),
    );
    if command.archived {
        params.insert("archived".to_string(), json!(true));
    }
    if let Some(cwd) = command.cwd {
        params.insert("cwd".to_string(), json!(cwd));
    }
    let result = if let Some(since) = since {
        scan_since_filtered(
            &mut client,
            "thread/list",
            params,
            command.cursor,
            limit,
            since,
            ThreadProjection::Direct,
        )
        .await?
    } else {
        client
            .request("thread/list", Value::Object(params), |_| {})
            .await?
    };
    emit_threads_result(&target, command.json, result, ThreadProjection::Direct)
}

async fn search_command(
    target: Target,
    mut client: RpcClient,
    command: SearchCommand,
) -> Result<i32> {
    let since = command.since.as_deref().map(parse_since).transpose()?;
    let mut params = Map::new();
    insert_opt(&mut params, "cursor", command.cursor.clone());
    let limit = command.limit.unwrap_or(DEFAULT_LIST_LIMIT);
    params.insert("limit".to_string(), json!(limit));
    params.insert("searchTerm".to_string(), json!(command.query));
    if command.archived {
        params.insert("archived".to_string(), json!(true));
    }
    let result = if let Some(since) = since {
        scan_since_filtered(
            &mut client,
            "thread/search",
            params,
            command.cursor,
            limit,
            since,
            ThreadProjection::SearchResult,
        )
        .await?
    } else {
        client
            .request("thread/search", Value::Object(params), |_| {})
            .await?
    };
    emit_threads_result(
        &target,
        command.json,
        result,
        ThreadProjection::SearchResult,
    )
}

#[derive(Clone, Copy)]
enum ThreadProjection {
    Direct,
    SearchResult,
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

async fn show_command(target: Target, mut client: RpcClient, command: ShowCommand) -> Result<i32> {
    let thread = client
        .request(
            "thread/read",
            json!({"threadId": command.thread_id, "includeTurns": false}),
            |_| {},
        )
        .await?;
    let turns = client
        .request(
            "thread/turns/list",
            json!({
                "threadId": command.thread_id,
                "cursor": command.cursor,
                "limit": command.last.unwrap_or(DEFAULT_SHOW_LAST),
                "sortDirection": direction(command.asc, command.desc),
                "itemsView": items_view(command.items)
            }),
            |_| {},
        )
        .await?;
    let result =
        json!({"server": target.server, "thread": thread["thread"].clone(), "turns": turns});
    if command.json {
        print_json(&result)?;
    } else {
        print_thread_detail(&result);
    }
    Ok(0)
}

async fn messages_command(
    target: Target,
    mut client: RpcClient,
    command: MessagesCommand,
) -> Result<i32> {
    let result = client
        .request(
            "thread/turns/list",
            json!({
                "threadId": command.thread_id,
                "limit": command.max_turns,
                "sortDirection": "desc",
                "itemsView": "full"
            }),
            |_| {},
        )
        .await?;
    let mut messages = flatten_messages(&result);
    if let Some(since) = &command.since {
        let cutoff = parse_since(since)?;
        messages.retain(|m| {
            m["turnStartedAt"]
                .as_i64()
                .or_else(|| m["turnCompletedAt"].as_i64())
                .unwrap_or(0)
                >= cutoff
        });
    }
    let filtered_role = command.role.map(message_role_name);
    if let Some(role) = filtered_role {
        messages.retain(|m| m["role"].as_str() == Some(role));
    }
    if let Some(last) = command.last
        && messages.len() > last
    {
        messages = messages.split_off(messages.len() - last);
    }
    let output = json!({
        "server": target.server,
        "threadId": command.thread_id,
        "messages": messages,
        "truncated": result["nextCursor"].is_string(),
        "nextCursor": result["nextCursor"].clone()
    });
    if command.json {
        print_json(&output)?;
    } else {
        print_messages(
            output["messages"].as_array().unwrap_or(&Vec::new()),
            filtered_role,
        );
        if output["truncated"].as_bool() == Some(true) {
            eprintln!("warning: message scan truncated; increase --max-turns for a wider scan");
        }
    }
    Ok(0)
}

async fn new_command(
    target: Target,
    mut client: RpcClient,
    command: NewCommand,
    yolo: bool,
) -> Result<i32> {
    if command.prompt.is_none() && (command.no_wait || command.stream) {
        return Err(usage_error(
            "new without PROMPT cannot use --no-wait or --stream",
        ));
    }
    let mut params = Map::new();
    params.insert("cwd".to_string(), json!(command.cwd));
    if yolo {
        insert_thread_yolo_permissions(&mut params);
    }
    let thread_model = command.model.clone().or_else(|| target.model.clone());
    let thread_effort = command
        .effort
        .clone()
        .or_else(|| target.model_reasoning_effort.clone());
    insert_opt(&mut params, "model", thread_model);
    if let Some(tier) = &command.service_tier {
        params.insert("serviceTier".to_string(), json!(tier));
    }
    if let Some(effort) = thread_effort.as_deref() {
        validate_effort(effort)?;
        params.insert(
            "config".to_string(),
            json!({"model_reasoning_effort": effort}),
        );
    }
    let start = client
        .request("thread/start", Value::Object(params), |_| {})
        .await?;
    let thread_id = start["thread"]["id"]
        .as_str()
        .ok_or_else(|| app_server_error("thread/start response missing thread.id"))?
        .to_string();
    if let Some(name) = &command.name {
        client
            .request(
                "thread/name/set",
                json!({"threadId": thread_id, "name": name}),
                |_| {},
            )
            .await?;
    }
    if let Some(prompt) = command.prompt {
        let turn = TurnOptions {
            model: command.model,
            effort: command.effort,
            service_tier: command.service_tier,
            json: command.json,
            stream: command.stream,
            no_wait: command.no_wait,
            yolo,
        };
        return start_turn(target, client, thread_id, prompt, turn).await;
    }
    let output = json!({"server": target.server, "threadId": thread_id, "thread": start["thread"], "model": start["model"], "effort": start["reasoningEffort"], "serviceTier": start["serviceTier"]});
    if command.json {
        print_json(&output)?;
    } else {
        print_key_values(&[
            ("server", target.server.as_str()),
            ("threadId", output["threadId"].as_str().unwrap_or("")),
        ]);
    }
    Ok(0)
}

async fn send_command(
    target: Target,
    client: RpcClient,
    command: SendCommand,
    yolo: bool,
) -> Result<i32> {
    start_turn(
        target,
        client,
        command.thread_id,
        command.prompt,
        TurnOptions {
            model: command.model,
            effort: command.effort,
            service_tier: command.service_tier,
            json: command.json,
            stream: command.stream,
            no_wait: command.no_wait,
            yolo,
        },
    )
    .await
}

struct TurnOptions {
    model: Option<String>,
    effort: Option<String>,
    service_tier: Option<String>,
    json: bool,
    stream: bool,
    no_wait: bool,
    yolo: bool,
}

async fn start_turn(
    target: Target,
    mut client: RpcClient,
    thread_id: String,
    prompt: String,
    options: TurnOptions,
) -> Result<i32> {
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
    if let Some(effort) = options.effort.as_deref() {
        validate_effort(effort)?;
        params.insert("effort".to_string(), json!(effort));
    }
    if let Some(tier) = options.service_tier {
        params.insert("serviceTier".to_string(), json!(tier));
    }
    let early_notifications = RefCell::new(Vec::new());
    let params = Value::Object(params);
    let result = request_with_resume_retry(
        &mut client,
        "turn/start",
        params,
        &thread_id,
        options.yolo,
        || {
            early_notifications.borrow_mut().clear();
        },
        |notification| {
            early_notifications.borrow_mut().push(notification);
        },
    )
    .await?;
    let turn_id = result["turn"]["id"]
        .as_str()
        .ok_or_else(|| app_server_error("turn/start response missing turn.id"))?
        .to_string();
    let acceptance = json!({"type": "accepted", "server": target.server, "threadId": thread_id, "turnId": turn_id, "status": "accepted"});
    if options.json && options.stream {
        println!("{}", serde_json::to_string(&acceptance)?);
    } else if options.json && options.no_wait {
        print_json(&acceptance)?;
    } else if !options.json {
        print_key_values(&[
            ("server", target.server.as_str()),
            ("threadId", thread_id.as_str()),
            ("turnId", turn_id.as_str()),
            ("status", "accepted"),
        ]);
    }
    if options.no_wait {
        return Ok(0);
    }

    let mut events = vec![acceptance];
    let mut assistant_text = String::new();
    let wait = TurnWaitContext {
        target: &target,
        thread_id: &thread_id,
        turn_id: &turn_id,
        json_out: options.json,
        stream: options.stream,
    };
    for notification in early_notifications.into_inner() {
        if let Some(code) =
            process_turn_notification(&wait, notification, &mut assistant_text, &mut events)?
        {
            return Ok(code);
        }
    }
    let mut poll = tokio::time::interval(std::time::Duration::from_secs(1));
    poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let turn_timeout = tokio::time::sleep(std::time::Duration::from_secs(TURN_WAIT_TIMEOUT_SECS));
    tokio::pin!(turn_timeout);
    loop {
        tokio::select! {
            _ = &mut turn_timeout => {
                return Err(app_server_error(format!(
                    "timed out waiting for turn `{turn_id}` to complete"
                )));
            }
            _ = tokio::signal::ctrl_c() => {
                eprintln!("interrupted locally; turn is still running");
                eprint!("{}", key_values_text(&[
                    ("server", target.server.as_str()),
                    ("threadId", thread_id.as_str()),
                    ("turnId", turn_id.as_str()),
                ]));
                return Ok(130);
            }
            notification = client.next_notification_or_request() => {
                let notification = notification?;
                if let Some(code) = process_turn_notification(
                    &wait,
                    notification,
                    &mut assistant_text,
                    &mut events,
                )? {
                    return Ok(code);
                }
            }
            _ = poll.tick() => {
                if let Some(code) = poll_turn_completion(
                    &mut client,
                    &wait,
                    &mut assistant_text,
                    &mut events,
                ).await? {
                    return Ok(code);
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
    // Only use this for operations whose app-server implementation requires a
    // loaded CodexThread. Persisted metadata/history/goal commands can operate
    // without this, and interrupting a non-loaded thread cannot become useful
    // by loading an inactive session.
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
    // Codex app-server currently returns invalid_request(-32600) with this
    // message from request_processors::{turn_processor,thread_processor}::load_thread.
    error.method == method
        && error.error.code == -32600
        && error.error.message == format!("thread not found: {thread_id}")
}

struct TurnWaitContext<'a> {
    target: &'a Target,
    thread_id: &'a str,
    turn_id: &'a str,
    json_out: bool,
    stream: bool,
}

async fn poll_turn_completion(
    client: &mut RpcClient,
    wait: &TurnWaitContext<'_>,
    assistant_text: &mut String,
    events: &mut Vec<Value>,
) -> Result<Option<i32>> {
    let mut notifications = Vec::new();
    let result = client
        .request(
            "thread/turns/list",
            json!({"threadId": wait.thread_id, "limit": TURN_SCAN_LIMIT, "sortDirection": "desc", "itemsView": "full"}),
            |notification| notifications.push(notification),
        )
        .await?;
    for notification in notifications {
        if let Some(code) = process_turn_notification(wait, notification, assistant_text, events)? {
            return Ok(Some(code));
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
        if !wait.json_out && !assistant_text.is_empty() {
            println!("{assistant_text}");
        }
    }
    let event = json!({"type": status, "server": wait.target.server, "threadId": wait.thread_id, "turnId": wait.turn_id, "status": status, "source": "poll"});
    if wait.json_out && wait.stream {
        println!("{}", serde_json::to_string(&event)?);
    }
    events.push(event);
    emit_turn_terminal(wait, status, assistant_text, events)
}

fn process_turn_notification(
    wait: &TurnWaitContext<'_>,
    notification: Notification,
    assistant_text: &mut String,
    events: &mut Vec<Value>,
) -> Result<Option<i32>> {
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
    if wait.json_out && wait.stream {
        println!("{}", serde_json::to_string(&event)?);
    } else if !wait.json_out {
        print_human_event(&event);
    }

    let status = event["status"].as_str().map(str::to_string);
    events.push(event);
    if !matches!(
        status.as_deref(),
        Some("completed" | "failed" | "interrupted")
    ) {
        return Ok(None);
    }

    let status = status.expect("status checked");
    emit_turn_terminal(wait, &status, assistant_text, events)
}

fn emit_turn_terminal(
    wait: &TurnWaitContext<'_>,
    status: &str,
    assistant_text: &str,
    events: &[Value],
) -> Result<Option<i32>> {
    let output = json!({
        "server": wait.target.server,
        "threadId": wait.thread_id,
        "turnId": wait.turn_id,
        "status": status,
        "progress": events,
        "assistantResponses": if assistant_text.is_empty() { Vec::<Value>::new() } else { vec![json!({"text": assistant_text})] },
        "finalAssistantText": assistant_text
    });
    if wait.json_out && !wait.stream {
        print_json(&output)?;
    } else if !wait.json_out {
        if events.iter().any(|event| event.get("delta").is_some()) {
            println!();
        }
        print_key_values(&[
            ("status", output["status"].as_str().unwrap_or("")),
            ("server", wait.target.server.as_str()),
            ("threadId", wait.thread_id),
            ("turnId", wait.turn_id),
        ]);
    }
    Ok(Some(if output["status"].as_str() == Some("completed") {
        0
    } else {
        1
    }))
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

async fn settings_show_command(
    target: Target,
    mut client: RpcClient,
    command: SettingsShowCommand,
) -> Result<i32> {
    let result = resume_thread_for_inspection(&mut client, &command.thread_id).await?;
    let output = json!({
        "server": target.server,
        "threadId": command.thread_id,
        "model": result["model"].clone(),
        "effort": result["reasoningEffort"].clone(),
        "serviceTier": result["serviceTier"].clone(),
        "cwd": result["cwd"].clone()
    });
    if command.json {
        print_json(&output)?;
    } else {
        print_key_values(&[
            ("model", output["model"].as_str().unwrap_or("")),
            ("effort", output["effort"].as_str().unwrap_or("")),
            ("serviceTier", output["serviceTier"].as_str().unwrap_or("")),
            ("cwd", output["cwd"].as_str().unwrap_or("")),
        ]);
    }
    Ok(0)
}

async fn settings_set_command(
    target: Target,
    mut client: RpcClient,
    command: SettingsSetCommand,
    yolo: bool,
) -> Result<i32> {
    if command.model.is_none()
        && command.effort.is_none()
        && command.service_tier.is_none()
        && !command.clear_service_tier
    {
        return Err(usage_error(
            "settings set requires at least one setting flag",
        ));
    }
    let mut params = Map::new();
    params.insert("threadId".to_string(), json!(command.thread_id.clone()));
    insert_opt(&mut params, "model", command.model.clone());
    if let Some(effort) = command.effort.as_deref() {
        validate_effort(effort)?;
        params.insert("effort".to_string(), json!(effort));
    }
    if command.clear_service_tier {
        params.insert("serviceTier".to_string(), Value::Null);
    } else if let Some(tier) = &command.service_tier {
        params.insert("serviceTier".to_string(), json!(tier));
    }
    let thread_id = command.thread_id.clone();
    let _ = request_with_resume_retry(
        &mut client,
        "thread/settings/update",
        Value::Object(params),
        &thread_id,
        yolo,
        || {},
        |_| {},
    )
    .await?;
    let output = json!({"server": target.server, "threadId": command.thread_id, "status": "accepted", "requested": {"model": command.model, "effort": command.effort, "serviceTier": command.service_tier, "clearServiceTier": command.clear_service_tier}});
    emit_json_or_status(command.json, &output)
}

async fn status_command(
    target: Target,
    mut client: RpcClient,
    command: StatusCommand,
) -> Result<i32> {
    if let Some(thread_id) = command.thread_id {
        if command.load {
            let _ = resume_thread_for_inspection(&mut client, &thread_id).await?;
        }
        let thread = client
            .request(
                "thread/read",
                json!({"threadId": thread_id, "includeTurns": false}),
                |_| {},
            )
            .await?;
        let turns = client
            .request(
                "thread/turns/list",
                json!({"threadId": thread_id, "limit": TURN_SCAN_LIMIT, "sortDirection": "desc", "itemsView": "notLoaded"}),
                |_| {},
            )
            .await?;
        let active_turn_id = turns["data"]
            .as_array()
            .and_then(|turns| turns.iter().find(|turn| turn_status(turn) == "inProgress"))
            .and_then(|turn| turn["id"].as_str())
            .map(str::to_string);
        let output = json!({"server": target.server, "threadId": thread_id, "thread": thread["thread"], "activeTurnId": active_turn_id, "truncated": turns["nextCursor"].is_string()});
        if command.json {
            print_json(&output)?;
        } else {
            print_key_values(&[
                ("server", target.server.as_str()),
                ("threadId", thread_id.as_str()),
                (
                    "status",
                    thread["thread"]["status"]["type"].as_str().unwrap_or(""),
                ),
                (
                    "activeTurnId",
                    output["activeTurnId"].as_str().unwrap_or(""),
                ),
            ]);
        }
    } else {
        let loaded = client
            .request(
                "thread/loaded/list",
                json!({"limit": DEFAULT_LIST_LIMIT}),
                |_| {},
            )
            .await?;
        let output = json!({"server": target.server, "reachable": true, "loadedThreadIds": loaded["data"], "nextCursor": loaded["nextCursor"]});
        if command.json {
            print_json(&output)?;
        } else {
            print_key_values(&[("server", target.server.as_str()), ("reachable", "true")]);
            if let Some(loaded) = output["loadedThreadIds"]
                .as_array()
                .filter(|loaded| !loaded.is_empty())
            {
                println!();
                print_table(
                    &["LOADED THREAD ID"],
                    loaded
                        .iter()
                        .map(|id| vec![table_cell(id.as_str().unwrap_or(""))])
                        .collect(),
                );
            }
        }
    }
    Ok(0)
}

async fn steer_command(
    target: Target,
    mut client: RpcClient,
    command: SteerCommand,
    yolo: bool,
) -> Result<i32> {
    let params = json!({"threadId": command.thread_id, "expectedTurnId": command.turn_id, "input": [{"type": "text", "text": command.prompt, "textElements": []}]});
    let result = request_with_resume_retry(
        &mut client,
        "turn/steer",
        params,
        &command.thread_id,
        yolo,
        || {},
        |_| {},
    )
    .await?;
    let output = json!({"server": target.server, "threadId": command.thread_id, "turnId": result["turnId"].as_str().unwrap_or(&command.turn_id), "status": "accepted"});
    emit_json_or_status(command.json, &output)
}

async fn interrupt_command(
    target: Target,
    mut client: RpcClient,
    command: InterruptCommand,
) -> Result<i32> {
    let _ = client
        .request(
            "turn/interrupt",
            json!({"threadId": command.thread_id, "turnId": command.turn_id}),
            |_| {},
        )
        .await?;
    let output = json!({"server": target.server, "threadId": command.thread_id, "turnId": command.turn_id, "status": "accepted"});
    emit_json_or_status(command.json, &output)
}

async fn name_command(target: Target, mut client: RpcClient, command: NameCommand) -> Result<i32> {
    let _ = client
        .request(
            "thread/name/set",
            json!({"threadId": command.thread_id, "name": command.name}),
            |_| {},
        )
        .await?;
    let output = json!({"server": target.server, "threadId": command.thread_id, "name": command.name, "status": "accepted"});
    emit_json_or_status(command.json, &output)
}

async fn archive_command(
    target: Target,
    mut client: RpcClient,
    command: ThreadOnlyCommand,
    archive: bool,
) -> Result<i32> {
    let method = if archive {
        "thread/archive"
    } else {
        "thread/unarchive"
    };
    let result = client
        .request(method, json!({"threadId": command.thread_id}), |_| {})
        .await?;
    let output = json!({
        "server": target.server,
        "threadId": command.thread_id,
        "archived": archive,
        "status": "accepted",
        "thread": result.get("thread").cloned().unwrap_or(Value::Null)
    });
    emit_json_or_status(command.json, &output)
}

async fn models_command(
    target: Target,
    mut client: RpcClient,
    command: ModelsCommand,
) -> Result<i32> {
    let result = client.request("model/list", json!({}), |_| {}).await?;
    let output = json!({"server": target.server, "models": result["data"], "nextCursor": result["nextCursor"], "backwardsCursor": result["backwardsCursor"]});
    if command.json {
        print_json(&output)?;
    } else {
        print_table(
            &["MODEL", "NAME"],
            output["models"]
                .as_array()
                .unwrap_or(&Vec::new())
                .iter()
                .map(|model| {
                    vec![
                        table_cell(model["id"].as_str().unwrap_or("")),
                        table_cell(
                            model["displayName"]
                                .as_str()
                                .or_else(|| model["name"].as_str())
                                .or_else(|| model["model"].as_str())
                                .unwrap_or(""),
                        ),
                    ]
                })
                .collect(),
        );
    }
    Ok(0)
}

async fn goal_get_command(
    target: Target,
    mut client: RpcClient,
    command: GoalGetCommand,
) -> Result<i32> {
    let result = client
        .request(
            "thread/goal/get",
            json!({"threadId": command.thread_id}),
            |_| {},
        )
        .await?;
    let output =
        json!({"server": target.server, "threadId": command.thread_id, "goal": result["goal"]});
    if command.json {
        print_json(&output)?;
    } else {
        let goal = output["goal"].to_string();
        print_key_values(&[("threadId", command.thread_id.as_str()), ("goal", &goal)]);
    }
    Ok(0)
}

async fn goal_set_command(
    target: Target,
    mut client: RpcClient,
    command: GoalSetCommand,
) -> Result<i32> {
    if command.objective.is_none() && command.status.is_none() && command.token_budget.is_none() {
        return Err(usage_error(
            "goal set requires --objective, --status, or --token-budget",
        ));
    }
    let mut params = Map::new();
    params.insert("threadId".to_string(), json!(command.thread_id));
    insert_opt(&mut params, "objective", command.objective);
    if let Some(status) = command.status {
        params.insert("status".to_string(), json!(goal_status(&status)?));
    }
    if let Some(budget) = command.token_budget {
        params.insert("tokenBudget".to_string(), json!(budget));
    }
    let result = client
        .request("thread/goal/set", Value::Object(params), |_| {})
        .await?;
    let output = json!({"server": target.server, "threadId": command.thread_id, "goal": result["goal"], "status": "accepted"});
    emit_json_or_status(command.json, &output)
}

async fn goal_clear_command(
    target: Target,
    mut client: RpcClient,
    command: GoalClearCommand,
) -> Result<i32> {
    let result = client
        .request(
            "thread/goal/clear",
            json!({"threadId": command.thread_id}),
            |_| {},
        )
        .await?;
    let output = json!({"server": target.server, "threadId": command.thread_id, "cleared": result["cleared"], "status": "accepted"});
    emit_json_or_status(command.json, &output)
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

fn print_human_event(event: &Value) {
    if let Some(delta) = event["delta"].as_str() {
        print!("{delta}");
        let _ = io::stdout().flush();
    } else if let Some(text) = event["text"].as_str()
        && !text.is_empty()
    {
        println!("{text}");
    }
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

fn print_messages(messages: &[Value], filtered_role: Option<&str>) {
    for (index, message) in messages.iter().enumerate() {
        if index > 0 {
            println!();
        }
        let timestamp = message["turnStartedAt"]
            .as_i64()
            .or_else(|| message["turnCompletedAt"].as_i64());
        if filtered_role.is_some() {
            println!("{}", format_timestamp(timestamp));
        } else {
            println!(
                "{} {}",
                format_timestamp(timestamp),
                message["role"].as_str().unwrap_or("")
            );
        }
        println!("{}", message["text"].as_str().unwrap_or(""));
    }
}

fn format_timestamp(timestamp: Option<i64>) -> String {
    let Some(timestamp) = timestamp else {
        return "unknown-time".to_string();
    };
    chrono::DateTime::from_timestamp(timestamp, 0)
        .map(|value| {
            value
                .with_timezone(&chrono::Local)
                .format("%Y-%m-%d %H:%M:%S")
                .to_string()
        })
        .unwrap_or_else(|| timestamp.to_string())
}

fn message_role_name(role: MessageRole) -> &'static str {
    match role {
        MessageRole::User => "user",
        MessageRole::Assistant => "assistant",
    }
}

fn print_thread_detail(result: &Value) {
    let thread = &result["thread"];
    print_key_values(&[
        ("server", result["server"].as_str().unwrap_or("")),
        ("id", thread["id"].as_str().unwrap_or("")),
        ("name", thread["name"].as_str().unwrap_or("")),
        ("cwd", thread["cwd"].as_str().unwrap_or("")),
        ("status", thread["status"]["type"].as_str().unwrap_or("")),
    ]);
    if let Some(turns) = result["turns"]["data"]
        .as_array()
        .filter(|turns| !turns.is_empty())
    {
        println!();
        print_table(
            &["TURN ID", "STATUS"],
            turns
                .iter()
                .map(|turn| {
                    vec![
                        table_cell(turn["id"].as_str().unwrap_or("")),
                        table_cell(turn_status(turn)),
                    ]
                })
                .collect(),
        );
    }
}

fn emit_threads_result(
    target: &Target,
    json_out: bool,
    result: Value,
    projection: ThreadProjection,
) -> Result<i32> {
    let label = match projection {
        ThreadProjection::Direct => "threads",
        ThreadProjection::SearchResult => "results",
    };
    let output = json!({"server": target.server, label: result["data"], "nextCursor": result["nextCursor"], "backwardsCursor": result["backwardsCursor"]});
    if json_out {
        print_json(&output)?;
    } else {
        let headers = match projection {
            ThreadProjection::Direct => vec!["UPDATED", "STATUS", "TITLE/PREVIEW", "THREAD ID"],
            ThreadProjection::SearchResult => {
                vec!["UPDATED", "STATUS", "TITLE/PREVIEW", "SNIPPET", "THREAD ID"]
            }
        };
        let rows = output[label]
            .as_array()
            .unwrap_or(&Vec::new())
            .iter()
            .map(|item| {
                let thread = item.get("thread").unwrap_or(item);
                let mut row = vec![
                    table_cell(format_timestamp(thread["updatedAt"].as_i64())),
                    table_cell(thread["status"]["type"].as_str().unwrap_or("")),
                    capped_cell(thread_label(thread), THREAD_LABEL_WIDTH),
                ];
                if matches!(projection, ThreadProjection::SearchResult) {
                    row.push(capped_cell(
                        item["snippet"].as_str().unwrap_or(""),
                        SEARCH_SNIPPET_WIDTH,
                    ));
                }
                row.push(table_cell(thread["id"].as_str().unwrap_or("")));
                row
            })
            .collect();
        print_table(&headers, rows);
    }
    Ok(0)
}

fn emit_json_or_status(json_out: bool, output: &Value) -> Result<i32> {
    if json_out {
        print_json(output)?;
    } else {
        let mut rows = Vec::new();
        if let Some(server) = output["server"].as_str() {
            rows.push(("server", server));
        }
        if let Some(thread_id) = output["threadId"].as_str() {
            rows.push(("threadId", thread_id));
        }
        if let Some(turn_id) = output["turnId"].as_str() {
            rows.push(("turnId", turn_id));
        }
        rows.push(("status", output["status"].as_str().unwrap_or("accepted")));
        print_key_values(&rows);
    }
    Ok(0)
}

#[derive(Clone)]
struct TableCell {
    text: String,
    max_width: Option<usize>,
}

fn table_cell(text: impl Into<String>) -> TableCell {
    TableCell {
        text: text.into(),
        max_width: None,
    }
}

fn capped_cell(text: impl Into<String>, max_width: usize) -> TableCell {
    TableCell {
        text: text.into(),
        max_width: Some(max_width),
    }
}

fn print_table(headers: &[&str], rows: Vec<Vec<TableCell>>) {
    let rendered_rows = rows
        .into_iter()
        .map(|row| {
            row.into_iter()
                .map(render_table_cell)
                .collect::<Vec<String>>()
        })
        .collect::<Vec<_>>();
    let mut widths = headers
        .iter()
        .map(|header| header.chars().count())
        .collect::<Vec<_>>();
    for row in &rendered_rows {
        for (index, value) in row.iter().enumerate() {
            if index >= widths.len() {
                widths.push(0);
            }
            widths[index] = widths[index].max(value.chars().count());
        }
    }
    print_table_row(
        &headers
            .iter()
            .map(|header| (*header).to_string())
            .collect::<Vec<_>>(),
        &widths,
    );
    for row in rendered_rows {
        print_table_row(&row, &widths);
    }
}

fn print_table_row(row: &[String], widths: &[usize]) {
    for (index, width) in widths.iter().enumerate() {
        if index > 0 {
            print!("  ");
        }
        let value = row.get(index).map(String::as_str).unwrap_or("");
        if index + 1 == widths.len() {
            print!("{value}");
        } else {
            print!("{value:<width$}");
        }
    }
    println!();
}

fn print_key_values(rows: &[(&str, &str)]) {
    print!("{}", key_values_text(rows));
}

fn key_values_text(rows: &[(&str, &str)]) -> String {
    let width = rows
        .iter()
        .map(|(key, _)| key.chars().count())
        .max()
        .unwrap_or_default();
    rows.iter()
        .map(|(key, value)| {
            format!(
                "{key:<width$}  {}",
                sanitize_table_text(value),
                width = width
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
        + "\n"
}

fn render_table_cell(cell: TableCell) -> String {
    let text = sanitize_table_text(&cell.text);
    match cell.max_width {
        Some(max_width) => truncate_text(&text, max_width),
        None => text,
    }
}

fn sanitize_table_text(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn truncate_text(text: &str, max_width: usize) -> String {
    if text.chars().count() <= max_width {
        return text.to_string();
    }
    if max_width <= 3 {
        return text.chars().take(max_width).collect();
    }
    let mut value = text.chars().take(max_width - 3).collect::<String>();
    value.push_str("...");
    value
}

fn thread_label(thread: &Value) -> &str {
    thread["name"]
        .as_str()
        .or_else(|| thread["preview"].as_str())
        .unwrap_or("")
}

fn print_json(value: &Value) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}

fn insert_opt(map: &mut Map<String, Value>, key: &str, value: Option<String>) {
    if let Some(value) = value {
        map.insert(key.to_string(), json!(value));
    }
}

fn insert_thread_yolo_permissions(map: &mut Map<String, Value>) {
    // Thread start/resume use the legacy SandboxMode string shape.
    map.insert("approvalPolicy".to_string(), json!("never"));
    map.insert("sandbox".to_string(), json!("danger-full-access"));
}

fn insert_turn_yolo_permissions(map: &mut Map<String, Value>) {
    // Turn start uses the newer SandboxPolicy object shape.
    map.insert("approvalPolicy".to_string(), json!("never"));
    map.insert(
        "sandboxPolicy".to_string(),
        json!({"type": "dangerFullAccess"}),
    );
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

fn validate_effort(effort: &str) -> Result<()> {
    if is_valid_reasoning_effort(effort) {
        Ok(())
    } else {
        Err(usage_error(format!("invalid effort `{effort}`")))
    }
}

fn goal_status(status: &str) -> Result<&'static str> {
    match status {
        "active" => Ok("active"),
        "paused" => Ok("paused"),
        "blocked" => Ok("blocked"),
        "usage-limited" => Ok("usageLimited"),
        "budget-limited" => Ok("budgetLimited"),
        "complete" => Ok("complete"),
        _ => Err(usage_error(format!("invalid goal status `{status}`"))),
    }
}

fn parse_since(since: &str) -> Result<i64> {
    if let Ok(timestamp) = since.parse::<i64>() {
        return Ok(timestamp);
    }
    let (number, multiplier) = if let Some(value) = since.strip_suffix('s') {
        (value, 1)
    } else if let Some(value) = since.strip_suffix('m') {
        (value, 60)
    } else if let Some(value) = since.strip_suffix('h') {
        (value, 60 * 60)
    } else if let Some(value) = since.strip_suffix('d') {
        (value, 60 * 60 * 24)
    } else {
        return Err(usage_error(format!("invalid --since value `{since}`")));
    };
    let seconds: i64 = number
        .parse()
        .with_context(|| format!("invalid --since value `{since}`"))?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    Ok(now - seconds * multiplier)
}

fn classify_error(err: &anyhow::Error) -> i32 {
    if let Some(error) = err.downcast_ref::<ExitError>() {
        return error.code;
    }
    let text = err.to_string();
    if text.contains("requires experimentalApi")
        || text.contains("app-server")
        || text.contains("UDS")
        || text.contains("websocket")
    {
        3
    } else {
        2
    }
}

fn usage_error(message: impl Into<String>) -> anyhow::Error {
    ExitError {
        code: 2,
        message: message.into(),
    }
    .into()
}

fn app_server_error(message: impl Into<String>) -> anyhow::Error {
    ExitError {
        code: 3,
        message: message.into(),
    }
    .into()
}
