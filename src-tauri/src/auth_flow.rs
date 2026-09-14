//! Automatic Apple Music sign-in (user token) via a one-shot localhost server.
//!
//! Flow (same idea as matteing/am-keyman, reimplemented here with no extra
//! deps — raw HTTP over tokio, mirroring `sidecar.rs`):
//! 1. `run_signin_server` binds `127.0.0.1:0` and serves an auth page that
//!    loads MusicKit JS with our developer token.
//! 2. The user clicks Authorize there; Apple's popup approves; MusicKit's
//!    `authorize()` resolves with the music-user-token directly in JS.
//! 3. The page POSTs it to `/token`; the server hands it to the Tauri
//!    command through a oneshot channel and shuts down.
//! No copy-paste involved. The old manual paste flow stays as a fallback.

use tokio::sync::oneshot;

const AUTH_PAGE_HTML: &str = include_str!("auth_page.html");

const SUCCESS_HTML: &str = "<!doctype html><html><body style=\"font-family:system-ui;background:#14141a;color:#f2f2f5;text-align:center;padding-top:64px\"><h1>Signed in</h1><p>You can close this tab and return to Sonora.</p></body></html>";

/// Outcome of one auth-server route: the HTTP response plus, on a valid
/// token POST, the token that completes the sign-in.
pub enum AuthRoute {
    Respond {
        status: &'static str,
        ctype: &'static str,
        payload: Vec<u8>,
    },
    Complete {
        status: &'static str,
        ctype: &'static str,
        payload: Vec<u8>,
        token: String,
    },
}

/// Pure route handler (no sockets — unit-tested). `page` is the auth HTML
/// with the developer token already injected.
pub fn auth_route(method: &str, path: &str, body: &[u8], page: &str) -> AuthRoute {
    let respond = |status: &'static str, ctype: &'static str, payload: Vec<u8>| AuthRoute::Respond {
        status,
        ctype,
        payload,
    };
    match (method, path) {
        ("GET", "/") | ("GET", "/index.html") => {
            respond("200 OK", "text/html; charset=utf-8", page.as_bytes().to_vec())
        }
        ("POST", "/token") => match extract_posted_token(body) {
            Some(token) => AuthRoute::Complete {
                status: "200 OK",
                ctype: "text/html; charset=utf-8",
                payload: SUCCESS_HTML.as_bytes().to_vec(),
                token,
            },
            None => respond(
                "400 Bad Request",
                "text/plain",
                b"missing or empty token".to_vec(),
            ),
        },
        _ => respond("404 Not Found", "text/plain", b"nope".to_vec()),
    }
}

/// Pull a non-empty `{"token": "..."}` out of a POST body. Pure, tested.
pub fn extract_posted_token(body: &[u8]) -> Option<String> {
    let v: serde_json::Value = serde_json::from_slice(body).ok()?;
    let t = v.get("token")?.as_str()?.trim();
    if t.is_empty() {
        return None;
    }
    Some(t.to_string())
}

/// Render the auth page with the developer token injected as a JSON-quoted
/// JS string literal (JWTs are base64url — quoting is belt and braces).
pub fn render_auth_page(developer_token: &str) -> String {
    let quoted = serde_json::to_string(developer_token).unwrap_or_else(|_| "\"\"".into());
    AUTH_PAGE_HTML.replacen("__DEV_TOKEN__", &quoted, 1)
}

/// Bind a one-shot sign-in server; returns its port plus the server task.
/// The task ends after a valid token POST (or when the handle is aborted
/// on cancel/timeout/logout).
pub async fn run_signin_server(
    developer_token: String,
    tx: oneshot::Sender<String>,
) -> Result<(u16, tokio::task::JoinHandle<()>), String> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(|e| format!("sign-in bind failed: {e}"))?;
    let port = listener
        .local_addr()
        .map_err(|e| e.to_string())?
        .port();
    let page = render_auth_page(&developer_token);
    let handle = tokio::spawn(async move {
        serve_auth(listener, page, tx).await;
    });
    Ok((port, handle))
}

async fn serve_auth(
    listener: tokio::net::TcpListener,
    page: String,
    tx: oneshot::Sender<String>,
) {
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt};
    let mut tx = Some(tx);
    loop {
        let Ok((stream, _)) = listener.accept().await else {
            break;
        };
        let mut reader = tokio::io::BufReader::new(stream);
        let mut head = Vec::new();
        loop {
            let mut line = String::new();
            match reader.read_line(&mut line).await {
                Ok(0) => break,
                Ok(_) => {}
                Err(_) => break,
            }
            head.extend_from_slice(line.as_bytes());
            if head.ends_with(b"\r\n\r\n") || head.len() > 65536 {
                break;
            }
        }
        let (method, path, _, content_len) =
            crate::sidecar::parse_head(&String::from_utf8_lossy(&head));
        let mut body = vec![0u8; content_len.min(1 << 14)];
        if content_len > 0 {
            let _ = reader.read_exact(&mut body).await;
        }
        let route = auth_route(&method, &path, &body, &page);
        let (status, ctype, payload, complete) = match route {
            AuthRoute::Respond {
                status,
                ctype,
                payload,
            } => (status, ctype, payload, None),
            AuthRoute::Complete {
                status,
                ctype,
                payload,
                token,
            } => (status, ctype, payload, Some(token)),
        };
        let mut stream = reader.into_inner();
        let resp = format!(
            "HTTP/1.1 {status}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            payload.len()
        );
        let _ = stream.write_all(resp.as_bytes()).await;
        let _ = stream.write_all(&payload).await;
        if let Some(token) = complete {
            // Receiver gone (cancelled meanwhile) → nothing to complete.
            if let Some(tx) = tx.take() {
                let _ = tx.send(token);
            }
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_posted_token() {
        assert_eq!(
            extract_posted_token(br#"{"token":"  MUT123  "}"#).as_deref(),
            Some("MUT123")
        );
        assert!(extract_posted_token(br#"{"token":"   "}"#).is_none());
        assert!(extract_posted_token(br#"{"nope":1}"#).is_none());
        assert!(extract_posted_token(b"not json").is_none());
        assert!(extract_posted_token(b"").is_none());
    }

    #[test]
    fn routes_serve_page_and_token() {
        let page = render_auth_page("DEV123");
        assert!(page.contains("\"DEV123\""));
        assert!(page.contains("musickit"));
        assert!(!page.contains("__DEV_TOKEN__"));
        match auth_route("GET", "/", &[], &page) {
            AuthRoute::Respond { status, payload, .. } => {
                assert_eq!(status, "200 OK");
                assert!(String::from_utf8(payload).unwrap().contains("Sonora"));
            }
            AuthRoute::Complete { .. } => panic!("GET / must not complete"),
        }
        match auth_route("POST", "/token", br#"{"token":"MUT9"}"#, &page) {
            AuthRoute::Complete { status, token, .. } => {
                assert_eq!(status, "200 OK");
                assert_eq!(token, "MUT9");
            }
            AuthRoute::Respond { .. } => panic!("valid token must complete"),
        }
        match auth_route("POST", "/token", br#"{"token":""}"#, &page) {
            AuthRoute::Respond { status, .. } => assert_eq!(status, "400 Bad Request"),
            AuthRoute::Complete { .. } => panic!("empty token must not complete"),
        }
        match auth_route("GET", "/favicon.ico", &[], &page) {
            AuthRoute::Respond { status, .. } => assert_eq!(status, "404 Not Found"),
            AuthRoute::Complete { .. } => panic!("unknown path must not complete"),
        }
    }
}
