//! Developer bearer token scraped from `music.apple.com`.
//!
//! Apple embeds a shared web-player JWT in one of its `index~*.js` bundles.
//! It is the same token for every user and is only used for the
//! `webPlayback`/license calls — the per-user `media-user-token` (MUT)
//! still comes from the app's own sign-in.

use crate::error::{PlaybackError, Result};
use std::sync::OnceLock;

static CACHED: OnceLock<String> = OnceLock::new();

const UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36";

/// Find the `index~*.js` bundle path inside the music.apple.com HTML.
pub fn find_bundle_path(html: &str) -> Option<String> {
    let start = html.find("/assets/index~")?;
    let rest = &html[start..];
    let end = rest
        .find(|c: char| ['"', '\'', '<', ' ', ')'].contains(&c))
        .unwrap_or(rest.len());
    let path = &rest[..end];
    if path.ends_with(".js") {
        Some(path.to_string())
    } else {
        None
    }
}

/// Find the first JWT (`eyJ....`) in JS text.
pub fn find_jwt(js: &str) -> Option<String> {
    let bytes = js.as_bytes();
    let mut i = 0;
    while i + 3 <= bytes.len() {
        if &bytes[i..i + 3] == b"eyJ" {
            let mut j = i + 3;
            while j < bytes.len()
                && (bytes[j].is_ascii_alphanumeric()
                    || bytes[j] == b'-'
                    || bytes[j] == b'_'
                    || bytes[j] == b'.')
            {
                j += 1;
            }
            if j - i > 64 {
                if let Ok(s) = std::str::from_utf8(&bytes[i..j]) {
                    return Some(s.to_string());
                }
            }
            i = j.max(i + 1);
        } else {
            i += 1;
        }
    }
    None
}

pub async fn get_bearer_token() -> Result<String> {
    if let Some(t) = CACHED.get() {
        return Ok(t.clone());
    }
    let client = crate::error::http_client()?;
    let html = client
        .get("https://music.apple.com")
        .header(reqwest::header::USER_AGENT, UA)
        .send()
        .await
        .map_err(|e| PlaybackError::Network(format!("fetch music.apple.com: {e}")))?
        .text()
        .await
        .map_err(|e| PlaybackError::Network(format!("read landing page: {e}")))?;
    let path = find_bundle_path(&html)
        .ok_or_else(|| PlaybackError::Auth("no index~*.js on music.apple.com".into()))?;
    let js = client
        .get(format!("https://music.apple.com{path}"))
        .header(reqwest::header::USER_AGENT, UA)
        .send()
        .await
        .map_err(|e| PlaybackError::Network(format!("fetch js bundle: {e}")))?
        .text()
        .await
        .map_err(|e| PlaybackError::Network(format!("read js bundle: {e}")))?;
    let token =
        find_jwt(&js).ok_or_else(|| PlaybackError::Auth("no bearer JWT in bundle".into()))?;
    let _ = CACHED.set(token.clone());
    Ok(token)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundle_path_found() {
        let html = r#"<script src="/assets/index~abc123.js"></script>"#;
        assert_eq!(
            find_bundle_path(html).as_deref(),
            Some("/assets/index~abc123.js")
        );
        assert_eq!(find_bundle_path("<html></html>"), None);
    }

    #[test]
    fn jwt_found() {
        let tok = format!("eyJ{}", "aB-_.".repeat(20));
        let js = format!("var t=\"{tok}\";");
        assert_eq!(find_jwt(&js).as_deref(), Some(tok.as_str()));
        assert_eq!(find_jwt("nothing here"), None);
    }
}
