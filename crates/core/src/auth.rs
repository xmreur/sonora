use serde::{Deserialize, Serialize};

/// Builds the official MusicKit authorize URL (authorize.music.apple.com/woa).
/// The MUT itself comes back via postMessage to the opener window —
/// see ui/player.html + docs/TOKEN_SETUP.md. We only build/parse here so
/// this crate stays GUI-free and testable.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AuthState {
    pub authorized: bool,
    pub storefront: String,
}

pub fn build_authorize_url(developer_token: &str, app_name: &str, referrer: &str) -> String {
    let third_party = serde_json::json!({
        "thirdPartyName": app_name,
        "thirdPartyIconURL": "",
        "thirdPartyToken": developer_token,
    });
    use base64::Engine as _;
    let a = base64::engine::general_purpose::STANDARD.encode(third_party.to_string().as_bytes());
    let params = url_params(&[
        ("a", &a),
        ("referrer", referrer),
        ("app", "music"),
        ("p", "subscribe"),
    ]);
    format!("https://authorize.music.apple.com/woa?{params}")
}

fn url_params(pairs: &[(&str, &str)]) -> String {
    pairs
        .iter()
        .map(|(k, v)| format!("{}={}", percent_encode(k), percent_encode(v)))
        .collect::<Vec<_>>()
        .join("&")
}

fn percent_encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// A `message` event from the auth popup with method=authorize carries
/// params[0] = music-user-token.
pub fn extract_user_token_from_message(msg: &serde_json::Value) -> Option<String> {
    if msg.get("method")?.as_str()? != "authorize" {
        return None;
    }
    msg.get("params")?.get(0)?.as_str().map(|s| s.to_string())
}

/// Decode `%XX` sequences (leaves `+` untouched: Apple's token uses `%2B`).
fn percent_decode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(hex) = std::str::from_utf8(&bytes[i + 1..i + 3]) {
                if let Ok(b) = u8::from_str_radix(hex, 16) {
                    out.push(b as char);
                    i += 3;
                    continue;
                }
            }
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    out
}

/// Extract the MUT from an authorize redirect URL pasted by the user, e.g.
/// `https://authorize.music.apple.com/?...&musicUserToken=0.Abc%2F...&...`.
pub fn extract_user_token_from_url(url: &str) -> Option<String> {
    let query = url.split_once('?')?.1;
    for pair in query.split('&') {
        if let Some(v) = pair.strip_prefix("musicUserToken=") {
            let decoded = percent_decode(v);
            if !decoded.is_empty() {
                return Some(decoded);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn authorize_url_shape() {
        let u = build_authorize_url("DEV", "TestApp", "https://localhost");
        assert!(u.starts_with("https://authorize.music.apple.com/woa?"));
        assert!(u.contains("app=music"));
    }

    #[test]
    fn extracts_mut() {
        let m = serde_json::json!({"method":"authorize","params":["MUT123"]});
        assert_eq!(
            extract_user_token_from_message(&m).as_deref(),
            Some("MUT123")
        );
        let m2 = serde_json::json!({"method":"close"});
        assert!(extract_user_token_from_message(&m2).is_none());
    }

    #[test]
    fn extracts_mut_from_url() {
        let url =
            "https://authorize.music.apple.com/?a=1&musicUserToken=0.Abc%2FDe%2BCg%3D%3D&cid=2";
        assert_eq!(
            extract_user_token_from_url(url).as_deref(),
            Some("0.Abc/De+Cg==")
        );
        assert!(extract_user_token_from_url("https://example.com/?x=1").is_none());
        assert!(extract_user_token_from_url("no-query").is_none());
    }
}
