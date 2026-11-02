use thiserror::Error;

/// Errors surfaced to the operator log or back to the requesting Telegram chat.
#[derive(Debug, Error)]
pub enum Error {
    #[error("configuration: {0}")]
    Config(String),

    #[error("Telegram API: {0}")]
    Telegram(String),

    #[error("yt-dlp failed: {0}")]
    YtDlp(String),

    #[error("invalid media metadata")]
    Json(#[from] serde_json::Error),

    #[error("operation timed out")]
    Timeout,

    #[error("upload limit exceeded")]
    TooLarge,

    #[error("cache: {0}")]
    Cache(String),

    #[error("queue: {0}")]
    Queue(&'static str),

    #[error("subprocess: {0}")]
    Process(String),

    #[error("subprocess returned an error")]
    ChildExit(Option<i32>),

    #[error("image recognition failed")]
    Image,

    #[error("no usable text in image")]
    ImageEmpty,

    #[error("image exceeds input limits")]
    ImageTooLarge,

    #[error(transparent)]
    Io(#[from] std::io::Error),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;
