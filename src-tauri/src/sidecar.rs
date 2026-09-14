//! Firefox sidecar for full-track (DRM) playback.
//!
//! Full Apple Music tracks are Widevine-encrypted: only a real browser engine
//! with the CDM can decrypt them. This module runs a localhost HTTP server
//! (tokio only, no extra deps) that:
//!   * serves the minimal audio-only MusicKit page (`ui/player.html`, embedded
//!     at compile time — never `music.apple.com` UI),
//!   * hands the player its developer token via `GET /config`,
//!   * relays [`PlaybackCommand`]s via `GET /cmd` (player polls),
//!   * receives player state via `POST /state`.
//!
//! Rust launches `firefox --profile <dedicated> --new-window` on first play;
//! the Apple approval popup appears there once, then the profile remembers it.

use apple_music_core::playback::PlaybackCommand;
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

const PLAYER_HTML: &str = include_str!("../../ui/player.html");

/// Fixed localhost rendezvous port for the sidecar server. A restart keeps
/// the same port, so an orphaned Firefox (still polling /cmd + POSTing
/// /state) phones home on its own and is readopted instead of stranded.
/// If the port is taken (another instance, stray socket), we fall back to
/// an ephemeral port and behave as before (no recovery, but playback works).
const SIDECAR_FIXED_PORT: u16 = 17877;

/// How recent (ms) the last player contact must be to count as "a player
/// is attached" (state POSTs every 250ms, cmd polls every 500ms).
const PLAYER_PRESENT_MS: u128 = 1500;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PlayerReport {
    #[serde(default)]
    pub playing: bool,
    #[serde(default)]
    pub track_id: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub artist: Option<String>,
    #[serde(default)]
    pub position_ms: u64,
    #[serde(default)]
    pub duration_ms: u64,
    #[serde(default)]
    pub detail: String,
}

struct Inner {
    cmds: Mutex<VecDeque<PlaybackCommand>>,
    report: Mutex<PlayerReport>,
    dev_token: Mutex<Option<String>>,
    mut_token: Mutex<Option<String>>,
    port: Mutex<Option<u16>>,
    child: Mutex<Option<std::process::Child>>,
    /// Process group of the spawned Firefox (it is started as a group
    /// leader), so stop() can SIGKILL the whole forked tree — killing the
    /// direct child alone leaves content processes behind often enough.
    pgid: Mutex<Option<u32>>,
    /// Last contact from the player page (/cmd poll or /state POST).
    /// Lets a restarted app tell "orphan still attached" apart from
    /// "nothing listening" without launching a second Firefox.
    last_seen: Mutex<Option<std::time::Instant>>,
    /// Hide the Firefox window (MOZ_HEADLESS). Default on; toggle in UI.
    /// Note: some builds can't do Widevine headless — toggle off if silent.
    headless: Mutex<bool>,
    /// Allow explicit content. Default on (adult subscriber assumption):
    /// MusicKit otherwise defaults several countries (incl. IT) to restricted
    /// and refuses explicit songs client-side with CONTENT_RESTRICTED.
    /// The license server stays authoritative — a truly restricted account
    /// still fails honestly at key exchange.
    explicit: Mutex<bool>,
}

impl Default for Inner {
    fn default() -> Self {
        Self {
            cmds: Mutex::default(),
            report: Mutex::default(),
            dev_token: Mutex::default(),
            mut_token: Mutex::default(),
            port: Mutex::default(),
            child: Mutex::default(),
            pgid: Mutex::default(),
            last_seen: Mutex::default(),
            headless: Mutex::new(true),
            explicit: Mutex::new(true),
        }
    }
}

impl Inner {
    fn is_headless(&self) -> bool {
        *self.headless.lock().unwrap_or_else(|e| e.into_inner())
    }
}

#[derive(Clone, Default)]
pub struct SidecarManager {
    inner: Arc<Inner>,
}

impl SidecarManager {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn enqueue(&self, cmd: PlaybackCommand) -> Result<(), String> {
        let mut q = self.inner.cmds.lock().map_err(|e| e.to_string())?;
        // Coalesce volume drags: only the latest level matters, otherwise a
        // fast slider floods the 100ms player poll with stale values.
        if matches!(cmd, PlaybackCommand::SetVolume { .. })
            && matches!(q.back(), Some(PlaybackCommand::SetVolume { .. }))
        {
            q.pop_back();
        }
        // A new PlayNow obsoletes queued-but-unplayed queue writes: during
        // a skip burst only the final target must load/play, never
        // intermediates. Transport (Play/Pause/Seek/SetVolume/Next/Previous),
        // explicit Clear, and SetQueue survive — only speculative writes die.
        if matches!(cmd, PlaybackCommand::PlayNow { .. }) {
            q.retain(|c| !matches!(
                c,
                PlaybackCommand::PlayNow { .. }
                    | PlaybackCommand::Append { .. }
                    | PlaybackCommand::PlayNext { .. }
            ));
        }
        q.push_back(cmd);
        Ok(())
    }

    pub fn status(&self) -> Result<PlayerReport, String> {
        Ok(self.inner.report.lock().map_err(|e| e.to_string())?.clone())
    }

    #[allow(dead_code)]
    pub fn is_running(&self) -> bool {
        self.inner.port.lock().map(|g| g.is_some()).unwrap_or(false)
    }

    pub fn set_headless(&self, headless: bool) -> Result<bool, String> {
        *self.inner.headless.lock().map_err(|e| e.to_string())? = headless;
        Ok(headless)
    }

    pub fn is_headless(&self) -> bool {
        self.inner.is_headless()
    }

    pub fn set_explicit(&self, explicit: bool) -> Result<bool, String> {
        *self.inner.explicit.lock().map_err(|e| e.to_string())? = explicit;
        Ok(explicit)
    }

    pub fn is_explicit(&self) -> bool {
        self.inner.explicit.lock().map(|g| *g).unwrap_or(true)
    }

    /// Kill the running sidecar (if any) so the next Play relaunches it,
    /// picking up changed settings (headless flag, prefs, fresh page).
    pub fn relaunch(&self) -> Result<(), String> {
        self.stop()?;
        *self.inner.port.lock().map_err(|e| e.to_string())? = None;
        Ok(())
    }

    fn profile_dir() -> Option<std::path::PathBuf> {
        crate::app_config_dir().map(|d| d.join("firefox-profile"))
    }

    fn legacy_profile_dir() -> Option<std::path::PathBuf> {
        std::env::var("HOME")
            .ok()
            .map(|h| std::path::PathBuf::from(h).join(".config/apple-music-linux/firefox-profile"))
    }

    /// Start server + Firefox if needed. Idempotent. Must be called from
    /// within a Tokio runtime (Tauri async commands qualify).
    /// `mut_token` (when the user already authorized in the app) is handed to
    /// the player via `/config` so no popup is needed.
    /// A live orphan (app restarted under a playing sidecar) is adopted —
    /// no second Firefox — otherwise Firefox is launched as before. This
    /// also heals the "port known but player dead" state (crash) that used
    /// to pile commands onto nobody.
    pub async fn ensure_running(
        &self,
        dev_token: String,
        mut_token: Option<String>,
    ) -> Result<u16, String> {
        // Refresh MUT every call (user may re-authorize); server reads live.
        *self.inner.mut_token.lock().map_err(|e| e.to_string())? = mut_token;
        *self.inner.dev_token.lock().map_err(|e| e.to_string())? = Some(dev_token);
        if let Some(port) = *self.inner.port.lock().map_err(|e| e.to_string())? {
            if Self::player_present(&self.inner) {
                return Ok(port);
            }
        }
        let port = self.bind_and_serve().await?;
        // Orphan already polling the fixed rendezvous port? Adopt it.
        if self.wait_for_player(800).await {
            return Ok(port);
        }
        self.launch_firefox(port)?;
        Ok(port)
    }

    /// Adopt an orphaned player without launching Firefox (app restart
    /// recovery, called at UI boot). Binds the server if needed, then
    /// waits briefly for the orphan to phone home on the fixed port.
    pub async fn reattach(&self) -> bool {
        if self.inner.port.lock().map(|g| g.is_some()).unwrap_or(false) {
            return Self::player_present(&self.inner);
        }
        let Ok(_) = self.bind_and_serve().await else {
            return false;
        };
        self.wait_for_player(900).await
    }

    /// Bind the sidecar server (fixed rendezvous port, ephemeral fallback)
    /// and start accepting, unless already bound. Returns the port.
    async fn bind_and_serve(&self) -> Result<u16, String> {
        if let Some(port) = *self.inner.port.lock().map_err(|e| e.to_string())? {
            return Ok(port);
        }
        let addr = format!("127.0.0.1:{SIDECAR_FIXED_PORT}");
        let listener = match tokio::net::TcpListener::bind(&addr).await {
            Ok(l) => l,
            Err(_) => {
                // Taken (second instance, stray socket) — or lost a bind
                // race with ourselves. Re-check before falling back.
                if let Some(port) = *self.inner.port.lock().map_err(|e| e.to_string())? {
                    return Ok(port);
                }
                tokio::net::TcpListener::bind("127.0.0.1:0")
                    .await
                    .map_err(|e| format!("sidecar bind failed: {e}"))?
            }
        };
        let port = listener.local_addr().map_err(|e| e.to_string())?.port();

        let inner = self.inner.clone();
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    break;
                };
                let inner = inner.clone();
                tokio::spawn(async move { handle(stream, inner).await });
            }
        });

        *self.inner.port.lock().map_err(|e| e.to_string())? = Some(port);
        Ok(port)
    }

    /// True when the player page contacted us recently.
    fn player_present(inner: &Inner) -> bool {
        inner
            .last_seen
            .lock()
            .map(|g| {
                g.map(|at| at.elapsed().as_millis() < PLAYER_PRESENT_MS)
                    .unwrap_or(false)
            })
            .unwrap_or(false)
    }

    fn touch(inner: &Inner) {
        if let Ok(mut g) = inner.last_seen.lock() {
            *g = Some(std::time::Instant::now());
        }
    }

    /// Poll `player_present` until it turns true or the budget (ms) runs out.
    async fn wait_for_player(&self, budget_ms: u64) -> bool {
        let start = std::time::Instant::now();
        let budget = std::time::Duration::from_millis(budget_ms);
        while start.elapsed() < budget {
            if Self::player_present(&self.inner) {
                return true;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        Self::player_present(&self.inner)
    }

    /// `user.js` prefs for the dedicated sidecar profile: the player page is
    /// never clicked, so stock autoplay policy would reject every `play()`
    /// with NotAllowedError. `media.autoplay.default = 0` allows it.
    fn profile_prefs() -> String {
        [
            r#"user_pref("media.autoplay.default", 0);"#,
            r#"user_pref("media.autoplay.block-event.enabled", false);"#,
            r#"user_pref("media.autoplay.enabled.user-gestures-needed", false);"#,
            // Keep EME/Widevine on even if the user disabled it globally.
            r#"user_pref("media.eme.enabled", true);"#,
            "",
        ]
        .join("\n")
    }

    /// Profile path needles identifying OUR sidecar Firefox (current +
    /// legacy config dirs) for pkill/pgrep matching.
    fn profile_needles() -> Vec<String> {
        let mut out = Vec::new();
        if let Some(profile) = Self::profile_dir() {
            out.push(profile.to_string_lossy().into_owned());
        }
        if let Some(legacy) = Self::legacy_profile_dir() {
            let s = legacy.to_string_lossy().into_owned();
            if !out.contains(&s) {
                out.push(s);
            }
        }
        out.into_iter().filter(|s| !s.is_empty()).collect()
    }

    fn needle_running(needle: &str) -> bool {
        std::process::Command::new("pgrep")
            .args(["-f", needle])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    fn any_profile_firefox_running() -> bool {
        Self::profile_needles()
            .iter()
            .any(|n| Self::needle_running(n))
    }

    /// pgids of live processes whose full cmdline contains `needle`
    /// (our sidecar trees, including adopted orphans whose stored pgid is
    /// long gone). Content procs never carry the profile path themselves,
    /// but they share the main's group — which is what we actually kill.
    /// Our own helper processes can't match: their argv holds a pid (ps)
    /// or a pgid (pkill -g), never the needle; pgrep/pkill also never
    /// match themselves.
    fn pgids_for_needle(needle: &str) -> Vec<u32> {
        let mut out = Vec::new();
        let pids = std::process::Command::new("pgrep")
            .args(["-f", needle])
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
            .unwrap_or_default();
        for pid in pids.split_whitespace() {
            let Ok(pid) = pid.parse::<u32>() else {
                continue;
            };
            if pid == std::process::id() {
                continue;
            }
            let pgid = std::process::Command::new("ps")
                .args(["-o", "pgid=", "-p", &pid.to_string()])
                .output()
                .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
                .unwrap_or_default();
            if let Ok(pgid) = pgid.parse::<u32>() {
                if pgid != 0 && !out.contains(&pgid) {
                    out.push(pgid);
                }
            }
        }
        out
    }

    /// Kill every sidecar tree matching our profile needles: whole process
    /// groups first (gets adopted orphans' content procs too), then plain
    /// profile pkill for mains that slipped through.
    fn kill_profile_trees() {
        for needle in Self::profile_needles() {
            for pgid in Self::pgids_for_needle(&needle) {
                let _ = std::process::Command::new("pkill")
                    .args(["-9", "-g", pgid.to_string().as_str()])
                    .status();
            }
            let _ = std::process::Command::new("pkill")
                .args(["-9", "-f", needle.as_str()])
                .status();
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }

    /// Kill Firefox still holding our sidecar profile. A previous app process
    /// (failed rebuild, crash) can leave one running; `--no-remote --profile`
    /// then fails to open the new player page and Play appears to do nothing.
    fn kill_stale_profile_firefox(_profile: &std::path::Path) {
        Self::kill_profile_trees();
    }

    fn launch_firefox(&self, port: u16) -> Result<(), String> {
        let profile = Self::profile_dir().ok_or("no HOME for firefox profile")?;
        std::fs::create_dir_all(&profile).map_err(|e| format!("profile dir: {e}"))?;
        // user.js is read at every Firefox startup; harmless to rewrite.
        std::fs::write(profile.join("user.js"), Self::profile_prefs())
            .map_err(|e| format!("profile prefs: {e}"))?;
        let url = format!("http://127.0.0.1:{port}/");
        {
            let mut slot = self.inner.child.lock().map_err(|e| e.to_string())?;
            if let Some(child) = slot.as_mut() {
                if child.try_wait().map_err(|e| e.to_string())?.is_none() {
                    return Ok(());
                }
            }
        }
        Self::kill_stale_profile_firefox(&profile);
        let headless = self.inner.is_headless();
        let mut cmd = std::process::Command::new("firefox");
        let profile_s = profile.to_string_lossy().into_owned();
        cmd.args(["--no-remote", "--profile", &profile_s, "--new-window", &url]);
        #[cfg(unix)]
        {
            // Own process group: the whole forked tree can be signalled at
            // once on stop (Firefox does not setsid itself).
            use std::os::unix::process::CommandExt;
            cmd.process_group(0);
            // Kernel-guaranteed cleanup: if WE die for any reason — missed
            // close event, SIGKILL, crash — the sidecar dies with us. No
            // exit hook runs on SIGKILL, so this (not stop()) is what makes
            // "close window, no firefox left" hold for every close method.
            // Async-signal-safe by construction (a single prctl).
            unsafe {
                cmd.pre_exec(|| {
                    libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL as libc::c_ulong);
                    Ok(())
                });
            }
        }
        if headless {
            // No window at all. If audio stays silent on your build, toggle
            // headless off in the UI — some builds need a real window for CDM.
            cmd.env("MOZ_HEADLESS", "1");
        }
        let child = cmd
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .map_err(|e| {
                format!("could not launch firefox ({e}); install it: sudo pacman -S firefox, then enable DRM content in its settings")
            })?;
        // Group leader == direct child pid (process_group(0) at spawn).
        let pgid = child.id();
        *self.inner.child.lock().map_err(|e| e.to_string())? = Some(child);
        *self.inner.pgid.lock().map_err(|e| e.to_string())? = Some(pgid);
        Ok(())
    }

    /// Stop the sidecar Firefox so no orphan keeps playing after exit.
    /// Order: known group (spawned this session), direct child handle,
    /// then full tree sweep (adopted orphans included). Locks use
    /// into_inner: a poisoned mutex must never silently skip the kill.
    /// A pgrep verify loop (up to ~2s) re-kills stragglers; survivors are
    /// reported on stderr instead of assumed dead.
    pub fn stop(&self) -> Result<(), String> {
        eprintln!("sonora: stopping sidecar");
        // Whole process group first (pkill -g): the forked tree dies
        // together — killing the direct child alone strands content
        // processes. ESRCH-style misses are fine; fallbacks below cover
        // whatever is left.
        #[cfg(unix)]
        if let Some(pgid) = self
            .inner
            .pgid
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
        {
            let _ = std::process::Command::new("pkill")
                .args(["-9", "-g", pgid.to_string().as_str()])
                .status();
        }
        if let Some(mut child) = self
            .inner
            .child
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
        {
            let _ = child.kill();
            let _ = child.wait();
        }
        for _ in 0..4 {
            Self::kill_profile_trees();
            if !Self::any_profile_firefox_running() {
                return Ok(());
            }
            std::thread::sleep(std::time::Duration::from_millis(250));
        }
        eprintln!("sonora: sidecar processes survived stop; kill them manually, e.g. pkill -9 -f firefox-profile");
        Ok(())
    }
}

async fn handle(mut stream: tokio::net::TcpStream, inner: Arc<Inner>) {
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt};
    let mut reader = tokio::io::BufReader::new(&mut stream);
    let mut head = Vec::new();
    loop {
        let mut line = String::new();
        match reader.read_line(&mut line).await {
            Ok(0) => return,
            Ok(_) => {}
            Err(_) => return,
        }
        head.extend_from_slice(line.as_bytes());
        if head.ends_with(b"\r\n\r\n") || head.len() > 65536 {
            break;
        }
    }
    let (method, path, headers, content_len) = parse_head(&String::from_utf8_lossy(&head));
    let mut body = vec![0u8; content_len.min(1 << 20)];
    if content_len > 0 {
        let _ = reader.read_exact(&mut body).await;
    }
    let (status, ctype, payload) = route(&inner, &method, &path, &headers, &body).await;
    let resp = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        payload.len()
    );
    let w = &mut stream;
    let _ = w.write_all(resp.as_bytes()).await;
    let _ = w.write_all(&payload).await;
}

/// Rewrite `https://api.music.apple.com/...` to the amp host that accepts
/// shared web-player tokens. Pure mapping, no network (unit-tested).
pub fn proxy_target(path_and_query: &str) -> String {
    format!("https://amp-api.music.apple.com{path_and_query}")
}

fn status_phrase(code: u16) -> String {
    let phrase = match code {
        200 => "OK",
        201 => "Created",
        204 => "No Content",
        206 => "Partial Content",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        _ => "Unknown",
    };
    format!("{code} {phrase}")
}

static PROXY_HTTP: std::sync::LazyLock<reqwest::Client> = std::sync::LazyLock::new(|| {
    reqwest::Client::builder()
        .user_agent("Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0 Safari/537.36")
        .build()
        .expect("proxy http client")
});

/// Forward a page request to Apple with the headers Apple demands
/// (`Origin: https://music.apple.com`), which a localhost page can't send.
async fn proxy_apple(
    method: &str,
    path_and_query: &str,
    headers: &[(String, String)],
    body: &[u8],
) -> (String, String, Vec<u8>) {
    const FORWARD: &[&str] = &[
        "authorization",
        "music-user-token",
        "media-user-token",
        "content-type",
        "accept",
        "accept-language",
        "range",
        "x-apple-music-user-token",
        "x-apple-renewal",
    ];
    let url = proxy_target(path_and_query);
    let mut req = match method {
        "POST" => PROXY_HTTP.post(&url),
        "PUT" => PROXY_HTTP.put(&url),
        "DELETE" => PROXY_HTTP.delete(&url),
        _ => PROXY_HTTP.get(&url),
    };
    for (k, v) in headers {
        if FORWARD.contains(&k.to_lowercase().as_str()) {
            req = req.header(k, v);
        }
    }
    req = req
        .header("Origin", "https://music.apple.com")
        .header("Referer", "https://music.apple.com/");
    if !body.is_empty() {
        req = req.body(body.to_vec());
    }
    let resp = match req.send().await {
        Ok(r) => r,
        Err(e) => {
            let msg = format!(r#"{{"proxyError":{e:?}}}"#);
            return (
                "502 Bad Gateway".into(),
                "application/json".into(),
                msg.into_bytes(),
            );
        }
    };
    let code = resp.status().as_u16();
    let ctype = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/json")
        .to_string();
    let bytes = resp.bytes().await.map(|b| b.to_vec()).unwrap_or_default();
    (status_phrase(code), ctype, bytes)
}

async fn route(
    inner: &Inner,
    method: &str,
    path: &str,
    headers: &[(String, String)],
    body: &[u8],
) -> (String, &'static str, Vec<u8>) {
    // Same-origin API proxy: the page's fetch wrapper rewrites
    // https://api.music.apple.com/... -> /apiproxy/...
    if let Some(rest) = path.strip_prefix("/apiproxy/") {
        let (status, ctype, payload) =
            proxy_apple(method, &format!("/{rest}"), headers, body).await;
        // Leak the owned content-type into a static: only two variants occur.
        let ctype_static: &'static str = if ctype.starts_with("application/json") {
            "application/json"
        } else if ctype.starts_with("text/") {
            "text/plain"
        } else {
            "application/octet-stream"
        };
        return (status, ctype_static, payload);
    }
    let ok = |ctype: &'static str, payload: Vec<u8>| ("200 OK".to_string(), ctype, payload);
    match (method, path) {
        ("GET", "/") | ("GET", "/index.html") => {
            ok("text/html; charset=utf-8", PLAYER_HTML.as_bytes().to_vec())
        }
        ("GET", "/config") => {
            let token = inner
                .dev_token
                .lock()
                .map(|g| g.clone().unwrap_or_default())
                .unwrap_or_default();
            let mut_ = inner
                .mut_token
                .lock()
                .map(|g| g.clone().unwrap_or_default())
                .unwrap_or_default();
            let explicit = inner.explicit.lock().map(|g| *g).unwrap_or(true);
            let v = serde_json::json!({ "devToken": token, "mut": mut_, "explicit": explicit });
            ok("application/json", v.to_string().into_bytes())
        }
        ("GET", "/cmd") => {
            // Player heartbeat: an orphan from before an app restart shows
            // up here first, before its next /state POST.
            SidecarManager::touch(inner);
            // PlayNow jumps the queue: a skip issued mid-append (up to
            // MIRROR_AHEAD=25 serial appends) would otherwise wait a full
            // append cycle plus a poll tick. Only ordering changes here;
            // nothing is dropped (drops belong to skip coalescing).
            let next = inner.cmds.lock().map(|mut g| {
                g.iter()
                    .position(|c| matches!(c, PlaybackCommand::PlayNow { .. }))
                    .and_then(|i| g.remove(i))
                    .or_else(|| g.pop_front())
            }).unwrap_or(None);
            let v = match next {
                Some(cmd) => serde_json::to_value(&cmd).unwrap_or(serde_json::Value::Null),
                None => serde_json::Value::Null,
            };
            let payload = serde_json::json!({ "cmd": v }).to_string().into_bytes();
            ok("application/json", payload)
        }
        ("POST", "/state") => {
            SidecarManager::touch(inner);
            if let Ok(rep) = serde_json::from_slice::<PlayerReport>(body) {
                if let Ok(mut g) = inner.report.lock() {
                    *g = rep;
                }
            }
            ok("application/json", b"{}".to_vec())
        }
        _ => ("404 Not Found".to_string(), "text/plain", b"nope".to_vec()),
    }
}

/// Parse an HTTP request head: returns (method, path, headers, content_len).
pub fn parse_head(head: &str) -> (String, String, Vec<(String, String)>, usize) {
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
        let (m, p, _, l) = parse_head("POST /state HTTP/1.1\r\nContent-Length: 42\r\n\r\n");
        assert_eq!((m.as_str(), p.as_str(), l), ("POST", "/state", 42));
    }

    #[test]
    fn explicit_defaults_on_and_served() {
        let m = SidecarManager::new();
        assert!(m.is_explicit());
        m.set_explicit(false).unwrap();
        assert!(!m.is_explicit());
    }

    #[tokio::test]
    async fn config_serves_explicit_flag() {
        let inner = Inner::default();
        let (_, _, cfg) = route(&inner, "GET", "/config", &[], &[]).await;
        let v: serde_json::Value = serde_json::from_slice(&cfg).unwrap();
        assert_eq!(v.get("explicit").and_then(|b| b.as_bool()), Some(true));
    }

    #[test]
    fn profile_prefs_allow_autoplay() {
        let prefs = SidecarManager::profile_prefs();
        assert!(prefs.contains(r#"user_pref("media.autoplay.default", 0);"#));
        assert!(prefs.contains(r#"user_pref("media.eme.enabled", true);"#));
    }

    #[test]
    fn proxy_target_rewrites_host() {
        assert_eq!(
            proxy_target("/v1/catalog/us/songs?ids=1"),
            "https://amp-api.music.apple.com/v1/catalog/us/songs?ids=1"
        );
    }

    #[tokio::test]
    async fn routes_serve_config_and_cmd() {
        let inner = Inner::default();
        *inner.dev_token.lock().unwrap() = Some("DEV".into());
        *inner.mut_token.lock().unwrap() = Some("MUT".into());
        let (_, _, cfg) = route(&inner, "GET", "/config", &[], &[]).await;
        let body = String::from_utf8(cfg).unwrap();
        assert!(body.contains("DEV"));
        assert!(body.contains("MUT"));
        inner.cmds.lock().unwrap().push_back(PlaybackCommand::Play);
        let (_, _, cmd) = route(&inner, "GET", "/cmd", &[], &[]).await;
        assert!(String::from_utf8(cmd).unwrap().contains("play"));
        let (_, _, empty) = route(&inner, "GET", "/cmd", &[], &[]).await;
        assert!(String::from_utf8(empty).unwrap().contains("null"));
    }

    #[tokio::test]
    async fn state_post_stored() {
        let inner = Inner::default();
        let body = br#"{"playing":true,"title":"T"}"#;
        route(&inner, "POST", "/state", &[], body).await;
        let rep = inner.report.lock().unwrap();
        assert!(rep.playing);
        assert_eq!(rep.title.as_deref(), Some("T"));
    }

    #[test]
    fn volume_commands_coalesce() {
        let m = SidecarManager::new();
        m.enqueue(PlaybackCommand::SetVolume { level: 0.1 })
            .unwrap();
        m.enqueue(PlaybackCommand::SetVolume { level: 0.2 })
            .unwrap();
        m.enqueue(PlaybackCommand::Play).unwrap();
        let q = m.inner.cmds.lock().unwrap();
        assert_eq!(q.len(), 2);
        assert!(
            matches!(q[0], PlaybackCommand::SetVolume { level } if (level - 0.2f32).abs() < f32::EPSILON)
        );
    }
    #[test]
    fn play_now_obsoletes_pending_queue_writes() {
        use apple_music_core::playback::QueueItem;
        let qi = |id: &str| QueueItem { id: id.into(), kind: "song".into() };
        let m = SidecarManager::new();
        m.enqueue(PlaybackCommand::PlayNow {
            items: vec![qi("old")],
            start_index: 0,
        })
        .unwrap();
        m.enqueue(PlaybackCommand::Append {
            items: vec![qi("mirror")],
        })
        .unwrap();
        m.enqueue(PlaybackCommand::Pause).unwrap();
        m.enqueue(PlaybackCommand::PlayNow {
            items: vec![qi("new")],
            start_index: 0,
        })
        .unwrap();
        let q = m.inner.cmds.lock().unwrap();
        // Only the final PlayNow + surviving transport remain; stale
        // PlayNow/Append died. Pause (transport) survives.
        assert_eq!(q.len(), 2);
        assert!(matches!(q[0], PlaybackCommand::Pause));
        assert!(matches!(&q[1], PlaybackCommand::PlayNow { items, .. } if items.len() == 1 && items[0].id == "new"));
    }

    #[tokio::test]
    async fn cmd_serves_play_now_ahead_of_append() {
        use apple_music_core::playback::QueueItem;
        let qi = |id: &str| QueueItem { id: id.into(), kind: "song".into() };
        let inner = Inner::default();
        inner.cmds.lock().unwrap().push_back(PlaybackCommand::Append {
            items: vec![qi("mirror")],
        });
        inner.cmds.lock().unwrap().push_back(PlaybackCommand::PlayNow {
            items: vec![qi("target")],
            start_index: 0,
        });
        let (_, _, first) = route(&inner, "GET", "/cmd", &[], &[]).await;
        let v: serde_json::Value = serde_json::from_slice(&first).unwrap();
        assert_eq!(v.get("cmd").and_then(|c| c.get("cmd")).and_then(|c| c.as_str()), Some("play-now"));
    }

    #[test]
    fn stop_without_sidecar_is_ok() {
        let m = SidecarManager::new();
        assert!(m.stop().is_ok());
    }

    /// Own-group tree with a needle only on the leader's cmdline (mirrors
    /// Firefox: content procs never carry the profile path). Discovery must
    /// find the group via the leader, and a group kill must reap the
    /// needle-less sleep child too.
    #[test]
    fn group_kill_reaps_needle_less_children() {
        let needle = format!("sonora-pkill-probe-{}", std::process::id());
        let mut cmd = std::process::Command::new("bash");
        cmd.args(["-c", &format!("sleep 120 & wait # {needle}")]);
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            cmd.process_group(0);
        }
        let mut leader = cmd.spawn().expect("spawn probe tree");
        let pgid = leader.id();
        std::thread::sleep(std::time::Duration::from_millis(400));
        assert!(
            SidecarManager::pgids_for_needle(&needle).contains(&pgid),
            "discovery finds the probe group"
        );
        let _ = std::process::Command::new("pkill")
            .args(["-9", "-g", pgid.to_string().as_str()])
            .status();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            match leader.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) if std::time::Instant::now() < deadline => {
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }
                _ => break,
            }
        }
        assert!(leader.try_wait().ok().flatten().is_some(), "leader reaped");
        assert!(
            !SidecarManager::pgids_for_needle(&needle).contains(&pgid),
            "group gone after group kill"
        );
    }

    #[test]
    fn player_presence_tracks_recency() {
        let inner = Inner::default();
        assert!(!SidecarManager::player_present(&inner));
        SidecarManager::touch(&inner);
        assert!(SidecarManager::player_present(&inner));
        *inner.last_seen.lock().unwrap() =
            Some(std::time::Instant::now() - std::time::Duration::from_secs(60));
        assert!(!SidecarManager::player_present(&inner));
    }

    #[tokio::test]
    async fn cmd_and_state_touch_presence() {
        let inner = Inner::default();
        assert!(!SidecarManager::player_present(&inner));
        route(&inner, "GET", "/cmd", &[], &[]).await;
        assert!(SidecarManager::player_present(&inner));
        *inner.last_seen.lock().unwrap() =
            Some(std::time::Instant::now() - std::time::Duration::from_secs(60));
        route(&inner, "POST", "/state", &[], br#"{"playing":false}"#).await;
        assert!(SidecarManager::player_present(&inner));
    }

    #[tokio::test]
    async fn reattach_without_player_is_false_and_bound() {
        // No player phoning home: returns false after the ~900ms budget,
        // but the server IS bound afterwards so a later play needs no rebind.
        // Hold the fixed rendezvous port so the manager falls back to an
        // ephemeral one: no ambient orphan on this machine can then phone
        // home to the test server and flip the result.
        let _guard =
            match tokio::net::TcpListener::bind(format!("127.0.0.1:{SIDECAR_FIXED_PORT}")).await {
                Ok(g) => g,
                Err(_) => {
                    eprintln!("SKIP: fixed sidecar port busy (another instance?)");
                    return;
                }
            };
        let m = SidecarManager::new();
        assert!(!m.reattach().await);
        assert!(m.is_running());
    }

    #[tokio::test]
    async fn fixed_port_falls_back_when_taken() {
        // Hold the fixed rendezvous port: bind_and_serve must fall back to
        // an ephemeral port instead of failing.
        let guard = tokio::net::TcpListener::bind(format!("127.0.0.1:{SIDECAR_FIXED_PORT}")).await;
        let m = SidecarManager::new();
        let port = m.bind_and_serve().await.expect("fallback bind");
        if guard.is_ok() {
            assert_ne!(port, SIDECAR_FIXED_PORT);
        } else {
            // Fixed port was already taken on this machine (another
            // instance): any successful bind proves the fallback path.
            assert!(port != 0);
        }
    }
}
