use std::time::Instant;

use serde_json::Value;

use crate::cli::SortKey;
use crate::tui::prefs::{TuiPrefs, VisibleColumns};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mode {
    Browser,
    Detail,
    SearchInput { draft: String },
    MessageSearchInput { draft: String },
    AnnotationInput { thread_id: String, draft: String },
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
    pub lines: Vec<MessageLine>,
    pub scroll: u16,
    pub search_query: String,
    pub matches: Vec<usize>,
    pub match_index: usize,
    pub next_cursor: Option<String>,
    pub backwards_cursor: Option<String>,
    pub active_turn_id: Option<String>,
    pub loading: bool,
    pub epoch: u64,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageLine {
    pub turn_id: Option<String>,
    pub role: String,
    pub kind: MessageLineKind,
    pub text: String,
    pub is_match: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageLineKind {
    Text,
    Heading,
    Quote,
    Code,
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
    pub events: Vec<Value>,
    pub last_error: Option<String>,
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
        let sort = init.sort.or(init.prefs.sort_key);
        let auto_refresh = init.prefs.auto_refresh;
        let auto_refresh_seconds = init.prefs.auto_refresh_seconds.max(5);
        Self {
            mode: Mode::Browser,
            browser: BrowserState {
                source,
                query,
                rows: Vec::new(),
                selected: 0,
                next_cursor: None,
                backwards_cursor: None,
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
            should_quit: false,
        }
    }

    pub fn visible_columns(&self) -> &VisibleColumns {
        &self.prefs.visible_columns
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
    ) {
        if epoch != self.browser.epoch {
            return;
        }
        let previous_id = self.selected_thread_id().map(str::to_string);
        self.browser.rows = rows;
        self.browser.next_cursor = next_cursor;
        self.browser.backwards_cursor = backwards_cursor;
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

    pub fn set_detail(&mut self, epoch: u64, detail: DetailState) {
        let same_detail_epoch = self
            .detail
            .as_ref()
            .is_some_and(|current| current.epoch == epoch);
        if !same_detail_epoch {
            return;
        }
        self.detail = Some(detail);
        self.mode = Mode::Detail;
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
            for (index, line) in detail.lines.iter_mut().enumerate() {
                line.is_match = !detail.search_query.is_empty()
                    && line.text.to_lowercase().contains(&detail.search_query);
                if line.is_match {
                    detail.matches.push(index);
                }
            }
            if let Some(index) = detail.matches.first() {
                detail.scroll = (*index).min(u16::MAX as usize) as u16;
            }
        }
    }
}
