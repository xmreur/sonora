use thiserror::Error;

#[derive(Debug, Error)]
pub enum PlaybackError {
    #[error("network: {0}")]
    Network(String),
    #[error("auth: {0}")]
    Auth(String),
    #[error("resolve: {0}")]
    Resolve(String),
    #[error("license: {0}")]
    License(String),
    #[error("decrypt: {0}")]
    Decrypt(String),
    #[error("decode: {0}")]
    Decode(String),
    #[error("audio: {0}")]
    Audio(String),
    #[error("unsupported: {0}")]
    Unsupported(String),
    #[error(" widevine CDM: {0}")]
    Cdm(String),
}

pub type Result<T> = std::result::Result<T, PlaybackError>;

pub(crate) fn http_client() -> std::result::Result<reqwest::Client, PlaybackError> {
    reqwest::Client::builder()
        .user_agent("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36")
        .build()
        .map_err(|e| PlaybackError::Network(format!("http client: {e}")))
}
