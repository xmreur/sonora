//! Automatic Apple Music sign-in (user token) via a one-shot localhost server.
//!
//! Flow (same idea as matteing/am-keyman, reimplemented here with no extra
//! deps — raw HTTP over tokio):
//! 1. `run_signin_server` binds `127.0.0.1:0` and serves an auth page that
//!    loads MusicKit JS with our developer token.
//! 2. The user clicks Authorize there; Apple's popup approves; MusicKit's
//!    `authorize()` resolves with the music-user-token directly in JS.
//! 3. The page POSTs it to `/token`; the server hands it to the waiting
//!    UI call through a oneshot channel and shuts down.
//! 4. If MusicKit setup fails while the tab is open — the injected
//!    developer token expired, or the cached one is stale/poisoned — the
//!    page pulls a fresh one from `GET /dev-token` (once with `force=1`,
//!    which drops the app's caches first) and retries MusicKit setup
//!    instead of dying with `Initialized with an expired token.`.
//!
//! No copy-paste involved. The old manual paste flow stays as a fallback.

use tokio::sync::oneshot;

const AUTH_PAGE_HTML: &str = include_str!("auth_page.html");

const SUCCESS_HTML: &str = "<!doctype html><html><body style=\"font-family:system-ui;background:#14141a;color:#f2f2f5;text-align:center;padding-top:64px\"><h1>Signed in</h1><p>You can close this tab and return to Sonora.</p></body></html>";

/// Async factory for a fresh developer token, so an open sign-in tab can
/// recover when the injected JWT expires (`GET /dev-token`). Built by
/// `commands::start_signin` from the shared state, which means it goes
/// through the same env/cache/scrape resolution as the rest of the app.
///
/// `force` asks the app to drop its cached copies (memory + disk) before
/// resolving: MusicKit can reject a token that still *looks* fresh locally
/// (stale or poisoned `web_token_cache`), and only a forced re-scrape
/// recovers from that. The page sends `force=1` on its one retry.
pub type TokenRefresh = std::sync::Arc<
    dyn Fn(
            bool,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = Result<String, String>> + Send + 'static>,
        > + Send
        + Sync,
>;

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
    let respond =
        |status: &'static str, ctype: &'static str, payload: Vec<u8>| AuthRoute::Respond {
            status,
            ctype,
            payload,
        };
    match (method, path) {
        ("GET", "/") | ("GET", "/index.html") => respond(
            "200 OK",
            "text/html; charset=utf-8",
            page.as_bytes().to_vec(),
        ),
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

/// Parse an HTTP request head: returns (method, path, headers, content_len).
fn parse_head(head: &str) -> (String, String, Vec<(String, String)>, usize) {
    let mut lines = head.lines();
    let req = lines.next().unwrap_or_default();
    let mut parts = req.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let path = parts.next().unwrap_or("/").to_string();
    let mut headers = Vec::new();
    let mut len = 0usize;
    for line in lines {
        if line.is_empty() {
            break;
        }
        if let Some((k, v)) = line.split_once(':') {
            let k = k.trim().to_string();
            let v = v.trim().to_string();
            if k.to_lowercase() == "content-length" {
                len = v.parse().unwrap_or(0);
            }
            headers.push((k, v));
        }
    }
    (method, path, headers, len)
}

/// Pull `Authorization` + `Music-User-Token` out of request headers
/// (case-insensitive). Pure, tested.
pub fn pick_auth_headers(headers: &[(String, String)]) -> (String, String) {
    let mut auth = String::new();
    let mut mut_ = String::new();
    for (k, v) in headers {
        match k.to_ascii_lowercase().as_str() {
            "authorization" => auth = v.clone(),
            "music-user-token" | "x-apple-music-user-token" | "media-user-token" => {
                mut_ = v.clone()
            }
            _ => {}
        }
    }
    (auth, mut_)
}

/// Does the request path carry `force=1`? Pure, tested — the sign-in
/// page appends it to `/dev-token` when MusicKit rejected a token that
/// still looked fresh locally, so the app must drop its caches first.
pub fn force_requested(path: &str) -> bool {
    path.split_once('?')
        .map(|(_, query)| query.split('&').any(|kv| kv == "force=1"))
        .unwrap_or(false)
}

fn status_phrase(code: u16) -> &'static str {
    match code {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        _ => "Unknown",
    }
}

/// Forward the account-storefront lookup to the amp host that accepts
/// shared web-player tokens (with the Origin header Apple demands).
/// Returns Apple's status code + body verbatim.
async fn proxy_storefront(auth: &str, mut_token: &str) -> (u16, Vec<u8>) {
    if auth.trim().is_empty() || mut_token.trim().is_empty() {
        return (400, b"missing Authorization or Music-User-Token".to_vec());
    }
    let client = reqwest::Client::builder()
        .user_agent("Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0 Safari/537.36")
        .build();
    let client = match client {
        Ok(c) => c,
        Err(e) => return (500, format!("http client: {e}").into_bytes()),
    };
    let resp = client
        .get("https://amp-api.music.apple.com/v1/me/storefront")
        .header("Authorization", auth.trim())
        .header("Music-User-Token", mut_token.trim())
        .header("Media-User-Token", mut_token.trim())
        .header("Origin", "https://music.apple.com")
        .header("Referer", "https://music.apple.com/")
        .header("Accept", "application/json")
        .send()
        .await;
    match resp {
        Ok(r) => {
            let code = r.status().as_u16();
            let body = r.bytes().await.map(|b| b.to_vec()).unwrap_or_default();
            (code, body)
        }
        Err(e) => (502, format!("storefront proxy: {e}").into_bytes()),
    }
}

/// Bind a one-shot sign-in server; returns its port plus the server task.
/// The task ends after a valid token POST (or when the handle is aborted
/// on cancel/timeout/logout). `refresh` answers the page's `/dev-token`
/// re-fetch when the injected developer token expires mid-session.
pub async fn run_signin_server(
    developer_token: String,
    refresh: TokenRefresh,
    tx: oneshot::Sender<String>,
) -> Result<(u16, tokio::task::JoinHandle<()>), String> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(|e| format!("sign-in bind failed: {e}"))?;
    let port = listener.local_addr().map_err(|e| e.to_string())?.port();
    let page = render_auth_page(&developer_token);
    let handle = tokio::spawn(async move {
        serve_auth(listener, page, refresh, tx).await;
    });
    Ok((port, handle))
}

async fn serve_auth(
    listener: tokio::net::TcpListener,
    page: String,
    refresh: TokenRefresh,
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
        let (method, path, headers, content_len) = parse_head(&String::from_utf8_lossy(&head));
        let mut body = vec![0u8; content_len.min(1 << 14)];
        if content_len > 0 {
            let _ = reader.read_exact(&mut body).await;
        }
        // Fresh developer token for the page (expired-JWT recovery).
        // Handled here — not in `auth_route` — because it needs async I/O.
        if method == "GET" && (path == "/dev-token" || path.starts_with("/dev-token?")) {
            let force = force_requested(&path);
            let (status, payload) = match refresh(force).await {
                Ok(token) => (
                    "200 OK",
                    serde_json::json!({ "token": token })
                        .to_string()
                        .into_bytes(),
                ),
                Err(e) => (
                    "503 Service Unavailable",
                    serde_json::json!({ "error": e }).to_string().into_bytes(),
                ),
            };
            let mut stream = reader.into_inner();
            let resp = format!(
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nCache-Control: no-store\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                payload.len()
            );
            let _ = stream.write_all(resp.as_bytes()).await;
            let _ = stream.write_all(&payload).await;
            continue;
        }
        // Account-storefront lookup, proxied to the amp host (see above).
        // Handled here — not in `auth_route` — because it needs async I/O.
        if method == "GET" && (path == "/mk-storefront" || path.starts_with("/mk-storefront?")) {
            let (auth, mut_token) = pick_auth_headers(&headers);
            let (code, payload) = proxy_storefront(&auth, &mut_token).await;
            let status = format!("{code} {}", status_phrase(code));
            let mut stream = reader.into_inner();
            let resp = format!(
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                payload.len()
            );
            let _ = stream.write_all(resp.as_bytes()).await;
            let _ = stream.write_all(&payload).await;
            continue;
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
    fn parses_get_head() {
        let (m, p, h, l) = parse_head("GET /cmd HTTP/1.1\r\nHost: x\r\n\r\n");
        assert_eq!((m.as_str(), p.as_str(), l), ("GET", "/cmd", 0));
        assert_eq!(h, vec![("Host".to_string(), "x".to_string())]);
    }

    #[test]
    fn parses_post_len() {
        let (m, p, _, l) = parse_head("POST /token HTTP/1.1\r\nContent-Length: 42\r\n\r\n");
        assert_eq!((m.as_str(), p.as_str(), l), ("POST", "/token", 42));
    }

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
    fn picks_auth_headers_case_insensitively() {
        let h = vec![
            ("Host".to_string(), "x".to_string()),
            ("authorization".to_string(), "Bearer DEV".to_string()),
            ("Music-User-Token".to_string(), "MUT1".to_string()),
        ];
        assert_eq!(
            pick_auth_headers(&h),
            ("Bearer DEV".to_string(), "MUT1".to_string())
        );
        // X-Apple variant + missing pair.
        let h2 = vec![("X-Apple-Music-User-Token".to_string(), "M2".to_string())];
        assert_eq!(pick_auth_headers(&h2), (String::new(), "M2".to_string()));
        let empty: Vec<(String, String)> = Vec::new();
        assert_eq!(pick_auth_headers(&empty), (String::new(), String::new()));
    }

    #[test]
    fn storefront_proxy_path_bypasses_pure_router() {
        // GET /mk-storefront needs async I/O, so serve_auth handles it
        // before auth_route — the pure router must not claim it.
        let page = render_auth_page("DEV123");
        match auth_route("GET", "/mk-storefront", &[], &page) {
            AuthRoute::Respond { status, .. } => assert_eq!(status, "404 Not Found"),
            AuthRoute::Complete { .. } => panic!("proxy path must not complete sign-in"),
        }
    }

    #[test]
    fn routes_serve_page_and_token() {
        let page = render_auth_page("DEV123");
        assert!(page.contains("\"DEV123\""));
        assert!(page.contains("musickit"));
        assert!(!page.contains("__DEV_TOKEN__"));
        match auth_route("GET", "/", &[], &page) {
            AuthRoute::Respond {
                status, payload, ..
            } => {
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

    #[test]
    fn dev_token_path_bypasses_pure_router() {
        // GET /dev-token needs async I/O (live token refresh); the pure
        // router must leave it to serve_auth — and never complete sign-in.
        let page = render_auth_page("DEV123");
        match auth_route("GET", "/dev-token", &[], &page) {
            AuthRoute::Respond { status, .. } => assert_eq!(status, "404 Not Found"),
            AuthRoute::Complete { .. } => panic!("/dev-token must not complete sign-in"),
        }
    }

    #[test]
    fn force_flag_is_only_read_from_the_query() {
        assert!(!force_requested("/dev-token"));
        assert!(!force_requested("/dev-token?x=1"));
        assert!(force_requested("/dev-token?force=1"));
        // Later query params count too.
        assert!(force_requested("/dev-token?attempt=1&force=1"));
        // …but a value other than 1 does not.
        assert!(!force_requested("/dev-token?force=0"));
    }

    /// Drive a raw request against the one-shot server; returns the response text.
    async fn get(port: u16, path: &str) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .expect("connect to sign-in server");
        let req = format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n");
        stream.write_all(req.as_bytes()).await.expect("write");
        let mut buf = Vec::new();
        stream.read_to_end(&mut buf).await.expect("read");
        String::from_utf8_lossy(&buf).into_owned()
    }

    #[tokio::test]
    async fn dev_token_route_serves_a_fresh_token() {
        let refresh: TokenRefresh = std::sync::Arc::new(|_force| {
            Box::pin(async { Ok::<String, String>("FRESH_TOKEN".into()) })
        });
        let (tx, _rx) = oneshot::channel::<String>();
        let (port, handle) = run_signin_server("STALE_TOKEN".into(), refresh, tx)
            .await
            .expect("bind");

        let resp = get(port, "/dev-token").await;
        assert!(resp.starts_with("HTTP/1.1 200 OK"), "{resp}");
        assert!(resp.contains(r#""token":"FRESH_TOKEN""#), "{resp}");
        assert!(resp.contains("no-store"), "{resp}");
        handle.abort();
    }

    #[tokio::test]
    async fn dev_token_route_reports_a_refresh_failure() {
        let refresh: TokenRefresh = std::sync::Arc::new(|_force| {
            Box::pin(async { Err::<String, String>("scrape broke".into()) })
        });
        let (tx, _rx) = oneshot::channel::<String>();
        let (port, handle) = run_signin_server("STALE_TOKEN".into(), refresh, tx)
            .await
            .expect("bind");

        let resp = get(port, "/dev-token").await;
        assert!(resp.starts_with("HTTP/1.1 503"), "{resp}");
        assert!(resp.contains("scrape broke"), "{resp}");
        handle.abort();
    }

    #[tokio::test]
    async fn dev_token_route_forwards_the_force_flag() {
        use std::sync::atomic::{AtomicU8, Ordering};
        // 0 = not called, 1 = plain call, 2 = forced call.
        let calls = std::sync::Arc::new(AtomicU8::new(0));
        let seen = calls.clone();
        let refresh: TokenRefresh = std::sync::Arc::new(move |force| {
            seen.store(if force { 2 } else { 1 }, Ordering::SeqCst);
            Box::pin(async { Ok::<String, String>("FRESH_TOKEN".into()) })
        });
        let (tx, _rx) = oneshot::channel::<String>();
        let (port, handle) = run_signin_server("STALE_TOKEN".into(), refresh, tx)
            .await
            .expect("bind");

        // The page's recovery retry carries force=1: the app must drop its
        // (locally-"fresh") cached token before re-resolving, otherwise a
        // poisoned cache hands the same dead token straight back.
        let resp = get(port, "/dev-token?force=1").await;
        assert!(resp.starts_with("HTTP/1.1 200 OK"), "{resp}");
        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "force=1 must reach refresh"
        );
        handle.abort();
    }
}
