use crate::error::{CoreError, Result};

/// Developer token = ES256 JWT `header.payload.signature`.
/// We never verify the signature here (Apple does); we only check
/// shape + expiry so the UI can warn early instead of failing on API calls.
pub fn decode_jwt_expiry(token: &str) -> Result<i64> {
    let parts: Vec<&str> = token.split('.').collect();
    if parts.len() != 3 {
        return Err(CoreError::InvalidToken("expected header.payload.signature".into()));
    }
    use base64::Engine as _;
    let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(parts[1])
        // fall back to standard padded base64
        .or_else(|_| base64::engine::general_purpose::STANDARD.decode(parts[1]))
        .map_err(|e| CoreError::InvalidToken(format!("bad base64 payload: {e}")))?;
    let v: serde_json::Value =
        serde_json::from_slice(&payload).map_err(|e| CoreError::InvalidToken(format!("bad json: {e}")))?;
    v.get("exp")
        .and_then(|e| e.as_i64())
        .ok_or_else(|| CoreError::InvalidToken("missing exp claim".into()))
}

/// Find all `/assets/*.js` bundle URLs inside beta.music.apple.com HTML,
/// ordered legacy-first (the token lives in `index-legacy~<hash>.js`).
/// Note: Apple uses a tilde (`index-legacy~abc.js`), not a dash.
pub fn find_bundle_urls(html: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut search_from = 0;
    while let Some(rel) = html[search_from..].find("/assets/") {
        let start = search_from + rel;
        let rest = &html[start..];
        let end = rest
            .find(|c: char| c == '"' || c == '\'' || c == '<' || c == ' ' || c == ')')
            .unwrap_or(rest.len());
        let url = rest[..end].to_string();
        if url.ends_with(".js") && !out.contains(&url) {
            out.push(url);
        }
        search_from = start + "/assets/".len();
    }
    // Legacy bundle (contains the token) first, then the rest stable.
    out.sort_by_key(|u| !u.contains("legacy"));
    out
}

/// First bundle URL (legacy-preferred). Kept for API compat.
pub fn find_bundle_url(html: &str) -> Option<String> {
    find_bundle_urls(html).into_iter().next()
}

fn is_jwt_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.'
}

/// Extract the first plausible JWT developer token (`eyJ...`) from JS text.
/// Apple's web tokens start with `eyJ0eXAi` (`{"typ":"JWT",...}`), NOT `eyJh`.
/// Validates shape via [`decode_jwt_expiry`] so ad/analytics JWTs are skipped.
pub fn extract_token_from_js(js: &str) -> Option<String> {
    let bytes = js.as_bytes();
    let mut i = 0;
    while i + 3 <= bytes.len() {
        if &bytes[i..i + 3] == b"eyJ" {
            let mut j = i + 3;
            while j < bytes.len() && is_jwt_char(bytes[j]) {
                j += 1;
            }
            if let Ok(cand) = std::str::from_utf8(&bytes[i..j]) {
                let cand = cand.trim_end_matches(|c| c == '\\' || c == 'n');
                if cand.len() > 64 && decode_jwt_expiry(cand).is_ok() {
                    return Some(cand.to_string());
                }
            }
            i = j.max(i + 1);
        } else {
            i += 1;
        }
    }
    None
}

/// Fetch Apple's own web-player developer token (same for everyone).
/// Works with a plain Apple Music subscription — no paid developer account.
/// Fragile by nature: Apple rotates it every few months and may change the
/// bundle layout; callers must surface errors and allow a manual token.
pub async fn fetch_web_player_token(http: &reqwest::Client) -> Result<String> {
    let html = http
        .get("https://beta.music.apple.com")
        .header(reqwest::header::USER_AGENT, "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0 Safari/537.36")
        .send()
        .await
        .map_err(|e| CoreError::Http(format!("web player page: {e}")))?
        .text()
        .await
        .map_err(|e| CoreError::Http(format!("web player body: {e}")))?;
    let bundles = find_bundle_urls(&html);
    if bundles.is_empty() {
        return Err(CoreError::Http("web player bundle not found (Apple changed layout?)".into()));
    }
    // Try each bundle until one yields a token (token chunk varies).
    for bundle in &bundles {
        let js_url = if bundle.starts_with("http") {
            bundle.clone()
        } else {
            format!("https://beta.music.apple.com{bundle}")
        };
        let js = http
            .get(&js_url)
            .header(reqwest::header::USER_AGENT, "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0 Safari/537.36")
            .send()
            .await
            .map_err(|e| CoreError::Http(format!("web player bundle: {e}")))?
            .text()
            .await
            .map_err(|e| CoreError::Http(format!("web player js body: {e}")))?;
        if let Some(token) = extract_token_from_js(&js) {
            return Ok(token);
        }
    }
    Err(CoreError::Http("developer token not found in bundle (Apple rotated format?)".into()))
}

pub fn validate_developer_token(token: &str) -> Result<()> {
    if token.trim().is_empty() {
        return Err(CoreError::MissingDeveloperToken);
    }
    // Apple-issued tokens are long JWTs; short strings are pasted incorrectly.
    if token.trim().len() < 64 {
        return Err(CoreError::InvalidToken("token too short".into()));
    }
    let _ = decode_jwt_expiry(token.trim())?;
    Ok(())
}

/// Source of the developer token. File/env/pasted-string all supported so
/// users without a paid flow yet can still onboard (see docs/TOKEN_SETUP.md).
pub trait TokenProvider: Send + Sync {
    fn developer_token(&self) -> Result<String>;
    fn music_user_token(&self) -> Option<String>;
    fn set_music_user_token(&self, token: String) -> Result<()>;
}

pub struct EnvTokenProvider {
    pub dev_env: String,
    pub mut_store: std::sync::Mutex<Option<String>>,
}

impl EnvTokenProvider {
    pub fn new(dev_env: &str) -> Self {
        Self { dev_env: dev_env.to_string(), mut_store: std::sync::Mutex::new(None) }
    }
}

impl TokenProvider for EnvTokenProvider {
    fn developer_token(&self) -> Result<String> {
        std::env::var(&self.dev_env).map_err(|_| CoreError::MissingDeveloperToken)
    }
    fn music_user_token(&self) -> Option<String> {
        self.mut_store.lock().ok()?.clone()
    }
    fn set_music_user_token(&self, token: String) -> Result<()> {
        *self.mut_store.lock().map_err(|_| CoreError::InvalidToken("lock".into()))? = Some(token);
        Ok(())
    }
}

pub struct FileTokenProvider {
    pub path: std::path::PathBuf,
    pub mut_cache: std::sync::Mutex<Option<String>>,
}

impl FileTokenProvider {
    pub fn new(path: std::path::PathBuf) -> Self {
        Self { path, mut_cache: std::sync::Mutex::new(None) }
    }
}

impl TokenProvider for FileTokenProvider {
    fn developer_token(&self) -> Result<String> {
        let raw = std::fs::read_to_string(&self.path).map_err(|_| CoreError::MissingDeveloperToken)?;
        // First non-empty, non-comment line.
        for line in raw.lines() {
            let t = line.trim();
            if !t.is_empty() && !t.starts_with('#') {
                return Ok(t.to_string());
            }
        }
        Err(CoreError::MissingDeveloperToken)
    }
    fn music_user_token(&self) -> Option<String> {
        self.mut_cache.lock().ok()?.clone()
    }
    fn set_music_user_token(&self, token: String) -> Result<()> {
        *self.mut_cache.lock().map_err(|_| CoreError::InvalidToken("lock".into()))? = Some(token);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;

    fn fake_jwt(exp: i64) -> String {
        let h = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(r#"{"alg":"ES256"}"#);
        let p = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(format!(r#"{{"exp":{exp}}}"#));
        format!("{h}.{p}.{}", "S".repeat(120))
    }

    #[test]
    fn decodes_expiry() {
        assert_eq!(decode_jwt_expiry(&fake_jwt(123)).unwrap(), 123);
    }

    #[test]
    fn rejects_garbage() {
        assert!(decode_jwt_expiry("abc").is_err());
        assert!(validate_developer_token("short").is_err());
    }

    #[test]
    fn finds_bundle_url() {
        // Apple uses a tilde, not a dash: index-legacy~<hash>.js
        let html = r#"<script src="/assets/index~aaa.js"></script><script src="/assets/index-legacy~abc123.js"></script>"#;
        let urls = find_bundle_urls(html);
        assert_eq!(urls.len(), 2);
        assert_eq!(urls[0], "/assets/index-legacy~abc123.js"); // legacy first
        assert_eq!(find_bundle_url(html).as_deref(), Some("/assets/index-legacy~abc123.js"));
        assert!(find_bundle_url("<html></html>").is_none());
    }

    #[test]
    fn extracts_token_from_js() {
        let tok = fake_jwt(999);
        let js = format!("var x=\"prefix {tok} suffix\";");
        assert_eq!(extract_token_from_js(&js).as_deref(), Some(tok.as_str()));
        assert!(extract_token_from_js("no token here").is_none());
    }
}
