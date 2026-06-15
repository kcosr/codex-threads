#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub(crate) struct ExitError {
    pub(crate) code: i32,
    pub(crate) message: String,
}

pub(crate) fn usage_error(message: impl Into<String>) -> anyhow::Error {
    ExitError {
        code: 2,
        message: message.into(),
    }
    .into()
}

pub(crate) fn app_server_error(message: impl Into<String>) -> anyhow::Error {
    ExitError {
        code: 3,
        message: message.into(),
    }
    .into()
}
