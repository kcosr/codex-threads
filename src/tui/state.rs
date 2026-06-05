use std::time::Instant;

use serde_json::Value;
use tokio::sync::mpsc;

use crate::cli::SortKey;
use crate::tui::prefs::{TuiPrefs, VisibleColumns};
use crate::turns::TurnControl;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mode {
    Browser,
    Detail,
    SearchInput {
        draft: String,
    },
    MessageSearchInput {
        draft: String,
    },
    FilterMenu,
    SortMenu,
    ColumnsMenu,
    ActiveTurnPrompt {
        thread_id: String,
        turn_id: String,
    },
    ConfirmInterrupt {
        thread_id: String,
        turn_id: String,
    },
    AnnotationInput {
        thread_id: String,
        draft: String,
        return_to_detail: bool,
    },
    Compose(ComposeState),
    Help,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BrowserSource {
    List,
    Search,
}

#[derive(Debug, Clone)]
pub struct BrowserState {
    pub source: BrowserSource,
    pub query: String,
    pub rows: Vec<ThreadRow>,
    pub selected: usize,
    pub next_cursor: Option<String>,
    pub backwards_cursor: Option<String>,
    pub current_cursor: Option<String>,
    pub limit: u32,
    pub since: Option<i64>,
    pub cwd: Option<String>,
    pub archived: bool,
    pub sort: Option<SortKey>,
    pub descending: bool,
    pub loading: bool,
    pub auto_refresh: bool,
    pub auto_refresh_seconds: u64,
    pub epoch: u64,
    pub last_refresh_at: Option<Instant>,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadRow {
    pub id: String,
    pub title: String,
    pub status: String,
    pub updated: String,
    pub cwd: String,
    pub annotation: Option<String>,
    pub snippet: Option<String>,
    pub raw: Value,
}

#[derive(Debug, Clone)]
pub struct DetailState {
    pub thread_id: String,
    pub title: String,
    pub status: String,
    pub annotation: Option<String>,
    pub messages: Vec<MessageBlock>,
    pub scroll: u16,
    pub search_query: String,
    pub matches: Vec<usize>,
    pub match_index: usize,
    pub next_cursor: Option<String>,
    pub backwards_cursor: Option<String>,
    pub current_cursor: Option<String>,
    pub active_turn_id: Option<String>,
    pub loading: bool,
    pub epoch: u64,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageBlock {
    pub turn_id: Option<String>,
    pub item_id: Option<String>,
    pub role: String,
    pub timestamp: Option<String>,
    pub lines: Vec<MessageLine>,
    pub is_match: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageLine {
    pub kind: MessageLineKind,
    pub text: String,
    pub spans: Vec<MessageSpan>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageLineKind {
    Text,
    Heading,
    Quote,
    Code,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageSpan {
    pub text: String,
    pub color: Option<MessageColor>,
    pub bold: bool,
    pub italic: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub enum MessageColor {
    Rgb(u8, u8, u8),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComposeState {
    pub target: ComposeTarget,
    pub text: String,
    pub send_mode: SendMode,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ComposeTarget {
    NewTurn { thread_id: String },
    Steer { thread_id: String, turn_id: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendMode {
    Stream,
    NoWait,
}

#[derive(Debug, Clone)]
pub struct StreamState {
    pub thread_id: String,
    pub turn_id: Option<String>,
    pub status: StreamStatus,
    pub accumulated_text: String,
    pub events: Vec<Value>,
    pub attached: bool,
    pub detached: bool,
    pub last_error: Option<String>,
    pub last_poll_at: Option<Instant>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamStatus {
    Starting,
    Running,
    Completed,
    Failed,
    Interrupted,
    Detached,
}

#[derive(Debug, Clone)]
pub struct TuiState {
    pub mode: Mode,
    pub browser: BrowserState,
    pub detail: Option<DetailState>,
    pub prefs: TuiPrefs,
    pub stream: Option<StreamState>,
    pub stream_control: Option<mpsc::UnboundedSender<TurnControl>>,
    pub should_quit: bool,
}

#[derive(Debug, Clone)]
pub struct TuiInit {
    pub query: Option<String>,
    pub since: Option<i64>,
    pub cwd: Option<String>,
    pub archived: bool,
    pub limit: u32,
    pub sort: Option<SortKey>,
    pub descending: bool,
    pub prefs: TuiPrefs,
}

impl TuiState {
    pub fn new(init: TuiInit) -> Self {
        let query = init.query.unwrap_or_default();
        let source = if query.is_empty() {
            BrowserSource::List
        } else {
            BrowserSource::Search
        };
        let sort = init.sort.or(init.prefs.browser.sort);
        let auto_refresh = init.prefs.refresh.auto;
        let auto_refresh_seconds = init.prefs.refresh.interval_seconds.max(5);
        Self {
            mode: Mode::Browser,
            browser: BrowserState {
                source,
                query,
                rows: Vec::new(),
                selected: 0,
                next_cursor: None,
                backwards_cursor: None,
                current_cursor: None,
                limit: init.limit,
                since: init.since,
                cwd: init.cwd,
                archived: init.archived,
                sort,
                descending: init.descending,
                loading: false,
                auto_refresh,
                auto_refresh_seconds,
                epoch: 0,
                last_refresh_at: None,
                last_error: None,
            },
            detail: None,
            prefs: init.prefs,
            stream: None,
            stream_control: None,
            should_quit: false,
        }
    }

    pub fn visible_columns(&self) -> &VisibleColumns {
        &self.prefs.browser.columns
    }

    pub fn selected_thread_id(&self) -> Option<&str> {
        self.browser
            .rows
            .get(self.browser.selected)
            .map(|row| row.id.as_str())
    }

    pub fn selected_thread_annotation(&self) -> Option<&str> {
        self.browser
            .rows
            .get(self.browser.selected)
            .and_then(|row| row.annotation.as_deref())
    }

    pub fn move_selection(&mut self, delta: isize) {
        if self.browser.rows.is_empty() {
            self.browser.selected = 0;
            return;
        }
        let len = self.browser.rows.len() as isize;
        let current = self.browser.selected as isize;
        let next = (current + delta).clamp(0, len - 1);
        self.browser.selected = next as usize;
    }

    pub fn set_browser_rows(
        &mut self,
        epoch: u64,
        rows: Vec<ThreadRow>,
        next_cursor: Option<String>,
        backwards_cursor: Option<String>,
        current_cursor: Option<String>,
    ) {
        if epoch != self.browser.epoch {
            return;
        }
        let previous_id = self.selected_thread_id().map(str::to_string);
        self.browser.rows = rows;
        self.browser.next_cursor = next_cursor;
        self.browser.backwards_cursor = backwards_cursor;
        self.browser.current_cursor = current_cursor;
        self.browser.loading = false;
        self.browser.last_refresh_at = Some(Instant::now());
        self.browser.last_error = None;
        self.browser.selected = previous_id
            .and_then(|id| self.browser.rows.iter().position(|row| row.id == id))
            .unwrap_or(0)
            .min(self.browser.rows.len().saturating_sub(1));
    }

    pub fn set_browser_error(&mut self, epoch: u64, error: String) {
        if epoch != self.browser.epoch {
            return;
        }
        self.browser.loading = false;
        self.browser.last_error = Some(error);
    }

    pub fn replace_detail(&mut self, epoch: u64, mut detail: DetailState) {
        let Some(current) = self.detail.as_ref() else {
            return;
        };
        if current.epoch != epoch {
            return;
        }
        let search_query = if current.thread_id == detail.thread_id {
            current.search_query.clone()
        } else {
            String::new()
        };
        detail.search_query = search_query;
        self.detail = Some(detail);
        if let Some(query) = self
            .detail
            .as_ref()
            .map(|detail| detail.search_query.clone())
            .filter(|query| !query.is_empty())
        {
            self.update_message_search(query);
        }
        self.mode = Mode::Detail;
    }

    pub fn extend_detail_older(&mut self, epoch: u64, mut page: DetailState) {
        let Some(detail) = &mut self.detail else {
            return;
        };
        if detail.epoch != epoch || detail.thread_id != page.thread_id {
            return;
        }
        append_unique_messages(&mut detail.messages, page.messages.drain(..));
        detail.next_cursor = page.next_cursor;
        detail.backwards_cursor = page.backwards_cursor.or(detail.backwards_cursor.clone());
        detail.current_cursor = page.current_cursor;
        detail.active_turn_id = page.active_turn_id;
        detail.status = page.status;
        detail.loading = false;
        detail.last_error = None;
        let query = detail.search_query.clone();
        if !query.is_empty() {
            self.update_message_search(query);
        }
    }

    pub fn extend_detail_newer(&mut self, epoch: u64, mut page: DetailState) {
        let Some(detail) = &mut self.detail else {
            return;
        };
        if detail.epoch != epoch || detail.thread_id != page.thread_id {
            return;
        }
        let mut merged = Vec::new();
        append_unique_messages(&mut merged, page.messages.drain(..));
        append_unique_messages(&mut merged, detail.messages.drain(..));
        detail.messages = merged;
        detail.next_cursor = page.next_cursor.or(detail.next_cursor.clone());
        detail.backwards_cursor = page.backwards_cursor;
        detail.current_cursor = page.current_cursor;
        detail.active_turn_id = page.active_turn_id;
        detail.status = page.status;
        detail.loading = false;
        detail.last_error = None;
        let query = detail.search_query.clone();
        if !query.is_empty() {
            self.update_message_search(query);
        }
    }

    pub fn set_detail_error(&mut self, epoch: u64, error: String) {
        if let Some(detail) = &mut self.detail
            && detail.epoch == epoch
        {
            detail.loading = false;
            detail.last_error = Some(error);
        }
    }

    pub fn update_query(&mut self, query: String) {
        self.browser.query = query;
        self.browser.source = if self.browser.query.is_empty() {
            BrowserSource::List
        } else {
            BrowserSource::Search
        };
        self.browser.selected = 0;
    }

    pub fn update_message_search(&mut self, query: String) {
        if let Some(detail) = &mut self.detail {
            detail.search_query = query.to_lowercase();
            detail.matches.clear();
            detail.match_index = 0;
            for (index, message) in detail.messages.iter_mut().enumerate() {
                let body_matches = message
                    .lines
                    .iter()
                    .any(|line| line.text.to_lowercase().contains(&detail.search_query));
                let header_matches =
                    message.turn_id.as_ref().is_some_and(|turn_id| {
                        turn_id.to_lowercase().contains(&detail.search_query)
                    }) || message.role.to_lowercase().contains(&detail.search_query);
                message.is_match =
                    !detail.search_query.is_empty() && (body_matches || header_matches);
                if message.is_match {
                    detail.matches.push(index);
                }
            }
            if let Some(message_index) = detail.matches.first().copied() {
                detail.scroll = detail
                    .message_scroll_offset(message_index)
                    .min(u16::MAX as usize) as u16;
            }
        }
    }

    pub fn next_message_match(&mut self) {
        if let Some(detail) = &mut self.detail
            && !detail.matches.is_empty()
        {
            detail.match_index = (detail.match_index + 1) % detail.matches.len();
            let message_index = detail.matches[detail.match_index];
            detail.scroll = detail
                .message_scroll_offset(message_index)
                .min(u16::MAX as usize) as u16;
        }
    }

    pub fn previous_message_match(&mut self) {
        if let Some(detail) = &mut self.detail
            && !detail.matches.is_empty()
        {
            detail.match_index = if detail.match_index == 0 {
                detail.matches.len() - 1
            } else {
                detail.match_index - 1
            };
            let message_index = detail.matches[detail.match_index];
            detail.scroll = detail
                .message_scroll_offset(message_index)
                .min(u16::MAX as usize) as u16;
        }
    }
}

impl DetailState {
    pub fn message_scroll_offset(&self, message_index: usize) -> usize {
        self.messages
            .iter()
            .take(message_index)
            .map(|message| 2 + message.lines.len())
            .sum()
    }
}

fn append_unique_messages(
    target: &mut Vec<MessageBlock>,
    messages: impl IntoIterator<Item = MessageBlock>,
) {
    for message in messages {
        let duplicate = target.iter().any(|existing| {
            if message.item_id.is_some() || existing.item_id.is_some() {
                existing.item_id == message.item_id
            } else {
                existing.turn_id == message.turn_id
                    && existing.role == message.role
                    && existing.lines == message.lines
            }
        });
        if !duplicate {
            target.push(message);
        }
    }
}
