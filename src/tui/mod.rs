mod prefs;
mod state;
mod views;

use std::io::{self, IsTerminal};
use std::panic;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use crossterm::event::{Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use futures_util::StreamExt;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use serde_json::Value;
use tokio::sync::mpsc;

use crate::annotations::{clear_annotation, set_annotation};
use crate::cli::{ItemsView, SortKey, TuiCommand};
use crate::config::Target;
use crate::errors::usage_error;
use crate::rpc::RpcClient;
use crate::session::{
    ListThreadsRequest, SearchThreadsRequest, ShowThreadRequest, ThreadStatusRequest, list_threads,
    read_thread_detail, search_threads, thread_status,
};
use crate::tui::prefs::{TuiPrefs, load_prefs, save_prefs};
use crate::tui::state::{
    BrowserSource, ComposeState, ComposeTarget, DetailState, MessageLine, Mode, SendMode,
    StreamState, StreamStatus, ThreadRow, TuiInit, TuiState,
};
use crate::turns::{
    TurnStartOptions, TurnWaitOutcome, interrupt_turn, start_turn as start_turn_request,
    steer_turn, wait_for_turn,
};

const DEFAULT_LIMIT: u32 = 50;
const DETAIL_TURN_LIMIT: u32 = 80;
const TURN_SCAN_LIMIT: u32 = 200;
const TURN_WAIT_TIMEOUT_SECS: u64 = 60 * 60;

pub async fn run_tui(target: Target, command: TuiCommand, yolo: bool) -> Result<i32> {
    if !io::stdout().is_terminal() {
        return Err(usage_error("tui requires an interactive terminal"));
    }

    let since = command.since.as_deref().map(parse_since).transpose()?;
    let limit = command.limit.unwrap_or(DEFAULT_LIMIT);
    let prefs = load_prefs();
    let descending = if command.asc {
        false
    } else if command.desc {
        true
    } else {
        prefs.sort_descending
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

    let _guard = TerminalGuard::enter()?;
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend)?;
    terminal.clear()?;

    let (fetch_tx, fetch_rx) = mpsc::channel(32);
    let (app_tx, mut app_rx) = mpsc::unbounded_channel();
    tokio::spawn(fetch_worker(target.clone(), fetch_rx, app_tx.clone()));
    schedule_browser_refresh(&mut state, &fetch_tx).await?;

    let mut events = EventStream::new();
    let mut tick = tokio::time::interval(Duration::from_millis(250));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        terminal.draw(|frame| views::draw(frame, &state))?;
        if state.should_quit {
            break;
        }

        tokio::select! {
            maybe_event = events.next() => {
                if let Some(Ok(event)) = maybe_event {
                    handle_terminal_event(event, &mut state, &target, yolo, &fetch_tx, &app_tx).await?;
                }
            }
            Some(event) = app_rx.recv() => {
                handle_app_event(event, &mut state);
            }
            _ = tick.tick() => {
                if state.browser.auto_refresh
                    && !state.browser.loading
                    && state.browser.last_refresh_at.is_none_or(|last| {
                        last.elapsed() >= Duration::from_secs(state.browser.auto_refresh_seconds)
                    })
                {
                    schedule_browser_refresh(&mut state, &fetch_tx).await?;
                }
            }
        }
    }

    state.prefs.auto_refresh = state.browser.auto_refresh;
    state.prefs.auto_refresh_seconds = state.browser.auto_refresh_seconds;
    state.prefs.sort_key = state.browser.sort;
    state.prefs.sort_descending = state.browser.descending;
    save_prefs(&state.prefs)?;
    terminal.clear()?;
    Ok(0)
}

type PanicHook = Box<dyn Fn(&panic::PanicHookInfo<'_>) + Sync + Send + 'static>;

struct TerminalGuard {
    previous_hook: Option<PanicHook>,
}

impl TerminalGuard {
    fn enter() -> Result<Self> {
        enable_raw_mode()?;
        execute!(io::stdout(), EnterAlternateScreen)?;
        let previous_hook = panic::take_hook();
        panic::set_hook(Box::new(|info| {
            let _ = disable_raw_mode();
            let _ = execute!(io::stdout(), LeaveAlternateScreen);
            eprintln!("{info}");
        }));
        Ok(Self {
            previous_hook: Some(previous_hook),
        })
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
        if let Some(previous_hook) = self.previous_hook.take() {
            panic::set_hook(previous_hook);
        }
    }
}

#[derive(Debug, Clone)]
struct BrowserQuery {
    source: BrowserSource,
    query: String,
    limit: u32,
    since: Option<i64>,
    cwd: Option<String>,
    archived: bool,
    sort: Option<SortKey>,
    descending: bool,
}

#[derive(Debug)]
enum FetchRequest {
    Browser { epoch: u64, query: BrowserQuery },
    Detail { epoch: u64, thread_id: String },
}

#[derive(Debug)]
enum AppEvent {
    BrowserLoaded {
        epoch: u64,
        rows: Vec<ThreadRow>,
        next_cursor: Option<String>,
        backwards_cursor: Option<String>,
    },
    BrowserLoadFailed {
        epoch: u64,
        error: String,
    },
    DetailLoaded {
        epoch: u64,
        detail: Box<DetailState>,
    },
    DetailLoadFailed {
        epoch: u64,
        error: String,
    },
    StreamEvent(Value),
    StreamFailed(String),
    StreamFinished(StreamStatus),
}

async fn fetch_worker(
    target: Target,
    mut fetch_rx: mpsc::Receiver<FetchRequest>,
    app_tx: mpsc::UnboundedSender<AppEvent>,
) {
    let mut client = match RpcClient::connect(&target.endpoint).await {
        Ok(client) => client,
        Err(err) => {
            while let Some(request) = fetch_rx.recv().await {
                let epoch = match request {
                    FetchRequest::Browser { epoch, .. } | FetchRequest::Detail { epoch, .. } => {
                        epoch
                    }
                };
                let _ = app_tx.send(AppEvent::BrowserLoadFailed {
                    epoch,
                    error: err.to_string(),
                });
            }
            return;
        }
    };

    while let Some(request) = fetch_rx.recv().await {
        match request {
            FetchRequest::Browser { epoch, query } => {
                let result = fetch_browser(&target, &mut client, query).await;
                match result {
                    Ok((rows, next_cursor, backwards_cursor)) => {
                        let _ = app_tx.send(AppEvent::BrowserLoaded {
                            epoch,
                            rows,
                            next_cursor,
                            backwards_cursor,
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
            FetchRequest::Detail { epoch, thread_id } => {
                let result = fetch_detail(&target, &mut client, thread_id, epoch).await;
                match result {
                    Ok(detail) => {
                        let _ = app_tx.send(AppEvent::DetailLoaded {
                            epoch,
                            detail: Box::new(detail),
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
        }
    }
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
                    cursor: None,
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
                    cursor: None,
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
        .map(|item| thread_row(item, query.source))
        .collect();
    Ok((
        rows,
        output["nextCursor"].as_str().map(str::to_string),
        output["backwardsCursor"].as_str().map(str::to_string),
    ))
}

async fn fetch_detail(
    target: &Target,
    client: &mut RpcClient,
    thread_id: String,
    epoch: u64,
) -> Result<DetailState> {
    let output = read_thread_detail(
        target,
        client,
        ShowThreadRequest {
            thread_id: thread_id.clone(),
            last: DETAIL_TURN_LIMIT,
            cursor: None,
            asc: true,
            desc: false,
            items: ItemsView::Full,
        },
    )
    .await?;
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
    Ok(detail_state(output, status, thread_id, epoch))
}

async fn schedule_browser_refresh(
    state: &mut TuiState,
    fetch_tx: &mpsc::Sender<FetchRequest>,
) -> Result<()> {
    state.browser.epoch += 1;
    state.browser.loading = true;
    state.browser.last_error = None;
    let query = BrowserQuery {
        source: state.browser.source,
        query: state.browser.query.clone(),
        limit: state.browser.limit,
        since: state.browser.since,
        cwd: state.browser.cwd.clone(),
        archived: state.browser.archived,
        sort: state.browser.sort,
        descending: state.browser.descending,
    };
    fetch_tx
        .send(FetchRequest::Browser {
            epoch: state.browser.epoch,
            query,
        })
        .await
        .context("failed to schedule browser refresh")
}

async fn schedule_detail_load(
    state: &mut TuiState,
    fetch_tx: &mpsc::Sender<FetchRequest>,
    thread_id: String,
) -> Result<()> {
    let epoch = state
        .detail
        .as_ref()
        .map(|detail| detail.epoch + 1)
        .unwrap_or(1);
    state.detail = Some(DetailState {
        thread_id: thread_id.clone(),
        title: thread_id.clone(),
        status: "loading".to_string(),
        annotation: None,
        lines: Vec::new(),
        scroll: 0,
        search_query: String::new(),
        matches: Vec::new(),
        match_index: 0,
        next_cursor: None,
        backwards_cursor: None,
        active_turn_id: None,
        loading: true,
        epoch,
        last_error: None,
    });
    state.mode = Mode::Detail;
    fetch_tx
        .send(FetchRequest::Detail { epoch, thread_id })
        .await
        .context("failed to schedule detail load")
}

async fn handle_terminal_event(
    event: Event,
    state: &mut TuiState,
    target: &Target,
    yolo: bool,
    fetch_tx: &mpsc::Sender<FetchRequest>,
    app_tx: &mpsc::UnboundedSender<AppEvent>,
) -> Result<()> {
    let Event::Key(key) = event else {
        return Ok(());
    };
    if key.kind != KeyEventKind::Press {
        return Ok(());
    }
    if key.modifiers.contains(KeyModifiers::CONTROL) && matches!(key.code, KeyCode::Char('c')) {
        state.should_quit = true;
        return Ok(());
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
            .await;
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
            .await;
        }
        Mode::AnnotationInput { thread_id, draft } => {
            return handle_text_input(
                key,
                draft,
                ModeKind::Annotation {
                    thread_id: thread_id.clone(),
                },
                move |value, state| {
                    if value.trim().is_empty() {
                        clear_annotation(target, &thread_id)?;
                    } else {
                        set_annotation(target, &thread_id, &value)?;
                    }
                    if let Some(row) = state
                        .browser
                        .rows
                        .iter_mut()
                        .find(|row| row.id == thread_id)
                    {
                        row.annotation = if value.trim().is_empty() {
                            None
                        } else {
                            Some(value.clone())
                        };
                    }
                    if let Some(detail) = &mut state.detail
                        && detail.thread_id == thread_id
                    {
                        detail.annotation = if value.trim().is_empty() {
                            None
                        } else {
                            Some(value)
                        };
                    }
                    Ok(InputAction::None)
                },
                state,
                fetch_tx,
            )
            .await;
        }
        Mode::Compose(compose) => {
            return handle_compose_input(key, state, compose.clone(), target, yolo, app_tx).await;
        }
        Mode::Help => {
            state.mode = Mode::Browser;
            return Ok(());
        }
        other => state.mode = other,
    }

    match key.code {
        KeyCode::Char('q') => {
            if let Some(stream) = &mut state.stream
                && matches!(
                    stream.status,
                    StreamStatus::Starting | StreamStatus::Running
                )
            {
                stream.status = StreamStatus::Detached;
            }
            state.should_quit = true;
        }
        KeyCode::Char('?') => state.mode = Mode::Help,
        KeyCode::Char('r') => match state.mode {
            Mode::Detail => {
                if let Some(thread_id) =
                    state.detail.as_ref().map(|detail| detail.thread_id.clone())
                {
                    schedule_detail_load(state, fetch_tx, thread_id).await?;
                }
            }
            _ => schedule_browser_refresh(state, fetch_tx).await?,
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
        KeyCode::Char('A') => {
            if let Some(thread_id) = active_thread_id(state) {
                let draft = active_annotation(state).unwrap_or_default();
                state.mode = Mode::AnnotationInput { thread_id, draft };
            }
        }
        KeyCode::Char('e') => {
            if let Some(thread_id) = active_thread_id(state) {
                state.mode = Mode::Compose(ComposeState {
                    target: ComposeTarget::NewTurn { thread_id },
                    text: String::new(),
                    send_mode: SendMode::Stream,
                });
            }
        }
        KeyCode::Char('S') => {
            if let Some(detail) = &state.detail
                && let Some(turn_id) = detail.active_turn_id.clone()
            {
                state.mode = Mode::Compose(ComposeState {
                    target: ComposeTarget::Steer {
                        thread_id: detail.thread_id.clone(),
                        turn_id,
                    },
                    text: String::new(),
                    send_mode: SendMode::NoWait,
                });
            }
        }
        KeyCode::Char('i') => {
            if let Some(detail) = &state.detail
                && let Some(turn_id) = detail.active_turn_id.clone()
            {
                spawn_interrupt_task(
                    target.clone(),
                    detail.thread_id.clone(),
                    turn_id,
                    app_tx.clone(),
                );
            }
        }
        KeyCode::Char('a') => {
            state.browser.archived = !state.browser.archived;
            schedule_browser_refresh(state, fetch_tx).await?;
        }
        KeyCode::Char('c') => {
            cycle_columns(&mut state.prefs);
            let _ = save_prefs(&state.prefs);
        }
        KeyCode::Char('t') => {
            state.browser.auto_refresh = !state.browser.auto_refresh;
            let _ = save_prefs(&state.prefs);
        }
        KeyCode::Char('s') if state.browser.source == BrowserSource::List => {
            state.browser.sort = Some(match state.browser.sort.unwrap_or(SortKey::Updated) {
                SortKey::Updated => SortKey::Created,
                SortKey::Created => SortKey::Updated,
            });
            schedule_browser_refresh(state, fetch_tx).await?;
        }
        KeyCode::Char('d') if state.browser.source == BrowserSource::List => {
            state.browser.descending = !state.browser.descending;
            schedule_browser_refresh(state, fetch_tx).await?;
        }
        KeyCode::Down | KeyCode::Char('j') => match state.mode {
            Mode::Detail => {
                if let Some(detail) = &mut state.detail {
                    detail.scroll = detail.scroll.saturating_add(1);
                }
            }
            _ => state.move_selection(1),
        },
        KeyCode::Up | KeyCode::Char('k') => match state.mode {
            Mode::Detail => {
                if let Some(detail) = &mut state.detail {
                    detail.scroll = detail.scroll.saturating_sub(1);
                }
            }
            _ => state.move_selection(-1),
        },
        KeyCode::Enter => {
            if matches!(state.mode, Mode::Browser)
                && let Some(thread_id) = state.selected_thread_id().map(str::to_string)
            {
                schedule_detail_load(state, fetch_tx, thread_id).await?;
            }
        }
        KeyCode::Esc => match state.mode {
            Mode::Detail => state.mode = Mode::Browser,
            _ => state.mode = Mode::Browser,
        },
        _ => {}
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InputAction {
    None,
    RefreshBrowser,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ModeKind {
    Search,
    MessageSearch,
    Annotation { thread_id: String },
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
        KeyCode::Esc => state.mode = Mode::Browser,
        KeyCode::Enter => {
            let value = draft.clone();
            state.mode = Mode::Browser;
            if on_submit(value, state)? == InputAction::RefreshBrowser {
                schedule_browser_refresh(state, fetch_tx).await?;
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
        ModeKind::Annotation { thread_id } => Mode::AnnotationInput { thread_id, draft },
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
    match key.code {
        KeyCode::Esc => state.mode = Mode::Detail,
        KeyCode::Tab => {
            if matches!(compose.target, ComposeTarget::NewTurn { .. }) {
                compose.send_mode = match compose.send_mode {
                    SendMode::Stream => SendMode::NoWait,
                    SendMode::NoWait => SendMode::Stream,
                };
            }
            state.mode = Mode::Compose(compose);
        }
        KeyCode::Backspace => {
            compose.text.pop();
            state.mode = Mode::Compose(compose);
        }
        KeyCode::Enter => {
            let prompt = compose.text.trim().to_string();
            if prompt.is_empty() {
                state.mode = Mode::Detail;
                return Ok(());
            }
            match compose.target.clone() {
                ComposeTarget::NewTurn { thread_id } => {
                    state.stream = Some(StreamState {
                        thread_id: thread_id.clone(),
                        turn_id: None,
                        status: StreamStatus::Starting,
                        events: Vec::new(),
                        last_error: None,
                    });
                    state.mode = Mode::Detail;
                    spawn_send_task(
                        target.clone(),
                        thread_id,
                        prompt,
                        compose.send_mode,
                        yolo,
                        app_tx.clone(),
                    );
                }
                ComposeTarget::Steer { thread_id, turn_id } => {
                    state.mode = Mode::Detail;
                    spawn_steer_task(
                        target.clone(),
                        thread_id,
                        turn_id,
                        prompt,
                        yolo,
                        app_tx.clone(),
                    );
                }
            }
        }
        KeyCode::Char(ch) => {
            compose.text.push(ch);
            state.mode = Mode::Compose(compose);
        }
        _ => state.mode = Mode::Compose(compose),
    }
    Ok(())
}

fn spawn_steer_task(
    target: Target,
    thread_id: String,
    turn_id: String,
    prompt: String,
    yolo: bool,
    app_tx: mpsc::UnboundedSender<AppEvent>,
) {
    tokio::spawn(async move {
        let result: Result<Value> = async {
            let mut client = RpcClient::connect(&target.endpoint).await?;
            steer_turn(&target, &mut client, thread_id, turn_id, prompt, yolo).await
        }
        .await;
        match result {
            Ok(event) => app_tx.send(AppEvent::StreamEvent(event)).ok(),
            Err(err) => app_tx.send(AppEvent::StreamFailed(err.to_string())).ok(),
        };
    });
}

fn spawn_interrupt_task(
    target: Target,
    thread_id: String,
    turn_id: String,
    app_tx: mpsc::UnboundedSender<AppEvent>,
) {
    tokio::spawn(async move {
        let result: Result<Value> = async {
            let mut client = RpcClient::connect(&target.endpoint).await?;
            interrupt_turn(&target, &mut client, thread_id, turn_id).await
        }
        .await;
        match result {
            Ok(event) => app_tx.send(AppEvent::StreamEvent(event)).ok(),
            Err(err) => app_tx.send(AppEvent::StreamFailed(err.to_string())).ok(),
        };
    });
}

fn spawn_send_task(
    target: Target,
    thread_id: String,
    prompt: String,
    send_mode: SendMode,
    yolo: bool,
    app_tx: mpsc::UnboundedSender<AppEvent>,
) {
    tokio::spawn(async move {
        let result: Result<StreamStatus> = async {
            let mut client = RpcClient::connect(&target.endpoint).await?;
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
            app_tx
                .send(AppEvent::StreamEvent(started.acceptance.clone()))
                .ok();
            if send_mode == SendMode::NoWait {
                return Ok(StreamStatus::Detached);
            }
            let tx = app_tx.clone();
            match wait_for_turn(
                &target,
                &mut client,
                started,
                TURN_SCAN_LIMIT,
                Duration::from_secs(TURN_WAIT_TIMEOUT_SECS),
                |event| {
                    tx.send(AppEvent::StreamEvent(event.clone())).ok();
                    Ok(())
                },
                |_| Ok(()),
            )
            .await?
            {
                TurnWaitOutcome::Terminal(terminal) => {
                    tx.send(AppEvent::StreamEvent(terminal.output)).ok();
                    Ok(match terminal.exit_code {
                        0 => StreamStatus::Completed,
                        _ => StreamStatus::Failed,
                    })
                }
                TurnWaitOutcome::LocalInterrupt { .. } => Ok(StreamStatus::Detached),
            }
        }
        .await;
        match result {
            Ok(status) => app_tx.send(AppEvent::StreamFinished(status)).ok(),
            Err(err) => app_tx.send(AppEvent::StreamFailed(err.to_string())).ok(),
        };
    });
}

fn handle_app_event(event: AppEvent, state: &mut TuiState) {
    match event {
        AppEvent::BrowserLoaded {
            epoch,
            rows,
            next_cursor,
            backwards_cursor,
        } => state.set_browser_rows(epoch, rows, next_cursor, backwards_cursor),
        AppEvent::BrowserLoadFailed { epoch, error } => state.set_browser_error(epoch, error),
        AppEvent::DetailLoaded { epoch, detail } => state.set_detail(epoch, *detail),
        AppEvent::DetailLoadFailed { epoch, error } => state.set_detail_error(epoch, error),
        AppEvent::StreamEvent(event) => {
            if state.stream.is_none()
                && let Some(thread_id) = event["threadId"].as_str()
            {
                state.stream = Some(StreamState {
                    thread_id: thread_id.to_string(),
                    turn_id: event["turnId"].as_str().map(str::to_string),
                    status: StreamStatus::Running,
                    events: Vec::new(),
                    last_error: None,
                });
            }
            if let Some(stream) = &mut state.stream {
                if let Some(turn_id) = event["turnId"].as_str() {
                    stream.turn_id = Some(turn_id.to_string());
                }
                stream.status = match event["status"].as_str() {
                    Some("completed") => StreamStatus::Completed,
                    Some("failed") => StreamStatus::Failed,
                    Some("interrupted") => StreamStatus::Interrupted,
                    _ => StreamStatus::Running,
                };
                stream.events.push(event);
            }
        }
        AppEvent::StreamFailed(error) => {
            if let Some(stream) = &mut state.stream {
                stream.status = StreamStatus::Failed;
                stream.last_error = Some(error);
            }
        }
        AppEvent::StreamFinished(status) => {
            if let Some(stream) = &mut state.stream {
                stream.status = status;
            }
        }
    }
}

fn active_thread_id(state: &TuiState) -> Option<String> {
    match state.mode {
        Mode::Detail => state.detail.as_ref().map(|detail| detail.thread_id.clone()),
        _ => state.selected_thread_id().map(str::to_string),
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

fn cycle_columns(prefs: &mut TuiPrefs) {
    let columns = &mut prefs.visible_columns;
    if columns.annotation {
        columns.annotation = false;
    } else if columns.cwd {
        columns.cwd = false;
    } else {
        columns.cwd = true;
        columns.annotation = true;
    }
}

fn thread_row(item: Value, source: BrowserSource) -> ThreadRow {
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
    let status = thread["status"]["type"]
        .as_str()
        .or_else(|| thread["status"].as_str())
        .unwrap_or("")
        .to_string();
    let updated = thread["updatedAt"]
        .as_i64()
        .map(format_epoch)
        .unwrap_or_default();
    let cwd = thread["cwd"].as_str().unwrap_or("").to_string();
    let annotation = thread["annotation"]["text"].as_str().map(str::to_string);
    let snippet = match source {
        BrowserSource::Search => item["snippet"].as_str().map(str::to_string),
        BrowserSource::List => None,
    };
    ThreadRow {
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

fn detail_state(
    output: Value,
    status_output: Option<Value>,
    thread_id: String,
    epoch: u64,
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
    let mut lines = Vec::new();
    for turn in output["turns"]["data"].as_array().unwrap_or(&Vec::new()) {
        let turn_id = turn["id"].as_str().map(str::to_string);
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
                    push_wrapped_lines(&mut lines, turn_id.clone(), "user", &text, width);
                }
                Some("agentMessage") => {
                    push_wrapped_lines(
                        &mut lines,
                        turn_id.clone(),
                        "assistant",
                        item["text"].as_str().unwrap_or(""),
                        width,
                    );
                }
                _ => {}
            }
        }
    }
    DetailState {
        thread_id,
        title,
        status,
        annotation,
        lines,
        scroll: 0,
        search_query: String::new(),
        matches: Vec::new(),
        match_index: 0,
        next_cursor: output["turns"]["nextCursor"].as_str().map(str::to_string),
        backwards_cursor: output["turns"]["backwardsCursor"]
            .as_str()
            .map(str::to_string),
        active_turn_id: status_output
            .as_ref()
            .and_then(|value| value["activeTurnId"].as_str())
            .map(str::to_string),
        loading: false,
        epoch,
        last_error: None,
    }
}

fn push_wrapped_lines(
    lines: &mut Vec<MessageLine>,
    turn_id: Option<String>,
    role: &str,
    text: &str,
    width: usize,
) {
    if text.is_empty() {
        lines.push(MessageLine {
            turn_id,
            role: role.to_string(),
            text: String::new(),
            is_match: false,
        });
        return;
    }
    for raw_line in text.lines() {
        for wrapped in textwrap::wrap(raw_line, width) {
            lines.push(MessageLine {
                turn_id: turn_id.clone(),
                role: role.to_string(),
                text: wrapped.to_string(),
                is_match: false,
            });
        }
    }
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
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    Ok(now - seconds * multiplier)
}

#[cfg(test)]
mod tests {
    use super::*;

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
        );
        assert!(state.browser.rows.is_empty());
    }

    #[test]
    fn detail_state_flattens_wrapped_messages() {
        let detail = detail_state(
            serde_json::json!({
                "thread": {"id": "t1", "name": "Thread", "status": {"type": "idle"}},
                "turns": {"nextCursor": null, "backwardsCursor": null, "data": [
                    {"id": "turn-1", "items": [
                        {"type": "userMessage", "content": [{"text": "hello"}]},
                        {"type": "agentMessage", "text": "world"}
                    ]}
                ]}
            }),
            None,
            "t1".to_string(),
            1,
        );
        assert_eq!(detail.lines.len(), 2);
        assert_eq!(detail.lines[0].role, "user");
        assert_eq!(detail.lines[1].role, "assistant");
    }
}
