use std::time::{Duration, Instant};

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
    ConfirmInterrupt {
        server: String,
        thread_id: String,
        turn_id: Option<String>,
        return_to_detail: bool,
    },
    ConfirmArchive {
        server: String,
        thread_id: String,
        archived: bool,
        return_to_detail: bool,
    },
    ConfirmOpenCodex {
        server: String,
        thread_id: String,
        cwd: String,
        return_to_detail: bool,
    },
    Usage(UsageModalState),
    ConfirmRateLimitReset {
        usage: UsageModalState,
        selected: ResetConfirmSelection,
    },
    AnnotationInput {
        server: String,
        thread_id: String,
        draft: String,
        return_to_detail: bool,
    },
    RenameInput {
        server: String,
        thread_id: String,
        draft: String,
        return_to_detail: bool,
    },
    NewSessionServerMenu {
        draft: NewSessionDraft,
        servers: Vec<String>,
        selected: usize,
    },
    NewSessionCwdInput {
        draft: NewSessionDraft,
    },
    NewSessionTitleInput {
        draft: NewSessionDraft,
    },
    Compose(ComposeState),
    Help,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewSessionDraft {
    pub server: String,
    pub cwd: String,
    pub title: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsageModalState {
    pub server: String,
    pub return_to_detail: bool,
    pub loading: bool,
    pub redeeming: bool,
    pub snapshot: Option<AccountUsageSnapshot>,
    pub error: Option<String>,
    pub message: Option<String>,
    pub selected: UsageAction,
    pub reset_idempotency_key: Option<String>,
}

impl UsageModalState {
    pub fn loading(server: String, return_to_detail: bool) -> Self {
        Self {
            server,
            return_to_detail,
            loading: true,
            redeeming: false,
            snapshot: None,
            error: None,
            message: None,
            selected: UsageAction::Close,
            reset_idempotency_key: None,
        }
    }

    pub fn reset_count(&self) -> Option<i64> {
        self.snapshot
            .as_ref()
            .and_then(|snapshot| snapshot.reset_credits)
    }

    pub fn clear_reset_idempotency_key(&mut self) {
        self.reset_idempotency_key = None;
    }

    pub fn invalidate_reset_availability(&mut self) {
        if let Some(snapshot) = &mut self.snapshot {
            snapshot.reset_credits = None;
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UsageAction {
    Close,
    Refresh,
    Redeem,
}

impl UsageAction {
    pub fn previous(self) -> Self {
        match self {
            Self::Close => Self::Redeem,
            Self::Refresh => Self::Close,
            Self::Redeem => Self::Refresh,
        }
    }

    pub fn next(self) -> Self {
        match self {
            Self::Close => Self::Refresh,
            Self::Refresh => Self::Redeem,
            Self::Redeem => Self::Close,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResetConfirmSelection {
    Cancel,
    Redeem,
}

impl ResetConfirmSelection {
    pub fn toggle(self) -> Self {
        match self {
            Self::Cancel => Self::Redeem,
            Self::Redeem => Self::Cancel,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountUsageSnapshot {
    pub plan: String,
    pub credits: String,
    pub limit_reached: String,
    pub reset_credits: Option<i64>,
    pub rows: Vec<AccountUsageRow>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountUsageRow {
    pub limit: String,
    pub window: String,
    pub used: String,
    pub reached: String,
    pub resets: String,
    pub duration: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BrowserSource {
    List,
    Search,
}

#[derive(Debug, Clone)]
pub struct BrowserState {
    pub multi_server: bool,
    pub source: BrowserSource,
    pub query: String,
    pub rows: Vec<ThreadRow>,
    pub selected: usize,
    /// First row rendered in the browser table; kept in sync with `selected`
    /// each frame so the selection stays inside the visible window.
    pub row_offset: usize,
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
    pub preview: BrowserPreviewState,
    pub last_refresh_at: Option<Instant>,
    pub last_error: Option<String>,
}

impl BrowserState {
    /// Clamps `row_offset` so the selected row stays within the
    /// `visible_rows`-tall window the browser table can actually render.
    pub fn clamp_row_offset(&mut self, visible_rows: usize) {
        if self.rows.is_empty() || visible_rows == 0 {
            self.row_offset = 0;
            return;
        }
        self.row_offset = self
            .row_offset
            .min(self.rows.len().saturating_sub(visible_rows));
        if self.selected < self.row_offset {
            self.row_offset = self.selected;
        } else if self.selected >= self.row_offset + visible_rows {
            self.row_offset = self.selected + 1 - visible_rows;
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct BrowserPreviewState {
    pub epoch: u64,
    pub server: Option<String>,
    pub thread_id: Option<String>,
    pub loading: bool,
    pub messages: Vec<MessageBlock>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadRow {
    pub server: String,
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
    pub server: String,
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
    pub last_refresh_at: Option<Instant>,
    pub viewport_height: Option<u16>,
    pub viewport_width: Option<u16>,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageBlock {
    pub turn_id: Option<String>,
    pub item_id: Option<String>,
    pub role: String,
    pub timestamp: Option<String>,
    /// Unrendered message text. `lines` is the wrapped markdown rendering and
    /// cannot be used to recover the original text for prefix comparisons.
    pub raw_text: String,
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
// `Rgb` is only constructed by the syntax-highlighting path; without that
// feature it is matched but never built.
#[cfg_attr(not(feature = "tui-syntax-highlighting"), allow(dead_code))]
pub enum MessageColor {
    Rgb(u8, u8, u8),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComposeState {
    pub target: ComposeTarget,
    pub text: String,
    pub send_mode: SendMode,
    pub return_to_detail: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ComposeTarget {
    NewTurn {
        server: String,
        thread_id: String,
    },
    NewThread {
        server: String,
        cwd: String,
        title: Option<String>,
    },
    Steer {
        server: String,
        thread_id: String,
        turn_id: String,
    },
    SteerSelected {
        server: String,
        thread_id: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendMode {
    Stream,
    NoWait,
}

#[derive(Debug, Clone)]
pub struct StreamState {
    pub id: u64,
    pub server: String,
    pub thread_id: String,
    pub turn_id: Option<String>,
    pub status: StreamStatus,
    pub assistant_items: Vec<StreamAssistantItem>,
    pub attached: bool,
    pub detached: bool,
    pub last_error: Option<String>,
    pub last_poll_at: Option<Instant>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamAssistantItem {
    pub turn_id: Option<String>,
    pub item_id: Option<String>,
    pub text: String,
}

impl StreamState {
    #[cfg(test)]
    pub fn new(
        thread_id: String,
        turn_id: Option<String>,
        status: StreamStatus,
        attached: bool,
    ) -> Self {
        Self::new_with_id(0, thread_id, turn_id, status, attached)
    }

    #[cfg(test)]
    pub fn new_with_id(
        id: u64,
        thread_id: String,
        turn_id: Option<String>,
        status: StreamStatus,
        attached: bool,
    ) -> Self {
        Self::new_for_server_with_id(id, "work".to_string(), thread_id, turn_id, status, attached)
    }

    pub fn new_for_server_with_id(
        id: u64,
        server: String,
        thread_id: String,
        turn_id: Option<String>,
        status: StreamStatus,
        attached: bool,
    ) -> Self {
        Self {
            id,
            server,
            thread_id,
            turn_id,
            status,
            assistant_items: Vec::new(),
            attached,
            detached: false,
            last_error: None,
            last_poll_at: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamStatus {
    Starting,
    Running,
    /// Probing for a queued follow-up turn after one completed; nothing is
    /// streaming yet.
    Following,
    Completed,
    Failed,
    Interrupted,
    Detached,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DetailJump {
    Start,
    End,
}

#[derive(Debug, Clone)]
pub struct TuiState {
    pub mode: Mode,
    pub browser: BrowserState,
    pub detail: Option<DetailState>,
    pub prefs: TuiPrefs,
    pub stream: Option<StreamState>,
    pub stream_control: Option<mpsc::UnboundedSender<TurnControl>>,
    pub next_stream_id: u64,
    pub pending_goto_top: bool,
    pub pending_detail_jump: Option<DetailJump>,
    pub force_terminal_clear: bool,
    pub notice: Option<Notice>,
    pub should_quit: bool,
}

#[derive(Debug, Clone)]
pub struct Notice {
    pub message: String,
    pub expires_at: Instant,
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
        let auto_refresh_seconds = init.prefs.refresh.interval_seconds.clamp(5, 300);
        Self {
            mode: Mode::Browser,
            browser: BrowserState {
                multi_server: false,
                source,
                query,
                rows: Vec::new(),
                selected: 0,
                row_offset: 0,
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
                preview: BrowserPreviewState::default(),
                last_refresh_at: None,
                last_error: None,
            },
            detail: None,
            prefs: init.prefs,
            stream: None,
            stream_control: None,
            next_stream_id: 0,
            pending_goto_top: false,
            pending_detail_jump: None,
            force_terminal_clear: false,
            notice: None,
            should_quit: false,
        }
    }

    pub fn visible_columns(&self) -> &VisibleColumns {
        &self.prefs.browser.columns
    }

    pub fn set_notice(&mut self, message: impl Into<String>) {
        self.notice = Some(Notice {
            message: message.into(),
            expires_at: Instant::now() + Duration::from_secs(2),
        });
    }

    pub fn allocate_stream_id(&mut self) -> u64 {
        self.next_stream_id += 1;
        self.next_stream_id
    }

    pub fn clear_expired_notice(&mut self) {
        if self
            .notice
            .as_ref()
            .is_some_and(|notice| Instant::now() >= notice.expires_at)
        {
            self.notice = None;
        }
    }

    pub fn selected_thread_id(&self) -> Option<&str> {
        self.browser
            .rows
            .get(self.browser.selected)
            .map(|row| row.id.as_str())
    }

    pub fn selected_thread_key(&self) -> Option<(String, String)> {
        self.browser
            .rows
            .get(self.browser.selected)
            .map(|row| (row.server.clone(), row.id.clone()))
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

    pub fn set_preview_loading(&mut self, server: String, thread_id: String) -> u64 {
        self.browser.preview.epoch += 1;
        self.browser.preview.server = Some(server);
        self.browser.preview.thread_id = Some(thread_id);
        self.browser.preview.loading = true;
        self.browser.preview.messages.clear();
        self.browser.preview.error = None;
        self.browser.preview.epoch
    }

    pub fn set_preview_loaded(
        &mut self,
        epoch: u64,
        server: String,
        thread_id: String,
        mut messages: Vec<MessageBlock>,
    ) {
        if self.browser.preview.epoch != epoch
            || self.browser.preview.server.as_deref() != Some(server.as_str())
            || self.browser.preview.thread_id.as_deref() != Some(thread_id.as_str())
        {
            return;
        }
        if !self.browser.preview.messages.is_empty()
            && self.stream_owns_thread_content(&server, &thread_id)
        {
            // The stream owns the preview content; the fetched history lags it.
            self.browser.preview.loading = false;
            self.browser.preview.error = None;
            return;
        }
        append_unpersisted_messages(&mut messages, &self.browser.preview.messages);
        self.browser.preview.loading = false;
        self.browser.preview.messages = messages;
        self.browser.preview.error = None;
    }

    pub fn set_preview_error(
        &mut self,
        epoch: u64,
        server: String,
        thread_id: String,
        error: String,
    ) {
        if self.browser.preview.epoch != epoch
            || self.browser.preview.server.as_deref() != Some(server.as_str())
            || self.browser.preview.thread_id.as_deref() != Some(thread_id.as_str())
        {
            return;
        }
        self.browser.preview.loading = false;
        self.browser.preview.error = Some(error);
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
        let previous_key = self.selected_thread_key();
        let previous_rows = self.browser.rows.clone();
        let rows = rows
            .into_iter()
            .map(|row| preserve_known_status(row, &previous_rows))
            .collect();
        self.browser.rows = group_running_rows_first(rows);
        self.browser.next_cursor = next_cursor;
        self.browser.backwards_cursor = backwards_cursor;
        self.browser.current_cursor = current_cursor;
        self.browser.loading = false;
        self.browser.last_refresh_at = Some(Instant::now());
        self.browser.last_error = None;
        self.browser.selected = previous_key
            .and_then(|(server, id)| {
                self.browser
                    .rows
                    .iter()
                    .position(|row| row.server == server && row.id == id)
            })
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

    /// Whether a delivering stream owns this thread's content. While it does,
    /// history fetches must not touch transcript content: the stream seeded
    /// the history snapshot at attach time and applies all further updates.
    /// The post-turn follow probe (`Following`) does not own anything.
    pub fn stream_owns_thread_content(&self, server: &str, thread_id: &str) -> bool {
        self.stream.as_ref().is_some_and(|stream| {
            stream.server == server
                && stream.thread_id == thread_id
                && !stream.detached
                && matches!(
                    stream.status,
                    StreamStatus::Starting | StreamStatus::Running
                )
        })
    }

    pub fn replace_detail(&mut self, epoch: u64, detail: DetailState) {
        self.replace_detail_inner(epoch, detail, false);
    }

    /// Replace driven by the stream itself (the attach history snapshot);
    /// bypasses the stream content-ownership guard.
    pub fn replace_detail_from_stream(&mut self, epoch: u64, detail: DetailState) {
        self.replace_detail_inner(epoch, detail, true);
    }

    fn replace_detail_inner(&mut self, epoch: u64, mut detail: DetailState, from_stream: bool) {
        let Some(current) = self.detail.as_ref() else {
            return;
        };
        if current.epoch != epoch {
            return;
        }
        let same_thread = current.thread_id == detail.thread_id;
        if !from_stream
            && same_thread
            && !current.messages.is_empty()
            && self.stream_owns_thread_content(&detail.server, &detail.thread_id)
        {
            // The stream owns the transcript: adopt refreshed metadata only.
            let current = self.detail.as_mut().expect("detail checked above");
            current.title = detail.title;
            current.status = detail.status;
            current.annotation = detail.annotation;
            current.active_turn_id = detail.active_turn_id;
            current.loading = false;
            current.last_refresh_at = Some(Instant::now());
            current.last_error = None;
            return;
        }
        let previous_mode = self.mode.clone();
        if current.thread_id == detail.thread_id {
            let was_at_bottom =
                current.scroll == u16::MAX || current.scroll >= current.max_scroll();
            append_unpersisted_messages(&mut detail.messages, &current.messages);
            detail.search_query = current.search_query.clone();
            detail.viewport_height = current.viewport_height;
            detail.viewport_width = current.viewport_width;
            detail.scroll = if was_at_bottom {
                detail.bottom_scroll_position()
            } else {
                current.scroll.min(detail.max_scroll())
            };
        }
        detail.last_refresh_at = Some(Instant::now());
        self.detail = Some(detail);
        if let Some(query) = self
            .detail
            .as_ref()
            .map(|detail| detail.search_query.clone())
            .filter(|query| !query.is_empty())
        {
            self.update_message_search(query);
        }
        self.mode = preserve_detail_overlay_mode(previous_mode, same_thread);
    }

    pub fn extend_detail_older(&mut self, epoch: u64, mut page: DetailState) {
        let stream_owns_content = self.detail.as_ref().is_some_and(|detail| {
            detail.epoch == epoch
                && detail.thread_id == page.thread_id
                && self.stream_owns_thread_content(&detail.server, &detail.thread_id)
        });
        let Some(detail) = &mut self.detail else {
            return;
        };
        if detail.epoch != epoch || detail.thread_id != page.thread_id {
            return;
        }
        if stream_owns_content {
            detail.next_cursor = page.next_cursor;
            detail.backwards_cursor = page.backwards_cursor.or(detail.backwards_cursor.clone());
            detail.current_cursor = page.current_cursor;
            detail.active_turn_id = page.active_turn_id;
            detail.status = page.status;
            detail.loading = false;
            detail.last_refresh_at = Some(Instant::now());
            detail.last_error = None;
            return;
        }
        let previous_offset = detail.scroll as usize;
        let width = detail.transcript_width();
        let previous_first = detail.messages.first().cloned();
        let mut merged = Vec::new();
        append_unique_messages(&mut merged, page.messages.drain(..));
        append_unique_messages(&mut merged, detail.messages.drain(..));
        let prepended_lines = previous_first
            .as_ref()
            .and_then(|first| {
                merged
                    .iter()
                    .position(|message| same_message(message, first))
            })
            .map(|index| transcript_scroll_offset(&merged, index, width))
            .unwrap_or_else(|| transcript_rendered_line_count(&merged, width));
        detail.messages = merged;
        if previous_offset > 0 || prepended_lines > 0 {
            detail.scroll = previous_offset
                .saturating_add(prepended_lines)
                .min(u16::MAX as usize) as u16;
        }
        detail.next_cursor = page.next_cursor;
        detail.backwards_cursor = page.backwards_cursor.or(detail.backwards_cursor.clone());
        detail.current_cursor = page.current_cursor;
        detail.active_turn_id = page.active_turn_id;
        detail.status = page.status;
        detail.loading = false;
        detail.last_refresh_at = Some(Instant::now());
        detail.last_error = None;
        let query = detail.search_query.clone();
        if !query.is_empty() {
            self.update_message_search(query);
        }
    }

    pub fn extend_detail_newer(&mut self, epoch: u64, mut page: DetailState) {
        let stream_owns_content = self.detail.as_ref().is_some_and(|detail| {
            detail.epoch == epoch
                && detail.thread_id == page.thread_id
                && self.stream_owns_thread_content(&detail.server, &detail.thread_id)
        });
        let Some(detail) = &mut self.detail else {
            return;
        };
        if detail.epoch != epoch || detail.thread_id != page.thread_id {
            return;
        }
        if stream_owns_content {
            detail.next_cursor = page.next_cursor.or(detail.next_cursor.clone());
            detail.backwards_cursor = page.backwards_cursor;
            detail.current_cursor = page.current_cursor;
            detail.active_turn_id = page.active_turn_id;
            detail.status = page.status;
            detail.loading = false;
            detail.last_refresh_at = Some(Instant::now());
            detail.last_error = None;
            return;
        }
        let was_at_bottom = detail.scroll == u16::MAX || detail.scroll >= detail.max_scroll();
        append_unique_messages(&mut detail.messages, page.messages.drain(..));
        detail.next_cursor = page.next_cursor.or(detail.next_cursor.clone());
        detail.backwards_cursor = page.backwards_cursor;
        detail.current_cursor = page.current_cursor;
        detail.active_turn_id = page.active_turn_id;
        detail.status = page.status;
        detail.loading = false;
        detail.last_refresh_at = Some(Instant::now());
        detail.last_error = None;
        if was_at_bottom {
            detail.scroll = detail.bottom_scroll_position();
        }
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
            self.pending_detail_jump = None;
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

fn preserve_detail_overlay_mode(previous_mode: Mode, same_thread: bool) -> Mode {
    if !same_thread {
        return Mode::Detail;
    }
    match previous_mode {
        Mode::Detail
        | Mode::MessageSearchInput { .. }
        | Mode::AnnotationInput {
            return_to_detail: true,
            ..
        }
        | Mode::RenameInput {
            return_to_detail: true,
            ..
        }
        | Mode::Compose(ComposeState {
            return_to_detail: true,
            ..
        })
        | Mode::ConfirmInterrupt { .. }
        | Mode::ConfirmArchive {
            return_to_detail: true,
            ..
        }
        | Mode::ConfirmOpenCodex {
            return_to_detail: true,
            ..
        } => previous_mode,
        _ => Mode::Detail,
    }
}

fn preserve_known_status(mut row: ThreadRow, previous_rows: &[ThreadRow]) -> ThreadRow {
    if !row.status.eq_ignore_ascii_case("notLoaded") {
        return row;
    }
    let Some(previous) = previous_rows
        .iter()
        .find(|previous| previous.server == row.server && previous.id == row.id)
    else {
        return row;
    };
    if previous.status.is_empty() || previous.status.eq_ignore_ascii_case("notLoaded") {
        return row;
    }
    row.set_status(previous.status.clone());
    row
}

fn set_thread_row_raw_status(row: &mut ThreadRow, status: &str) {
    if let Some(raw_thread) = row.raw.get_mut("thread") {
        raw_thread["status"]["type"] = serde_json::json!(status);
    } else {
        row.raw["status"]["type"] = serde_json::json!(status);
    }
}

fn group_running_rows_first(rows: Vec<ThreadRow>) -> Vec<ThreadRow> {
    let (mut running, idle): (Vec<_>, Vec<_>) = rows.into_iter().partition(ThreadRow::is_running);
    running.extend(idle);
    running
}

impl ThreadRow {
    pub fn is_running(&self) -> bool {
        matches!(
            self.status.to_ascii_lowercase().as_str(),
            "active" | "running" | "inprogress" | "starting"
        )
    }

    pub fn set_status(&mut self, status: impl Into<String>) {
        let status = status.into();
        self.status = status.clone();
        set_thread_row_raw_status(self, &status);
    }
}

impl DetailState {
    pub fn set_viewport_size(&mut self, height: u16, width: u16) {
        self.viewport_height = Some(height.max(1));
        self.viewport_width = Some(width.max(1));
        if self.scroll == u16::MAX && self.transcript_line_count() == 0 {
            return;
        }
        self.scroll = self.scroll.min(self.max_scroll());
    }

    pub fn bottom_scroll_position(&self) -> u16 {
        if self.viewport_height.is_none() {
            return u16::MAX;
        }
        self.max_scroll()
    }

    pub fn is_at_bottom(&self) -> bool {
        self.scroll == u16::MAX || self.scroll >= self.max_scroll()
    }

    pub fn max_scroll(&self) -> u16 {
        let visible_height = self.viewport_height.unwrap_or(1).max(1) as usize;
        self.transcript_line_count()
            .saturating_sub(visible_height)
            .min(u16::MAX as usize) as u16
    }

    pub fn message_scroll_offset(&self, message_index: usize) -> usize {
        let width = self.transcript_width();
        transcript_scroll_offset(&self.messages, message_index, width)
    }

    pub fn transcript_line_count(&self) -> usize {
        let width = self.transcript_width();
        transcript_rendered_line_count(&self.messages, width)
    }

    fn transcript_width(&self) -> usize {
        self.viewport_width
            .unwrap_or(DEFAULT_TRANSCRIPT_WIDTH)
            .max(1) as usize
    }
}

pub const DEFAULT_TRANSCRIPT_WIDTH: u16 = 100;

pub fn transcript_rendered_line_count(messages: &[MessageBlock], width: usize) -> usize {
    let mut previous_turn_id: Option<&str> = None;
    let mut previous_role: Option<&str> = None;
    let mut count = 0usize;
    for message in messages {
        count += message_rendered_line_count_with_header(
            message,
            width,
            message_header_visible(message, previous_turn_id, previous_role),
        );
        previous_turn_id = message.turn_id.as_deref();
        previous_role = Some(message.role.as_str());
    }
    count
}

pub fn transcript_scroll_offset(
    messages: &[MessageBlock],
    message_index: usize,
    width: usize,
) -> usize {
    let mut previous_turn_id: Option<&str> = None;
    let mut previous_role: Option<&str> = None;
    let mut offset = 0usize;
    for message in messages.iter().take(message_index) {
        offset += message_rendered_line_count_with_header(
            message,
            width,
            message_header_visible(message, previous_turn_id, previous_role),
        );
        previous_turn_id = message.turn_id.as_deref();
        previous_role = Some(message.role.as_str());
    }
    offset
}

pub fn message_header_visible(
    message: &MessageBlock,
    previous_turn_id: Option<&str>,
    previous_role: Option<&str>,
) -> bool {
    let turn_id = message.turn_id.as_deref();
    let same_turn = turn_id.is_some() && turn_id == previous_turn_id;
    let same_role = previous_role == Some(message.role.as_str());
    !(same_turn && same_role)
}

fn message_rendered_line_count_with_header(
    message: &MessageBlock,
    width: usize,
    show_header: bool,
) -> usize {
    usize::from(show_header)
        + message
            .lines
            .iter()
            .map(|line| rendered_line_count(&line.text, width))
            .sum::<usize>()
        + 1
}

pub fn rendered_line_count(text: &str, width: usize) -> usize {
    if text.is_empty() {
        1
    } else {
        textwrap::wrap(text, textwrap::Options::new(width.max(1)).break_words(true))
            .len()
            .max(1)
    }
}

fn append_unique_messages(
    target: &mut Vec<MessageBlock>,
    messages: impl IntoIterator<Item = MessageBlock>,
) {
    for message in messages {
        let duplicate = target
            .iter()
            .any(|existing| same_message(existing, &message));
        if !duplicate {
            target.push(message);
        }
    }
}

fn append_unpersisted_messages(target: &mut Vec<MessageBlock>, current: &[MessageBlock]) {
    for message in current
        .iter()
        .filter(|message| is_unpersisted_local_message(message))
    {
        if !target
            .iter()
            .any(|existing| equivalent_persisted_message(existing, message))
        {
            target.push(message.clone());
        }
    }
}

fn is_unpersisted_local_message(message: &MessageBlock) -> bool {
    message.item_id.is_none() && matches!(message.role.as_str(), "user" | "assistant")
}

fn equivalent_persisted_message(existing: &MessageBlock, local: &MessageBlock) -> bool {
    if existing.role != local.role || existing.lines != local.lines {
        return false;
    }
    match (existing.turn_id.as_deref(), local.turn_id.as_deref()) {
        (Some(existing_turn), Some(local_turn)) => existing_turn == local_turn,
        _ => existing.item_id.is_none() && local.turn_id.is_none(),
    }
}

fn same_message(left: &MessageBlock, right: &MessageBlock) -> bool {
    if left.item_id.is_some() || right.item_id.is_some() {
        left.item_id == right.item_id || same_persisted_live_item(left, right)
    } else {
        left.turn_id == right.turn_id && left.role == right.role && left.lines == right.lines
    }
}

fn same_persisted_live_item(left: &MessageBlock, right: &MessageBlock) -> bool {
    left.item_id.is_some()
        && right.item_id.is_some()
        && left.turn_id.is_some()
        && left.turn_id == right.turn_id
        && left.role == right.role
        && left.raw_text == right.raw_text
}
