mod events;
#[cfg(feature = "tui-syntax-highlighting")]
mod highlight;
mod input;
mod keymap;
mod prefs;
mod state;
mod views;

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::io::{self, IsTerminal, Write};
use std::panic;
use std::process::{Command, ExitStatus, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use crossterm::cursor::{MoveTo, Show};
use crossterm::event::{
    DisableMouseCapture, EnableMouseCapture, Event, EventStream, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers, MouseEvent, MouseEventKind,
};
use crossterm::execute;
use crossterm::terminal::{
    Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use futures_util::{StreamExt, future::join_all};
use pulldown_cmark::{CodeBlockKind, Event as MarkdownEvent, Options, Parser, Tag, TagEnd};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use serde_json::{Value, json};
use tokio::sync::mpsc;

use crate::annotations::{clear_annotation, set_annotation};
use crate::cli::{ItemsView, SortKey, TuiCommand};
use crate::config::{Endpoint, Target};
use crate::errors::usage_error;
use crate::rpc::{RpcClient, RpcRequestError};
use crate::session::{
    ListThreadsRequest, SearchThreadsRequest, ShowThreadRequest, ThreadStartOptions,
    ThreadStatusRequest, list_threads, read_thread_detail, search_threads, set_thread_archived,
    set_thread_name, start_thread, thread_id_from_start, thread_status,
};
use crate::time_filter::parse_since;
use crate::tui::events::{
    AppEvent, BrowserQuery, DetailPageDirection, FetchRequest, PreviewRequest,
};
use crate::tui::input::{InputAction, ModeKind};
use crate::tui::prefs::{SortDirectionPref, load_prefs_with_warning, save_prefs};
use crate::tui::state::{
    BrowserSource, ComposeState, ComposeTarget, DetailJump, DetailState, MessageBlock, MessageLine,
    MessageLineKind, MessageSpan, Mode, NewSessionDraft, SendMode, StreamAssistantItem,
    StreamState, StreamStatus, ThreadRow, TuiInit, TuiState,
};
use crate::turns::{
    AttachTurnOptions, ControlledTurnWaitOptions, StartedTurn, TurnControl, TurnStartOptions,
    TurnWaitOutcome, attach_turn, interrupt_turn, start_turn as start_turn_request, steer_turn,
    wait_for_turn_controlled,
};

const DEFAULT_LIMIT: u32 = 50;
const DETAIL_TURN_LIMIT: u32 = 10;
const DETAIL_JUMP_TURN_LIMIT: u32 = 100;
const PREVIEW_TURN_LIMIT: u32 = 3;
const DETAIL_FOLLOW_REFRESH_SECS: u64 = 5;
const AUTO_REFRESH_MIN_SECS: u64 = 5;
const AUTO_REFRESH_MAX_SECS: u64 = 300;
const AUTO_REFRESH_STEP_SECS: u64 = 5;
const SCHEDULER_TICK_SECS: u64 = 5;
const TURN_SCAN_LIMIT: u32 = 200;
const TURN_WAIT_TIMEOUT_SECS: u64 = 60 * 60;
const FOLLOW_NEXT_TURN_POLL_ATTEMPTS: usize = 8;
const FOLLOW_NEXT_TURN_POLL_INTERVAL_MS: u64 = 500;
const CODEX_BIN_ENV: &str = "CODEX_THREADS_CODEX_BIN";
const CODEX_REMOTE_AUTH_ENV: &str = "CODEX_THREADS_CODEX_REMOTE_AUTH_TOKEN";

#[derive(Debug, Clone)]
struct TuiTargets {
    targets: BTreeMap<String, Target>,
}

impl TuiTargets {
    fn new(targets: Vec<Target>) -> Result<Self> {
        if targets.is_empty() {
            return Err(usage_error("tui requires at least one server target"));
        }
        Ok(Self {
            targets: targets
                .into_iter()
                .map(|target| (target.server.clone(), target))
                .collect(),
        })
    }

    fn is_multi(&self) -> bool {
        self.targets.len() > 1
    }

    fn all(&self) -> impl Iterator<Item = &Target> {
        self.targets.values()
    }

    fn get(&self, server: &str) -> Result<&Target> {
        self.targets
            .get(server)
            .ok_or_else(|| usage_error(format!("unknown TUI server `{server}`")))
    }
}

pub async fn run_tui(targets: Vec<Target>, command: TuiCommand, yolo: bool) -> Result<i32> {
    if !io::stdout().is_terminal() {
        return Err(usage_error("tui requires an interactive terminal"));
    }
    let targets = TuiTargets::new(targets)?;

    let since = command.since.as_deref().map(parse_since).transpose()?;
    let limit = command.limit.unwrap_or(DEFAULT_LIMIT);
    let loaded_prefs = load_prefs_with_warning();
    let prefs = loaded_prefs.prefs;
    let descending = if command.asc {
        false
    } else if command.desc {
        true
    } else {
        prefs.browser.direction == SortDirectionPref::Desc
    };
    let mut state = TuiState::new(TuiInit {
        query: command.query,
        since,
        cwd: command.cwd,
        archived: command.archived,
        limit,
        sort: command.sort,
        descending,
        prefs,
    });
    state.browser.multi_server = targets.is_multi();
    state.browser.last_error = loaded_prefs.warning;

    let _guard = TerminalGuard::enter()?;
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend)?;
    terminal.clear()?;

    let (fetch_tx, fetch_rx) = mpsc::channel(32);
    let (preview_tx, preview_rx) = mpsc::channel(8);
    let (app_tx, mut app_rx) = mpsc::unbounded_channel();
    tokio::spawn(fetch_worker(targets.clone(), fetch_rx, app_tx.clone()));
    tokio::spawn(preview_worker(targets.clone(), preview_rx, app_tx.clone()));
    spawn_shutdown_signal_task(app_tx.clone());
    schedule_browser_refresh(&mut state, &fetch_tx).await?;

    let mut events = Some(EventStream::new());
    let mut tick = tokio::time::interval(Duration::from_secs(SCHEDULER_TICK_SECS));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        if state.force_terminal_clear {
            terminal.clear()?;
            state.force_terminal_clear = false;
        }
        let size = terminal.size()?;
        let area = ratatui::layout::Rect::new(0, 0, size.width, size.height);
        views::sync_viewport_state(&mut state, area);
        terminal.draw(|frame| views::draw(frame, &state))?;
        if state.should_quit {
            break;
        }

        tokio::select! {
            maybe_event = next_terminal_event(&mut events) => {
                if let Some(Ok(event)) = maybe_event {
                    let previous_selection = state.selected_thread_key();
                    let outcome = handle_terminal_event(event, &mut state, &targets, yolo, &fetch_tx, &app_tx)
                        .await?;
                    if let Some(launch) = outcome.codex_launch {
                        suspend_terminal_events(&mut events);
                        let status = launch_codex_resume(&launch.launch);
                        resume_terminal_events(&mut events);
                        match status {
                            Ok(status) if status.success() => {
                                state.set_notice("codex exited");
                            }
                            Ok(status) => {
                                state.set_notice(format!("codex exited with {status}"));
                            }
                            Err(error) => {
                                state.set_notice(format!("failed to launch codex: {error}"));
                            }
                        }
                        state.force_terminal_clear = true;
                        schedule_browser_refresh(&mut state, &fetch_tx).await?;
                        if state.detail.as_ref().is_some_and(|detail| {
                            detail.server == launch.server && detail.thread_id == launch.thread_id
                        }) {
                            schedule_detail_refresh_for_server(
                                &mut state,
                                &fetch_tx,
                                launch.server,
                                launch.thread_id,
                            )
                            .await?;
                        }
                    }
                    detach_stream_if_browser_selection_changed(&mut state, previous_selection);
                    schedule_selected_preview_if_needed(&mut state, &preview_tx).await?;
                }
            }
            Some(event) = app_rx.recv() => {
                let auto_attach_initial_active =
                    initial_browser_load_needs_auto_attach(&event, &state);
                let refresh_detail_after_stream = stream_finish_detail_thread(&event, &state);
                let refresh_after_archive = archive_changed_thread(&event);
                let refresh_after_rename = rename_changed_thread(&event);
                let refresh_after_submit = turn_submitted_thread(&event);
                let refresh_after_load = loaded_thread(&event);
                let follow_after_stream = stream_finish_follow_thread(&event, &state);
                let detail_loaded = matches!(event, AppEvent::DetailLoaded { .. });
                let detail_replace_loaded = matches!(
                    event,
                    AppEvent::DetailLoaded {
                        page_direction: DetailPageDirection::Replace,
                        ..
                    }
                );
                handle_app_event(event, &mut state);
                if detail_loaded && !state.should_quit {
                    schedule_pending_detail_jump(&mut state, &fetch_tx).await?;
                }
                if detail_replace_loaded && !state.should_quit {
                    auto_attach_open_detail_thread_if_active(
                        &mut state,
                        targets.clone(),
                        yolo,
                        app_tx.clone(),
                    );
                }
                if auto_attach_initial_active && !state.should_quit {
                    auto_attach_selected_browser_thread_if_active(
                        &mut state,
                        targets.clone(),
                        yolo,
                        app_tx.clone(),
                    );
                }
                if let Some((server, thread_id)) = refresh_detail_after_stream
                    && !state.should_quit
                    && state
                        .detail
                        .as_ref()
                        .is_some_and(|detail| detail.server == server && detail.thread_id == thread_id)
                {
                    schedule_detail_refresh_for_server(&mut state, &fetch_tx, server, thread_id)
                        .await?;
                }
                if refresh_after_archive.is_some() && !state.should_quit {
                    schedule_browser_refresh(&mut state, &fetch_tx).await?;
                }
                if refresh_after_rename.is_some() && !state.should_quit {
                    schedule_browser_refresh(&mut state, &fetch_tx).await?;
                }
                if refresh_after_submit.is_some() && !state.should_quit {
                    schedule_browser_refresh(&mut state, &fetch_tx).await?;
                }
                if let Some((server, thread_id)) = follow_after_stream
                    && !state.should_quit
                {
                    follow_thread_stream_if_active(
                        &mut state,
                        targets.clone(),
                        yolo,
                        server,
                        thread_id,
                        app_tx.clone(),
                    );
                }
                if let Some((server, thread_id)) = refresh_after_load
                    && !state.should_quit
                {
                    schedule_browser_refresh(&mut state, &fetch_tx).await?;
                    if state
                        .detail
                        .as_ref()
                        .is_some_and(|detail| detail.server == server && detail.thread_id == thread_id)
                    {
                        schedule_detail_refresh_for_server(&mut state, &fetch_tx, server, thread_id)
                            .await?;
                    }
                }
                schedule_selected_preview_if_needed(&mut state, &preview_tx).await?;
            }
            _ = tick.tick() => {
                state.clear_expired_notice();
                if state.browser.auto_refresh
                    && !state.browser.loading
                    && state.browser.last_refresh_at.is_none_or(|last| {
                        last.elapsed() >= Duration::from_secs(state.browser.auto_refresh_seconds)
                    })
                {
                    schedule_browser_refresh(&mut state, &fetch_tx).await?;
                }
                if let Some((server, thread_id)) = detail_follow_refresh_thread(&state) {
                    schedule_detail_refresh_for_server(&mut state, &fetch_tx, server, thread_id)
                        .await?;
                }
            }
        }
    }

    state.prefs.refresh.auto = state.browser.auto_refresh;
    state.prefs.refresh.interval_seconds = state.browser.auto_refresh_seconds;
    state.prefs.browser.sort = state.browser.sort;
    state.prefs.browser.direction = if state.browser.descending {
        SortDirectionPref::Desc
    } else {
        SortDirectionPref::Asc
    };
    let _ = save_prefs(&state.prefs);
    terminal.clear()?;
    Ok(0)
}

type PanicHook = Box<dyn Fn(&panic::PanicHookInfo<'_>) + Sync + Send + 'static>;

struct TerminalGuard {
    previous_hook: Arc<Mutex<Option<PanicHook>>>,
}

impl TerminalGuard {
    fn enter() -> Result<Self> {
        enter_terminal()?;
        let previous_hook = panic::take_hook();
        let previous_hook = Arc::new(Mutex::new(Some(previous_hook)));
        let hook_for_panic = Arc::clone(&previous_hook);
        panic::set_hook(Box::new(move |info| {
            restore_terminal();
            if let Ok(hook) = hook_for_panic.lock()
                && let Some(previous) = hook.as_ref()
            {
                previous(info);
            } else {
                eprintln!("{info}");
            }
        }));
        Ok(Self { previous_hook })
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        restore_terminal();
        let previous_hook = self
            .previous_hook
            .lock()
            .ok()
            .and_then(|mut hook| hook.take());
        if let Some(previous_hook) = previous_hook {
            panic::set_hook(previous_hook);
        }
    }
}

fn enter_terminal() -> Result<()> {
    enable_raw_mode()?;
    execute!(io::stdout(), EnterAlternateScreen, EnableMouseCapture)?;
    Ok(())
}

fn force_terminal_redraw() {
    let _ = execute!(io::stdout(), Clear(ClearType::All), MoveTo(0, 0));
    let _ = io::stdout().flush();
}

fn restore_terminal() {
    let _ = disable_raw_mode();
    let _ = execute!(
        io::stdout(),
        DisableMouseCapture,
        Show,
        LeaveAlternateScreen
    );
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CodexResumeLaunch {
    program: OsString,
    args: Vec<OsString>,
    env: Vec<(OsString, OsString)>,
}

#[derive(Debug, Default)]
struct TerminalEventOutcome {
    codex_launch: Option<PendingCodexLaunch>,
}

impl TerminalEventOutcome {
    fn none() -> Self {
        Self::default()
    }
}

#[derive(Debug)]
struct PendingCodexLaunch {
    launch: CodexResumeLaunch,
    server: String,
    thread_id: String,
}

async fn next_terminal_event(events: &mut Option<EventStream>) -> Option<io::Result<Event>> {
    match events.as_mut() {
        Some(events) => events.next().await,
        None => std::future::pending().await,
    }
}

fn suspend_terminal_events(events: &mut Option<EventStream>) {
    let _ = events.take();
    // Dropping EventStream only signals crossterm's background poll thread; give
    // it a brief chance to stop reading the tty before the child inherits stdin.
    std::thread::sleep(Duration::from_millis(20));
}

fn resume_terminal_events(events: &mut Option<EventStream>) {
    *events = Some(EventStream::new());
}

fn build_codex_resume_launch(
    target: &Target,
    thread_id: &str,
    cwd: &str,
    yolo: bool,
) -> CodexResumeLaunch {
    let mut args = vec![
        OsString::from("resume"),
        OsString::from(thread_id),
        OsString::from("--remote"),
        OsString::from(target.endpoint.display()),
    ];
    let mut env = Vec::new();
    if let Endpoint::WebSocket {
        auth_token: Some(token),
        ..
    } = &target.endpoint
    {
        args.push(OsString::from("--remote-auth-token-env"));
        args.push(OsString::from(CODEX_REMOTE_AUTH_ENV));
        env.push((OsString::from(CODEX_REMOTE_AUTH_ENV), OsString::from(token)));
    }
    if yolo {
        args.push(OsString::from("--dangerously-bypass-approvals-and-sandbox"));
    }
    args.push(OsString::from("--cd"));
    args.push(OsString::from(cwd));
    CodexResumeLaunch {
        program: std::env::var_os(CODEX_BIN_ENV).unwrap_or_else(|| OsString::from("codex")),
        args,
        env,
    }
}

fn launch_codex_resume(launch: &CodexResumeLaunch) -> Result<ExitStatus> {
    let _ = io::stdout().flush();
    restore_terminal();
    let status = Command::new(&launch.program)
        .args(&launch.args)
        .envs(launch.env.iter().cloned())
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .with_context(|| format!("failed to launch `{}`", launch.program.to_string_lossy()));
    enter_terminal().context("failed to restore tui terminal after codex exited")?;
    force_terminal_redraw();
    status
}

#[cfg(unix)]
fn spawn_shutdown_signal_task(app_tx: mpsc::UnboundedSender<AppEvent>) {
    tokio::spawn(async move {
        use tokio::signal::unix::{SignalKind, signal};

        let Ok(mut terminate) = signal(SignalKind::terminate()) else {
            return;
        };
        let Ok(mut hangup) = signal(SignalKind::hangup()) else {
            return;
        };
        tokio::select! {
            _ = terminate.recv() => {}
            _ = hangup.recv() => {}
        }
        let _ = app_tx.send(AppEvent::ShutdownSignal);
    });
}

#[cfg(not(unix))]
fn spawn_shutdown_signal_task(_app_tx: mpsc::UnboundedSender<AppEvent>) {}

async fn fetch_worker(
    targets: TuiTargets,
    mut fetch_rx: mpsc::Receiver<FetchRequest>,
    app_tx: mpsc::UnboundedSender<AppEvent>,
) {
    let mut clients = RpcClientCache::default();
    while let Some(request) = fetch_rx.recv().await {
        match request {
            FetchRequest::Browser { epoch, query } => {
                let result = fetch_browser_all(&targets, &mut clients, query).await;
                match result {
                    Ok((rows, next_cursor, backwards_cursor, warning)) => {
                        let _ = app_tx.send(AppEvent::BrowserLoaded {
                            epoch,
                            rows,
                            next_cursor,
                            backwards_cursor,
                            warning,
                        });
                    }
                    Err(err) => {
                        let _ = app_tx.send(AppEvent::BrowserLoadFailed {
                            epoch,
                            error: err.to_string(),
                        });
                    }
                }
            }
            FetchRequest::Detail {
                epoch,
                server,
                thread_id,
                cursor,
                limit,
                page_direction,
            } => {
                let result = fetch_detail_cached(
                    &targets,
                    &mut clients,
                    server,
                    thread_id,
                    cursor,
                    limit,
                    epoch,
                )
                .await;
                match result {
                    Ok(detail) => {
                        let _ = app_tx.send(AppEvent::DetailLoaded {
                            epoch,
                            detail: Box::new(detail),
                            page_direction,
                        });
                    }
                    Err(err) => {
                        let _ = app_tx.send(AppEvent::DetailLoadFailed {
                            epoch,
                            error: err.to_string(),
                        });
                    }
                }
            }
            FetchRequest::LoadThread { server, thread_id } => {
                let result =
                    load_thread_cached(&targets, &mut clients, &server, thread_id.clone()).await;
                match result {
                    Ok(status) => {
                        let _ = app_tx.send(AppEvent::ThreadLoaded {
                            server,
                            thread_id,
                            status,
                        });
                    }
                    Err(err) => {
                        let _ = app_tx.send(AppEvent::ThreadLoadFailed {
                            server,
                            thread_id,
                            error: err.to_string(),
                        });
                    }
                }
            }
        }
    }
}

async fn preview_worker(
    targets: TuiTargets,
    mut preview_rx: mpsc::Receiver<PreviewRequest>,
    app_tx: mpsc::UnboundedSender<AppEvent>,
) {
    let mut clients = RpcClientCache::default();
    while let Some(mut request) = preview_rx.recv().await {
        while let Ok(newer) = preview_rx.try_recv() {
            request = newer;
        }
        let result = fetch_preview_cached(
            &targets,
            &mut clients,
            &request.server,
            request.thread_id.clone(),
        )
        .await;
        match result {
            Ok(text) => {
                let _ = app_tx.send(AppEvent::PreviewLoaded {
                    epoch: request.epoch,
                    server: request.server,
                    thread_id: request.thread_id,
                    messages: text,
                });
            }
            Err(err) => {
                let _ = app_tx.send(AppEvent::PreviewLoadFailed {
                    epoch: request.epoch,
                    server: request.server,
                    thread_id: request.thread_id,
                    error: err.to_string(),
                });
            }
        }
    }
}

#[derive(Default)]
struct RpcClientCache {
    clients: BTreeMap<String, RpcClient>,
}

impl RpcClientCache {
    fn take(&mut self, server: &str) -> Option<RpcClient> {
        self.clients.remove(server)
    }

    fn insert(&mut self, server: String, client: RpcClient) {
        self.clients.insert(server, client);
    }
}

fn keep_client_after_error(err: &anyhow::Error) -> bool {
    err.downcast_ref::<RpcRequestError>().is_some()
}

async fn fetch_browser(
    target: &Target,
    client: &mut RpcClient,
    query: BrowserQuery,
) -> Result<(Vec<ThreadRow>, Option<String>, Option<String>)> {
    let output = match query.source {
        BrowserSource::List => {
            list_threads(
                target,
                client,
                ListThreadsRequest {
                    limit: query.limit,
                    cursor: query.cursor,
                    since: query.since,
                    cwd: query.cwd,
                    archived: query.archived,
                    sort: query.sort,
                    asc: !query.descending,
                    desc: query.descending,
                },
            )
            .await?
        }
        BrowserSource::Search => {
            let mut output = search_threads(
                target,
                client,
                SearchThreadsRequest {
                    query: query.query,
                    limit: query.limit,
                    cursor: query.cursor,
                    since: query.since,
                    archived: query.archived,
                },
            )
            .await?;
            if let Some(cwd) = query.cwd {
                filter_search_cwd(&mut output, &cwd);
            }
            output
        }
    };
    let rows = output["data"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .map(|item| {
            thread_row(
                target.server.clone(),
                item,
                query.source,
                query.relative_updated,
            )
        })
        .collect();
    Ok((
        rows,
        output["nextCursor"].as_str().map(str::to_string),
        output["backwardsCursor"].as_str().map(str::to_string),
    ))
}

async fn fetch_browser_all(
    targets: &TuiTargets,
    clients: &mut RpcClientCache,
    query: BrowserQuery,
) -> Result<BrowserMergeResult> {
    let fetches = targets.all().map(|target| {
        let target = target.clone();
        let query = query.clone();
        let cached = clients.take(&target.server);
        async move {
            let server = target.server.clone();
            let (rows, client) = fetch_browser_with_client(target, cached, query).await;
            (BrowserFetchOutcome { server, rows }, client)
        }
    });
    let mut outcomes = Vec::new();
    for (outcome, client) in join_all(fetches).await {
        if let Some(client) = client {
            clients.insert(outcome.server.clone(), client);
        }
        outcomes.push(outcome);
    }
    merge_browser_fetch_outcomes(outcomes, targets.is_multi())
}

async fn fetch_browser_with_client(
    target: Target,
    cached: Option<RpcClient>,
    query: BrowserQuery,
) -> (
    std::result::Result<BrowserPageRows, String>,
    Option<RpcClient>,
) {
    let mut client = match cached {
        Some(client) => client,
        None => match RpcClient::connect(&target.endpoint).await {
            Ok(client) => client,
            Err(err) => return (Err(err.to_string()), None),
        },
    };
    match fetch_browser(&target, &mut client, query).await {
        Ok(rows) => (Ok(rows), Some(client)),
        Err(err) => {
            let keep = keep_client_after_error(&err);
            (Err(err.to_string()), keep.then_some(client))
        }
    }
}

async fn fetch_detail_cached(
    targets: &TuiTargets,
    clients: &mut RpcClientCache,
    server: String,
    thread_id: String,
    cursor: Option<String>,
    limit: u32,
    epoch: u64,
) -> Result<DetailState> {
    let target = targets.get(&server)?.clone();
    let cached = clients.take(&server);
    let (result, client) =
        fetch_detail_with_client(target, cached, thread_id, cursor, limit, epoch).await;
    if let Some(client) = client {
        clients.insert(server, client);
    }
    result
}

async fn fetch_detail_with_client(
    target: Target,
    cached: Option<RpcClient>,
    thread_id: String,
    cursor: Option<String>,
    limit: u32,
    epoch: u64,
) -> (Result<DetailState>, Option<RpcClient>) {
    let mut client = match cached {
        Some(client) => client,
        None => match RpcClient::connect(&target.endpoint).await {
            Ok(client) => client,
            Err(err) => return (Err(err), None),
        },
    };
    match fetch_detail(&target, &mut client, thread_id, cursor, limit, epoch).await {
        Ok(detail) => (Ok(detail), Some(client)),
        Err(err) => {
            let keep = keep_client_after_error(&err);
            (Err(err), keep.then_some(client))
        }
    }
}

async fn load_thread_cached(
    targets: &TuiTargets,
    clients: &mut RpcClientCache,
    server: &str,
    thread_id: String,
) -> Result<Value> {
    let target = targets.get(server)?.clone();
    let cached = clients.take(server);
    let (result, client) = load_thread_with_client(target, cached, thread_id).await;
    if let Some(client) = client {
        clients.insert(server.to_string(), client);
    }
    result
}

async fn load_thread_with_client(
    target: Target,
    cached: Option<RpcClient>,
    thread_id: String,
) -> (Result<Value>, Option<RpcClient>) {
    let mut client = match cached {
        Some(client) => client,
        None => match RpcClient::connect(&target.endpoint).await {
            Ok(client) => client,
            Err(err) => return (Err(err), None),
        },
    };
    let result = thread_status(
        &target,
        &mut client,
        ThreadStatusRequest {
            thread_id,
            load: true,
            turn_scan_limit: TURN_SCAN_LIMIT,
        },
    )
    .await;
    match result {
        Ok(status) => (Ok(status), Some(client)),
        Err(err) => {
            let keep = keep_client_after_error(&err);
            (Err(err), keep.then_some(client))
        }
    }
}

async fn fetch_preview_cached(
    targets: &TuiTargets,
    clients: &mut RpcClientCache,
    server: &str,
    thread_id: String,
) -> Result<Vec<MessageBlock>> {
    let target = targets.get(server)?.clone();
    let cached = clients.take(server);
    let (result, client) = fetch_preview_with_client(target, cached, thread_id).await;
    if let Some(client) = client {
        clients.insert(server.to_string(), client);
    }
    result
}

async fn fetch_preview_with_client(
    target: Target,
    cached: Option<RpcClient>,
    thread_id: String,
) -> (Result<Vec<MessageBlock>>, Option<RpcClient>) {
    let mut client = match cached {
        Some(client) => client,
        None => match RpcClient::connect(&target.endpoint).await {
            Ok(client) => client,
            Err(err) => return (Err(err), None),
        },
    };
    match fetch_preview(&target, &mut client, thread_id).await {
        Ok(messages) => (Ok(messages), Some(client)),
        Err(err) => {
            let keep = keep_client_after_error(&err);
            (Err(err), keep.then_some(client))
        }
    }
}

#[derive(Debug)]
struct BrowserFetchOutcome {
    server: String,
    rows: std::result::Result<BrowserPageRows, String>,
}

type BrowserPageRows = (Vec<ThreadRow>, Option<String>, Option<String>);
type BrowserMergeResult = (
    Vec<ThreadRow>,
    Option<String>,
    Option<String>,
    Option<String>,
);

fn merge_browser_fetch_outcomes(
    outcomes: Vec<BrowserFetchOutcome>,
    is_multi: bool,
) -> Result<BrowserMergeResult> {
    let mut rows = Vec::new();
    let mut next_cursor = None;
    let mut backwards_cursor = None;
    let mut errors = Vec::new();
    let mut successes = 0usize;
    for outcome in outcomes {
        match outcome.rows {
            Ok((mut target_rows, target_next, target_backwards)) => {
                successes += 1;
                rows.append(&mut target_rows);
                if !is_multi {
                    next_cursor = target_next;
                    backwards_cursor = target_backwards;
                }
            }
            Err(err) if is_multi => errors.push(format!("{}: {err}", outcome.server)),
            Err(err) => return Err(anyhow::anyhow!(err)),
        }
    }
    if successes == 0 && !errors.is_empty() {
        return Err(anyhow::anyhow!(errors.join("; ")));
    }
    rows.sort_by(|left, right| {
        thread_row_updated_epoch(right)
            .cmp(&thread_row_updated_epoch(left))
            .then_with(|| left.server.cmp(&right.server))
            .then_with(|| left.id.cmp(&right.id))
    });
    let warning = if errors.is_empty() {
        None
    } else {
        Some(format!("some servers failed: {}", errors.join("; ")))
    };
    Ok((rows, next_cursor, backwards_cursor, warning))
}

async fn fetch_detail(
    target: &Target,
    client: &mut RpcClient,
    thread_id: String,
    cursor: Option<String>,
    limit: u32,
    epoch: u64,
) -> Result<DetailState> {
    let mut output = read_thread_detail(
        target,
        client,
        ShowThreadRequest {
            thread_id: thread_id.clone(),
            last: limit,
            cursor: cursor.clone(),
            asc: false,
            desc: true,
            items: ItemsView::Full,
        },
    )
    .await?;
    normalize_detail_turns_for_display(&mut output);
    let status = thread_status(
        target,
        client,
        ThreadStatusRequest {
            thread_id: thread_id.clone(),
            load: false,
            turn_scan_limit: TURN_SCAN_LIMIT,
        },
    )
    .await
    .ok();
    Ok(detail_state_for_server(
        target.server.clone(),
        output,
        status,
        thread_id,
        epoch,
        cursor,
    ))
}

async fn fetch_preview(
    target: &Target,
    client: &mut RpcClient,
    thread_id: String,
) -> Result<Vec<MessageBlock>> {
    let mut output = read_thread_detail(
        target,
        client,
        ShowThreadRequest {
            thread_id: thread_id.clone(),
            last: PREVIEW_TURN_LIMIT,
            cursor: None,
            asc: false,
            desc: true,
            items: ItemsView::Full,
        },
    )
    .await?;
    normalize_detail_turns_for_display(&mut output);
    Ok(detail_state_for_server(target.server.clone(), output, None, thread_id, 0, None).messages)
}

async fn schedule_browser_refresh(
    state: &mut TuiState,
    fetch_tx: &mpsc::Sender<FetchRequest>,
) -> Result<()> {
    schedule_browser_page(state, fetch_tx, state.browser.current_cursor.clone()).await
}

async fn schedule_thread_load(
    state: &mut TuiState,
    fetch_tx: &mpsc::Sender<FetchRequest>,
    server: String,
    thread_id: String,
) -> Result<()> {
    state.set_notice(format!("loading {thread_id}..."));
    match fetch_tx.try_send(FetchRequest::LoadThread {
        server,
        thread_id: thread_id.clone(),
    }) {
        Ok(()) => Ok(()),
        Err(err) => {
            state.set_notice(format!("failed to schedule load {thread_id}: {err}"));
            Ok(())
        }
    }
}

async fn schedule_browser_reset(
    state: &mut TuiState,
    fetch_tx: &mpsc::Sender<FetchRequest>,
) -> Result<()> {
    schedule_browser_page(state, fetch_tx, None).await
}

async fn schedule_browser_page(
    state: &mut TuiState,
    fetch_tx: &mpsc::Sender<FetchRequest>,
    cursor: Option<String>,
) -> Result<()> {
    state.browser.epoch += 1;
    state.browser.loading = true;
    state.browser.last_error = None;
    state.browser.current_cursor = cursor.clone();
    let query = BrowserQuery {
        source: state.browser.source,
        query: state.browser.query.clone(),
        cursor,
        limit: state.browser.limit,
        since: state.browser.since,
        cwd: state.browser.cwd.clone(),
        archived: state.browser.archived,
        sort: state.browser.sort,
        descending: state.browser.descending,
        relative_updated: state.prefs.browser.relative_updated,
    };
    match fetch_tx.try_send(FetchRequest::Browser {
        epoch: state.browser.epoch,
        query,
    }) {
        Ok(()) => Ok(()),
        Err(err) => {
            state.browser.loading = false;
            state.browser.last_error = Some(format!("failed to schedule browser refresh: {err}"));
            Ok(())
        }
    }
}

async fn schedule_detail_load(
    state: &mut TuiState,
    fetch_tx: &mpsc::Sender<FetchRequest>,
    server: String,
    thread_id: String,
) -> Result<()> {
    schedule_detail_page(
        state,
        fetch_tx,
        server,
        thread_id,
        None,
        DetailPageDirection::Replace,
    )
    .await
}

async fn schedule_detail_refresh_for_server(
    state: &mut TuiState,
    fetch_tx: &mpsc::Sender<FetchRequest>,
    server: String,
    thread_id: String,
) -> Result<()> {
    let cursor = state
        .detail
        .as_ref()
        .filter(|detail| detail.server == server && detail.thread_id == thread_id)
        .and_then(|detail| detail.current_cursor.clone());
    schedule_detail_page(
        state,
        fetch_tx,
        server,
        thread_id,
        cursor,
        DetailPageDirection::Replace,
    )
    .await
}

async fn schedule_detail_page(
    state: &mut TuiState,
    fetch_tx: &mpsc::Sender<FetchRequest>,
    server: String,
    thread_id: String,
    cursor: Option<String>,
    page_direction: DetailPageDirection,
) -> Result<()> {
    schedule_detail_page_with_limit(
        state,
        fetch_tx,
        server,
        thread_id,
        cursor,
        page_direction,
        DETAIL_TURN_LIMIT,
    )
    .await
}

async fn schedule_detail_page_with_limit(
    state: &mut TuiState,
    fetch_tx: &mpsc::Sender<FetchRequest>,
    server: String,
    thread_id: String,
    cursor: Option<String>,
    page_direction: DetailPageDirection,
    limit: u32,
) -> Result<()> {
    let epoch = state
        .detail
        .as_ref()
        .map(|detail| detail.epoch + 1)
        .unwrap_or(1);
    if let Some(detail) = &mut state.detail
        && detail.server == server
        && detail.thread_id == thread_id
    {
        detail.epoch = epoch;
        detail.loading = true;
        detail.last_error = None;
        detail.current_cursor = cursor.clone();
    } else {
        state.detail = Some(DetailState {
            server: server.clone(),
            thread_id: thread_id.clone(),
            title: thread_id.clone(),
            status: "loading".to_string(),
            annotation: None,
            messages: Vec::new(),
            scroll: u16::MAX,
            search_query: String::new(),
            matches: Vec::new(),
            match_index: 0,
            next_cursor: None,
            backwards_cursor: None,
            current_cursor: cursor.clone(),
            active_turn_id: None,
            loading: true,
            epoch,
            last_refresh_at: None,
            viewport_height: None,
            viewport_width: None,
            last_error: None,
        });
    }
    if page_direction == DetailPageDirection::Replace {
        state.pending_detail_jump = None;
    }
    state.mode = Mode::Detail;
    match fetch_tx.try_send(FetchRequest::Detail {
        epoch,
        server,
        thread_id,
        cursor,
        limit,
        page_direction,
    }) {
        Ok(()) => Ok(()),
        Err(err) => {
            if let Some(detail) = &mut state.detail {
                detail.loading = false;
                detail.last_error = Some(format!("failed to schedule detail load: {err}"));
            }
            Ok(())
        }
    }
}

fn active_server_for_thread(state: &TuiState, thread_id: &str) -> Option<String> {
    if let Some(row) = state
        .browser
        .rows
        .get(state.browser.selected)
        .filter(|row| row.id == thread_id)
        && !matches!(state.mode, Mode::Detail)
    {
        return Some(row.server.clone());
    }
    if let Some(detail) = &state.detail
        && detail.thread_id == thread_id
    {
        return Some(detail.server.clone());
    }
    state
        .browser
        .rows
        .iter()
        .find(|row| row.id == thread_id)
        .map(|row| row.server.clone())
}

fn resolve_event_server(
    state: &TuiState,
    event: &Value,
    thread_id: Option<&str>,
) -> Option<String> {
    event["server"]
        .as_str()
        .map(str::to_string)
        .or_else(|| state.stream.as_ref().map(|stream| stream.server.clone()))
        .or_else(|| thread_id.and_then(|thread_id| active_server_for_thread(state, thread_id)))
}

async fn schedule_selected_preview_if_needed(
    state: &mut TuiState,
    preview_tx: &mpsc::Sender<PreviewRequest>,
) -> Result<()> {
    if !state.prefs.browser.preview_pane || !matches!(state.mode, Mode::Browser) {
        return Ok(());
    }
    let Some((server, thread_id)) = state.selected_thread_key() else {
        return Ok(());
    };
    if state.browser.preview.server.as_deref() == Some(server.as_str())
        && state.browser.preview.thread_id.as_deref() == Some(thread_id.as_str())
        && (state.browser.preview.loading
            || !state.browser.preview.messages.is_empty()
            || state.browser.preview.error.is_some())
    {
        return Ok(());
    }
    let epoch = state.set_preview_loading(server.clone(), thread_id.clone());
    match preview_tx.try_send(PreviewRequest {
        epoch,
        server,
        thread_id,
    }) {
        Ok(()) => Ok(()),
        Err(err) => {
            state.browser.preview.loading = false;
            state.browser.preview.error = Some(format!("failed to schedule preview load: {err}"));
            Ok(())
        }
    }
}

async fn handle_terminal_event(
    event: Event,
    state: &mut TuiState,
    targets: &TuiTargets,
    yolo: bool,
    fetch_tx: &mpsc::Sender<FetchRequest>,
    app_tx: &mpsc::UnboundedSender<AppEvent>,
) -> Result<TerminalEventOutcome> {
    let key = match event {
        Event::Key(key) => key,
        Event::Mouse(mouse) => {
            if handle_mouse_event(mouse, state) {
                schedule_detail_older_if_available(state, fetch_tx).await?;
            }
            return Ok(TerminalEventOutcome::none());
        }
        _ => return Ok(TerminalEventOutcome::none()),
    };
    if key.kind != KeyEventKind::Press {
        return Ok(TerminalEventOutcome::none());
    }
    if key.modifiers.contains(KeyModifiers::CONTROL) && matches!(key.code, KeyCode::Char('c')) {
        if let Some(stream) = &state.stream
            && matches!(
                stream.status,
                StreamStatus::Starting | StreamStatus::Running
            )
            && let Some(turn_id) = stream.turn_id.clone()
        {
            state.mode = Mode::ConfirmInterrupt {
                server: stream.server.clone(),
                thread_id: stream.thread_id.clone(),
                turn_id: Some(turn_id),
                return_to_detail: matches!(state.mode, Mode::Detail),
            };
        } else {
            state.should_quit = true;
        }
        return Ok(TerminalEventOutcome::none());
    }

    let current_mode = std::mem::replace(&mut state.mode, Mode::Browser);
    match current_mode {
        Mode::SearchInput { draft } => {
            return handle_text_input(
                key,
                draft,
                ModeKind::Search,
                |value, state| {
                    state.update_query(value);
                    Ok(InputAction::RefreshBrowser)
                },
                state,
                fetch_tx,
            )
            .await
            .map(|()| TerminalEventOutcome::none());
        }
        Mode::MessageSearchInput { draft } => {
            return handle_text_input(
                key,
                draft,
                ModeKind::MessageSearch,
                |value, state| {
                    state.update_message_search(value);
                    Ok(InputAction::None)
                },
                state,
                fetch_tx,
            )
            .await
            .map(|()| TerminalEventOutcome::none());
        }
        Mode::AnnotationInput {
            server,
            thread_id,
            draft,
            return_to_detail,
        } => {
            let target = targets.get(&server)?;
            return handle_annotation_input(key, target, state, thread_id, draft, return_to_detail)
                .map(|()| TerminalEventOutcome::none());
        }
        Mode::RenameInput {
            server,
            thread_id,
            draft,
            return_to_detail,
        } => {
            let target = targets.get(&server)?;
            return handle_rename_input(
                key,
                target,
                state,
                thread_id,
                draft,
                return_to_detail,
                app_tx,
            )
            .map(|()| TerminalEventOutcome::none());
        }
        Mode::NewSessionServerMenu {
            mut draft,
            servers,
            mut selected,
        } => {
            match key.code {
                KeyCode::Esc => state.mode = Mode::Browser,
                KeyCode::Down | KeyCode::Char('j') => {
                    selected = (selected + 1).min(servers.len().saturating_sub(1));
                    state.mode = Mode::NewSessionServerMenu {
                        draft,
                        servers,
                        selected,
                    };
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    selected = selected.saturating_sub(1);
                    state.mode = Mode::NewSessionServerMenu {
                        draft,
                        servers,
                        selected,
                    };
                }
                KeyCode::Enter => {
                    if let Some(server) = servers.get(selected) {
                        draft.server = server.clone();
                    }
                    state.mode = Mode::NewSessionCwdInput { draft };
                }
                _ => {
                    state.mode = Mode::NewSessionServerMenu {
                        draft,
                        servers,
                        selected,
                    };
                }
            }
            return Ok(TerminalEventOutcome::none());
        }
        Mode::NewSessionCwdInput { draft } => {
            handle_new_session_cwd_input(key, state, draft);
            return Ok(TerminalEventOutcome::none());
        }
        Mode::NewSessionTitleInput { draft } => {
            handle_new_session_title_input(key, state, draft);
            return Ok(TerminalEventOutcome::none());
        }
        Mode::Compose(compose) => {
            let target = targets.get(compose_target_server(&compose.target))?;
            return handle_compose_input(key, state, compose.clone(), target, yolo, app_tx)
                .await
                .map(|()| TerminalEventOutcome::none());
        }
        Mode::FilterMenu => {
            match key.code {
                KeyCode::Esc => state.mode = Mode::Browser,
                KeyCode::Char('a') => {
                    state.browser.archived = !state.browser.archived;
                    state.mode = Mode::Browser;
                    schedule_browser_refresh(state, fetch_tx).await?;
                }
                _ => state.mode = Mode::FilterMenu,
            }
            return Ok(TerminalEventOutcome::none());
        }
        Mode::SortMenu => {
            match key.code {
                KeyCode::Esc => state.mode = Mode::Browser,
                KeyCode::Char('u') if state.browser.source == BrowserSource::List => {
                    state.browser.sort = Some(SortKey::Updated);
                    state.mode = Mode::Browser;
                    schedule_browser_refresh(state, fetch_tx).await?;
                }
                KeyCode::Char('c') if state.browser.source == BrowserSource::List => {
                    state.browser.sort = Some(SortKey::Created);
                    state.mode = Mode::Browser;
                    schedule_browser_refresh(state, fetch_tx).await?;
                }
                KeyCode::Char('d') if state.browser.source == BrowserSource::List => {
                    state.browser.descending = !state.browser.descending;
                    state.mode = Mode::Browser;
                    schedule_browser_refresh(state, fetch_tx).await?;
                }
                _ => state.mode = Mode::SortMenu,
            }
            return Ok(TerminalEventOutcome::none());
        }
        Mode::ColumnsMenu => {
            let mut changed = false;
            match key.code {
                KeyCode::Esc => state.mode = Mode::Browser,
                KeyCode::Char('1') => {
                    state.prefs.browser.columns.status = !state.prefs.browser.columns.status;
                    changed = true;
                }
                KeyCode::Char('2') => {
                    state.prefs.browser.columns.updated = !state.prefs.browser.columns.updated;
                    changed = true;
                }
                KeyCode::Char('3') => {
                    state.prefs.browser.columns.cwd = !state.prefs.browser.columns.cwd;
                    changed = true;
                }
                KeyCode::Char('4') => {
                    state.prefs.browser.columns.annotation =
                        !state.prefs.browser.columns.annotation;
                    changed = true;
                }
                KeyCode::Char('5') => {
                    state.prefs.browser.relative_updated = !state.prefs.browser.relative_updated;
                    changed = true;
                    schedule_browser_refresh(state, fetch_tx).await?;
                }
                KeyCode::Char('t') => {
                    state.browser.auto_refresh = !state.browser.auto_refresh;
                    state.prefs.refresh.auto = state.browser.auto_refresh;
                    changed = true;
                    if state.browser.auto_refresh {
                        schedule_browser_refresh(state, fetch_tx).await?;
                    }
                }
                KeyCode::Char('-') | KeyCode::Char('_') => {
                    changed = adjust_auto_refresh_interval(state, -(AUTO_REFRESH_STEP_SECS as i64));
                }
                KeyCode::Char('+') | KeyCode::Char('=') => {
                    changed = adjust_auto_refresh_interval(state, AUTO_REFRESH_STEP_SECS as i64);
                }
                _ => {}
            }
            if !matches!(key.code, KeyCode::Esc) {
                state.mode = Mode::ColumnsMenu;
            }
            if changed {
                let _ = save_prefs(&state.prefs);
            }
            return Ok(TerminalEventOutcome::none());
        }
        Mode::ConfirmInterrupt {
            server,
            thread_id,
            turn_id,
            return_to_detail,
        } => {
            let target = targets.get(&server)?;
            let return_mode = if return_to_detail {
                Mode::Detail
            } else {
                Mode::Browser
            };
            match key.code {
                KeyCode::Esc => state.mode = return_mode,
                KeyCode::Enter => {
                    state.mode = return_mode;
                    spawn_interrupt_task(target.clone(), thread_id, turn_id, app_tx.clone());
                }
                _ => {
                    state.mode = Mode::ConfirmInterrupt {
                        server,
                        thread_id,
                        turn_id,
                        return_to_detail,
                    }
                }
            }
            return Ok(TerminalEventOutcome::none());
        }
        Mode::ConfirmArchive {
            server,
            thread_id,
            archived,
            return_to_detail,
        } => {
            let target = targets.get(&server)?;
            let return_mode = if return_to_detail {
                Mode::Detail
            } else {
                Mode::Browser
            };
            match key.code {
                KeyCode::Esc => state.mode = return_mode,
                KeyCode::Enter => {
                    state.mode = return_mode;
                    state.set_notice(format!(
                        "{} {thread_id}...",
                        if archived { "archiving" } else { "unarchiving" }
                    ));
                    spawn_archive_task(target.clone(), thread_id, archived, app_tx.clone());
                }
                _ => {
                    state.mode = Mode::ConfirmArchive {
                        server,
                        thread_id,
                        archived,
                        return_to_detail,
                    }
                }
            }
            return Ok(TerminalEventOutcome::none());
        }
        Mode::ConfirmOpenCodex {
            server,
            thread_id,
            cwd,
            return_to_detail,
        } => {
            let target = targets.get(&server)?;
            let return_mode = if return_to_detail {
                Mode::Detail
            } else {
                Mode::Browser
            };
            match key.code {
                KeyCode::Esc => state.mode = return_mode,
                KeyCode::Enter => {
                    state.mode = return_mode;
                    detach_stream(state);
                    return Ok(TerminalEventOutcome {
                        codex_launch: Some(PendingCodexLaunch {
                            launch: build_codex_resume_launch(target, &thread_id, &cwd, yolo),
                            server,
                            thread_id,
                        }),
                    });
                }
                _ => {
                    state.mode = Mode::ConfirmOpenCodex {
                        server,
                        thread_id,
                        cwd,
                        return_to_detail,
                    }
                }
            }
            return Ok(TerminalEventOutcome::none());
        }
        Mode::Help => {
            state.mode = Mode::Browser;
            return Ok(TerminalEventOutcome::none());
        }
        other => state.mode = other,
    }

    if let Some(goto_key) = normalized_goto_key(&key) {
        handle_goto_key(goto_key, state, fetch_tx).await?;
        return Ok(TerminalEventOutcome::none());
    }
    state.pending_goto_top = false;

    match key.code {
        KeyCode::Char('q') => {
            detach_stream(state);
            state.should_quit = true;
        }
        KeyCode::Char('?') => state.mode = Mode::Help,
        KeyCode::Char('r') => match state.mode {
            _ if stream_is_running(state) => {
                if let Some(control) = &state.stream_control {
                    let _ = control.send(TurnControl::PollNow);
                }
            }
            Mode::Detail => {
                if let Some((server, thread_id)) = state
                    .detail
                    .as_ref()
                    .map(|detail| (detail.server.clone(), detail.thread_id.clone()))
                {
                    schedule_detail_refresh_for_server(state, fetch_tx, server, thread_id).await?;
                }
            }
            _ => schedule_browser_refresh(state, fetch_tx).await?,
        },
        KeyCode::Char('R') => match state.mode {
            Mode::Detail => {
                if let Some((server, thread_id)) = state
                    .detail
                    .as_ref()
                    .map(|detail| (detail.server.clone(), detail.thread_id.clone()))
                {
                    schedule_detail_load(state, fetch_tx, server, thread_id).await?;
                }
            }
            _ => schedule_browser_reset(state, fetch_tx).await?,
        },
        KeyCode::Char(']') => match state.mode {
            Mode::Detail => {
                jump_to_bottom(state);
            }
            _ => {
                if let Some(cursor) = state.browser.next_cursor.clone() {
                    schedule_browser_page(state, fetch_tx, Some(cursor)).await?;
                }
            }
        },
        KeyCode::Char('[') => match state.mode {
            Mode::Detail => {
                jump_to_top(state);
            }
            _ => {
                if let Some(cursor) = state.browser.backwards_cursor.clone() {
                    schedule_browser_page(state, fetch_tx, Some(cursor)).await?;
                }
            }
        },
        KeyCode::Char('/') => match state.mode {
            Mode::Detail => {
                let draft = state
                    .detail
                    .as_ref()
                    .map(|detail| detail.search_query.clone())
                    .unwrap_or_default();
                state.mode = Mode::MessageSearchInput { draft };
            }
            _ => {
                state.mode = Mode::SearchInput {
                    draft: state.browser.query.clone(),
                }
            }
        },
        KeyCode::Char('n') if matches!(state.mode, Mode::Detail) => {
            state.next_message_match();
        }
        KeyCode::Char('n') if matches!(state.mode, Mode::Browser) => {
            start_new_session_flow(state, targets);
        }
        KeyCode::Char('N') if matches!(state.mode, Mode::Detail) => {
            state.previous_message_match();
        }
        KeyCode::Char('a') => {
            if let Some((server, thread_id)) = active_thread_key(state) {
                let draft = active_annotation(state).unwrap_or_default();
                let return_to_detail = matches!(state.mode, Mode::Detail);
                state.mode = Mode::AnnotationInput {
                    server,
                    thread_id,
                    draft,
                    return_to_detail,
                };
            }
        }
        KeyCode::Char('A') => {
            if let Some((server, thread_id)) = active_thread_key(state) {
                let archived = !active_thread_is_archived(state);
                let return_to_detail = matches!(state.mode, Mode::Detail);
                state.mode = Mode::ConfirmArchive {
                    server,
                    thread_id,
                    archived,
                    return_to_detail,
                };
            }
        }
        KeyCode::Char('e') => {
            if let Some((server, thread_id)) = active_thread_key(state) {
                let draft = active_thread_title(state).unwrap_or_default();
                let return_to_detail = matches!(state.mode, Mode::Detail);
                state.mode = Mode::RenameInput {
                    server,
                    thread_id,
                    draft,
                    return_to_detail,
                };
            }
        }
        KeyCode::Char('o') => {
            if let Some((server, thread_id)) = active_thread_key(state) {
                if let Some(cwd) = active_thread_cwd(state, &server, &thread_id) {
                    let return_to_detail = matches!(state.mode, Mode::Detail);
                    state.mode = Mode::ConfirmOpenCodex {
                        server,
                        thread_id,
                        cwd,
                        return_to_detail,
                    };
                } else {
                    state.set_notice("thread cwd unavailable; refresh or load first");
                }
            }
        }
        KeyCode::Char('y') => {
            copy_active_thread_id(state)?;
        }
        KeyCode::Char('m') => {
            if let Some((server, thread_id)) = active_thread_key(state) {
                open_message_action(state, server, thread_id);
            }
        }
        KeyCode::Char('l') => {
            if let Some((server, thread_id)) = active_thread_key(state) {
                schedule_thread_load(state, fetch_tx, server, thread_id).await?;
            }
        }
        KeyCode::Char('T') => {
            if matches!(state.mode, Mode::Detail)
                && let Some(detail) = &state.detail
                && let Some(turn_id) = detail.active_turn_id.clone()
            {
                let server = detail.server.clone();
                let thread_id = detail.thread_id.clone();
                detach_stream(state);
                let (control_tx, control_rx) = mpsc::unbounded_channel();
                let stream_id = state.allocate_stream_id();
                state.stream = Some(StreamState::new_for_server_with_id(
                    stream_id,
                    server.clone(),
                    thread_id.clone(),
                    Some(turn_id.clone()),
                    StreamStatus::Running,
                    true,
                ));
                state.stream_control = Some(control_tx);
                spawn_attach_task(
                    targets.get(&server)?.clone(),
                    thread_id.clone(),
                    turn_id,
                    yolo,
                    control_rx,
                    stream_id,
                    app_tx.clone(),
                );
            } else if matches!(state.mode, Mode::Browser)
                && let Some((server, thread_id)) = state.selected_thread_key()
            {
                detach_stream(state);
                let stream_id = state.allocate_stream_id();
                state.stream = Some(StreamState::new_for_server_with_id(
                    stream_id,
                    server.clone(),
                    thread_id.clone(),
                    None,
                    StreamStatus::Starting,
                    true,
                ));
                let (control_tx, control_rx) = mpsc::unbounded_channel();
                state.stream_control = Some(control_tx);
                spawn_browser_attach_task(
                    targets.get(&server)?.clone(),
                    thread_id,
                    yolo,
                    control_rx,
                    stream_id,
                    app_tx.clone(),
                );
            }
        }
        KeyCode::Char('i') => match state.mode {
            Mode::Detail => {
                if let Some(detail) = &state.detail
                    && let Some(turn_id) = detail.active_turn_id.clone()
                {
                    state.mode = Mode::ConfirmInterrupt {
                        server: detail.server.clone(),
                        thread_id: detail.thread_id.clone(),
                        turn_id: Some(turn_id),
                        return_to_detail: true,
                    };
                }
            }
            Mode::Browser => {
                if let Some((server, thread_id)) = state.selected_thread_key()
                    && selected_thread_is_running(state, &server, &thread_id)
                {
                    let turn_id = active_turn_hint_for_compose(state, &server, &thread_id);
                    state.mode = Mode::ConfirmInterrupt {
                        server,
                        thread_id,
                        turn_id,
                        return_to_detail: false,
                    };
                }
            }
            _ => {}
        },
        KeyCode::Char('f') if matches!(state.mode, Mode::Browser) => state.mode = Mode::FilterMenu,
        KeyCode::Char('c') if matches!(state.mode, Mode::Browser) => state.mode = Mode::ColumnsMenu,
        KeyCode::Char('t') => {
            state.browser.auto_refresh = !state.browser.auto_refresh;
            state.prefs.refresh.auto = state.browser.auto_refresh;
            let _ = save_prefs(&state.prefs);
            if state.browser.auto_refresh && matches!(state.mode, Mode::Browser) {
                schedule_browser_refresh(state, fetch_tx).await?;
            }
        }
        KeyCode::Char('p') if matches!(state.mode, Mode::Browser) => {
            state.prefs.browser.preview_pane = !state.prefs.browser.preview_pane;
            if !state.prefs.browser.preview_pane {
                state.browser.preview = Default::default();
            }
            let _ = save_prefs(&state.prefs);
        }
        KeyCode::Char('s') if matches!(state.mode, Mode::Browser) => state.mode = Mode::SortMenu,
        KeyCode::Down | KeyCode::Char('j') => match state.mode {
            Mode::Detail => {
                scroll_detail(state, 1);
            }
            _ => state.move_selection(1),
        },
        KeyCode::Up | KeyCode::Char('k') => match state.mode {
            Mode::Detail => {
                if scroll_detail(state, -1) {
                    schedule_detail_older_if_available(state, fetch_tx).await?;
                }
            }
            _ => state.move_selection(-1),
        },
        KeyCode::Enter => match state.mode {
            Mode::Browser => {
                if let Some((server, thread_id)) = state.selected_thread_key() {
                    schedule_detail_load(state, fetch_tx, server, thread_id).await?;
                }
            }
            Mode::Detail => {
                if let Some((server, thread_id)) = active_thread_key(state) {
                    open_message_action(state, server, thread_id);
                }
            }
            _ => {}
        },
        KeyCode::Esc => match state.mode {
            Mode::Detail => {
                unlink_detail_session(state);
            }
            _ => state.mode = Mode::Browser,
        },
        _ => {}
    }
    Ok(TerminalEventOutcome::none())
}

async fn handle_goto_key(
    code: KeyCode,
    state: &mut TuiState,
    fetch_tx: &mpsc::Sender<FetchRequest>,
) -> Result<bool> {
    match code {
        KeyCode::Char('g') => {
            if state.pending_goto_top {
                begin_detail_jump_or_browser_jump(state, fetch_tx, DetailJump::Start).await?;
                state.pending_goto_top = false;
            } else {
                state.pending_goto_top = true;
            }
            Ok(true)
        }
        KeyCode::Char('G') | KeyCode::End => {
            begin_detail_jump_or_browser_jump(state, fetch_tx, DetailJump::End).await?;
            state.pending_goto_top = false;
            Ok(true)
        }
        KeyCode::Home => {
            begin_detail_jump_or_browser_jump(state, fetch_tx, DetailJump::Start).await?;
            state.pending_goto_top = false;
            Ok(true)
        }
        _ => {
            state.pending_goto_top = false;
            Ok(false)
        }
    }
}

fn normalized_goto_key(key: &KeyEvent) -> Option<KeyCode> {
    match key.code {
        KeyCode::Char('g') if key.modifiers.contains(KeyModifiers::SHIFT) => {
            Some(KeyCode::Char('G'))
        }
        KeyCode::Char('g') | KeyCode::Char('G') | KeyCode::Home | KeyCode::End => Some(key.code),
        _ => None,
    }
}

async fn begin_detail_jump_or_browser_jump(
    state: &mut TuiState,
    fetch_tx: &mpsc::Sender<FetchRequest>,
    jump: DetailJump,
) -> Result<()> {
    if matches!(state.mode, Mode::Detail) {
        state.pending_detail_jump = Some(jump);
        schedule_pending_detail_jump(state, fetch_tx).await
    } else {
        match jump {
            DetailJump::Start => jump_to_top(state),
            DetailJump::End => jump_to_bottom(state),
        }
        Ok(())
    }
}

async fn schedule_pending_detail_jump(
    state: &mut TuiState,
    fetch_tx: &mpsc::Sender<FetchRequest>,
) -> Result<()> {
    let Some(jump) = state.pending_detail_jump else {
        return Ok(());
    };
    let Some(detail) = state.detail.as_ref() else {
        state.pending_detail_jump = None;
        return Ok(());
    };
    if detail.loading {
        return Ok(());
    }
    let server = detail.server.clone();
    let thread_id = detail.thread_id.clone();
    let (cursor, direction) = match jump {
        DetailJump::Start => (detail.next_cursor.clone(), DetailPageDirection::Older),
        DetailJump::End => (detail.backwards_cursor.clone(), DetailPageDirection::Newer),
    };
    if let Some(cursor) = cursor {
        if detail.current_cursor.as_deref() == Some(cursor.as_str()) {
            state.pending_detail_jump = None;
            state.set_notice("stopped history jump: cursor did not advance");
            return Ok(());
        }
        schedule_detail_page_with_limit(
            state,
            fetch_tx,
            server,
            thread_id,
            Some(cursor),
            direction,
            DETAIL_JUMP_TURN_LIMIT,
        )
        .await
    } else {
        state.pending_detail_jump = None;
        match jump {
            DetailJump::Start => jump_to_top(state),
            DetailJump::End => jump_to_bottom(state),
        }
        Ok(())
    }
}

fn jump_to_top(state: &mut TuiState) {
    state.pending_detail_jump = None;
    match state.mode {
        Mode::Detail => {
            if let Some(detail) = &mut state.detail {
                detail.scroll = 0;
            }
        }
        _ => state.browser.selected = 0,
    }
}

fn jump_to_bottom(state: &mut TuiState) {
    state.pending_detail_jump = None;
    match state.mode {
        Mode::Detail => {
            if let Some(detail) = &mut state.detail {
                detail.scroll = detail.bottom_scroll_position();
            }
        }
        _ => {
            state.browser.selected = state.browser.rows.len().saturating_sub(1);
        }
    }
}

fn start_new_session_flow(state: &mut TuiState, targets: &TuiTargets) {
    let server = state
        .selected_thread_key()
        .map(|(server, _)| server)
        .or_else(|| targets.all().next().map(|target| target.server.clone()))
        .unwrap_or_default();
    let cwd = state
        .browser
        .rows
        .get(state.browser.selected)
        .map(|row| row.cwd.clone())
        .filter(|cwd| !cwd.is_empty())
        .or_else(|| {
            std::env::current_dir()
                .ok()
                .map(|dir| dir.to_string_lossy().into_owned())
        })
        .unwrap_or_default();
    let draft = NewSessionDraft {
        server,
        cwd,
        title: String::new(),
    };
    if targets.is_multi() {
        let servers: Vec<String> = targets.all().map(|target| target.server.clone()).collect();
        let selected = servers
            .iter()
            .position(|name| *name == draft.server)
            .unwrap_or(0);
        state.mode = Mode::NewSessionServerMenu {
            draft,
            servers,
            selected,
        };
    } else {
        state.mode = Mode::NewSessionCwdInput { draft };
    }
}

fn handle_new_session_cwd_input(key: KeyEvent, state: &mut TuiState, mut draft: NewSessionDraft) {
    match key.code {
        KeyCode::Esc => state.mode = Mode::Browser,
        KeyCode::Enter => {
            let cwd = draft.cwd.trim().to_string();
            if cwd.is_empty() {
                state.set_notice("cwd cannot be empty");
                state.mode = Mode::NewSessionCwdInput { draft };
            } else {
                draft.cwd = cwd;
                state.mode = Mode::NewSessionTitleInput { draft };
            }
        }
        KeyCode::Backspace => {
            draft.cwd.pop();
            state.mode = Mode::NewSessionCwdInput { draft };
        }
        KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            draft.cwd.clear();
            state.mode = Mode::NewSessionCwdInput { draft };
        }
        KeyCode::Char(ch) => {
            draft.cwd.push(ch);
            state.mode = Mode::NewSessionCwdInput { draft };
        }
        _ => state.mode = Mode::NewSessionCwdInput { draft },
    }
}

fn handle_new_session_title_input(key: KeyEvent, state: &mut TuiState, mut draft: NewSessionDraft) {
    match key.code {
        KeyCode::Esc => state.mode = Mode::Browser,
        KeyCode::Enter => {
            let title = draft.title.trim().to_string();
            state.mode = Mode::Compose(ComposeState {
                target: ComposeTarget::NewThread {
                    server: draft.server,
                    cwd: draft.cwd,
                    title: (!title.is_empty()).then_some(title),
                },
                text: String::new(),
                send_mode: SendMode::Stream,
                return_to_detail: false,
            });
        }
        KeyCode::Backspace => {
            draft.title.pop();
            state.mode = Mode::NewSessionTitleInput { draft };
        }
        KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            draft.title.clear();
            state.mode = Mode::NewSessionTitleInput { draft };
        }
        KeyCode::Char(ch) => {
            draft.title.push(ch);
            state.mode = Mode::NewSessionTitleInput { draft };
        }
        _ => state.mode = Mode::NewSessionTitleInput { draft },
    }
}

fn open_message_action(state: &mut TuiState, server: String, thread_id: String) {
    let return_to_detail = matches!(state.mode, Mode::Detail);
    let (target, send_mode) = default_compose_target(state, server, thread_id, return_to_detail);
    state.mode = Mode::Compose(ComposeState {
        target,
        text: String::new(),
        send_mode,
        return_to_detail,
    });
}

fn default_compose_target(
    state: &TuiState,
    server: String,
    thread_id: String,
    return_to_detail: bool,
) -> (ComposeTarget, SendMode) {
    if let Some(target) = steer_target_for_thread(state, &server, &thread_id, return_to_detail) {
        (target, SendMode::Stream)
    } else {
        (
            ComposeTarget::NewTurn { server, thread_id },
            SendMode::Stream,
        )
    }
}

fn toggle_compose_target_or_mode(state: &TuiState, mut compose: ComposeState) -> ComposeState {
    match &compose.target {
        ComposeTarget::Steer {
            server, thread_id, ..
        }
        | ComposeTarget::SteerSelected { server, thread_id } => {
            compose.target = ComposeTarget::NewTurn {
                server: server.clone(),
                thread_id: thread_id.clone(),
            };
            compose.send_mode = SendMode::Stream;
        }
        ComposeTarget::NewTurn { server, thread_id } => {
            if let Some(target) =
                steer_target_for_thread(state, server, thread_id, compose.return_to_detail)
            {
                compose.target = target;
                compose.send_mode = SendMode::Stream;
            } else {
                compose.send_mode = match compose.send_mode {
                    SendMode::Stream => SendMode::NoWait,
                    SendMode::NoWait => SendMode::Stream,
                };
            }
        }
        ComposeTarget::NewThread { .. } => {}
    }
    compose
}

fn steer_target_for_thread(
    state: &TuiState,
    server: &str,
    thread_id: &str,
    return_to_detail: bool,
) -> Option<ComposeTarget> {
    if return_to_detail
        && let Some(detail) = &state.detail
        && detail.server == server
        && detail.thread_id == thread_id
        && let Some(turn_id) = detail.active_turn_id.clone()
    {
        return Some(ComposeTarget::Steer {
            server: server.to_string(),
            thread_id: thread_id.to_string(),
            turn_id,
        });
    }
    if let Some(stream) = &state.stream
        && stream.server == server
        && stream.thread_id == thread_id
        && matches!(
            stream.status,
            StreamStatus::Starting | StreamStatus::Running
        )
    {
        if let Some(turn_id) = stream.turn_id.clone() {
            return Some(ComposeTarget::Steer {
                server: server.to_string(),
                thread_id: thread_id.to_string(),
                turn_id,
            });
        }
        return Some(ComposeTarget::SteerSelected {
            server: server.to_string(),
            thread_id: thread_id.to_string(),
        });
    }
    if state
        .browser
        .rows
        .iter()
        .any(|row| row.server == server && row.id == thread_id && row.is_running())
    {
        return Some(ComposeTarget::SteerSelected {
            server: server.to_string(),
            thread_id: thread_id.to_string(),
        });
    }
    None
}

async fn schedule_detail_older_if_available(
    state: &mut TuiState,
    fetch_tx: &mpsc::Sender<FetchRequest>,
) -> Result<()> {
    let Some((server, thread_id, cursor)) = state.detail.as_ref().and_then(|detail| {
        if detail.loading {
            return None;
        }
        if detail
            .next_cursor
            .as_deref()
            .is_some_and(|cursor| detail.current_cursor.as_deref() == Some(cursor))
        {
            return None;
        }
        detail
            .next_cursor
            .clone()
            .map(|cursor| (detail.server.clone(), detail.thread_id.clone(), cursor))
    }) else {
        return Ok(());
    };
    schedule_detail_page(
        state,
        fetch_tx,
        server,
        thread_id,
        Some(cursor),
        DetailPageDirection::Older,
    )
    .await
}

fn handle_mouse_event(mouse: MouseEvent, state: &mut TuiState) -> bool {
    state.pending_goto_top = false;
    let delta: isize = match mouse.kind {
        MouseEventKind::ScrollUp => -3,
        MouseEventKind::ScrollDown => 3,
        _ => return false,
    };
    match state.mode {
        Mode::Detail
        | Mode::MessageSearchInput { .. }
        | Mode::Compose(_)
        | Mode::ConfirmInterrupt {
            return_to_detail: true,
            ..
        }
        | Mode::ConfirmArchive {
            return_to_detail: true,
            ..
        }
        | Mode::ConfirmOpenCodex {
            return_to_detail: true,
            ..
        } => scroll_detail(state, delta),
        Mode::Browser
        | Mode::SearchInput { .. }
        | Mode::FilterMenu
        | Mode::SortMenu
        | Mode::ColumnsMenu
        | Mode::Help
        | Mode::NewSessionServerMenu { .. }
        | Mode::NewSessionCwdInput { .. }
        | Mode::NewSessionTitleInput { .. }
        | Mode::ConfirmArchive {
            return_to_detail: false,
            ..
        }
        | Mode::ConfirmInterrupt {
            return_to_detail: false,
            ..
        }
        | Mode::ConfirmOpenCodex {
            return_to_detail: false,
            ..
        } => {
            state.move_selection(delta);
            false
        }
        Mode::AnnotationInput {
            return_to_detail: true,
            ..
        }
        | Mode::RenameInput {
            return_to_detail: true,
            ..
        } => scroll_detail(state, delta),
        Mode::AnnotationInput {
            return_to_detail: false,
            ..
        }
        | Mode::RenameInput {
            return_to_detail: false,
            ..
        } => {
            state.move_selection(delta);
            false
        }
    }
}

fn scroll_detail(state: &mut TuiState, delta: isize) -> bool {
    let Some(detail) = &mut state.detail else {
        return false;
    };
    if delta.is_negative() {
        detail.scroll = detail.scroll.saturating_sub(delta.unsigned_abs() as u16);
    } else {
        detail.scroll = detail
            .scroll
            .saturating_add(delta as u16)
            .min(detail.max_scroll());
    }
    delta.is_negative() && detail.scroll == 0 && detail.next_cursor.is_some() && !detail.loading
}

async fn handle_text_input<F>(
    key: KeyEvent,
    mut draft: String,
    mode_kind: ModeKind,
    mut on_submit: F,
    state: &mut TuiState,
    fetch_tx: &mpsc::Sender<FetchRequest>,
) -> Result<()>
where
    F: FnMut(String, &mut TuiState) -> Result<InputAction>,
{
    match key.code {
        KeyCode::Esc => {
            state.mode = match mode_kind {
                ModeKind::MessageSearch => Mode::Detail,
                _ => Mode::Browser,
            }
        }
        KeyCode::Enter => {
            let value = draft.clone();
            if on_submit(value, state)? == InputAction::RefreshBrowser {
                state.mode = Mode::Browser;
                schedule_browser_refresh(state, fetch_tx).await?;
            } else {
                state.mode = match mode_kind {
                    ModeKind::MessageSearch => Mode::Detail,
                    _ => Mode::Browser,
                };
            }
        }
        KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            if on_submit(String::new(), state)? == InputAction::RefreshBrowser {
                state.mode = Mode::Browser;
                schedule_browser_refresh(state, fetch_tx).await?;
            } else {
                state.mode = match mode_kind {
                    ModeKind::MessageSearch => Mode::Detail,
                    _ => Mode::Browser,
                };
            }
        }
        KeyCode::Backspace => {
            draft.pop();
            state.mode = mode_from_kind(mode_kind, draft);
        }
        KeyCode::Char(ch) => {
            draft.push(ch);
            state.mode = mode_from_kind(mode_kind, draft);
        }
        _ => state.mode = mode_from_kind(mode_kind, draft),
    }
    Ok(())
}

fn mode_from_kind(kind: ModeKind, draft: String) -> Mode {
    match kind {
        ModeKind::Search => Mode::SearchInput { draft },
        ModeKind::MessageSearch => Mode::MessageSearchInput { draft },
    }
}

fn handle_annotation_input(
    key: KeyEvent,
    target: &Target,
    state: &mut TuiState,
    thread_id: String,
    mut draft: String,
    return_to_detail: bool,
) -> Result<()> {
    let return_mode = if return_to_detail {
        Mode::Detail
    } else {
        Mode::Browser
    };
    match key.code {
        KeyCode::Esc => state.mode = return_mode.clone(),
        KeyCode::Enter => {
            save_annotation_draft(target, state, &thread_id, draft)?;
            state.mode = return_mode.clone();
        }
        KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            clear_annotation(target, &thread_id)?;
            set_annotation_in_state(state, &target.server, &thread_id, None);
            state.mode = return_mode.clone();
        }
        KeyCode::Backspace => {
            draft.pop();
            state.mode = Mode::AnnotationInput {
                server: target.server.clone(),
                thread_id,
                draft,
                return_to_detail,
            };
        }
        KeyCode::Char(ch)
            if !key.modifiers.contains(KeyModifiers::CONTROL)
                && !key.modifiers.contains(KeyModifiers::ALT) =>
        {
            draft.push(ch);
            state.mode = Mode::AnnotationInput {
                server: target.server.clone(),
                thread_id,
                draft,
                return_to_detail,
            };
        }
        _ => {
            state.mode = Mode::AnnotationInput {
                server: target.server.clone(),
                thread_id,
                draft,
                return_to_detail,
            };
        }
    }
    Ok(())
}

fn handle_rename_input(
    key: KeyEvent,
    target: &Target,
    state: &mut TuiState,
    thread_id: String,
    mut draft: String,
    return_to_detail: bool,
    app_tx: &mpsc::UnboundedSender<AppEvent>,
) -> Result<()> {
    let return_mode = if return_to_detail {
        Mode::Detail
    } else {
        Mode::Browser
    };
    match key.code {
        KeyCode::Esc => state.mode = return_mode,
        KeyCode::Enter => {
            let name = draft.trim().to_string();
            if name.is_empty() {
                state.set_notice("name cannot be empty");
                state.mode = Mode::RenameInput {
                    server: target.server.clone(),
                    thread_id,
                    draft,
                    return_to_detail,
                };
            } else {
                state.set_notice(format!("renaming {thread_id}..."));
                spawn_rename_task(target.clone(), thread_id, name, app_tx.clone());
                state.mode = return_mode;
            }
        }
        KeyCode::Backspace => {
            draft.pop();
            state.mode = Mode::RenameInput {
                server: target.server.clone(),
                thread_id,
                draft,
                return_to_detail,
            };
        }
        KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            draft.clear();
            state.mode = Mode::RenameInput {
                server: target.server.clone(),
                thread_id,
                draft,
                return_to_detail,
            };
        }
        KeyCode::Char(ch)
            if !key.modifiers.contains(KeyModifiers::CONTROL)
                && !key.modifiers.contains(KeyModifiers::ALT) =>
        {
            draft.push(ch);
            state.mode = Mode::RenameInput {
                server: target.server.clone(),
                thread_id,
                draft,
                return_to_detail,
            };
        }
        _ => {
            state.mode = Mode::RenameInput {
                server: target.server.clone(),
                thread_id,
                draft,
                return_to_detail,
            };
        }
    }
    Ok(())
}

fn save_annotation_draft(
    target: &Target,
    state: &mut TuiState,
    thread_id: &str,
    value: String,
) -> Result<()> {
    if value.trim().is_empty() {
        clear_annotation(target, thread_id)?;
        set_annotation_in_state(state, &target.server, thread_id, None);
    } else {
        set_annotation(target, thread_id, &value)?;
        set_annotation_in_state(state, &target.server, thread_id, Some(value));
    }
    Ok(())
}

fn set_annotation_in_state(
    state: &mut TuiState,
    server: &str,
    thread_id: &str,
    value: Option<String>,
) {
    if let Some(row) = state
        .browser
        .rows
        .iter_mut()
        .find(|row| row.server == server && row.id == thread_id)
    {
        row.annotation = value.clone();
    }
    if let Some(detail) = &mut state.detail
        && detail.server == server
        && detail.thread_id == thread_id
    {
        detail.annotation = value;
    }
}

async fn handle_compose_input(
    key: KeyEvent,
    state: &mut TuiState,
    mut compose: ComposeState,
    target: &Target,
    yolo: bool,
    app_tx: &mpsc::UnboundedSender<AppEvent>,
) -> Result<()> {
    let return_mode = if compose.return_to_detail {
        Mode::Detail
    } else {
        Mode::Browser
    };
    match key.code {
        KeyCode::Esc => state.mode = return_mode.clone(),
        KeyCode::Tab => {
            compose = toggle_compose_target_or_mode(state, compose);
            state.mode = Mode::Compose(compose);
        }
        KeyCode::Backspace => {
            compose.text.pop();
            state.mode = Mode::Compose(compose);
        }
        KeyCode::Char('j') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            compose.text.push('\n');
            state.mode = Mode::Compose(compose);
        }
        KeyCode::Enter => {
            submit_compose(state, compose, target, yolo, app_tx, return_mode);
        }
        KeyCode::Char(ch) => {
            compose.text.push(ch);
            state.mode = Mode::Compose(compose);
        }
        _ => state.mode = Mode::Compose(compose),
    }
    Ok(())
}

fn submit_compose(
    state: &mut TuiState,
    compose: ComposeState,
    target: &Target,
    yolo: bool,
    app_tx: &mpsc::UnboundedSender<AppEvent>,
    return_mode: Mode,
) {
    let prompt = compose.text.trim().to_string();
    if prompt.is_empty() {
        state.mode = return_mode;
        return;
    }
    match compose.target {
        ComposeTarget::NewTurn { server, thread_id } => {
            let sent_at = format_current_epoch();
            append_detail_message(
                state,
                thread_id.as_str(),
                None,
                "user",
                Some(sent_at.clone()),
                &prompt,
            );
            append_preview_message(
                state,
                &server,
                thread_id.as_str(),
                None,
                "user",
                Some(sent_at),
                &prompt,
            );
            touch_browser_thread_updated(state, &server, &thread_id);
            state.mode = return_mode;
            match compose.send_mode {
                SendMode::Stream => {
                    if active_stream_can_queue_prompt(state, &server, &thread_id) {
                        if let Some(control) = &state.stream_control {
                            let _ = control.send(TurnControl::Submit { prompt, yolo });
                        } else {
                            spawn_queue_turn_task(
                                target.clone(),
                                thread_id,
                                prompt,
                                yolo,
                                app_tx.clone(),
                            );
                        }
                    } else {
                        detach_stream(state);
                        let (control_tx, control_rx) = mpsc::unbounded_channel();
                        let stream_id = state.allocate_stream_id();
                        state.stream = Some(StreamState::new_for_server_with_id(
                            stream_id,
                            server.clone(),
                            thread_id.clone(),
                            None,
                            StreamStatus::Starting,
                            false,
                        ));
                        state.stream_control = Some(control_tx);
                        spawn_stream_send_task(
                            target.clone(),
                            thread_id,
                            prompt,
                            yolo,
                            control_rx,
                            stream_id,
                            app_tx.clone(),
                        );
                    }
                }
                SendMode::NoWait => {
                    spawn_no_wait_send_task(
                        target.clone(),
                        thread_id,
                        prompt,
                        yolo,
                        app_tx.clone(),
                    );
                }
            }
        }
        ComposeTarget::NewThread { server, cwd, title } => {
            debug_assert_eq!(server, target.server);
            detach_stream(state);
            // Drop the detached stream now: its task's terminal event must
            // not match state and clear the new session's stream control.
            state.stream = None;
            let (control_tx, control_rx) = mpsc::unbounded_channel();
            let stream_id = state.allocate_stream_id();
            state.stream_control = Some(control_tx);
            state.mode = return_mode;
            state.set_notice("creating session...");
            spawn_create_session_task(
                target.clone(),
                cwd,
                title,
                prompt,
                yolo,
                control_rx,
                stream_id,
                app_tx.clone(),
            );
        }
        ComposeTarget::Steer {
            server,
            thread_id,
            turn_id,
        } => {
            let sent_at = format_current_epoch();
            append_detail_message(
                state,
                thread_id.as_str(),
                Some(turn_id.clone()),
                "user",
                Some(sent_at.clone()),
                &prompt,
            );
            append_preview_message(
                state,
                &server,
                thread_id.as_str(),
                Some(turn_id.clone()),
                "user",
                Some(sent_at),
                &prompt,
            );
            touch_browser_thread_updated(state, &server, &thread_id);
            state.mode = return_mode;
            spawn_steer_task(
                target.clone(),
                thread_id,
                Some(turn_id),
                prompt,
                yolo,
                app_tx.clone(),
            );
        }
        ComposeTarget::SteerSelected { server, thread_id } => {
            let sent_at = format_current_epoch();
            append_detail_message(
                state,
                thread_id.as_str(),
                None,
                "user",
                Some(sent_at.clone()),
                &prompt,
            );
            append_preview_message(
                state,
                &server,
                thread_id.as_str(),
                None,
                "user",
                Some(sent_at),
                &prompt,
            );
            touch_browser_thread_updated(state, &server, &thread_id);
            state.mode = return_mode;
            spawn_steer_task(
                target.clone(),
                thread_id,
                None,
                prompt,
                yolo,
                app_tx.clone(),
            );
        }
    }
}

fn compose_target_server(target: &ComposeTarget) -> &str {
    match target {
        ComposeTarget::NewTurn { server, .. }
        | ComposeTarget::NewThread { server, .. }
        | ComposeTarget::Steer { server, .. }
        | ComposeTarget::SteerSelected { server, .. } => server,
    }
}

fn active_turn_hint_for_compose(state: &TuiState, server: &str, thread_id: &str) -> Option<String> {
    state.stream.as_ref().and_then(|stream| {
        if stream.server == server
            && stream.thread_id == thread_id
            && matches!(
                stream.status,
                StreamStatus::Starting | StreamStatus::Running
            )
        {
            stream.turn_id.clone()
        } else {
            None
        }
    })
}

fn active_stream_can_queue_prompt(state: &TuiState, server: &str, thread_id: &str) -> bool {
    state.stream.as_ref().is_some_and(|stream| {
        stream.server == server
            && stream.thread_id == thread_id
            && matches!(
                stream.status,
                StreamStatus::Starting | StreamStatus::Running
            )
    })
}

fn send_stream_event(app_tx: &mpsc::UnboundedSender<AppEvent>, stream_id: u64, event: Value) {
    app_tx
        .send(AppEvent::StreamEvent {
            stream_id: Some(stream_id),
            event,
        })
        .ok();
}

fn stream_status_from_wait_outcome(
    outcome: TurnWaitOutcome,
    stream_id: u64,
    app_tx: &mpsc::UnboundedSender<AppEvent>,
) -> StreamStatus {
    match outcome {
        TurnWaitOutcome::Terminal(terminal) => {
            send_stream_event(app_tx, stream_id, terminal.output);
            match terminal.exit_code {
                0 => StreamStatus::Completed,
                _ => StreamStatus::Failed,
            }
        }
        TurnWaitOutcome::LocalInterrupt { .. } => StreamStatus::Detached,
    }
}

async fn attach_existing_turn_stream(
    target: &Target,
    client: &mut RpcClient,
    options: AttachTurnOptions,
    control_rx: mpsc::UnboundedReceiver<TurnControl>,
    stream_id: u64,
    app_tx: &mpsc::UnboundedSender<AppEvent>,
) -> Result<StreamStatus> {
    let tx = app_tx.clone();
    let outcome = attach_turn(
        target,
        client,
        options,
        control_rx,
        |event| {
            send_stream_event(&tx, stream_id, event.clone());
            Ok(())
        },
        |_| Ok(()),
    )
    .await?;
    Ok(stream_status_from_wait_outcome(outcome, stream_id, &tx))
}

async fn wait_started_turn_stream(
    target: &Target,
    client: &mut RpcClient,
    started: StartedTurn,
    control_rx: mpsc::UnboundedReceiver<TurnControl>,
    stream_id: u64,
    app_tx: &mpsc::UnboundedSender<AppEvent>,
) -> Result<StreamStatus> {
    let tx = app_tx.clone();
    let outcome = wait_for_turn_controlled(
        target,
        client,
        started,
        ControlledTurnWaitOptions {
            poll_limit: TURN_SCAN_LIMIT,
            timeout: Duration::from_secs(TURN_WAIT_TIMEOUT_SECS),
            unsubscribe_on_detach: false,
        },
        control_rx,
        |event| {
            send_stream_event(&tx, stream_id, event.clone());
            Ok(())
        },
        |_| Ok(()),
    )
    .await?;
    Ok(stream_status_from_wait_outcome(outcome, stream_id, &tx))
}

fn report_stream_task_result(
    app_tx: &mpsc::UnboundedSender<AppEvent>,
    stream_id: u64,
    server: String,
    thread_id: String,
    turn_id: Option<String>,
    result: Result<StreamStatus>,
) {
    match result {
        Ok(status) => app_tx
            .send(AppEvent::StreamFinished {
                stream_id,
                server: server.clone(),
                thread_id,
                turn_id,
                status,
            })
            .ok(),
        Err(err) => app_tx
            .send(AppEvent::StreamFailed {
                stream_id: Some(stream_id),
                server,
                thread_id,
                turn_id,
                error: err.to_string(),
            })
            .ok(),
    };
}

fn spawn_attach_task(
    target: Target,
    thread_id: String,
    turn_id: String,
    yolo: bool,
    control_rx: mpsc::UnboundedReceiver<TurnControl>,
    stream_id: u64,
    app_tx: mpsc::UnboundedSender<AppEvent>,
) {
    tokio::spawn(async move {
        let server = target.server.clone();
        let stream_thread_id = thread_id.clone();
        let stream_turn_id = Some(turn_id.clone());
        let result: Result<StreamStatus> = async {
            let mut client = RpcClient::connect(&target.endpoint).await?;
            attach_existing_turn_stream(
                &target,
                &mut client,
                AttachTurnOptions {
                    thread_id,
                    turn_id,
                    yolo,
                    poll_limit: TURN_SCAN_LIMIT,
                    timeout: Duration::from_secs(TURN_WAIT_TIMEOUT_SECS),
                },
                control_rx,
                stream_id,
                &app_tx,
            )
            .await
        }
        .await;
        report_stream_task_result(
            &app_tx,
            stream_id,
            server,
            stream_thread_id,
            stream_turn_id,
            result,
        );
    });
}

fn spawn_browser_attach_task(
    target: Target,
    thread_id: String,
    yolo: bool,
    control_rx: mpsc::UnboundedReceiver<TurnControl>,
    stream_id: u64,
    app_tx: mpsc::UnboundedSender<AppEvent>,
) {
    tokio::spawn(async move {
        let server = target.server.clone();
        let stream_thread_id = thread_id.clone();
        let mut stream_turn_id = None;
        let result: Result<StreamStatus> = async {
            let mut client = RpcClient::connect(&target.endpoint).await?;
            let turn_id = active_turn_id_for_thread(&target, &mut client, &thread_id, true)
                .await?
                .ok_or_else(|| usage_error(format!("thread `{thread_id}` has no active turn")))?;
            stream_turn_id = Some(turn_id.clone());
            attach_existing_turn_stream(
                &target,
                &mut client,
                AttachTurnOptions {
                    thread_id,
                    turn_id,
                    yolo,
                    poll_limit: TURN_SCAN_LIMIT,
                    timeout: Duration::from_secs(TURN_WAIT_TIMEOUT_SECS),
                },
                control_rx,
                stream_id,
                &app_tx,
            )
            .await
        }
        .await;
        report_stream_task_result(
            &app_tx,
            stream_id,
            server,
            stream_thread_id,
            stream_turn_id,
            result,
        );
    });
}

fn spawn_thread_follow_task(
    target: Target,
    thread_id: String,
    yolo: bool,
    mut control_rx: mpsc::UnboundedReceiver<TurnControl>,
    stream_id: u64,
    app_tx: mpsc::UnboundedSender<AppEvent>,
) {
    tokio::spawn(async move {
        let server = target.server.clone();
        let stream_thread_id = thread_id.clone();
        let mut stream_turn_id = None;
        let result: Result<Option<StreamStatus>> = async {
            let mut client = RpcClient::connect(&target.endpoint).await?;
            let Some(turn_id) = wait_for_next_active_turn(
                &target,
                &mut client,
                &thread_id,
                &mut control_rx,
                stream_id,
                &app_tx,
            )
            .await?
            else {
                return Ok(None);
            };
            stream_turn_id = Some(turn_id.clone());
            attach_existing_turn_stream(
                &target,
                &mut client,
                AttachTurnOptions {
                    thread_id,
                    turn_id,
                    yolo,
                    poll_limit: TURN_SCAN_LIMIT,
                    timeout: Duration::from_secs(TURN_WAIT_TIMEOUT_SECS),
                },
                control_rx,
                stream_id,
                &app_tx,
            )
            .await
            .map(Some)
        }
        .await;
        match result {
            Ok(Some(status)) => report_stream_task_result(
                &app_tx,
                stream_id,
                server.clone(),
                stream_thread_id,
                stream_turn_id,
                Ok(status),
            ),
            Ok(None) => {
                let _ = app_tx.send(AppEvent::StreamIdle {
                    stream_id,
                    server: server.clone(),
                    thread_id: stream_thread_id,
                });
            }
            Err(err) => {
                let _ = app_tx.send(AppEvent::StreamFailed {
                    stream_id: Some(stream_id),
                    server,
                    thread_id: stream_thread_id,
                    turn_id: stream_turn_id,
                    error: err.to_string(),
                });
            }
        }
    });
}

async fn wait_for_next_active_turn(
    target: &Target,
    client: &mut RpcClient,
    thread_id: &str,
    control_rx: &mut mpsc::UnboundedReceiver<TurnControl>,
    stream_id: u64,
    app_tx: &mpsc::UnboundedSender<AppEvent>,
) -> Result<Option<String>> {
    for attempt in 0..=FOLLOW_NEXT_TURN_POLL_ATTEMPTS {
        if let Some(turn_id) = active_turn_id_for_thread(target, client, thread_id, true).await? {
            return Ok(Some(turn_id));
        }
        if attempt == FOLLOW_NEXT_TURN_POLL_ATTEMPTS {
            break;
        }
        let sleep = tokio::time::sleep(Duration::from_millis(FOLLOW_NEXT_TURN_POLL_INTERVAL_MS));
        tokio::pin!(sleep);
        tokio::select! {
            control = control_rx.recv() => {
                match control {
                    Some(TurnControl::PollNow) => {}
                    Some(TurnControl::Submit { prompt, yolo }) => {
                        let queued = start_turn_request(
                            target,
                            client,
                            thread_id.to_string(),
                            prompt.clone(),
                            TurnStartOptions {
                                model: None,
                                effort: None,
                                service_tier: None,
                                yolo,
                            },
                        )
                        .await?;
                        let _ = app_tx.send(AppEvent::StreamEvent {
                            stream_id: Some(stream_id),
                            event: json!({
                                "type": "queued",
                                "server": target.server,
                                "threadId": thread_id,
                                "turnId": queued.turn_id,
                                "status": "accepted",
                                "prompt": prompt
                            }),
                        });
                    }
                    Some(TurnControl::Detach) | None => return Ok(None),
                }
            }
            _ = &mut sleep => {}
        }
    }
    Ok(None)
}

fn spawn_steer_task(
    target: Target,
    thread_id: String,
    turn_id: Option<String>,
    prompt: String,
    yolo: bool,
    app_tx: mpsc::UnboundedSender<AppEvent>,
) {
    tokio::spawn(async move {
        let server = target.server.clone();
        let stream_thread_id = thread_id.clone();
        let mut stream_turn_id = turn_id.clone();
        let result: Result<Value> = async {
            let mut client = RpcClient::connect(&target.endpoint).await?;
            let turn_id = match turn_id {
                Some(turn_id) => turn_id,
                None => active_turn_id_for_thread(&target, &mut client, &thread_id, true)
                    .await?
                    .ok_or_else(|| {
                        usage_error(format!("thread `{thread_id}` has no active turn"))
                    })?,
            };
            stream_turn_id = Some(turn_id.clone());
            steer_turn(&target, &mut client, thread_id, turn_id, prompt, yolo).await
        }
        .await;
        match result {
            Ok(event) => app_tx
                .send(AppEvent::StreamEvent {
                    stream_id: None,
                    event,
                })
                .ok(),
            Err(err) => app_tx
                .send(AppEvent::StreamFailed {
                    stream_id: None,
                    server,
                    thread_id: stream_thread_id,
                    turn_id: stream_turn_id,
                    error: err.to_string(),
                })
                .ok(),
        };
    });
}

fn spawn_interrupt_task(
    target: Target,
    thread_id: String,
    turn_id: Option<String>,
    app_tx: mpsc::UnboundedSender<AppEvent>,
) {
    tokio::spawn(async move {
        let server = target.server.clone();
        let stream_thread_id = thread_id.clone();
        let mut stream_turn_id = turn_id.clone();
        let result: Result<Value> = async {
            let mut client = RpcClient::connect(&target.endpoint).await?;
            let turn_id = match turn_id {
                Some(turn_id) => turn_id,
                None => active_turn_id_for_thread(&target, &mut client, &thread_id, true)
                    .await?
                    .ok_or_else(|| {
                        usage_error(format!("thread `{thread_id}` has no active turn"))
                    })?,
            };
            stream_turn_id = Some(turn_id.clone());
            interrupt_turn(&target, &mut client, thread_id, turn_id).await
        }
        .await;
        match result {
            Ok(event) => app_tx
                .send(AppEvent::StreamEvent {
                    stream_id: None,
                    event,
                })
                .ok(),
            Err(err) => app_tx
                .send(AppEvent::StreamFailed {
                    stream_id: None,
                    server,
                    thread_id: stream_thread_id,
                    turn_id: stream_turn_id,
                    error: err.to_string(),
                })
                .ok(),
        };
    });
}

fn spawn_archive_task(
    target: Target,
    thread_id: String,
    archived: bool,
    app_tx: mpsc::UnboundedSender<AppEvent>,
) {
    tokio::spawn(async move {
        let server = target.server.clone();
        let result: Result<Value> = async {
            let mut client = RpcClient::connect(&target.endpoint).await?;
            set_thread_archived(&target, &mut client, thread_id.clone(), archived).await
        }
        .await;
        match result {
            Ok(output) => app_tx
                .send(AppEvent::ArchiveChanged {
                    server: server.clone(),
                    thread_id,
                    archived,
                    thread: output["thread"].clone(),
                })
                .ok(),
            Err(err) => app_tx
                .send(AppEvent::ArchiveChangeFailed {
                    server,
                    thread_id,
                    archived,
                    error: err.to_string(),
                })
                .ok(),
        };
    });
}

fn spawn_rename_task(
    target: Target,
    thread_id: String,
    name: String,
    app_tx: mpsc::UnboundedSender<AppEvent>,
) {
    tokio::spawn(async move {
        let server = target.server.clone();
        let result: Result<Value> = async {
            let mut client = RpcClient::connect(&target.endpoint).await?;
            set_thread_name(&target, &mut client, thread_id.clone(), name.clone()).await
        }
        .await;
        match result {
            Ok(output) => app_tx
                .send(AppEvent::RenameChanged {
                    server: server.clone(),
                    thread_id,
                    name,
                    thread: output["thread"].clone(),
                })
                .ok(),
            Err(err) => app_tx
                .send(AppEvent::RenameChangeFailed {
                    server,
                    thread_id,
                    name,
                    error: err.to_string(),
                })
                .ok(),
        };
    });
}

fn spawn_no_wait_send_task(
    target: Target,
    thread_id: String,
    prompt: String,
    yolo: bool,
    app_tx: mpsc::UnboundedSender<AppEvent>,
) {
    tokio::spawn(async move {
        let server = target.server.clone();
        let result: Result<StartedTurn> = async {
            let mut client = RpcClient::connect(&target.endpoint).await?;
            start_turn_request(
                &target,
                &mut client,
                thread_id.clone(),
                prompt,
                TurnStartOptions {
                    model: None,
                    effort: None,
                    service_tier: None,
                    yolo,
                },
            )
            .await
        }
        .await;
        match result {
            Ok(started) => {
                let prompt = started.prompt().unwrap_or_default().to_string();
                app_tx
                    .send(AppEvent::TurnQueued {
                        server: server.clone(),
                        thread_id,
                        turn_id: started.turn_id,
                        prompt,
                    })
                    .ok()
            }
            Err(err) => app_tx
                .send(AppEvent::TurnSubmitFailed {
                    server,
                    thread_id,
                    error: err.to_string(),
                })
                .ok(),
        };
    });
}

fn spawn_queue_turn_task(
    target: Target,
    thread_id: String,
    prompt: String,
    yolo: bool,
    app_tx: mpsc::UnboundedSender<AppEvent>,
) {
    tokio::spawn(async move {
        let server = target.server.clone();
        let result: Result<StartedTurn> = async {
            let mut client = RpcClient::connect(&target.endpoint).await?;
            start_turn_request(
                &target,
                &mut client,
                thread_id.clone(),
                prompt,
                TurnStartOptions {
                    model: None,
                    effort: None,
                    service_tier: None,
                    yolo,
                },
            )
            .await
        }
        .await;
        match result {
            Ok(started) => {
                let prompt = started.prompt().unwrap_or_default().to_string();
                app_tx
                    .send(AppEvent::TurnQueued {
                        server: server.clone(),
                        thread_id,
                        turn_id: started.turn_id,
                        prompt,
                    })
                    .ok()
            }
            Err(err) => app_tx
                .send(AppEvent::TurnSubmitFailed {
                    server,
                    thread_id,
                    error: err.to_string(),
                })
                .ok(),
        };
    });
}

async fn active_turn_id_for_thread(
    target: &Target,
    client: &mut RpcClient,
    thread_id: &str,
    load: bool,
) -> Result<Option<String>> {
    let status = thread_status(
        target,
        client,
        ThreadStatusRequest {
            thread_id: thread_id.to_string(),
            load,
            turn_scan_limit: TURN_SCAN_LIMIT,
        },
    )
    .await?;
    Ok(status["activeTurnId"].as_str().map(str::to_string))
}

fn spawn_stream_send_task(
    target: Target,
    thread_id: String,
    prompt: String,
    yolo: bool,
    control_rx: mpsc::UnboundedReceiver<TurnControl>,
    stream_id: u64,
    app_tx: mpsc::UnboundedSender<AppEvent>,
) {
    tokio::spawn(async move {
        let server = target.server.clone();
        let stream_thread_id = thread_id.clone();
        let mut stream_turn_id = None;
        let result: Result<StreamStatus> = async {
            let mut client = RpcClient::connect(&target.endpoint).await?;
            let prompt_for_event = prompt.clone();
            let started = start_turn_request(
                &target,
                &mut client,
                thread_id.clone(),
                prompt,
                TurnStartOptions {
                    model: None,
                    effort: None,
                    service_tier: None,
                    yolo,
                },
            )
            .await?;
            stream_turn_id = Some(started.turn_id.clone());
            let mut acceptance = started.acceptance.clone();
            acceptance["prompt"] = json!(prompt_for_event);
            send_stream_event(&app_tx, stream_id, acceptance);
            wait_started_turn_stream(
                &target,
                &mut client,
                started,
                control_rx,
                stream_id,
                &app_tx,
            )
            .await
        }
        .await;
        report_stream_task_result(
            &app_tx,
            stream_id,
            server,
            stream_thread_id,
            stream_turn_id,
            result,
        );
    });
}

#[allow(clippy::too_many_arguments)]
fn spawn_create_session_task(
    target: Target,
    cwd: String,
    title: Option<String>,
    prompt: String,
    yolo: bool,
    control_rx: mpsc::UnboundedReceiver<TurnControl>,
    stream_id: u64,
    app_tx: mpsc::UnboundedSender<AppEvent>,
) {
    tokio::spawn(async move {
        let server = target.server.clone();
        let mut created_thread_id: Option<String> = None;
        let mut stream_turn_id = None;
        let result: Result<StreamStatus> = async {
            let mut client = RpcClient::connect(&target.endpoint).await?;
            let start = start_thread(
                &mut client,
                std::path::Path::new(&cwd),
                ThreadStartOptions {
                    model: target.model.clone(),
                    effort: target.model_reasoning_effort.clone(),
                    service_tier: None,
                    yolo,
                },
            )
            .await?;
            let thread_id = thread_id_from_start(&start)?;
            created_thread_id = Some(thread_id.clone());
            if let Some(name) = &title {
                set_thread_name(&target, &mut client, thread_id.clone(), name.clone()).await?;
            }
            let _ = app_tx.send(AppEvent::SessionCreated {
                stream_id,
                server: server.clone(),
                thread_id: thread_id.clone(),
                cwd: cwd.clone(),
                title: title.clone(),
                prompt: prompt.clone(),
            });
            let prompt_for_event = prompt.clone();
            let started = start_turn_request(
                &target,
                &mut client,
                thread_id,
                prompt,
                TurnStartOptions {
                    model: None,
                    effort: None,
                    service_tier: None,
                    yolo,
                },
            )
            .await?;
            stream_turn_id = Some(started.turn_id.clone());
            let mut acceptance = started.acceptance.clone();
            acceptance["prompt"] = json!(prompt_for_event);
            send_stream_event(&app_tx, stream_id, acceptance);
            wait_started_turn_stream(
                &target,
                &mut client,
                started,
                control_rx,
                stream_id,
                &app_tx,
            )
            .await
        }
        .await;
        match created_thread_id {
            Some(thread_id) => {
                report_stream_task_result(
                    &app_tx,
                    stream_id,
                    server,
                    thread_id,
                    stream_turn_id,
                    result,
                );
            }
            None => {
                if let Err(error) = result {
                    let _ = app_tx.send(AppEvent::SessionCreateFailed {
                        server,
                        error: format!("{error:#}"),
                    });
                }
            }
        }
    });
}

fn handle_app_event(event: AppEvent, state: &mut TuiState) {
    match event {
        AppEvent::BrowserLoaded {
            epoch,
            rows,
            next_cursor,
            backwards_cursor,
            warning,
        } => {
            let current_cursor = state.browser.current_cursor.clone();
            state.set_browser_rows(epoch, rows, next_cursor, backwards_cursor, current_cursor);
            if state.browser.epoch == epoch {
                state.browser.last_error = warning;
            }
        }
        AppEvent::BrowserLoadFailed { epoch, error } => state.set_browser_error(epoch, error),
        AppEvent::DetailLoaded {
            epoch,
            detail,
            page_direction,
        } => match page_direction {
            DetailPageDirection::Replace => state.replace_detail(epoch, *detail),
            DetailPageDirection::Older => state.extend_detail_older(epoch, *detail),
            DetailPageDirection::Newer => state.extend_detail_newer(epoch, *detail),
        },
        AppEvent::DetailLoadFailed { epoch, error } => state.set_detail_error(epoch, error),
        AppEvent::PreviewLoaded {
            epoch,
            server,
            thread_id,
            messages,
        } => state.set_preview_loaded(epoch, server, thread_id, messages),
        AppEvent::PreviewLoadFailed {
            epoch,
            server,
            thread_id,
            error,
        } => state.set_preview_error(epoch, server, thread_id, error),
        AppEvent::ThreadLoaded {
            server,
            thread_id,
            status,
        } => {
            apply_thread_load_status(state, &server, &thread_id, &status);
            reset_preview_for_thread(state, &server, &thread_id);
            state.set_notice(format!("loaded {thread_id}"));
        }
        AppEvent::ThreadLoadFailed {
            server,
            thread_id,
            error,
        } => {
            state.set_notice(format!("failed to load {server}/{thread_id}: {error}"));
        }
        AppEvent::SessionCreated {
            stream_id,
            server,
            thread_id,
            cwd,
            title,
            prompt,
        } => {
            let sent_at = format_current_epoch();
            let row_title = title.unwrap_or_else(|| {
                prompt
                    .lines()
                    .next()
                    .unwrap_or(thread_id.as_str())
                    .to_string()
            });
            state.browser.rows.insert(
                0,
                ThreadRow {
                    server: server.clone(),
                    id: thread_id.clone(),
                    title: row_title,
                    status: "active".to_string(),
                    updated: sent_at.clone(),
                    cwd,
                    annotation: None,
                    snippet: None,
                    raw: json!({}),
                },
            );
            state.browser.selected = 0;
            state.browser.row_offset = 0;
            state.stream = Some(StreamState::new_for_server_with_id(
                stream_id,
                server.clone(),
                thread_id.clone(),
                None,
                StreamStatus::Starting,
                false,
            ));
            state.browser.preview.epoch += 1;
            state.browser.preview.server = Some(server);
            state.browser.preview.thread_id = Some(thread_id);
            state.browser.preview.loading = false;
            state.browser.preview.error = None;
            state.browser.preview.messages = vec![message_block(
                None,
                None,
                "user",
                Some(sent_at),
                &prompt,
                100,
            )];
            state.set_notice("session created");
        }
        AppEvent::SessionCreateFailed { server, error } => {
            state.stream_control = None;
            state.set_notice(format!("create session on {server} failed: {error}"));
        }
        AppEvent::StreamEvent { stream_id, event } => {
            log_stream_event(&event);
            let event_thread_id = event["threadId"].as_str();
            let Some(event_server) = resolve_event_server(state, &event, event_thread_id) else {
                return;
            };
            let event_server = event_server.as_str();
            let event_type = event["type"].as_str();
            if let Some(current_stream) = &state.stream {
                if let Some(stream_id) = stream_id {
                    if current_stream.id != stream_id {
                        return;
                    }
                } else if event_thread_id.is_some_and(|thread_id| {
                    event_server != current_stream.server
                        || thread_id != current_stream.thread_id.as_str()
                }) {
                    return;
                }
            }
            if let Some(thread_id) = event_thread_id {
                touch_browser_thread_updated(state, event_server, thread_id);
                match event["status"].as_str() {
                    Some("completed" | "failed" | "interrupted") => {
                        set_browser_thread_status(state, event_server, thread_id, "idle");
                    }
                    _ => set_browser_thread_status(state, event_server, thread_id, "active"),
                }
            }
            if state.stream.is_none()
                && let Some(thread_id) = event_thread_id
            {
                if !stream_event_matches_visible_thread(state, event_server, thread_id) {
                    return;
                }
                state.stream = Some(StreamState::new_for_server_with_id(
                    stream_id.unwrap_or(0),
                    event_server.to_string(),
                    thread_id.to_string(),
                    event["turnId"].as_str().map(str::to_string),
                    StreamStatus::Running,
                    event["type"].as_str() == Some("attached"),
                ));
            }
            let mut pending_turn_id = None;
            let pending_prompt = event["prompt"].as_str().map(str::to_string);
            let mut assistant_updates = Vec::new();
            let initial_event_turn_id =
                event["turnId"].as_str().map(str::to_string).or_else(|| {
                    state
                        .stream
                        .as_ref()
                        .and_then(|stream| stream.turn_id.clone())
                });
            let snapshot_detail =
                detail_from_resume_snapshot(state, &event, &initial_event_turn_id);
            if let Some(stream) = &mut state.stream {
                if let Some(turn_id) = event["turnId"].as_str() {
                    if event_type != Some("queued") {
                        stream.turn_id = Some(turn_id.to_string());
                    }
                    if matches!(event_type, Some("accepted" | "queued")) {
                        pending_turn_id = Some(turn_id.to_string());
                    }
                }
                let event_turn_id = event["turnId"]
                    .as_str()
                    .map(str::to_string)
                    .or_else(|| stream.turn_id.clone());
                seed_stream_from_resume_snapshot(stream, event_turn_id.as_deref(), &event);
                if let Some(delta) = event["delta"].as_str() {
                    let ids = EventItemIds::from_value(&event);
                    let item =
                        append_stream_assistant_delta(stream, event_turn_id.clone(), &ids, delta);
                    assistant_updates.push((
                        item.turn_id.clone(),
                        ids.with_resolved(item.item_id.as_deref()),
                        item.text.clone(),
                        false,
                    ));
                } else if let Some(text) = event["text"].as_str() {
                    let ids = EventItemIds::from_value(&event);
                    let item = set_stream_assistant_text(stream, event_turn_id.clone(), &ids, text);
                    assistant_updates.push((
                        item.turn_id.clone(),
                        ids.with_resolved(item.item_id.as_deref()),
                        item.text.clone(),
                        true,
                    ));
                } else if let Some(responses) = event["assistantResponses"].as_array() {
                    for response in responses {
                        let Some(text) = response["text"].as_str() else {
                            continue;
                        };
                        let ids = EventItemIds::from_value(response);
                        let item =
                            set_stream_assistant_text(stream, event_turn_id.clone(), &ids, text);
                        assistant_updates.push((
                            item.turn_id.clone(),
                            ids.with_resolved(item.item_id.as_deref()),
                            item.text.clone(),
                            true,
                        ));
                    }
                } else if let Some(text) = event["finalAssistantText"].as_str()
                    && !text.is_empty()
                    && stream.assistant_items.is_empty()
                {
                    let ids = EventItemIds::default();
                    let item = set_stream_assistant_text(stream, event_turn_id.clone(), &ids, text);
                    assistant_updates.push((
                        item.turn_id.clone(),
                        ids.with_resolved(item.item_id.as_deref()),
                        item.text.clone(),
                        true,
                    ));
                }
                stream.status = match event["status"].as_str() {
                    Some("completed") => StreamStatus::Completed,
                    Some("failed") => StreamStatus::Failed,
                    Some("interrupted") => StreamStatus::Interrupted,
                    _ => StreamStatus::Running,
                };
                if event["source"].as_str() == Some("poll") {
                    stream.last_poll_at = Some(std::time::Instant::now());
                }
            }
            if let Some(turn_id) = pending_turn_id {
                fill_pending_turn_id(state, event_server, &turn_id, pending_prompt.as_deref());
                fill_preview_pending_turn_id(
                    state,
                    event_server,
                    &turn_id,
                    pending_prompt.as_deref(),
                );
            }
            if let Some(detail) = snapshot_detail {
                state.replace_detail(detail.epoch, detail);
            }
            seed_preview_from_resume_snapshot(state, &event);
            for (turn_id, ids, text, finalized) in assistant_updates {
                upsert_preview_assistant_message(state, turn_id.clone(), &ids, &text, finalized);
                upsert_streaming_assistant_message(state, turn_id, &ids, &text, finalized);
            }
        }
        AppEvent::StreamFailed {
            stream_id,
            server,
            thread_id,
            turn_id,
            error,
        } => {
            if !stream_terminal_event_applies(
                state,
                stream_id,
                &server,
                &thread_id,
                turn_id.as_deref(),
            ) {
                return;
            }
            touch_browser_thread_updated(state, &server, &thread_id);
            if let Some(stream) = &mut state.stream {
                stream.status = StreamStatus::Failed;
                stream.last_error = Some(error);
            } else {
                state.set_notice(format!("stream failed: {error}"));
            }
            state.stream_control = None;
        }
        AppEvent::StreamFinished {
            stream_id,
            server,
            thread_id,
            turn_id,
            status,
        } => {
            if !stream_terminal_event_applies(
                state,
                Some(stream_id),
                &server,
                &thread_id,
                turn_id.as_deref(),
            ) {
                return;
            }
            touch_browser_thread_updated(state, &server, &thread_id);
            if status != StreamStatus::Detached {
                set_browser_thread_status(state, &server, &thread_id, "idle");
            }
            if let Some(stream) = &mut state.stream {
                stream.status = status;
                if status == StreamStatus::Detached {
                    stream.detached = true;
                }
            }
            state.stream_control = None;
        }
        AppEvent::StreamIdle {
            stream_id,
            server,
            thread_id,
        } => {
            if state.stream.as_ref().is_some_and(|stream| {
                stream.id == stream_id && stream.server == server && stream.thread_id == thread_id
            }) {
                set_browser_thread_status(state, &server, &thread_id, "idle");
                if let Some(detail) = &mut state.detail
                    && detail.server == server
                    && detail.thread_id == thread_id
                {
                    detail.status = "idle".to_string();
                    detail.active_turn_id = None;
                }
                if let Some(stream) = &mut state.stream {
                    stream.status = StreamStatus::Completed;
                    stream.turn_id = None;
                }
                state.stream_control = None;
            }
        }
        AppEvent::TurnQueued {
            server,
            thread_id,
            turn_id,
            prompt,
        } => {
            set_browser_thread_status(state, &server, &thread_id, "active");
            touch_browser_thread_updated(state, &server, &thread_id);
            fill_pending_turn_id(state, &server, &turn_id, Some(&prompt));
            fill_preview_pending_turn_id(state, &server, &turn_id, Some(&prompt));
            state.set_notice(format!("sent {thread_id}"));
        }
        AppEvent::TurnSubmitFailed {
            server,
            thread_id,
            error,
        } => {
            state.set_notice(format!("failed to send {server}/{thread_id}: {error}"));
        }
        AppEvent::ArchiveChanged {
            server,
            thread_id,
            archived,
            thread,
        } => {
            apply_archive_change(state, &server, &thread_id, archived, &thread);
            state.set_notice(format!(
                "{} {thread_id}",
                if archived { "archived" } else { "unarchived" }
            ));
        }
        AppEvent::ArchiveChangeFailed {
            server,
            thread_id,
            archived,
            error,
        } => {
            state.set_notice(format!(
                "failed to {} {server}/{thread_id}: {error}",
                if archived { "archive" } else { "unarchive" }
            ));
        }
        AppEvent::RenameChanged {
            server,
            thread_id,
            name,
            thread,
        } => {
            set_thread_name_in_state(state, &server, &thread_id, &name, &thread);
            state.set_notice(format!("renamed {thread_id}"));
        }
        AppEvent::RenameChangeFailed {
            server,
            thread_id,
            name,
            error,
        } => {
            state.set_notice(format!(
                "failed to rename {server}/{thread_id} to {name}: {error}"
            ));
        }
        AppEvent::ShutdownSignal => {
            detach_stream(state);
            state.should_quit = true;
        }
    }
}

fn archive_changed_thread(event: &AppEvent) -> Option<(String, String)> {
    match event {
        AppEvent::ArchiveChanged {
            server, thread_id, ..
        } => Some((server.clone(), thread_id.clone())),
        _ => None,
    }
}

fn rename_changed_thread(event: &AppEvent) -> Option<(String, String)> {
    match event {
        AppEvent::RenameChanged {
            server, thread_id, ..
        } => Some((server.clone(), thread_id.clone())),
        _ => None,
    }
}

fn turn_submitted_thread(event: &AppEvent) -> Option<(String, String)> {
    match event {
        AppEvent::TurnQueued {
            server, thread_id, ..
        } => Some((server.clone(), thread_id.clone())),
        _ => None,
    }
}

fn loaded_thread(event: &AppEvent) -> Option<(String, String)> {
    match event {
        AppEvent::ThreadLoaded {
            server, thread_id, ..
        } => Some((server.clone(), thread_id.clone())),
        _ => None,
    }
}

fn log_stream_event(event: &Value) {
    let Ok(path) = std::env::var("CODEX_THREADS_TUI_STREAM_LOG") else {
        return;
    };
    if path.trim().is_empty() {
        return;
    }
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let line = json!({"timestamp": timestamp, "event": event});
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = writeln!(file, "{line}");
    }
}

fn detail_from_resume_snapshot(
    state: &TuiState,
    event: &Value,
    active_turn_id: &Option<String>,
) -> Option<DetailState> {
    let current = state.detail.as_ref()?;
    let thread = event.get("thread")?;
    let thread_id = thread["id"]
        .as_str()
        .or_else(|| event["threadId"].as_str())?;
    if current.thread_id != thread_id {
        return None;
    }
    let turns = thread["turns"].as_array()?;
    if turns.is_empty() {
        return None;
    }
    let output = json!({
        "thread": thread,
        "turns": {
            "data": turns,
            "nextCursor": Value::Null,
            "backwardsCursor": Value::Null
        }
    });
    let status_output = active_turn_id
        .as_ref()
        .map(|turn_id| json!({"activeTurnId": turn_id}));
    Some(detail_state_for_server(
        current.server.clone(),
        output,
        status_output,
        thread_id.to_string(),
        current.epoch,
        current.current_cursor.clone(),
    ))
}

fn seed_stream_from_resume_snapshot(
    stream: &mut StreamState,
    active_turn_id: Option<&str>,
    event: &Value,
) {
    let Some(turns) = event["thread"]["turns"].as_array() else {
        return;
    };
    for turn in turns {
        let Some(turn_id) = turn["id"].as_str() else {
            continue;
        };
        if active_turn_id.is_some_and(|active| active != turn_id) {
            continue;
        }
        for item in turn["items"].as_array().unwrap_or(&Vec::new()) {
            if item["type"].as_str() != Some("agentMessage") {
                continue;
            }
            let Some(text) = item["text"].as_str() else {
                continue;
            };
            set_stream_assistant_snapshot_text(
                stream,
                Some(turn_id.to_string()),
                item["id"].as_str().map(str::to_string),
                text,
            );
        }
    }
}

fn stream_terminal_event_applies(
    state: &TuiState,
    stream_id: Option<u64>,
    server: &str,
    thread_id: &str,
    turn_id: Option<&str>,
) -> bool {
    let Some(stream) = &state.stream else {
        return stream_id.is_none()
            && stream_event_matches_visible_thread(state, server, thread_id);
    };
    if let Some(stream_id) = stream_id
        && stream.id != stream_id
    {
        return false;
    }
    if stream.server != server || stream.thread_id != thread_id {
        return false;
    }
    if let (Some(expected), Some(actual)) = (stream.turn_id.as_deref(), turn_id) {
        return expected == actual;
    }
    true
}

fn stream_is_running(state: &TuiState) -> bool {
    state.stream.as_ref().is_some_and(stream_is_running_status)
}

fn stream_is_running_status(stream: &StreamState) -> bool {
    matches!(
        stream.status,
        StreamStatus::Starting | StreamStatus::Running
    )
}

fn stream_is_visible_in_browser(state: &TuiState, server: &str, thread_id: &str) -> bool {
    state
        .selected_thread_key()
        .as_ref()
        .is_some_and(|(selected_server, selected_thread)| {
            selected_server == server && selected_thread == thread_id
        })
        || state
            .browser
            .preview
            .thread_id
            .as_deref()
            .is_some_and(|preview_thread_id| {
                state.browser.preview.server.as_deref() == Some(server)
                    && preview_thread_id == thread_id
            })
        || (state.selected_thread_id().is_none()
            && state.browser.preview.thread_id.is_none()
            && state
                .detail
                .as_ref()
                .is_some_and(|detail| detail.server == server && detail.thread_id == thread_id))
}

fn stream_event_matches_visible_thread(state: &TuiState, server: &str, thread_id: &str) -> bool {
    match state.mode {
        Mode::Browser => stream_is_visible_in_browser(state, server, thread_id),
        _ => state
            .detail
            .as_ref()
            .is_some_and(|detail| detail.server == server && detail.thread_id == thread_id),
    }
}

fn detach_stream(state: &mut TuiState) {
    if let Some(control) = &state.stream_control {
        let _ = control.send(TurnControl::Detach);
    }
    if let Some(stream) = &mut state.stream
        && matches!(
            stream.status,
            StreamStatus::Starting | StreamStatus::Running
        )
    {
        stream.status = StreamStatus::Detached;
        stream.detached = true;
    }
    state.stream_control = None;
}

fn detach_stream_if_browser_selection_changed(
    state: &mut TuiState,
    previous_selection: Option<(String, String)>,
) {
    if !matches!(state.mode, Mode::Browser) {
        return;
    }
    let current_selection = state.selected_thread_key();
    if previous_selection == current_selection {
        return;
    }
    let Some((stream_server, stream_thread_id)) = state
        .stream
        .as_ref()
        .map(|stream| (stream.server.clone(), stream.thread_id.clone()))
    else {
        return;
    };
    if current_selection != Some((stream_server, stream_thread_id)) {
        detach_stream(state);
        state.stream = None;
    }
}

fn unlink_detail_session(state: &mut TuiState) {
    let detail_key = state
        .detail
        .as_ref()
        .map(|detail| (detail.server.clone(), detail.thread_id.clone()));
    if let Some((detail_server, detail_thread_id)) = detail_key
        && state.stream.as_ref().is_some_and(|stream| {
            stream.server == detail_server && stream.thread_id == detail_thread_id
        })
    {
        detach_stream(state);
        state.stream = None;
    }
    state.detail = None;
    state.mode = Mode::Browser;
}

fn initial_browser_load_needs_auto_attach(event: &AppEvent, state: &TuiState) -> bool {
    matches!(event, AppEvent::BrowserLoaded { .. })
        && matches!(state.mode, Mode::Browser)
        && state.browser.rows.is_empty()
        && state.stream.is_none()
}

fn auto_attach_selected_browser_thread_if_active(
    state: &mut TuiState,
    targets: TuiTargets,
    yolo: bool,
    app_tx: mpsc::UnboundedSender<AppEvent>,
) {
    if !matches!(state.mode, Mode::Browser) || state.stream.is_some() {
        return;
    }
    let Some(row) = state.browser.rows.get(state.browser.selected) else {
        return;
    };
    if !row.is_running() {
        return;
    }
    let server = row.server.clone();
    let thread_id = row.id.clone();
    let Ok(target) = targets.get(&server).cloned() else {
        return;
    };
    let stream_id = state.allocate_stream_id();
    state.stream = Some(StreamState::new_for_server_with_id(
        stream_id,
        server,
        thread_id.clone(),
        None,
        StreamStatus::Starting,
        true,
    ));
    let (control_tx, control_rx) = mpsc::unbounded_channel();
    state.stream_control = Some(control_tx);
    spawn_browser_attach_task(target, thread_id, yolo, control_rx, stream_id, app_tx);
}

fn auto_attach_open_detail_thread_if_active(
    state: &mut TuiState,
    targets: TuiTargets,
    yolo: bool,
    app_tx: mpsc::UnboundedSender<AppEvent>,
) {
    if !matches!(state.mode, Mode::Detail) {
        return;
    }
    let Some(detail) = state.detail.as_ref() else {
        return;
    };
    if detail.loading {
        return;
    }
    let Some(turn_id) = detail.active_turn_id.clone() else {
        return;
    };
    let server = detail.server.clone();
    let thread_id = detail.thread_id.clone();
    if state.stream.as_ref().is_some_and(|stream| {
        stream.server == server && stream.thread_id == thread_id && stream_is_running_status(stream)
    }) {
        return;
    }
    detach_stream(state);
    state.stream = None;
    let stream_id = state.allocate_stream_id();
    state.stream = Some(StreamState::new_for_server_with_id(
        stream_id,
        server.clone(),
        thread_id.clone(),
        Some(turn_id.clone()),
        StreamStatus::Running,
        true,
    ));
    let (control_tx, control_rx) = mpsc::unbounded_channel();
    state.stream_control = Some(control_tx);
    let Ok(target) = targets.get(&server).cloned() else {
        return;
    };
    spawn_attach_task(
        target, thread_id, turn_id, yolo, control_rx, stream_id, app_tx,
    );
}

fn stream_finish_detail_thread(event: &AppEvent, state: &TuiState) -> Option<(String, String)> {
    if !matches!(event, AppEvent::StreamFinished { .. }) {
        return None;
    }
    let detail = state.detail.as_ref()?;
    let stream = state.stream.as_ref()?;
    if detail.server == stream.server && detail.thread_id == stream.thread_id {
        Some((detail.server.clone(), detail.thread_id.clone()))
    } else {
        None
    }
}

fn stream_finish_follow_thread(event: &AppEvent, state: &TuiState) -> Option<(String, String)> {
    let AppEvent::StreamFinished {
        stream_id,
        server,
        thread_id,
        status,
        ..
    } = event
    else {
        return None;
    };
    if *status != StreamStatus::Completed {
        return None;
    }
    let stream = state.stream.as_ref()?;
    if stream.id != *stream_id || stream.server != *server || stream.thread_id != *thread_id {
        return None;
    }
    if !stream_event_matches_visible_thread(state, server, thread_id) {
        return None;
    }
    Some((server.clone(), thread_id.clone()))
}

fn follow_thread_stream_if_active(
    state: &mut TuiState,
    targets: TuiTargets,
    yolo: bool,
    server: String,
    thread_id: String,
    app_tx: mpsc::UnboundedSender<AppEvent>,
) {
    if !stream_event_matches_visible_thread(state, &server, &thread_id) {
        return;
    }
    let Ok(target) = targets.get(&server).cloned() else {
        return;
    };
    let stream_id = state.allocate_stream_id();
    state.stream = Some(StreamState::new_for_server_with_id(
        stream_id,
        server,
        thread_id.clone(),
        None,
        StreamStatus::Starting,
        true,
    ));
    let (control_tx, control_rx) = mpsc::unbounded_channel();
    state.stream_control = Some(control_tx);
    spawn_thread_follow_task(target, thread_id, yolo, control_rx, stream_id, app_tx);
}

fn detail_follow_refresh_thread(state: &TuiState) -> Option<(String, String)> {
    if !matches!(state.mode, Mode::Detail) || stream_is_running(state) {
        return None;
    }
    let detail = state.detail.as_ref()?;
    if detail.loading {
        return None;
    }
    if detail
        .last_refresh_at
        .is_none_or(|last| last.elapsed() >= Duration::from_secs(DETAIL_FOLLOW_REFRESH_SECS))
    {
        Some((detail.server.clone(), detail.thread_id.clone()))
    } else {
        None
    }
}

fn append_detail_message(
    state: &mut TuiState,
    thread_id: &str,
    turn_id: Option<String>,
    role: &str,
    timestamp: Option<String>,
    text: &str,
) {
    let Some(detail) = &mut state.detail else {
        return;
    };
    if detail.thread_id != thread_id {
        return;
    }
    detail
        .messages
        .push(message_block(turn_id, None, role, timestamp, text, 100));
    detail.scroll = detail.bottom_scroll_position();
    if !detail.search_query.is_empty() {
        let query = detail.search_query.clone();
        state.update_message_search(query);
    }
}

fn preview_matches_thread(state: &TuiState, server: &str, thread_id: &str) -> bool {
    (state.browser.preview.server.as_deref() == Some(server)
        && state.browser.preview.thread_id.as_deref() == Some(thread_id))
        || (matches!(state.mode, Mode::Browser)
            && state.selected_thread_key().as_ref().is_some_and(
                |(selected_server, selected_thread)| {
                    selected_server == server && selected_thread == thread_id
                },
            ))
}

fn ensure_preview_thread(state: &mut TuiState, server: &str, thread_id: &str) {
    if state.browser.preview.server.as_deref() != Some(server)
        || state.browser.preview.thread_id.as_deref() != Some(thread_id)
    {
        state.browser.preview.server = Some(server.to_string());
        state.browser.preview.thread_id = Some(thread_id.to_string());
        state.browser.preview.loading = false;
        state.browser.preview.messages.clear();
        state.browser.preview.error = None;
    }
}

fn append_preview_message(
    state: &mut TuiState,
    server: &str,
    thread_id: &str,
    turn_id: Option<String>,
    role: &str,
    timestamp: Option<String>,
    text: &str,
) {
    if !preview_matches_thread(state, server, thread_id) {
        return;
    }
    ensure_preview_thread(state, server, thread_id);
    state
        .browser
        .preview
        .messages
        .push(message_block(turn_id, None, role, timestamp, text, 100));
}

fn fill_pending_turn_id(state: &mut TuiState, server: &str, turn_id: &str, prompt: Option<&str>) {
    let Some(detail) = &mut state.detail else {
        return;
    };
    if detail.server != server {
        return;
    }
    for message in &mut detail.messages {
        if pending_message_matches(message, prompt) {
            message.turn_id = Some(turn_id.to_string());
            break;
        }
    }
}

fn fill_preview_pending_turn_id(
    state: &mut TuiState,
    server: &str,
    turn_id: &str,
    prompt: Option<&str>,
) {
    if state.browser.preview.server.as_deref() != Some(server) {
        return;
    }
    for message in &mut state.browser.preview.messages {
        if pending_message_matches(message, prompt) {
            message.turn_id = Some(turn_id.to_string());
            break;
        }
    }
}

fn pending_message_matches(message: &MessageBlock, prompt: Option<&str>) -> bool {
    if message.turn_id.is_some() || message.item_id.is_some() || message.role != "user" {
        return false;
    }
    let Some(prompt) = prompt else {
        return true;
    };
    message_text(message) == prompt
}

fn message_text(message: &MessageBlock) -> String {
    message.raw_text.clone()
}

fn seed_preview_from_resume_snapshot(state: &mut TuiState, event: &Value) {
    let Some(thread) = event.get("thread") else {
        return;
    };
    let Some(thread_id) = thread["id"].as_str().or_else(|| event["threadId"].as_str()) else {
        return;
    };
    let Some(server) = resolve_event_server(state, event, Some(thread_id)) else {
        return;
    };
    if !preview_matches_thread(state, &server, thread_id) || thread["turns"].as_array().is_none() {
        return;
    }
    let output = json!({
        "thread": thread,
        "turns": {
            "data": thread["turns"].clone(),
            "nextCursor": Value::Null,
            "backwardsCursor": Value::Null
        }
    });
    let status_output = event["turnId"]
        .as_str()
        .map(|turn_id| json!({"activeTurnId": turn_id}));
    let detail = detail_state_for_server(
        server.clone(),
        output,
        status_output,
        thread_id.to_string(),
        0,
        None,
    );
    ensure_preview_thread(state, &server, thread_id);
    state.browser.preview.messages = detail.messages;
}

/// Item identity attached to a stream event: the canonical id plus every
/// alias the turn wait layer knows for the same item. Codex app-server names
/// one item differently per surface (live `msg_<hash>` vs persisted
/// `item-N`), so matching must accept any known alias.
#[derive(Debug, Clone, Default)]
struct EventItemIds {
    item_id: Option<String>,
    aliases: Vec<String>,
}

impl EventItemIds {
    fn from_value(value: &Value) -> Self {
        let item_id = value["itemId"].as_str().map(str::to_string);
        let mut aliases: Vec<String> = value["itemAliases"]
            .as_array()
            .map(|values| {
                values
                    .iter()
                    .filter_map(|value| value.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        if let Some(item_id) = &item_id
            && !aliases.contains(item_id)
        {
            aliases.insert(0, item_id.clone());
        }
        Self { item_id, aliases }
    }

    fn is_identified(&self) -> bool {
        self.item_id.is_some()
    }

    fn matches_id(&self, candidate: Option<&str>) -> bool {
        candidate.is_some_and(|candidate| self.aliases.iter().any(|alias| alias == candidate))
    }

    fn with_resolved(&self, resolved: Option<&str>) -> Self {
        let mut ids = self.clone();
        if let Some(resolved) = resolved {
            if !ids.aliases.iter().any(|alias| alias == resolved) {
                ids.aliases.push(resolved.to_string());
            }
            ids.item_id = Some(resolved.to_string());
        }
        ids
    }
}

fn append_stream_assistant_delta(
    stream: &mut StreamState,
    turn_id: Option<String>,
    ids: &EventItemIds,
    delta: &str,
) -> StreamAssistantItem {
    // Deltas are exact continuations of the text streamed so far: the turn
    // wait layer trims any replayed content against the attach snapshot
    // before emitting (see reconcile_replayed_deltas in turns.rs).
    let item = stream_assistant_item_mut_for_delta(stream, turn_id, ids);
    item.text.push_str(delta);
    item.clone()
}

fn set_stream_assistant_text(
    stream: &mut StreamState,
    turn_id: Option<String>,
    ids: &EventItemIds,
    text: &str,
) -> StreamAssistantItem {
    let item = stream_assistant_item_mut_for_text(stream, turn_id, ids);
    item.text = text.to_string();
    item.clone()
}

fn set_stream_assistant_snapshot_text(
    stream: &mut StreamState,
    turn_id: Option<String>,
    item_id: Option<String>,
    text: &str,
) {
    let item = stream_assistant_item_mut(stream, turn_id, item_id);
    item.text = text.to_string();
}

fn stream_assistant_item_mut_for_delta<'a>(
    stream: &'a mut StreamState,
    turn_id: Option<String>,
    ids: &EventItemIds,
) -> &'a mut StreamAssistantItem {
    let incoming_turn_id = turn_id.as_deref();
    if let Some(index) = stream
        .assistant_items
        .iter()
        .position(|item| assistant_item_exact_match(item, incoming_turn_id, ids))
    {
        return &mut stream.assistant_items[index];
    }
    if ids.is_identified()
        && let Some(index) = stream
            .assistant_items
            .iter()
            .position(|item| assistant_item_provisional_match(item, incoming_turn_id))
    {
        let item = &mut stream.assistant_items[index];
        item.item_id = ids.item_id.clone();
        if item.turn_id.is_none() {
            item.turn_id = turn_id;
        }
        return item;
    }
    if !ids.is_identified()
        && let Some(index) = stream
            .assistant_items
            .iter()
            .rposition(|item| turns_compatible(item.turn_id.as_deref(), incoming_turn_id))
    {
        let item = &mut stream.assistant_items[index];
        if item.turn_id.is_none() {
            item.turn_id = turn_id;
        }
        return item;
    }
    push_stream_assistant_item(stream, turn_id, ids.item_id.clone())
}

fn stream_assistant_item_mut_for_text<'a>(
    stream: &'a mut StreamState,
    turn_id: Option<String>,
    ids: &EventItemIds,
) -> &'a mut StreamAssistantItem {
    let incoming_turn_id = turn_id.as_deref();
    if let Some(index) = stream
        .assistant_items
        .iter()
        .position(|item| assistant_item_exact_match(item, incoming_turn_id, ids))
    {
        return &mut stream.assistant_items[index];
    }
    if ids.is_identified()
        && let Some(index) = stream
            .assistant_items
            .iter()
            .position(|item| assistant_item_provisional_match(item, incoming_turn_id))
    {
        let item = &mut stream.assistant_items[index];
        item.item_id = ids.item_id.clone();
        if item.turn_id.is_none() {
            item.turn_id = turn_id;
        }
        return item;
    }
    push_stream_assistant_item(stream, turn_id, ids.item_id.clone())
}

fn stream_assistant_item_mut(
    stream: &mut StreamState,
    turn_id: Option<String>,
    item_id: Option<String>,
) -> &mut StreamAssistantItem {
    let ids = EventItemIds {
        item_id: item_id.clone(),
        aliases: item_id.iter().cloned().collect(),
    };
    if let Some(index) = stream
        .assistant_items
        .iter()
        .position(|item| assistant_item_exact_match(item, turn_id.as_deref(), &ids))
    {
        return &mut stream.assistant_items[index];
    }
    if item_id.is_some()
        && let Some(index) = stream
            .assistant_items
            .iter()
            .position(|item| assistant_item_provisional_match(item, turn_id.as_deref()))
    {
        let item = &mut stream.assistant_items[index];
        item.item_id = item_id;
        if item.turn_id.is_none() {
            item.turn_id = turn_id;
        }
        return item;
    }
    if item_id.is_none()
        && let Some(index) = stream
            .assistant_items
            .iter()
            .rposition(|item| turns_compatible(item.turn_id.as_deref(), turn_id.as_deref()))
    {
        let item = &mut stream.assistant_items[index];
        if item.turn_id.is_none() {
            item.turn_id = turn_id;
        }
        return item;
    }
    push_stream_assistant_item(stream, turn_id, item_id)
}

fn push_stream_assistant_item(
    stream: &mut StreamState,
    turn_id: Option<String>,
    item_id: Option<String>,
) -> &mut StreamAssistantItem {
    stream.assistant_items.push(StreamAssistantItem {
        turn_id,
        item_id,
        text: String::new(),
    });
    stream
        .assistant_items
        .last_mut()
        .expect("stream assistant item just pushed")
}

fn assistant_item_exact_match(
    item: &StreamAssistantItem,
    turn_id: Option<&str>,
    ids: &EventItemIds,
) -> bool {
    if ids.is_identified() {
        ids.matches_id(item.item_id.as_deref())
            && turns_compatible(item.turn_id.as_deref(), turn_id)
    } else {
        item.item_id.is_none() && item.turn_id.as_deref() == turn_id
    }
}

fn assistant_item_provisional_match(item: &StreamAssistantItem, turn_id: Option<&str>) -> bool {
    item.item_id.is_none()
        && turn_id.is_some()
        && turns_compatible(item.turn_id.as_deref(), turn_id)
}

fn turns_compatible(existing: Option<&str>, incoming: Option<&str>) -> bool {
    match (existing, incoming) {
        (Some(existing), Some(incoming)) => existing == incoming,
        _ => true,
    }
}

fn upsert_streaming_assistant_message(
    state: &mut TuiState,
    turn_id: Option<String>,
    ids: &EventItemIds,
    text: &str,
    _finalized: bool,
) {
    let Some(stream) = &state.stream else {
        return;
    };
    let Some(detail) = &mut state.detail else {
        return;
    };
    if detail.server != stream.server || detail.thread_id != stream.thread_id {
        return;
    }
    let was_at_bottom = detail.is_at_bottom();
    let (message_index, display_text) =
        streaming_message_update_target(&detail.messages, turn_id.as_deref(), ids, text);
    if display_text.is_empty() {
        return;
    }
    if let Some(message) = message_index.and_then(|index| detail.messages.get_mut(index)) {
        message.lines = markdown_lines(&display_text, 100);
        message.raw_text = display_text;
        if message.turn_id.is_none() {
            message.turn_id = turn_id.clone();
        }
        if message.item_id.is_none() {
            message.item_id = ids.item_id.clone();
        }
    } else {
        detail.messages.push(message_block(
            turn_id,
            ids.item_id.clone(),
            "assistant",
            None,
            &display_text,
            100,
        ));
    }
    if was_at_bottom {
        detail.scroll = detail.bottom_scroll_position();
    }
    if !detail.search_query.is_empty() {
        let query = detail.search_query.clone();
        state.update_message_search(query);
    }
}

fn upsert_preview_assistant_message(
    state: &mut TuiState,
    turn_id: Option<String>,
    ids: &EventItemIds,
    text: &str,
    _finalized: bool,
) {
    let Some(stream) = &state.stream else {
        return;
    };
    if !preview_matches_thread(state, &stream.server, &stream.thread_id) {
        return;
    }
    let server = stream.server.clone();
    let thread_id = stream.thread_id.clone();
    ensure_preview_thread(state, &server, &thread_id);
    let messages = &mut state.browser.preview.messages;
    let (message_index, display_text) =
        streaming_message_update_target(messages, turn_id.as_deref(), ids, text);
    if display_text.is_empty() {
        return;
    }
    if let Some(message) = message_index.and_then(|index| messages.get_mut(index)) {
        message.lines = markdown_lines(&display_text, 100);
        message.raw_text = display_text;
        if message.turn_id.is_none() {
            message.turn_id = turn_id.clone();
        }
        if message.item_id.is_none() {
            message.item_id = ids.item_id.clone();
        }
    } else {
        messages.push(message_block(
            turn_id,
            ids.item_id.clone(),
            "assistant",
            None,
            &display_text,
            100,
        ));
    }
}

fn streaming_message_update_target(
    messages: &[MessageBlock],
    turn_id: Option<&str>,
    ids: &EventItemIds,
    text: &str,
) -> (Option<usize>, String) {
    if let Some(user_index) = latest_user_index_for_turn(messages, turn_id)
        && let Some(prior_index) = latest_assistant_match_before(messages, user_index, turn_id, ids)
    {
        let display_text = suffix_after_message_text(&messages[prior_index], text);
        let match_index = latest_assistant_match_after(messages, user_index, turn_id, ids);
        return (match_index, display_text);
    }

    let match_index = messages
        .iter()
        .rposition(|message| streaming_message_exact_match(message, turn_id, ids));
    let provisional_index = if ids.is_identified() && match_index.is_none() {
        messages
            .iter()
            .rposition(|message| streaming_message_provisional_match(message, turn_id))
    } else {
        None
    };
    (match_index.or(provisional_index), text.to_string())
}

fn latest_user_index_for_turn(messages: &[MessageBlock], turn_id: Option<&str>) -> Option<usize> {
    let turn_id = turn_id?;
    messages
        .iter()
        .rposition(|message| message.role == "user" && message.turn_id.as_deref() == Some(turn_id))
}

fn latest_assistant_match_before(
    messages: &[MessageBlock],
    before_index: usize,
    turn_id: Option<&str>,
    ids: &EventItemIds,
) -> Option<usize> {
    messages[..before_index]
        .iter()
        .rposition(|message| streaming_message_exact_match(message, turn_id, ids))
}

fn latest_assistant_match_after(
    messages: &[MessageBlock],
    after_index: usize,
    turn_id: Option<&str>,
    ids: &EventItemIds,
) -> Option<usize> {
    messages
        .iter()
        .enumerate()
        .skip(after_index + 1)
        .filter(|(_, message)| streaming_message_exact_match(message, turn_id, ids))
        .map(|(index, _)| index)
        .next_back()
}

fn suffix_after_message_text(message: &MessageBlock, text: &str) -> String {
    let prefix = message_text(message);
    text.strip_prefix(&prefix)
        .map(trim_stream_segment_boundary)
        .unwrap_or(text)
        .to_string()
}

fn trim_stream_segment_boundary(text: &str) -> &str {
    text.strip_prefix("\r\n")
        .or_else(|| text.strip_prefix('\n'))
        .or_else(|| text.strip_prefix(' '))
        .unwrap_or(text)
}

fn streaming_message_exact_match(
    message: &MessageBlock,
    turn_id: Option<&str>,
    ids: &EventItemIds,
) -> bool {
    if message.role != "assistant" {
        return false;
    }
    if ids.is_identified() {
        ids.matches_id(message.item_id.as_deref())
            && turns_compatible(message.turn_id.as_deref(), turn_id)
    } else {
        message.item_id.is_none() && message.turn_id.as_deref() == turn_id
    }
}

fn streaming_message_provisional_match(message: &MessageBlock, turn_id: Option<&str>) -> bool {
    message.role == "assistant"
        && message.item_id.is_none()
        && turn_id.is_some()
        && turns_compatible(message.turn_id.as_deref(), turn_id)
}

fn active_thread_id(state: &TuiState) -> Option<String> {
    match state.mode {
        Mode::Detail => state.detail.as_ref().map(|detail| detail.thread_id.clone()),
        _ => state.selected_thread_id().map(str::to_string),
    }
}

fn active_thread_key(state: &TuiState) -> Option<(String, String)> {
    match state.mode {
        Mode::Detail => state
            .detail
            .as_ref()
            .map(|detail| (detail.server.clone(), detail.thread_id.clone())),
        _ => state.selected_thread_key(),
    }
}

fn active_thread_cwd(state: &TuiState, server: &str, thread_id: &str) -> Option<String> {
    state
        .browser
        .rows
        .iter()
        .find(|row| row.server == server && row.id == thread_id)
        .map(|row| row.cwd.clone())
        .filter(|cwd| !cwd.trim().is_empty())
}

fn selected_thread_is_running(state: &TuiState, server: &str, thread_id: &str) -> bool {
    state.stream.as_ref().is_some_and(|stream| {
        stream.server == server && stream.thread_id == thread_id && stream_is_running_status(stream)
    }) || state
        .browser
        .rows
        .get(state.browser.selected)
        .is_some_and(|row| row.server == server && row.id == thread_id && row.is_running())
}

fn active_thread_is_archived(state: &TuiState) -> bool {
    match state.mode {
        Mode::Detail => state
            .detail
            .as_ref()
            .is_some_and(|detail| detail.status == "archived" || state.browser.archived),
        _ => state
            .browser
            .rows
            .get(state.browser.selected)
            .is_some_and(|row| row.status == "archived" || state.browser.archived),
    }
}

fn active_annotation(state: &TuiState) -> Option<String> {
    match state.mode {
        Mode::Detail => state
            .detail
            .as_ref()
            .and_then(|detail| detail.annotation.clone()),
        _ => state.selected_thread_annotation().map(str::to_string),
    }
}

fn active_thread_title(state: &TuiState) -> Option<String> {
    match state.mode {
        Mode::Detail => state.detail.as_ref().map(|detail| detail.title.clone()),
        _ => state
            .browser
            .rows
            .get(state.browser.selected)
            .map(|row| row.title.clone()),
    }
}

fn adjust_auto_refresh_interval(state: &mut TuiState, delta_seconds: i64) -> bool {
    let current = state.browser.auto_refresh_seconds as i64;
    let next = current
        .saturating_add(delta_seconds)
        .clamp(AUTO_REFRESH_MIN_SECS as i64, AUTO_REFRESH_MAX_SECS as i64) as u64;
    if state.browser.auto_refresh_seconds == next {
        return false;
    }
    state.browser.auto_refresh_seconds = next;
    state.prefs.refresh.interval_seconds = next;
    true
}

fn touch_browser_thread_updated(state: &mut TuiState, server: &str, thread_id: &str) {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    let updated = format_browser_updated_epoch(now, state.prefs.browser.relative_updated);
    for row in &mut state.browser.rows {
        if row.server == server && row.id == thread_id {
            row.updated = updated.clone();
            if let Some(raw_thread) = row.raw.get_mut("thread") {
                raw_thread["updatedAt"] = json!(now);
            } else {
                row.raw["updatedAt"] = json!(now);
            }
        }
    }
}

fn set_browser_thread_status(state: &mut TuiState, server: &str, thread_id: &str, status: &str) {
    for row in &mut state.browser.rows {
        if row.server == server && row.id == thread_id {
            row.set_status(status);
        }
    }
    if let Some(detail) = &mut state.detail
        && detail.server == server
        && detail.thread_id == thread_id
    {
        detail.status = status.to_string();
    }
}

fn apply_thread_load_status(state: &mut TuiState, server: &str, thread_id: &str, status: &Value) {
    let thread = &status["thread"];
    let lifecycle = thread_status_label(thread);
    let title = thread["name"]
        .as_str()
        .or_else(|| thread["preview"].as_str())
        .map(str::to_string);
    let cwd = thread["cwd"].as_str().map(str::to_string);
    let updated = thread["updatedAt"].as_i64().map(|updated_at| {
        format_browser_updated_epoch(updated_at, state.prefs.browser.relative_updated)
    });
    for row in &mut state.browser.rows {
        if row.server != server || row.id != thread_id {
            continue;
        }
        if let Some(title) = &title {
            row.title = title.clone();
        }
        if let Some(cwd) = &cwd {
            row.cwd = cwd.clone();
        }
        if let Some(updated) = &updated {
            row.updated = updated.clone();
        }
        if !thread.is_null() {
            if row.raw.get("thread").is_some() {
                row.raw["thread"] = thread.clone();
            } else {
                row.raw = thread.clone();
            }
        }
        row.set_status(lifecycle.clone());
    }
    if let Some(detail) = &mut state.detail
        && detail.server == server
        && detail.thread_id == thread_id
    {
        if let Some(title) = title {
            detail.title = title;
        }
        detail.status = lifecycle;
        detail.active_turn_id = status["activeTurnId"].as_str().map(str::to_string);
    }
}

fn reset_preview_for_thread(state: &mut TuiState, server: &str, thread_id: &str) {
    if state.browser.preview.server.as_deref() == Some(server)
        && state.browser.preview.thread_id.as_deref() == Some(thread_id)
    {
        state.browser.preview = Default::default();
    }
}

fn set_thread_name_in_state(
    state: &mut TuiState,
    server: &str,
    thread_id: &str,
    name: &str,
    thread: &Value,
) {
    for row in &mut state.browser.rows {
        if row.server == server && row.id == thread_id {
            row.title = name.to_string();
            if !thread.is_null() {
                if let Some(raw_thread) = row.raw.get_mut("thread") {
                    raw_thread["name"] = json!(name);
                } else {
                    row.raw["name"] = json!(name);
                }
            }
        }
    }
    if let Some(detail) = &mut state.detail
        && detail.server == server
        && detail.thread_id == thread_id
    {
        detail.title = name.to_string();
    }
}

fn apply_archive_change(
    state: &mut TuiState,
    server: &str,
    thread_id: &str,
    archived: bool,
    thread: &Value,
) {
    let status = if archived {
        "archived".to_string()
    } else {
        thread_status_label(thread)
    };
    for row in &mut state.browser.rows {
        if row.server == server && row.id == thread_id {
            row.status = status.clone();
            if !thread.is_null() {
                row.raw = thread.clone();
            }
        }
    }
    if state.browser.archived != archived {
        state
            .browser
            .rows
            .retain(|row| !(row.server == server && row.id == thread_id));
        state.browser.selected = state
            .browser
            .selected
            .min(state.browser.rows.len().saturating_sub(1));
    }
    if let Some(detail) = &mut state.detail
        && detail.server == server
        && detail.thread_id == thread_id
    {
        detail.status = status;
    }
}

fn copy_active_thread_id(state: &mut TuiState) -> Result<()> {
    let Some(thread_id) = active_thread_id(state) else {
        state.set_notice("no thread selected");
        return Ok(());
    };
    write_osc52_clipboard(&thread_id)?;
    state.set_notice(format!("copied {thread_id}"));
    Ok(())
}

fn write_osc52_clipboard(text: &str) -> Result<()> {
    let sequence = osc52_clipboard_sequence(text);
    let mut stdout = io::stdout();
    stdout
        .write_all(sequence.as_bytes())
        .context("failed to write OSC 52 clipboard sequence")?;
    stdout
        .flush()
        .context("failed to flush OSC 52 clipboard sequence")
}

fn osc52_clipboard_sequence(text: &str) -> String {
    format!("\x1b]52;c;{}\x07", base64_encode(text.as_bytes()))
}

fn base64_encode(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut encoded = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0];
        let b1 = chunk.get(1).copied().unwrap_or(0);
        let b2 = chunk.get(2).copied().unwrap_or(0);
        encoded.push(TABLE[(b0 >> 2) as usize] as char);
        encoded.push(TABLE[(((b0 & 0b0000_0011) << 4) | (b1 >> 4)) as usize] as char);
        if chunk.len() > 1 {
            encoded.push(TABLE[(((b1 & 0b0000_1111) << 2) | (b2 >> 6)) as usize] as char);
        } else {
            encoded.push('=');
        }
        if chunk.len() > 2 {
            encoded.push(TABLE[(b2 & 0b0011_1111) as usize] as char);
        } else {
            encoded.push('=');
        }
    }
    encoded
}

fn normalize_detail_turns_for_display(output: &mut Value) {
    if let Some(turns) = output["turns"]["data"].as_array_mut() {
        turns.reverse();
    }
}

fn thread_row(
    server: String,
    item: Value,
    source: BrowserSource,
    relative_updated: bool,
) -> ThreadRow {
    let thread = match source {
        BrowserSource::List => &item,
        BrowserSource::Search => item.get("thread").unwrap_or(&item),
    };
    let id = thread["id"].as_str().unwrap_or("").to_string();
    let title = thread["name"]
        .as_str()
        .or_else(|| thread["preview"].as_str())
        .unwrap_or(&id)
        .to_string();
    let status = thread_status_label(thread);
    let updated = thread["updatedAt"]
        .as_i64()
        .map(|updated_at| format_browser_updated_epoch(updated_at, relative_updated))
        .unwrap_or_default();
    let cwd = thread["cwd"].as_str().unwrap_or("").to_string();
    let annotation = thread["annotation"]["text"].as_str().map(str::to_string);
    let snippet = match source {
        BrowserSource::Search => item["snippet"].as_str().map(str::to_string),
        BrowserSource::List => None,
    };
    ThreadRow {
        server,
        id,
        title,
        status,
        updated,
        cwd,
        annotation,
        snippet,
        raw: item,
    }
}

fn thread_row_updated_epoch(row: &ThreadRow) -> i64 {
    let thread = match row.raw.get("thread") {
        Some(thread) => thread,
        None => &row.raw,
    };
    thread["updatedAt"].as_i64().unwrap_or(0)
}

fn thread_status_label(thread: &Value) -> String {
    thread["status"]["type"]
        .as_str()
        .or_else(|| thread["status"].as_str())
        .unwrap_or("")
        .to_string()
}

fn detail_state_for_server(
    server: String,
    output: Value,
    status_output: Option<Value>,
    thread_id: String,
    epoch: u64,
    current_cursor: Option<String>,
) -> DetailState {
    let thread = &output["thread"];
    let title = thread["name"]
        .as_str()
        .or_else(|| thread["preview"].as_str())
        .unwrap_or(&thread_id)
        .to_string();
    let status = thread["status"]["type"]
        .as_str()
        .or_else(|| thread["status"].as_str())
        .unwrap_or("")
        .to_string();
    let annotation = thread["annotation"]["text"].as_str().map(str::to_string);
    let width = 100;
    let mut messages = Vec::new();
    for turn in output["turns"]["data"].as_array().unwrap_or(&Vec::new()) {
        let turn_id = turn["id"].as_str().map(str::to_string);
        let timestamp = turn["startedAt"]
            .as_i64()
            .or_else(|| turn["completedAt"].as_i64())
            .map(format_epoch);
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
                    messages.push(message_block(
                        turn_id.clone(),
                        item["id"].as_str().map(str::to_string),
                        "user",
                        timestamp.clone(),
                        &text,
                        width,
                    ));
                }
                Some("agentMessage") => {
                    messages.push(message_block(
                        turn_id.clone(),
                        item["id"].as_str().map(str::to_string),
                        "assistant",
                        timestamp.clone(),
                        item["text"].as_str().unwrap_or(""),
                        width,
                    ));
                }
                _ => {}
            }
        }
    }
    DetailState {
        server,
        thread_id,
        title,
        status,
        annotation,
        messages,
        scroll: 0,
        search_query: String::new(),
        matches: Vec::new(),
        match_index: 0,
        next_cursor: output["turns"]["nextCursor"].as_str().map(str::to_string),
        backwards_cursor: output["turns"]["backwardsCursor"]
            .as_str()
            .map(str::to_string),
        current_cursor,
        active_turn_id: status_output
            .as_ref()
            .and_then(|value| value["activeTurnId"].as_str())
            .map(str::to_string),
        loading: false,
        epoch,
        last_refresh_at: Some(std::time::Instant::now()),
        viewport_height: None,
        viewport_width: None,
        last_error: None,
    }
}

#[cfg(test)]
fn detail_state(
    output: Value,
    status_output: Option<Value>,
    thread_id: String,
    epoch: u64,
    current_cursor: Option<String>,
) -> DetailState {
    detail_state_for_server(
        "work".to_string(),
        output,
        status_output,
        thread_id,
        epoch,
        current_cursor,
    )
}

fn message_block(
    turn_id: Option<String>,
    item_id: Option<String>,
    role: &str,
    timestamp: Option<String>,
    text: &str,
    width: usize,
) -> MessageBlock {
    MessageBlock {
        turn_id,
        item_id,
        role: role.to_string(),
        timestamp,
        raw_text: text.to_string(),
        lines: markdown_lines(text, width),
        is_match: false,
    }
}

fn markdown_lines(text: &str, width: usize) -> Vec<MessageLine> {
    let mut lines = Vec::new();
    if text.is_empty() {
        lines.push(MessageLine {
            kind: MessageLineKind::Text,
            text: String::new(),
            spans: Vec::new(),
        });
        return lines;
    }
    let parser = Parser::new_ext(text, Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH);
    let mut kind = MessageLineKind::Text;
    let mut buffer = String::new();
    let mut list_depth = 0usize;
    let mut item_open = false;
    let mut code_language: Option<String> = None;

    for event in parser {
        match event {
            MarkdownEvent::Start(Tag::Heading { .. }) => {
                flush_markdown_buffer(
                    &mut lines,
                    &mut buffer,
                    kind,
                    width,
                    code_language.as_deref(),
                );
                kind = MessageLineKind::Heading;
            }
            MarkdownEvent::End(TagEnd::Heading(_)) => {
                flush_markdown_buffer(
                    &mut lines,
                    &mut buffer,
                    kind,
                    width,
                    code_language.as_deref(),
                );
                kind = MessageLineKind::Text;
            }
            MarkdownEvent::Start(Tag::BlockQuote(_)) => {
                flush_markdown_buffer(
                    &mut lines,
                    &mut buffer,
                    kind,
                    width,
                    code_language.as_deref(),
                );
                kind = MessageLineKind::Quote;
            }
            MarkdownEvent::End(TagEnd::BlockQuote(_)) => {
                flush_markdown_buffer(
                    &mut lines,
                    &mut buffer,
                    kind,
                    width,
                    code_language.as_deref(),
                );
                kind = MessageLineKind::Text;
            }
            MarkdownEvent::Start(Tag::CodeBlock(block_kind)) => {
                flush_markdown_buffer(
                    &mut lines,
                    &mut buffer,
                    kind,
                    width,
                    code_language.as_deref(),
                );
                let label = match block_kind {
                    CodeBlockKind::Fenced(info) if !info.trim().is_empty() => {
                        code_language = Some(info.trim().to_string());
                        format!("code {}", info.trim())
                    }
                    _ => "code".to_string(),
                };
                lines.push(MessageLine {
                    kind: MessageLineKind::Code,
                    text: label,
                    spans: Vec::new(),
                });
                kind = MessageLineKind::Code;
            }
            MarkdownEvent::End(TagEnd::CodeBlock) => {
                flush_markdown_buffer(
                    &mut lines,
                    &mut buffer,
                    kind,
                    width,
                    code_language.as_deref(),
                );
                code_language = None;
                kind = MessageLineKind::Text;
            }
            MarkdownEvent::Start(Tag::List(_)) => {
                list_depth += 1;
            }
            MarkdownEvent::End(TagEnd::List(_)) => {
                flush_markdown_buffer(
                    &mut lines,
                    &mut buffer,
                    kind,
                    width,
                    code_language.as_deref(),
                );
                list_depth = list_depth.saturating_sub(1);
            }
            MarkdownEvent::Start(Tag::Item) => {
                flush_markdown_buffer(
                    &mut lines,
                    &mut buffer,
                    kind,
                    width,
                    code_language.as_deref(),
                );
                if list_depth > 0 {
                    buffer.push_str("- ");
                }
                item_open = true;
            }
            MarkdownEvent::End(TagEnd::Item) => {
                flush_markdown_buffer(
                    &mut lines,
                    &mut buffer,
                    kind,
                    width,
                    code_language.as_deref(),
                );
                item_open = false;
            }
            MarkdownEvent::Text(value) | MarkdownEvent::Code(value) => {
                buffer.push_str(&value);
            }
            MarkdownEvent::SoftBreak | MarkdownEvent::HardBreak => {
                flush_markdown_buffer(
                    &mut lines,
                    &mut buffer,
                    kind,
                    width,
                    code_language.as_deref(),
                );
                if item_open && list_depth > 0 {
                    buffer.push_str("  ");
                }
            }
            MarkdownEvent::End(TagEnd::Paragraph) => {
                flush_markdown_buffer(
                    &mut lines,
                    &mut buffer,
                    kind,
                    width,
                    code_language.as_deref(),
                );
                if list_depth == 0 {
                    push_blank_line(&mut lines, kind);
                }
            }
            _ => {}
        }
    }
    flush_markdown_buffer(
        &mut lines,
        &mut buffer,
        kind,
        width,
        code_language.as_deref(),
    );
    if lines.is_empty() {
        lines.push(MessageLine {
            kind: MessageLineKind::Text,
            text: String::new(),
            spans: Vec::new(),
        });
    }
    while lines.len() > 1 && lines.last().is_some_and(|line| line.text.is_empty()) {
        lines.pop();
    }
    lines
}

fn flush_markdown_buffer(
    lines: &mut Vec<MessageLine>,
    buffer: &mut String,
    kind: MessageLineKind,
    width: usize,
    code_language: Option<&str>,
) {
    if buffer.is_empty() {
        return;
    }
    if kind == MessageLineKind::Code {
        flush_code_buffer(lines, buffer, width, code_language);
        return;
    }
    for raw_line in buffer.lines() {
        if raw_line.is_empty() {
            push_blank_line(lines, kind);
            continue;
        }
        for wrapped in textwrap::wrap(raw_line, width) {
            lines.push(MessageLine {
                kind,
                text: wrapped.to_string(),
                spans: Vec::new(),
            });
        }
    }
    buffer.clear();
}

fn flush_code_buffer(
    lines: &mut Vec<MessageLine>,
    buffer: &mut String,
    width: usize,
    code_language: Option<&str>,
) {
    if let Some(highlighted) = highlighted_code_lines(code_language, buffer) {
        for spans in highlighted {
            let text = spans
                .iter()
                .map(|span| span.text.as_str())
                .collect::<String>();
            lines.push(MessageLine {
                kind: MessageLineKind::Code,
                text,
                spans,
            });
        }
        buffer.clear();
        return;
    }
    for raw_line in buffer.lines() {
        if raw_line.is_empty() {
            push_blank_line(lines, MessageLineKind::Code);
            continue;
        }
        for wrapped in textwrap::wrap(raw_line, width) {
            lines.push(MessageLine {
                kind: MessageLineKind::Code,
                text: wrapped.to_string(),
                spans: Vec::new(),
            });
        }
    }
    buffer.clear();
}

fn push_blank_line(lines: &mut Vec<MessageLine>, kind: MessageLineKind) {
    if lines.last().is_some_and(|line| line.text.is_empty()) {
        return;
    }
    lines.push(MessageLine {
        kind,
        text: String::new(),
        spans: Vec::new(),
    });
}

#[cfg(feature = "tui-syntax-highlighting")]
fn highlighted_code_lines(language: Option<&str>, code: &str) -> Option<Vec<Vec<MessageSpan>>> {
    highlight::highlight_code_lines(language, code)
}

#[cfg(not(feature = "tui-syntax-highlighting"))]
fn highlighted_code_lines(_language: Option<&str>, _code: &str) -> Option<Vec<Vec<MessageSpan>>> {
    None
}

fn filter_search_cwd(output: &mut Value, cwd: &str) {
    let Some(data) = output["data"].as_array_mut() else {
        return;
    };
    data.retain(|item| item["thread"]["cwd"].as_str() == Some(cwd));
}

fn format_epoch(value: i64) -> String {
    chrono::DateTime::from_timestamp(value, 0)
        .map(|timestamp| timestamp.format("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_default()
}

fn format_current_epoch() -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    format_epoch(now)
}

fn format_browser_updated_epoch(value: i64, relative: bool) -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    format_browser_updated_epoch_at(value, now, relative)
}

fn format_browser_updated_epoch_at(value: i64, now: i64, relative: bool) -> String {
    if !relative || value > now {
        return format_epoch(value);
    }
    let age = now.saturating_sub(value);
    if age < 60 {
        return format!("{} ago", plural_duration(age, "second"));
    }
    if age < 60 * 60 {
        return format!("{} ago", plural_duration(age / 60, "minute"));
    }
    if age < 24 * 60 * 60 {
        return format!("{} ago", plural_duration(age / (60 * 60), "hour"));
    }
    let days = age / (24 * 60 * 60);
    let hours = (age % (24 * 60 * 60)) / (60 * 60);
    if hours == 0 {
        format!("{} ago", plural_duration(days, "day"))
    } else {
        format!(
            "{}, {} ago",
            plural_duration(days, "day"),
            plural_duration(hours, "hour")
        )
    }
}

fn plural_duration(value: i64, unit: &str) -> String {
    if value == 1 {
        format!("{value} {unit}")
    } else {
        format!("{value} {unit}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::tui::prefs::TuiPrefs;

    fn stream_event(event: Value) -> AppEvent {
        AppEvent::StreamEvent {
            stream_id: None,
            event,
        }
    }

    fn stream_event_for(stream_id: u64, event: Value) -> AppEvent {
        AppEvent::StreamEvent {
            stream_id: Some(stream_id),
            event,
        }
    }

    fn stream_finished(
        stream_id: u64,
        thread_id: &str,
        turn_id: Option<&str>,
        status: StreamStatus,
    ) -> AppEvent {
        AppEvent::StreamFinished {
            server: "work".to_string(),
            stream_id,
            thread_id: thread_id.to_string(),
            turn_id: turn_id.map(str::to_string),
            status,
        }
    }

    fn stream_failed(
        stream_id: u64,
        thread_id: &str,
        turn_id: Option<&str>,
        error: &str,
    ) -> AppEvent {
        AppEvent::StreamFailed {
            server: "work".to_string(),
            stream_id: Some(stream_id),
            thread_id: thread_id.to_string(),
            turn_id: turn_id.map(str::to_string),
            error: error.to_string(),
        }
    }

    fn test_target() -> Target {
        Target {
            server: "work".to_string(),
            endpoint: crate::config::Endpoint::Unix {
                path: "/tmp/missing.sock".into(),
            },
            model: None,
            model_reasoning_effort: None,
        }
    }

    fn test_targets() -> TuiTargets {
        TuiTargets::new(vec![test_target()]).expect("test targets")
    }

    #[test]
    fn search_clearing_switches_back_to_list_mode() {
        let mut state = TuiState::new(TuiInit {
            query: Some("abc".to_string()),
            since: None,
            cwd: None,
            archived: false,
            limit: 50,
            sort: None,
            descending: true,
            prefs: TuiPrefs::default(),
        });
        assert_eq!(state.browser.source, BrowserSource::Search);
        state.update_query(String::new());
        assert_eq!(state.browser.source, BrowserSource::List);
    }

    #[test]
    fn stale_browser_results_are_ignored() {
        let mut state = TuiState::new(TuiInit {
            query: None,
            since: None,
            cwd: None,
            archived: false,
            limit: 50,
            sort: None,
            descending: true,
            prefs: TuiPrefs::default(),
        });
        state.browser.epoch = 2;
        state.set_browser_rows(
            1,
            vec![ThreadRow {
                server: "work".to_string(),
                id: "old".to_string(),
                title: "old".to_string(),
                status: String::new(),
                updated: String::new(),
                cwd: String::new(),
                annotation: None,
                snippet: None,
                raw: serde_json::json!({}),
            }],
            None,
            None,
            None,
        );
        assert!(state.browser.rows.is_empty());
    }

    #[test]
    fn notices_clear_after_expiry() {
        let mut state = TuiState::new(TuiInit {
            query: None,
            since: None,
            cwd: None,
            archived: false,
            limit: 50,
            sort: None,
            descending: true,
            prefs: TuiPrefs::default(),
        });

        state.set_notice("copied thread-1");
        assert_eq!(
            state.notice.as_ref().map(|notice| notice.message.as_str()),
            Some("copied thread-1")
        );
        state.notice.as_mut().expect("notice").expires_at =
            std::time::Instant::now() - Duration::from_secs(1);

        state.clear_expired_notice();

        assert!(state.notice.is_none());
    }

    #[test]
    fn browser_rows_store_current_cursor_for_refresh() {
        let mut state = TuiState::new(TuiInit {
            query: None,
            since: None,
            cwd: None,
            archived: false,
            limit: 50,
            sort: None,
            descending: true,
            prefs: TuiPrefs::default(),
        });
        state.browser.epoch = 1;
        state.set_browser_rows(
            1,
            vec![ThreadRow {
                server: "work".to_string(),
                id: "current".to_string(),
                title: "current".to_string(),
                status: String::new(),
                updated: String::new(),
                cwd: String::new(),
                annotation: None,
                snippet: None,
                raw: serde_json::json!({}),
            }],
            Some("older".to_string()),
            Some("newer".to_string()),
            Some("page-2".to_string()),
        );
        assert_eq!(state.browser.current_cursor.as_deref(), Some("page-2"));
        assert_eq!(state.browser.next_cursor.as_deref(), Some("older"));
        assert_eq!(state.browser.backwards_cursor.as_deref(), Some("newer"));
    }

    #[test]
    fn browser_refresh_preserves_locally_known_status_over_not_loaded() {
        let mut state = TuiState::new(TuiInit {
            query: None,
            since: None,
            cwd: None,
            archived: false,
            limit: 50,
            sort: None,
            descending: true,
            prefs: TuiPrefs::default(),
        });
        state.browser.rows = vec![test_thread_row("t1", "notLoaded")];
        set_browser_thread_status(&mut state, "work", "t1", "idle");
        state.browser.epoch = 1;

        state.set_browser_rows(
            1,
            vec![test_thread_row("t1", "notLoaded")],
            None,
            None,
            None,
        );

        assert_eq!(state.browser.rows[0].status, "idle");
        assert_eq!(
            state.browser.rows[0].raw["status"]["type"].as_str(),
            Some("idle")
        );
    }

    #[test]
    fn thread_status_updates_are_isolated_by_server_and_thread_id() {
        let mut state = TuiState::new(TuiInit {
            query: None,
            since: None,
            cwd: None,
            archived: false,
            limit: 50,
            sort: None,
            descending: true,
            prefs: TuiPrefs::default(),
        });
        let mut work = test_thread_row("shared", "idle");
        work.server = "work".to_string();
        work.raw = serde_json::json!({"id": "shared", "status": {"type": "idle"}});
        let mut main = test_thread_row("shared", "idle");
        main.server = "main".to_string();
        main.raw = serde_json::json!({"id": "shared", "status": {"type": "idle"}});
        state.browser.rows = vec![work, main];

        set_browser_thread_status(&mut state, "main", "shared", "active");

        assert_eq!(state.browser.rows[0].status, "idle");
        assert_eq!(state.browser.rows[1].status, "active");
        assert_eq!(
            state.browser.rows[0].raw["status"]["type"].as_str(),
            Some("idle")
        );
        assert_eq!(
            state.browser.rows[1].raw["status"]["type"].as_str(),
            Some("active")
        );
    }

    #[test]
    fn browser_refresh_preserves_selection_by_server_and_thread_id() {
        let mut state = TuiState::new(TuiInit {
            query: None,
            since: None,
            cwd: None,
            archived: false,
            limit: 50,
            sort: None,
            descending: true,
            prefs: TuiPrefs::default(),
        });
        let mut work = test_thread_row("shared", "idle");
        work.server = "work".to_string();
        let mut main = test_thread_row("shared", "idle");
        main.server = "main".to_string();
        state.browser.rows = vec![work.clone(), main.clone()];
        state.browser.selected = 1;
        state.browser.epoch = 1;

        state.set_browser_rows(1, vec![work, main], None, None, None);

        assert_eq!(state.browser.rows[state.browser.selected].server, "main");
        assert_eq!(state.browser.rows[state.browser.selected].id, "shared");
    }

    #[test]
    fn browser_mode_server_resolution_prefers_selected_row_over_stale_detail() {
        let mut state = TuiState::new(TuiInit {
            query: None,
            since: None,
            cwd: None,
            archived: false,
            limit: 50,
            sort: None,
            descending: true,
            prefs: TuiPrefs::default(),
        });
        let mut work = test_thread_row("shared", "idle");
        work.server = "work".to_string();
        let mut main = test_thread_row("shared", "idle");
        main.server = "main".to_string();
        state.browser.rows = vec![work, main];
        state.browser.selected = 1;
        state.detail = Some(detail_state(
            serde_json::json!({
                "thread": {"id": "shared", "name": "Shared", "status": {"type": "idle"}},
                "turns": {"nextCursor": null, "backwardsCursor": null, "data": []}
            }),
            None,
            "shared".to_string(),
            1,
            None,
        ));
        state.detail.as_mut().unwrap().server = "work".to_string();
        state.mode = Mode::Browser;

        assert_eq!(
            active_server_for_thread(&state, "shared").as_deref(),
            Some("main")
        );
    }

    #[test]
    fn event_server_resolution_returns_none_when_event_has_no_server_context() {
        let state = TuiState::new(TuiInit {
            query: None,
            since: None,
            cwd: None,
            archived: false,
            limit: 50,
            sort: None,
            descending: true,
            prefs: TuiPrefs::default(),
        });

        assert_eq!(
            resolve_event_server(
                &state,
                &serde_json::json!({"type": "delta"}),
                Some("missing")
            ),
            None
        );
    }

    #[test]
    fn browser_updated_time_is_toggleable_and_relative_for_all_past_times() {
        let now = 1_700_000_000;

        assert_eq!(
            format_browser_updated_epoch_at(now - 30, now, true),
            "30 seconds ago"
        );
        assert_eq!(
            format_browser_updated_epoch_at(now - 5 * 60, now, true),
            "5 minutes ago"
        );
        assert_eq!(
            format_browser_updated_epoch_at(now - 2 * 60 * 60, now, true),
            "2 hours ago"
        );
        assert_eq!(
            format_browser_updated_epoch_at(now - 24 * 60 * 60, now, true),
            "1 day ago"
        );
        assert_eq!(
            format_browser_updated_epoch_at(now - (3 * 24 * 60 * 60 + 4 * 60 * 60), now, true),
            "3 days, 4 hours ago"
        );
        assert_eq!(
            format_browser_updated_epoch_at(now - 30, now, false),
            "2023-11-14 22:12"
        );
        assert_eq!(
            format_browser_updated_epoch_at(now + 60, now, true),
            "2023-11-14 22:14"
        );
    }

    #[test]
    fn auto_refresh_interval_adjustment_clamps_and_updates_prefs() {
        let mut state = TuiState::new(TuiInit {
            query: None,
            since: None,
            cwd: None,
            archived: false,
            limit: 50,
            sort: None,
            descending: true,
            prefs: TuiPrefs::default(),
        });

        adjust_auto_refresh_interval(&mut state, -(AUTO_REFRESH_MAX_SECS as i64));
        assert_eq!(state.browser.auto_refresh_seconds, AUTO_REFRESH_MIN_SECS);
        assert_eq!(state.prefs.refresh.interval_seconds, AUTO_REFRESH_MIN_SECS);

        adjust_auto_refresh_interval(&mut state, AUTO_REFRESH_MAX_SECS as i64);
        assert_eq!(state.browser.auto_refresh_seconds, AUTO_REFRESH_MAX_SECS);
        assert_eq!(state.prefs.refresh.interval_seconds, AUTO_REFRESH_MAX_SECS);
    }

    #[test]
    fn tui_init_clamps_loaded_auto_refresh_interval() {
        let mut prefs = TuiPrefs::default();
        prefs.refresh.interval_seconds = 1;
        let state = TuiState::new(TuiInit {
            query: None,
            since: None,
            cwd: None,
            archived: false,
            limit: 50,
            sort: None,
            descending: true,
            prefs,
        });
        assert_eq!(state.browser.auto_refresh_seconds, AUTO_REFRESH_MIN_SECS);

        let mut prefs = TuiPrefs::default();
        prefs.refresh.interval_seconds = AUTO_REFRESH_MAX_SECS + 1;
        let state = TuiState::new(TuiInit {
            query: None,
            since: None,
            cwd: None,
            archived: false,
            limit: 50,
            sort: None,
            descending: true,
            prefs,
        });
        assert_eq!(state.browser.auto_refresh_seconds, AUTO_REFRESH_MAX_SECS);
    }

    #[test]
    fn browser_rows_group_running_threads_first_and_preserve_selection() {
        let mut state = TuiState::new(TuiInit {
            query: None,
            since: None,
            cwd: None,
            archived: false,
            limit: 50,
            sort: None,
            descending: true,
            prefs: TuiPrefs::default(),
        });
        state.browser.epoch = 1;
        state.browser.rows = vec![
            test_thread_row("idle-1", "idle"),
            test_thread_row("active-1", "active"),
            test_thread_row("idle-2", "idle"),
        ];
        state.browser.selected = 2;

        state.set_browser_rows(
            1,
            vec![
                test_thread_row("idle-1", "idle"),
                test_thread_row("active-1", "active"),
                test_thread_row("idle-2", "idle"),
            ],
            None,
            None,
            None,
        );

        assert_eq!(
            state
                .browser
                .rows
                .iter()
                .map(|row| row.id.as_str())
                .collect::<Vec<_>>(),
            vec!["active-1", "idle-1", "idle-2"]
        );
        assert_eq!(state.selected_thread_id(), Some("idle-2"));
    }

    #[test]
    fn detail_state_builds_message_blocks() {
        let detail = detail_state(
            serde_json::json!({
                "thread": {"id": "t1", "name": "Thread", "status": {"type": "idle"}},
                "turns": {"nextCursor": null, "backwardsCursor": null, "data": [
                    {"id": "turn-1", "startedAt": 1780680000, "items": [
                        {"id": "item-user", "type": "userMessage", "content": [{"text": "hello"}]},
                        {"id": "item-agent", "type": "agentMessage", "text": "# Summary\n> note\n```rust\nfn main() {}\n```"}
                    ]}
                ]}
            }),
            None,
            "t1".to_string(),
            1,
            None,
        );
        assert_eq!(detail.messages.len(), 2);
        assert_eq!(detail.messages[0].role, "user");
        assert_eq!(detail.messages[0].turn_id.as_deref(), Some("turn-1"));
        assert_eq!(detail.messages[0].item_id.as_deref(), Some("item-user"));
        assert_eq!(detail.messages[0].lines[0].text, "hello");
        assert_eq!(detail.messages[1].role, "assistant");
        assert_eq!(detail.messages[1].lines.len(), 5);
        assert_eq!(detail.messages[1].lines[0].kind, MessageLineKind::Heading);
        assert_eq!(detail.messages[1].lines[1].kind, MessageLineKind::Quote);
        assert_eq!(detail.messages[1].lines[2].text, "");
        assert_eq!(detail.messages[1].lines[3].kind, MessageLineKind::Code);
        assert_eq!(detail.messages[1].lines[4].kind, MessageLineKind::Code);
        assert_eq!(detail.messages[1].lines[3].text, "code rust");
    }

    #[test]
    fn detail_output_normalizes_descending_turns_for_display() {
        let mut output = serde_json::json!({
            "thread": {"id": "t1"},
            "turns": {"data": [
                {"id": "new"},
                {"id": "middle"},
                {"id": "old"}
            ]}
        });

        normalize_detail_turns_for_display(&mut output);

        let ids = output["turns"]["data"]
            .as_array()
            .unwrap()
            .iter()
            .map(|turn| turn["id"].as_str().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(ids, vec!["old", "middle", "new"]);
    }

    #[test]
    fn preview_detail_state_keeps_recent_user_and_assistant_messages() {
        let detail = detail_state(
            serde_json::json!({
            "thread": {"id": "t1"},
            "turns": {"data": [
                {"id": "old", "items": [
                    {"id": "u1", "type": "userMessage", "content": [{"text": "older user"}]},
                    {"id": "a1", "type": "agentMessage", "text": "older assistant"}
                ]},
                {"id": "new", "items": [
                    {"id": "u2", "type": "userMessage", "content": [{"text": "latest user"}]},
                    {"id": "a2", "type": "agentMessage", "text": "latest\nassistant"}
                ]}
            ]}
            }),
            None,
            "t1".to_string(),
            0,
            None,
        );

        let roles = detail
            .messages
            .iter()
            .map(|message| message.role.as_str())
            .collect::<Vec<_>>();
        assert_eq!(roles, vec!["user", "assistant", "user", "assistant"]);
        assert_eq!(detail.messages[2].lines[0].text, "latest user");
        assert_eq!(detail.messages[3].lines[0].text, "latest");
        assert_eq!(detail.messages[3].lines[1].text, "assistant");
    }

    #[tokio::test]
    async fn selected_preview_schedules_and_ignores_stale_results() {
        let mut prefs = TuiPrefs::default();
        prefs.browser.preview_pane = true;
        let mut state = TuiState::new(TuiInit {
            query: None,
            since: None,
            cwd: None,
            archived: false,
            limit: 50,
            sort: None,
            descending: true,
            prefs,
        });
        state.browser.rows = vec![test_thread_row("t1", "idle")];
        let (preview_tx, mut preview_rx) = mpsc::channel(1);

        schedule_selected_preview_if_needed(&mut state, &preview_tx)
            .await
            .unwrap();

        let request = preview_rx.recv().await.expect("preview request");
        assert_eq!(request.thread_id, "t1");
        assert_eq!(request.epoch, 1);
        assert!(state.browser.preview.loading);

        handle_app_event(
            AppEvent::PreviewLoaded {
                server: "work".to_string(),
                epoch: 0,
                thread_id: "t1".to_string(),
                messages: vec![message_block(None, None, "assistant", None, "stale", 100)],
            },
            &mut state,
        );
        assert!(state.browser.preview.messages.is_empty());

        handle_app_event(
            AppEvent::PreviewLoaded {
                server: "work".to_string(),
                epoch: 1,
                thread_id: "t1".to_string(),
                messages: vec![message_block(None, None, "assistant", None, "fresh", 100)],
            },
            &mut state,
        );
        assert_eq!(state.browser.preview.messages.len(), 1);
        assert_eq!(state.browser.preview.messages[0].lines[0].text, "fresh");
    }

    #[test]
    fn markdown_lines_preserve_paragraph_and_code_gaps() {
        let lines = markdown_lines(
            "first paragraph\n\nsecond paragraph\n\n```text\none\n\ntwo\n```",
            100,
        );
        let texts = lines
            .iter()
            .map(|line| line.text.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            texts,
            vec![
                "first paragraph",
                "",
                "second paragraph",
                "",
                "code text",
                "one",
                "",
                "two"
            ]
        );
    }

    #[cfg(feature = "tui-syntax-highlighting")]
    #[test]
    fn markdown_code_blocks_include_highlight_spans_when_feature_enabled() {
        let lines = markdown_lines("```rust\nfn main() {}\n```", 100);
        assert_eq!(lines[0].text, "code rust");
        assert_eq!(lines[1].kind, MessageLineKind::Code);
        assert_eq!(lines[1].text, "fn main() {}");
        assert!(!lines[1].spans.is_empty());
    }

    #[test]
    fn message_search_marks_blocks_and_navigates_matches() {
        let mut state = TuiState::new(TuiInit {
            query: None,
            since: None,
            cwd: None,
            archived: false,
            limit: 50,
            sort: None,
            descending: true,
            prefs: TuiPrefs::default(),
        });
        state.detail = Some(detail_state(
            serde_json::json!({
                "thread": {"id": "t1", "name": "Thread", "status": {"type": "idle"}},
                "turns": {"nextCursor": null, "backwardsCursor": null, "data": [
                    {"id": "turn-1", "items": [
                        {"id": "a", "type": "agentMessage", "text": "first prune match"},
                        {"id": "b", "type": "agentMessage", "text": "no hit"},
                        {"id": "c", "type": "agentMessage", "text": "second prune match"}
                    ]}
                ]}
            }),
            None,
            "t1".to_string(),
            1,
            None,
        ));
        state.update_message_search("prune".to_string());
        let detail = state.detail.as_ref().expect("detail");
        assert_eq!(detail.matches, vec![0, 2]);
        assert_eq!(detail.match_index, 0);
        assert_eq!(detail.scroll, 0);
        assert!(detail.messages[0].is_match);
        assert!(!detail.messages[1].is_match);

        state.next_message_match();
        let detail = state.detail.as_ref().expect("detail");
        assert_eq!(detail.match_index, 1);
        assert_eq!(detail.scroll as usize, detail.message_scroll_offset(2));

        state.previous_message_match();
        let detail = state.detail.as_ref().expect("detail");
        assert_eq!(detail.match_index, 0);
    }

    #[test]
    fn detail_pages_append_and_prepend_without_duplicates() {
        let mut state = TuiState::new(TuiInit {
            query: None,
            since: None,
            cwd: None,
            archived: false,
            limit: 50,
            sort: None,
            descending: true,
            prefs: TuiPrefs::default(),
        });
        state.detail = Some(detail_state(
            serde_json::json!({
                "thread": {"id": "t1", "name": "Thread", "status": {"type": "idle"}},
                "turns": {"nextCursor": "older", "backwardsCursor": "newer", "data": [
                    {"id": "turn-2", "items": [
                        {"id": "middle", "type": "agentMessage", "text": "middle"}
                    ]}
                ]}
            }),
            None,
            "t1".to_string(),
            1,
            None,
        ));

        let older = detail_state(
            serde_json::json!({
                "thread": {"id": "t1", "name": "Thread", "status": {"type": "idle"}},
                "turns": {"nextCursor": null, "backwardsCursor": "newer", "data": [
                    {"id": "turn-1", "items": [
                        {"id": "old", "type": "agentMessage", "text": "old"}
                    ]},
                    {"id": "turn-2", "items": [
                        {"id": "middle", "type": "agentMessage", "text": "middle"}
                    ]}
                ]}
            }),
            None,
            "t1".to_string(),
            1,
            Some("older".to_string()),
        );
        state.extend_detail_older(1, older);
        let detail = state.detail.as_ref().expect("detail");
        assert_eq!(detail.messages.len(), 2);
        assert_eq!(detail.messages[0].item_id.as_deref(), Some("old"));
        assert_eq!(detail.messages[1].item_id.as_deref(), Some("middle"));

        let newer = detail_state(
            serde_json::json!({
                "thread": {"id": "t1", "name": "Thread", "status": {"type": "idle"}},
                "turns": {"nextCursor": "older", "backwardsCursor": null, "data": [
                    {"id": "turn-2", "items": [
                        {"id": "middle", "type": "agentMessage", "text": "middle"}
                    ]},
                    {"id": "turn-3", "items": [
                        {"id": "new", "type": "agentMessage", "text": "new"}
                    ]}
                ]}
            }),
            None,
            "t1".to_string(),
            1,
            Some("newer".to_string()),
        );
        state.extend_detail_newer(1, newer);
        let detail = state.detail.as_ref().expect("detail");
        assert_eq!(detail.messages.len(), 3);
        assert_eq!(detail.messages[0].item_id.as_deref(), Some("old"));
        assert_eq!(detail.messages[1].item_id.as_deref(), Some("middle"));
        assert_eq!(detail.messages[2].item_id.as_deref(), Some("new"));
    }

    #[test]
    fn stream_delta_updates_detail_transcript_and_search() {
        let mut state = TuiState::new(TuiInit {
            query: None,
            since: None,
            cwd: None,
            archived: false,
            limit: 50,
            sort: None,
            descending: true,
            prefs: TuiPrefs::default(),
        });
        state.detail = Some(detail_state(
            serde_json::json!({
                "thread": {"id": "t1", "name": "Thread", "status": {"type": "idle"}},
                "turns": {"nextCursor": null, "backwardsCursor": null, "data": []}
            }),
            None,
            "t1".to_string(),
            1,
            None,
        ));
        append_detail_message(
            &mut state,
            "t1",
            None,
            "user",
            Some("2026-06-05 09:00".to_string()),
            "please stream",
        );

        handle_app_event(
            stream_event(serde_json::json!({
                "type": "accepted",
                "threadId": "t1",
                "turnId": "turn-1",
                "status": "accepted",
                "prompt": "please stream"
            })),
            &mut state,
        );
        handle_app_event(
            stream_event(serde_json::json!({
                "type": "delta",
                "threadId": "t1",
                "turnId": "turn-1",
                "itemId": "assistant-1",
                "delta": "first prune"
            })),
            &mut state,
        );
        handle_app_event(
            stream_event(serde_json::json!({
                "type": "delta",
                "threadId": "t1",
                "turnId": "turn-1",
                "itemId": "assistant-1",
                "delta": " chunk"
            })),
            &mut state,
        );

        state.update_message_search("prune".to_string());
        let detail = state.detail.as_ref().expect("detail");
        assert_eq!(detail.messages.len(), 2);
        assert_eq!(detail.messages[0].turn_id.as_deref(), Some("turn-1"));
        assert_eq!(detail.messages[1].role, "assistant");
        assert_eq!(detail.messages[1].item_id.as_deref(), Some("assistant-1"));
        assert_eq!(detail.messages[1].timestamp, None);
        assert_eq!(detail.messages[1].lines[0].text, "first prune chunk");
        assert_eq!(detail.matches, vec![1]);
    }

    #[test]
    fn stream_compose_queues_prompt_on_active_stream_without_replacing_turn() {
        let mut state = TuiState::new(TuiInit {
            query: None,
            since: None,
            cwd: None,
            archived: false,
            limit: 50,
            sort: None,
            descending: true,
            prefs: TuiPrefs::default(),
        });
        state.mode = Mode::Detail;
        state.detail = Some(detail_state(
            serde_json::json!({
                "thread": {"id": "t1", "name": "Thread", "status": {"type": "active"}},
                "turns": {"nextCursor": null, "backwardsCursor": null, "data": [
                    {"id": "turn-1", "items": [
                        {"id": "u1", "type": "userMessage", "content": [{"text": "test 1"}]}
                    ]}
                ]}
            }),
            Some(serde_json::json!({"activeTurnId": "turn-1"})),
            "t1".to_string(),
            1,
            None,
        ));
        state.stream = Some(StreamState::new_with_id(
            7,
            "t1".to_string(),
            Some("turn-1".to_string()),
            StreamStatus::Running,
            true,
        ));
        let (control_tx, mut control_rx) = mpsc::unbounded_channel();
        state.stream_control = Some(control_tx);
        let (app_tx, _app_rx) = mpsc::unbounded_channel();

        submit_compose(
            &mut state,
            ComposeState {
                target: ComposeTarget::NewTurn {
                    server: "work".to_string(),
                    thread_id: "t1".to_string(),
                },
                text: "test 2".to_string(),
                send_mode: SendMode::Stream,
                return_to_detail: true,
            },
            &test_target(),
            false,
            &app_tx,
            Mode::Detail,
        );

        let stream = state.stream.as_ref().expect("stream");
        assert_eq!(stream.id, 7);
        assert_eq!(stream.turn_id.as_deref(), Some("turn-1"));
        assert!(matches!(stream.status, StreamStatus::Running));
        assert!(matches!(
            control_rx.try_recv(),
            Ok(TurnControl::Submit { prompt, yolo: false }) if prompt == "test 2"
        ));
        let detail = state.detail.as_ref().expect("detail");
        let pending = detail.messages.last().expect("pending user");
        assert_eq!(pending.role, "user");
        assert_eq!(pending.turn_id, None);
        assert_eq!(message_text(pending), "test 2");
    }

    #[tokio::test]
    async fn steer_compose_appends_user_message_in_active_turn() {
        let mut state = TuiState::new(TuiInit {
            query: None,
            since: None,
            cwd: None,
            archived: false,
            limit: 50,
            sort: None,
            descending: true,
            prefs: TuiPrefs::default(),
        });
        state.mode = Mode::Detail;
        state.detail = Some(detail_state(
            serde_json::json!({
                "thread": {"id": "t1", "name": "Thread", "status": {"type": "active"}},
                "turns": {"nextCursor": null, "backwardsCursor": null, "data": [
                    {"id": "turn-1", "items": [
                        {"id": "u1", "type": "userMessage", "content": [{"text": "test 1"}]}
                    ]}
                ]}
            }),
            Some(serde_json::json!({"activeTurnId": "turn-1"})),
            "t1".to_string(),
            1,
            None,
        ));
        let (app_tx, _app_rx) = mpsc::unbounded_channel();

        submit_compose(
            &mut state,
            ComposeState {
                target: ComposeTarget::Steer {
                    server: "work".to_string(),
                    thread_id: "t1".to_string(),
                    turn_id: "turn-1".to_string(),
                },
                text: "steer here".to_string(),
                send_mode: SendMode::Stream,
                return_to_detail: true,
            },
            &test_target(),
            false,
            &app_tx,
            Mode::Detail,
        );

        let detail = state.detail.as_ref().expect("detail");
        let steer = detail.messages.last().expect("steer message");
        assert_eq!(steer.role, "user");
        assert_eq!(steer.turn_id.as_deref(), Some("turn-1"));
        assert_eq!(message_text(steer), "steer here");
    }

    #[test]
    fn queued_stream_event_assigns_one_pending_message_without_stealing_active_turn() {
        let mut state = TuiState::new(TuiInit {
            query: None,
            since: None,
            cwd: None,
            archived: false,
            limit: 50,
            sort: None,
            descending: true,
            prefs: TuiPrefs::default(),
        });
        state.detail = Some(detail_state(
            serde_json::json!({
                "thread": {"id": "t1", "name": "Thread", "status": {"type": "active"}},
                "turns": {"nextCursor": null, "backwardsCursor": null, "data": []}
            }),
            Some(serde_json::json!({"activeTurnId": "turn-1"})),
            "t1".to_string(),
            1,
            None,
        ));
        append_detail_message(&mut state, "t1", None, "user", None, "test 2");
        append_detail_message(&mut state, "t1", None, "user", None, "test 3");
        state.stream = Some(StreamState::new_with_id(
            7,
            "t1".to_string(),
            Some("turn-1".to_string()),
            StreamStatus::Running,
            true,
        ));

        handle_app_event(
            stream_event_for(
                7,
                serde_json::json!({
                    "type": "queued",
                    "threadId": "t1",
                    "turnId": "turn-2",
                    "status": "accepted",
                    "prompt": "test 2"
                }),
            ),
            &mut state,
        );

        assert_eq!(
            state
                .stream
                .as_ref()
                .and_then(|stream| stream.turn_id.as_deref()),
            Some("turn-1")
        );
        let detail = state.detail.as_ref().expect("detail");
        assert_eq!(detail.messages[0].turn_id.as_deref(), Some("turn-2"));
        assert_eq!(message_text(&detail.messages[0]), "test 2");
        assert_eq!(detail.messages[1].turn_id, None);
        assert_eq!(message_text(&detail.messages[1]), "test 3");
    }

    #[test]
    fn detail_refresh_preserves_queued_optimistic_messages_until_history_contains_them() {
        let mut state = TuiState::new(TuiInit {
            query: None,
            since: None,
            cwd: None,
            archived: false,
            limit: 50,
            sort: None,
            descending: true,
            prefs: TuiPrefs::default(),
        });
        state.detail = Some(detail_state(
            serde_json::json!({
                "thread": {"id": "t1", "name": "Thread", "status": {"type": "active"}},
                "turns": {"nextCursor": null, "backwardsCursor": null, "data": [
                    {"id": "turn-1", "items": [
                        {"id": "u1", "type": "userMessage", "content": [{"text": "test 1"}]}
                    ]}
                ]}
            }),
            Some(serde_json::json!({"activeTurnId": "turn-1"})),
            "t1".to_string(),
            1,
            None,
        ));
        append_detail_message(
            &mut state,
            "t1",
            Some("turn-2".to_string()),
            "user",
            None,
            "test 2",
        );

        let refreshed_without_turn = detail_state(
            serde_json::json!({
                "thread": {"id": "t1", "name": "Thread", "status": {"type": "active"}},
                "turns": {"nextCursor": null, "backwardsCursor": null, "data": [
                    {"id": "turn-1", "items": [
                        {"id": "u1", "type": "userMessage", "content": [{"text": "test 1"}]}
                    ]}
                ]}
            }),
            Some(serde_json::json!({"activeTurnId": "turn-1"})),
            "t1".to_string(),
            1,
            None,
        );
        state.replace_detail(1, refreshed_without_turn);
        let detail = state.detail.as_ref().expect("detail");
        assert!(detail.messages.iter().any(|message| {
            message.turn_id.as_deref() == Some("turn-2") && message_text(message) == "test 2"
        }));

        let refreshed_with_turn = detail_state(
            serde_json::json!({
                "thread": {"id": "t1", "name": "Thread", "status": {"type": "idle"}},
                "turns": {"nextCursor": null, "backwardsCursor": null, "data": [
                    {"id": "turn-1", "items": [
                        {"id": "u1", "type": "userMessage", "content": [{"text": "test 1"}]}
                    ]},
                    {"id": "turn-2", "items": [
                        {"id": "u2", "type": "userMessage", "content": [{"text": "test 2"}]},
                        {"id": "a2", "type": "agentMessage", "text": "done"}
                    ]}
                ]}
            }),
            None,
            "t1".to_string(),
            1,
            None,
        );
        state.replace_detail(1, refreshed_with_turn);
        let detail = state.detail.as_ref().expect("detail");
        let matching_user_messages = detail
            .messages
            .iter()
            .filter(|message| {
                message.role == "user"
                    && message.turn_id.as_deref() == Some("turn-2")
                    && message_text(message) == "test 2"
            })
            .count();
        assert_eq!(matching_user_messages, 1);
        assert!(detail.messages.iter().any(|message| {
            message.role == "assistant"
                && message.turn_id.as_deref() == Some("turn-2")
                && message_text(message) == "done"
        }));
    }

    #[test]
    fn completed_visible_stream_requests_follow_and_idle_clears_running_state() {
        let mut state = TuiState::new(TuiInit {
            query: None,
            since: None,
            cwd: None,
            archived: false,
            limit: 50,
            sort: None,
            descending: true,
            prefs: TuiPrefs::default(),
        });
        state.mode = Mode::Detail;
        state.detail = Some(detail_state(
            serde_json::json!({
                "thread": {"id": "t1", "name": "Thread", "status": {"type": "active"}},
                "turns": {"nextCursor": null, "backwardsCursor": null, "data": []}
            }),
            Some(serde_json::json!({"activeTurnId": "turn-1"})),
            "t1".to_string(),
            1,
            None,
        ));
        state.browser.rows = vec![test_thread_row("t1", "active")];
        state.stream = Some(StreamState::new_with_id(
            7,
            "t1".to_string(),
            Some("turn-1".to_string()),
            StreamStatus::Running,
            true,
        ));

        let finished = stream_finished(7, "t1", Some("turn-1"), StreamStatus::Completed);
        assert_eq!(
            stream_finish_follow_thread(&finished, &state),
            Some(("work".to_string(), "t1".to_string()))
        );
        handle_app_event(finished, &mut state);
        let stream_id = state.allocate_stream_id();
        state.stream = Some(StreamState::new_with_id(
            stream_id,
            "t1".to_string(),
            None,
            StreamStatus::Starting,
            true,
        ));
        assert!(matches!(
            state.stream.as_ref().expect("stream").status,
            StreamStatus::Starting
        ));

        handle_app_event(
            AppEvent::StreamIdle {
                server: "work".to_string(),
                stream_id,
                thread_id: "t1".to_string(),
            },
            &mut state,
        );

        assert_eq!(state.browser.rows[0].status, "idle");
        let detail = state.detail.as_ref().expect("detail");
        assert_eq!(detail.status, "idle");
        assert_eq!(detail.active_turn_id, None);
        assert!(matches!(
            state.stream.as_ref().expect("stream").status,
            StreamStatus::Completed
        ));
    }

    #[test]
    fn stream_delta_preserves_existing_active_turn_timestamp() {
        let mut state = TuiState::new(TuiInit {
            query: None,
            since: None,
            cwd: None,
            archived: false,
            limit: 50,
            sort: None,
            descending: true,
            prefs: TuiPrefs::default(),
        });
        state.detail = Some(detail_state(
            serde_json::json!({
                "thread": {"id": "t1", "name": "Thread", "status": {"type": "active"}},
                "turns": {"nextCursor": null, "backwardsCursor": null, "data": [
                    {"id": "turn-1", "startedAt": 1_700_000_000_i64, "items": [
                        {"id": "assistant-1", "type": "agentMessage", "text": "old text"}
                    ]}
                ]}
            }),
            None,
            "t1".to_string(),
            1,
            None,
        ));
        let old_timestamp = state
            .detail
            .as_ref()
            .expect("detail")
            .messages
            .first()
            .and_then(|message| message.timestamp.clone())
            .expect("historical timestamp");
        state.stream = Some(StreamState::new_with_id(
            1,
            "t1".to_string(),
            Some("turn-1".to_string()),
            StreamStatus::Running,
            true,
        ));
        state
            .stream
            .as_mut()
            .expect("stream")
            .assistant_items
            .push(StreamAssistantItem {
                turn_id: Some("turn-1".to_string()),
                item_id: Some("assistant-1".to_string()),
                text: "old text".to_string(),
            });

        handle_app_event(
            stream_event(serde_json::json!({
                "type": "delta",
                "threadId": "t1",
                "turnId": "turn-1",
                "itemId": "assistant-1",
                "delta": "\nlive update"
            })),
            &mut state,
        );

        let detail = state.detail.as_ref().expect("detail");
        let message = detail.messages.first().expect("message");
        assert_eq!(message.lines[0].text, "old text");
        assert_eq!(message.lines[1].text, "live update");
        assert_eq!(message.timestamp.as_deref(), Some(old_timestamp.as_str()));
    }

    #[test]
    fn same_item_delta_after_steer_renders_below_steer_message() {
        let mut state = TuiState::new(TuiInit {
            query: None,
            since: None,
            cwd: None,
            archived: false,
            limit: 50,
            sort: None,
            descending: true,
            prefs: TuiPrefs::default(),
        });
        state.detail = Some(detail_state(
            serde_json::json!({
                "thread": {"id": "t1", "name": "Thread", "status": {"type": "active"}},
                "turns": {"nextCursor": null, "backwardsCursor": null, "data": [
                    {"id": "turn-1", "items": [
                        {"id": "u1", "type": "userMessage", "content": [{"text": "start"}]},
                        {"id": "assistant-1", "type": "agentMessage", "text": "before steer"}
                    ]}
                ]}
            }),
            Some(serde_json::json!({"activeTurnId": "turn-1"})),
            "t1".to_string(),
            1,
            None,
        ));
        append_detail_message(
            &mut state,
            "t1",
            Some("turn-1".to_string()),
            "user",
            None,
            "steer here",
        );
        state.stream = Some(StreamState::new_with_id(
            7,
            "t1".to_string(),
            Some("turn-1".to_string()),
            StreamStatus::Running,
            true,
        ));
        state
            .stream
            .as_mut()
            .expect("stream")
            .assistant_items
            .push(StreamAssistantItem {
                turn_id: Some("turn-1".to_string()),
                item_id: Some("assistant-1".to_string()),
                text: "before steer".to_string(),
            });

        handle_app_event(
            stream_event_for(
                7,
                serde_json::json!({
                    "type": "delta",
                    "threadId": "t1",
                    "turnId": "turn-1",
                    "itemId": "assistant-1",
                    "delta": " after steer"
                }),
            ),
            &mut state,
        );

        let detail = state.detail.as_ref().expect("detail");
        let rendered = detail
            .messages
            .iter()
            .map(|message| format!("{}:{}", message.role, message_text(message)))
            .collect::<Vec<_>>();
        assert_eq!(
            rendered,
            vec![
                "user:start",
                "assistant:before steer",
                "user:steer here",
                "assistant:after steer"
            ]
        );
    }

    #[test]
    fn stream_deltas_for_distinct_assistant_items_render_separate_messages() {
        let mut state = TuiState::new(TuiInit {
            query: None,
            since: None,
            cwd: None,
            archived: false,
            limit: 50,
            sort: None,
            descending: true,
            prefs: TuiPrefs::default(),
        });
        state.detail = Some(detail_state(
            serde_json::json!({
                "thread": {"id": "t1", "name": "Thread", "status": {"type": "idle"}},
                "turns": {"nextCursor": null, "backwardsCursor": null, "data": []}
            }),
            None,
            "t1".to_string(),
            1,
            None,
        ));

        for event in [
            serde_json::json!({
                "type": "progress",
                "threadId": "t1",
                "turnId": "turn-1",
                "itemId": "assistant-1",
                "delta": "first"
            }),
            serde_json::json!({
                "type": "progress",
                "threadId": "t1",
                "turnId": "turn-1",
                "itemId": "assistant-1",
                "delta": " message"
            }),
            serde_json::json!({
                "type": "progress",
                "threadId": "t1",
                "turnId": "turn-1",
                "itemId": "assistant-2",
                "delta": "second"
            }),
            serde_json::json!({
                "type": "assistantMessage",
                "threadId": "t1",
                "turnId": "turn-1",
                "itemId": "assistant-2",
                "text": "second corrected"
            }),
        ] {
            handle_app_event(stream_event(event), &mut state);
        }

        let detail = state.detail.as_ref().expect("detail");
        assert_eq!(detail.messages.len(), 2);
        assert_eq!(detail.messages[0].item_id.as_deref(), Some("assistant-1"));
        assert_eq!(detail.messages[0].lines[0].text, "first message");
        assert_eq!(detail.messages[1].item_id.as_deref(), Some("assistant-2"));
        assert_eq!(detail.messages[1].lines[0].text, "second corrected");
    }

    #[test]
    fn terminal_response_adopts_provisional_stream_message() {
        let mut state = TuiState::new(TuiInit {
            query: None,
            since: None,
            cwd: None,
            archived: false,
            limit: 50,
            sort: None,
            descending: true,
            prefs: TuiPrefs::default(),
        });
        state.detail = Some(detail_state(
            serde_json::json!({
                "thread": {"id": "t1", "name": "Thread", "status": {"type": "idle"}},
                "turns": {"nextCursor": null, "backwardsCursor": null, "data": []}
            }),
            None,
            "t1".to_string(),
            1,
            None,
        ));

        handle_app_event(
            stream_event(serde_json::json!({
                "type": "progress",
                "threadId": "t1",
                "turnId": "turn-1",
                "delta": "Verification passed. I'm"
            })),
            &mut state,
        );
        handle_app_event(
            stream_event(serde_json::json!({
                "server": "work",
                "threadId": "t1",
                "turnId": "turn-1",
                "status": "completed",
                "assistantResponses": [
                    {
                        "itemId": "assistant-1",
                        "text": "Verification passed. I'm installing the rebuilt binary."
                    }
                ],
                "finalAssistantText": "Verification passed. I'm installing the rebuilt binary."
            })),
            &mut state,
        );

        let detail = state.detail.as_ref().expect("detail");
        assert_eq!(detail.messages.len(), 1);
        assert_eq!(detail.messages[0].item_id.as_deref(), Some("assistant-1"));
        assert_eq!(detail.messages[0].timestamp, None);
        assert_eq!(
            detail.messages[0].lines[0].text,
            "Verification passed. I'm installing the rebuilt binary."
        );
    }

    #[test]
    fn terminal_response_does_not_leave_punctuation_fragment() {
        let mut state = TuiState::new(TuiInit {
            query: None,
            since: None,
            cwd: None,
            archived: false,
            limit: 50,
            sort: None,
            descending: true,
            prefs: TuiPrefs::default(),
        });
        state.detail = Some(detail_state(
            serde_json::json!({
                "thread": {"id": "t1", "name": "Thread", "status": {"type": "idle"}},
                "turns": {"nextCursor": null, "backwardsCursor": null, "data": []}
            }),
            None,
            "t1".to_string(),
            1,
            None,
        ));

        handle_app_event(
            stream_event(serde_json::json!({
                "type": "progress",
                "threadId": "t1",
                "turnId": "turn-1",
                "delta": "."
            })),
            &mut state,
        );
        handle_app_event(
            stream_event(serde_json::json!({
                "server": "work",
                "threadId": "t1",
                "turnId": "turn-1",
                "status": "completed",
                "assistantResponses": [
                    {"itemId": "assistant-1", "text": "Done."}
                ],
                "finalAssistantText": "Done."
            })),
            &mut state,
        );

        let detail = state.detail.as_ref().expect("detail");
        assert_eq!(detail.messages.len(), 1);
        assert_eq!(detail.messages[0].item_id.as_deref(), Some("assistant-1"));
        assert_eq!(detail.messages[0].timestamp, None);
        assert_eq!(detail.messages[0].lines[0].text, "Done.");
    }

    #[test]
    fn streaming_updates_follow_bottom_when_detail_was_at_bottom() {
        let mut state = TuiState::new(TuiInit {
            query: None,
            since: None,
            cwd: None,
            archived: false,
            limit: 50,
            sort: None,
            descending: true,
            prefs: TuiPrefs::default(),
        });
        state.mode = Mode::Detail;
        state.detail = Some(detail_state(
            serde_json::json!({
                "thread": {"id": "t1", "name": "Thread", "status": {"type": "idle"}},
                "turns": {"nextCursor": null, "backwardsCursor": null, "data": [
                    {"id": "turn-1", "items": [
                        {"id": "a", "type": "userMessage", "content": [{"type": "input_text", "text": "one\ntwo\nthree\nfour"}]}
                    ]}
                ]}
            }),
            None,
            "t1".to_string(),
            1,
            None,
        ));
        let detail = state.detail.as_mut().expect("detail");
        detail.set_viewport_size(4, 100);
        detail.scroll = detail.bottom_scroll_position();
        let before = detail.scroll;

        handle_app_event(
            stream_event(serde_json::json!({
                "type": "delta",
                "threadId": "t1",
                "turnId": "turn-2",
                "delta": "assistant\nresponse\nkeeps\ngrowing"
            })),
            &mut state,
        );

        let detail = state.detail.as_ref().expect("detail");
        assert_eq!(detail.scroll, detail.bottom_scroll_position());
        assert!(detail.scroll > before);
    }

    #[test]
    fn stream_finish_and_detach_update_local_state() {
        let mut state = TuiState::new(TuiInit {
            query: None,
            since: None,
            cwd: None,
            archived: false,
            limit: 50,
            sort: None,
            descending: true,
            prefs: TuiPrefs::default(),
        });
        state.browser.rows = vec![test_thread_row("t1", "notLoaded")];
        let (control_tx, mut control_rx) = mpsc::unbounded_channel();
        state.stream_control = Some(control_tx);
        state.stream = Some(StreamState::new_with_id(
            0,
            "t1".to_string(),
            Some("turn-1".to_string()),
            StreamStatus::Running,
            true,
        ));

        detach_stream(&mut state);
        let stream = state.stream.as_ref().expect("stream");
        assert_eq!(stream.status, StreamStatus::Detached);
        assert!(stream.detached);
        assert!(state.stream_control.is_none());
        assert!(matches!(control_rx.try_recv(), Ok(TurnControl::Detach)));

        handle_app_event(
            stream_finished(0, "t1", Some("turn-1"), StreamStatus::Completed),
            &mut state,
        );
        assert_eq!(
            state.stream.as_ref().expect("stream").status,
            StreamStatus::Completed
        );
        assert_eq!(state.browser.rows[0].status, "idle");
        assert_eq!(
            state.browser.rows[0].raw["status"]["type"].as_str(),
            Some("idle")
        );
    }

    #[test]
    fn stale_stream_events_do_not_poison_next_browser_send() {
        let mut state = TuiState::new(TuiInit {
            query: None,
            since: None,
            cwd: None,
            archived: false,
            limit: 50,
            sort: None,
            descending: true,
            prefs: TuiPrefs::default(),
        });
        state.browser.rows = vec![test_thread_row("t1", "idle"), test_thread_row("t2", "idle")];
        let first_stream_id = state.allocate_stream_id();
        state.stream = Some(StreamState::new_with_id(
            first_stream_id,
            "t1".to_string(),
            Some("turn-1".to_string()),
            StreamStatus::Running,
            false,
        ));

        handle_app_event(
            stream_finished(
                first_stream_id,
                "t1",
                Some("turn-1"),
                StreamStatus::Completed,
            ),
            &mut state,
        );
        state.move_selection(1);
        detach_stream_if_browser_selection_changed(
            &mut state,
            Some(("work".to_string(), "t1".to_string())),
        );
        assert!(state.stream.is_none());

        state.move_selection(-1);
        let second_stream_id = state.allocate_stream_id();
        state.stream = Some(StreamState::new_with_id(
            second_stream_id,
            "t1".to_string(),
            Some("turn-2".to_string()),
            StreamStatus::Starting,
            false,
        ));
        handle_app_event(
            stream_failed(first_stream_id, "t1", Some("turn-1"), "late old failure"),
            &mut state,
        );
        handle_app_event(
            stream_event_for(
                first_stream_id,
                serde_json::json!({
                    "type": "delta",
                    "threadId": "t1",
                    "turnId": "turn-1",
                    "delta": "stale"
                }),
            ),
            &mut state,
        );

        let stream = state.stream.as_ref().expect("stream");
        assert_eq!(stream.id, second_stream_id);
        assert_eq!(stream.status, StreamStatus::Starting);
        assert!(stream.assistant_items.is_empty());
    }

    #[test]
    fn unlink_detail_session_detaches_and_clears_local_thread_state() {
        let mut state = TuiState::new(TuiInit {
            query: None,
            since: None,
            cwd: None,
            archived: false,
            limit: 50,
            sort: None,
            descending: true,
            prefs: TuiPrefs::default(),
        });
        state.mode = Mode::Detail;
        state.detail = Some(detail_state(
            serde_json::json!({
                "thread": {"id": "t1", "name": "Thread", "status": {"type": "idle"}},
                "turns": {"nextCursor": null, "backwardsCursor": null, "data": []}
            }),
            None,
            "t1".to_string(),
            1,
            None,
        ));
        let (control_tx, mut control_rx) = mpsc::unbounded_channel();
        state.stream_control = Some(control_tx);
        state.stream = Some(StreamState::new(
            "t1".to_string(),
            Some("turn-1".to_string()),
            StreamStatus::Running,
            true,
        ));

        unlink_detail_session(&mut state);

        assert!(matches!(state.mode, Mode::Browser));
        assert!(state.detail.is_none());
        assert!(state.stream.is_none());
        assert!(state.stream_control.is_none());
        assert!(matches!(control_rx.try_recv(), Ok(TurnControl::Detach)));
    }

    #[test]
    fn detail_follow_refresh_tracks_open_idle_detail_only() {
        let mut state = TuiState::new(TuiInit {
            query: None,
            since: None,
            cwd: None,
            archived: false,
            limit: 50,
            sort: None,
            descending: true,
            prefs: TuiPrefs::default(),
        });
        state.mode = Mode::Detail;
        state.detail = Some(detail_state(
            serde_json::json!({
                "thread": {"id": "t1", "name": "Thread", "status": {"type": "idle"}},
                "turns": {"nextCursor": null, "backwardsCursor": null, "data": []}
            }),
            None,
            "t1".to_string(),
            1,
            None,
        ));
        state.detail.as_mut().unwrap().last_refresh_at =
            Some(std::time::Instant::now() - Duration::from_secs(DETAIL_FOLLOW_REFRESH_SECS));

        assert_eq!(
            detail_follow_refresh_thread(&state),
            Some(("work".to_string(), "t1".to_string()))
        );

        state.stream = Some(StreamState::new(
            "t1".to_string(),
            Some("turn-1".to_string()),
            StreamStatus::Running,
            true,
        ));
        assert!(detail_follow_refresh_thread(&state).is_none());
    }

    #[tokio::test]
    async fn detail_refresh_preserves_existing_transcript_while_loading() {
        let mut state = TuiState::new(TuiInit {
            query: None,
            since: None,
            cwd: None,
            archived: false,
            limit: 50,
            sort: None,
            descending: true,
            prefs: TuiPrefs::default(),
        });
        state.mode = Mode::Detail;
        state.detail = Some(detail_state(
            serde_json::json!({
                "thread": {"id": "t1", "name": "Thread", "status": {"type": "idle"}},
                "turns": {"nextCursor": null, "backwardsCursor": null, "data": [
                    {"id": "turn-1", "items": [
                        {"id": "a", "type": "agentMessage", "text": "existing"}
                    ]}
                ]}
            }),
            None,
            "t1".to_string(),
            3,
            Some("current".to_string()),
        ));
        state.detail.as_mut().unwrap().scroll = 4;
        let (fetch_tx, mut fetch_rx) = mpsc::channel(1);

        schedule_detail_refresh_for_server(
            &mut state,
            &fetch_tx,
            "work".to_string(),
            "t1".to_string(),
        )
        .await
        .unwrap();

        let detail = state.detail.as_ref().expect("detail");
        assert!(detail.loading);
        assert_eq!(detail.epoch, 4);
        assert_eq!(detail.scroll, 4);
        assert_eq!(detail.messages[0].lines[0].text, "existing");

        let request = fetch_rx.recv().await.expect("request");
        let FetchRequest::Detail {
            epoch,
            thread_id,
            cursor,
            limit,
            page_direction,
            ..
        } = request
        else {
            panic!("expected detail request");
        };
        assert_eq!(epoch, 4);
        assert_eq!(thread_id, "t1");
        assert_eq!(cursor.as_deref(), Some("current"));
        assert_eq!(limit, DETAIL_TURN_LIMIT);
        assert_eq!(page_direction, DetailPageDirection::Replace);
    }

    #[test]
    fn merge_browser_fetch_outcomes_sorts_all_server_rows_by_updated_then_server() {
        let mut old_main = test_thread_row_with_updated("old", "main", 10);
        let mut new_work = test_thread_row_with_updated("new", "work", 30);
        let mut new_main = test_thread_row_with_updated("new", "main", 30);
        old_main.title = "old main".to_string();
        new_work.title = "new work".to_string();
        new_main.title = "new main".to_string();

        let (rows, next, backwards, warning) = merge_browser_fetch_outcomes(
            vec![
                BrowserFetchOutcome {
                    server: "work".to_string(),
                    rows: Ok((vec![old_main, new_work], Some("ignored".to_string()), None)),
                },
                BrowserFetchOutcome {
                    server: "main".to_string(),
                    rows: Ok((vec![new_main], Some("also-ignored".to_string()), None)),
                },
            ],
            true,
        )
        .unwrap();

        assert_eq!(
            rows.iter()
                .map(|row| format!("{}/{}", row.server, row.id))
                .collect::<Vec<_>>(),
            vec!["main/new", "work/new", "main/old"]
        );
        assert_eq!(next, None);
        assert_eq!(backwards, None);
        assert_eq!(warning, None);
    }

    #[test]
    fn merge_browser_fetch_outcomes_keeps_empty_success_with_partial_failure() {
        let (rows, _, _, warning) = merge_browser_fetch_outcomes(
            vec![
                BrowserFetchOutcome {
                    server: "main".to_string(),
                    rows: Ok((Vec::new(), None, None)),
                },
                BrowserFetchOutcome {
                    server: "slow".to_string(),
                    rows: Err("timeout".to_string()),
                },
            ],
            true,
        )
        .unwrap();

        assert!(rows.is_empty());
        assert_eq!(
            warning.as_deref(),
            Some("some servers failed: slow: timeout")
        );
    }

    #[test]
    fn merge_browser_fetch_outcomes_errors_when_every_server_fails() {
        let error = merge_browser_fetch_outcomes(
            vec![
                BrowserFetchOutcome {
                    server: "main".to_string(),
                    rows: Err("down".to_string()),
                },
                BrowserFetchOutcome {
                    server: "work".to_string(),
                    rows: Err("timeout".to_string()),
                },
            ],
            true,
        )
        .unwrap_err();

        assert_eq!(error.to_string(), "main: down; work: timeout");
    }

    #[test]
    fn merge_browser_fetch_outcomes_preserves_single_server_cursors() {
        let (rows, next, backwards, warning) = merge_browser_fetch_outcomes(
            vec![BrowserFetchOutcome {
                server: "main".to_string(),
                rows: Ok((
                    vec![test_thread_row_with_updated("t1", "main", 10)],
                    Some("older".to_string()),
                    Some("newer".to_string()),
                )),
            }],
            false,
        )
        .unwrap();

        assert_eq!(rows.len(), 1);
        assert_eq!(next.as_deref(), Some("older"));
        assert_eq!(backwards.as_deref(), Some("newer"));
        assert_eq!(warning, None);
    }

    #[tokio::test]
    async fn detail_gg_loads_older_pages_before_jumping_to_real_start() {
        let _target = test_target();
        let (fetch_tx, mut fetch_rx) = mpsc::channel(1);
        let (app_tx, _app_rx) = mpsc::unbounded_channel();
        let mut state = TuiState::new(TuiInit {
            query: None,
            since: None,
            cwd: None,
            archived: false,
            limit: 50,
            sort: None,
            descending: true,
            prefs: TuiPrefs::default(),
        });
        state.mode = Mode::Detail;
        state.detail = Some(detail_state(
            serde_json::json!({
                "thread": {"id": "t1", "name": "Thread", "status": {"type": "idle"}},
                "turns": {"nextCursor": "older", "backwardsCursor": "newer", "data": [
                    {"id": "turn-2", "items": [
                        {"id": "middle", "type": "agentMessage", "text": "middle"}
                    ]}
                ]}
            }),
            None,
            "t1".to_string(),
            1,
            None,
        ));

        handle_terminal_event(
            Event::Key(KeyEvent::new(KeyCode::Char('g'), KeyModifiers::empty())),
            &mut state,
            &test_targets(),
            false,
            &fetch_tx,
            &app_tx,
        )
        .await
        .unwrap();
        assert!(fetch_rx.try_recv().is_err());
        assert!(state.pending_goto_top);

        handle_terminal_event(
            Event::Key(KeyEvent::new(KeyCode::Char('g'), KeyModifiers::empty())),
            &mut state,
            &test_targets(),
            false,
            &fetch_tx,
            &app_tx,
        )
        .await
        .unwrap();

        let FetchRequest::Detail {
            epoch,
            thread_id,
            cursor,
            limit,
            page_direction,
            ..
        } = fetch_rx.recv().await.expect("detail request")
        else {
            panic!("expected detail request");
        };
        assert_eq!(thread_id, "t1");
        assert_eq!(cursor.as_deref(), Some("older"));
        assert_eq!(limit, DETAIL_JUMP_TURN_LIMIT);
        assert_eq!(page_direction, DetailPageDirection::Older);
        assert_eq!(state.pending_detail_jump, Some(DetailJump::Start));

        handle_app_event(
            AppEvent::DetailLoaded {
                epoch,
                detail: Box::new(detail_state(
                    serde_json::json!({
                        "thread": {"id": "t1", "name": "Thread", "status": {"type": "idle"}},
                        "turns": {"nextCursor": null, "backwardsCursor": "newer", "data": [
                            {"id": "turn-1", "items": [
                                {"id": "old", "type": "agentMessage", "text": "old"}
                            ]},
                            {"id": "turn-2", "items": [
                                {"id": "middle", "type": "agentMessage", "text": "middle"}
                            ]}
                        ]}
                    }),
                    None,
                    "t1".to_string(),
                    epoch,
                    Some("older".to_string()),
                )),
                page_direction: DetailPageDirection::Older,
            },
            &mut state,
        );
        schedule_pending_detail_jump(&mut state, &fetch_tx)
            .await
            .unwrap();
        assert_eq!(state.pending_detail_jump, None);
        assert_eq!(state.detail.as_ref().unwrap().scroll, 0);
        assert!(fetch_rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn scrolling_up_at_detail_top_schedules_older_page() {
        let _target = test_target();
        let (fetch_tx, mut fetch_rx) = mpsc::channel(1);
        let (app_tx, _app_rx) = mpsc::unbounded_channel();
        let mut state = TuiState::new(TuiInit {
            query: None,
            since: None,
            cwd: None,
            archived: false,
            limit: 50,
            sort: None,
            descending: true,
            prefs: TuiPrefs::default(),
        });
        state.mode = Mode::Detail;
        state.detail = Some(detail_state(
            serde_json::json!({
                "thread": {"id": "t1", "name": "Thread", "status": {"type": "idle"}},
                "turns": {"nextCursor": "older", "backwardsCursor": null, "data": [
                    {"id": "turn-2", "items": [
                        {"id": "middle", "type": "agentMessage", "text": "middle"}
                    ]}
                ]}
            }),
            None,
            "t1".to_string(),
            1,
            None,
        ));

        handle_terminal_event(
            Event::Key(KeyEvent::new(KeyCode::Up, KeyModifiers::empty())),
            &mut state,
            &test_targets(),
            false,
            &fetch_tx,
            &app_tx,
        )
        .await
        .unwrap();

        let FetchRequest::Detail {
            thread_id,
            cursor,
            limit,
            page_direction,
            ..
        } = fetch_rx.recv().await.expect("detail request")
        else {
            panic!("expected detail request");
        };
        assert_eq!(thread_id, "t1");
        assert_eq!(cursor.as_deref(), Some("older"));
        assert_eq!(limit, DETAIL_TURN_LIMIT);
        assert_eq!(page_direction, DetailPageDirection::Older);
    }

    #[tokio::test]
    async fn shifted_lowercase_g_jumps_to_real_end() {
        let _target = test_target();
        let (fetch_tx, mut fetch_rx) = mpsc::channel(1);
        let (app_tx, _app_rx) = mpsc::unbounded_channel();
        let mut state = TuiState::new(TuiInit {
            query: None,
            since: None,
            cwd: None,
            archived: false,
            limit: 50,
            sort: None,
            descending: true,
            prefs: TuiPrefs::default(),
        });
        state.mode = Mode::Detail;
        state.detail = Some(detail_state(
            serde_json::json!({
                "thread": {"id": "t1", "name": "Thread", "status": {"type": "idle"}},
                "turns": {"nextCursor": null, "backwardsCursor": "newer", "data": [
                    {"id": "turn-1", "items": [
                        {"id": "old", "type": "agentMessage", "text": "old"}
                    ]}
                ]}
            }),
            None,
            "t1".to_string(),
            1,
            None,
        ));

        handle_terminal_event(
            Event::Key(KeyEvent::new(KeyCode::Char('g'), KeyModifiers::SHIFT)),
            &mut state,
            &test_targets(),
            false,
            &fetch_tx,
            &app_tx,
        )
        .await
        .unwrap();

        let FetchRequest::Detail {
            thread_id,
            cursor,
            limit,
            page_direction,
            ..
        } = fetch_rx.recv().await.expect("detail request")
        else {
            panic!("expected detail request");
        };
        assert_eq!(thread_id, "t1");
        assert_eq!(cursor.as_deref(), Some("newer"));
        assert_eq!(limit, DETAIL_JUMP_TURN_LIMIT);
        assert_eq!(page_direction, DetailPageDirection::Newer);
        assert_eq!(state.pending_detail_jump, Some(DetailJump::End));
    }

    #[tokio::test]
    async fn detail_jump_stops_on_non_advancing_cursor() {
        let (fetch_tx, mut fetch_rx) = mpsc::channel(1);
        let mut state = TuiState::new(TuiInit {
            query: None,
            since: None,
            cwd: None,
            archived: false,
            limit: 50,
            sort: None,
            descending: true,
            prefs: TuiPrefs::default(),
        });
        state.mode = Mode::Detail;
        state.pending_detail_jump = Some(DetailJump::Start);
        state.detail = Some(detail_state(
            serde_json::json!({
                "thread": {"id": "t1", "name": "Thread", "status": {"type": "idle"}},
                "turns": {"nextCursor": "same", "backwardsCursor": null, "data": [
                    {"id": "turn-1", "items": [
                        {"id": "a", "type": "agentMessage", "text": "one"}
                    ]}
                ]}
            }),
            None,
            "t1".to_string(),
            1,
            Some("same".to_string()),
        ));

        schedule_pending_detail_jump(&mut state, &fetch_tx)
            .await
            .unwrap();

        assert_eq!(state.pending_detail_jump, None);
        assert!(state.notice.as_ref().unwrap().message.contains("cursor"));
        assert!(fetch_rx.try_recv().is_err());
    }

    #[test]
    fn initial_detail_load_scrolls_to_bottom_after_loaded() {
        let mut state = TuiState::new(TuiInit {
            query: None,
            since: None,
            cwd: None,
            archived: false,
            limit: 50,
            sort: None,
            descending: true,
            prefs: TuiPrefs::default(),
        });
        state.mode = Mode::Detail;
        state.detail = Some(DetailState {
            server: "work".to_string(),
            thread_id: "t1".to_string(),
            title: "t1".to_string(),
            status: "loading".to_string(),
            annotation: None,
            messages: Vec::new(),
            scroll: u16::MAX,
            search_query: String::new(),
            matches: Vec::new(),
            match_index: 0,
            next_cursor: None,
            backwards_cursor: None,
            current_cursor: None,
            active_turn_id: None,
            loading: true,
            epoch: 1,
            last_refresh_at: None,
            viewport_height: None,
            viewport_width: None,
            last_error: None,
        });
        let loaded = detail_state(
            serde_json::json!({
                "thread": {"id": "t1", "name": "Thread", "status": {"type": "idle"}},
                "turns": {"nextCursor": null, "backwardsCursor": null, "data": [
                    {"id": "turn-1", "items": [
                        {"id": "a", "type": "agentMessage", "text": "one\ntwo\nthree\nfour"}
                    ]}
                ]}
            }),
            None,
            "t1".to_string(),
            1,
            None,
        );
        state.detail.as_mut().unwrap().set_viewport_size(4, 100);

        state.replace_detail(1, loaded);

        let detail = state.detail.as_ref().expect("detail");
        assert_eq!(detail.scroll as usize, detail.transcript_line_count() - 4);
        let bottom = detail.scroll;
        scroll_detail(&mut state, -1);
        assert_eq!(state.detail.as_ref().unwrap().scroll, bottom - 1);
    }

    #[test]
    fn detail_refresh_follows_bottom_when_new_messages_arrive() {
        let mut state = TuiState::new(TuiInit {
            query: None,
            since: None,
            cwd: None,
            archived: false,
            limit: 50,
            sort: None,
            descending: true,
            prefs: TuiPrefs::default(),
        });
        state.mode = Mode::Detail;
        state.detail = Some(detail_state(
            serde_json::json!({
                "thread": {"id": "t1", "name": "Thread", "status": {"type": "idle"}},
                "turns": {"nextCursor": null, "backwardsCursor": null, "data": [
                    {"id": "turn-1", "items": [
                        {"id": "a", "type": "agentMessage", "text": "one\ntwo\nthree\nfour"}
                    ]}
                ]}
            }),
            None,
            "t1".to_string(),
            1,
            None,
        ));
        let detail = state.detail.as_mut().expect("detail");
        detail.set_viewport_size(4, 100);
        detail.scroll = detail.bottom_scroll_position();
        let before = detail.scroll;
        let refreshed = detail_state(
            serde_json::json!({
                "thread": {"id": "t1", "name": "Thread", "status": {"type": "idle"}},
                "turns": {"nextCursor": null, "backwardsCursor": null, "data": [
                    {"id": "turn-1", "items": [
                        {"id": "a", "type": "agentMessage", "text": "one\ntwo\nthree\nfour"},
                        {"id": "b", "type": "agentMessage", "text": "five\nsix\nseven\neight"}
                    ]}
                ]}
            }),
            None,
            "t1".to_string(),
            1,
            None,
        );

        state.replace_detail(1, refreshed);

        let detail = state.detail.as_ref().expect("detail");
        assert_eq!(detail.scroll, detail.bottom_scroll_position());
        assert!(detail.scroll > before);
    }

    #[test]
    fn detail_refresh_preserves_compose_overlay() {
        let mut state = TuiState::new(TuiInit {
            query: None,
            since: None,
            cwd: None,
            archived: false,
            limit: 50,
            sort: None,
            descending: true,
            prefs: TuiPrefs::default(),
        });
        state.mode = Mode::Compose(ComposeState {
            target: ComposeTarget::NewTurn {
                server: "work".to_string(),
                thread_id: "t1".to_string(),
            },
            text: "draft in progress".to_string(),
            send_mode: SendMode::Stream,
            return_to_detail: true,
        });
        state.detail = Some(detail_state(
            serde_json::json!({
                "thread": {"id": "t1", "name": "Thread", "status": {"type": "idle"}},
                "turns": {"nextCursor": null, "backwardsCursor": null, "data": []}
            }),
            None,
            "t1".to_string(),
            1,
            None,
        ));
        let refreshed = detail_state(
            serde_json::json!({
                "thread": {"id": "t1", "name": "Thread", "status": {"type": "idle"}},
                "turns": {"nextCursor": null, "backwardsCursor": null, "data": [
                    {"id": "turn-1", "items": [
                        {"id": "a", "type": "agentMessage", "text": "new update"}
                    ]}
                ]}
            }),
            None,
            "t1".to_string(),
            1,
            None,
        );

        state.replace_detail(1, refreshed);

        let Mode::Compose(compose) = &state.mode else {
            panic!("expected compose overlay");
        };
        assert_eq!(compose.text, "draft in progress");
        assert_eq!(state.detail.as_ref().unwrap().messages.len(), 1);
    }

    #[test]
    fn detail_refresh_preserves_annotation_overlay() {
        let mut state = TuiState::new(TuiInit {
            query: None,
            since: None,
            cwd: None,
            archived: false,
            limit: 50,
            sort: None,
            descending: true,
            prefs: TuiPrefs::default(),
        });
        state.mode = Mode::AnnotationInput {
            server: "work".to_string(),
            thread_id: "t1".to_string(),
            draft: "annotation draft".to_string(),
            return_to_detail: true,
        };
        state.detail = Some(detail_state(
            serde_json::json!({
                "thread": {"id": "t1", "name": "Thread", "status": {"type": "idle"}},
                "turns": {"nextCursor": null, "backwardsCursor": null, "data": []}
            }),
            None,
            "t1".to_string(),
            1,
            None,
        ));
        let refreshed = detail_state(
            serde_json::json!({
                "thread": {"id": "t1", "name": "Thread", "status": {"type": "idle"}},
                "turns": {"nextCursor": null, "backwardsCursor": null, "data": [
                    {"id": "turn-1", "items": [
                        {"id": "a", "type": "agentMessage", "text": "new update"}
                    ]}
                ]}
            }),
            None,
            "t1".to_string(),
            1,
            None,
        );

        state.replace_detail(1, refreshed);

        let Mode::AnnotationInput { draft, .. } = &state.mode else {
            panic!("expected annotation overlay");
        };
        assert_eq!(draft, "annotation draft");
        assert_eq!(state.detail.as_ref().unwrap().messages.len(), 1);
    }

    #[test]
    fn detail_message_action_defaults_to_steer_when_active() {
        let mut state = TuiState::new(TuiInit {
            query: None,
            since: None,
            cwd: None,
            archived: false,
            limit: 50,
            sort: None,
            descending: true,
            prefs: TuiPrefs::default(),
        });
        state.mode = Mode::Detail;
        state.detail = Some(detail_state(
            serde_json::json!({
                "thread": {"id": "t1", "name": "Thread", "status": {"type": "idle"}},
                "turns": {"nextCursor": null, "backwardsCursor": null, "data": []}
            }),
            Some(serde_json::json!({"activeTurnId": "turn-1"})),
            "t1".to_string(),
            1,
            None,
        ));

        open_message_action(&mut state, "work".to_string(), "t1".to_string());

        assert!(matches!(
            state.mode,
            Mode::Compose(ComposeState {
                target: ComposeTarget::Steer {
                    ref thread_id,
                    ref turn_id,
                    ..
                },
                send_mode: SendMode::Stream,
                return_to_detail: true,
                ..
            }) if thread_id == "t1" && turn_id == "turn-1"
        ));

        state.mode = Mode::Detail;
        state.detail.as_mut().unwrap().active_turn_id = None;
        open_message_action(&mut state, "work".to_string(), "t1".to_string());

        assert!(matches!(
            state.mode,
            Mode::Compose(ComposeState {
                target: ComposeTarget::NewTurn { ref thread_id, .. },
                send_mode: SendMode::Stream,
                return_to_detail: true,
                ..
            }) if thread_id == "t1"
        ));
    }

    #[test]
    fn attached_resume_snapshot_seeds_active_turn_text_for_future_deltas() {
        let mut state = TuiState::new(TuiInit {
            query: None,
            since: None,
            cwd: None,
            archived: false,
            limit: 50,
            sort: None,
            descending: true,
            prefs: TuiPrefs::default(),
        });
        state.mode = Mode::Detail;
        state.detail = Some(detail_state(
            serde_json::json!({
                "thread": {"id": "t1", "name": "Thread", "status": {"type": "idle"}},
                "turns": {"nextCursor": null, "backwardsCursor": null, "data": []}
            }),
            Some(serde_json::json!({"activeTurnId": "turn-1"})),
            "t1".to_string(),
            1,
            None,
        ));

        handle_app_event(
            stream_event(serde_json::json!({
                "type": "attached",
                "threadId": "t1",
                "turnId": "turn-1",
                "status": "attached",
                "thread": {
                    "id": "t1",
                    "name": "Thread",
                    "status": {"type": "active"},
                    "turns": [
                        {
                            "id": "turn-1",
                            "status": "inProgress",
                            "startedAt": 1_700_000_000,
                            "completedAt": null,
                            "items": [
                                {
                                    "id": "user-1",
                                    "type": "userMessage",
                                    "content": [{"type": "input_text", "text": "continue"}]
                                },
                                {
                                    "id": "assistant-1",
                                    "type": "agentMessage",
                                    "text": "Already streamed"
                                }
                            ],
                            "itemsView": "full"
                        }
                    ]
                }
            })),
            &mut state,
        );

        let detail = state.detail.as_ref().expect("detail");
        assert_eq!(detail.messages.len(), 2);
        assert_eq!(detail.messages[1].item_id.as_deref(), Some("assistant-1"));
        assert_eq!(detail.messages[1].lines[0].text, "Already streamed");

        handle_app_event(
            stream_event(serde_json::json!({
                "type": "progress",
                "threadId": "t1",
                "turnId": "turn-1",
                "itemId": "assistant-1",
                "delta": " plus new"
            })),
            &mut state,
        );

        let detail = state.detail.as_ref().expect("detail");
        assert_eq!(detail.messages.len(), 2);
        assert_eq!(
            detail.messages[1].lines[0].text,
            "Already streamed plus new"
        );
    }

    #[test]
    fn anonymous_delta_after_snapshot_updates_existing_assistant_message() {
        let mut state = TuiState::new(TuiInit {
            query: None,
            since: None,
            cwd: None,
            archived: false,
            limit: 50,
            sort: None,
            descending: true,
            prefs: TuiPrefs::default(),
        });
        state.mode = Mode::Detail;
        state.detail = Some(detail_state(
            serde_json::json!({
                "thread": {"id": "t1", "name": "Thread", "status": {"type": "idle"}},
                "turns": {"nextCursor": null, "backwardsCursor": null, "data": []}
            }),
            Some(serde_json::json!({"activeTurnId": "turn-1"})),
            "t1".to_string(),
            1,
            None,
        ));

        handle_app_event(
            stream_event(serde_json::json!({
                "type": "attached",
                "threadId": "t1",
                "turnId": "turn-1",
                "status": "attached",
                "thread": {
                    "id": "t1",
                    "name": "Thread",
                    "status": {"type": "active"},
                    "turns": [
                        {
                            "id": "turn-1",
                            "status": "inProgress",
                            "startedAt": 1_700_000_000,
                            "items": [
                                {
                                    "id": "assistant-1",
                                    "type": "agentMessage",
                                    "text": "Already streamed"
                                }
                            ],
                            "itemsView": "full"
                        }
                    ]
                }
            })),
            &mut state,
        );
        handle_app_event(
            stream_event(serde_json::json!({
                "type": "progress",
                "threadId": "t1",
                "turnId": "turn-1",
                "delta": " plus anonymous"
            })),
            &mut state,
        );

        let detail = state.detail.as_ref().expect("detail");
        assert_eq!(detail.messages.len(), 1);
        assert_eq!(detail.messages[0].item_id.as_deref(), Some("assistant-1"));
        assert_eq!(
            detail.messages[0].lines[0].text,
            "Already streamed plus anonymous"
        );
    }

    #[test]
    fn poll_text_with_alias_updates_live_streamed_message() {
        let mut state = TuiState::new(TuiInit {
            query: None,
            since: None,
            cwd: None,
            archived: false,
            limit: 50,
            sort: None,
            descending: true,
            prefs: TuiPrefs::default(),
        });
        state.mode = Mode::Detail;
        state.detail = Some(detail_state(
            serde_json::json!({
                "thread": {"id": "t1", "name": "Thread", "status": {"type": "active"}},
                "turns": {"nextCursor": null, "backwardsCursor": null, "data": [
                    {"id": "turn-1", "items": [
                        {"id": "item-1", "type": "userMessage", "content": [{"text": "go"}]}
                    ]}
                ]}
            }),
            Some(serde_json::json!({"activeTurnId": "turn-1"})),
            "t1".to_string(),
            1,
            None,
        ));
        state.stream = Some(StreamState::new_with_id(
            3,
            "t1".to_string(),
            Some("turn-1".to_string()),
            StreamStatus::Running,
            true,
        ));

        // The item streams live under its notification id.
        for delta in ["Hello", " world"] {
            handle_app_event(
                stream_event_for(
                    3,
                    serde_json::json!({
                        "type": "progress",
                        "threadId": "t1",
                        "turnId": "turn-1",
                        "itemId": "msg_a",
                        "delta": delta
                    }),
                ),
                &mut state,
            );
        }
        // The poll re-reads the turn, which lists the item under its
        // persisted id; the turn wait layer emits it under the canonical
        // live id plus aliases.
        handle_app_event(
            stream_event_for(
                3,
                serde_json::json!({
                    "type": "progress",
                    "threadId": "t1",
                    "turnId": "turn-1",
                    "itemId": "msg_a",
                    "itemAliases": ["msg_a", "item-2"],
                    "text": "Hello world",
                    "source": "poll"
                }),
            ),
            &mut state,
        );

        let detail = state.detail.as_ref().expect("detail");
        let assistant_messages: Vec<&MessageBlock> = detail
            .messages
            .iter()
            .filter(|message| message.role == "assistant")
            .collect();
        assert_eq!(assistant_messages.len(), 1, "poll text must not duplicate");
        assert_eq!(message_text(assistant_messages[0]), "Hello world");

        // After a detail reload rebuilds blocks under persisted ids, further
        // updates with aliases still land on the same block.
        let reloaded = detail_state(
            serde_json::json!({
                "thread": {"id": "t1", "name": "Thread", "status": {"type": "active"}},
                "turns": {"nextCursor": null, "backwardsCursor": null, "data": [
                    {"id": "turn-1", "items": [
                        {"id": "item-1", "type": "userMessage", "content": [{"text": "go"}]},
                        {"id": "item-2", "type": "agentMessage", "text": "Hello world"}
                    ]}
                ]}
            }),
            Some(serde_json::json!({"activeTurnId": "turn-1"})),
            "t1".to_string(),
            1,
            None,
        );
        let epoch = reloaded.epoch;
        state.replace_detail(epoch, reloaded);
        handle_app_event(
            stream_event_for(
                3,
                serde_json::json!({
                    "type": "progress",
                    "threadId": "t1",
                    "turnId": "turn-1",
                    "itemId": "msg_a",
                    "itemAliases": ["msg_a", "item-2"],
                    "delta": "!"
                }),
            ),
            &mut state,
        );
        let detail = state.detail.as_ref().expect("detail");
        let assistant_messages: Vec<&MessageBlock> = detail
            .messages
            .iter()
            .filter(|message| message.role == "assistant")
            .collect();
        assert_eq!(
            assistant_messages.len(),
            1,
            "aliased delta must update the reloaded block"
        );
        assert_eq!(message_text(assistant_messages[0]), "Hello world!");
    }

    #[test]
    fn post_attach_new_item_deltas_create_separate_message() {
        let mut state = TuiState::new(TuiInit {
            query: None,
            since: None,
            cwd: None,
            archived: false,
            limit: 50,
            sort: None,
            descending: true,
            prefs: TuiPrefs::default(),
        });
        state.mode = Mode::Detail;
        state.detail = Some(detail_state(
            serde_json::json!({
                "thread": {"id": "t1", "name": "Thread", "status": {"type": "idle"}},
                "turns": {"nextCursor": null, "backwardsCursor": null, "data": []}
            }),
            Some(serde_json::json!({"activeTurnId": "turn-1"})),
            "t1".to_string(),
            1,
            None,
        ));

        handle_app_event(
            stream_event(serde_json::json!({
                "type": "attached",
                "threadId": "t1",
                "turnId": "turn-1",
                "status": "attached",
                "thread": {
                    "id": "t1",
                    "name": "Thread",
                    "status": {"type": "active"},
                    "turns": [
                        {
                            "id": "turn-1",
                            "status": "inProgress",
                            "startedAt": 1_700_000_000,
                            "items": [
                                {
                                    "id": "assistant-1",
                                    "type": "agentMessage",
                                    "text": "First paragraph"
                                }
                            ],
                            "itemsView": "full"
                        }
                    ]
                }
            })),
            &mut state,
        );

        // An item that starts streaming after the attach snapshot must render
        // as its own message instead of merging into the snapshot item.
        for delta in ["Second", " paragraph"] {
            handle_app_event(
                stream_event(serde_json::json!({
                    "type": "progress",
                    "threadId": "t1",
                    "turnId": "turn-1",
                    "itemId": "assistant-2",
                    "delta": delta
                })),
                &mut state,
            );
        }
        handle_app_event(
            stream_event(serde_json::json!({
                "type": "assistantMessage",
                "threadId": "t1",
                "turnId": "turn-1",
                "itemId": "assistant-2",
                "text": "Second paragraph"
            })),
            &mut state,
        );

        let detail = state.detail.as_ref().expect("detail");
        assert_eq!(detail.messages.len(), 2);
        assert_eq!(detail.messages[0].item_id.as_deref(), Some("assistant-1"));
        assert_eq!(message_text(&detail.messages[0]), "First paragraph");
        assert_eq!(detail.messages[1].item_id.as_deref(), Some("assistant-2"));
        assert_eq!(message_text(&detail.messages[1]), "Second paragraph");
        let stream = state.stream.as_ref().expect("stream");
        assert_eq!(stream.assistant_items.len(), 2);
        assert_eq!(stream.assistant_items[0].text, "First paragraph");
        assert_eq!(stream.assistant_items[1].text, "Second paragraph");
    }

    #[test]
    fn poll_full_text_for_wrapped_presteer_item_does_not_duplicate() {
        let long_paragraph = "This assistant paragraph is deliberately much longer than one \
            hundred columns so that the markdown renderer wraps it across multiple lines in \
            the transcript view.";
        let mut state = TuiState::new(TuiInit {
            query: None,
            since: None,
            cwd: None,
            archived: false,
            limit: 50,
            sort: None,
            descending: true,
            prefs: TuiPrefs::default(),
        });
        state.mode = Mode::Detail;
        state.detail = Some(detail_state(
            serde_json::json!({
                "thread": {"id": "t1", "name": "Thread", "status": {"type": "active"}},
                "turns": {"nextCursor": null, "backwardsCursor": null, "data": [
                    {"id": "turn-1", "items": [
                        {"id": "u1", "type": "userMessage", "content": [{"text": "start"}]},
                        {"id": "assistant-1", "type": "agentMessage", "text": long_paragraph}
                    ]}
                ]}
            }),
            Some(serde_json::json!({"activeTurnId": "turn-1"})),
            "t1".to_string(),
            1,
            None,
        ));
        append_detail_message(
            &mut state,
            "t1",
            Some("turn-1".to_string()),
            "user",
            None,
            "steer here",
        );
        assert!(
            state.detail.as_ref().expect("detail").messages[1]
                .lines
                .len()
                > 1,
            "fixture paragraph must wrap"
        );
        state.stream = Some(StreamState::new_with_id(
            7,
            "t1".to_string(),
            Some("turn-1".to_string()),
            StreamStatus::Running,
            true,
        ));

        // A poll re-emits the pre-steer item's full text unchanged: nothing
        // new to display.
        handle_app_event(
            stream_event_for(
                7,
                serde_json::json!({
                    "type": "progress",
                    "threadId": "t1",
                    "turnId": "turn-1",
                    "itemId": "assistant-1",
                    "text": long_paragraph,
                    "source": "poll"
                }),
            ),
            &mut state,
        );

        let rendered = |state: &TuiState| {
            state
                .detail
                .as_ref()
                .expect("detail")
                .messages
                .iter()
                .map(|message| format!("{}:{}", message.role, message_text(message)))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            rendered(&state),
            vec![
                "user:start".to_string(),
                format!("assistant:{long_paragraph}"),
                "user:steer here".to_string(),
            ]
        );

        // When the item later grows, only the continuation renders below the
        // steer message.
        handle_app_event(
            stream_event_for(
                7,
                serde_json::json!({
                    "type": "progress",
                    "threadId": "t1",
                    "turnId": "turn-1",
                    "itemId": "assistant-1",
                    "text": format!("{long_paragraph} Continued after steer."),
                    "source": "poll"
                }),
            ),
            &mut state,
        );
        assert_eq!(
            rendered(&state),
            vec![
                "user:start".to_string(),
                format!("assistant:{long_paragraph}"),
                "user:steer here".to_string(),
                "assistant:Continued after steer.".to_string(),
            ]
        );
    }

    #[test]
    fn browser_auto_attach_snapshot_seeds_stream_for_anonymous_delta() {
        let mut prefs = TuiPrefs::default();
        prefs.browser.preview_pane = true;
        let mut state = TuiState::new(TuiInit {
            query: None,
            since: None,
            cwd: None,
            archived: false,
            limit: 50,
            sort: None,
            descending: true,
            prefs,
        });
        state.browser.rows = vec![test_thread_row("t1", "active")];
        state.browser.preview.server = Some("work".to_string());
        state.browser.preview.thread_id = Some("t1".to_string());

        handle_app_event(
            stream_event(serde_json::json!({
                "type": "attached",
                "threadId": "t1",
                "turnId": "turn-1",
                "status": "attached",
                "thread": {
                    "id": "t1",
                    "name": "Thread",
                    "status": {"type": "active"},
                    "turns": [
                        {
                            "id": "turn-1",
                            "status": "inProgress",
                            "startedAt": 1_700_000_000,
                            "items": [
                                {
                                    "id": "assistant-1",
                                    "type": "agentMessage",
                                    "text": "Already streamed"
                                }
                            ],
                            "itemsView": "full"
                        }
                    ]
                }
            })),
            &mut state,
        );

        assert_eq!(state.browser.preview.messages.len(), 1);
        assert_eq!(
            state.browser.preview.messages[0].item_id.as_deref(),
            Some("assistant-1")
        );
        assert_eq!(
            state
                .stream
                .as_ref()
                .unwrap()
                .assistant_items
                .first()
                .unwrap()
                .item_id
                .as_deref(),
            Some("assistant-1")
        );

        handle_app_event(
            stream_event(serde_json::json!({
                "type": "progress",
                "threadId": "t1",
                "turnId": "turn-1",
                "delta": " plus anonymous"
            })),
            &mut state,
        );

        assert_eq!(state.browser.preview.messages.len(), 1);
        assert_eq!(
            state.browser.preview.messages[0].item_id.as_deref(),
            Some("assistant-1")
        );
        assert_eq!(
            state.browser.preview.messages[0].lines[0].text,
            "Already streamed plus anonymous"
        );
    }

    #[test]
    fn shutdown_signal_detaches_stream_and_quits() {
        let mut state = TuiState::new(TuiInit {
            query: None,
            since: None,
            cwd: None,
            archived: false,
            limit: 50,
            sort: None,
            descending: true,
            prefs: TuiPrefs::default(),
        });
        let (control_tx, mut control_rx) = mpsc::unbounded_channel();
        state.stream_control = Some(control_tx);
        state.stream = Some(StreamState::new(
            "t1".to_string(),
            Some("turn-1".to_string()),
            StreamStatus::Running,
            true,
        ));

        handle_app_event(AppEvent::ShutdownSignal, &mut state);
        assert!(state.should_quit);
        assert!(state.stream.as_ref().expect("stream").detached);
        assert!(matches!(control_rx.try_recv(), Ok(TurnControl::Detach)));
    }

    #[test]
    fn annotation_state_updates_browser_and_detail() {
        let mut state = TuiState::new(TuiInit {
            query: None,
            since: None,
            cwd: None,
            archived: false,
            limit: 50,
            sort: None,
            descending: true,
            prefs: TuiPrefs::default(),
        });
        state.browser.rows.push(ThreadRow {
            server: "work".to_string(),
            id: "t1".to_string(),
            title: "Thread".to_string(),
            status: String::new(),
            updated: String::new(),
            cwd: String::new(),
            annotation: None,
            snippet: None,
            raw: serde_json::json!({}),
        });
        state.detail = Some(detail_state(
            serde_json::json!({
                "thread": {"id": "t1", "name": "Thread", "status": {"type": "idle"}},
                "turns": {"nextCursor": null, "backwardsCursor": null, "data": []}
            }),
            None,
            "t1".to_string(),
            1,
            None,
        ));

        set_annotation_in_state(&mut state, "work", "t1", Some("note".to_string()));
        assert_eq!(state.browser.rows[0].annotation.as_deref(), Some("note"));
        assert_eq!(
            state.detail.as_ref().unwrap().annotation.as_deref(),
            Some("note")
        );
        set_annotation_in_state(&mut state, "work", "t1", None);
        assert!(state.browser.rows[0].annotation.is_none());
        assert!(state.detail.as_ref().unwrap().annotation.is_none());
    }

    #[test]
    fn annotation_input_ignores_control_s_instead_of_inserting_text() {
        let target = Target {
            server: "work".to_string(),
            endpoint: crate::config::Endpoint::Unix {
                path: "/tmp/missing.sock".into(),
            },
            model: None,
            model_reasoning_effort: None,
        };
        let mut state = TuiState::new(TuiInit {
            query: None,
            since: None,
            cwd: None,
            archived: false,
            limit: 50,
            sort: None,
            descending: true,
            prefs: TuiPrefs::default(),
        });

        handle_annotation_input(
            KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL),
            &target,
            &mut state,
            "t1".to_string(),
            "note".to_string(),
            false,
        )
        .unwrap();

        let Mode::AnnotationInput { draft, .. } = &state.mode else {
            panic!("expected annotation input");
        };
        assert_eq!(draft, "note");
    }

    #[test]
    fn rename_state_updates_browser_and_detail_title() {
        let mut state = TuiState::new(TuiInit {
            query: None,
            since: None,
            cwd: None,
            archived: false,
            limit: 50,
            sort: None,
            descending: true,
            prefs: TuiPrefs::default(),
        });
        state.browser.rows = vec![test_thread_row("t1", "idle")];
        state.detail = Some(detail_state(
            serde_json::json!({
                "thread": {"id": "t1", "name": "Old", "status": {"type": "idle"}},
                "turns": {"nextCursor": null, "backwardsCursor": null, "data": []}
            }),
            None,
            "t1".to_string(),
            1,
            None,
        ));

        set_thread_name_in_state(
            &mut state,
            "work",
            "t1",
            "New name",
            &serde_json::json!({"id": "t1", "name": "New name"}),
        );

        assert_eq!(state.browser.rows[0].title, "New name");
        assert_eq!(state.browser.rows[0].raw["name"], "New name");
        assert_eq!(state.detail.as_ref().unwrap().title, "New name");
        assert_eq!(active_thread_title(&state).as_deref(), Some("New name"));
    }

    #[test]
    fn rename_input_rejects_empty_names() {
        let target = Target {
            server: "work".to_string(),
            endpoint: crate::config::Endpoint::Unix {
                path: "/tmp/missing.sock".into(),
            },
            model: None,
            model_reasoning_effort: None,
        };
        let (app_tx, mut app_rx) = mpsc::unbounded_channel();
        let mut state = TuiState::new(TuiInit {
            query: None,
            since: None,
            cwd: None,
            archived: false,
            limit: 50,
            sort: None,
            descending: true,
            prefs: TuiPrefs::default(),
        });

        handle_rename_input(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::empty()),
            &target,
            &mut state,
            "t1".to_string(),
            "   ".to_string(),
            false,
            &app_tx,
        )
        .unwrap();

        assert!(matches!(state.mode, Mode::RenameInput { .. }));
        assert!(state.notice.as_ref().unwrap().message.contains("empty"));
        assert!(app_rx.try_recv().is_err());
    }

    #[test]
    fn rename_input_ctrl_d_clears_draft_only() {
        let target = Target {
            server: "work".to_string(),
            endpoint: crate::config::Endpoint::Unix {
                path: "/tmp/missing.sock".into(),
            },
            model: None,
            model_reasoning_effort: None,
        };
        let (app_tx, mut app_rx) = mpsc::unbounded_channel();
        let mut state = TuiState::new(TuiInit {
            query: None,
            since: None,
            cwd: None,
            archived: false,
            limit: 50,
            sort: None,
            descending: true,
            prefs: TuiPrefs::default(),
        });

        handle_rename_input(
            KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL),
            &target,
            &mut state,
            "t1".to_string(),
            "Custom name".to_string(),
            false,
            &app_tx,
        )
        .unwrap();

        let Mode::RenameInput { draft, .. } = &state.mode else {
            panic!("expected rename input");
        };
        assert!(draft.is_empty());
        assert!(app_rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn ctrl_d_clears_thread_search_and_refreshes_browser() {
        let (fetch_tx, mut fetch_rx) = mpsc::channel(1);
        let mut state = TuiState::new(TuiInit {
            query: Some("needle".to_string()),
            since: None,
            cwd: None,
            archived: false,
            limit: 50,
            sort: None,
            descending: true,
            prefs: TuiPrefs::default(),
        });

        handle_text_input(
            KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL),
            "needle".to_string(),
            ModeKind::Search,
            |value, state| {
                state.update_query(value);
                Ok(InputAction::RefreshBrowser)
            },
            &mut state,
            &fetch_tx,
        )
        .await
        .unwrap();

        assert!(matches!(state.mode, Mode::Browser));
        assert!(state.browser.query.is_empty());
        assert!(matches!(
            fetch_rx.try_recv().unwrap(),
            FetchRequest::Browser { .. }
        ));
    }

    #[tokio::test]
    async fn ctrl_d_clears_message_search_without_browser_refresh() {
        let (fetch_tx, mut fetch_rx) = mpsc::channel(1);
        let mut state = TuiState::new(TuiInit {
            query: None,
            since: None,
            cwd: None,
            archived: false,
            limit: 50,
            sort: None,
            descending: true,
            prefs: TuiPrefs::default(),
        });
        state.detail = Some(detail_state(
            serde_json::json!({
                "thread": {"id": "t1", "name": "Thread", "status": {"type": "idle"}},
                "turns": {"nextCursor": null, "backwardsCursor": null, "data": [
                    {"id": "turn-1", "items": [
                        {"id": "a", "type": "agentMessage", "text": "needle"}
                    ]}
                ]}
            }),
            None,
            "t1".to_string(),
            1,
            None,
        ));
        state.update_message_search("needle".to_string());

        handle_text_input(
            KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL),
            "needle".to_string(),
            ModeKind::MessageSearch,
            |value, state| {
                state.update_message_search(value);
                Ok(InputAction::None)
            },
            &mut state,
            &fetch_tx,
        )
        .await
        .unwrap();

        let detail = state.detail.as_ref().expect("detail");
        assert!(matches!(state.mode, Mode::Detail));
        assert!(detail.search_query.is_empty());
        assert!(detail.matches.is_empty());
        assert!(fetch_rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn load_shortcut_schedules_selected_browser_thread_load() {
        let _target = Target {
            server: "work".to_string(),
            endpoint: crate::config::Endpoint::Unix {
                path: "/tmp/missing.sock".into(),
            },
            model: None,
            model_reasoning_effort: None,
        };
        let (fetch_tx, mut fetch_rx) = mpsc::channel(1);
        let (app_tx, _app_rx) = mpsc::unbounded_channel();
        let mut state = TuiState::new(TuiInit {
            query: None,
            since: None,
            cwd: None,
            archived: false,
            limit: 50,
            sort: None,
            descending: true,
            prefs: TuiPrefs::default(),
        });
        state.browser.rows = vec![test_thread_row("t1", "notLoaded")];

        handle_terminal_event(
            Event::Key(KeyEvent::new(KeyCode::Char('l'), KeyModifiers::empty())),
            &mut state,
            &test_targets(),
            false,
            &fetch_tx,
            &app_tx,
        )
        .await
        .unwrap();

        let FetchRequest::LoadThread { thread_id, .. } = fetch_rx.recv().await.unwrap() else {
            panic!("expected load thread request");
        };
        assert_eq!(thread_id, "t1");
        assert!(
            state
                .notice
                .as_ref()
                .unwrap()
                .message
                .contains("loading t1")
        );
    }

    #[tokio::test]
    async fn load_shortcut_schedules_open_detail_thread_load() {
        let _target = Target {
            server: "work".to_string(),
            endpoint: crate::config::Endpoint::Unix {
                path: "/tmp/missing.sock".into(),
            },
            model: None,
            model_reasoning_effort: None,
        };
        let (fetch_tx, mut fetch_rx) = mpsc::channel(1);
        let (app_tx, _app_rx) = mpsc::unbounded_channel();
        let mut state = TuiState::new(TuiInit {
            query: None,
            since: None,
            cwd: None,
            archived: false,
            limit: 50,
            sort: None,
            descending: true,
            prefs: TuiPrefs::default(),
        });
        state.mode = Mode::Detail;
        state.detail = Some(detail_state(
            serde_json::json!({
                "thread": {"id": "t1", "name": "Thread", "status": {"type": "notLoaded"}},
                "turns": {"nextCursor": null, "backwardsCursor": null, "data": []}
            }),
            None,
            "t1".to_string(),
            1,
            None,
        ));

        handle_terminal_event(
            Event::Key(KeyEvent::new(KeyCode::Char('l'), KeyModifiers::empty())),
            &mut state,
            &test_targets(),
            false,
            &fetch_tx,
            &app_tx,
        )
        .await
        .unwrap();

        let FetchRequest::LoadThread { thread_id, .. } = fetch_rx.recv().await.unwrap() else {
            panic!("expected load thread request");
        };
        assert_eq!(thread_id, "t1");
        assert!(matches!(state.mode, Mode::Detail));
    }

    #[test]
    fn thread_loaded_event_updates_visible_state_and_invalidates_preview() {
        let mut state = TuiState::new(TuiInit {
            query: None,
            since: None,
            cwd: None,
            archived: false,
            limit: 50,
            sort: None,
            descending: true,
            prefs: TuiPrefs::default(),
        });
        state.browser.rows = vec![test_thread_row("t1", "notLoaded")];
        state.browser.preview.server = Some("work".to_string());
        state.browser.preview.thread_id = Some("t1".to_string());
        state.browser.preview.messages = vec![message_block(
            Some("turn-old".to_string()),
            None,
            "assistant",
            None,
            "old",
            100,
        )];
        state.detail = Some(detail_state(
            serde_json::json!({
                "thread": {"id": "t1", "name": "Old", "status": {"type": "notLoaded"}},
                "turns": {"nextCursor": null, "backwardsCursor": null, "data": []}
            }),
            None,
            "t1".to_string(),
            1,
            None,
        ));

        handle_app_event(
            AppEvent::ThreadLoaded {
                server: "work".to_string(),
                thread_id: "t1".to_string(),
                status: serde_json::json!({
                    "threadId": "t1",
                    "thread": {
                        "id": "t1",
                        "name": "Loaded title",
                        "cwd": "/tmp/repo",
                        "updatedAt": 1_700_000_000,
                        "status": {"type": "idle"}
                    },
                    "activeTurnId": null,
                    "truncated": false
                }),
            },
            &mut state,
        );

        assert_eq!(state.browser.rows[0].title, "Loaded title");
        assert_eq!(state.browser.rows[0].cwd, "/tmp/repo");
        assert_eq!(state.browser.rows[0].status, "idle");
        assert_eq!(
            state.browser.rows[0].raw["status"]["type"].as_str(),
            Some("idle")
        );
        assert_eq!(state.detail.as_ref().unwrap().title, "Loaded title");
        assert_eq!(state.detail.as_ref().unwrap().status, "idle");
        assert!(state.browser.preview.thread_id.is_none());
        assert!(state.browser.preview.messages.is_empty());
    }

    #[test]
    fn archive_change_updates_visible_rows_and_detail_status() {
        let mut state = TuiState::new(TuiInit {
            query: None,
            since: None,
            cwd: None,
            archived: false,
            limit: 50,
            sort: None,
            descending: true,
            prefs: TuiPrefs::default(),
        });
        state.browser.rows = vec![test_thread_row("t1", "idle"), test_thread_row("t2", "idle")];
        state.browser.selected = 1;
        state.detail = Some(detail_state(
            serde_json::json!({
                "thread": {"id": "t1", "name": "Thread", "status": {"type": "idle"}},
                "turns": {"nextCursor": null, "backwardsCursor": null, "data": []}
            }),
            None,
            "t1".to_string(),
            1,
            None,
        ));

        apply_archive_change(
            &mut state,
            "work",
            "t1",
            true,
            &serde_json::json!({"id": "t1", "status": {"type": "archived"}}),
        );

        assert_eq!(
            state
                .browser
                .rows
                .iter()
                .map(|row| row.id.as_str())
                .collect::<Vec<_>>(),
            vec!["t2"]
        );
        assert_eq!(state.browser.selected, 0);
        assert_eq!(state.detail.as_ref().unwrap().status, "archived");
    }

    #[test]
    fn archive_toggle_direction_follows_current_archived_context() {
        let mut state = TuiState::new(TuiInit {
            query: None,
            since: None,
            cwd: None,
            archived: false,
            limit: 50,
            sort: None,
            descending: true,
            prefs: TuiPrefs::default(),
        });
        state.browser.rows = vec![test_thread_row("t1", "idle")];
        assert!(!active_thread_is_archived(&state));

        state.browser.archived = true;
        assert!(active_thread_is_archived(&state));

        apply_archive_change(
            &mut state,
            "work",
            "t1",
            false,
            &serde_json::json!({"id": "t1", "status": {"type": "idle"}}),
        );
        assert!(state.browser.rows.is_empty());
    }

    #[tokio::test]
    async fn archive_shortcut_opens_confirmation_before_rpc() {
        let _target = Target {
            server: "work".to_string(),
            endpoint: crate::config::Endpoint::Unix {
                path: "/tmp/missing.sock".into(),
            },
            model: None,
            model_reasoning_effort: None,
        };
        let mut state = TuiState::new(TuiInit {
            query: None,
            since: None,
            cwd: None,
            archived: false,
            limit: 50,
            sort: None,
            descending: true,
            prefs: TuiPrefs::default(),
        });
        state.browser.rows = vec![test_thread_row("t1", "idle")];
        let (fetch_tx, _fetch_rx) = mpsc::channel(1);
        let (app_tx, mut app_rx) = mpsc::unbounded_channel();

        handle_terminal_event(
            Event::Key(KeyEvent::new(KeyCode::Char('A'), KeyModifiers::SHIFT)),
            &mut state,
            &test_targets(),
            false,
            &fetch_tx,
            &app_tx,
        )
        .await
        .unwrap();

        assert!(matches!(
            state.mode,
            Mode::ConfirmArchive {
                ref thread_id,
                archived: true,
                return_to_detail: false,
                ..
            } if thread_id == "t1"
        ));
        assert!(app_rx.try_recv().is_err());

        handle_terminal_event(
            Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::empty())),
            &mut state,
            &test_targets(),
            false,
            &fetch_tx,
            &app_tx,
        )
        .await
        .unwrap();

        assert!(matches!(state.mode, Mode::Browser));
        assert!(app_rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn open_codex_shortcut_opens_confirmation_with_thread_cwd() {
        let _target = Target {
            server: "work".to_string(),
            endpoint: crate::config::Endpoint::Unix {
                path: "/tmp/missing.sock".into(),
            },
            model: None,
            model_reasoning_effort: None,
        };
        let mut state = TuiState::new(TuiInit {
            query: None,
            since: None,
            cwd: None,
            archived: false,
            limit: 50,
            sort: None,
            descending: true,
            prefs: TuiPrefs::default(),
        });
        let mut row = test_thread_row("t1", "idle");
        row.cwd = "/tmp/project".to_string();
        state.browser.rows = vec![row];
        let (fetch_tx, _fetch_rx) = mpsc::channel(1);
        let (app_tx, mut app_rx) = mpsc::unbounded_channel();

        handle_terminal_event(
            Event::Key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::empty())),
            &mut state,
            &test_targets(),
            false,
            &fetch_tx,
            &app_tx,
        )
        .await
        .unwrap();

        assert!(matches!(
            state.mode,
            Mode::ConfirmOpenCodex {
                ref thread_id,
                ref cwd,
                return_to_detail: false,
                ..
            } if thread_id == "t1" && cwd == "/tmp/project"
        ));
        assert!(app_rx.try_recv().is_err());
    }

    #[test]
    fn codex_resume_launch_uses_remote_cwd_and_yolo_flag() {
        let target = Target {
            server: "work".to_string(),
            endpoint: crate::config::Endpoint::Unix {
                path: "/tmp/codex.sock".into(),
            },
            model: None,
            model_reasoning_effort: None,
        };

        let launch = build_codex_resume_launch(&target, "session-1", "/tmp/project", true);
        let args = launch
            .args
            .iter()
            .map(|arg| arg.to_string_lossy().to_string())
            .collect::<Vec<_>>();

        assert_eq!(
            args,
            vec![
                "resume",
                "session-1",
                "--remote",
                "unix:///tmp/codex.sock",
                "--dangerously-bypass-approvals-and-sandbox",
                "--cd",
                "/tmp/project",
            ]
        );
        assert!(launch.env.is_empty());
    }

    #[test]
    fn codex_resume_launch_passes_websocket_auth_by_env() {
        let target = Target {
            server: "work".to_string(),
            endpoint: crate::config::Endpoint::WebSocket {
                url: "ws://127.0.0.1:1234/".to_string(),
                auth_token: Some("secret-token".to_string()),
            },
            model: None,
            model_reasoning_effort: None,
        };

        let launch = build_codex_resume_launch(&target, "session-1", "/tmp/project", false);
        let args = launch
            .args
            .iter()
            .map(|arg| arg.to_string_lossy().to_string())
            .collect::<Vec<_>>();

        assert_eq!(
            args,
            vec![
                "resume",
                "session-1",
                "--remote",
                "ws://127.0.0.1:1234/",
                "--remote-auth-token-env",
                CODEX_REMOTE_AUTH_ENV,
                "--cd",
                "/tmp/project",
            ]
        );
        assert_eq!(
            launch
                .env
                .iter()
                .map(|(name, value)| (
                    name.to_string_lossy().to_string(),
                    value.to_string_lossy().to_string()
                ))
                .collect::<Vec<_>>(),
            vec![(
                CODEX_REMOTE_AUTH_ENV.to_string(),
                "secret-token".to_string()
            )]
        );
    }

    #[tokio::test]
    async fn archive_confirmation_preserves_detail_return_mode() {
        let _target = Target {
            server: "work".to_string(),
            endpoint: crate::config::Endpoint::Unix {
                path: "/tmp/missing.sock".into(),
            },
            model: None,
            model_reasoning_effort: None,
        };
        let mut state = TuiState::new(TuiInit {
            query: None,
            since: None,
            cwd: None,
            archived: false,
            limit: 50,
            sort: None,
            descending: true,
            prefs: TuiPrefs::default(),
        });
        state.mode = Mode::Detail;
        state.detail = Some(detail_state(
            serde_json::json!({
                "thread": {"id": "t1", "name": "Thread", "status": {"type": "archived"}},
                "turns": {"nextCursor": null, "backwardsCursor": null, "data": []}
            }),
            None,
            "t1".to_string(),
            1,
            None,
        ));
        let (fetch_tx, _fetch_rx) = mpsc::channel(1);
        let (app_tx, _app_rx) = mpsc::unbounded_channel();

        handle_terminal_event(
            Event::Key(KeyEvent::new(KeyCode::Char('A'), KeyModifiers::SHIFT)),
            &mut state,
            &test_targets(),
            false,
            &fetch_tx,
            &app_tx,
        )
        .await
        .unwrap();

        assert!(matches!(
            state.mode,
            Mode::ConfirmArchive {
                ref thread_id,
                archived: false,
                return_to_detail: true,
                ..
            } if thread_id == "t1"
        ));
    }

    #[tokio::test]
    async fn browser_message_action_targets_selected_active_thread_as_steer() {
        let _target = Target {
            server: "work".to_string(),
            endpoint: crate::config::Endpoint::Unix {
                path: "/tmp/missing.sock".into(),
            },
            model: None,
            model_reasoning_effort: None,
        };
        let mut state = TuiState::new(TuiInit {
            query: None,
            since: None,
            cwd: None,
            archived: false,
            limit: 50,
            sort: None,
            descending: true,
            prefs: TuiPrefs::default(),
        });
        state.browser.rows = vec![test_thread_row("t1", "active")];
        let (fetch_tx, _fetch_rx) = mpsc::channel(1);
        let (app_tx, _app_rx) = mpsc::unbounded_channel();

        handle_terminal_event(
            Event::Key(KeyEvent::new(KeyCode::Char('m'), KeyModifiers::empty())),
            &mut state,
            &test_targets(),
            false,
            &fetch_tx,
            &app_tx,
        )
        .await
        .unwrap();

        assert!(matches!(
            state.mode,
            Mode::Compose(ComposeState {
                target: ComposeTarget::SteerSelected { ref thread_id, .. },
                send_mode: SendMode::Stream,
                return_to_detail: false,
                ..
            }) if thread_id == "t1"
        ));
    }

    #[tokio::test]
    async fn active_compose_tab_switches_between_steer_and_send() {
        let _target = Target {
            server: "work".to_string(),
            endpoint: crate::config::Endpoint::Unix {
                path: "/tmp/missing.sock".into(),
            },
            model: None,
            model_reasoning_effort: None,
        };
        let mut state = TuiState::new(TuiInit {
            query: None,
            since: None,
            cwd: None,
            archived: false,
            limit: 50,
            sort: None,
            descending: true,
            prefs: TuiPrefs::default(),
        });
        state.browser.rows = vec![test_thread_row("t1", "active")];
        let (app_tx, _app_rx) = mpsc::unbounded_channel();

        open_message_action(&mut state, "work".to_string(), "t1".to_string());
        let Mode::Compose(compose) = &state.mode else {
            panic!("expected compose mode");
        };
        assert!(matches!(
            compose.target,
            ComposeTarget::SteerSelected { ref thread_id, .. } if thread_id == "t1"
        ));
        let compose = compose.clone();

        handle_compose_input(
            KeyEvent::new(KeyCode::Tab, KeyModifiers::empty()),
            &mut state,
            compose,
            &test_target(),
            false,
            &app_tx,
        )
        .await
        .unwrap();
        let Mode::Compose(compose) = &state.mode else {
            panic!("expected compose mode");
        };
        assert!(matches!(
            compose.target,
            ComposeTarget::NewTurn { ref thread_id, .. } if thread_id == "t1"
        ));
        let compose = compose.clone();

        handle_compose_input(
            KeyEvent::new(KeyCode::Tab, KeyModifiers::empty()),
            &mut state,
            compose,
            &test_target(),
            false,
            &app_tx,
        )
        .await
        .unwrap();
        let Mode::Compose(compose) = &state.mode else {
            panic!("expected compose mode");
        };
        assert!(matches!(
            compose.target,
            ComposeTarget::SteerSelected { ref thread_id, .. } if thread_id == "t1"
        ));
    }

    #[tokio::test]
    async fn browser_interrupt_shortcut_returns_to_browser() {
        let _target = Target {
            server: "work".to_string(),
            endpoint: crate::config::Endpoint::Unix {
                path: "/tmp/missing.sock".into(),
            },
            model: None,
            model_reasoning_effort: None,
        };
        let mut state = TuiState::new(TuiInit {
            query: None,
            since: None,
            cwd: None,
            archived: false,
            limit: 50,
            sort: None,
            descending: true,
            prefs: TuiPrefs::default(),
        });
        state.browser.rows = vec![test_thread_row("t1", "active")];
        let (fetch_tx, _fetch_rx) = mpsc::channel(1);
        let (app_tx, _app_rx) = mpsc::unbounded_channel();

        handle_terminal_event(
            Event::Key(KeyEvent::new(KeyCode::Char('i'), KeyModifiers::empty())),
            &mut state,
            &test_targets(),
            false,
            &fetch_tx,
            &app_tx,
        )
        .await
        .unwrap();

        assert!(matches!(
            state.mode,
            Mode::ConfirmInterrupt {
                ref thread_id,
                turn_id: None,
                return_to_detail: false,
                ..
            } if thread_id == "t1"
        ));

        handle_terminal_event(
            Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::empty())),
            &mut state,
            &test_targets(),
            false,
            &fetch_tx,
            &app_tx,
        )
        .await
        .unwrap();

        assert!(matches!(state.mode, Mode::Browser));
    }

    #[tokio::test]
    async fn detail_interrupt_shortcut_returns_to_detail() {
        let _target = Target {
            server: "work".to_string(),
            endpoint: crate::config::Endpoint::Unix {
                path: "/tmp/missing.sock".into(),
            },
            model: None,
            model_reasoning_effort: None,
        };
        let mut state = TuiState::new(TuiInit {
            query: None,
            since: None,
            cwd: None,
            archived: false,
            limit: 50,
            sort: None,
            descending: true,
            prefs: TuiPrefs::default(),
        });
        state.mode = Mode::Detail;
        state.detail = Some(detail_state(
            serde_json::json!({
                "thread": {"id": "t1", "name": "Thread", "status": {"type": "active"}},
                "turns": {"nextCursor": null, "backwardsCursor": null, "data": []}
            }),
            Some(serde_json::json!({"activeTurnId": "turn-1"})),
            "t1".to_string(),
            1,
            None,
        ));
        let (fetch_tx, _fetch_rx) = mpsc::channel(1);
        let (app_tx, _app_rx) = mpsc::unbounded_channel();

        handle_terminal_event(
            Event::Key(KeyEvent::new(KeyCode::Char('i'), KeyModifiers::empty())),
            &mut state,
            &test_targets(),
            false,
            &fetch_tx,
            &app_tx,
        )
        .await
        .unwrap();

        assert!(matches!(
            state.mode,
            Mode::ConfirmInterrupt {
                ref thread_id,
                turn_id: Some(ref turn_id),
                return_to_detail: true,
                ..
            } if thread_id == "t1" && turn_id == "turn-1"
        ));

        handle_terminal_event(
            Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::empty())),
            &mut state,
            &test_targets(),
            false,
            &fetch_tx,
            &app_tx,
        )
        .await
        .unwrap();

        assert!(matches!(state.mode, Mode::Detail));
    }

    #[test]
    fn mouse_wheel_moves_browser_selection() {
        let mut state = TuiState::new(TuiInit {
            query: None,
            since: None,
            cwd: None,
            archived: false,
            limit: 50,
            sort: None,
            descending: true,
            prefs: TuiPrefs::default(),
        });
        for index in 0..5 {
            state.browser.rows.push(ThreadRow {
                server: "work".to_string(),
                id: format!("t{index}"),
                title: format!("Thread {index}"),
                status: String::new(),
                updated: String::new(),
                cwd: String::new(),
                annotation: None,
                snippet: None,
                raw: serde_json::json!({}),
            });
        }

        handle_mouse_event(mouse_wheel(MouseEventKind::ScrollDown), &mut state);
        assert_eq!(state.browser.selected, 3);
        handle_mouse_event(mouse_wheel(MouseEventKind::ScrollUp), &mut state);
        assert_eq!(state.browser.selected, 0);
    }

    #[test]
    fn browser_selection_change_detaches_stream_for_previous_thread() {
        let mut state = TuiState::new(TuiInit {
            query: None,
            since: None,
            cwd: None,
            archived: false,
            limit: 50,
            sort: None,
            descending: true,
            prefs: TuiPrefs::default(),
        });
        state.browser.rows = vec![
            test_thread_row("t1", "active"),
            test_thread_row("t2", "idle"),
        ];
        state.stream = Some(StreamState::new(
            "t1".to_string(),
            Some("turn-1".to_string()),
            StreamStatus::Running,
            true,
        ));
        let (control_tx, mut control_rx) = mpsc::unbounded_channel();
        state.stream_control = Some(control_tx);

        state.move_selection(1);
        detach_stream_if_browser_selection_changed(
            &mut state,
            Some(("work".to_string(), "t1".to_string())),
        );

        assert!(state.stream.is_none());
        assert!(matches!(control_rx.try_recv(), Ok(TurnControl::Detach)));
    }

    #[test]
    fn mouse_wheel_scrolls_detail_transcript() {
        let mut state = TuiState::new(TuiInit {
            query: None,
            since: None,
            cwd: None,
            archived: false,
            limit: 50,
            sort: None,
            descending: true,
            prefs: TuiPrefs::default(),
        });
        state.mode = Mode::Detail;
        state.detail = Some(detail_state(
            serde_json::json!({
                "thread": {"id": "t1", "name": "Thread", "status": {"type": "idle"}},
                "turns": {"nextCursor": null, "backwardsCursor": null, "data": [
                    {"id": "turn-1", "items": [
                        {"id": "a", "type": "agentMessage", "text": "one\ntwo\nthree\nfour"}
                    ]}
                ]}
            }),
            None,
            "t1".to_string(),
            1,
            None,
        ));

        handle_mouse_event(mouse_wheel(MouseEventKind::ScrollDown), &mut state);
        assert_eq!(state.detail.as_ref().unwrap().scroll, 3);
        handle_mouse_event(mouse_wheel(MouseEventKind::ScrollUp), &mut state);
        assert_eq!(state.detail.as_ref().unwrap().scroll, 0);
    }

    #[tokio::test]
    async fn vim_goto_shortcuts_jump_browser_and_detail() {
        let mut state = TuiState::new(TuiInit {
            query: None,
            since: None,
            cwd: None,
            archived: false,
            limit: 50,
            sort: None,
            descending: true,
            prefs: TuiPrefs::default(),
        });
        for index in 0..4 {
            state.browser.rows.push(ThreadRow {
                server: "work".to_string(),
                id: format!("t{index}"),
                title: format!("Thread {index}"),
                status: String::new(),
                updated: String::new(),
                cwd: String::new(),
                annotation: None,
                snippet: None,
                raw: serde_json::json!({}),
            });
        }
        let (fetch_tx, mut fetch_rx) = mpsc::channel(1);
        handle_goto_key(KeyCode::Char('G'), &mut state, &fetch_tx)
            .await
            .unwrap();
        assert_eq!(state.browser.selected, 3);
        handle_goto_key(KeyCode::Char('g'), &mut state, &fetch_tx)
            .await
            .unwrap();
        assert!(state.pending_goto_top);
        handle_goto_key(KeyCode::Char('g'), &mut state, &fetch_tx)
            .await
            .unwrap();
        assert_eq!(state.browser.selected, 0);
        assert!(!state.pending_goto_top);
        assert!(fetch_rx.try_recv().is_err());

        state.mode = Mode::Detail;
        state.detail = Some(detail_state(
            serde_json::json!({
                "thread": {"id": "t1", "name": "Thread", "status": {"type": "idle"}},
                "turns": {"nextCursor": null, "backwardsCursor": null, "data": [
                    {"id": "turn-1", "items": [
                        {"id": "a", "type": "agentMessage", "text": "one\ntwo\nthree\nfour"}
                    ]}
                ]}
            }),
            None,
            "t1".to_string(),
            1,
            None,
        ));
        handle_goto_key(KeyCode::Char('G'), &mut state, &fetch_tx)
            .await
            .unwrap();
        assert!(state.detail.as_ref().unwrap().scroll > 0);
        handle_goto_key(KeyCode::Home, &mut state, &fetch_tx)
            .await
            .unwrap();
        assert_eq!(state.detail.as_ref().unwrap().scroll, 0);
        assert!(fetch_rx.try_recv().is_err());
    }

    #[test]
    fn osc52_clipboard_sequence_encodes_thread_id() {
        assert_eq!(
            osc52_clipboard_sequence("thread-1"),
            "\x1b]52;c;dGhyZWFkLTE=\x07"
        );
        assert_eq!(base64_encode(b"abc"), "YWJj");
        assert_eq!(base64_encode(b"ab"), "YWI=");
        assert_eq!(base64_encode(b"a"), "YQ==");
    }

    #[tokio::test]
    async fn compose_enter_submits_and_ctrl_j_inserts_newline() {
        let _target = Target {
            server: "work".to_string(),
            endpoint: crate::config::Endpoint::Unix {
                path: "/tmp/missing.sock".into(),
            },
            model: None,
            model_reasoning_effort: None,
        };
        let (app_tx, _app_rx) = mpsc::unbounded_channel();
        let mut state = TuiState::new(TuiInit {
            query: None,
            since: None,
            cwd: None,
            archived: false,
            limit: 50,
            sort: None,
            descending: true,
            prefs: TuiPrefs::default(),
        });

        handle_compose_input(
            KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL),
            &mut state,
            ComposeState {
                target: ComposeTarget::NewTurn {
                    server: "work".to_string(),
                    thread_id: "t1".to_string(),
                },
                text: "hello".to_string(),
                send_mode: SendMode::NoWait,
                return_to_detail: false,
            },
            &test_target(),
            true,
            &app_tx,
        )
        .await
        .unwrap();
        let Mode::Compose(compose) = &state.mode else {
            panic!("expected compose mode");
        };
        assert_eq!(compose.text, "hello\n");

        handle_compose_input(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::empty()),
            &mut state,
            ComposeState {
                target: ComposeTarget::NewTurn {
                    server: "work".to_string(),
                    thread_id: "t1".to_string(),
                },
                text: "send me".to_string(),
                send_mode: SendMode::NoWait,
                return_to_detail: false,
            },
            &test_target(),
            true,
            &app_tx,
        )
        .await
        .unwrap();
        assert!(matches!(state.mode, Mode::Browser));
        assert!(state.stream.is_none());
    }

    #[tokio::test]
    async fn browser_compose_origin_ignores_loaded_detail() {
        let _target = Target {
            server: "work".to_string(),
            endpoint: crate::config::Endpoint::Unix {
                path: "/tmp/missing.sock".into(),
            },
            model: None,
            model_reasoning_effort: None,
        };
        let (app_tx, _app_rx) = mpsc::unbounded_channel();
        let mut state = TuiState::new(TuiInit {
            query: None,
            since: None,
            cwd: None,
            archived: false,
            limit: 50,
            sort: None,
            descending: true,
            prefs: TuiPrefs::default(),
        });
        state.mode = Mode::Browser;
        state.detail = Some(detail_state(
            serde_json::json!({
                "thread": {"id": "old", "name": "Old", "status": {"type": "idle"}},
                "turns": {"nextCursor": null, "backwardsCursor": null, "data": []}
            }),
            None,
            "old".to_string(),
            1,
            None,
        ));

        handle_compose_input(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::empty()),
            &mut state,
            ComposeState {
                target: ComposeTarget::NewTurn {
                    server: "work".to_string(),
                    thread_id: "new".to_string(),
                },
                text: "send me".to_string(),
                send_mode: SendMode::NoWait,
                return_to_detail: false,
            },
            &test_target(),
            true,
            &app_tx,
        )
        .await
        .unwrap();

        assert!(matches!(state.mode, Mode::Browser));
        assert!(state.stream.is_none());
    }

    #[tokio::test]
    async fn browser_stream_compose_sets_stream_and_preview_draft() {
        let _target = Target {
            server: "work".to_string(),
            endpoint: crate::config::Endpoint::Unix {
                path: "/tmp/missing.sock".into(),
            },
            model: None,
            model_reasoning_effort: None,
        };
        let (app_tx, _app_rx) = mpsc::unbounded_channel();
        let mut state = TuiState::new(TuiInit {
            query: None,
            since: None,
            cwd: None,
            archived: false,
            limit: 50,
            sort: None,
            descending: true,
            prefs: TuiPrefs::default(),
        });
        state.browser.rows = vec![test_thread_row("t1", "idle")];
        state.browser.preview.thread_id = Some("t1".to_string());

        handle_compose_input(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::empty()),
            &mut state,
            ComposeState {
                target: ComposeTarget::NewTurn {
                    server: "work".to_string(),
                    thread_id: "t1".to_string(),
                },
                text: "send me".to_string(),
                send_mode: SendMode::Stream,
                return_to_detail: false,
            },
            &test_target(),
            true,
            &app_tx,
        )
        .await
        .unwrap();

        assert!(matches!(state.mode, Mode::Browser));
        assert_eq!(
            state
                .stream
                .as_ref()
                .map(|stream| stream.thread_id.as_str()),
            Some("t1")
        );
        assert_eq!(state.browser.preview.messages.len(), 1);
        assert_eq!(state.browser.preview.messages[0].role, "user");
        assert_eq!(state.browser.preview.messages[0].lines[0].text, "send me");
    }

    #[tokio::test]
    async fn browser_compose_defaults_stream_and_tab_toggles_no_wait() {
        let _target = Target {
            server: "work".to_string(),
            endpoint: crate::config::Endpoint::Unix {
                path: "/tmp/missing.sock".into(),
            },
            model: None,
            model_reasoning_effort: None,
        };
        let (app_tx, _app_rx) = mpsc::unbounded_channel();
        let mut state = TuiState::new(TuiInit {
            query: None,
            since: None,
            cwd: None,
            archived: false,
            limit: 50,
            sort: None,
            descending: true,
            prefs: TuiPrefs::default(),
        });

        open_message_action(&mut state, "work".to_string(), "t1".to_string());

        let Mode::Compose(compose) = &state.mode else {
            panic!("expected compose mode");
        };
        assert_eq!(compose.send_mode, SendMode::Stream);
        assert!(!compose.return_to_detail);
        let compose = compose.clone();

        handle_compose_input(
            KeyEvent::new(KeyCode::Tab, KeyModifiers::empty()),
            &mut state,
            compose,
            &test_target(),
            true,
            &app_tx,
        )
        .await
        .unwrap();

        let Mode::Compose(compose) = &state.mode else {
            panic!("expected compose mode");
        };
        assert_eq!(compose.send_mode, SendMode::NoWait);
    }

    #[test]
    fn initial_browser_load_needs_auto_attach_only_for_empty_browser() {
        let state = TuiState::new(TuiInit {
            query: None,
            since: None,
            cwd: None,
            archived: false,
            limit: 50,
            sort: None,
            descending: true,
            prefs: TuiPrefs::default(),
        });
        let event = AppEvent::BrowserLoaded {
            epoch: 1,
            rows: vec![test_thread_row("active-thread", "active")],
            next_cursor: None,
            backwards_cursor: None,
            warning: None,
        };
        assert!(initial_browser_load_needs_auto_attach(&event, &state));

        let mut loaded = state.clone();
        loaded.browser.rows = vec![test_thread_row("active-thread", "active")];
        assert!(!initial_browser_load_needs_auto_attach(&event, &loaded));
    }

    #[tokio::test]
    async fn initial_active_browser_selection_auto_attaches() {
        let _target = Target {
            server: "work".to_string(),
            endpoint: crate::config::Endpoint::Unix {
                path: "/tmp/missing.sock".into(),
            },
            model: None,
            model_reasoning_effort: None,
        };
        let (app_tx, _app_rx) = mpsc::unbounded_channel();
        let mut state = TuiState::new(TuiInit {
            query: None,
            since: None,
            cwd: None,
            archived: false,
            limit: 50,
            sort: None,
            descending: true,
            prefs: TuiPrefs::default(),
        });
        state.browser.rows = vec![test_thread_row("active-thread", "active")];

        auto_attach_selected_browser_thread_if_active(&mut state, test_targets(), true, app_tx);

        let stream = state.stream.as_ref().expect("stream");
        assert_eq!(stream.thread_id, "active-thread");
        assert_eq!(stream.status, StreamStatus::Starting);
        assert!(stream.attached);
        assert!(state.stream_control.is_some());
    }

    #[tokio::test]
    async fn opening_active_detail_auto_attaches() {
        let _target = Target {
            server: "work".to_string(),
            endpoint: crate::config::Endpoint::Unix {
                path: "/tmp/missing.sock".into(),
            },
            model: None,
            model_reasoning_effort: None,
        };
        let (app_tx, _app_rx) = mpsc::unbounded_channel();
        let mut state = TuiState::new(TuiInit {
            query: None,
            since: None,
            cwd: None,
            archived: false,
            limit: 50,
            sort: None,
            descending: true,
            prefs: TuiPrefs::default(),
        });
        state.mode = Mode::Detail;
        state.detail = Some(detail_state(
            serde_json::json!({
                "thread": {"id": "active-thread", "name": "Active", "status": {"type": "active"}},
                "turns": {"nextCursor": null, "backwardsCursor": null, "data": []}
            }),
            Some(serde_json::json!({"activeTurnId": "turn-active"})),
            "active-thread".to_string(),
            1,
            None,
        ));

        auto_attach_open_detail_thread_if_active(&mut state, test_targets(), true, app_tx);

        let stream = state.stream.as_ref().expect("stream");
        assert_eq!(stream.thread_id, "active-thread");
        assert_eq!(stream.turn_id.as_deref(), Some("turn-active"));
        assert_eq!(stream.status, StreamStatus::Running);
        assert!(stream.attached);
        assert!(state.stream_control.is_some());
    }

    #[test]
    fn opening_idle_detail_does_not_auto_attach() {
        let _target = Target {
            server: "work".to_string(),
            endpoint: crate::config::Endpoint::Unix {
                path: "/tmp/missing.sock".into(),
            },
            model: None,
            model_reasoning_effort: None,
        };
        let (app_tx, _app_rx) = mpsc::unbounded_channel();
        let mut state = TuiState::new(TuiInit {
            query: None,
            since: None,
            cwd: None,
            archived: false,
            limit: 50,
            sort: None,
            descending: true,
            prefs: TuiPrefs::default(),
        });
        state.mode = Mode::Detail;
        state.detail = Some(detail_state(
            serde_json::json!({
                "thread": {"id": "idle-thread", "name": "Idle", "status": {"type": "idle"}},
                "turns": {"nextCursor": null, "backwardsCursor": null, "data": []}
            }),
            None,
            "idle-thread".to_string(),
            1,
            None,
        ));

        auto_attach_open_detail_thread_if_active(&mut state, test_targets(), true, app_tx);

        assert!(state.stream.is_none());
        assert!(state.stream_control.is_none());
    }

    #[test]
    fn browser_stream_activity_touches_updated_timestamp() {
        let mut state = TuiState::new(TuiInit {
            query: None,
            since: None,
            cwd: None,
            archived: false,
            limit: 50,
            sort: None,
            descending: true,
            prefs: TuiPrefs::default(),
        });
        state.browser.rows = vec![ThreadRow {
            server: "work".to_string(),
            id: "t1".to_string(),
            title: "Thread".to_string(),
            status: "idle".to_string(),
            updated: "old".to_string(),
            cwd: String::new(),
            annotation: None,
            snippet: None,
            raw: serde_json::json!({"id": "t1", "updatedAt": 1}),
        }];

        touch_browser_thread_updated(&mut state, "work", "t1");

        assert_ne!(state.browser.rows[0].updated, "old");
        assert!(
            state.browser.rows[0].raw["updatedAt"]
                .as_i64()
                .unwrap_or_default()
                > 1
        );
    }

    #[tokio::test]
    async fn detail_compose_can_toggle_stream_mode() {
        let _target = Target {
            server: "work".to_string(),
            endpoint: crate::config::Endpoint::Unix {
                path: "/tmp/missing.sock".into(),
            },
            model: None,
            model_reasoning_effort: None,
        };
        let (app_tx, _app_rx) = mpsc::unbounded_channel();
        let mut state = TuiState::new(TuiInit {
            query: None,
            since: None,
            cwd: None,
            archived: false,
            limit: 50,
            sort: None,
            descending: true,
            prefs: TuiPrefs::default(),
        });
        state.mode = Mode::Detail;
        state.detail = Some(detail_state(
            serde_json::json!({
                "thread": {"id": "t1", "name": "Thread", "status": {"type": "idle"}},
                "turns": {"nextCursor": null, "backwardsCursor": null, "data": []}
            }),
            None,
            "t1".to_string(),
            1,
            None,
        ));

        open_message_action(&mut state, "work".to_string(), "t1".to_string());

        let Mode::Compose(compose) = &state.mode else {
            panic!("expected compose mode");
        };
        assert_eq!(compose.send_mode, SendMode::Stream);
        assert!(compose.return_to_detail);
        let compose = compose.clone();

        handle_compose_input(
            KeyEvent::new(KeyCode::Tab, KeyModifiers::empty()),
            &mut state,
            compose,
            &test_target(),
            true,
            &app_tx,
        )
        .await
        .unwrap();

        let Mode::Compose(compose) = &state.mode else {
            panic!("expected compose mode");
        };
        assert_eq!(compose.send_mode, SendMode::NoWait);
    }

    #[test]
    fn preview_toggle_updates_pref_state() {
        let mut state = TuiState::new(TuiInit {
            query: None,
            since: None,
            cwd: None,
            archived: false,
            limit: 50,
            sort: None,
            descending: true,
            prefs: TuiPrefs::default(),
        });
        assert!(state.prefs.browser.preview_pane);
        state.prefs.browser.preview_pane = !state.prefs.browser.preview_pane;
        assert!(!state.prefs.browser.preview_pane);
    }

    fn mouse_wheel(kind: MouseEventKind) -> MouseEvent {
        MouseEvent {
            kind,
            column: 0,
            row: 0,
            modifiers: KeyModifiers::empty(),
        }
    }

    fn test_targets_named(servers: &[&str]) -> TuiTargets {
        TuiTargets::new(
            servers
                .iter()
                .map(|server| Target {
                    server: server.to_string(),
                    endpoint: crate::config::Endpoint::Unix {
                        path: std::path::PathBuf::from(format!("/tmp/{server}.sock")),
                    },
                    model: None,
                    model_reasoning_effort: None,
                })
                .collect(),
        )
        .expect("targets")
    }

    fn plain_key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::empty())
    }

    #[test]
    fn new_session_flow_prefills_from_selected_row_and_opens_compose() {
        let mut state = TuiState::new(TuiInit {
            query: None,
            since: None,
            cwd: None,
            archived: false,
            limit: 50,
            sort: None,
            descending: true,
            prefs: TuiPrefs::default(),
        });
        let mut row = test_thread_row("t1", "idle");
        row.cwd = "/repos/project".to_string();
        state.browser.rows = vec![row];
        state.browser.selected = 0;

        start_new_session_flow(&mut state, &test_targets_named(&["work"]));
        let Mode::NewSessionCwdInput { draft } = &state.mode else {
            panic!("expected cwd input, got {:?}", state.mode);
        };
        assert_eq!(draft.server, "work");
        assert_eq!(draft.cwd, "/repos/project");

        // Accept the prefilled cwd, then provide a title.
        let Mode::NewSessionCwdInput { draft } = std::mem::replace(&mut state.mode, Mode::Browser)
        else {
            unreachable!();
        };
        handle_new_session_cwd_input(plain_key(KeyCode::Enter), &mut state, draft);
        let Mode::NewSessionTitleInput { mut draft } =
            std::mem::replace(&mut state.mode, Mode::Browser)
        else {
            panic!("expected title input");
        };
        draft.title.push_str("My session");
        handle_new_session_title_input(plain_key(KeyCode::Enter), &mut state, draft);
        let Mode::Compose(compose) = &state.mode else {
            panic!("expected compose, got {:?}", state.mode);
        };
        assert_eq!(
            compose.target,
            ComposeTarget::NewThread {
                server: "work".to_string(),
                cwd: "/repos/project".to_string(),
                title: Some("My session".to_string()),
            }
        );
        assert!(!compose.return_to_detail);
    }

    #[test]
    fn new_session_flow_rejects_empty_cwd_and_allows_empty_title() {
        let mut state = TuiState::new(TuiInit {
            query: None,
            since: None,
            cwd: None,
            archived: false,
            limit: 50,
            sort: None,
            descending: true,
            prefs: TuiPrefs::default(),
        });
        let draft = NewSessionDraft {
            server: "work".to_string(),
            cwd: "   ".to_string(),
            title: String::new(),
        };
        handle_new_session_cwd_input(plain_key(KeyCode::Enter), &mut state, draft);
        assert!(
            matches!(state.mode, Mode::NewSessionCwdInput { .. }),
            "empty cwd must stay on the cwd prompt"
        );

        let draft = NewSessionDraft {
            server: "work".to_string(),
            cwd: "/repos/project".to_string(),
            title: String::new(),
        };
        handle_new_session_title_input(plain_key(KeyCode::Enter), &mut state, draft);
        let Mode::Compose(compose) = &state.mode else {
            panic!("expected compose");
        };
        assert_eq!(
            compose.target,
            ComposeTarget::NewThread {
                server: "work".to_string(),
                cwd: "/repos/project".to_string(),
                title: None,
            }
        );
    }

    #[test]
    fn new_session_flow_offers_server_menu_for_multi_server() {
        let mut state = TuiState::new(TuiInit {
            query: None,
            since: None,
            cwd: None,
            archived: false,
            limit: 50,
            sort: None,
            descending: true,
            prefs: TuiPrefs::default(),
        });
        let mut row = test_thread_row("t1", "idle");
        row.server = "second".to_string();
        row.cwd = "/repos/project".to_string();
        state.browser.rows = vec![row];
        state.browser.selected = 0;

        start_new_session_flow(&mut state, &test_targets_named(&["first", "second"]));
        let Mode::NewSessionServerMenu {
            draft,
            servers,
            selected,
        } = &state.mode
        else {
            panic!("expected server menu, got {:?}", state.mode);
        };
        assert_eq!(servers, &["first".to_string(), "second".to_string()]);
        assert_eq!(
            *selected, 1,
            "selection defaults to the selected row's server"
        );
        assert_eq!(draft.server, "second");
    }

    #[test]
    fn session_created_inserts_row_seeds_stream_and_preview() {
        let mut state = TuiState::new(TuiInit {
            query: None,
            since: None,
            cwd: None,
            archived: false,
            limit: 50,
            sort: None,
            descending: true,
            prefs: TuiPrefs::default(),
        });
        state.browser.rows = vec![test_thread_row("existing", "idle")];
        state.browser.selected = 0;

        handle_app_event(
            AppEvent::SessionCreated {
                stream_id: 9,
                server: "work".to_string(),
                thread_id: "thread_new".to_string(),
                cwd: "/repos/project".to_string(),
                title: Some("Fresh session".to_string()),
                prompt: "hello there".to_string(),
            },
            &mut state,
        );

        assert_eq!(state.browser.rows.len(), 2);
        assert_eq!(state.browser.rows[0].id, "thread_new");
        assert_eq!(state.browser.rows[0].title, "Fresh session");
        assert_eq!(state.browser.rows[0].cwd, "/repos/project");
        assert_eq!(state.browser.selected, 0);
        let stream = state.stream.as_ref().expect("stream");
        assert_eq!(stream.id, 9);
        assert_eq!(stream.thread_id, "thread_new");
        assert_eq!(
            state.browser.preview.thread_id.as_deref(),
            Some("thread_new")
        );
        assert_eq!(state.browser.preview.messages.len(), 1);
        assert_eq!(state.browser.preview.messages[0].role, "user");
        assert_eq!(
            message_text(&state.browser.preview.messages[0]),
            "hello there"
        );
    }

    fn test_thread_row(id: &str, status: &str) -> ThreadRow {
        ThreadRow {
            server: "work".to_string(),
            id: id.to_string(),
            title: id.to_string(),
            status: status.to_string(),
            updated: String::new(),
            cwd: String::new(),
            annotation: None,
            snippet: None,
            raw: serde_json::json!({}),
        }
    }

    fn test_thread_row_with_updated(id: &str, server: &str, updated_at: i64) -> ThreadRow {
        let mut row = test_thread_row(id, "idle");
        row.server = server.to_string();
        row.raw = serde_json::json!({
            "id": id,
            "updatedAt": updated_at,
            "status": {"type": "idle"}
        });
        row
    }
}
