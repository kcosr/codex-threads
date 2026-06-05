#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InputAction {
    None,
    RefreshBrowser,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ModeKind {
    Search,
    MessageSearch,
    Annotation { thread_id: String },
}
