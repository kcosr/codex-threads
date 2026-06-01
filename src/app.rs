use std::ffi::OsString;
use std::io::{self, Write};

use anyhow::{Context, Result};
use clap::Parser;
use serde_json::{Map, Value, json};

use crate::cli::*;
use crate::config::{AppConfig, Target, load_config, resolve_config_path, resolve_target};
use crate::rpc::{Notification, RpcClient};

const DEFAULT_LIST_LIMIT: u32 = 50;
const DEFAULT_SHOW_LAST: u32 = 20;
const TURN_SCAN_LIMIT: u32 = 200;
const TURN_WAIT_TIMEOUT_SECS: u64 = 60 * 60;

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
    if let Command::Servers(command) = &cli.command {
        return servers_command(&config_path, command).await;
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
                |target, client| async move { new_command(target, client, command).await },
            )
            .await
        }
        Command::Send(command) => {
            with_client(
                &config,
                cli.connect.as_deref(),
                command.server.server.clone(),
                |target, client| async move { send_command(target, client, command).await },
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
                            settings_set_command(target, client, command).await
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
                |target, client| async move { steer_command(target, client, command).await },
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

async fn servers_command(config_path: &std::path::Path, command: &ServersCommand) -> Result<i32> {
    let config = load_config(config_path)?;
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
                for row in rows {
                    println!(
                        "{}\t{}\t{}",
                        row["alias"].as_str().unwrap_or(""),
                        row["type"].as_str().unwrap_or(""),
                        row["path"].as_str().unwrap_or("")
                    );
                }
            }
            Ok(0)
        }
        Some(ServersSubcommand::Ping(ping)) => {
            let targets = if ping.all {
                config
                    .servers
                    .iter()
                    .map(|(server, cfg)| Target {
                        server: server.clone(),
                        path: cfg.path.clone(),
                    })
                    .collect::<Vec<_>>()
            } else {
                vec![resolve_target(&config, None, ping.server.as_deref())?]
            };
            let mut results = Vec::new();
            for target in targets {
                let ok = RpcClient::connect(&target.path).await.is_ok();
                if !ping.json {
                    println!("{}\t{}", target.server, if ok { "ok" } else { "error" });
                }
                results.push(json!({"server": target.server, "ok": ok}));
            }
            if ping.json {
                print_json(&json!({"servers": results}))?;
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
    let mut params = Map::new();
    insert_opt(&mut params, "cursor", command.cursor);
    params.insert(
        "limit".to_string(),
        json!(command.limit.unwrap_or(DEFAULT_LIST_LIMIT)),
    );
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
    let result = client
        .request("thread/list", Value::Object(params), |_| {})
        .await?;
    emit_result(&target, command.json, result, "threads")
}

async fn search_command(
    target: Target,
    mut client: RpcClient,
    command: SearchCommand,
) -> Result<i32> {
    let mut params = Map::new();
    insert_opt(&mut params, "cursor", command.cursor);
    params.insert(
        "limit".to_string(),
        json!(command.limit.unwrap_or(DEFAULT_LIST_LIMIT)),
    );
    params.insert("searchTerm".to_string(), json!(command.query));
    if command.archived {
        params.insert("archived".to_string(), json!(true));
    }
    let result = client
        .request("thread/search", Value::Object(params), |_| {})
        .await?;
    emit_result(&target, command.json, result, "results")
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
        for message in output["messages"].as_array().unwrap_or(&Vec::new()) {
            println!(
                "{}\t{}",
                message["role"].as_str().unwrap_or(""),
                message["text"].as_str().unwrap_or("")
            );
        }
        if output["truncated"].as_bool() == Some(true) {
            eprintln!("warning: message scan truncated; increase --max-turns for a wider scan");
        }
    }
    Ok(0)
}

async fn new_command(target: Target, mut client: RpcClient, command: NewCommand) -> Result<i32> {
    if command.prompt.is_none() && (command.no_wait || command.stream) {
        return Err(usage_error(
            "new without PROMPT cannot use --no-wait or --stream",
        ));
    }
    let mut params = Map::new();
    params.insert("cwd".to_string(), json!(command.cwd));
    insert_opt(&mut params, "model", command.model.clone());
    if let Some(tier) = &command.service_tier {
        params.insert("serviceTier".to_string(), json!(tier));
    }
    if let Some(effort) = command.effort.as_deref() {
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
        };
        return start_turn(target, client, thread_id, prompt, turn).await;
    }
    let output = json!({"server": target.server, "threadId": thread_id, "thread": start["thread"], "model": start["model"], "effort": start["reasoningEffort"], "serviceTier": start["serviceTier"]});
    if command.json {
        print_json(&output)?;
    } else {
        println!("server\t{}", target.server);
        println!("threadId\t{}", output["threadId"].as_str().unwrap_or(""));
    }
    Ok(0)
}

async fn send_command(target: Target, client: RpcClient, command: SendCommand) -> Result<i32> {
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
    insert_opt(&mut params, "model", options.model);
    if let Some(effort) = options.effort.as_deref() {
        validate_effort(effort)?;
        params.insert("effort".to_string(), json!(effort));
    }
    if let Some(tier) = options.service_tier {
        params.insert("serviceTier".to_string(), json!(tier));
    }
    let mut early_notifications = Vec::new();
    let result = client
        .request("turn/start", Value::Object(params), |notification| {
            early_notifications.push(notification);
        })
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
        println!("server\t{}", target.server);
        println!("threadId\t{}", thread_id);
        println!("turnId\t{}", turn_id);
        println!("status\taccepted");
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
    for notification in early_notifications {
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
                eprintln!("server\t{}", target.server);
                eprintln!("threadId\t{}", thread_id);
                eprintln!("turnId\t{}", turn_id);
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
    let status = turn_status(turn);
    reject_unknown_turn_status(turn)?;
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
    ) else {
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
        println!("status\t{}", output["status"].as_str().unwrap_or(""));
        println!("server\t{}", wait.target.server);
        println!("threadId\t{}", wait.thread_id);
        println!("turnId\t{}", wait.turn_id);
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
    let result = client
        .request(
            "thread/resume",
            json!({"threadId": command.thread_id, "excludeTurns": true}),
            |_| {},
        )
        .await?;
    let _ = client
        .request(
            "thread/unsubscribe",
            json!({"threadId": command.thread_id}),
            |_| {},
        )
        .await;
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
        println!("model\t{}", output["model"].as_str().unwrap_or(""));
        println!("effort\t{}", output["effort"].as_str().unwrap_or(""));
        println!(
            "serviceTier\t{}",
            output["serviceTier"].as_str().unwrap_or("")
        );
        println!("cwd\t{}", output["cwd"].as_str().unwrap_or(""));
    }
    Ok(0)
}

async fn settings_set_command(
    target: Target,
    mut client: RpcClient,
    command: SettingsSetCommand,
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
    params.insert("threadId".to_string(), json!(command.thread_id));
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
    let _ = client
        .request("thread/settings/update", Value::Object(params), |_| {})
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
            println!("server\t{}", target.server);
            println!("threadId\t{}", thread_id);
            println!(
                "status\t{}",
                thread["thread"]["status"]["type"].as_str().unwrap_or("")
            );
            println!(
                "activeTurnId\t{}",
                output["activeTurnId"].as_str().unwrap_or("")
            );
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
            println!("server\t{}", target.server);
            println!("reachable\ttrue");
            for id in output["loadedThreadIds"].as_array().unwrap_or(&Vec::new()) {
                println!("loaded\t{}", id.as_str().unwrap_or(""));
            }
        }
    }
    Ok(0)
}

async fn steer_command(
    target: Target,
    mut client: RpcClient,
    command: SteerCommand,
) -> Result<i32> {
    let result = client
        .request(
            "turn/steer",
            json!({"threadId": command.thread_id, "expectedTurnId": command.turn_id, "input": [{"type": "text", "text": command.prompt, "textElements": []}]}),
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
        for model in output["models"].as_array().unwrap_or(&Vec::new()) {
            println!(
                "{}\t{}",
                model["id"].as_str().unwrap_or(""),
                model["displayName"]
                    .as_str()
                    .or_else(|| model["name"].as_str())
                    .or_else(|| model["model"].as_str())
                    .unwrap_or("")
            );
        }
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
        println!("threadId\t{}", command.thread_id);
        println!("goal\t{}", output["goal"]);
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
) -> Option<Value> {
    match notification.method.as_str() {
        "item/agentMessage/delta"
            if notification.params["threadId"] == thread_id
                && notification.params["turnId"] == turn_id =>
        {
            let delta = notification.params["delta"].as_str().unwrap_or("");
            assistant_text.push_str(delta);
            Some(
                json!({"type": "progress", "server": server, "threadId": thread_id, "turnId": turn_id, "delta": delta}),
            )
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
                return Some(
                    json!({"type": "assistantMessage", "server": server, "threadId": thread_id, "turnId": turn_id, "text": text}),
                );
            }
            None
        }
        "turn/completed" if notification.params["threadId"] == thread_id => {
            let status = turn_status(&notification.params["turn"]);
            Some(
                json!({"type": status, "server": server, "threadId": thread_id, "turnId": turn_id, "status": status}),
            )
        }
        _ => None,
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

fn print_thread_detail(result: &Value) {
    let thread = &result["thread"];
    println!("server\t{}", result["server"].as_str().unwrap_or(""));
    println!("id\t{}", thread["id"].as_str().unwrap_or(""));
    println!("name\t{}", thread["name"].as_str().unwrap_or(""));
    println!("cwd\t{}", thread["cwd"].as_str().unwrap_or(""));
    println!(
        "status\t{}",
        thread["status"]["type"].as_str().unwrap_or("")
    );
    for turn in result["turns"]["data"].as_array().unwrap_or(&Vec::new()) {
        println!(
            "turn\t{}\t{}",
            turn["id"].as_str().unwrap_or(""),
            turn_status(turn)
        );
    }
}

fn emit_result(target: &Target, json_out: bool, result: Value, label: &str) -> Result<i32> {
    let output = json!({"server": target.server, label: result["data"], "nextCursor": result["nextCursor"], "backwardsCursor": result["backwardsCursor"]});
    if json_out {
        print_json(&output)?;
    } else {
        for item in output[label].as_array().unwrap_or(&Vec::new()) {
            let thread = item.get("thread").unwrap_or(item);
            println!(
                "{}\t{}\t{}\t{}",
                thread["updatedAt"].as_i64().unwrap_or_default(),
                thread["status"]["type"].as_str().unwrap_or(""),
                thread["name"]
                    .as_str()
                    .or_else(|| thread["preview"].as_str())
                    .unwrap_or(""),
                thread["id"].as_str().unwrap_or("")
            );
        }
    }
    Ok(0)
}

fn emit_json_or_status(json_out: bool, output: &Value) -> Result<i32> {
    if json_out {
        print_json(output)?;
    } else {
        if let Some(server) = output["server"].as_str() {
            println!("server\t{server}");
        }
        if let Some(thread_id) = output["threadId"].as_str() {
            println!("threadId\t{thread_id}");
        }
        if let Some(turn_id) = output["turnId"].as_str() {
            println!("turnId\t{turn_id}");
        }
        println!(
            "status\t{}",
            output["status"].as_str().unwrap_or("accepted")
        );
    }
    Ok(0)
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
    match effort {
        "none" | "minimal" | "low" | "medium" | "high" | "xhigh" => Ok(()),
        _ => Err(usage_error(format!("invalid effort `{effort}`"))),
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
