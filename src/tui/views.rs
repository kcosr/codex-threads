use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Clear, List, ListItem, Paragraph, Row, Table, Wrap};

use crate::tui::state::{BrowserSource, ComposeTarget, Mode, SendMode, StreamStatus, TuiState};

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

    match state.mode {
        Mode::Detail | Mode::MessageSearchInput { .. } | Mode::Compose(_) => {
            draw_detail(frame, chunks[0], state);
        }
        _ => draw_browser(frame, chunks[0], state),
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
            draw_prompt(frame, area, "Annotation", draft, "Enter save, Esc cancel");
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
                ComposeTarget::Steer { .. } => "Enter steer, Esc cancel",
                ComposeTarget::NewTurn { .. } => "Enter send, Tab mode, Esc cancel",
            };
            draw_prompt(frame, area, label, &compose.text, footer);
        }
        Mode::Help => draw_help(frame, area),
        _ => {}
    }
}

fn draw_browser(frame: &mut Frame<'_>, area: Rect, state: &TuiState) {
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
    frame.render_widget(table, area);
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

    let lines = detail.lines.iter().map(|line| {
        let role_style = match line.role.as_str() {
            "user" => Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
            "assistant" => Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
            _ => Style::default().fg(Color::Gray),
        };
        let text_style = if line.is_match {
            Style::default().fg(Color::Black).bg(Color::Yellow)
        } else {
            Style::default()
        };
        ListItem::new(Line::from(vec![
            Span::styled(format!("{:>9} ", line.role), role_style),
            Span::styled(line.text.clone(), text_style),
        ]))
    });
    frame.render_widget(
        List::new(lines)
            .block(Block::default().title(" Transcript ").borders(Borders::ALL))
            .style(Style::default())
            .highlight_style(Style::default()),
        chunks[1],
    );
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
    let stream = state.stream.as_ref().map(format_stream).unwrap_or_default();
    let error = state
        .browser
        .last_error
        .as_ref()
        .map(|error| format!(" error={error}"))
        .unwrap_or_default();
    let status = format!(
        "{} rows={}{}{}{}{}",
        match state.browser.source {
            BrowserSource::List => "list",
            BrowserSource::Search => "search",
        },
        state.browser.rows.len(),
        query,
        loading,
        stream,
        error
    );
    frame.render_widget(Paragraph::new(status), area);
}

fn draw_help_bar(frame: &mut Frame<'_>, area: Rect, state: &TuiState) {
    let text = match state.mode {
        Mode::Browser => {
            "j/k move  Enter open  / search  r refresh  A annotate  e send  c cols  t auto  ? help  q quit"
        }
        Mode::Detail => {
            "Esc browser  j/k scroll  / search  e send  S steer  i interrupt  A annotate  r refresh  q quit"
        }
        Mode::Compose(_) => "Enter send  Tab stream/no-wait  Esc cancel",
        _ => "Enter accept  Esc cancel",
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

fn draw_help(frame: &mut Frame<'_>, area: Rect) {
    let area = centered_rect(area, 76, 16);
    frame.render_widget(Clear, area);
    let items = [
        "Browser",
        "  j/k or arrows move; Enter opens a thread; r refreshes; / searches.",
        "  A edits annotation; c cycles visible columns; t toggles auto-refresh.",
        "Detail",
        "  Esc returns to the browser; / searches loaded transcript lines.",
        "  e opens compose; Tab in compose toggles stream and no-wait.",
        "Streams",
        "  Esc or q detaches locally; remote turns keep running unless interrupted.",
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
    format!(
        " stream={}{}",
        state.thread_id,
        state
            .turn_id
            .as_ref()
            .map(|turn_id| format!(":{turn_id}:{status}"))
            .unwrap_or_else(|| format!(":{status}"))
    )
}

#[cfg(test)]
mod tests {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use crate::tui::prefs::TuiPrefs;
    use crate::tui::state::{ThreadRow, TuiInit, TuiState};

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
            snippet: None,
            raw: serde_json::json!({}),
        });
        let backend = TestBackend::new(100, 12);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw(frame, &state)).unwrap();
        let content = terminal.backend().buffer().content();
        let text = content.iter().map(|cell| cell.symbol()).collect::<String>();
        assert!(text.contains("Fix tests"));
        assert!(text.contains("needs review"));
    }
}
