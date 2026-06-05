use std::ffi::OsString;
use std::io::{self, Write};
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;
use serde_json::{Map, Value, json};

use crate::annotations::{
    AnnotationListItem, clear_annotation, clear_annotations, list_annotations, load_annotation,
    set_annotation,
};
use crate::cli::*;
use crate::completion::{
    completion_candidates, completion_instructions, completion_script, normalize_shell,
};
use crate::config::{
    AppConfig, Target, is_valid_reasoning_effort, legacy_server_warnings, load_config,
    resolve_config_path, resolve_direct_target, resolve_target,
};
use crate::errors::{ExitError, app_server_error, usage_error};
use crate::rpc::{Notification, RpcClient, RpcRequestError};
use crate::session::{
    ListThreadsRequest, LoadedStatusRequest, MessagesRequest, SearchThreadsRequest,
    ShowThreadRequest, ThreadProjection, ThreadStatusRequest, list_threads, load_messages,
    loaded_status, read_thread_detail, search_threads, thread_status,
};
use crate::turns::{
    TurnStartOptions, TurnTerminal, TurnWaitOutcome, start_turn as start_turn_request,
    wait_for_turn,
};

const DEFAULT_LIST_LIMIT: u32 = 50;
const DEFAULT_SHOW_LAST: u32 = 20;
const TURN_SCAN_LIMIT: u32 = 200;
const TURN_WAIT_TIMEOUT_SECS: u64 = 60 * 60;
const THREAD_LABEL_WIDTH: usize = 56;
const SEARCH_SNIPPET_WIDTH: usize = 48;
const ANNOTATION_WIDTH: usize = 40;

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
    match &cli.command {
        Command::Completion(command) => {
            match &command.command {
                Some(CompletionSubcommand::Script(script)) => {
                    io::stdout().write_all(completion_script(script.shell).as_bytes())?;
                }
                None => {
                    let shell = normalize_shell(command.shell)?;
                    io::stdout().write_all(completion_instructions(shell).as_bytes())?;
                }
            }
            io::stdout().flush()?;
            return Ok(0);
        }
        Command::Complete(command) => {
            io::stdout()
                .write_all(completion_candidates(&command.prefix, &command.words).as_bytes())?;
            io::stdout().flush()?;
            return Ok(0);
        }
        _ => {}
    }

    let config_path = resolve_config_path(cli.config.clone());
    let yolo = !cli.no_yolo;
    if let Command::Servers(command) = &cli.command {
        return servers_command(
            &config_path,
            cli.connect.as_deref(),
            cli.connect_auth_token_env.as_deref(),
            cli.connect_auth_token.as_deref(),
            command,
        )
        .await;
    }
    let config = if cli.connect.is_some() {
        AppConfig::default()
    } else {
        let config = load_config(&config_path)?;
        print_legacy_warnings(&config);
        config
    };
    match cli.command {
        Command::Servers(_) => unreachable!(),
        Command::List(command) => {
            with_client(
                &config,
                cli.connect.as_deref(),
                cli.connect_auth_token_env.as_deref(),
                cli.connect_auth_token.as_deref(),
                command.server.server.clone(),
                |target, client| async move { list_command(target, client, command).await },
            )
            .await
        }
        Command::Search(command) => {
            with_client(
                &config,
                cli.connect.as_deref(),
                cli.connect_auth_token_env.as_deref(),
                cli.connect_auth_token.as_deref(),
                command.server.server.clone(),
                |target, client| async move { search_command(target, client, command).await },
            )
            .await
        }
        Command::Show(command) => {
            with_client(
                &config,
                cli.connect.as_deref(),
                cli.connect_auth_token_env.as_deref(),
                cli.connect_auth_token.as_deref(),
                command.server.server.clone(),
                |target, client| async move { show_command(target, client, command).await },
            )
            .await
        }
        Command::Messages(command) => {
            with_client(
                &config,
                cli.connect.as_deref(),
                cli.connect_auth_token_env.as_deref(),
                cli.connect_auth_token.as_deref(),
                command.server.server.clone(),
                |target, client| async move { messages_command(target, client, command).await },
            )
            .await
        }
        Command::New(command) => {
            with_client(
                &config,
                cli.connect.as_deref(),
                cli.connect_auth_token_env.as_deref(),
                cli.connect_auth_token.as_deref(),
                command.server.server.clone(),
                |target, client| async move { new_command(target, client, command, yolo).await },
            )
            .await
        }
        Command::Send(command) => {
            with_client(
                &config,
                cli.connect.as_deref(),
                cli.connect_auth_token_env.as_deref(),
                cli.connect_auth_token.as_deref(),
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
                        cli.connect_auth_token_env.as_deref(),
                        cli.connect_auth_token.as_deref(),
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
                        cli.connect_auth_token_env.as_deref(),
                        cli.connect_auth_token.as_deref(),
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
                cli.connect_auth_token_env.as_deref(),
                cli.connect_auth_token.as_deref(),
                command.server.server.clone(),
                |target, client| async move { status_command(target, client, command).await },
            )
            .await
        }
        Command::Steer(command) => {
            with_client(
                &config,
                cli.connect.as_deref(),
                cli.connect_auth_token_env.as_deref(),
                cli.connect_auth_token.as_deref(),
                command.server.server.clone(),
                |target, client| async move { steer_command(target, client, command, yolo).await },
            )
            .await
        }
        Command::Interrupt(command) => {
            with_client(
                &config,
                cli.connect.as_deref(),
                cli.connect_auth_token_env.as_deref(),
                cli.connect_auth_token.as_deref(),
                command.server.server.clone(),
                |target, client| async move { interrupt_command(target, client, command).await },
            )
            .await
        }
        Command::Name(command) => {
            with_client(
                &config,
                cli.connect.as_deref(),
                cli.connect_auth_token_env.as_deref(),
                cli.connect_auth_token.as_deref(),
                command.server.server.clone(),
                |target, client| async move { name_command(target, client, command).await },
            )
            .await
        }
        Command::Archive(command) => with_client(
            &config,
            cli.connect.as_deref(),
            cli.connect_auth_token_env.as_deref(),
            cli.connect_auth_token.as_deref(),
            command.server.server.clone(),
            |target, client| async move { archive_command(target, client, command, true).await },
        )
        .await,
        Command::Unarchive(command) => with_client(
            &config,
            cli.connect.as_deref(),
            cli.connect_auth_token_env.as_deref(),
            cli.connect_auth_token.as_deref(),
            command.server.server.clone(),
            |target, client| async move { archive_command(target, client, command, false).await },
        )
        .await,
        Command::Models(command) => {
            with_client(
                &config,
                cli.connect.as_deref(),
                cli.connect_auth_token_env.as_deref(),
                cli.connect_auth_token.as_deref(),
                command.server.server.clone(),
                |target, client| async move { models_command(target, client, command).await },
            )
            .await
        }
        Command::Usage(command) => {
            with_client(
                &config,
                cli.connect.as_deref(),
                cli.connect_auth_token_env.as_deref(),
                cli.connect_auth_token.as_deref(),
                command.server.server.clone(),
                |target, client| async move { usage_command(target, client, command).await },
            )
            .await
        }
        Command::Goal(command) => match command.command {
            GoalSubcommand::Get(command) => {
                with_client(
                    &config,
                    cli.connect.as_deref(),
                    cli.connect_auth_token_env.as_deref(),
                    cli.connect_auth_token.as_deref(),
                    command.server.server.clone(),
                    |target, client| async move { goal_get_command(target, client, command).await },
                )
                .await
            }
            GoalSubcommand::Set(command) => {
                with_client(
                    &config,
                    cli.connect.as_deref(),
                    cli.connect_auth_token_env.as_deref(),
                    cli.connect_auth_token.as_deref(),
                    command.server.server.clone(),
                    |target, client| async move { goal_set_command(target, client, command).await },
                )
                .await
            }
            GoalSubcommand::Clear(command) => with_client(
                &config,
                cli.connect.as_deref(),
                cli.connect_auth_token_env.as_deref(),
                cli.connect_auth_token.as_deref(),
                command.server.server.clone(),
                |target, client| async move { goal_clear_command(target, client, command).await },
            )
            .await,
        },
        Command::Annotate(command) => match command.command {
            AnnotateSubcommand::Set(command) => {
                let target = resolve_target_for_command(
                    &config,
                    cli.connect.as_deref(),
                    cli.connect_auth_token_env.as_deref(),
                    cli.connect_auth_token.as_deref(),
                    command.server.server.clone(),
                )?;
                annotate_set_command(target, command).await
            }
            AnnotateSubcommand::Get(command) => {
                let target = resolve_target_for_command(
                    &config,
                    cli.connect.as_deref(),
                    cli.connect_auth_token_env.as_deref(),
                    cli.connect_auth_token.as_deref(),
                    command.server.server.clone(),
                )?;
                annotate_get_command(target, command).await
            }
            AnnotateSubcommand::Clear(command) => {
                let target = resolve_target_for_command(
                    &config,
                    cli.connect.as_deref(),
                    cli.connect_auth_token_env.as_deref(),
                    cli.connect_auth_token.as_deref(),
                    command.server.server.clone(),
                )?;
                annotate_clear_command(target, command).await
            }
            AnnotateSubcommand::List(command) => {
                let target = resolve_target_for_command(
                    &config,
                    cli.connect.as_deref(),
                    cli.connect_auth_token_env.as_deref(),
                    cli.connect_auth_token.as_deref(),
                    command.server.server.clone(),
                )?;
                annotate_list_command(target, command).await
            }
            AnnotateSubcommand::Search(command) => {
                let target = resolve_target_for_command(
                    &config,
                    cli.connect.as_deref(),
                    cli.connect_auth_token_env.as_deref(),
                    cli.connect_auth_token.as_deref(),
                    command.server.server.clone(),
                )?;
                annotate_search_command(target, command).await
            }
            AnnotateSubcommand::Prune(command) => {
                with_client(
                    &config,
                    cli.connect.as_deref(),
                    cli.connect_auth_token_env.as_deref(),
                    cli.connect_auth_token.as_deref(),
                    command.server.server.clone(),
                    |target, client| async move {
                        annotate_prune_command(target, client, command).await
                    },
                )
                .await
            }
        },
        Command::Completion(_) | Command::Complete(_) => unreachable!(),
    }
}

fn resolve_target_for_command(
    config: &AppConfig,
    connect: Option<&str>,
    connect_auth_token_env: Option<&str>,
    connect_auth_token: Option<&str>,
    server: Option<String>,
) -> Result<Target> {
    if let Some(endpoint) = connect {
        if server.is_some() || std::env::var("CODEX_THREADS_SERVER").is_ok() {
            return Err(usage_error(
                "--connect is mutually exclusive with --server and CODEX_THREADS_SERVER",
            ));
        }
        return resolve_direct_target(endpoint, connect_auth_token_env, connect_auth_token);
    }

    if connect_auth_token_env.is_some() || connect_auth_token.is_some() {
        return Err(usage_error(
            "--connect-auth-token and --connect-auth-token-env require --connect",
        ));
    }
    resolve_target(config, server.as_deref())
}

async fn with_client<F, Fut>(
    config: &AppConfig,
    connect: Option<&str>,
    connect_auth_token_env: Option<&str>,
    connect_auth_token: Option<&str>,
    server: Option<String>,
    f: F,
) -> Result<i32>
where
    F: FnOnce(Target, RpcClient) -> Fut,
    Fut: std::future::Future<Output = Result<i32>>,
{
    let target = resolve_target_for_command(
        config,
        connect,
        connect_auth_token_env,
        connect_auth_token,
        server,
    )?;
    let client = RpcClient::connect(&target.endpoint).await?;
    f(target, client).await
}

async fn servers_command(
    config_path: &std::path::Path,
    connect: Option<&str>,
    connect_auth_token_env: Option<&str>,
    connect_auth_token: Option<&str>,
    command: &ServersCommand,
) -> Result<i32> {
    let config = if connect.is_some() {
        AppConfig::default()
    } else {
        let config = load_config(config_path)?;
        print_legacy_warnings(&config);
        config
    };
    match &command.command {
        None => {
            if connect_auth_token_env.is_some() || connect_auth_token.is_some() {
                return Err(usage_error(
                    "--connect-auth-token and --connect-auth-token-env are not valid for servers listing",
                ));
            }
            let rows: Vec<_> = config
                .servers
                .iter()
                .map(|(alias, server)| {
                    let endpoint = server.endpoint_display(alias)?;
                    Ok(json!({"alias": alias, "endpoint": endpoint}))
                })
                .collect::<Result<Vec<_>>>()?;
            if command.json {
                print_json(&json!({ "servers": rows }))?;
            } else {
                print_table(
                    &["ALIAS", "ENDPOINT"],
                    rows.iter()
                        .map(|row| {
                            vec![
                                table_cell(row["alias"].as_str().unwrap_or("")),
                                table_cell(row["endpoint"].as_str().unwrap_or("")),
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
            if connect.is_some()
                && (ping.server.is_some() || std::env::var("CODEX_THREADS_SERVER").is_ok())
            {
                return Err(usage_error(
                    "--connect is mutually exclusive with --server and CODEX_THREADS_SERVER",
                ));
            }
            if ping.all {
                let mut results = Vec::new();
                for (server, cfg) in &config.servers {
                    let ok = match Target::configured(server, cfg, &config) {
                        Ok(target) => RpcClient::connect(&target.endpoint).await.is_ok(),
                        Err(_) => false,
                    };
                    results.push(json!({"server": server, "ok": ok}));
                }
                return render_server_ping_results(results, ping.json);
            }

            let targets = {
                let target = if let Some(endpoint) = connect {
                    resolve_direct_target(endpoint, connect_auth_token_env, connect_auth_token)?
                } else {
                    if connect_auth_token_env.is_some() || connect_auth_token.is_some() {
                        return Err(usage_error(
                            "--connect-auth-token and --connect-auth-token-env require --connect",
                        ));
                    }
                    resolve_target(&config, ping.server.as_deref())?
                };
                vec![target]
            };
            let mut results = Vec::new();
            for target in targets {
                let ok = RpcClient::connect(&target.endpoint).await.is_ok();
                results.push(json!({"server": target.server, "ok": ok}));
            }
            render_server_ping_results(results, ping.json)
        }
    }
}

fn render_server_ping_results(results: Vec<Value>, json_output: bool) -> Result<i32> {
    if json_output {
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

async fn list_command(target: Target, mut client: RpcClient, command: ListCommand) -> Result<i32> {
    let since = command.since.as_deref().map(parse_since).transpose()?;
    let limit = command.limit.unwrap_or(DEFAULT_LIST_LIMIT);
    let result = list_threads(
        &target,
        &mut client,
        ListThreadsRequest {
            limit,
            cursor: command.cursor,
            since,
            cwd: command.cwd,
            archived: command.archived,
            sort: command.sort,
            asc: command.asc,
            desc: command.desc,
        },
    )
    .await?;
    emit_threads_result(&target, command.json, result, ThreadProjection::Direct)
}

async fn search_command(
    target: Target,
    mut client: RpcClient,
    command: SearchCommand,
) -> Result<i32> {
    let since = command.since.as_deref().map(parse_since).transpose()?;
    let limit = command.limit.unwrap_or(DEFAULT_LIST_LIMIT);
    let result = search_threads(
        &target,
        &mut client,
        SearchThreadsRequest {
            query: command.query,
            limit,
            cursor: command.cursor,
            since,
            archived: command.archived,
        },
    )
    .await?;
    emit_threads_result(
        &target,
        command.json,
        result,
        ThreadProjection::SearchResult,
    )
}

async fn show_command(target: Target, mut client: RpcClient, command: ShowCommand) -> Result<i32> {
    let result = read_thread_detail(
        &target,
        &mut client,
        ShowThreadRequest {
            thread_id: command.thread_id,
            last: command.last.unwrap_or(DEFAULT_SHOW_LAST),
            cursor: command.cursor,
            asc: command.asc,
            desc: command.desc,
            items: command.items,
        },
    )
    .await?;
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
    let since = command.since.as_deref().map(parse_since).transpose()?;
    let result = load_messages(
        &target,
        &mut client,
        MessagesRequest {
            thread_id: command.thread_id,
            last: command.last,
            since,
            role: command.role,
            max_turns: command.max_turns,
        },
    )
    .await?;
    let output = result.output;
    let filtered_role = result.filtered_role.map(message_role_name);
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
    if let Some(effort) = options.effort.as_deref() {
        validate_effort(effort)?;
    }
    let json_out = options.json;
    let stream = options.stream;
    let no_wait = options.no_wait;

    let started = start_turn_request(
        &target,
        &mut client,
        thread_id,
        prompt,
        TurnStartOptions {
            model: options.model,
            effort: options.effort,
            service_tier: options.service_tier,
            yolo: options.yolo,
        },
    )
    .await?;
    if json_out && stream {
        println!("{}", serde_json::to_string(&started.acceptance)?);
    } else if json_out && no_wait {
        print_json(&started.acceptance)?;
    } else if !json_out {
        print_key_values(&[
            ("server", target.server.as_str()),
            ("threadId", started.thread_id.as_str()),
            ("turnId", started.turn_id.as_str()),
            ("status", "accepted"),
        ]);
    }
    if no_wait {
        return Ok(0);
    }

    let outcome = wait_for_turn(
        &target,
        &mut client,
        started,
        TURN_SCAN_LIMIT,
        Duration::from_secs(TURN_WAIT_TIMEOUT_SECS),
        |event| {
            if json_out && stream {
                println!("{}", serde_json::to_string(event)?);
            } else if !json_out {
                print_human_event(event);
            }
            Ok(())
        },
        |text| {
            if !json_out && !text.is_empty() {
                println!("{text}");
            }
            Ok(())
        },
    )
    .await?;
    match outcome {
        TurnWaitOutcome::Terminal(terminal) => {
            emit_turn_terminal_output(json_out, stream, &terminal, target.server.as_str())
        }
        TurnWaitOutcome::LocalInterrupt { thread_id, turn_id } => {
            eprintln!("interrupted locally; turn is still running");
            eprint!(
                "{}",
                key_values_text(&[
                    ("server", target.server.as_str()),
                    ("threadId", thread_id.as_str()),
                    ("turnId", turn_id.as_str()),
                ])
            );
            Ok(130)
        }
    }
}

fn emit_turn_terminal_output(
    json_out: bool,
    stream: bool,
    terminal: &TurnTerminal,
    server: &str,
) -> Result<i32> {
    if json_out && !stream {
        print_json(&terminal.output)?;
    } else if !json_out {
        if terminal
            .output
            .get("progress")
            .and_then(Value::as_array)
            .is_some_and(|events| events.iter().any(|event| event.get("delta").is_some()))
        {
            println!();
        }
        print_key_values(&[
            ("status", terminal.output["status"].as_str().unwrap_or("")),
            ("server", server),
            (
                "threadId",
                terminal.output["threadId"].as_str().unwrap_or(""),
            ),
            ("turnId", terminal.output["turnId"].as_str().unwrap_or("")),
        ]);
    }
    Ok(terminal.exit_code)
}

fn print_legacy_warnings(config: &AppConfig) {
    for warning in legacy_server_warnings(config) {
        eprintln!("warning: {warning}");
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
        let output = thread_status(
            &target,
            &mut client,
            ThreadStatusRequest {
                thread_id: thread_id.clone(),
                load: command.load,
                turn_scan_limit: TURN_SCAN_LIMIT,
            },
        )
        .await?;
        if command.json {
            print_json(&output)?;
        } else {
            print_key_values(&[
                ("server", target.server.as_str()),
                ("threadId", thread_id.as_str()),
                (
                    "status",
                    output["thread"]["status"]["type"].as_str().unwrap_or(""),
                ),
                (
                    "activeTurnId",
                    output["activeTurnId"].as_str().unwrap_or(""),
                ),
            ]);
        }
    } else {
        let output = loaded_status(
            &target,
            &mut client,
            LoadedStatusRequest {
                limit: DEFAULT_LIST_LIMIT,
            },
        )
        .await?;
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

async fn annotate_set_command(target: Target, command: AnnotateSetCommand) -> Result<i32> {
    let annotation = set_annotation(&target, &command.thread_id, &command.text)?;
    let output = json!({
        "server": target.server,
        "threadId": command.thread_id,
        "annotation": annotation,
        "status": "accepted"
    });
    if command.json {
        print_json(&output)?;
    } else {
        print_key_values(&[
            ("server", output["server"].as_str().unwrap_or("")),
            ("threadId", output["threadId"].as_str().unwrap_or("")),
            ("status", output["status"].as_str().unwrap_or("accepted")),
        ]);
    }
    Ok(0)
}

async fn annotate_get_command(target: Target, command: AnnotateGetCommand) -> Result<i32> {
    let Some(annotation) = load_annotation(&target, &command.thread_id)? else {
        return Err(ExitError {
            code: 2,
            message: format!("annotation not found for thread `{}`", command.thread_id),
        }
        .into());
    };
    let output = json!({
        "server": target.server,
        "threadId": command.thread_id,
        "annotation": annotation
    });
    if command.json {
        print_json(&output)?;
    } else {
        print_annotation_detail(&output);
    }
    Ok(0)
}

async fn annotate_clear_command(target: Target, command: AnnotateClearCommand) -> Result<i32> {
    let cleared = clear_annotation(&target, &command.thread_id)?;
    let output = json!({
        "server": target.server,
        "threadId": command.thread_id,
        "cleared": cleared,
        "status": "accepted"
    });
    if command.json {
        print_json(&output)?;
    } else {
        print_key_values(&[
            ("server", output["server"].as_str().unwrap_or("")),
            ("threadId", output["threadId"].as_str().unwrap_or("")),
            ("cleared", if cleared { "true" } else { "false" }),
            ("status", output["status"].as_str().unwrap_or("accepted")),
        ]);
    }
    Ok(0)
}

async fn annotate_list_command(target: Target, command: AnnotateListCommand) -> Result<i32> {
    emit_annotation_list(
        list_annotations(&target, command.query.as_deref())?,
        command.json,
    )
}

async fn annotate_search_command(target: Target, command: AnnotateSearchCommand) -> Result<i32> {
    emit_annotation_list(
        list_annotations(&target, Some(&command.query))?,
        command.json,
    )
}

async fn annotate_prune_command(
    target: Target,
    mut client: RpcClient,
    command: AnnotatePruneCommand,
) -> Result<i32> {
    let annotations = list_annotations(&target, None)?;
    let mut stale = Vec::new();
    for item in &annotations {
        match client
            .request(
                "thread/read",
                json!({"threadId": item.thread_id, "includeTurns": false}),
                |_| {},
            )
            .await
        {
            Ok(_) => {}
            Err(err) if is_thread_not_found_error(&err, "thread/read", &item.thread_id) => {
                stale.push(item.thread_id.clone());
            }
            Err(err) => return Err(err),
        }
    }
    let removed = if command.dry_run || stale.is_empty() {
        0
    } else {
        clear_annotations(&target, &stale)?
    };
    let output = json!({
        "server": target.server,
        "checked": annotations.len(),
        "stale": stale,
        "removed": removed,
        "dryRun": command.dry_run
    });
    if command.json {
        print_json(&output)?;
    } else {
        print_key_values(&[
            ("server", output["server"].as_str().unwrap_or("")),
            ("checked", &output["checked"].to_string()),
            (
                "stale",
                &output["stale"]
                    .as_array()
                    .unwrap_or(&Vec::new())
                    .len()
                    .to_string(),
            ),
            ("removed", &output["removed"].to_string()),
            ("dryRun", if command.dry_run { "true" } else { "false" }),
        ]);
    }
    Ok(0)
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

async fn usage_command(
    target: Target,
    mut client: RpcClient,
    command: UsageCommand,
) -> Result<i32> {
    let result = client
        .request("account/rateLimits/read", json!({}), |_| {})
        .await?;
    let output = json!({
        "server": target.server,
        "rateLimits": result["rateLimits"],
        "rateLimitsByLimitId": result["rateLimitsByLimitId"],
    });
    if command.json {
        print_json(&output)?;
    } else {
        print_usage(&output);
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
    let mut rows = vec![
        ("server", result["server"].as_str().unwrap_or("")),
        ("id", thread["id"].as_str().unwrap_or("")),
        ("name", thread["name"].as_str().unwrap_or("")),
        ("cwd", thread["cwd"].as_str().unwrap_or("")),
        ("status", thread["status"]["type"].as_str().unwrap_or("")),
    ];
    let annotation = thread["annotation"]["text"].as_str();
    if let Some(annotation) = annotation.filter(|text| !text.contains('\n')) {
        rows.push(("annotation", annotation));
    }
    print_key_values(&rows);
    if let Some(annotation) = annotation.filter(|text| text.contains('\n')) {
        println!("annotation");
        for line in annotation.lines() {
            println!("  {line}");
        }
    }
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

fn print_annotation_detail(result: &Value) {
    print_key_values(&[
        ("server", result["server"].as_str().unwrap_or("")),
        ("threadId", result["threadId"].as_str().unwrap_or("")),
        (
            "annotation",
            result["annotation"]["text"].as_str().unwrap_or(""),
        ),
        (
            "updated",
            &format_timestamp(result["annotation"]["updatedAt"].as_i64()),
        ),
    ]);
}

fn emit_annotation_list(items: Vec<AnnotationListItem>, json_out: bool) -> Result<i32> {
    if json_out {
        let annotations = items
            .iter()
            .map(|item| {
                json!({
                    "server": item.server,
                    "endpoint": item.endpoint,
                    "threadId": item.thread_id,
                    "annotation": item.annotation
                })
            })
            .collect::<Vec<_>>();
        print_json(&json!({"annotations": annotations}))?;
    } else {
        print_table(
            &["UPDATED", "SERVER", "THREAD ID", "ANNOTATION"],
            items
                .iter()
                .map(|item| {
                    vec![
                        table_cell(format_timestamp(Some(item.annotation.updated_at))),
                        table_cell(&item.server),
                        table_cell(&item.thread_id),
                        capped_cell(&item.annotation.text, ANNOTATION_WIDTH),
                    ]
                })
                .collect(),
        );
    }
    Ok(0)
}

fn print_usage(result: &Value) {
    let snapshots = usage_snapshots(result);
    let summary = usage_summary_snapshot(result, &snapshots);
    let plan = summary
        .and_then(|snapshot| snapshot["planType"].as_str())
        .unwrap_or("unknown");
    let reached = summary
        .and_then(|snapshot| snapshot["rateLimitReachedType"].as_str())
        .unwrap_or("none");
    let credits = summary
        .and_then(|snapshot| snapshot.get("credits"))
        .map(format_credits)
        .unwrap_or_else(|| "unknown".to_string());
    let key_values = [
        ("server", result["server"].as_str().unwrap_or("")),
        ("plan", plan),
        ("credits", credits.as_str()),
        ("limitReached", reached),
    ];
    print_key_values(&key_values);

    if snapshots.is_empty() {
        return;
    }

    println!();
    print_table(
        &["LIMIT", "WINDOW", "USED", "REACHED", "RESETS", "DURATION"],
        snapshots
            .iter()
            .flat_map(|(limit_key, snapshot)| usage_window_rows(limit_key, snapshot))
            .collect(),
    );
}

fn usage_summary_snapshot<'a>(
    result: &'a Value,
    snapshots: &'a [(String, &'a Value)],
) -> Option<&'a Value> {
    if !result["rateLimits"].is_null() {
        Some(&result["rateLimits"])
    } else {
        snapshots.first().map(|(_, snapshot)| *snapshot)
    }
}

fn usage_snapshots(result: &Value) -> Vec<(String, &Value)> {
    let mut snapshots = result["rateLimitsByLimitId"]
        .as_object()
        .map(|by_id| {
            by_id
                .iter()
                .map(|(limit_id, snapshot)| (limit_id.clone(), snapshot))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    snapshots.sort_by(|left, right| left.0.cmp(&right.0));
    if snapshots.is_empty() && !result["rateLimits"].is_null() {
        let fallback_id = result["rateLimits"]["limitId"]
            .as_str()
            .unwrap_or("codex")
            .to_string();
        snapshots.push((fallback_id, &result["rateLimits"]));
    }
    snapshots
}

fn usage_window_rows(limit_key: &str, snapshot: &Value) -> Vec<Vec<TableCell>> {
    let limit = usage_limit_label(limit_key, snapshot);
    let reached = snapshot["rateLimitReachedType"]
        .as_str()
        .unwrap_or("none")
        .to_string();
    ["primary", "secondary"]
        .into_iter()
        .filter_map(|window_name| {
            let window = snapshot.get(window_name)?;
            if window.is_null() {
                return None;
            }
            Some(vec![
                table_cell(limit.clone()),
                table_cell(window_name),
                table_cell(format_used_percent(&window["usedPercent"])),
                table_cell(reached.clone()),
                table_cell(format_timestamp(window["resetsAt"].as_i64())),
                table_cell(format_duration_mins(window["windowDurationMins"].as_i64())),
            ])
        })
        .collect()
}

fn usage_limit_label(limit_key: &str, snapshot: &Value) -> String {
    let limit_id = snapshot["limitId"].as_str().unwrap_or(limit_key);
    match snapshot["limitName"].as_str() {
        Some(name) if name != limit_id => format!("{name} ({limit_id})"),
        Some(name) => name.to_string(),
        None => limit_id.to_string(),
    }
}

fn format_credits(credits: &Value) -> String {
    if credits["unlimited"].as_bool().unwrap_or(false) {
        return "unlimited".to_string();
    }
    match (
        credits["hasCredits"].as_bool(),
        credits["balance"]
            .as_str()
            .filter(|balance| !balance.is_empty()),
    ) {
        (Some(true), Some(balance)) => balance.to_string(),
        (Some(true), None) => "available".to_string(),
        (Some(false), Some(balance)) => format!("depleted ({balance})"),
        (Some(false), None) => "depleted".to_string(),
        (None, Some(balance)) => balance.to_string(),
        (None, None) => "unknown".to_string(),
    }
}

fn format_used_percent(value: &Value) -> String {
    if let Some(percent) = value.as_i64() {
        return format!("{percent}%");
    }
    if let Some(percent) = value.as_f64() {
        return format!("{percent:.0}%");
    }
    "unknown".to_string()
}

fn format_duration_mins(minutes: Option<i64>) -> String {
    match minutes {
        Some(1) => "1 min".to_string(),
        Some(minutes) => format!("{minutes} mins"),
        None => "unknown".to_string(),
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
        let empty_items = Vec::new();
        let items = output[label].as_array().unwrap_or(&empty_items);
        let show_annotations = items.iter().any(|item| {
            item.get("thread")
                .unwrap_or(item)
                .get("annotation")
                .is_some()
        });
        let mut headers = match projection {
            ThreadProjection::Direct => vec!["UPDATED", "STATUS", "TITLE/PREVIEW"],
            ThreadProjection::SearchResult => {
                vec!["UPDATED", "STATUS", "TITLE/PREVIEW", "SNIPPET"]
            }
        };
        if show_annotations {
            headers.push("ANNOTATION");
        }
        headers.push("THREAD ID");
        let rows = items
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
                if show_annotations {
                    row.push(capped_cell(
                        thread["annotation"]["text"].as_str().unwrap_or(""),
                        ANNOTATION_WIDTH,
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

fn turn_status(turn: &Value) -> &'static str {
    match turn["status"].as_str().unwrap_or("inProgress") {
        "completed" => "completed",
        "interrupted" => "interrupted",
        "failed" => "failed",
        _ => "inProgress",
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
    if text.contains("auth token requires")
        || text.contains("cannot set both `auth_token`")
        || text.contains("endpoint must")
    {
        return 2;
    }
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
