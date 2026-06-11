use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Clear, Paragraph, Row, Table, Wrap};

use crate::tui::keymap::{
    BROWSER_HELP, COMPOSE_HELP, DEFAULT_HELP, DETAIL_CONNECTED_HELP, DETAIL_HELP,
};
use crate::tui::state::{
    BrowserSource, ComposeState, ComposeTarget, Mode, SendMode, StreamStatus, TuiState,
    message_header_visible, transcript_rendered_line_count,
};
use crate::tui::state::{MessageColor, MessageLine, MessageLineKind, MessageSpan};

const BROWSER_COLUMN_SPACING: u16 = 2;

pub fn draw(frame: &mut Frame<'_>, state: &TuiState) {
    let area = frame.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(8),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .split(area);

    let detail_background = matches!(
        state.mode,
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
            }
    ) || (matches!(
        state.mode,
        Mode::AnnotationInput { .. } | Mode::RenameInput { .. }
    ) && state.detail.is_some());
    if detail_background {
        draw_detail(frame, chunks[0], state);
    } else {
        draw_browser(frame, chunks[0], state);
    }
    draw_status(frame, chunks[1], state);
    draw_help_bar(frame, chunks[2], state);

    match &state.mode {
        Mode::SearchInput { draft } => {
            draw_prompt(
                frame,
                area,
                "Search threads",
                draft,
                "Enter search, Ctrl-D clear, Esc cancel",
            );
        }
        Mode::MessageSearchInput { draft } => {
            draw_prompt(
                frame,
                area,
                "Search messages",
                draft,
                "Enter search, Ctrl-D clear, Esc cancel",
            );
        }
        Mode::AnnotationInput { draft, .. } => {
            draw_prompt(
                frame,
                area,
                "Annotation",
                draft,
                "Enter save, Ctrl-D clear, Esc cancel",
            );
        }
        Mode::RenameInput { draft, .. } => draw_prompt(
            frame,
            area,
            "Rename",
            draft,
            "Enter save, Ctrl-D clear draft, Esc cancel",
        ),
        Mode::FilterMenu => draw_filter_menu(frame, area, state),
        Mode::SortMenu => draw_sort_menu(frame, area, state),
        Mode::ColumnsMenu => draw_columns_menu(frame, area, state),
        Mode::ConfirmInterrupt {
            thread_id, turn_id, ..
        } => {
            draw_confirm_interrupt(frame, area, thread_id, turn_id.as_deref());
        }
        Mode::ConfirmArchive {
            thread_id,
            archived,
            ..
        } => {
            draw_confirm_archive(frame, area, thread_id, *archived);
        }
        Mode::ConfirmOpenCodex { thread_id, cwd, .. } => {
            draw_confirm_open_codex(frame, area, thread_id, cwd);
        }
        Mode::NewSessionServerMenu {
            servers, selected, ..
        } => {
            draw_new_session_server_menu(frame, area, servers, *selected);
        }
        Mode::NewSessionCwdInput { draft } => draw_prompt(
            frame,
            area,
            "New session cwd",
            &draft.cwd,
            "Enter continue, Ctrl-D clear, Esc cancel",
        ),
        Mode::NewSessionTitleInput { draft } => draw_prompt(
            frame,
            area,
            "New session name (optional)",
            &draft.title,
            "Enter continue, Ctrl-D clear, Esc cancel",
        ),
        Mode::Compose(compose) => {
            let label = match compose.target {
                ComposeTarget::Steer { .. } | ComposeTarget::SteerSelected { .. } => {
                    "Steer active turn"
                }
                ComposeTarget::NewThread { .. } => "New session first message",
                ComposeTarget::NewTurn { .. } => match compose.send_mode {
                    SendMode::Stream if compose_new_turn_can_steer(state, compose) => {
                        "Send new turn"
                    }
                    SendMode::Stream => "Compose stream",
                    SendMode::NoWait => "Compose no-wait",
                },
            };
            let footer = match compose.target {
                ComposeTarget::Steer { .. } | ComposeTarget::SteerSelected { .. } => {
                    "Enter steer, Ctrl-J newline, Tab send, Esc cancel"
                }
                ComposeTarget::NewThread { .. } => {
                    "Enter create session + send, Ctrl-J newline, Esc cancel"
                }
                ComposeTarget::NewTurn { .. } if compose_new_turn_can_steer(state, compose) => {
                    "Enter send, Ctrl-J newline, Tab steer, Esc cancel"
                }
                ComposeTarget::NewTurn { .. } => "Enter send, Ctrl-J newline, Tab mode, Esc cancel",
            };
            draw_compose(frame, area, label, &compose.text, footer);
        }
        Mode::Help => draw_help(frame, area),
        _ => {}
    }
}

fn compose_new_turn_can_steer(state: &TuiState, compose: &ComposeState) -> bool {
    let ComposeTarget::NewTurn { thread_id, .. } = &compose.target else {
        return false;
    };
    state.stream.as_ref().is_some_and(|stream| {
        stream.thread_id.as_str() == thread_id.as_str()
            && matches!(
                stream.status,
                StreamStatus::Starting | StreamStatus::Running
            )
    }) || state.detail.as_ref().is_some_and(|detail| {
        detail.thread_id.as_str() == thread_id.as_str() && detail.active_turn_id.is_some()
    }) || state
        .browser
        .rows
        .iter()
        .any(|row| row.id.as_str() == thread_id.as_str() && row.is_running())
}

pub fn sync_viewport_state(state: &mut TuiState, area: Rect) {
    let chunks = root_chunks(area);
    let (table_area, _) = browser_areas(state, chunks[0]);
    state
        .browser
        .clamp_row_offset(browser_visible_rows(table_area));
    if state.detail.is_none() {
        return;
    }
    let detail_chunks = detail_chunks(chunks[0]);
    if let Some(detail) = &mut state.detail {
        detail.set_viewport_size(
            detail_chunks[1].height.saturating_sub(2),
            detail_chunks[1].width.saturating_sub(2),
        );
    }
}

fn root_chunks(area: Rect) -> std::rc::Rc<[Rect]> {
    Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(8),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .split(area)
}

fn detail_chunks(area: Rect) -> std::rc::Rc<[Rect]> {
    Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Min(5)])
        .split(area)
}

fn browser_areas(state: &TuiState, area: Rect) -> (Rect, Option<Rect>) {
    if state.prefs.browser.preview_pane && area.height >= 16 {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
            .split(area);
        (chunks[0], Some(chunks[1]))
    } else {
        (area, None)
    }
}

/// Data rows that fit in the browser table: the area minus its two border
/// rows and the header row.
fn browser_visible_rows(table_area: Rect) -> usize {
    table_area.height.saturating_sub(3) as usize
}

fn draw_browser(frame: &mut Frame<'_>, area: Rect, state: &TuiState) {
    let (table_area, preview_area) = browser_areas(state, area);
    let visible = state.visible_columns();
    let mut header = vec![Cell::from("THREAD")];
    if state.browser.multi_server {
        header.push(Cell::from("SERVER"));
    }
    if visible.status {
        header.push(Cell::from("STATUS"));
    }
    if visible.updated {
        header.push(Cell::from("UPDATED"));
    }
    if visible.cwd {
        header.push(Cell::from("CWD"));
    }
    if visible.annotation {
        header.push(Cell::from("ANNOTATION"));
    }
    let widths = browser_column_widths(table_area.width, visible, state.browser.multi_server);

    let rows = state
        .browser
        .rows
        .iter()
        .enumerate()
        .skip(state.browser.row_offset)
        .take(browser_visible_rows(table_area))
        .map(|(index, row)| {
            let title = if let Some(snippet) = &row.snippet {
                format!("{}  {}", row.title, snippet)
            } else {
                row.title.clone()
            };
            let mut cells = vec![Cell::from(title)];
            if state.browser.multi_server {
                cells.push(Cell::from(row.server.clone()));
            }
            if visible.status {
                cells.push(Cell::from(browser_row_status(
                    state,
                    &row.server,
                    &row.id,
                    &row.status,
                )));
            }
            if visible.updated {
                cells.push(Cell::from(row.updated.clone()));
            }
            if visible.cwd {
                cells.push(Cell::from(compact_home_path(&row.cwd)));
            }
            if visible.annotation {
                cells.push(Cell::from(row.annotation.clone().unwrap_or_default()));
            }
            let style = if index == state.browser.selected {
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Cyan)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            Row::new(cells).style(style)
        });

    let title = match state.browser.source {
        BrowserSource::List => " Threads ",
        BrowserSource::Search => " Search ",
    };
    let table = Table::new(rows, widths)
        .header(
            Row::new(header).style(
                Style::default()
                    .fg(Color::Gray)
                    .add_modifier(Modifier::BOLD),
            ),
        )
        .block(Block::default().title(title).borders(Borders::ALL))
        .column_spacing(BROWSER_COLUMN_SPACING);
    frame.render_widget(table, table_area);
    if let Some(area) = preview_area {
        draw_browser_preview(frame, area, state);
    }
}

fn browser_row_status(state: &TuiState, server: &str, thread_id: &str, fallback: &str) -> String {
    let Some(stream) = &state.stream else {
        return fallback.to_string();
    };
    if stream.server != server || stream.thread_id != thread_id {
        return fallback.to_string();
    }
    match stream.status {
        StreamStatus::Starting | StreamStatus::Running if !stream.detached => {
            format!(
                "{} {}",
                live_spinner_frame(),
                format_stream_status(stream.status)
            )
        }
        StreamStatus::Starting | StreamStatus::Running => {
            format_stream_status(stream.status).to_string()
        }
        StreamStatus::Failed | StreamStatus::Interrupted => {
            format_stream_status(stream.status).to_string()
        }
        // The follow probe is not a live turn: show the thread's own status.
        StreamStatus::Following | StreamStatus::Completed | StreamStatus::Detached => {
            fallback.to_string()
        }
    }
}

fn browser_column_widths(
    table_width: u16,
    visible: &crate::tui::prefs::VisibleColumns,
    multi_server: bool,
) -> Vec<Constraint> {
    const TITLE_MAX: u16 = 44;
    const SERVER_WIDTH: u16 = 12;
    const CWD_MAX: u16 = 46;
    const ANNOTATION_MAX: u16 = 40;
    const STATUS_WIDTH: u16 = 11;
    const UPDATED_WIDTH: u16 = 22;

    let mut fixed_width = 0;
    let mut flexible_columns = vec![(0_u16, TITLE_MAX, 4_u16)];
    if multi_server {
        fixed_width += SERVER_WIDTH;
    }
    if visible.status {
        fixed_width += STATUS_WIDTH;
    }
    if visible.updated {
        fixed_width += UPDATED_WIDTH;
    }
    if visible.cwd {
        flexible_columns.push((1, CWD_MAX, 4));
    }
    if visible.annotation {
        flexible_columns.push((2, ANNOTATION_MAX, 3));
    }

    let column_count = 1
        + usize::from(multi_server)
        + usize::from(visible.status)
        + usize::from(visible.updated)
        + usize::from(visible.cwd)
        + usize::from(visible.annotation);
    let spacing = column_count.saturating_sub(1) as u16 * BROWSER_COLUMN_SPACING;
    let available = table_width
        .saturating_sub(2)
        .saturating_sub(spacing)
        .saturating_sub(fixed_width);
    let flexible_widths = allocate_flexible_widths(available, &flexible_columns);

    let mut widths = vec![Constraint::Length(flexible_widths[0])];
    if multi_server {
        widths.push(Constraint::Length(SERVER_WIDTH));
    }
    if visible.status {
        widths.push(Constraint::Length(STATUS_WIDTH));
    }
    if visible.updated {
        widths.push(Constraint::Length(UPDATED_WIDTH));
    }
    if visible.cwd {
        widths.push(Constraint::Length(flexible_widths[1]));
    }
    if visible.annotation {
        widths.push(Constraint::Length(flexible_widths[2]));
    }
    widths
}

fn allocate_flexible_widths(available: u16, columns: &[(u16, u16, u16)]) -> [u16; 3] {
    let mut widths = [0_u16; 3];
    let total_weight = columns.iter().map(|(_, _, weight)| *weight).sum::<u16>();
    for (index, max, weight) in columns {
        widths[*index as usize] = available
            .saturating_mul(*weight)
            .checked_div(total_weight.max(1))
            .unwrap_or(0)
            .min(*max);
    }
    widths
}

const LIVE_SPINNER_FRAMES: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

fn live_spinner_frame() -> char {
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as usize;
    LIVE_SPINNER_FRAMES[(millis / 120) % LIVE_SPINNER_FRAMES.len()]
}

fn live_indicator_span(trailing: &str) -> Span<'static> {
    Span::styled(
        format!("{} live{trailing}", live_spinner_frame()),
        Style::default()
            .fg(Color::Green)
            .add_modifier(Modifier::BOLD),
    )
}

/// "Live" means: this thread's event feed is subscribed (send- or
/// attach-originated) for a turn that is in progress. Stream tasks are
/// per-turn, so a non-detached Starting/Running stream implies an executing
/// (or imminently starting) turn; the post-turn `Following` probe and all
/// terminal states are explicitly not live.
fn stream_is_live_for(state: &TuiState, server: &str, thread_id: &str) -> bool {
    state.stream.as_ref().is_some_and(|stream| {
        stream.server == server
            && stream.thread_id == thread_id
            && !stream.detached
            && matches!(
                stream.status,
                StreamStatus::Starting | StreamStatus::Running
            )
    })
}

fn draw_browser_preview(frame: &mut Frame<'_>, area: Rect, state: &TuiState) {
    let Some(row) = state.browser.rows.get(state.browser.selected) else {
        frame.render_widget(
            Paragraph::new("No thread selected")
                .block(Block::default().title(" Preview ").borders(Borders::ALL)),
            area,
        );
        return;
    };
    let preview = &state.browser.preview;
    let (text, scroll) = if preview.thread_id.as_deref() != Some(row.id.as_str()) || preview.loading
    {
        (vec![Line::from("Loading recent messages...")], 0)
    } else if let Some(error) = &preview.error {
        (vec![Line::from(format!("Preview failed: {error}"))], 0)
    } else if !preview.messages.is_empty() {
        let lines = transcript_lines(&preview.messages);
        let viewport = area.height.saturating_sub(2) as usize;
        let width = area.width.saturating_sub(2).max(1) as usize;
        let rendered_lines = transcript_rendered_line_count(&preview.messages, width);
        let scroll = rendered_lines
            .saturating_sub(viewport)
            .min(u16::MAX as usize) as u16;
        (lines, scroll)
    } else if let Some(snippet) = &row.snippet {
        (vec![Line::from(snippet.clone())], 0)
    } else {
        (vec![Line::from("No message preview available")], 0)
    };
    let title = if stream_is_live_for(state, &row.server, &row.id) {
        Line::from(vec![
            Span::raw(" Recent Messages "),
            live_indicator_span(" "),
        ])
    } else {
        Line::from(" Recent Messages ")
    };
    frame.render_widget(
        Paragraph::new(text)
            .wrap(Wrap { trim: false })
            .scroll((scroll, 0))
            .block(Block::default().title(title).borders(Borders::ALL)),
        area,
    );
}

fn draw_detail(frame: &mut Frame<'_>, area: Rect, state: &TuiState) {
    let Some(detail) = &state.detail else {
        draw_browser(frame, area, state);
        return;
    };
    let chunks = detail_chunks(area);
    let detail_status = detail_header_status(state);
    let connection = detail_connection_label(state);
    let inner_width = chunks[0].width.saturating_sub(2) as usize;
    let mut used_width = detail_status.chars().count();
    let mut metadata_spans = Vec::new();
    if detail_has_connected_stream(state) {
        let live = live_indicator_span("  ");
        used_width += live.content.chars().count();
        metadata_spans.push(live);
    }
    metadata_spans.push(Span::raw(detail_status));
    if let Some(connection) = connection {
        metadata_spans.push(Span::raw("  "));
        metadata_spans.push(Span::raw(connection));
        used_width += 2 + connection.chars().count();
    }
    if let Some(annotation) =
        detail_header_annotation(detail.annotation.as_deref(), inner_width, used_width)
    {
        metadata_spans.push(Span::raw("  "));
        metadata_spans.push(Span::raw(annotation));
    }
    let metadata = vec![Line::from(metadata_spans)];
    frame.render_widget(
        Paragraph::new(metadata).block(
            Block::default()
                .title(detail.title.clone())
                .borders(Borders::ALL),
        ),
        chunks[0],
    );

    let lines = transcript_lines(&detail.messages);
    let scroll = detail.scroll.min(detail.max_scroll());
    frame.render_widget(
        Paragraph::new(lines)
            .block(Block::default().title(" Transcript ").borders(Borders::ALL))
            .scroll((scroll, 0))
            .wrap(Wrap { trim: false })
            .style(Style::default()),
        chunks[1],
    );
}

fn detail_header_annotation(
    annotation: Option<&str>,
    inner_width: usize,
    used_width: usize,
) -> Option<String> {
    let annotation = annotation?.trim();
    if annotation.is_empty() {
        return None;
    }
    let available = inner_width.checked_sub(used_width + 2)?;
    if available < "note: ".chars().count() {
        return None;
    }
    let text = annotation.split_whitespace().collect::<Vec<_>>().join(" ");
    Some(truncate_text(&format!("note: {text}"), available))
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

fn transcript_lines(messages: &[crate::tui::state::MessageBlock]) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let mut previous_turn_id: Option<&str> = None;
    let mut previous_role: Option<&str> = None;
    for message in messages {
        let role_style = match message.role.as_str() {
            "user" => Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
            "assistant" => Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
            _ => Style::default().fg(Color::Gray),
        };
        let header_style = if message.is_match {
            role_style.bg(Color::DarkGray)
        } else {
            role_style
        };
        let turn_id = message.turn_id.as_deref();
        let role = message.role.as_str();
        let show_header = message_header_visible(message, previous_turn_id, previous_role);
        if show_header {
            let show_timestamp = turn_id.is_none() || turn_id != previous_turn_id;
            lines.push(Line::from(Span::styled(
                message_header(message, show_timestamp),
                header_style,
            )));
        }
        previous_turn_id = turn_id;
        previous_role = Some(role);
        for line in &message.lines {
            let mut text_style = match line.kind {
                MessageLineKind::Text => Style::default(),
                MessageLineKind::Heading => Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
                MessageLineKind::Quote => Style::default().fg(Color::Gray),
                MessageLineKind::Code => Style::default().fg(Color::LightBlue),
            };
            if message.is_match {
                text_style = text_style.bg(Color::DarkGray);
            }
            lines.push(render_message_line(line, text_style));
        }
        lines.push(Line::from(""));
    }
    lines
}

fn detail_header_status(state: &TuiState) -> String {
    let Some(detail) = &state.detail else {
        return String::new();
    };
    if let Some(stream) = matching_detail_stream(state) {
        return format_stream_status(stream.status).to_string();
    }
    if detail.active_turn_id.is_some() {
        return "running".to_string();
    }
    detail.status.clone()
}

fn detail_connection_label(state: &TuiState) -> Option<&'static str> {
    let detail = state.detail.as_ref()?;
    if let Some(stream) = matching_detail_stream(state) {
        if stream.detached || stream.status == StreamStatus::Detached {
            return Some("detached");
        }
        if matches!(
            stream.status,
            StreamStatus::Starting | StreamStatus::Running
        ) {
            return Some("connected");
        }
        return None;
    }
    if detail.active_turn_id.is_some() {
        return Some("not connected");
    }
    None
}

fn matching_detail_stream(state: &TuiState) -> Option<&crate::tui::state::StreamState> {
    let detail = state.detail.as_ref()?;
    state
        .stream
        .as_ref()
        .filter(|stream| stream.thread_id == detail.thread_id)
}

fn message_header(message: &crate::tui::state::MessageBlock, show_timestamp: bool) -> String {
    let role = message.role.to_uppercase();
    if !show_timestamp {
        return role;
    }
    let timestamp = message.timestamp.as_deref().unwrap_or("");
    if timestamp.is_empty() {
        role
    } else {
        format!("{role} · {timestamp}")
    }
}

fn compact_home_path(path: &str) -> String {
    let Ok(home) = std::env::var("HOME") else {
        return path.to_string();
    };
    compact_path_with_home(path, &home)
}

fn compact_path_with_home(path: &str, home: &str) -> String {
    if home.is_empty() || home == "/" {
        return path.to_string();
    }
    if path == home {
        return "~".to_string();
    }
    let Some(rest) = path.strip_prefix(home) else {
        return path.to_string();
    };
    let Some(rest) = rest.strip_prefix('/') else {
        return path.to_string();
    };
    format!("~/{rest}")
}

fn render_message_line(line: &MessageLine, base_style: Style) -> Line<'static> {
    if line.spans.is_empty() {
        return Line::from(Span::styled(line.text.clone(), base_style));
    }
    Line::from(
        line.spans
            .iter()
            .map(|span| Span::styled(span.text.clone(), span_style(span, base_style)))
            .collect::<Vec<_>>(),
    )
}

fn span_style(span: &MessageSpan, base_style: Style) -> Style {
    let mut style = base_style;
    if let Some(MessageColor::Rgb(red, green, blue)) = span.color {
        style = style.fg(Color::Rgb(red, green, blue));
    }
    if span.bold {
        style = style.add_modifier(Modifier::BOLD);
    }
    if span.italic {
        style = style.add_modifier(Modifier::ITALIC);
    }
    style
}

fn draw_status(frame: &mut Frame<'_>, area: Rect, state: &TuiState) {
    let status = if draws_detail_background(state) {
        detail_status_bar(state)
    } else {
        browser_status_bar(state)
    };
    frame.render_widget(Paragraph::new(status), area);
}

fn draws_detail_background(state: &TuiState) -> bool {
    matches!(
        state.mode,
        Mode::Detail
            | Mode::MessageSearchInput { .. }
            | Mode::Compose(_)
            | Mode::ConfirmInterrupt { .. }
            | Mode::ConfirmArchive {
                return_to_detail: true,
                ..
            }
            | Mode::ConfirmOpenCodex {
                return_to_detail: true,
                ..
            }
    ) || (matches!(
        state.mode,
        Mode::AnnotationInput { .. } | Mode::RenameInput { .. }
    ) && state.detail.is_some())
}

fn browser_status_bar(state: &TuiState) -> String {
    let loading = if state.browser.loading {
        " loading"
    } else {
        ""
    };
    let query = if state.browser.query.is_empty() {
        String::new()
    } else {
        format!(" query={}", state.browser.query)
    };
    let local_cwd =
        if matches!(state.browser.source, BrowserSource::Search) && state.browser.cwd.is_some() {
            " cwd=local"
        } else {
            ""
        };
    let auto_refresh = if state.browser.auto_refresh {
        format!(" auto={}s", state.browser.auto_refresh_seconds)
    } else {
        String::new()
    };
    let stream = state.stream.as_ref().map(format_stream).unwrap_or_default();
    let error = state
        .browser
        .last_error
        .as_ref()
        .map(|error| format!(" error={error}"))
        .unwrap_or_default();
    let notice = notice_status(state);
    format!(
        "{} rows={}{}{}{}{}{}{}",
        match state.browser.source {
            BrowserSource::List => "list",
            BrowserSource::Search => "search",
        },
        state.browser.rows.len(),
        query,
        local_cwd,
        auto_refresh,
        loading,
        stream,
        error,
    ) + &notice
}

fn detail_status_bar(state: &TuiState) -> String {
    let notice = state
        .notice
        .as_ref()
        .map(|notice| format!(" {}", notice.message))
        .unwrap_or_default();
    let message_search = state
        .detail
        .as_ref()
        .filter(|detail| !detail.search_query.is_empty())
        .map(|detail| {
            if detail.matches.is_empty() {
                format!("message_search={} 0 matches", detail.search_query)
            } else {
                format!(
                    "message_search={} match={}/{}",
                    detail.search_query,
                    detail.match_index + 1,
                    detail.matches.len()
                )
            }
        })
        .unwrap_or_default();
    let error = state
        .detail
        .as_ref()
        .and_then(|detail| detail.last_error.as_ref())
        .map(|error| format!(" error={error}"))
        .unwrap_or_default();
    format!("{message_search}{error}{notice}")
}

fn notice_status(state: &TuiState) -> String {
    state
        .notice
        .as_ref()
        .map(|notice| format!(" {}", notice.message))
        .unwrap_or_default()
}

fn draw_help_bar(frame: &mut Frame<'_>, area: Rect, state: &TuiState) {
    let text = match state.mode {
        Mode::Browser => BROWSER_HELP,
        Mode::Detail if detail_has_connected_stream(state) => DETAIL_CONNECTED_HELP,
        Mode::Detail => DETAIL_HELP,
        Mode::Compose(_) => COMPOSE_HELP,
        _ => DEFAULT_HELP,
    };
    frame.render_widget(
        Paragraph::new(text).style(Style::default().fg(Color::Gray)),
        area,
    );
}

fn detail_has_connected_stream(state: &TuiState) -> bool {
    matching_detail_stream(state).is_some_and(|stream| {
        !stream.detached
            && matches!(
                stream.status,
                StreamStatus::Starting | StreamStatus::Running
            )
    })
}

fn draw_prompt(frame: &mut Frame<'_>, area: Rect, title: &str, value: &str, footer: &str) {
    let area = centered_rect(area, 70, 5);
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(value.to_string())
            .wrap(Wrap { trim: false })
            .block(
                Block::default()
                    .title(title)
                    .title_bottom(Line::from(Span::styled(
                        footer,
                        Style::default().fg(Color::Gray),
                    )))
                    .borders(Borders::ALL),
            ),
        area,
    );
}

fn draw_compose(frame: &mut Frame<'_>, area: Rect, title: &str, value: &str, footer: &str) {
    let panel_width = area.width.saturating_mul(80).checked_div(100).unwrap_or(0);
    let inner_width = panel_width.saturating_sub(4).max(1) as usize;
    let lines = compose_display_lines(value, inner_width);
    let desired_height = (lines.len() as u16).saturating_add(2);
    let max_height = area.height.saturating_sub(1).clamp(3, 18);
    let min_height = 6.min(max_height);
    let height = desired_height.min(max_height).max(min_height);
    let content_height = height.saturating_sub(2).max(1) as usize;
    let scroll = lines
        .len()
        .saturating_sub(content_height)
        .min(u16::MAX as usize) as u16;
    let area = centered_rect(area, 80, height);
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(lines).scroll((scroll, 0)).block(
            Block::default()
                .title(title)
                .title_bottom(Line::from(Span::styled(
                    footer,
                    Style::default().fg(Color::Gray),
                )))
                .borders(Borders::ALL),
        ),
        area,
    );
}

fn compose_display_lines(value: &str, width: usize) -> Vec<Line<'static>> {
    let width = width.max(1);
    if value.is_empty() {
        return vec![Line::from("")];
    }
    let mut lines = Vec::new();
    for raw in value.split('\n') {
        if raw.is_empty() {
            lines.push(Line::from(""));
            continue;
        }
        for wrapped in textwrap::wrap(raw, width) {
            lines.push(Line::from(wrapped.to_string()));
        }
    }
    lines
}

fn draw_filter_menu(frame: &mut Frame<'_>, area: Rect, state: &TuiState) {
    let value = if state.browser.archived { "on" } else { "off" };
    draw_static_modal(
        frame,
        area,
        "Filters",
        &[
            format!("archived: {value}"),
            "a toggle archived".to_string(),
            "Esc close".to_string(),
        ],
    );
}

fn draw_sort_menu(frame: &mut Frame<'_>, area: Rect, state: &TuiState) {
    let sort = state
        .browser
        .sort
        .map(|sort| format!("{sort:?}").to_lowercase())
        .unwrap_or_else(|| "updated".to_string());
    let direction = if state.browser.descending {
        "desc"
    } else {
        "asc"
    };
    let local_note = if matches!(state.browser.source, BrowserSource::Search) {
        "search sort disabled until app-server supports it"
    } else {
        "u updated  c created  d direction"
    };
    draw_static_modal(
        frame,
        area,
        "Sort",
        &[
            format!("sort: {sort} {direction}"),
            local_note.to_string(),
            "Esc close".to_string(),
        ],
    );
}

fn draw_new_session_server_menu(
    frame: &mut Frame<'_>,
    area: Rect,
    servers: &[String],
    selected: usize,
) {
    let mut lines: Vec<String> = servers
        .iter()
        .enumerate()
        .map(|(index, server)| {
            if index == selected {
                format!("> {server}")
            } else {
                format!("  {server}")
            }
        })
        .collect();
    lines.push("j/k move  Enter select  Esc cancel".to_string());
    draw_static_modal(frame, area, "New session server", &lines);
}

fn draw_columns_menu(frame: &mut Frame<'_>, area: Rect, state: &TuiState) {
    let columns = state.visible_columns();
    draw_static_modal(
        frame,
        area,
        "Columns",
        &[
            format!("1 status: {}", on_off(columns.status)),
            format!("2 updated: {}", on_off(columns.updated)),
            format!("3 cwd: {}", on_off(columns.cwd)),
            format!("4 annotation: {}", on_off(columns.annotation)),
            format!(
                "5 relative updated: {}",
                on_off(state.prefs.browser.relative_updated)
            ),
            format!("t auto-refresh: {}", on_off(state.browser.auto_refresh)),
            format!(
                "-/+ refresh interval: {}s",
                state.browser.auto_refresh_seconds
            ),
            format!(
                "a auto-attach live streams: {}",
                on_off(state.prefs.browser.auto_attach)
            ),
            "Esc close".to_string(),
        ],
    );
}

fn draw_confirm_interrupt(
    frame: &mut Frame<'_>,
    area: Rect,
    thread_id: &str,
    turn_id: Option<&str>,
) {
    let target = turn_id
        .map(|turn_id| format!("{turn_id} on {thread_id}"))
        .unwrap_or_else(|| format!("the active turn on {thread_id}"));
    draw_static_modal(
        frame,
        area,
        "Interrupt Turn",
        &[
            format!("Interrupt {target}?"),
            "Enter interrupt".to_string(),
            "Esc cancel".to_string(),
        ],
    );
}

fn draw_confirm_archive(frame: &mut Frame<'_>, area: Rect, thread_id: &str, archived: bool) {
    let verb = if archived { "Archive" } else { "Unarchive" };
    draw_static_modal(
        frame,
        area,
        &format!("{verb} Thread"),
        &[
            format!("{verb} {thread_id}?"),
            format!("Enter {}", verb.to_lowercase()),
            "Esc cancel".to_string(),
        ],
    );
}

fn draw_confirm_open_codex(frame: &mut Frame<'_>, area: Rect, thread_id: &str, cwd: &str) {
    draw_static_modal(
        frame,
        area,
        "Open In Codex",
        &[
            format!("Launch Codex TUI for {thread_id}?"),
            format!("cwd: {cwd}"),
            "Enter launch, Esc cancel".to_string(),
        ],
    );
}

fn draw_static_modal(frame: &mut Frame<'_>, area: Rect, title: &str, lines: &[String]) {
    let height = (lines.len() as u16 + 2).max(5);
    let area = centered_rect(area, 70, height);
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(lines.join("\n"))
            .wrap(Wrap { trim: false })
            .block(Block::default().title(title).borders(Borders::ALL)),
        area,
    );
}

fn on_off(value: bool) -> &'static str {
    if value { "on" } else { "off" }
}

fn draw_help(frame: &mut Frame<'_>, area: Rect) {
    let height = area.height.saturating_sub(2).clamp(18, 32);
    let area = centered_rect(area, 86, height);
    frame.render_widget(Clear, area);
    let items = [
        "Global",
        "  ? help  q quit  Ctrl-C quit or interrupt active stream",
        "  r refresh or poll active stream  R reload/reset  l load thread  y copy thread id",
        "  j/k, arrows, or mouse wheel move/scroll  gg/Home top  G/End bottom",
        "  Browser: [ previous page  ] next page",
        "",
        "Browser",
        "  Enter open detail  m compose message or steer active turn  o open in Codex TUI",
        "  n new session (server, cwd, optional name, first message)  / search threads",
        "  l load selected thread",
        "  i interrupt selected active turn",
        "  a annotate  e rename  A confirm archive/unarchive",
        "  f filters  s sort  c columns/time/refresh  p preview  t auto-refresh",
        "",
        "Detail",
        "  Esc browser/detach detail session  Enter or m compose message or steer",
        "  gg/Home real transcript start  G/End real transcript end",
        "  / search loaded transcript  n/N next/previous match",
        "  l load thread  o open in Codex TUI  a annotate  e rename  A confirm archive/unarchive",
        "  i interrupt",
        "",
        "Compose and Text Inputs",
        "  Compose: Enter submit  Ctrl-J newline  Tab steer/send or stream/no-wait  Esc cancel",
        "  Search: Enter apply  Ctrl-D clear  Esc cancel",
        "  Rename: Enter save  Ctrl-D clear draft  Esc cancel",
        "  Annotation: Enter save  Ctrl-D clear  Esc cancel",
        "",
        "Menus and Prompts",
        "  Filters: a toggle archived filter  Sort: u updated, c created, d direction",
        "  Columns: 1 status, 2 updated, 3 cwd, 4 annotation, 5 relative time",
        "  Columns: t auto-refresh, -/+ refresh interval, a auto-attach",
        "  Interrupt confirmation: Enter interrupt, Esc cancel",
        "  Archive confirmation: Enter archive/unarchive, Esc cancel",
        "  Open in Codex confirmation: Enter launch, Esc cancel",
    ];
    frame.render_widget(
        Paragraph::new(items.join("\n"))
            .block(Block::default().title(" Help ").borders(Borders::ALL)),
        area,
    );
}

fn centered_rect(area: Rect, percent_x: u16, height: u16) -> Rect {
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(1),
            Constraint::Length(height),
            Constraint::Min(1),
        ])
        .split(area);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(vertical[1])[1]
}

fn format_stream(state: &crate::tui::state::StreamState) -> String {
    let error = state
        .last_error
        .as_ref()
        .map(|error| format!(" error={error}"))
        .unwrap_or_default();
    format!(" stream={}{}", format_stream_state_label(state), error)
}

fn format_stream_state_label(state: &crate::tui::state::StreamState) -> &'static str {
    if state.detached {
        "detached"
    } else if state.attached {
        "attached"
    } else {
        format_stream_status(state.status)
    }
}

fn format_stream_status(status: StreamStatus) -> &'static str {
    match status {
        StreamStatus::Starting => "starting",
        StreamStatus::Running => "running",
        StreamStatus::Following => "following",
        StreamStatus::Completed => "completed",
        StreamStatus::Failed => "failed",
        StreamStatus::Interrupted => "interrupted",
        StreamStatus::Detached => "detached",
    }
}

#[cfg(test)]
mod tests {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use crate::tui::prefs::TuiPrefs;
    use crate::tui::state::{
        ComposeState, DetailState, MessageBlock, MessageLine, MessageLineKind, Mode, StreamState,
        ThreadRow, TuiInit, TuiState,
    };

    use super::*;

    fn numbered_thread_row(index: usize) -> ThreadRow {
        ThreadRow {
            server: "work".to_string(),
            id: format!("thread-{index:02}"),
            title: format!("Thread {index:02}"),
            status: "idle".to_string(),
            updated: "2026-06-05 09:30".to_string(),
            cwd: "/home/kevin/repo".to_string(),
            annotation: None,
            snippet: None,
            raw: serde_json::json!({}),
        }
    }

    #[test]
    fn browser_selection_stays_visible_when_preview_pane_shrinks_table() {
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
        state.browser.rows = (0..12).map(numbered_thread_row).collect();
        state.browser.selected = 9;

        // 18-row terminal: 16 rows for the browser, split 8/8 with the
        // preview pane, leaving 5 visible table rows after borders + header.
        let area = Rect::new(0, 0, 100, 18);
        sync_viewport_state(&mut state, area);
        let backend = TestBackend::new(100, 18);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw(frame, &state)).unwrap();
        let content = terminal.backend().buffer().content();
        let text = content.iter().map(|cell| cell.symbol()).collect::<String>();
        assert!(text.contains("Thread 09"), "selected row must stay visible");
        assert!(text.contains("Thread 05"));
        assert!(
            !text.contains("Thread 04"),
            "rows above window are scrolled out"
        );
        assert!(!text.contains("Thread 10"), "rows below window stay hidden");

        // Moving back above the window scrolls it up again.
        state.browser.selected = 2;
        sync_viewport_state(&mut state, area);
        terminal.draw(|frame| draw(frame, &state)).unwrap();
        let content = terminal.backend().buffer().content();
        let text = content.iter().map(|cell| cell.symbol()).collect::<String>();
        assert!(text.contains("Thread 02"));
        assert!(!text.contains("Thread 09"));
    }

    #[test]
    fn clamp_row_offset_tracks_selection_and_row_count() {
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
        state.browser.rows = (0..12).map(numbered_thread_row).collect();

        state.browser.selected = 9;
        state.browser.clamp_row_offset(5);
        assert_eq!(state.browser.row_offset, 5);

        // Scrolling within the window keeps the offset stable.
        state.browser.selected = 6;
        state.browser.clamp_row_offset(5);
        assert_eq!(state.browser.row_offset, 5);

        state.browser.selected = 1;
        state.browser.clamp_row_offset(5);
        assert_eq!(state.browser.row_offset, 1);

        // Shrinking the row list clamps a stale offset.
        state.browser.selected = 0;
        state.browser.row_offset = 10;
        state.browser.rows.truncate(6);
        state.browser.clamp_row_offset(5);
        assert_eq!(state.browser.row_offset, 0);

        state.browser.rows.clear();
        state.browser.row_offset = 3;
        state.browser.clamp_row_offset(5);
        assert_eq!(state.browser.row_offset, 0);
    }

    #[test]
    fn browser_render_includes_rows_and_annotation_column() {
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
            id: "thread-1".to_string(),
            title: "Fix tests".to_string(),
            status: "idle".to_string(),
            updated: "2026-06-05 09:30".to_string(),
            cwd: compact_path_with_home("/home/kevin/repo", "/home/kevin"),
            annotation: Some("needs review".to_string()),
            snippet: Some("recent assistant message".to_string()),
            raw: serde_json::json!({}),
        });
        state
            .browser
            .preview
            .thread_id
            .replace("thread-1".to_string());
        state.browser.preview.messages = vec![
            MessageBlock {
                turn_id: Some("turn-1".to_string()),
                item_id: Some("user-1".to_string()),
                role: "user".to_string(),
                timestamp: Some("2026-06-05 09:29".to_string()),
                raw_text: "recent user message".to_string(),
                lines: vec![MessageLine {
                    kind: MessageLineKind::Text,
                    text: "recent user message".to_string(),
                    spans: Vec::new(),
                }],
                is_match: false,
            },
            MessageBlock {
                turn_id: Some("turn-1".to_string()),
                item_id: Some("assistant-1".to_string()),
                role: "assistant".to_string(),
                timestamp: Some("2026-06-05 09:30".to_string()),
                raw_text: "recent assistant message".to_string(),
                lines: vec![MessageLine {
                    kind: MessageLineKind::Text,
                    text: "recent assistant message".to_string(),
                    spans: Vec::new(),
                }],
                is_match: false,
            },
        ];
        let backend = TestBackend::new(100, 18);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw(frame, &state)).unwrap();
        let content = terminal.backend().buffer().content();
        let text = content.iter().map(|cell| cell.symbol()).collect::<String>();
        assert!(text.contains("Fix tests"));
        assert!(text.contains("2026-06-05 09:30"));
        assert!(text.contains("~/repo"));
        assert!(text.contains("needs review"));
        assert!(text.contains("USER"));
        assert!(text.contains("recent user message"));
        assert!(text.contains("ASSISTANT"));
        assert!(text.contains("recent assistant message"));
    }

    #[test]
    fn browser_render_overlays_matching_stream_status() {
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
            id: "thread-1".to_string(),
            title: "Running task".to_string(),
            status: "idle".to_string(),
            updated: "2026-06-05 09:30".to_string(),
            cwd: "~/repo".to_string(),
            annotation: None,
            snippet: None,
            raw: serde_json::json!({}),
        });
        state.stream = Some(StreamState::new_with_id(
            1,
            "thread-1".to_string(),
            Some("turn-1".to_string()),
            StreamStatus::Running,
            false,
        ));

        let backend = TestBackend::new(100, 12);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw(frame, &state)).unwrap();
        let text = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();

        assert!(text.contains("Running task"));
        assert!(text.contains("running"));
    }

    #[test]
    fn footer_exposes_help_and_refresh_shortcuts() {
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
        let backend = TestBackend::new(140, 18);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw(frame, &state)).unwrap();
        let text = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();

        assert!(text.contains("? help"));
        assert!(text.contains("r/R refresh"));
        assert!(text.contains("l load"));
        assert!(text.contains("m msg/steer"));
        assert!(text.contains("i int"));
        assert!(text.contains("[] page"));
    }

    #[test]
    fn detail_footer_shows_connected_state_without_attach_hint() {
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
            thread_id: "thread-1".to_string(),
            title: "Thread".to_string(),
            status: "active".to_string(),
            annotation: None,
            messages: Vec::new(),
            scroll: 0,
            search_query: String::new(),
            matches: Vec::new(),
            match_index: 0,
            next_cursor: None,
            backwards_cursor: None,
            current_cursor: None,
            active_turn_id: Some("turn-1".to_string()),
            loading: false,
            epoch: 1,
            last_refresh_at: None,
            viewport_height: None,
            viewport_width: None,
            last_error: None,
        });
        state.stream = Some(StreamState::new_with_id(
            1,
            "thread-1".to_string(),
            Some("turn-1".to_string()),
            StreamStatus::Running,
            true,
        ));

        let backend = TestBackend::new(160, 18);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw(frame, &state)).unwrap();
        let text = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();

        assert!(text.contains("Esc detach/browser"));
        assert!(text.contains("r poll"));
        assert!(!text.contains("T/S/i"));
    }

    #[test]
    fn narrow_detail_follow_bottom_reaches_post_wrap_bottom() {
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
            thread_id: "thread-1".to_string(),
            title: "Thread".to_string(),
            status: "idle".to_string(),
            annotation: None,
            messages: vec![MessageBlock {
                turn_id: Some("turn-1".to_string()),
                item_id: Some("item-1".to_string()),
                role: "assistant".to_string(),
                timestamp: None,
                raw_text: String::new(),
                lines: (0..10)
                    .map(|index| MessageLine {
                        kind: MessageLineKind::Text,
                        text: if index == 9 {
                            "final wrapped assistant content includes TAIL_MARKER".to_string()
                        } else {
                            "wide assistant content that wraps again on a narrow terminal"
                                .to_string()
                        },
                        spans: Vec::new(),
                    })
                    .collect(),
                is_match: false,
            }],
            scroll: u16::MAX,
            search_query: String::new(),
            matches: Vec::new(),
            match_index: 0,
            next_cursor: None,
            backwards_cursor: None,
            current_cursor: None,
            active_turn_id: None,
            loading: false,
            epoch: 1,
            last_refresh_at: None,
            viewport_height: None,
            viewport_width: None,
            last_error: None,
        });

        let backend = TestBackend::new(60, 16);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                sync_viewport_state(&mut state, frame.area());
                if let Some(detail) = &mut state.detail {
                    detail.scroll = detail.bottom_scroll_position();
                }
                draw(frame, &state);
            })
            .unwrap();
        let text = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();

        assert!(text.contains("TAIL_MARKER"));
    }

    #[test]
    fn help_modal_lists_core_shortcuts_by_context() {
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
        state.mode = Mode::Help;

        let backend = TestBackend::new(140, 36);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw(frame, &state)).unwrap();
        let text = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();

        assert!(text.contains("Global"));
        assert!(text.contains("r refresh or poll active stream"));
        assert!(text.contains("l load thread"));
        assert!(text.contains("Browser: [ previous page"));
        assert!(text.contains("real transcript start"));
        assert!(text.contains("Browser"));
        assert!(text.contains("a annotate"));
        assert!(text.contains("Detail"));
        assert!(text.contains("n/N next/previous match"));
        assert!(text.contains("Compose and Text Inputs"));
        assert!(text.contains("Ctrl-J newline"));
        assert!(text.contains("Menus and Prompts"));
        assert!(text.contains("Columns: 1 status"));
    }

    #[test]
    fn compact_path_with_home_only_rewrites_matching_home_prefix() {
        assert_eq!(
            compact_path_with_home("/home/kevin/repo", "/home/kevin"),
            "~/repo"
        );
        assert_eq!(compact_path_with_home("/home/kevin", "/home/kevin"), "~");
        assert_eq!(
            compact_path_with_home("/home/kevin-other/repo", "/home/kevin"),
            "/home/kevin-other/repo"
        );
    }

    #[test]
    fn browser_columns_use_capped_widths_on_wide_terminals() {
        let prefs = TuiPrefs::default();

        assert_eq!(
            browser_column_widths(250, &prefs.browser.columns, false),
            vec![
                Constraint::Length(44),
                Constraint::Length(11),
                Constraint::Length(22),
                Constraint::Length(46),
                Constraint::Length(40),
            ]
        );
    }

    #[test]
    fn browser_draws_server_column_when_multiple_servers_are_visible() {
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
        state.browser.multi_server = true;
        state.browser.rows = vec![ThreadRow {
            server: "main".to_string(),
            id: "thread-1".to_string(),
            title: "Thread".to_string(),
            status: "idle".to_string(),
            updated: "now".to_string(),
            cwd: "~".to_string(),
            annotation: None,
            snippet: None,
            raw: serde_json::json!({}),
        }];

        let backend = TestBackend::new(140, 12);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw(frame, &state)).unwrap();
        let text = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();

        assert!(text.contains("SERVER"));
        assert!(text.contains("main"));
    }

    #[test]
    fn stream_status_omits_ids_and_duplicate_attachment_flags() {
        let mut stream = StreamState::new(
            "019e95bd-1b12-7c32-81de-89d02e9bcbfc".to_string(),
            Some("019e99e7-decc-7bb2-8c80-0c7f0a54d413".to_string()),
            StreamStatus::Detached,
            true,
        );
        stream.detached = true;
        stream.last_poll_at = Some(std::time::Instant::now());

        assert_eq!(format_stream(&stream), " stream=detached");
    }

    #[test]
    fn compose_panel_keeps_footer_separate_from_draft() {
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
                thread_id: "thread-1".to_string(),
            },
            text: "first line\nsecond line".to_string(),
            send_mode: SendMode::Stream,
            return_to_detail: true,
        });

        let backend = TestBackend::new(100, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw(frame, &state)).unwrap();
        let content = terminal.backend().buffer().content();
        let text = content.iter().map(|cell| cell.symbol()).collect::<String>();
        assert!(text.contains("first line"));
        assert!(text.contains("second line"));
        assert!(text.contains("Enter send, Ctrl-J newline, Tab mode, Esc cancel"));
    }

    #[test]
    fn annotation_panel_keeps_footer_on_bottom_border() {
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
            thread_id: "thread-1".to_string(),
            draft: "annotation text".to_string(),
            return_to_detail: false,
        };

        let backend = TestBackend::new(100, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw(frame, &state)).unwrap();
        let buffer = terminal.backend().buffer();
        let text = buffer
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(text.contains("annotation text"));
        assert!(text.contains("Enter save, Ctrl-D clear, Esc cancel"));
        assert!(
            buffer
                .content()
                .iter()
                .any(|cell| { cell.symbol() == "C" && cell.style().fg == Some(Color::Gray) })
        );
    }

    #[test]
    fn rename_panel_keeps_footer_on_bottom_border() {
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
        state.mode = Mode::RenameInput {
            server: "work".to_string(),
            thread_id: "thread-1".to_string(),
            draft: "New thread name".to_string(),
            return_to_detail: false,
        };

        let backend = TestBackend::new(100, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw(frame, &state)).unwrap();
        let buffer = terminal.backend().buffer();
        let text = buffer
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(text.contains("New thread name"));
        assert!(text.contains("Enter save, Ctrl-D clear draft, Esc cancel"));
        assert!(
            buffer
                .content()
                .iter()
                .any(|cell| { cell.symbol() == "E" && cell.style().fg == Some(Color::Gray) })
        );
    }

    #[test]
    fn compose_panel_scrolls_to_bottom_for_long_drafts() {
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
                thread_id: "thread-1".to_string(),
            },
            text: (1..=30)
                .map(|line| format!("draft line {line}"))
                .collect::<Vec<_>>()
                .join("\n"),
            send_mode: SendMode::Stream,
            return_to_detail: true,
        });

        let backend = TestBackend::new(100, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw(frame, &state)).unwrap();
        let content = terminal.backend().buffer().content();
        let text = content.iter().map(|cell| cell.symbol()).collect::<String>();
        assert!(text.contains("draft line 30"));
        assert!(!text.contains("draft line 1 "));
    }

    #[test]
    fn detail_render_uses_message_headers_not_role_prefix_per_line() {
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
            thread_id: "thread-1".to_string(),
            title: "Thread".to_string(),
            status: "idle".to_string(),
            annotation: None,
            messages: vec![
                MessageBlock {
                    turn_id: Some("turn-1".to_string()),
                    item_id: Some("item-1".to_string()),
                    role: "user".to_string(),
                    timestamp: Some("2026-06-05 09:00".to_string()),
                    raw_text: "Please inspect this".to_string(),
                    lines: vec![MessageLine {
                        kind: MessageLineKind::Text,
                        text: "Please inspect this".to_string(),
                        spans: Vec::new(),
                    }],
                    is_match: false,
                },
                MessageBlock {
                    turn_id: Some("turn-1".to_string()),
                    item_id: Some("item-2".to_string()),
                    role: "assistant".to_string(),
                    timestamp: Some("2026-06-05 09:01".to_string()),
                    raw_text: "First response line\nContinuation line".to_string(),
                    lines: vec![
                        MessageLine {
                            kind: MessageLineKind::Text,
                            text: "First response line".to_string(),
                            spans: Vec::new(),
                        },
                        MessageLine {
                            kind: MessageLineKind::Text,
                            text: "Continuation line".to_string(),
                            spans: Vec::new(),
                        },
                    ],
                    is_match: false,
                },
                MessageBlock {
                    turn_id: Some("turn-1".to_string()),
                    item_id: Some("item-2b".to_string()),
                    role: "assistant".to_string(),
                    timestamp: Some("2026-06-05 09:01".to_string()),
                    raw_text: "Second assistant item".to_string(),
                    lines: vec![MessageLine {
                        kind: MessageLineKind::Text,
                        text: "Second assistant item".to_string(),
                        spans: Vec::new(),
                    }],
                    is_match: false,
                },
                MessageBlock {
                    turn_id: Some("turn-2".to_string()),
                    item_id: Some("item-3".to_string()),
                    role: "user".to_string(),
                    timestamp: Some("2026-06-05 09:02".to_string()),
                    raw_text: "Next turn".to_string(),
                    lines: vec![MessageLine {
                        kind: MessageLineKind::Text,
                        text: "Next turn".to_string(),
                        spans: Vec::new(),
                    }],
                    is_match: false,
                },
            ],
            scroll: 0,
            search_query: String::new(),
            matches: Vec::new(),
            match_index: 0,
            next_cursor: None,
            backwards_cursor: None,
            current_cursor: None,
            active_turn_id: None,
            loading: false,
            epoch: 1,
            last_refresh_at: None,
            viewport_height: None,
            viewport_width: None,
            last_error: None,
        });
        let detail = state.detail.as_ref().expect("detail");
        assert_eq!(
            transcript_lines(&detail.messages).len(),
            detail.transcript_line_count()
        );
        let backend = TestBackend::new(100, 18);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw(frame, &state)).unwrap();
        let content = terminal.backend().buffer().content();
        let text = content.iter().map(|cell| cell.symbol()).collect::<String>();
        assert!(text.contains("USER · 2026-06-05 09:00"));
        assert_eq!(text.matches("ASSISTANT").count(), 1);
        assert!(!text.contains("ASSISTANT · 2026-06-05 09:01"));
        assert!(text.contains("USER · 2026-06-05 09:02"));
        assert!(!text.contains("USER · 2026-06-05 09:00 · turn-1"));
        assert!(!text.contains("ASSISTANT · 2026-06-05 09:01 · turn-1"));
        assert!(text.contains("First response line"));
        assert!(text.contains("Continuation line"));
        assert!(text.contains("Second assistant item"));
        assert!(text.contains("Next turn"));
        assert!(!text.contains("assistant Continuation line"));
    }

    #[test]
    fn detail_header_renders_labeled_annotation_text() {
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
            thread_id: "thread-1".to_string(),
            title: "Thread".to_string(),
            status: "idle".to_string(),
            annotation: Some("unexpected message-like annotation".to_string()),
            messages: vec![MessageBlock {
                turn_id: Some("turn-1".to_string()),
                item_id: Some("item-1".to_string()),
                role: "user".to_string(),
                timestamp: None,
                raw_text: "transcript content".to_string(),
                lines: vec![MessageLine {
                    kind: MessageLineKind::Text,
                    text: "transcript content".to_string(),
                    spans: Vec::new(),
                }],
                is_match: false,
            }],
            scroll: 0,
            search_query: String::new(),
            matches: Vec::new(),
            match_index: 0,
            next_cursor: None,
            backwards_cursor: None,
            current_cursor: None,
            active_turn_id: None,
            loading: false,
            epoch: 1,
            last_refresh_at: None,
            viewport_height: None,
            viewport_width: None,
            last_error: None,
        });
        let backend = TestBackend::new(100, 12);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw(frame, &state)).unwrap();
        let content = terminal.backend().buffer().content();
        let text = content.iter().map(|cell| cell.symbol()).collect::<String>();
        assert!(text.contains("Thread"));
        assert!(text.contains("idle"));
        assert!(text.contains("note: unexpected message-like annotation"));
        assert!(text.contains("transcript content"));
    }

    #[test]
    fn detail_interrupt_confirm_keeps_detail_background() {
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
        state.mode = Mode::ConfirmInterrupt {
            server: "work".to_string(),
            thread_id: "thread-1".to_string(),
            turn_id: Some("turn-1".to_string()),
            return_to_detail: true,
        };
        state.detail = Some(DetailState {
            server: "work".to_string(),
            thread_id: "thread-1".to_string(),
            title: "Thread".to_string(),
            status: "idle".to_string(),
            annotation: None,
            messages: vec![MessageBlock {
                turn_id: Some("turn-1".to_string()),
                item_id: Some("item-1".to_string()),
                role: "user".to_string(),
                timestamp: None,
                raw_text: "detail stays visible".to_string(),
                lines: vec![MessageLine {
                    kind: MessageLineKind::Text,
                    text: "detail stays visible".to_string(),
                    spans: Vec::new(),
                }],
                is_match: false,
            }],
            scroll: 0,
            search_query: String::new(),
            matches: Vec::new(),
            match_index: 0,
            next_cursor: None,
            backwards_cursor: None,
            current_cursor: None,
            active_turn_id: Some("turn-1".to_string()),
            loading: false,
            epoch: 1,
            last_refresh_at: None,
            viewport_height: None,
            viewport_width: None,
            last_error: None,
        });
        let backend = TestBackend::new(100, 18);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw(frame, &state)).unwrap();
        let content = terminal.backend().buffer().content();
        let text = content.iter().map(|cell| cell.symbol()).collect::<String>();
        assert!(text.contains("detail stays visible"));
        assert!(text.contains("Interrupt Turn"));
    }

    #[test]
    fn detail_header_uses_matching_stream_status() {
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
            thread_id: "thread-1".to_string(),
            title: "Thread".to_string(),
            status: "idle".to_string(),
            annotation: None,
            messages: Vec::new(),
            scroll: 0,
            search_query: String::new(),
            matches: Vec::new(),
            match_index: 0,
            next_cursor: None,
            backwards_cursor: None,
            current_cursor: None,
            active_turn_id: Some("turn-1".to_string()),
            loading: false,
            epoch: 1,
            last_refresh_at: None,
            viewport_height: None,
            viewport_width: None,
            last_error: None,
        });
        state.stream = Some(StreamState::new(
            "thread-1".to_string(),
            Some("turn-1".to_string()),
            StreamStatus::Running,
            false,
        ));

        let backend = TestBackend::new(100, 18);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw(frame, &state)).unwrap();
        let content = terminal.backend().buffer().content();
        let text = content.iter().map(|cell| cell.symbol()).collect::<String>();
        assert!(text.contains("running  connected"));
        assert!(
            text.contains(" live"),
            "connected running stream shows the animated live indicator"
        );
        assert!(!text.contains("stream=running"));
        assert!(!text.contains("thread-1"));
        assert!(!text.contains("turn-1"));
        assert!(!text.contains("list rows="));
    }

    #[test]
    fn follow_probe_shows_thread_status_without_live_indicator() {
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
        state.browser.rows = vec![ThreadRow {
            server: "work".to_string(),
            id: "thread-1".to_string(),
            title: "Finished thread".to_string(),
            status: "idle".to_string(),
            updated: "2026-06-05 09:30".to_string(),
            cwd: "/tmp/repo".to_string(),
            annotation: None,
            snippet: None,
            raw: serde_json::json!({}),
        }];
        state.browser.selected = 0;
        state.browser.preview.server = Some("work".to_string());
        state.browser.preview.thread_id = Some("thread-1".to_string());
        // After a turn completes, the follow probe waits for a queued
        // follow-up turn; it must not present itself as a live stream.
        state.stream = Some(StreamState::new(
            "thread-1".to_string(),
            None,
            StreamStatus::Following,
            true,
        ));

        let backend = TestBackend::new(100, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw(frame, &state)).unwrap();
        let content = terminal.backend().buffer().content();
        let text = content.iter().map(|cell| cell.symbol()).collect::<String>();
        assert!(text.contains("idle"), "row shows its own status");
        assert!(!text.contains("starting"), "probe must not show starting");
        assert!(!text.contains(" live"), "probe must not show live");
    }

    #[test]
    fn preview_title_shows_live_indicator_for_streaming_selection() {
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
        state.browser.rows = vec![ThreadRow {
            server: "work".to_string(),
            id: "thread-1".to_string(),
            title: "Streaming thread".to_string(),
            status: "active".to_string(),
            updated: "2026-06-05 09:30".to_string(),
            cwd: "/tmp/repo".to_string(),
            annotation: None,
            snippet: None,
            raw: serde_json::json!({}),
        }];
        state.browser.selected = 0;
        state.browser.preview.server = Some("work".to_string());
        state.browser.preview.thread_id = Some("thread-1".to_string());

        let backend = TestBackend::new(100, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw(frame, &state)).unwrap();
        let content = terminal.backend().buffer().content();
        let text = content.iter().map(|cell| cell.symbol()).collect::<String>();
        assert!(
            !text.contains(" live"),
            "no live indicator without a running stream"
        );

        state.stream = Some(StreamState::new(
            "thread-1".to_string(),
            Some("turn-1".to_string()),
            StreamStatus::Running,
            false,
        ));
        terminal.draw(|frame| draw(frame, &state)).unwrap();
        let content = terminal.backend().buffer().content();
        let text = content.iter().map(|cell| cell.symbol()).collect::<String>();
        assert!(
            text.contains("Recent Messages") && text.contains(" live"),
            "preview title shows the live indicator while streaming"
        );
    }
}
