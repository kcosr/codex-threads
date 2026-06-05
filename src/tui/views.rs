use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Clear, Paragraph, Row, Table, Wrap};

use crate::tui::keymap::{BROWSER_HELP, COMPOSE_HELP, DEFAULT_HELP, DETAIL_HELP};
use crate::tui::state::{BrowserSource, ComposeTarget, Mode, SendMode, StreamStatus, TuiState};
use crate::tui::state::{MessageColor, MessageLine, MessageLineKind, MessageSpan};

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
            | Mode::ActiveTurnPrompt { .. }
            | Mode::ConfirmInterrupt { .. }
    ) || (matches!(state.mode, Mode::AnnotationInput { .. })
        && state.detail.is_some());
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
                "Enter search, Esc cancel",
            );
        }
        Mode::MessageSearchInput { draft } => {
            draw_prompt(
                frame,
                area,
                "Search messages",
                draft,
                "Enter search, Esc cancel",
            );
        }
        Mode::AnnotationInput { draft, .. } => {
            draw_prompt(
                frame,
                area,
                "Annotation",
                draft,
                "Ctrl-S save, Ctrl-D clear, Esc cancel",
            );
        }
        Mode::FilterMenu => draw_filter_menu(frame, area, state),
        Mode::SortMenu => draw_sort_menu(frame, area, state),
        Mode::ColumnsMenu => draw_columns_menu(frame, area, state),
        Mode::ActiveTurnPrompt { thread_id, turn_id } => {
            draw_active_turn_prompt(frame, area, thread_id, turn_id);
        }
        Mode::ConfirmInterrupt { thread_id, turn_id } => {
            draw_confirm_interrupt(frame, area, thread_id, turn_id);
        }
        Mode::Compose(compose) => {
            let label = match compose.target {
                ComposeTarget::Steer { .. } => "Steer active turn",
                ComposeTarget::NewTurn { .. } => match compose.send_mode {
                    SendMode::Stream => "Compose stream",
                    SendMode::NoWait => "Compose no-wait",
                },
            };
            let footer = match compose.target {
                ComposeTarget::Steer { .. } => "Ctrl-S steer, Esc cancel",
                ComposeTarget::NewTurn { .. } => "Ctrl-S send, Tab mode, Esc cancel",
            };
            draw_prompt(frame, area, label, &compose.text, footer);
        }
        Mode::Help => draw_help(frame, area),
        _ => {}
    }
}

fn draw_browser(frame: &mut Frame<'_>, area: Rect, state: &TuiState) {
    let (table_area, preview_area) = if state.prefs.browser.preview_pane && area.height >= 16 {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(6), Constraint::Length(9)])
            .split(area);
        (chunks[0], Some(chunks[1]))
    } else {
        (area, None)
    };
    let visible = state.visible_columns();
    let mut widths = vec![Constraint::Min(20)];
    let mut header = vec![Cell::from("THREAD")];
    if visible.status {
        widths.push(Constraint::Length(11));
        header.push(Cell::from("STATUS"));
    }
    if visible.updated {
        widths.push(Constraint::Length(14));
        header.push(Cell::from("UPDATED"));
    }
    if visible.cwd {
        widths.push(Constraint::Percentage(25));
        header.push(Cell::from("CWD"));
    }
    if visible.annotation {
        widths.push(Constraint::Percentage(24));
        header.push(Cell::from("ANNOTATION"));
    }

    let rows = state.browser.rows.iter().enumerate().map(|(index, row)| {
        let title = if let Some(snippet) = &row.snippet {
            format!("{}  {}", row.title, snippet)
        } else {
            row.title.clone()
        };
        let mut cells = vec![Cell::from(title)];
        if visible.status {
            cells.push(Cell::from(row.status.clone()));
        }
        if visible.updated {
            cells.push(Cell::from(row.updated.clone()));
        }
        if visible.cwd {
            cells.push(Cell::from(row.cwd.clone()));
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
        .column_spacing(1);
    frame.render_widget(table, table_area);
    if let Some(area) = preview_area {
        draw_browser_preview(frame, area, state);
    }
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
    let mut text = vec![
        Line::from(row.title.clone()),
        Line::from(format!("cwd: {}", row.cwd)),
        Line::from(format!("thread: {}", row.id)),
        Line::from(format!("updated: {}", row.updated)),
        Line::from(format!(
            "annotation: {}",
            row.annotation.as_deref().unwrap_or("")
        )),
    ];
    if let Some(snippet) = &row.snippet {
        text.push(Line::from("Last message:"));
        text.push(Line::from(snippet.clone()));
    }
    frame.render_widget(
        Paragraph::new(text)
            .wrap(Wrap { trim: false })
            .block(Block::default().title(" Preview ").borders(Borders::ALL)),
        area,
    );
}

fn draw_detail(frame: &mut Frame<'_>, area: Rect, state: &TuiState) {
    let Some(detail) = &state.detail else {
        draw_browser(frame, area, state);
        return;
    };
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Min(5)])
        .split(area);
    let metadata = vec![Line::from(vec![
        Span::styled(detail.thread_id.clone(), Style::default().fg(Color::Cyan)),
        Span::raw("  "),
        Span::raw(detail.status.clone()),
        Span::raw("  "),
        Span::raw(
            detail
                .active_turn_id
                .as_ref()
                .map(|turn_id| format!("active={turn_id}  "))
                .unwrap_or_default(),
        ),
        Span::raw(if detail.next_cursor.is_some() {
            "older  "
        } else {
            ""
        }),
        Span::raw(if detail.backwards_cursor.is_some() {
            "newer  "
        } else {
            ""
        }),
        Span::raw(detail.annotation.clone().unwrap_or_default()),
    ])];
    frame.render_widget(
        Paragraph::new(metadata).block(
            Block::default()
                .title(detail.title.clone())
                .borders(Borders::ALL),
        ),
        chunks[0],
    );

    let mut lines = Vec::new();
    for message in &detail.messages {
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
        lines.push(Line::from(Span::styled(
            message_header(message),
            header_style,
        )));
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
        lines.push(Line::from(Span::styled(
            "────────────────────────────────────────────────────────────────",
            Style::default().fg(Color::DarkGray),
        )));
    }
    frame.render_widget(
        Paragraph::new(lines)
            .block(Block::default().title(" Transcript ").borders(Borders::ALL))
            .scroll((detail.scroll, 0))
            .wrap(Wrap { trim: false })
            .style(Style::default()),
        chunks[1],
    );
}

fn message_header(message: &crate::tui::state::MessageBlock) -> String {
    let role = message.role.to_uppercase();
    let timestamp = message.timestamp.as_deref().unwrap_or("");
    let turn_id = message.turn_id.as_deref().unwrap_or("");
    match (timestamp.is_empty(), turn_id.is_empty()) {
        (true, true) => role,
        (true, false) => format!("{role} · {turn_id}"),
        (false, true) => format!("{role} · {timestamp}"),
        (false, false) => format!("{role} · {timestamp} · {turn_id}"),
    }
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
    let stream = state.stream.as_ref().map(format_stream).unwrap_or_default();
    let message_search = state
        .detail
        .as_ref()
        .filter(|detail| !detail.search_query.is_empty())
        .map(|detail| {
            if detail.matches.is_empty() {
                format!(" message_search={} 0 matches", detail.search_query)
            } else {
                format!(
                    " message_search={} match={}/{}",
                    detail.search_query,
                    detail.match_index + 1,
                    detail.matches.len()
                )
            }
        })
        .unwrap_or_default();
    let error = state
        .browser
        .last_error
        .as_ref()
        .map(|error| format!(" error={error}"))
        .unwrap_or_default();
    let status = format!(
        "{} rows={}{}{}{}{}{}{}",
        match state.browser.source {
            BrowserSource::List => "list",
            BrowserSource::Search => "search",
        },
        state.browser.rows.len(),
        query,
        local_cwd,
        loading,
        stream,
        message_search,
        error
    );
    frame.render_widget(Paragraph::new(status), area);
}

fn draw_help_bar(frame: &mut Frame<'_>, area: Rect, state: &TuiState) {
    let text = match state.mode {
        Mode::Browser => BROWSER_HELP,
        Mode::Detail => DETAIL_HELP,
        Mode::Compose(_) => COMPOSE_HELP,
        _ => DEFAULT_HELP,
    };
    frame.render_widget(
        Paragraph::new(text).style(Style::default().fg(Color::Gray)),
        area,
    );
}

fn draw_prompt(frame: &mut Frame<'_>, area: Rect, title: &str, value: &str, footer: &str) {
    let area = centered_rect(area, 70, 5);
    frame.render_widget(Clear, area);
    let text = vec![
        Line::from(value.to_string()),
        Line::from(Span::styled(footer, Style::default().fg(Color::Gray))),
    ];
    frame.render_widget(
        Paragraph::new(text)
            .wrap(Wrap { trim: false })
            .block(Block::default().title(title).borders(Borders::ALL)),
        area,
    );
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
            "Esc close".to_string(),
        ],
    );
}

fn draw_active_turn_prompt(frame: &mut Frame<'_>, area: Rect, thread_id: &str, turn_id: &str) {
    draw_static_modal(
        frame,
        area,
        "Active Turn",
        &[
            format!("Thread {thread_id} already has active turn {turn_id}."),
            "Enter/T attach".to_string(),
            "s steer".to_string(),
            "i interrupt".to_string(),
            "Esc cancel".to_string(),
        ],
    );
}

fn draw_confirm_interrupt(frame: &mut Frame<'_>, area: Rect, thread_id: &str, turn_id: &str) {
    draw_static_modal(
        frame,
        area,
        "Interrupt Turn",
        &[
            format!("Interrupt {turn_id} on {thread_id}?"),
            "Enter interrupt".to_string(),
            "Esc cancel".to_string(),
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
    let area = centered_rect(area, 76, 16);
    frame.render_widget(Clear, area);
    let items = [
        "Browser",
        "  j/k or arrows move; Enter opens a thread; r refreshes; / searches.",
        "  f filters; s sort; c columns; A edits annotation; t toggles auto-refresh.",
        "Detail",
        "  Esc returns to the browser; / searches loaded transcript lines.",
        "  m opens compose; T attaches; S steers; i opens interrupt confirmation.",
        "Streams",
        "  T attaches to an active turn. Esc or q detaches locally; remote turns keep running unless interrupted.",
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
    let status = match state.status {
        StreamStatus::Starting => "starting",
        StreamStatus::Running => "running",
        StreamStatus::Completed => "completed",
        StreamStatus::Failed => "failed",
        StreamStatus::Interrupted => "interrupted",
        StreamStatus::Detached => "detached",
    };
    let attached = if state.attached { ":attached" } else { "" };
    let detached = if state.detached { ":detached" } else { "" };
    let polled = if state.last_poll_at.is_some() {
        ":polled"
    } else {
        ""
    };
    let error = state
        .last_error
        .as_ref()
        .map(|error| format!(" error={error}"))
        .unwrap_or_default();
    format!(
        " stream={}{}{}{}{}{}",
        state.thread_id,
        state
            .turn_id
            .as_ref()
            .map(|turn_id| format!(":{turn_id}:{status}"))
            .unwrap_or_else(|| format!(":{status}")),
        attached,
        detached,
        polled,
        error
    )
}

#[cfg(test)]
mod tests {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use crate::tui::prefs::TuiPrefs;
    use crate::tui::state::{
        DetailState, MessageBlock, MessageLine, MessageLineKind, Mode, ThreadRow, TuiInit, TuiState,
    };

    use super::*;

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
            id: "thread-1".to_string(),
            title: "Fix tests".to_string(),
            status: "idle".to_string(),
            updated: "2026-06-05".to_string(),
            cwd: "/repo".to_string(),
            annotation: Some("needs review".to_string()),
            snippet: Some("recent assistant message".to_string()),
            raw: serde_json::json!({}),
        });
        let backend = TestBackend::new(100, 18);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw(frame, &state)).unwrap();
        let content = terminal.backend().buffer().content();
        let text = content.iter().map(|cell| cell.symbol()).collect::<String>();
        assert!(text.contains("Fix tests"));
        assert!(text.contains("needs review"));
        assert!(text.contains("recent assistant message"));
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
            last_error: None,
        });
        let backend = TestBackend::new(100, 18);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw(frame, &state)).unwrap();
        let content = terminal.backend().buffer().content();
        let text = content.iter().map(|cell| cell.symbol()).collect::<String>();
        assert!(text.contains("USER · 2026-06-05 09:00 · turn-1"));
        assert!(text.contains("ASSISTANT · 2026-06-05 09:01 · turn-1"));
        assert!(text.contains("First response line"));
        assert!(text.contains("Continuation line"));
        assert!(!text.contains("assistant Continuation line"));
    }

    #[test]
    fn active_turn_prompt_keeps_detail_background() {
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
        state.mode = Mode::ActiveTurnPrompt {
            thread_id: "thread-1".to_string(),
            turn_id: "turn-1".to_string(),
        };
        state.detail = Some(DetailState {
            thread_id: "thread-1".to_string(),
            title: "Thread".to_string(),
            status: "idle".to_string(),
            annotation: None,
            messages: vec![MessageBlock {
                turn_id: Some("turn-1".to_string()),
                item_id: Some("item-1".to_string()),
                role: "user".to_string(),
                timestamp: None,
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
            last_error: None,
        });
        let backend = TestBackend::new(100, 18);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw(frame, &state)).unwrap();
        let content = terminal.backend().buffer().content();
        let text = content.iter().map(|cell| cell.symbol()).collect::<String>();
        assert!(text.contains("detail stays visible"));
        assert!(text.contains("Active Turn"));
    }
}
