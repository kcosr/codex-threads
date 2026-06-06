use serde_json::Value;

use crate::cli::SortKey;
use crate::tui::state::StreamStatus;
use crate::tui::state::{BrowserSource, DetailState, MessageBlock, ThreadRow};

#[derive(Debug, Clone)]
pub(crate) struct BrowserQuery {
    pub source: BrowserSource,
    pub query: String,
    pub cursor: Option<String>,
    pub limit: u32,
    pub since: Option<i64>,
    pub cwd: Option<String>,
    pub archived: bool,
    pub sort: Option<SortKey>,
    pub descending: bool,
    pub relative_updated: bool,
}

#[derive(Debug)]
pub(crate) enum FetchRequest {
    Browser {
        epoch: u64,
        query: BrowserQuery,
    },
    Detail {
        epoch: u64,
        thread_id: String,
        cursor: Option<String>,
        limit: u32,
        page_direction: DetailPageDirection,
    },
    LoadThread {
        thread_id: String,
    },
}

#[derive(Debug)]
pub(crate) struct PreviewRequest {
    pub epoch: u64,
    pub thread_id: String,
}

#[derive(Debug)]
pub(crate) enum AppEvent {
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
        page_direction: DetailPageDirection,
    },
    DetailLoadFailed {
        epoch: u64,
        error: String,
    },
    PreviewLoaded {
        epoch: u64,
        thread_id: String,
        messages: Vec<MessageBlock>,
    },
    PreviewLoadFailed {
        epoch: u64,
        thread_id: String,
        error: String,
    },
    ThreadLoaded {
        thread_id: String,
        status: Value,
    },
    ThreadLoadFailed {
        thread_id: String,
        error: String,
    },
    StreamEvent {
        stream_id: Option<u64>,
        event: Value,
    },
    StreamFailed {
        stream_id: Option<u64>,
        thread_id: String,
        turn_id: Option<String>,
        error: String,
    },
    StreamFinished {
        stream_id: u64,
        thread_id: String,
        turn_id: Option<String>,
        status: StreamStatus,
    },
    TurnSubmitted {
        thread_id: String,
    },
    TurnSubmitFailed {
        thread_id: String,
        error: String,
    },
    ArchiveChanged {
        thread_id: String,
        archived: bool,
        thread: Value,
    },
    ArchiveChangeFailed {
        thread_id: String,
        archived: bool,
        error: String,
    },
    RenameChanged {
        thread_id: String,
        name: String,
        thread: Value,
    },
    RenameChangeFailed {
        thread_id: String,
        name: String,
        error: String,
    },
    ShutdownSignal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DetailPageDirection {
    Replace,
    Older,
    Newer,
}
