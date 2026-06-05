mod events;
#[cfg(feature = "tui-syntax-highlighting")]
mod highlight;
mod input;
mod keymap;
mod prefs;
mod state;
mod views;

use std::io::{self, IsTerminal};
use std::panic;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use crossterm::cursor::Show;
use crossterm::event::{
    DisableMouseCapture, EnableMouseCapture, Event, EventStream, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers, MouseEvent, MouseEventKind,
};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use futures_util::StreamExt;
use pulldown_cmark::{CodeBlockKind, Event as MarkdownEvent, Options, Parser, Tag, TagEnd};
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
use crate::tui::events::{AppEvent, BrowserQuery, DetailPageDirection, FetchRequest};
use crate::tui::input::{InputAction, ModeKind};
use crate::tui::prefs::{SortDirectionPref, load_prefs_with_warning, save_prefs};
use crate::tui::state::{
    BrowserSource, ComposeState, ComposeTarget, DetailState, MessageBlock, MessageLine,
    MessageLineKind, MessageSpan, Mode, SendMode, StreamState, StreamStatus, ThreadRow, TuiInit,
    TuiState,
};
use crate::turns::{
    AttachTurnOptions, ControlledTurnWaitOptions, TurnControl, TurnStartOptions, TurnWaitOutcome,
    attach_turn, interrupt_turn, start_turn as start_turn_request, steer_turn,
    wait_for_turn_controlled,
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
    state.browser.last_error = loaded_prefs.warning;

    let _guard = TerminalGuard::enter()?;
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend)?;
    terminal.clear()?;

    let (fetch_tx, fetch_rx) = mpsc::channel(32);
    let (app_tx, mut app_rx) = mpsc::unbounded_channel();
    tokio::spawn(fetch_worker(target.clone(), fetch_rx, app_tx.clone()));
    spawn_shutdown_signal_task(app_tx.clone());
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

    state.prefs.refresh.auto = state.browser.auto_refresh;
    state.prefs.refresh.interval_seconds = state.browser.auto_refresh_seconds;
    state.prefs.browser.sort = state.browser.sort;
    state.prefs.browser.direction = if state.browser.descending {
        SortDirectionPref::Desc
    } else {
        SortDirectionPref::Asc
    };
    save_prefs(&state.prefs)?;
    terminal.clear()?;
    Ok(0)
}

type PanicHook = Box<dyn Fn(&panic::PanicHookInfo<'_>) + Sync + Send + 'static>;

struct TerminalGuard {
    previous_hook: Arc<Mutex<Option<PanicHook>>>,
}

impl TerminalGuard {
    fn enter() -> Result<Self> {
        enable_raw_mode()?;
        execute!(io::stdout(), EnterAlternateScreen, EnableMouseCapture)?;
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

fn restore_terminal() {
    let _ = disable_raw_mode();
    let _ = execute!(
        io::stdout(),
        DisableMouseCapture,
        Show,
        LeaveAlternateScreen
    );
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
    target: Target,
    mut fetch_rx: mpsc::Receiver<FetchRequest>,
    app_tx: mpsc::UnboundedSender<AppEvent>,
) {
    let mut client = match RpcClient::connect(&target.endpoint).await {
        Ok(client) => client,
        Err(err) => {
            while let Some(request) = fetch_rx.recv().await {
                match request {
                    FetchRequest::Browser { epoch, .. } => {
                        let _ = app_tx.send(AppEvent::BrowserLoadFailed {
                            epoch,
                            error: err.to_string(),
                        });
                    }
                    FetchRequest::Detail { epoch, .. } => {
                        let _ = app_tx.send(AppEvent::DetailLoadFailed {
                            epoch,
                            error: err.to_string(),
                        });
                    }
                }
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
            FetchRequest::Detail {
                epoch,
                thread_id,
                cursor,
                page_direction,
            } => {
                let result = fetch_detail(&target, &mut client, thread_id, cursor, epoch).await;
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
    cursor: Option<String>,
    epoch: u64,
) -> Result<DetailState> {
    let output = read_thread_detail(
        target,
        client,
        ShowThreadRequest {
            thread_id: thread_id.clone(),
            last: DETAIL_TURN_LIMIT,
            cursor: cursor.clone(),
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
    Ok(detail_state(output, status, thread_id, epoch, cursor))
}

async fn schedule_browser_refresh(
    state: &mut TuiState,
    fetch_tx: &mpsc::Sender<FetchRequest>,
) -> Result<()> {
    schedule_browser_page(state, fetch_tx, state.browser.current_cursor.clone()).await
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
    schedule_detail_page(
        state,
        fetch_tx,
        thread_id,
        None,
        DetailPageDirection::Replace,
    )
    .await
}

async fn schedule_detail_refresh(
    state: &mut TuiState,
    fetch_tx: &mpsc::Sender<FetchRequest>,
    thread_id: String,
) -> Result<()> {
    let cursor = state
        .detail
        .as_ref()
        .filter(|detail| detail.thread_id == thread_id)
        .and_then(|detail| detail.current_cursor.clone());
    schedule_detail_page(
        state,
        fetch_tx,
        thread_id,
        cursor,
        DetailPageDirection::Replace,
    )
    .await
}

async fn schedule_detail_page(
    state: &mut TuiState,
    fetch_tx: &mpsc::Sender<FetchRequest>,
    thread_id: String,
    cursor: Option<String>,
    page_direction: DetailPageDirection,
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
        messages: Vec::new(),
        scroll: 0,
        search_query: String::new(),
        matches: Vec::new(),
        match_index: 0,
        next_cursor: None,
        backwards_cursor: None,
        current_cursor: cursor.clone(),
        active_turn_id: None,
        loading: true,
        epoch,
        last_error: None,
    });
    state.mode = Mode::Detail;
    fetch_tx
        .send(FetchRequest::Detail {
            epoch,
            thread_id,
            cursor,
            page_direction,
        })
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
    let key = match event {
        Event::Key(key) => key,
        Event::Mouse(mouse) => {
            handle_mouse_event(mouse, state);
            return Ok(());
        }
        _ => return Ok(()),
    };
    if key.kind != KeyEventKind::Press {
        return Ok(());
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
                thread_id: stream.thread_id.clone(),
                turn_id,
            };
        } else {
            state.should_quit = true;
        }
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
        Mode::AnnotationInput {
            thread_id,
            draft,
            return_to_detail,
        } => {
            return handle_annotation_input(key, target, state, thread_id, draft, return_to_detail);
        }
        Mode::Compose(compose) => {
            return handle_compose_input(key, state, compose.clone(), target, yolo, app_tx).await;
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
            return Ok(());
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
            return Ok(());
        }
        Mode::ColumnsMenu => {
            match key.code {
                KeyCode::Esc => state.mode = Mode::Browser,
                KeyCode::Char('1') => {
                    state.prefs.browser.columns.status = !state.prefs.browser.columns.status
                }
                KeyCode::Char('2') => {
                    state.prefs.browser.columns.updated = !state.prefs.browser.columns.updated
                }
                KeyCode::Char('3') => {
                    state.prefs.browser.columns.cwd = !state.prefs.browser.columns.cwd
                }
                KeyCode::Char('4') => {
                    state.prefs.browser.columns.annotation = !state.prefs.browser.columns.annotation
                }
                _ => {}
            }
            if !matches!(key.code, KeyCode::Esc) {
                state.mode = Mode::ColumnsMenu;
            }
            let _ = save_prefs(&state.prefs);
            return Ok(());
        }
        Mode::ActiveTurnPrompt { thread_id, turn_id } => {
            match key.code {
                KeyCode::Esc => state.mode = Mode::Detail,
                KeyCode::Enter | KeyCode::Char('T') | KeyCode::Char('t') => {
                    let (control_tx, control_rx) = mpsc::unbounded_channel();
                    state.stream = Some(StreamState {
                        thread_id: thread_id.clone(),
                        turn_id: Some(turn_id.clone()),
                        status: StreamStatus::Running,
                        accumulated_text: String::new(),
                        events: Vec::new(),
                        attached: true,
                        detached: false,
                        last_error: None,
                        last_poll_at: None,
                    });
                    state.stream_control = Some(control_tx);
                    state.mode = Mode::Detail;
                    spawn_attach_task(
                        target.clone(),
                        thread_id,
                        turn_id,
                        yolo,
                        control_rx,
                        app_tx.clone(),
                    );
                }
                KeyCode::Char('s') | KeyCode::Char('S') => {
                    state.mode = Mode::Compose(ComposeState {
                        target: ComposeTarget::Steer { thread_id, turn_id },
                        text: String::new(),
                        send_mode: SendMode::NoWait,
                    });
                }
                KeyCode::Char('i') => {
                    state.mode = Mode::ConfirmInterrupt { thread_id, turn_id };
                }
                _ => state.mode = Mode::ActiveTurnPrompt { thread_id, turn_id },
            }
            return Ok(());
        }
        Mode::ConfirmInterrupt { thread_id, turn_id } => {
            match key.code {
                KeyCode::Esc => state.mode = Mode::Detail,
                KeyCode::Enter => {
                    state.mode = Mode::Detail;
                    if let Some(control) = &state.stream_control {
                        let _ = control.send(TurnControl::Interrupt);
                    } else {
                        spawn_interrupt_task(target.clone(), thread_id, turn_id, app_tx.clone());
                    }
                }
                _ => state.mode = Mode::ConfirmInterrupt { thread_id, turn_id },
            }
            return Ok(());
        }
        Mode::Help => {
            state.mode = Mode::Browser;
            return Ok(());
        }
        other => state.mode = other,
    }

    if matches!(
        key.code,
        KeyCode::Char('g') | KeyCode::Char('G') | KeyCode::Home | KeyCode::End
    ) {
        handle_goto_key(key.code, state);
        return Ok(());
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
                if let Some(thread_id) =
                    state.detail.as_ref().map(|detail| detail.thread_id.clone())
                {
                    schedule_detail_refresh(state, fetch_tx, thread_id).await?;
                }
            }
            _ => schedule_browser_refresh(state, fetch_tx).await?,
        },
        KeyCode::Char('R') => match state.mode {
            Mode::Detail => {
                if let Some(thread_id) =
                    state.detail.as_ref().map(|detail| detail.thread_id.clone())
                {
                    schedule_detail_load(state, fetch_tx, thread_id).await?;
                }
            }
            _ => schedule_browser_reset(state, fetch_tx).await?,
        },
        KeyCode::Char(']') => match state.mode {
            Mode::Detail => {
                if let Some((thread_id, cursor)) = state.detail.as_ref().and_then(|detail| {
                    detail
                        .next_cursor
                        .clone()
                        .map(|cursor| (detail.thread_id.clone(), cursor))
                }) {
                    schedule_detail_page(
                        state,
                        fetch_tx,
                        thread_id,
                        Some(cursor),
                        DetailPageDirection::Older,
                    )
                    .await?;
                }
            }
            _ => {
                if let Some(cursor) = state.browser.next_cursor.clone() {
                    schedule_browser_page(state, fetch_tx, Some(cursor)).await?;
                }
            }
        },
        KeyCode::Char('[') => match state.mode {
            Mode::Detail => {
                if let Some((thread_id, cursor)) = state.detail.as_ref().and_then(|detail| {
                    detail
                        .backwards_cursor
                        .clone()
                        .map(|cursor| (detail.thread_id.clone(), cursor))
                }) {
                    schedule_detail_page(
                        state,
                        fetch_tx,
                        thread_id,
                        Some(cursor),
                        DetailPageDirection::Newer,
                    )
                    .await?;
                }
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
        KeyCode::Char('N') if matches!(state.mode, Mode::Detail) => {
            state.previous_message_match();
        }
        KeyCode::Char('A') => {
            if let Some(thread_id) = active_thread_id(state) {
                let draft = active_annotation(state).unwrap_or_default();
                let return_to_detail = matches!(state.mode, Mode::Detail);
                state.mode = Mode::AnnotationInput {
                    thread_id,
                    draft,
                    return_to_detail,
                };
            }
        }
        KeyCode::Char('m') => {
            if let Some(thread_id) = active_thread_id(state) {
                if let Some(detail) = &state.detail
                    && detail.thread_id == thread_id
                    && let Some(turn_id) = detail.active_turn_id.clone()
                {
                    state.mode = Mode::ActiveTurnPrompt { thread_id, turn_id };
                } else {
                    state.mode = Mode::Compose(ComposeState {
                        target: ComposeTarget::NewTurn { thread_id },
                        text: String::new(),
                        send_mode: SendMode::Stream,
                    });
                }
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
        KeyCode::Char('T') => {
            if let Some(detail) = &state.detail
                && let Some(turn_id) = detail.active_turn_id.clone()
            {
                let (control_tx, control_rx) = mpsc::unbounded_channel();
                state.stream = Some(StreamState {
                    thread_id: detail.thread_id.clone(),
                    turn_id: Some(turn_id.clone()),
                    status: StreamStatus::Running,
                    accumulated_text: String::new(),
                    events: Vec::new(),
                    attached: true,
                    detached: false,
                    last_error: None,
                    last_poll_at: None,
                });
                state.stream_control = Some(control_tx);
                spawn_attach_task(
                    target.clone(),
                    detail.thread_id.clone(),
                    turn_id,
                    yolo,
                    control_rx,
                    app_tx.clone(),
                );
            }
        }
        KeyCode::Char('i') => {
            if let Some(detail) = &state.detail
                && let Some(turn_id) = detail.active_turn_id.clone()
            {
                state.mode = Mode::ConfirmInterrupt {
                    thread_id: detail.thread_id.clone(),
                    turn_id,
                };
            }
        }
        KeyCode::Char('f') if matches!(state.mode, Mode::Browser) => state.mode = Mode::FilterMenu,
        KeyCode::Char('c') if matches!(state.mode, Mode::Browser) => state.mode = Mode::ColumnsMenu,
        KeyCode::Char('t') => {
            state.browser.auto_refresh = !state.browser.auto_refresh;
            state.prefs.refresh.auto = state.browser.auto_refresh;
            let _ = save_prefs(&state.prefs);
        }
        KeyCode::Char('p') if matches!(state.mode, Mode::Browser) => {
            state.prefs.browser.preview_pane = !state.prefs.browser.preview_pane;
            let _ = save_prefs(&state.prefs);
        }
        KeyCode::Char('s') if matches!(state.mode, Mode::Browser) => state.mode = Mode::SortMenu,
        KeyCode::Down | KeyCode::Char('j') => match state.mode {
            Mode::Detail => scroll_detail(state, 1),
            _ => state.move_selection(1),
        },
        KeyCode::Up | KeyCode::Char('k') => match state.mode {
            Mode::Detail => scroll_detail(state, -1),
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
            Mode::Detail => {
                if stream_is_running(state) {
                    detach_stream(state);
                } else if state
                    .detail
                    .as_ref()
                    .is_some_and(|detail| !detail.search_query.is_empty())
                {
                    state.update_message_search(String::new());
                } else {
                    state.mode = Mode::Browser;
                }
            }
            _ => state.mode = Mode::Browser,
        },
        _ => {}
    }
    Ok(())
}

fn handle_goto_key(code: KeyCode, state: &mut TuiState) -> bool {
    match code {
        KeyCode::Char('g') => {
            if state.pending_goto_top {
                jump_to_top(state);
                state.pending_goto_top = false;
            } else {
                state.pending_goto_top = true;
            }
            true
        }
        KeyCode::Char('G') | KeyCode::End => {
            jump_to_bottom(state);
            state.pending_goto_top = false;
            true
        }
        KeyCode::Home => {
            jump_to_top(state);
            state.pending_goto_top = false;
            true
        }
        _ => {
            state.pending_goto_top = false;
            false
        }
    }
}

fn jump_to_top(state: &mut TuiState) {
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
    match state.mode {
        Mode::Detail => {
            if let Some(detail) = &mut state.detail {
                detail.scroll = detail
                    .transcript_line_count()
                    .saturating_sub(1)
                    .min(u16::MAX as usize) as u16;
            }
        }
        _ => {
            state.browser.selected = state.browser.rows.len().saturating_sub(1);
        }
    }
}

fn handle_mouse_event(mouse: MouseEvent, state: &mut TuiState) {
    state.pending_goto_top = false;
    let delta: isize = match mouse.kind {
        MouseEventKind::ScrollUp => -3,
        MouseEventKind::ScrollDown => 3,
        _ => return,
    };
    match state.mode {
        Mode::Detail
        | Mode::MessageSearchInput { .. }
        | Mode::Compose(_)
        | Mode::ActiveTurnPrompt { .. }
        | Mode::ConfirmInterrupt { .. } => scroll_detail(state, delta),
        Mode::Browser
        | Mode::SearchInput { .. }
        | Mode::FilterMenu
        | Mode::SortMenu
        | Mode::ColumnsMenu
        | Mode::Help => state.move_selection(delta),
        Mode::AnnotationInput {
            return_to_detail: true,
            ..
        } => scroll_detail(state, delta),
        Mode::AnnotationInput {
            return_to_detail: false,
            ..
        } => state.move_selection(delta),
    }
}

fn scroll_detail(state: &mut TuiState, delta: isize) {
    let Some(detail) = &mut state.detail else {
        return;
    };
    if delta.is_negative() {
        detail.scroll = detail.scroll.saturating_sub(delta.unsigned_abs() as u16);
    } else {
        detail.scroll = detail.scroll.saturating_add(delta as u16);
    }
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
        KeyCode::Char('s') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            save_annotation_draft(target, state, &thread_id, draft)?;
            state.mode = return_mode.clone();
        }
        KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            clear_annotation(target, &thread_id)?;
            set_annotation_in_state(state, &thread_id, None);
            state.mode = return_mode.clone();
        }
        KeyCode::Backspace => {
            draft.pop();
            state.mode = Mode::AnnotationInput {
                thread_id,
                draft,
                return_to_detail,
            };
        }
        KeyCode::Enter => {
            draft.push('\n');
            state.mode = Mode::AnnotationInput {
                thread_id,
                draft,
                return_to_detail,
            };
        }
        KeyCode::Char(ch) => {
            draft.push(ch);
            state.mode = Mode::AnnotationInput {
                thread_id,
                draft,
                return_to_detail,
            };
        }
        _ => {
            state.mode = Mode::AnnotationInput {
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
        set_annotation_in_state(state, thread_id, None);
    } else {
        set_annotation(target, thread_id, &value)?;
        set_annotation_in_state(state, thread_id, Some(value));
    }
    Ok(())
}

fn set_annotation_in_state(state: &mut TuiState, thread_id: &str, value: Option<String>) {
    if let Some(row) = state
        .browser
        .rows
        .iter_mut()
        .find(|row| row.id == thread_id)
    {
        row.annotation = value.clone();
    }
    if let Some(detail) = &mut state.detail
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
    let return_mode = if state.detail.is_some() {
        Mode::Detail
    } else {
        Mode::Browser
    };
    match key.code {
        KeyCode::Esc => state.mode = return_mode.clone(),
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
        KeyCode::Enter if key.modifiers.contains(KeyModifiers::SHIFT) => {
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
        ComposeTarget::NewTurn { thread_id } => {
            let (control_tx, control_rx) = mpsc::unbounded_channel();
            state.stream = Some(StreamState {
                thread_id: thread_id.clone(),
                turn_id: None,
                status: StreamStatus::Starting,
                accumulated_text: String::new(),
                events: Vec::new(),
                attached: false,
                detached: false,
                last_error: None,
                last_poll_at: None,
            });
            state.stream_control = Some(control_tx);
            append_detail_message(
                state,
                thread_id.as_str(),
                None,
                "user",
                Some("draft sent".to_string()),
                &prompt,
            );
            state.mode = return_mode;
            spawn_send_task(
                target.clone(),
                thread_id,
                prompt,
                compose.send_mode,
                yolo,
                control_rx,
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

fn spawn_attach_task(
    target: Target,
    thread_id: String,
    turn_id: String,
    yolo: bool,
    control_rx: mpsc::UnboundedReceiver<TurnControl>,
    app_tx: mpsc::UnboundedSender<AppEvent>,
) {
    tokio::spawn(async move {
        let result: Result<StreamStatus> = async {
            let mut client = RpcClient::connect(&target.endpoint).await?;
            let tx = app_tx.clone();
            match attach_turn(
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
    control_rx: mpsc::UnboundedReceiver<TurnControl>,
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
            match wait_for_turn_controlled(
                &target,
                &mut client,
                started,
                ControlledTurnWaitOptions {
                    poll_limit: TURN_SCAN_LIMIT,
                    timeout: Duration::from_secs(TURN_WAIT_TIMEOUT_SECS),
                    unsubscribe_on_detach: false,
                },
                control_rx,
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
        } => state.set_browser_rows(
            epoch,
            rows,
            next_cursor,
            backwards_cursor,
            state.browser.current_cursor.clone(),
        ),
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
        AppEvent::StreamEvent(event) => {
            if state.stream.is_none()
                && let Some(thread_id) = event["threadId"].as_str()
            {
                state.stream = Some(StreamState {
                    thread_id: thread_id.to_string(),
                    turn_id: event["turnId"].as_str().map(str::to_string),
                    status: StreamStatus::Running,
                    accumulated_text: String::new(),
                    events: Vec::new(),
                    attached: event["type"].as_str() == Some("attached"),
                    detached: false,
                    last_error: None,
                    last_poll_at: None,
                });
            }
            let mut pending_turn_id = None;
            let mut assistant_text = None;
            if let Some(stream) = &mut state.stream {
                if let Some(turn_id) = event["turnId"].as_str() {
                    stream.turn_id = Some(turn_id.to_string());
                    pending_turn_id = Some(turn_id.to_string());
                }
                if let Some(delta) = event["delta"].as_str() {
                    stream.accumulated_text.push_str(delta);
                    assistant_text = Some(stream.accumulated_text.clone());
                } else if let Some(text) = event["text"].as_str() {
                    stream.accumulated_text = text.to_string();
                    assistant_text = Some(stream.accumulated_text.clone());
                } else if let Some(text) = event["finalAssistantText"].as_str()
                    && !text.is_empty()
                {
                    stream.accumulated_text = text.to_string();
                    assistant_text = Some(stream.accumulated_text.clone());
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
                stream.events.push(event);
            }
            if let Some(turn_id) = pending_turn_id {
                fill_pending_turn_ids(state, &turn_id);
            }
            if let Some(text) = assistant_text {
                upsert_streaming_assistant_message(state, &text);
            }
        }
        AppEvent::StreamFailed(error) => {
            if let Some(stream) = &mut state.stream {
                stream.status = StreamStatus::Failed;
                stream.last_error = Some(error);
            }
            state.stream_control = None;
        }
        AppEvent::StreamFinished(status) => {
            if let Some(stream) = &mut state.stream {
                stream.status = status;
                if status == StreamStatus::Detached {
                    stream.detached = true;
                }
            }
            state.stream_control = None;
        }
        AppEvent::ShutdownSignal => {
            detach_stream(state);
            state.should_quit = true;
        }
    }
}

fn stream_is_running(state: &TuiState) -> bool {
    state.stream.as_ref().is_some_and(|stream| {
        matches!(
            stream.status,
            StreamStatus::Starting | StreamStatus::Running
        )
    })
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
    if !detail.search_query.is_empty() {
        let query = detail.search_query.clone();
        state.update_message_search(query);
    }
}

fn fill_pending_turn_ids(state: &mut TuiState, turn_id: &str) {
    let Some(detail) = &mut state.detail else {
        return;
    };
    for message in &mut detail.messages {
        if message.turn_id.is_none() {
            message.turn_id = Some(turn_id.to_string());
        }
    }
}

fn upsert_streaming_assistant_message(state: &mut TuiState, text: &str) {
    let Some(stream) = &state.stream else {
        return;
    };
    let Some(detail) = &mut state.detail else {
        return;
    };
    if detail.thread_id != stream.thread_id {
        return;
    }
    let turn_id = stream.turn_id.clone();
    if let Some(message) = detail
        .messages
        .iter_mut()
        .rev()
        .find(|message| message.role == "assistant" && message.turn_id == turn_id)
    {
        message.lines = markdown_lines(text, 100);
    } else {
        detail.messages.push(message_block(
            turn_id,
            None,
            "assistant",
            Some("streaming".to_string()),
            text,
            100,
        ));
    }
    if !detail.search_query.is_empty() {
        let query = detail.search_query.clone();
        state.update_message_search(query);
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
        last_error: None,
    }
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

    use crate::tui::prefs::TuiPrefs;

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
            None,
        );
        assert!(state.browser.rows.is_empty());
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
                    {"id": "turn-2", "items": [
                        {"id": "middle", "type": "agentMessage", "text": "middle"}
                    ]},
                    {"id": "turn-1", "items": [
                        {"id": "old", "type": "agentMessage", "text": "old"}
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
        assert_eq!(detail.messages[0].item_id.as_deref(), Some("middle"));
        assert_eq!(detail.messages[1].item_id.as_deref(), Some("old"));

        let newer = detail_state(
            serde_json::json!({
                "thread": {"id": "t1", "name": "Thread", "status": {"type": "idle"}},
                "turns": {"nextCursor": "older", "backwardsCursor": null, "data": [
                    {"id": "turn-3", "items": [
                        {"id": "new", "type": "agentMessage", "text": "new"}
                    ]},
                    {"id": "turn-2", "items": [
                        {"id": "middle", "type": "agentMessage", "text": "middle"}
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
        assert_eq!(detail.messages[0].item_id.as_deref(), Some("new"));
        assert_eq!(detail.messages[1].item_id.as_deref(), Some("middle"));
        assert_eq!(detail.messages[2].item_id.as_deref(), Some("old"));
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
            Some("draft sent".to_string()),
            "please stream",
        );

        handle_app_event(
            AppEvent::StreamEvent(serde_json::json!({
                "type": "delta",
                "threadId": "t1",
                "turnId": "turn-1",
                "delta": "first prune"
            })),
            &mut state,
        );
        handle_app_event(
            AppEvent::StreamEvent(serde_json::json!({
                "type": "delta",
                "threadId": "t1",
                "turnId": "turn-1",
                "delta": " chunk"
            })),
            &mut state,
        );

        state.update_message_search("prune".to_string());
        let detail = state.detail.as_ref().expect("detail");
        assert_eq!(detail.messages.len(), 2);
        assert_eq!(detail.messages[0].turn_id.as_deref(), Some("turn-1"));
        assert_eq!(detail.messages[1].role, "assistant");
        assert_eq!(detail.messages[1].lines[0].text, "first prune chunk");
        assert_eq!(detail.matches, vec![1]);
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
        let (control_tx, mut control_rx) = mpsc::unbounded_channel();
        state.stream_control = Some(control_tx);
        state.stream = Some(StreamState {
            thread_id: "t1".to_string(),
            turn_id: Some("turn-1".to_string()),
            status: StreamStatus::Running,
            accumulated_text: String::new(),
            events: Vec::new(),
            attached: true,
            detached: false,
            last_error: None,
            last_poll_at: None,
        });

        detach_stream(&mut state);
        let stream = state.stream.as_ref().expect("stream");
        assert_eq!(stream.status, StreamStatus::Detached);
        assert!(stream.detached);
        assert!(state.stream_control.is_none());
        assert!(matches!(control_rx.try_recv(), Ok(TurnControl::Detach)));

        handle_app_event(
            AppEvent::StreamFinished(StreamStatus::Completed),
            &mut state,
        );
        assert_eq!(
            state.stream.as_ref().expect("stream").status,
            StreamStatus::Completed
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
        state.stream = Some(StreamState {
            thread_id: "t1".to_string(),
            turn_id: Some("turn-1".to_string()),
            status: StreamStatus::Running,
            accumulated_text: String::new(),
            events: Vec::new(),
            attached: true,
            detached: false,
            last_error: None,
            last_poll_at: None,
        });

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

        set_annotation_in_state(&mut state, "t1", Some("note".to_string()));
        assert_eq!(state.browser.rows[0].annotation.as_deref(), Some("note"));
        assert_eq!(
            state.detail.as_ref().unwrap().annotation.as_deref(),
            Some("note")
        );
        set_annotation_in_state(&mut state, "t1", None);
        assert!(state.browser.rows[0].annotation.is_none());
        assert!(state.detail.as_ref().unwrap().annotation.is_none());
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

    #[test]
    fn vim_goto_shortcuts_jump_browser_and_detail() {
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
        handle_goto_key(KeyCode::Char('G'), &mut state);
        assert_eq!(state.browser.selected, 3);
        handle_goto_key(KeyCode::Char('g'), &mut state);
        assert!(state.pending_goto_top);
        handle_goto_key(KeyCode::Char('g'), &mut state);
        assert_eq!(state.browser.selected, 0);
        assert!(!state.pending_goto_top);

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
        handle_goto_key(KeyCode::Char('G'), &mut state);
        assert!(state.detail.as_ref().unwrap().scroll > 0);
        handle_goto_key(KeyCode::Home, &mut state);
        assert_eq!(state.detail.as_ref().unwrap().scroll, 0);
    }

    #[tokio::test]
    async fn compose_enter_submits_and_shift_enter_inserts_newline() {
        let target = Target {
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
            KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT),
            &mut state,
            ComposeState {
                target: ComposeTarget::NewTurn {
                    thread_id: "t1".to_string(),
                },
                text: "hello".to_string(),
                send_mode: SendMode::NoWait,
            },
            &target,
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
                    thread_id: "t1".to_string(),
                },
                text: "send me".to_string(),
                send_mode: SendMode::NoWait,
            },
            &target,
            true,
            &app_tx,
        )
        .await
        .unwrap();
        assert!(matches!(state.mode, Mode::Browser));
        assert!(state.stream.is_some());
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
}
