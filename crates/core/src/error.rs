use thiserror::Error;

#[derive(Debug, Error)]
pub enum CoreError {
    #[error("missing developer token (see docs/TOKEN_SETUP.md)")]
    MissingDeveloperToken,
    #[error("missing music user token (authorize first)")]
    MissingUserToken,
    #[error("http error: {0}")]
    Http(String),
    #[error("invalid token: {0}")]
    InvalidToken(String),
    #[error("engine error [{engine}]: {message}")]
    Engine { engine: String, message: String },
    #[error("unsupported: {0}")]
    Unsupported(String),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

pub type Result<T> = std::result::Result<T, CoreError>;
