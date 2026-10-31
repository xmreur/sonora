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
    /// Background work (prefetch/download) stepped aside for a play
    /// request that needs the CDM license slot. Not a failure: callers
    /// back off and retry after the play settles.
    #[error("yielded to playback")]
    Yielded,
}

pub type Result<T> = std::result::Result<T, PlaybackError>;

use std::sync::OnceLock;

static SHARED_CLIENT: OnceLock<reqwest::Client> = OnceLock::new();

/// Process-wide HTTP client (connection pool reuse). Every playback request
/// previously built its own `Client` (fresh pool → fresh TLS handshakes for
/// the webPlayback POST, M3U8 GET, asset GET and license POST of EVERY
/// track). Cloning the handle is cheap; pools are shared.
///
/// Every timeout here is load-bearing: without them a wedged TCP
/// connection in `webPlayback`/license/asset fetch left the play task
/// pending forever, and the song change froze at 0:00 until the app was
/// killed. Connect stalls fail in 10s, idle reads in 20s, and a whole
/// request (incl. body) never exceeds 60s — all of which flow into the
/// normal retry machinery instead of hanging.
pub(crate) fn shared_client() -> reqwest::Client {
    SHARED_CLIENT
        .get_or_init(|| {
            reqwest::Client::builder()
                .user_agent("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36")
                .connect_timeout(std::time::Duration::from_secs(10))
                .read_timeout(std::time::Duration::from_secs(20))
                .timeout(std::time::Duration::from_secs(60))
                .build()
                .expect("shared http client builds")
        })
        .clone()
}

pub(crate) fn http_client() -> std::result::Result<reqwest::Client, PlaybackError> {
    Ok(shared_client())
}
