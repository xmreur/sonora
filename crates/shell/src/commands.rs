//! Command layer: every backend operation the UI can invoke, as plain
//! functions over [`AppState`](crate::AppState) — no UI types anywhere.
//!
//! The egui UI calls these directly. Shared state helpers (dead ids,
//! MUT, storefront, dev token) and the play chain (resolve → metadata
//! retry → engine handoff) live alongside the commands they serve.
use crate::{
    app_state::AppState, discord::PresencePayload, native_player, native_player::PlayerReport,
    paths,
};
use apple_music_core::{api::ApiClient, auth, playback::QueueItem, token::TokenProvider};

/// Credential file names inside the config dir (shared by the accessors
/// and the logout wipe so they can never drift apart).
const MUT_FILE: &str = "music_user_token";
const WEB_TOKEN_FILE: &str = "web_token_cache";

/// Minimum remaining lifetime before a cached developer token is reused.
/// A token that dies mid-request (or mid sign-in) costs more than the
/// one scrape it saves, so anything closer than this gets re-fetched.
pub const DEV_TOKEN_MIN_TTL_SECS: i64 = 300;

pub fn web_token_cache_path() -> Option<std::path::PathBuf> {
    paths::app_config_dir().map(|d| d.join(WEB_TOKEN_FILE))
}

pub fn mut_cache_path() -> Option<std::path::PathBuf> {
    paths::app_config_dir().map(|d| d.join(MUT_FILE))
}

/// Remove every on-disk credential in `dir` (MUT + cached dev token).
/// Separate from [`logout`] so it runs against a temp dir in tests.
pub fn wipe_token_files(dir: &std::path::Path) {
    let _ = std::fs::remove_file(dir.join(MUT_FILE));
    let _ = std::fs::remove_file(dir.join(WEB_TOKEN_FILE));
}

/// Persistent store-dead id set (`dead_ids.json` in the config dir).
pub fn dead_ids_path() -> Option<std::path::PathBuf> {
    paths::app_config_dir().map(|d| d.join("dead_ids.json"))
}

pub fn load_dead_ids() -> std::collections::HashSet<String> {
    dead_ids_path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str::<Vec<String>>(&s).ok())
        .map(|v| v.into_iter().take(2000).collect())
        .unwrap_or_default()
}

pub fn save_dead_ids(set: &std::collections::HashSet<String>) {
    let Some(path) = dead_ids_path() else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let v: Vec<&String> = set.iter().take(2000).collect();
    let _ = std::fs::write(path, serde_json::to_string(&v).unwrap_or_default());
}

/// Remember store-dead ids so listings gray them without replaying the
/// failure. Only call with id-problem signatures (never network/auth —
/// an outage must not gray the world).
pub fn note_dead_ids(state: &AppState, ids: &[&str]) {
    let Ok(mut set) = state.dead_ids.lock() else {
        return;
    };
    let mut changed = false;
    for id in ids {
        if !id.trim().is_empty() && set.insert((*id).to_string()) {
            changed = true;
        }
    }
    while set.len() > 2000 {
        if let Some(k) = set.iter().next().cloned() {
            set.remove(&k);
        } else {
            break;
        }
    }
    if changed {
        save_dead_ids(&set);
    }
}

/// A later successful play clears the id again (transient 307x flakiness).
pub fn clear_dead_ids(state: &AppState, ids: &[&str]) {
    let Ok(mut set) = state.dead_ids.lock() else {
        return;
    };
    let mut changed = false;
    for id in ids {
        if set.remove(*id) {
            changed = true;
        }
    }
    if changed {
        save_dead_ids(&set);
    }
}

/// Stamp known-dead ids onto a listing response so the UI grays them
/// without attempting playback.
pub fn apply_dead_ids(state: &AppState, tracks: &mut [apple_music_core::models::Track]) {
    if tracks.is_empty() {
        return;
    }
    let Ok(set) = state.dead_ids.lock() else {
        return;
    };
    if set.is_empty() {
        return;
    }
    for t in tracks {
        if !t.unavailable && set.contains(&t.id) {
            t.unavailable = true;
        }
    }
}

/// Shape-check a token before it is persisted or loaded from disk. A
/// cached token that is really a JSON/HTML error body (or leftover test
/// garbage) wedges the app: nothing overwrites it because it is "set",
/// and every MusicKit/catalog call rejects it until the file is deleted
/// by hand. Real MUTs are opaque high-entropy strings, so reject
/// anything too short or containing whitespace / markup / quoting
/// characters.
pub fn valid_mut(token: &str) -> bool {
    let t = token.trim();
    t.len() >= 16
        && !t
            .chars()
            .any(|c| c.is_whitespace() || matches!(c, '<' | '{' | '"'))
}

/// Write a credential file atomically (temp file + rename) so a crash
/// mid-write can never leave a truncated token behind — a half-written
/// file is exactly the poisoned state that used to block sign-in until
/// it was deleted by hand. 0600 on Unix.
fn write_token_file(path: &std::path::Path, contents: &str) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, contents).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        e.to_string()
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

/// Write the MUT to disk (0600) so it survives restarts. Garbage is
/// refused up front — see [`valid_mut`].
pub fn persist_mut(token: &str) -> Result<(), String> {
    if !valid_mut(token) {
        return Err("refusing to save an invalid music user token".into());
    }
    let path = mut_cache_path().ok_or("no config dir for MUT cache")?;
    write_token_file(&path, token.trim())
}

/// MUT from memory, falling back to the disk cache (memory is empty after restart).
pub fn current_mut(state: &AppState) -> Option<String> {
    if let Some(t) = state.tokens.music_user_token() {
        if valid_mut(&t) {
            return Some(t);
        }
    }
    // A poisoned cache file (JSON/HTML body, test garbage) must read as
    // "not signed in" — never be handed to Apple as a token.
    mut_cache_path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| valid_mut(s))
}

/// Catalog storefront for reads: the account's own storefront when a MUT is
/// saved (regional catalogs differ — a hardcoded "us" hides e.g. Italian rap
/// from an Italian account), else the device locale, else "us".
/// The per-play `/v1/me/storefront` probe is cached keyed by MUT: warm plays
/// skip one network round-trip. Overwritten whenever the MUT differs; no TTL.
pub async fn resolve_storefront(state: &AppState, provider: &ResolvedProvider) -> String {
    if let Some(m) = provider.music_user_token() {
        if let Ok(cached) = state.storefront_cache.lock() {
            if let Some((fp, sf)) = cached.as_ref() {
                if *fp == m {
                    return sf.clone();
                }
            }
        }
        if let Ok(probe) = ApiClient::new(provider, "us") {
            if let Ok(sf) = probe.user_storefront().await {
                if let Ok(mut cached) = state.storefront_cache.lock() {
                    *cached = Some((m, sf.clone()));
                }
                return sf;
            }
        }
    }
    apple_music_core::api::system_locale_storefront().unwrap_or_else(|| "us".to_string())
}

/// Resolve developer token without requiring a paid account:
/// 1. `APPLE_MUSIC_DEVELOPER_TOKEN` env (official key, preferred)
/// 2. In-memory scraped cache → file cache → live scrape of beta.music.apple.com
///
/// Every source is checked for remaining lifetime ([`DEV_TOKEN_MIN_TTL_SECS`]):
/// an expired JWT here is what blocks re-login on the localhost sign-in page
/// (`MusicKit setup failed: Initialized with an expired token.`) and turns
/// catalog calls into 401s, so stale copies are dropped and re-fetched.
pub async fn resolve_developer_token(state: &AppState) -> Result<String, String> {
    if let Ok(t) = state.tokens.developer_token() {
        if !t.trim().is_empty() {
            if apple_music_core::token::token_fresh_for(&t, DEV_TOKEN_MIN_TTL_SECS) {
                return Ok(t);
            }
            // The env key is an explicit override: never silently swap it
            // for the scraped token — say what is wrong instead.
            return Err(
                "APPLE_MUSIC_DEVELOPER_TOKEN has expired — renew it (or unset it to use \
                 the automatic shared web-player token)."
                    .into(),
            );
        }
    }
    let memory_cached = state
        .web_token_cache
        .lock()
        .map_err(|e| e.to_string())?
        .clone();
    if let Some(cached) = memory_cached {
        if apple_music_core::token::token_fresh_for(&cached, DEV_TOKEN_MIN_TTL_SECS) {
            return Ok(cached);
        }
        // Stale in-memory copy: drop it so the scrape below replaces it.
        *state.web_token_cache.lock().map_err(|e| e.to_string())? = None;
    }
    if let Some(path) = web_token_cache_path() {
        if let Ok(raw) = std::fs::read_to_string(&path) {
            let t = raw.trim().to_string();
            if apple_music_core::token::token_fresh_for(&t, DEV_TOKEN_MIN_TTL_SECS) {
                *state.web_token_cache.lock().map_err(|e| e.to_string())? = Some(t.clone());
                return Ok(t);
            }
        }
    }
    let http = reqwest::Client::builder()
        .user_agent("Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0 Safari/537.36")
        .build()
        .map_err(|e| format!("http client: {e}"))?;
    let token = apple_music_core::token::fetch_web_player_token(&http)
        .await
        .map_err(|e| {
            format!(
                "couldn't fetch Apple's developer token ({e}) — check your connection, or \
                 set APPLE_MUSIC_DEVELOPER_TOKEN"
            )
        })?;
    *state.web_token_cache.lock().map_err(|e| e.to_string())? = Some(token.clone());
    if let Some(path) = web_token_cache_path() {
        // Best-effort: a cache that cannot be written just gets re-fetched
        // next run — the in-memory copy already serves this session.
        let _ = write_token_file(&path, &token);
    }
    Ok(token)
}

/// Human-readable developer-token source for the account card. Cached
/// copies that are expired are reported as missing — they will be
/// re-fetched, not used.
pub async fn token_status(state: &AppState) -> Result<String, String> {
    if let Ok(t) = state.tokens.developer_token() {
        if !t.trim().is_empty() {
            if apple_music_core::token::token_fresh_for(&t, DEV_TOKEN_MIN_TTL_SECS) {
                return Ok("official (env)".into());
            }
            return Ok("official (env) — expired, renew the env token".into());
        }
    }
    let memory_cached = state
        .web_token_cache
        .lock()
        .map_err(|e| e.to_string())?
        .clone();
    let file_cached = web_token_cache_path().and_then(|p| std::fs::read_to_string(p).ok());
    for t in memory_cached.into_iter().chain(file_cached) {
        let t = t.trim().to_string();
        if apple_music_core::token::token_fresh_for(&t, DEV_TOKEN_MIN_TTL_SECS) {
            let left = apple_music_core::token::decode_jwt_expiry(&t)
                .ok()
                .map(|exp| exp - now_secs());
            return Ok(match left {
                Some(s) if s >= 86_400 => {
                    format!("shared web-player (cached, ~{}d left)", s / 86_400)
                }
                Some(s) if s > 0 => format!("shared web-player (cached, ~{}h left)", s / 3600),
                _ => "shared web-player (cached)".into(),
            });
        }
    }
    Ok("shared web-player (will auto-fetch on authorize)".into())
}

/// Wall-clock seconds since the Unix epoch (mirrors core's clock for
/// the status/freshness reads below).
fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// One-shot provider holding the resolved dev token + current MUT.
pub struct ResolvedProvider {
    dev: String,
    mut_token: Option<String>,
}

impl TokenProvider for ResolvedProvider {
    fn developer_token(&self) -> Result<String, apple_music_core::CoreError> {
        Ok(self.dev.clone())
    }
    fn music_user_token(&self) -> Option<String> {
        self.mut_token.clone()
    }
    fn set_music_user_token(&self, _token: String) -> Result<(), apple_music_core::CoreError> {
        Err(apple_music_core::CoreError::Unsupported("one-shot".into()))
    }
}

pub async fn search_catalog(
    state: &AppState,
    term: String,
) -> Result<apple_music_core::models::SearchResults, String> {
    use apple_music_core::models::{
        merge_search_results, normalize_search_term, rank_search_results, should_rank_results,
    };
    let dev = resolve_developer_token(state).await?;
    let provider = ResolvedProvider {
        dev,
        mut_token: current_mut(state),
    };
    let storefront = resolve_storefront(state, &provider).await;
    let client = ApiClient::new(&provider, &storefront).map_err(|e| e.to_string())?;
    let mut out = client.search(&term, 25).await.map_err(|e| e.to_string())?;
    // Punctuation-cleaned query can only add hits (merged, deduped).
    let normalized = normalize_search_term(&term);
    if !normalized.is_empty() && normalized != term.trim() {
        if let Ok(extra) = client.search(&normalized, 25).await {
            out = merge_search_results(out, extra);
        }
    }
    // Short queries get relevance-ranked; lyric-like phrases keep Apple's
    // blended title+lyric order (ranking those by title would bury hits
    // that only match by lyrics).
    if should_rank_results(&term) {
        rank_search_results(&mut out, &term);
    }
    apply_dead_ids(state, &mut out.tracks);
    Ok(out)
}

pub async fn browse_charts(
    state: &AppState,
) -> Result<apple_music_core::models::SearchResults, String> {
    let dev = resolve_developer_token(state).await?;
    let provider = ResolvedProvider {
        dev,
        mut_token: current_mut(state),
    };
    let storefront = resolve_storefront(state, &provider).await;
    let client = ApiClient::new(&provider, &storefront).map_err(|e| e.to_string())?;
    client.charts(12).await.map_err(|e| e.to_string())
}

pub async fn get_artist(
    state: &AppState,
    id: String,
) -> Result<apple_music_core::models::ArtistDetail, String> {
    let dev = resolve_developer_token(state).await?;
    let provider = ResolvedProvider {
        dev,
        mut_token: current_mut(state),
    };
    let storefront = resolve_storefront(state, &provider).await;
    let client = ApiClient::new(&provider, &storefront).map_err(|e| e.to_string())?;
    client.get_artist(&id).await.map_err(|e| e.to_string())
}

pub async fn add_to_playlist(
    state: &AppState,
    playlist_id: String,
    song_ids: Vec<String>,
) -> Result<String, String> {
    let dev = resolve_developer_token(state).await?;
    let provider = ResolvedProvider {
        dev,
        mut_token: current_mut(state),
    };
    let storefront = resolve_storefront(state, &provider).await;
    let client = ApiClient::new(&provider, &storefront).map_err(|e| e.to_string())?;
    let n = client
        .add_to_playlist(&playlist_id, &song_ids)
        .await
        .map_err(|e| e.to_string())?;
    Ok(format!("added {n} track(s)"))
}

/// Map a library-song id (`i.…`) to its catalog id. Catalog ids pass
/// through untouched (no login needed); unmapped ids pass through as-is
/// so callers degrade to today's behavior instead of failing.
pub async fn resolve_track_id(state: &AppState, track_id: String) -> Result<String, String> {
    if !ApiClient::is_library_song_id(&track_id) {
        return Ok(track_id);
    }
    let dev = resolve_developer_token(state).await?;
    let provider = ResolvedProvider {
        dev,
        mut_token: current_mut(state),
    };
    let storefront = resolve_storefront(state, &provider).await;
    let client = ApiClient::new(&provider, &storefront).map_err(|e| e.to_string())?;
    Ok(client
        .catalog_id_for_library_song(&track_id)
        .await
        .unwrap_or(track_id))
}

pub async fn remove_from_playlist(
    state: &AppState,
    playlist_id: String,
    song_ids: Vec<String>,
) -> Result<String, String> {
    let dev = resolve_developer_token(state).await?;
    let provider = ResolvedProvider {
        dev,
        mut_token: current_mut(state),
    };
    // Removal needs a valid login — fail fast with a re-auth hint instead
    // of a bare 401 from deep inside the multi-attempt removal flow.
    let probe = ApiClient::new(&provider, "us").map_err(|e| e.to_string())?;
    let storefront = match probe.user_storefront().await {
        Ok(sf) => sf,
        Err(e) => {
            return Err(format!(
                "Apple rejected the login check ({e}). Your saved token may have expired — re-save your MUT in Settings → Account, then retry."
            ));
        }
    };
    let client = ApiClient::new(&provider, &storefront).map_err(|e| e.to_string())?;
    let n = client
        .remove_from_playlist(&playlist_id, &song_ids)
        .await
        .map_err(|e| e.to_string())?;
    Ok(format!("removed {n} track(s)"))
}

pub async fn create_playlist(state: &AppState, name: String) -> Result<String, String> {
    let dev = resolve_developer_token(state).await?;
    let provider = ResolvedProvider {
        dev,
        mut_token: current_mut(state),
    };
    let client = ApiClient::new(&provider, "us").map_err(|e| e.to_string())?;
    client
        .create_playlist(&name)
        .await
        .map_err(|e| e.to_string())
}

/// Rename a library playlist (needs MUT). Thin IPC over
/// [`ApiClient::rename_playlist`]; empty names are rejected in core.
pub async fn rename_playlist(
    state: &AppState,
    playlist_id: String,
    name: String,
) -> Result<String, String> {
    let dev = resolve_developer_token(state).await?;
    let provider = ResolvedProvider {
        dev,
        mut_token: current_mut(state),
    };
    let storefront = resolve_storefront(state, &provider).await;
    let client = ApiClient::new(&provider, &storefront).map_err(|e| e.to_string())?;
    client
        .rename_playlist(&playlist_id, &name)
        .await
        .map_err(|e| e.to_string())?;
    Ok(format!("renamed to “{name}”"))
}

/// Delete a library playlist (needs MUT). Thin IPC over
/// [`ApiClient::delete_playlist`].
pub async fn delete_playlist(state: &AppState, playlist_id: String) -> Result<String, String> {
    let dev = resolve_developer_token(state).await?;
    let provider = ResolvedProvider {
        dev,
        mut_token: current_mut(state),
    };
    let storefront = resolve_storefront(state, &provider).await;
    let client = ApiClient::new(&provider, &storefront).map_err(|e| e.to_string())?;
    client
        .delete_playlist(&playlist_id)
        .await
        .map_err(|e| e.to_string())?;
    Ok("playlist deleted".into())
}

/// Share a library playlist (needs MUT): publish it if private, then return
/// the public `music.apple.com` link. Thin IPC over
/// [`ApiClient::share_library_playlist`].
pub async fn share_playlist(state: &AppState, playlist_id: String) -> Result<String, String> {
    let dev = resolve_developer_token(state).await?;
    let provider = ResolvedProvider {
        dev,
        mut_token: current_mut(state),
    };
    let storefront = resolve_storefront(state, &provider).await;
    let client = ApiClient::new(&provider, &storefront).map_err(|e| e.to_string())?;
    client
        .share_library_playlist(&playlist_id, &storefront)
        .await
        .map_err(|e| e.to_string())
}

/// Make a library playlist private again (needs MUT). Thin IPC over
/// [`ApiClient::set_playlist_public`] with `false`.
pub async fn unshare_playlist(state: &AppState, playlist_id: String) -> Result<String, String> {
    let dev = resolve_developer_token(state).await?;
    let provider = ResolvedProvider {
        dev,
        mut_token: current_mut(state),
    };
    let storefront = resolve_storefront(state, &provider).await;
    let client = ApiClient::new(&provider, &storefront).map_err(|e| e.to_string())?;
    client
        .set_playlist_public(&playlist_id, false)
        .await
        .map_err(|e| e.to_string())?;
    Ok("playlist is private".into())
}

pub async fn add_to_favorites(state: &AppState, song_ids: Vec<String>) -> Result<String, String> {
    let dev = resolve_developer_token(state).await?;
    let provider = ResolvedProvider {
        dev,
        mut_token: current_mut(state),
    };
    let storefront = resolve_storefront(state, &provider).await;
    let client = ApiClient::new(&provider, &storefront).map_err(|e| e.to_string())?;
    let n = client
        .add_to_library(&song_ids)
        .await
        .map_err(|e| e.to_string())?;
    Ok(format!("added {n} track(s) to favorites"))
}

pub async fn get_album(
    state: &AppState,
    id: String,
) -> Result<apple_music_core::models::AlbumDetail, String> {
    let dev = resolve_developer_token(state).await?;
    let provider = ResolvedProvider {
        dev,
        mut_token: current_mut(state),
    };
    let storefront = resolve_storefront(state, &provider).await;
    let client = ApiClient::new(&provider, &storefront).map_err(|e| e.to_string())?;
    let mut d = client.get_album(&id).await.map_err(|e| e.to_string())?;
    apply_dead_ids(state, &mut d.tracks);
    Ok(d)
}

/// Animated album cover (Apple Motion) for a song (resolved through its
/// album) or an album id directly. `None` means no motion art — play the
/// static artwork. Errors surface as strings; the UI treats them the same
/// as "no motion" and keeps static art.
pub async fn motion_artwork(
    state: &AppState,
    song_id: Option<String>,
    album_id: Option<String>,
) -> Result<Option<apple_music_core::models::MotionArtwork>, String> {
    let dev = resolve_developer_token(state).await?;
    let provider = ResolvedProvider {
        dev,
        mut_token: current_mut(state),
    };
    let storefront = resolve_storefront(state, &provider).await;
    let client = ApiClient::new(&provider, &storefront).map_err(|e| e.to_string())?;
    let art = if let Some(album) = album_id.filter(|s| !s.trim().is_empty()) {
        client.album_motion_artwork(&album).await
    } else if let Some(song) = song_id.filter(|s| !s.trim().is_empty()) {
        client.song_motion_artwork(&song).await
    } else {
        Ok(None)
    };
    let mut art = art.map_err(|e| e.to_string())?;
    if let Some(ma) = art.as_mut() {
        resolve_motion_variants(ma).await;
    }
    Ok(art)
}

/// Replace every motion HLS URL with a directly playable variant when it
/// points at a master playlist (see [`pick_hls_variant`]). Fetch or parse
/// trouble keeps the original URL untouched — the demux then falls back to
/// its own unfiltered variant choice, which at worst fails into the static
/// cover like any other motion error.
async fn resolve_motion_variants(ma: &mut apple_music_core::models::MotionArtwork) {
    for slot in [
        &mut ma.square_hls,
        &mut ma.tall_hls,
        &mut ma.square_small_hls,
    ] {
        let Some(url) = slot.clone() else { continue };
        if let Some(resolved) = resolve_hls_variant(&url).await {
            *slot = Some(resolved);
        }
    }
}

/// Fetch `url` and, when it holds an HLS master playlist, return the
/// [`pick_hls_variant`] choice — a single media playlist URL. Non-master
/// playlists and any network/HTTP trouble yield `None`.
async fn resolve_hls_variant(url: &str) -> Option<String> {
    let text = reqwest::Client::new()
        .get(url)
        .timeout(std::time::Duration::from_secs(10))
        .send()
        .await
        .ok()?
        .error_for_status()
        .ok()?
        .text()
        .await
        .ok()?;
    pick_hls_variant(&text, url)
}

/// Read one `KEY=` / `KEY="…"` attribute out of an `#EXT-X-STREAM-INF`
/// line body. Returns `None` when the key is absent.
fn hls_attr<'a>(attrs: &'a str, key: &str) -> Option<&'a str> {
    let start = attrs.find(&format!("{key}="))? + key.len() + 1;
    let rest = &attrs[start..];
    if let Some(quoted) = rest.strip_prefix('"') {
        quoted.find('"').map(|end| &quoted[..end])
    } else {
        rest.split(',').next()
    }
}

/// Join a playlist URI against the playlist's own URL: absolute URIs pass
/// through, root-relative ones keep the origin, path-relative ones resolve
/// against the parent directory.
fn hls_join(base: &str, uri: &str) -> String {
    if uri.starts_with("http://") || uri.starts_with("https://") {
        return uri.to_owned();
    }
    if let Some(rest) = uri.strip_prefix('/') {
        if let Some(scheme_end) = base.find("://") {
            let authority_start = scheme_end + 3;
            let path_start = base[authority_start..]
                .find('/')
                .map_or(base.len(), |i| authority_start + i);
            return format!("{}/{}", &base[..path_start], rest);
        }
    }
    match base.rfind('/') {
        Some(i) => format!("{}/{}", &base[..i], uri),
        None => uri.to_owned(),
    }
}

/// Pick a playable H.264 variant out of an HLS master playlist text.
///
/// Apple masters interleave `hvc1` (HEVC) renditions with `avc1` ones and
/// GStreamer's adaptive demux switches variants purely by bandwidth: on a
/// fast link it walks up to an HEVC rendition mid-stream, which then fails
/// to renegotiate against the h264 parser already wired downstream
/// (`not-negotiated` — the motion slot dies and restart-loops). Resolving
/// the master to one media playlist ourselves removes the alternatives
/// entirely, so the demux starts on a codec we know we can decode.
///
/// Policy: the smallest H.264 rendition that still covers the 512px
/// render target on every side, or the largest one when the album only
/// ships smaller renditions (ties keep the first listed). Returns `None`
/// when `text` isn't a master playlist or carries no `avc1` variant —
/// callers keep the original URL.
pub fn pick_hls_variant(text: &str, base: &str) -> Option<String> {
    const MIN_PX: u32 = 512;
    if !text.contains("#EXT-X-STREAM-INF:") {
        return None;
    }
    let lines: Vec<&str> = text.lines().map(str::trim).collect();
    // (min side, uri) of the best candidate above/below the target size.
    let mut big: Option<(u32, &str)> = None;
    let mut small: Option<(u32, &str)> = None;
    for (i, line) in lines.iter().enumerate() {
        let Some(attrs) = line.strip_prefix("#EXT-X-STREAM-INF:") else {
            continue;
        };
        // Only H.264 renditions; the URI sits on the line right after the
        // attributes (I-frame entries carry it inline and never match).
        let uri = match lines.get(i + 1) {
            Some(next) if !next.starts_with('#') && !next.is_empty() => *next,
            _ => continue,
        };
        if !hls_attr(attrs, "CODECS").is_some_and(|c| c.contains("avc1")) {
            continue;
        }
        let Some((w, h)) = hls_attr(attrs, "RESOLUTION")
            .and_then(|r| r.split_once('x'))
            .and_then(|(w, h)| Some((w.parse::<u32>().ok()?, h.parse::<u32>().ok()?)))
        else {
            continue;
        };
        let side = w.min(h);
        let slot = if side >= MIN_PX { &mut big } else { &mut small };
        if slot.is_none_or(|(s, _)| if side >= MIN_PX { side < s } else { side > s }) {
            *slot = Some((side, uri));
        }
    }
    big.or(small).map(|(_, uri)| hls_join(base, uri))
}

pub async fn get_playlist(
    state: &AppState,
    id: String,
) -> Result<apple_music_core::models::PlaylistDetail, String> {
    let dev = resolve_developer_token(state).await?;
    let provider = ResolvedProvider {
        dev,
        mut_token: current_mut(state),
    };
    let storefront = resolve_storefront(state, &provider).await;
    let client = ApiClient::new(&provider, &storefront).map_err(|e| e.to_string())?;
    let mut d = client.get_playlist(&id).await.map_err(|e| e.to_string())?;
    apply_dead_ids(state, &mut d.tracks);
    Ok(d)
}

pub async fn library_playlists(
    state: &AppState,
) -> Result<Vec<apple_music_core::models::Playlist>, String> {
    let dev = resolve_developer_token(state).await?;
    let provider = ResolvedProvider {
        dev,
        mut_token: current_mut(state),
    };
    let client = ApiClient::new(&provider, "us").map_err(|e| e.to_string())?;
    client.library_playlists().await.map_err(|e| e.to_string())
}

pub async fn get_lyrics(
    state: &AppState,
    song_id: String,
    artist: String,
    title: String,
) -> Result<apple_music_core::models::Lyrics, String> {
    let dev = resolve_developer_token(state).await?;
    let provider = ResolvedProvider {
        dev,
        mut_token: current_mut(state),
    };
    let storefront = resolve_storefront(state, &provider).await;
    let amp = ApiClient::new(&provider, &storefront).map_err(|e| e.to_string())?;
    let resolved = amp
        .resolve_catalog_song_id(&song_id, &artist, &title)
        .await
        .unwrap_or(song_id);
    let ctx = format!("sf={storefront}; id={resolved}");

    let mut apple_err = match amp.get_syllable_lyrics(&resolved).await {
        Ok(lyrics) if !lyrics.lines.is_empty() => return Ok(lyrics),
        Ok(_) => "syllable-lyrics: empty".into(),
        Err(e) => e.to_string(),
    };

    match amp.get_lyrics(&resolved).await {
        Ok(mut lyrics) if !lyrics.lines.is_empty() => {
            lyrics.source = "apple-line".into();
            return Ok(lyrics);
        }
        Ok(_) => {
            apple_err = if apple_err.is_empty() {
                "line-lyrics: empty".into()
            } else {
                format!("{apple_err}; line-lyrics: empty")
            };
        }
        Err(e) => {
            apple_err = if apple_err.is_empty() {
                e.to_string()
            } else {
                format!("{apple_err}; {e}")
            };
        }
    }

    match amp.get_lyrics_lrclib(&artist, &title).await {
        Ok(mut lyrics) => {
            lyrics.text = format!("{}\n\n— via LRCLIB", lyrics.text);
            lyrics.source = if apple_err.is_empty() {
                format!("lrclib; {ctx}")
            } else {
                format!("lrclib; {ctx}; {apple_err}")
            };
            Ok(lyrics)
        }
        Err(e) => Err(if apple_err.is_empty() {
            e.to_string()
        } else {
            format!("{apple_err}; LRCLIB: {e}")
        }),
    }
}

pub async fn authorize_url(state: &AppState) -> Result<String, String> {
    let dev = resolve_developer_token(state).await?;
    // Third argument is the `referrer` hint Apple's WOA page records —
    // not a redirect target, so nothing has to handle this scheme. Kept
    // verbatim from the old shell so the manual flow behaves the same.
    Ok(auth::build_authorize_url(
        &dev,
        "Sonora",
        "tauri://localhost",
    ))
}

pub fn open_auth_url(url: String) -> Result<(), String> {
    crate::open_url(url)
}

pub fn submit_user_token(state: &AppState, token: String) -> Result<(), String> {
    let token = token.trim().to_string();
    if !valid_mut(&token) {
        return Err(
            "that doesn't look like a music user token (too short or not an opaque string)".into(),
        );
    }
    state
        .tokens
        .set_music_user_token(token.clone())
        .map_err(|e| e.to_string())?;
    persist_mut(&token)?;
    Ok(())
}

/// Paste the full `authorize.music.apple.com/?...&musicUserToken=...` redirect
/// URL; the app extracts and stores the MUT.
pub fn submit_auth_url(state: &AppState, url: String) -> Result<String, String> {
    let token = auth::extract_user_token_from_url(&url)
        .ok_or_else(|| "no musicUserToken found in that URL".to_string())?;
    if !valid_mut(&token) {
        return Err(
            "that URL's musicUserToken doesn't look like a real token — paste the full \
             redirect URL from the browser"
                .into(),
        );
    }
    state
        .tokens
        .set_music_user_token(token.clone())
        .map_err(|e| e.to_string())?;
    persist_mut(&token)?;
    Ok(format!(
        "MUT saved to disk ({} chars). Try Search.",
        token.len()
    ))
}

/// Drop a pending automatic sign-in server, if any.
pub fn abort_auth_flow(state: &AppState) {
    if let Ok(mut slot) = state.auth_server.lock() {
        if let Some(handle) = slot.take() {
            handle.abort();
        }
    }
}

/// Automatic sign-in: opens the system browser on a one-shot localhost
/// page (MusicKit `authorize()`), waits up to 5 minutes for the approval,
/// then stores the MUT like a manual paste. No copy-paste involved.
///
/// Takes the shared state by `Arc` (not `&`) so the localhost server can
/// hand a `'static` token-refresh callback to the auth page: if the
/// injected developer token expires while the tab is open, the page
/// re-resolves it instead of dying with `Initialized with an expired token.`.
pub async fn start_signin(state: std::sync::Arc<AppState>) -> Result<String, String> {
    if let Ok(slot) = state.auth_server.lock() {
        if let Some(handle) = slot.as_ref() {
            if !handle.is_finished() {
                return Err("sign-in already in progress — finish or cancel it first".into());
            }
        }
    }
    let dev = resolve_developer_token(&state).await?;
    let (tx, rx) = tokio::sync::oneshot::channel::<String>();
    let refresh_state = state.clone();
    let refresh: crate::auth_flow::TokenRefresh = std::sync::Arc::new(move |force| {
        let state = refresh_state.clone();
        Box::pin(async move {
            if force {
                // The page's MusicKit setup failed even though the token
                // looked fresh locally — the cache itself is the suspect
                // (stale or poisoned `web_token_cache`). Drop memory + disk
                // copies so the scrape below is the only source left.
                clear_dev_token_cache(&state);
            }
            resolve_developer_token(&state).await
        })
    });
    let (port, handle) = crate::auth_flow::run_signin_server(dev, refresh, tx).await?;
    *state.auth_server.lock().map_err(|e| e.to_string())? = Some(handle);
    crate::open_url(format!("http://127.0.0.1:{port}/"))?;
    let token = match tokio::time::timeout(std::time::Duration::from_secs(300), rx).await {
        Ok(Ok(t)) => t,
        Ok(Err(_)) => {
            abort_auth_flow(&state);
            return Err("sign-in cancelled".into());
        }
        Err(_) => {
            abort_auth_flow(&state);
            return Err("sign-in timed out after 5 minutes — try again".into());
        }
    };
    abort_auth_flow(&state);
    let token = token.trim().to_string();
    if !valid_mut(&token) {
        return Err("sign-in returned an empty or invalid token — try again".into());
    }
    state
        .tokens
        .set_music_user_token(token.clone())
        .map_err(|e| e.to_string())?;
    persist_mut(&token)?;
    Ok(format!(
        "Signed in — MUT saved to disk ({} chars). Try Search.",
        token.len()
    ))
}

/// Abort a pending automatic sign-in (the browser tab will just stop working).
pub fn cancel_signin(state: &AppState) -> Result<String, String> {
    let mut slot = state.auth_server.lock().map_err(|e| e.to_string())?;
    match slot.take() {
        Some(handle) => {
            handle.abort();
            Ok("sign-in cancelled".into())
        }
        None => Err("no sign-in in progress".into()),
    }
}

/// Whether an MUT is currently stored (in memory or on disk).
pub fn auth_state(state: &AppState) -> Result<bool, String> {
    Ok(current_mut(state).is_some())
}

/// Drop the scraped developer token everywhere it lives (memory + disk)
/// so the next [`resolve_developer_token`] re-fetches from Apple. The env
/// token is never touched — it is the user's own key.
pub fn clear_dev_token_cache(state: &AppState) {
    if let Ok(mut slot) = state.web_token_cache.lock() {
        *slot = None;
    }
    if let Some(path) = web_token_cache_path() {
        let _ = std::fs::remove_file(&path);
    }
}

/// Log out: drop every credential the app stored for the account — the
/// MUT and the cached developer token, in memory and on disk — abort any
/// pending sign-in, and stop playback. Wiping the dev-token cache too
/// matters: a stale copy is what blocks the *next* login on the localhost
/// sign-in page (`Initialized with an expired token.`).
pub fn logout(state: &AppState) -> Result<String, String> {
    abort_auth_flow(state);
    state
        .tokens
        .set_music_user_token(String::new())
        .map_err(|e| e.to_string())?;
    if let Some(dir) = paths::app_config_dir() {
        wipe_token_files(&dir);
    }
    clear_dev_token_cache(state);
    if let Ok(mut sf) = state.storefront_cache.lock() {
        *sf = None;
    }
    let _ = state.native.stop();
    Ok("Logged out — saved login and cached tokens removed, playback stopped.".into())
}

/// True when an error string means "Apple rejected our credentials with
/// 401" rather than an id-level or transient failure. Producers format
/// the status as `http 401 Unauthorized` (`core::api`). Deliberately *not*
/// matched: `missing music user token` — that is "never signed in", not a
/// dead session, and must not raise the expiry banner.
pub fn is_auth_error(e: &str) -> bool {
    let l = e.to_lowercase();
    l.contains("401 unauthorized") || l.contains("http 401")
}

/// What a 401 means for this app right now.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum AuthFailure {
    /// The cached developer token expired locally; it was dropped, so the
    /// next call re-fetches from Apple — retrying the action is enough.
    DevTokenRefreshed,
    /// The developer token is still good: Apple rejected the MUT, the
    /// user has to sign in again.
    SessionExpired,
    /// `APPLE_MUSIC_DEVELOPER_TOKEN` itself expired; only the user can
    /// renew it (the env key is an explicit override, never auto-swapped).
    EnvTokenExpired,
}

/// Classify a 401: inspect the local token state to tell an expired
/// developer token (auto-fixable) from an expired MUT (re-login). Only
/// the source [`resolve_developer_token`] would have used is judged —
/// memory first, then the file. The auto-fix branch performs the cache
/// drop as its side effect.
pub fn classify_auth_failure(state: &AppState) -> AuthFailure {
    if let Ok(t) = state.tokens.developer_token() {
        if !t.trim().is_empty() {
            if apple_music_core::token::token_fresh_for(&t, DEV_TOKEN_MIN_TTL_SECS) {
                return AuthFailure::SessionExpired;
            }
            return AuthFailure::EnvTokenExpired;
        }
    }
    let memory_cached = state.web_token_cache.lock().ok().and_then(|g| g.clone());
    if let Some(t) = memory_cached {
        if apple_music_core::token::token_fresh_for(t.trim(), DEV_TOKEN_MIN_TTL_SECS) {
            return AuthFailure::SessionExpired;
        }
        clear_dev_token_cache(state);
        return AuthFailure::DevTokenRefreshed;
    }
    if let Some(raw) = web_token_cache_path().and_then(|p| std::fs::read_to_string(p).ok()) {
        if apple_music_core::token::token_fresh_for(raw.trim(), DEV_TOKEN_MIN_TTL_SECS) {
            return AuthFailure::SessionExpired;
        }
        clear_dev_token_cache(state);
        return AuthFailure::DevTokenRefreshed;
    }
    AuthFailure::SessionExpired
}

/// Expand one queue item to song ids: songs pass through, albums/playlists
/// expand via the catalog. Capped so a huge playlist can't stall playback.
pub async fn expand_queue_item(
    client: &ApiClient<'_>,
    item: &QueueItem,
) -> Result<Vec<String>, String> {
    match item.kind.as_str() {
        "album" => {
            let detail = client
                .get_album(&item.id)
                .await
                .map_err(|e| e.to_string())?;
            Ok(detail.tracks.into_iter().map(|t| t.id).take(200).collect())
        }
        "playlist" => {
            let detail = client
                .get_playlist(&item.id)
                .await
                .map_err(|e| e.to_string())?;
            Ok(detail.tracks.into_iter().map(|t| t.id).take(200).collect())
        }
        _ => Ok(vec![item.id.clone()]),
    }
}

/// Resolve one queued song to its catalog id (library `i.*` ids map first).
/// Unresolvable ids (e.g. uploaded tracks with no catalog mapping) pass
/// through unchanged: the native pipeline plays library ids directly via
/// `universalLibraryId` dispatch.
pub async fn resolve_song_id(client: &ApiClient<'_>, id: &str) -> String {
    if ApiClient::is_library_song_id(id) {
        match client.catalog_id_for_library_song(id).await {
            Ok(catalog) => return catalog,
            Err(e) => eprintln!(
                "sonora native: catalog resolve failed for {id} ({e}) — trying library dispatch"
            ),
        }
    }
    id.to_string()
}

/// Shared context for one play request's resolve chain: everything the
/// `native_play_item` → `play_catalog_id` → `retry_by_metadata` chain
/// threads through (kept as one struct so clippy's argument count stays
/// sane as the chain grows checks). The catalog client is built per call
/// inside [`native_play_item`] and passed alongside (its storefront
/// resolve needs an await first).
pub struct PlayReq<'a> {
    state: &'a AppState,
    provider: &'a ResolvedProvider,
    songs: &'a [QueueItem],
    start: usize,
    play_gen: u64,
}

/// Play one queued song through the native engine: catalog resolve →
/// metadata → decrypt → decode → publish. Returns `(title, played_id)`.
/// A dead library id is retried once via metadata search (stale ids after
/// library re-sync). The queue cursor is owned by the caller
/// (`player_play` sets it, `step` moves it). `play_gen` is the click-order
/// claim from [`native_play`]/[`native_step`]: every await re-checks it so
/// a superseded request unwinds instead of clobbering the newer track.
pub async fn native_play_item(req: &PlayReq<'_>) -> Result<(String, String), String> {
    use native_player::SUPERSEDED_MSG;
    if !req.state.native.is_current_play(req.play_gen) {
        return Err(SUPERSEDED_MSG.into());
    }
    let storefront = resolve_storefront(req.state, req.provider).await;
    if !req.state.native.is_current_play(req.play_gen) {
        return Err(SUPERSEDED_MSG.into());
    }
    let client = ApiClient::new(req.provider, &storefront).map_err(|e| e.to_string())?;
    let item = req
        .songs
        .get(req.start)
        .ok_or_else(|| "nothing playable in that queue".to_string())?;
    let catalog_id = resolve_song_id(&client, &item.id).await;
    if !req.state.native.is_current_play(req.play_gen) {
        return Err(SUPERSEDED_MSG.into());
    }
    let result = match play_catalog_id(req, &client, &catalog_id).await {
        Ok(title) => {
            let _ = req.state.native.retarget_current(&catalog_id);
            clear_dead_ids(req.state, &[item.id.as_str(), catalog_id.as_str()]);
            Ok((title, catalog_id.clone()))
        }
        Err(e) if is_unresolvable_play_error(&e) => {
            retry_by_metadata(req, &client, item, &catalog_id, &e).await
        }
        Err(e) => Err(e),
    };
    if let Err(e) = &result {
        if is_unresolvable_play_error(e) {
            note_dead_ids(req.state, &[item.id.as_str(), catalog_id.as_str()]);
        }
    }
    result
}

/// Commit the queue and start playback. Runs with no awaits between the
/// ownership check and the engine handoff, so a superseded play can never
/// leave its queue behind for the winner to play into.
pub fn commit_queue_and_start(req: &PlayReq<'_>) -> Result<(), String> {
    use native_player::SUPERSEDED_MSG;
    if !req.state.native.is_current_play(req.play_gen) {
        return Err(SUPERSEDED_MSG.into());
    }
    req.state.native.set_queue(req.songs.to_vec(), req.start)?;
    Ok(())
}

/// Play an already-resolved catalog id: metadata → decrypt → publish.
/// The PCM-file hit loads the buffer concurrently with the metadata fetch;
/// the miss path runs the metadata fetch concurrently with
/// resolve → download → license (previously a sequential RTT chain).
/// The player queue commits here (post-resolve, synchronously with the
/// engine handoff) so overlapping plays can't interleave queue writes.
pub async fn play_catalog_id(
    req: &PlayReq<'_>,
    client: &ApiClient<'_>,
    catalog_id: &str,
) -> Result<String, String> {
    use native_player::SUPERSEDED_MSG;
    let state = req.state;
    let play_gen = req.play_gen;
    if !state.native.is_current_play(play_gen) {
        return Err(SUPERSEDED_MSG.into());
    }
    let mut_ = req
        .provider
        .music_user_token()
        .ok_or_else(|| "no MUT saved — sign in first".to_string())?;
    if am_playback::pcm_cache::is_cached(catalog_id) {
        let id = catalog_id.to_string();
        let (meta_res, file_res) = tokio::join!(
            client.get_song(catalog_id),
            tokio::task::spawn_blocking(move || {
                am_playback::pcm_cache::load_valid(&id)
                    .map(std::sync::Arc::new)
                    .ok_or_else(|| "pcm miss".to_string())
            })
        );
        let meta = meta_res
            .map(|v| native_player::meta_from_song(&v))
            .unwrap_or_default();
        match file_res.map_err(|e| format!("pcm load task: {e}"))? {
            Ok(decoded) => {
                commit_queue_and_start(req)?;
                state.native.start_decoded(
                    catalog_id.to_string(),
                    meta.clone(),
                    decoded,
                    play_gen,
                )?;
                // Prefetch the next few tracks into PCM files on disk.
                prefetch_ids(state, state.native.upcoming(3)).await;
                return Ok(meta.title.clone().unwrap_or_else(|| catalog_id.to_string()));
            }
            Err(_) => {
                // File vanished/invalid between check and load — fall
                // through to the progressive path below.
            }
        }
    }
    let (meta_res, begin_res) = tokio::join!(
        client.get_song(catalog_id),
        am_playback::stream::ProgressiveCtx::begin(catalog_id, &mut_)
    );
    let meta = meta_res
        .map(|v| native_player::meta_from_song(&v))
        .unwrap_or_default();
    let prog = begin_res.map_err(|e| {
        // Honest failure of THIS play only. A newer skip already owns
        // `play_gen` and must keep the device.
        let _ = state.native.stop_if_current(play_gen);
        e.to_string()
    })?;
    // No awaits from here to the engine handoff: queue commit and audio
    // start belong to the same ownership check.
    commit_queue_and_start(req)?;
    state
        .native
        .start_ready(catalog_id.to_string(), meta.clone(), prog, play_gen)?;
    // Prefetch the next few tracks into PCM files on disk.
    prefetch_ids(state, state.native.upcoming(3)).await;
    Ok(meta.title.clone().unwrap_or_else(|| catalog_id.to_string()))
}

/// True for play failures worth a metadata retry: the id (not the session)
/// is the problem — unknown mapping, empty store answer, gone track.
/// Covers both the store-message shape (`track unavailable (…) [code
/// 3076]`) and the describe-fallback shape (`missing-songList …,
/// …ItemNotAvailableForFuse…`) when Apple sends no commerce dialog.
pub fn is_unresolvable_play_error(e: &str) -> bool {
    let lower = e.to_lowercase();
    [
        "itemnotfound",
        "itemnotavailable",
        "notavailable",
        "no longer available",
        "not available",
        "track unavailable",
        "unavailable",
        "no songlist",
        "empty songlist",
        "missing-songlist",
        "no mapping",
        "404",
    ]
    .iter()
    .any(|m| lower.contains(m))
}

/// One-shot retry: find the song by title/artist and play the fresh
/// catalog id. Metadata comes from the queue item, else from the library
/// resource itself. When no fresher id turns up, the same id is tried once
/// more for transient store failures (307x flakiness on live ids) before
/// giving up. Never recurses — a second failure propagates as-is.
pub async fn retry_by_metadata(
    req: &PlayReq<'_>,
    client: &ApiClient<'_>,
    item: &QueueItem,
    catalog_id: &str,
    original: &str,
) -> Result<(String, String), String> {
    let state = req.state;
    let play_gen = req.play_gen;
    if let Some((title, artist)) = match (&item.title, &item.artist) {
        (Some(t), Some(a)) => Some((t.clone(), a.clone())),
        _ => client.library_song_attrs(&item.id).await.ok(),
    } {
        if !title.trim().is_empty() && !artist.trim().is_empty() {
            if let Ok(results) = client.search(&format!("{title} {artist}"), 10).await {
                if let Some(hit) = results
                    .tracks
                    .iter()
                    .find(|t| apple_music_core::models::track_matches(&title, &artist, t))
                {
                    if hit.id != item.id {
                        if !state.native.is_current_play(play_gen) {
                            return Err(native_player::SUPERSEDED_MSG.into());
                        }
                        eprintln!(
                            "sonora native: stale id {} → catalog match {} ({title} — {artist})",
                            item.id, hit.id
                        );
                        let played = hit.id.clone();
                        let title = match play_catalog_id(req, client, &played).await {
                            Ok(t) => t,
                            Err(e) => {
                                if is_unresolvable_play_error(&e) {
                                    note_dead_ids(req.state, &[played.as_str()]);
                                }
                                return Err(e);
                            }
                        };
                        let _ = state.native.retarget_current(&played);
                        clear_dead_ids(req.state, &[played.as_str()]);
                        return Ok((title, played));
                    }
                }
            }
        }
    }
    // No fresher id (or no metadata to search with): one immediate retry
    // of the same id for transient store failures, then give up.
    if !state.native.is_current_play(play_gen) {
        return Err(native_player::SUPERSEDED_MSG.into());
    }
    eprintln!("sonora native: re-trying {catalog_id} after ({original})");
    let title = play_catalog_id(req, client, catalog_id).await?;
    Ok((title, catalog_id.to_string()))
}

/// What actually started playing: the UI awaits one id, but a metadata
/// retry may substitute a fresh catalog id — the UI reconciles onto it.
#[derive(serde::Serialize)]
pub struct PlayResult {
    pub track_id: String,
    pub message: String,
}

/// Native `PlayNow`: expand album/playlist items, store the queue, play the target.
pub async fn native_play(
    state: &AppState,
    provider: ResolvedProvider,
    items: Vec<QueueItem>,
    start_index: u32,
) -> Result<PlayResult, String> {
    if items.is_empty() {
        return Err("nothing playable in that queue".into());
    }
    // Click-order claim BEFORE the first await: overlapping plays resolve
    // in click order (see NativePlayer::claim_play). Silence the old track
    // NOW so a slow resolve never leaves stale audio under the new UI.
    let play_gen = state.native.claim_and_silence()?;
    let storefront = resolve_storefront(state, &provider).await;
    let client = ApiClient::new(&provider, &storefront).map_err(|e| e.to_string())?;
    let mut songs: Vec<QueueItem> = Vec::new();
    for item in &items {
        if !state.native.is_current_play(play_gen) {
            return Err(native_player::SUPERSEDED_MSG.into());
        }
        let ids = expand_queue_item(&client, item).await?;
        // Keep UI metadata only when the item passes through untouched
        // (container expansions must not inherit it for fallback search).
        let passthrough = ids.len() == 1 && ids[0] == item.id;
        for id in ids {
            songs.push(QueueItem {
                id,
                kind: "song".into(),
                title: passthrough.then(|| item.title.clone()).flatten(),
                artist: passthrough.then(|| item.artist.clone()).flatten(),
            });
        }
    }
    if songs.is_empty() {
        return Err("nothing playable in that queue".into());
    }
    let start = (start_index as usize).min(songs.len() - 1);
    eprintln!(
        "sonora native: queue={} start={} target={}",
        songs.len(),
        start,
        songs[start].id
    );
    // The player queue commits post-resolve inside `play_catalog_id`
    // (synchronously with the engine handoff), never here: an early commit
    // lets a superseded play leave its queue behind for the winner.
    let req = PlayReq {
        state,
        provider: &provider,
        songs: &songs,
        start,
        play_gen,
    };
    let (title, played_id) = native_play_item(&req).await?;
    Ok(PlayResult {
        track_id: played_id,
        message: format!("playing (native): {title}"),
    })
}

pub async fn native_step(
    state: &AppState,
    provider: ResolvedProvider,
    delta: isize,
) -> Result<(), String> {
    // Cursor move first (no audio touched): an empty queue errors without
    // silencing anything. Claim + announce follow before the first await.
    let stepped = state.native.step(delta)?;
    if stepped.is_none() {
        return Err("queue is empty".into());
    }
    let play_gen = state.native.claim_and_silence()?;
    // Re-snapshot after the step: the commit path re-asserts this play's
    // ownership over the (unchanged) queue at handoff time.
    let (songs, start) = state.native.snapshot_queue()?;
    let req = PlayReq {
        state,
        provider: &provider,
        songs: &songs,
        start,
        play_gen,
    };
    native_play_item(&req).await.map(|_| ())?;
    Ok(())
}

pub fn native_provider(state: &AppState, dev: String) -> ResolvedProvider {
    ResolvedProvider {
        dev,
        mut_token: current_mut(state),
    }
}

/// Fire-and-forget prefetch of upcoming ids into the decoded cache.
pub async fn prefetch_ids(state: &AppState, items: Vec<QueueItem>) {
    if items.is_empty() {
        return;
    }
    let Ok(dev) = resolve_developer_token(state).await else {
        return;
    };
    let provider = native_provider(state, dev.clone());
    let Some(mut_) = provider.music_user_token().filter(|t| !t.trim().is_empty()) else {
        return;
    };
    let storefront = resolve_storefront(state, &provider).await;
    state.native.prefetch(
        dev,
        mut_,
        storefront,
        items.into_iter().map(|q| q.id).collect(),
    );
}

pub async fn player_play(
    state: &AppState,
    items: Vec<QueueItem>,
    start_index: Option<u32>,
) -> Result<PlayResult, String> {
    let dev = resolve_developer_token(state).await?;
    if current_mut(state).is_none() {
        return Err("no MUT saved — sign in first".into());
    }
    let provider = native_provider(state, dev);
    native_play(state, provider, items, start_index.unwrap_or(0)).await
}
/// Warm the player at boot: prefetch the bearer token + CDM in the
/// background (first play then skips the ~20MB download + scrape).
pub async fn player_warmup(_state: &AppState) -> Result<u16, String> {
    tokio::spawn(async {
        let _ = am_playback::bearer::get_bearer_token().await;
        let _ = am_playback::widevine::fetch::ensure().await;
    });
    Ok(0)
}

/// Resume without touching the queue (pause → play path).
pub async fn player_resume(state: &AppState) -> Result<(), String> {
    // No current track yet: resume is a no-op rather than an error so a
    // stray play press never fails the UI.
    state.native.resume()
}

pub async fn player_pause(state: &AppState) -> Result<(), String> {
    state.native.pause()
}

pub async fn player_next(state: &AppState) -> Result<(), String> {
    let dev = resolve_developer_token(state).await?;
    let provider = native_provider(state, dev);
    native_step(state, provider, 1).await
}

pub async fn player_previous(state: &AppState) -> Result<(), String> {
    let dev = resolve_developer_token(state).await?;
    let provider = native_provider(state, dev);
    native_step(state, provider, -1).await
}

pub async fn player_seek(state: &AppState, position_ms: u64) -> Result<(), String> {
    state.native.seek(position_ms).await
}

pub async fn player_status(state: &AppState) -> Result<PlayerReport, String> {
    Ok(state.native.status())
}

pub fn player_stop(state: &AppState) -> Result<(), String> {
    state.native.stop()
}

pub async fn player_volume(state: &AppState, level: f32) -> Result<(), String> {
    state.native.set_volume(level.clamp(0.0, 1.0))
}

pub async fn player_append(state: &AppState, items: Vec<QueueItem>) -> Result<(), String> {
    let pre = items.clone();
    state.native.queue_append(items)?;
    prefetch_ids(state, pre).await;
    Ok(())
}

pub async fn player_play_next(state: &AppState, items: Vec<QueueItem>) -> Result<(), String> {
    let pre = items.clone();
    state.native.queue_next(items)?;
    prefetch_ids(state, pre).await;
    Ok(())
}

pub async fn player_clear(state: &AppState) -> Result<(), String> {
    state.native.queue_clear()?;
    state.native.stop()
}

pub async fn playlist_recommendations(
    state: &AppState,
    seed_ids: Vec<String>,
    exclude_ids: Option<Vec<String>>,
    limit: Option<u8>,
    page: Option<u32>,
) -> Result<Vec<apple_music_core::models::Track>, String> {
    let dev = resolve_developer_token(state).await?;
    let provider = ResolvedProvider {
        dev,
        mut_token: current_mut(state),
    };
    let storefront = resolve_storefront(state, &provider).await;
    let client = ApiClient::new(&provider, &storefront).map_err(|e| e.to_string())?;
    let exclude: std::collections::HashSet<String> =
        exclude_ids.unwrap_or_default().into_iter().collect();
    let mut v = client
        .playlist_recommendations(
            &seed_ids,
            limit.unwrap_or(10).clamp(1, 25),
            &exclude,
            page.unwrap_or(0).min(8),
        )
        .await
        .map_err(|e| e.to_string())?;
    apply_dead_ids(state, &mut v);
    Ok(v)
}

pub async fn similar_songs(
    state: &AppState,
    song_id: String,
    exclude_ids: Option<Vec<String>>,
    depth: Option<u32>,
) -> Result<Vec<apple_music_core::models::Track>, String> {
    let dev = resolve_developer_token(state).await?;
    let provider = ResolvedProvider {
        dev,
        mut_token: current_mut(state),
    };
    let storefront = resolve_storefront(state, &provider).await;
    let client = ApiClient::new(&provider, &storefront).map_err(|e| e.to_string())?;
    let exclude: std::collections::HashSet<String> =
        exclude_ids.unwrap_or_default().into_iter().collect();
    let mut v = client
        .similar_songs(&song_id, 25, &exclude, depth.unwrap_or(0).min(8))
        .await
        .map_err(|e| e.to_string())?;
    apply_dead_ids(state, &mut v);
    Ok(v)
}

/// Track-change desktop notifications (Settings → Notifications, opt-in,
/// default off). Checked by the MPRIS bridge before firing a toast.
pub fn set_player_notifications(state: &AppState, enabled: bool) -> Result<String, String> {
    state.native.set_notifications(enabled)?;
    Ok(if enabled {
        "notifications on".into()
    } else {
        "notifications off".into()
    })
}

pub fn player_notifications(state: &AppState) -> Result<bool, String> {
    Ok(state.native.notifications_enabled())
}

/// Known store-dead ids for the UI's session set (gray restored queues and
/// lists before any backend detail fetch re-stamps them).
pub fn dead_ids(state: &AppState) -> Result<Vec<String>, String> {
    Ok(state
        .dead_ids
        .lock()
        .map(|s| s.iter().cloned().collect())
        .unwrap_or_default())
}

// ---- Discord Rich Presence (opt-in) ----

pub fn set_discord_enabled(state: &AppState, enabled: bool) -> Result<String, String> {
    state.discord.set_enabled(enabled);
    Ok(if enabled {
        "discord status on".into()
    } else {
        "discord status off".into()
    })
}

pub fn set_discord_app_id(state: &AppState, app_id: String) -> Result<String, String> {
    state.discord.set_app_id(app_id);
    Ok("discord app id saved".into())
}

pub fn update_discord_presence(state: &AppState, payload: PresencePayload) -> Result<(), String> {
    state.discord.update(&payload);
    Ok(())
}

pub fn clear_discord_presence(state: &AppState) -> Result<(), String> {
    state.discord.clear();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::pick_hls_variant;

    const MASTER: &str = "\
#EXTM3U
#EXT-X-I-FRAME-STREAM-INF:BANDWIDTH=1000,CODECS=\"avc1.64001f\",RESOLUTION=486x486,URI=\"https://example.com/iframe.m3u8\"
#EXT-X-STREAM-INF:AVERAGE-BANDWIDTH=900000,BANDWIDTH=1000000,CODECS=\"avc1.64001f\",RESOLUTION=486x486
https://example.com/v/avc486.m3u8
#EXT-X-STREAM-INF:AVERAGE-BANDWIDTH=800000,BANDWIDTH=900000,CODECS=\"hvc1.2.20000000.L123.B0\",RESOLUTION=768x768
https://example.com/v/hevc768.m3u8
#EXT-X-STREAM-INF:BANDWIDTH=2100000,CODECS=\"avc1.64001f\",RESOLUTION=768x768
https://example.com/v/avc768.m3u8
#EXT-X-STREAM-INF:BANDWIDTH=4000000,CODECS=\"avc1.640020\",RESOLUTION=1080x1080
https://example.com/v/avc1080.m3u8
";

    #[test]
    fn picks_smallest_avc1_covering_the_render_target() {
        // 768 (not the hevc 768, not the costlier 1080) is the first H.264
        // rendition at or above 512px.
        let got = pick_hls_variant(MASTER, "https://example.com/m/master.m3u8");
        assert_eq!(got.as_deref(), Some("https://example.com/v/avc768.m3u8"));
    }

    #[test]
    fn falls_back_to_the_largest_smaller_avc1() {
        let smaller = MASTER.replace(
            "#EXT-X-STREAM-INF:BANDWIDTH=2100000,CODECS=\"avc1.64001f\",RESOLUTION=768x768\nhttps://example.com/v/avc768.m3u8\n",
            "",
        );
        let smaller = smaller.replace(
            "#EXT-X-STREAM-INF:BANDWIDTH=4000000,CODECS=\"avc1.640020\",RESOLUTION=1080x1080\nhttps://example.com/v/avc1080.m3u8\n",
            "",
        );
        // Nothing ≥512 left; 486 wins over… nothing else, and the hevc is ignored.
        let got = pick_hls_variant(&smaller, "https://example.com/m/master.m3u8");
        assert_eq!(got.as_deref(), Some("https://example.com/v/avc486.m3u8"));
    }

    #[test]
    fn rejects_non_masters_and_hevc_only_masters() {
        let hevc_only = MASTER
            .replace(
                "CODECS=\"avc1.64001f\",RESOLUTION=486x486",
                "CODECS=\"hvc1.2\",RESOLUTION=486x486",
            )
            .replace(
                "CODECS=\"avc1.64001f\",RESOLUTION=768x768",
                "CODECS=\"hvc1.2\",RESOLUTION=768x768",
            )
            .replace(
                "CODECS=\"avc1.640020\",RESOLUTION=1080x1080",
                "CODECS=\"hvc1.2\",RESOLUTION=1080x1080",
            );
        assert_eq!(pick_hls_variant(&hevc_only, "https://e.com/m.m3u8"), None);
        let media = "#EXTM3U\n#EXT-X-TARGETDURATION:3\n#EXTINF:2.9,\nseg1.m4s\n";
        assert_eq!(pick_hls_variant(media, "https://e.com/m.m3u8"), None);
    }

    #[test]
    fn resolves_relative_variant_uris_against_the_master() {
        let rel = MASTER.replace("https://example.com/v/avc768.m3u8", "v/avc768.m3u8");
        let got = pick_hls_variant(&rel, "https://example.com/hls/master.m3u8");
        assert_eq!(
            got.as_deref(),
            Some("https://example.com/hls/v/avc768.m3u8")
        );
    }

    #[test]
    fn wipes_both_credential_files() {
        let dir = std::env::temp_dir().join(format!("sonora-wipe-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(super::MUT_FILE), "MUT").unwrap();
        std::fs::write(dir.join(super::WEB_TOKEN_FILE), "JWT").unwrap();
        std::fs::write(dir.join("keep-me.json"), "[]").unwrap();

        super::wipe_token_files(&dir);

        assert!(!dir.join(super::MUT_FILE).exists());
        assert!(!dir.join(super::WEB_TOKEN_FILE).exists());
        // Only credentials go — other config stays.
        assert!(dir.join("keep-me.json").exists());
        // Idempotent on an already-clean dir.
        super::wipe_token_files(&dir);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn validates_mut_shape_before_persisting() {
        // Real MUTs: long opaque strings — accepted.
        let real = "AsDfGhJkLqWeRtYuIoPzXcVbNmQwErTyUiOp123456";
        assert!(super::valid_mut(real));
        // Trimmed on the way in.
        assert!(super::valid_mut(&format!("  {real}\n")));

        // The poison this guards against: error bodies and leftovers from
        // a crashed write, none of which can be a token.
        assert!(!super::valid_mut(""), "empty");
        assert!(!super::valid_mut("   "), "whitespace only");
        assert!(!super::valid_mut("short"), "too short");
        assert!(
            !super::valid_mut(r#"{"error":"not authorized"}"#),
            "JSON error body"
        );
        assert!(!super::valid_mut("<html>401</html>"), "HTML error body");
        assert!(
            !super::valid_mut("AsDfGhJkLqWe\nRtYuIoPzXcVbNmQwErTyUiOp"),
            "interior whitespace (truncated/multi-line write)"
        );
    }

    #[test]
    fn recognizes_auth_errors() {
        assert!(super::is_auth_error(
            "add-to-library: http 401 Unauthorized (not authorized to view resources)"
        ));
        assert!(super::is_auth_error("search: http 401 Unauthorized"));
        // Id-level and transport failures must not be mistaken for a dead
        // session (they would gray tracks / nag for re-login), and
        // "never signed in" is not a dead session either.
        assert!(!super::is_auth_error("song: http 404 Not Found"));
        assert!(!super::is_auth_error(
            "lyrics: none published for this song"
        ));
        assert!(!super::is_auth_error(
            "track unavailable (no mapping) [code 3076]"
        ));
        assert!(!super::is_auth_error(
            "missing music user token (authorize first)"
        ));
    }
}
