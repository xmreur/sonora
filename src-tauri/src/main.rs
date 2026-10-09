//! Tauri shell: thin IPC layer over `apple-music-core` + `am-playback`.
//! Audio plays in-process (webPlayback resolve → Widevine license → CENC
//! decrypt → local output); the webview is UI only.

use apple_music_core::{
    api::ApiClient,
    auth,
    playback::QueueItem,
    token::{EnvTokenProvider, TokenProvider},
};
use std::sync::Mutex;
use tauri::{Manager, State};
use tauri_plugin_opener::OpenerExt;

mod paths;

mod native_player;
use native_player::{NativePlayer, PlayerReport};

mod media_session;

mod auth_flow;

mod discord;
use discord::{DiscordManager, PresencePayload};

struct AppState {
    tokens: EnvTokenProvider,
    /// Cache for the auto-fetched web-player token (subscription-only path).
    web_token_cache: Mutex<Option<String>>,
    /// Cached (MUT, storefront) so warm plays skip the per-play
    /// `/v1/me/storefront` probe. Overwritten whenever the MUT differs.
    storefront_cache: Mutex<Option<(String, String)>>,
    /// In-process playback engine + shared status hub.
    native: NativePlayer,
    /// Pending automatic sign-in server (aborted on cancel/logout/timeout).
    auth_server: Mutex<Option<tokio::task::JoinHandle<()>>>,
    /// Discord Rich Presence (opt-in via Settings → Discord).
    discord: DiscordManager,
}

fn web_token_cache_path() -> Option<std::path::PathBuf> {
    paths::app_config_dir().map(|d| d.join("web_token_cache"))
}

fn mut_cache_path() -> Option<std::path::PathBuf> {
    paths::app_config_dir().map(|d| d.join("music_user_token"))
}

/// Write the MUT to disk (0600) so it survives restarts.
fn persist_mut(token: &str) -> Result<(), String> {
    let path = mut_cache_path().ok_or("no config dir for MUT cache")?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    std::fs::write(&path, token).map_err(|e| e.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

/// MUT from memory, falling back to the disk cache (memory is empty after restart).
fn current_mut(state: &AppState) -> Option<String> {
    if let Some(t) = state.tokens.music_user_token() {
        if !t.trim().is_empty() {
            return Some(t);
        }
    }
    mut_cache_path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Catalog storefront for reads: the account's own storefront when a MUT is
/// saved (regional catalogs differ — a hardcoded "us" hides e.g. Italian rap
/// from an Italian account), else the device locale, else "us".
/// The per-play `/v1/me/storefront` probe is cached keyed by MUT: warm plays
/// skip one network round-trip. Overwritten whenever the MUT differs; no TTL.
async fn resolve_storefront(state: &AppState, provider: &ResolvedProvider) -> String {
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
async fn resolve_developer_token(state: &AppState) -> Result<String, String> {
    if let Ok(t) = state.tokens.developer_token() {
        if !t.trim().is_empty() {
            return Ok(t);
        }
    }
    if let Some(cached) = state
        .web_token_cache
        .lock()
        .map_err(|e| e.to_string())?
        .clone()
    {
        return Ok(cached);
    }
    if let Some(path) = web_token_cache_path() {
        if let Ok(raw) = std::fs::read_to_string(&path) {
            let t = raw.trim().to_string();
            if apple_music_core::token::validate_developer_token(&t).is_ok() {
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
        .map_err(|e| format!("auto token fetch failed (Apple may have changed layout): {e}"))?;
    *state.web_token_cache.lock().map_err(|e| e.to_string())? = Some(token.clone());
    if let Some(path) = web_token_cache_path() {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::write(&path, &token);
    }
    Ok(token)
}

#[tauri::command]
async fn token_status(state: State<'_, AppState>) -> Result<String, String> {
    if state
        .tokens
        .developer_token()
        .map(|t| !t.trim().is_empty())
        .unwrap_or(false)
    {
        return Ok("official (env)".into());
    }
    if state
        .web_token_cache
        .lock()
        .map_err(|e| e.to_string())?
        .is_some()
    {
        return Ok("shared web-player (cached)".into());
    }
    if let Some(path) = web_token_cache_path() {
        if std::fs::read_to_string(&path)
            .map(|s| !s.trim().is_empty())
            .unwrap_or(false)
        {
            return Ok("shared web-player (cached)".into());
        }
    }
    Ok("shared web-player (will auto-fetch on authorize)".into())
}

/// One-shot provider holding the resolved dev token + current MUT.
struct ResolvedProvider {
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

#[tauri::command]
async fn search_catalog(
    state: State<'_, AppState>,
    term: String,
) -> Result<apple_music_core::models::SearchResults, String> {
    use apple_music_core::models::{
        merge_search_results, normalize_search_term, rank_search_results, should_rank_results,
    };
    let dev = resolve_developer_token(&state).await?;
    let provider = ResolvedProvider {
        dev,
        mut_token: current_mut(&state),
    };
    let storefront = resolve_storefront(&state, &provider).await;
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
    Ok(out)
}

#[tauri::command]
async fn browse_charts(
    state: State<'_, AppState>,
) -> Result<apple_music_core::models::SearchResults, String> {
    let dev = resolve_developer_token(&state).await?;
    let provider = ResolvedProvider {
        dev,
        mut_token: current_mut(&state),
    };
    let storefront = resolve_storefront(&state, &provider).await;
    let client = ApiClient::new(&provider, &storefront).map_err(|e| e.to_string())?;
    client.charts(12).await.map_err(|e| e.to_string())
}

#[tauri::command]
async fn get_artist(
    state: State<'_, AppState>,
    id: String,
) -> Result<apple_music_core::models::ArtistDetail, String> {
    let dev = resolve_developer_token(&state).await?;
    let provider = ResolvedProvider {
        dev,
        mut_token: current_mut(&state),
    };
    let storefront = resolve_storefront(&state, &provider).await;
    let client = ApiClient::new(&provider, &storefront).map_err(|e| e.to_string())?;
    client.get_artist(&id).await.map_err(|e| e.to_string())
}

#[tauri::command]
async fn add_to_playlist(
    state: State<'_, AppState>,
    playlist_id: String,
    song_ids: Vec<String>,
) -> Result<String, String> {
    let dev = resolve_developer_token(&state).await?;
    let provider = ResolvedProvider {
        dev,
        mut_token: current_mut(&state),
    };
    let storefront = resolve_storefront(&state, &provider).await;
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
#[tauri::command]
async fn resolve_track_id(state: State<'_, AppState>, track_id: String) -> Result<String, String> {
    if !ApiClient::is_library_song_id(&track_id) {
        return Ok(track_id);
    }
    let dev = resolve_developer_token(&state).await?;
    let provider = ResolvedProvider {
        dev,
        mut_token: current_mut(&state),
    };
    let storefront = resolve_storefront(&state, &provider).await;
    let client = ApiClient::new(&provider, &storefront).map_err(|e| e.to_string())?;
    Ok(client
        .catalog_id_for_library_song(&track_id)
        .await
        .unwrap_or(track_id))
}

#[tauri::command]
async fn remove_from_playlist(
    state: State<'_, AppState>,
    playlist_id: String,
    song_ids: Vec<String>,
) -> Result<String, String> {
    let dev = resolve_developer_token(&state).await?;
    let provider = ResolvedProvider {
        dev,
        mut_token: current_mut(&state),
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

#[tauri::command]
async fn create_playlist(state: State<'_, AppState>, name: String) -> Result<String, String> {
    let dev = resolve_developer_token(&state).await?;
    let provider = ResolvedProvider {
        dev,
        mut_token: current_mut(&state),
    };
    let client = ApiClient::new(&provider, "us").map_err(|e| e.to_string())?;
    client
        .create_playlist(&name)
        .await
        .map_err(|e| e.to_string())
}

/// Rename a library playlist (needs MUT). Thin IPC over
/// [`ApiClient::rename_playlist`]; empty names are rejected in core.
#[tauri::command]
async fn rename_playlist(
    state: State<'_, AppState>,
    playlist_id: String,
    name: String,
) -> Result<String, String> {
    let dev = resolve_developer_token(&state).await?;
    let provider = ResolvedProvider {
        dev,
        mut_token: current_mut(&state),
    };
    let storefront = resolve_storefront(&state, &provider).await;
    let client = ApiClient::new(&provider, &storefront).map_err(|e| e.to_string())?;
    client
        .rename_playlist(&playlist_id, &name)
        .await
        .map_err(|e| e.to_string())?;
    Ok(format!("renamed to “{name}”"))
}

/// Delete a library playlist (needs MUT). Thin IPC over
/// [`ApiClient::delete_playlist`].
#[tauri::command]
async fn delete_playlist(
    state: State<'_, AppState>,
    playlist_id: String,
) -> Result<String, String> {
    let dev = resolve_developer_token(&state).await?;
    let provider = ResolvedProvider {
        dev,
        mut_token: current_mut(&state),
    };
    let storefront = resolve_storefront(&state, &provider).await;
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
#[tauri::command]
async fn share_playlist(state: State<'_, AppState>, playlist_id: String) -> Result<String, String> {
    let dev = resolve_developer_token(&state).await?;
    let provider = ResolvedProvider {
        dev,
        mut_token: current_mut(&state),
    };
    let storefront = resolve_storefront(&state, &provider).await;
    let client = ApiClient::new(&provider, &storefront).map_err(|e| e.to_string())?;
    client
        .share_library_playlist(&playlist_id, &storefront)
        .await
        .map_err(|e| e.to_string())
}

/// Make a library playlist private again (needs MUT). Thin IPC over
/// [`ApiClient::set_playlist_public`] with `false`.
#[tauri::command]
async fn unshare_playlist(
    state: State<'_, AppState>,
    playlist_id: String,
) -> Result<String, String> {
    let dev = resolve_developer_token(&state).await?;
    let provider = ResolvedProvider {
        dev,
        mut_token: current_mut(&state),
    };
    let storefront = resolve_storefront(&state, &provider).await;
    let client = ApiClient::new(&provider, &storefront).map_err(|e| e.to_string())?;
    client
        .set_playlist_public(&playlist_id, false)
        .await
        .map_err(|e| e.to_string())?;
    Ok("playlist is private".into())
}

#[tauri::command]
async fn add_to_favorites(
    state: State<'_, AppState>,
    song_ids: Vec<String>,
) -> Result<String, String> {
    let dev = resolve_developer_token(&state).await?;
    let provider = ResolvedProvider {
        dev,
        mut_token: current_mut(&state),
    };
    let storefront = resolve_storefront(&state, &provider).await;
    let client = ApiClient::new(&provider, &storefront).map_err(|e| e.to_string())?;
    let n = client
        .add_to_library(&song_ids)
        .await
        .map_err(|e| e.to_string())?;
    Ok(format!("added {n} track(s) to favorites"))
}

#[tauri::command]
async fn get_album(
    state: State<'_, AppState>,
    id: String,
) -> Result<apple_music_core::models::AlbumDetail, String> {
    let dev = resolve_developer_token(&state).await?;
    let provider = ResolvedProvider {
        dev,
        mut_token: current_mut(&state),
    };
    let storefront = resolve_storefront(&state, &provider).await;
    let client = ApiClient::new(&provider, &storefront).map_err(|e| e.to_string())?;
    client.get_album(&id).await.map_err(|e| e.to_string())
}

/// Animated album cover (Apple Motion) for a song (resolved through its
/// album) or an album id directly. `None` means no motion art — play the
/// static artwork. Errors surface as strings; the UI treats them the same
/// as "no motion" and keeps static art.
#[tauri::command]
async fn motion_artwork(
    state: State<'_, AppState>,
    song_id: Option<String>,
    album_id: Option<String>,
) -> Result<Option<apple_music_core::models::MotionArtwork>, String> {
    let dev = resolve_developer_token(&state).await?;
    let provider = ResolvedProvider {
        dev,
        mut_token: current_mut(&state),
    };
    let storefront = resolve_storefront(&state, &provider).await;
    let client = ApiClient::new(&provider, &storefront).map_err(|e| e.to_string())?;
    if let Some(album) = album_id.filter(|s| !s.trim().is_empty()) {
        return client
            .album_motion_artwork(&album)
            .await
            .map_err(|e| e.to_string());
    }
    if let Some(song) = song_id.filter(|s| !s.trim().is_empty()) {
        return client
            .song_motion_artwork(&song)
            .await
            .map_err(|e| e.to_string());
    }
    Ok(None)
}

#[tauri::command]
async fn get_playlist(
    state: State<'_, AppState>,
    id: String,
) -> Result<apple_music_core::models::PlaylistDetail, String> {
    let dev = resolve_developer_token(&state).await?;
    let provider = ResolvedProvider {
        dev,
        mut_token: current_mut(&state),
    };
    let storefront = resolve_storefront(&state, &provider).await;
    let client = ApiClient::new(&provider, &storefront).map_err(|e| e.to_string())?;
    client.get_playlist(&id).await.map_err(|e| e.to_string())
}

#[tauri::command]
async fn library_playlists(
    state: State<'_, AppState>,
) -> Result<Vec<apple_music_core::models::Playlist>, String> {
    let dev = resolve_developer_token(&state).await?;
    let provider = ResolvedProvider {
        dev,
        mut_token: current_mut(&state),
    };
    let client = ApiClient::new(&provider, "us").map_err(|e| e.to_string())?;
    client.library_playlists().await.map_err(|e| e.to_string())
}

#[tauri::command]
async fn get_lyrics(
    state: State<'_, AppState>,
    song_id: String,
    artist: String,
    title: String,
) -> Result<apple_music_core::models::Lyrics, String> {
    let dev = resolve_developer_token(&state).await?;
    let provider = ResolvedProvider {
        dev,
        mut_token: current_mut(&state),
    };
    let storefront = resolve_storefront(&state, &provider).await;
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

#[tauri::command]
async fn authorize_url(state: State<'_, AppState>) -> Result<String, String> {
    let dev = resolve_developer_token(&state).await?;
    Ok(auth::build_authorize_url(
        &dev,
        "Sonora",
        "tauri://localhost",
    ))
}

#[tauri::command]
async fn open_auth_url(app: tauri::AppHandle, url: String) -> Result<(), String> {
    app.opener()
        .open_url(url, None::<&str>)
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn submit_user_token(state: State<'_, AppState>, token: String) -> Result<(), String> {
    let token = token.trim().to_string();
    state
        .tokens
        .set_music_user_token(token.clone())
        .map_err(|e| e.to_string())?;
    persist_mut(&token)?;
    Ok(())
}

/// Paste the full `authorize.music.apple.com/?...&musicUserToken=...` redirect
/// URL; the app extracts and stores the MUT.
#[tauri::command]
fn submit_auth_url(state: State<'_, AppState>, url: String) -> Result<String, String> {
    let token = auth::extract_user_token_from_url(&url)
        .ok_or_else(|| "no musicUserToken found in that URL".to_string())?;
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
fn abort_auth_flow(state: &AppState) {
    if let Ok(mut slot) = state.auth_server.lock() {
        if let Some(handle) = slot.take() {
            handle.abort();
        }
    }
}

/// Automatic sign-in: opens the system browser on a one-shot localhost
/// page (MusicKit `authorize()`), waits up to 5 minutes for the approval,
/// then stores the MUT like a manual paste. No copy-paste involved.
#[tauri::command]
async fn start_signin(state: State<'_, AppState>, app: tauri::AppHandle) -> Result<String, String> {
    if let Ok(slot) = state.auth_server.lock() {
        if let Some(handle) = slot.as_ref() {
            if !handle.is_finished() {
                return Err("sign-in already in progress — finish or cancel it first".into());
            }
        }
    }
    let dev = resolve_developer_token(&state).await?;
    let (tx, rx) = tokio::sync::oneshot::channel::<String>();
    let (port, handle) = crate::auth_flow::run_signin_server(dev, tx).await?;
    *state.auth_server.lock().map_err(|e| e.to_string())? = Some(handle);
    app.opener()
        .open_url(format!("http://127.0.0.1:{port}/"), None::<&str>)
        .map_err(|e| e.to_string())?;
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
    if token.is_empty() {
        return Err("sign-in returned an empty token".into());
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
#[tauri::command]
fn cancel_signin(state: State<'_, AppState>) -> Result<String, String> {
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
#[tauri::command]
fn auth_state(state: State<'_, AppState>) -> Result<bool, String> {
    Ok(current_mut(&state).is_some())
}

/// Log out: drop the MUT from memory and disk, abort any pending sign-in,
/// and stop playback.
#[tauri::command]
fn logout(state: State<'_, AppState>) -> Result<String, String> {
    abort_auth_flow(&state);
    state
        .tokens
        .set_music_user_token(String::new())
        .map_err(|e| e.to_string())?;
    if let Some(path) = mut_cache_path() {
        let _ = std::fs::remove_file(&path);
    }
    let _ = state.native.stop();
    Ok("Logged out — credentials removed, playback stopped.".into())
}

/// Expand one queue item to song ids: songs pass through, albums/playlists
/// expand via the catalog. Capped so a huge playlist can't stall playback.
async fn expand_queue_item(
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
async fn resolve_song_id(client: &ApiClient<'_>, id: &str) -> String {
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

/// Play one queued song through the native engine: catalog resolve →
/// metadata → decrypt → decode → publish. The queue cursor is owned by the
/// caller (`player_play` sets it, `step` moves it). A dead library id is
/// retried once via metadata search (stale ids after library re-sync).
async fn native_play_item(
    state: &AppState,
    provider: &ResolvedProvider,
    item: &QueueItem,
) -> Result<String, String> {
    let storefront = resolve_storefront(state, provider).await;
    let client = ApiClient::new(provider, &storefront).map_err(|e| e.to_string())?;
    let catalog_id = resolve_song_id(&client, &item.id).await;
    match play_catalog_id(state, provider, &client, &catalog_id).await {
        Ok(title) => Ok(title),
        Err(e) if is_unresolvable_play_error(&e) => {
            retry_by_metadata(state, provider, &client, item, &e).await
        }
        Err(e) => Err(e),
    }
}

/// Play an already-resolved catalog id: metadata → decrypt → publish.
async fn play_catalog_id(
    state: &AppState,
    provider: &ResolvedProvider,
    client: &ApiClient<'_>,
    catalog_id: &str,
) -> Result<String, String> {
    let meta = client
        .get_song(catalog_id)
        .await
        .map(|v| native_player::meta_from_song(&v))
        .unwrap_or_default();
    let mut_ = provider
        .music_user_token()
        .ok_or_else(|| "no MUT saved — sign in first".to_string())?;
    // Progressive playback handles its own failure reset internally.
    state
        .native
        .play_progressive(catalog_id.to_string(), meta.clone(), mut_)
        .await?;
    prefetch_ids(state, state.native.upcoming(2)).await;
    Ok(meta.title.clone().unwrap_or_else(|| catalog_id.to_string()))
}

/// True for play failures worth a metadata retry: the id (not the session)
/// is the problem — unknown mapping, empty store answer, gone track.
fn is_unresolvable_play_error(e: &str) -> bool {
    let lower = e.to_lowercase();
    [
        "itemnotfound",
        "no longer available",
        "track unavailable",
        "no songlist",
        "empty songlist",
        "no mapping",
        "404",
    ]
    .iter()
    .any(|m| lower.contains(m))
}

/// One-shot retry: find the song by title/artist and play the fresh
/// catalog id. Metadata comes from the queue item, else from the library
/// resource itself. Never recurses — a second failure propagates as-is.
async fn retry_by_metadata(
    state: &AppState,
    provider: &ResolvedProvider,
    client: &ApiClient<'_>,
    item: &QueueItem,
    original: &str,
) -> Result<String, String> {
    let (title, artist) = match (&item.title, &item.artist) {
        (Some(t), Some(a)) => (t.clone(), a.clone()),
        _ => client
            .library_song_attrs(&item.id)
            .await
            .map_err(|_| original.to_string())?,
    };
    if title.trim().is_empty() || artist.trim().is_empty() {
        return Err(original.to_string());
    }
    let results = client
        .search(&format!("{title} {artist}"), 10)
        .await
        .map_err(|_| original.to_string())?;
    let hit = results
        .tracks
        .iter()
        .find(|t| apple_music_core::models::track_matches(&title, &artist, t))
        .ok_or_else(|| original.to_string())?;
    if hit.id == item.id {
        return Err(original.to_string());
    }
    eprintln!(
        "sonora native: stale id {} → catalog match {} ({title} — {artist})",
        item.id, hit.id
    );
    play_catalog_id(state, provider, client, &hit.id).await
}

/// Native `PlayNow`: expand album/playlist items, store the queue, play the target.
async fn native_play(
    state: &AppState,
    provider: ResolvedProvider,
    items: Vec<QueueItem>,
    start_index: u32,
) -> Result<String, String> {
    let storefront = resolve_storefront(state, &provider).await;
    let client = ApiClient::new(&provider, &storefront).map_err(|e| e.to_string())?;
    let mut songs: Vec<QueueItem> = Vec::new();
    for item in &items {
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
    state.native.set_queue(songs.clone(), start)?;
    eprintln!(
        "sonora native: queue={} start={} target={}",
        songs.len(),
        start,
        songs[start].id
    );
    let title = native_play_item(state, &provider, &songs[start]).await?;
    Ok(format!("playing (native): {title}"))
}

async fn native_step(
    state: &AppState,
    provider: ResolvedProvider,
    delta: isize,
) -> Result<(), String> {
    match state.native.step(delta)? {
        Some(item) => {
            native_play_item(state, &provider, &item).await?;
            Ok(())
        }
        None => Err("queue is empty".into()),
    }
}

fn native_provider(state: &AppState, dev: String) -> ResolvedProvider {
    ResolvedProvider {
        dev,
        mut_token: current_mut(state),
    }
}

/// Fire-and-forget prefetch of upcoming ids into the decoded cache.
async fn prefetch_ids(state: &AppState, items: Vec<QueueItem>) {
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

#[tauri::command]
async fn player_play(
    state: State<'_, AppState>,
    items: Vec<QueueItem>,
    start_index: Option<u32>,
) -> Result<String, String> {
    let dev = resolve_developer_token(&state).await?;
    if current_mut(&state).is_none() {
        return Err("no MUT saved — sign in first".into());
    }
    let provider = native_provider(&state, dev);
    native_play(&state, provider, items, start_index.unwrap_or(0)).await
}
/// Warm the player at boot: prefetch the bearer token + CDM in the
/// background (first play then skips the ~20MB download + scrape).
#[tauri::command]
async fn player_warmup(_state: State<'_, AppState>) -> Result<u16, String> {
    tauri::async_runtime::spawn(async {
        let _ = am_playback::bearer::get_bearer_token().await;
        let _ = am_playback::widevine::fetch::ensure().await;
    });
    Ok(0)
}

/// Resume without touching the queue (pause → play path).
#[tauri::command]
async fn player_resume(state: State<'_, AppState>) -> Result<(), String> {
    // No current track yet: resume is a no-op rather than an error so a
    // stray play press never fails the UI.
    state.native.resume()
}

#[tauri::command]
async fn player_pause(state: State<'_, AppState>) -> Result<(), String> {
    state.native.pause()
}

#[tauri::command]
async fn player_next(state: State<'_, AppState>) -> Result<(), String> {
    let dev = resolve_developer_token(&state).await?;
    let provider = native_provider(&state, dev);
    native_step(&state, provider, 1).await
}

#[tauri::command]
async fn player_previous(state: State<'_, AppState>) -> Result<(), String> {
    let dev = resolve_developer_token(&state).await?;
    let provider = native_provider(&state, dev);
    native_step(&state, provider, -1).await
}

#[tauri::command]
async fn player_seek(state: State<'_, AppState>, position_ms: u64) -> Result<(), String> {
    state.native.seek(position_ms).await
}

#[tauri::command]
async fn player_status(state: State<'_, AppState>) -> Result<PlayerReport, String> {
    Ok(state.native.status())
}

#[tauri::command]
fn player_stop(state: State<'_, AppState>) -> Result<(), String> {
    state.native.stop()
}

#[tauri::command]
async fn player_volume(state: State<'_, AppState>, level: f32) -> Result<(), String> {
    state.native.set_volume(level.clamp(0.0, 1.0))
}

#[tauri::command]
async fn player_append(state: State<'_, AppState>, items: Vec<QueueItem>) -> Result<(), String> {
    let pre = items.clone();
    state.native.queue_append(items)?;
    prefetch_ids(&state, pre).await;
    Ok(())
}

#[tauri::command]
async fn player_play_next(state: State<'_, AppState>, items: Vec<QueueItem>) -> Result<(), String> {
    let pre = items.clone();
    state.native.queue_next(items)?;
    prefetch_ids(&state, pre).await;
    Ok(())
}

#[tauri::command]
async fn player_clear(state: State<'_, AppState>) -> Result<(), String> {
    state.native.queue_clear()?;
    state.native.stop()
}

#[tauri::command]
async fn playlist_recommendations(
    state: State<'_, AppState>,
    seed_ids: Vec<String>,
    exclude_ids: Option<Vec<String>>,
    limit: Option<u8>,
    page: Option<u32>,
) -> Result<Vec<apple_music_core::models::Track>, String> {
    let dev = resolve_developer_token(&state).await?;
    let provider = ResolvedProvider {
        dev,
        mut_token: current_mut(&state),
    };
    let storefront = resolve_storefront(&state, &provider).await;
    let client = ApiClient::new(&provider, &storefront).map_err(|e| e.to_string())?;
    let exclude: std::collections::HashSet<String> =
        exclude_ids.unwrap_or_default().into_iter().collect();
    client
        .playlist_recommendations(
            &seed_ids,
            limit.unwrap_or(10).clamp(1, 25),
            &exclude,
            page.unwrap_or(0).min(8),
        )
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn similar_songs(
    state: State<'_, AppState>,
    song_id: String,
    exclude_ids: Option<Vec<String>>,
    depth: Option<u32>,
) -> Result<Vec<apple_music_core::models::Track>, String> {
    let dev = resolve_developer_token(&state).await?;
    let provider = ResolvedProvider {
        dev,
        mut_token: current_mut(&state),
    };
    let storefront = resolve_storefront(&state, &provider).await;
    let client = ApiClient::new(&provider, &storefront).map_err(|e| e.to_string())?;
    let exclude: std::collections::HashSet<String> =
        exclude_ids.unwrap_or_default().into_iter().collect();
    client
        .similar_songs(&song_id, 25, &exclude, depth.unwrap_or(0).min(8))
        .await
        .map_err(|e| e.to_string())
}

/// Track-change desktop notifications (Settings → Notifications, opt-in,
/// default off). Checked by the MPRIS bridge before firing a toast.
#[tauri::command]
fn set_player_notifications(state: State<'_, AppState>, enabled: bool) -> Result<String, String> {
    state.native.set_notifications(enabled)?;
    Ok(if enabled {
        "notifications on".into()
    } else {
        "notifications off".into()
    })
}

#[tauri::command]
fn player_notifications(state: State<'_, AppState>) -> Result<bool, String> {
    Ok(state.native.notifications_enabled())
}

// ---- Discord Rich Presence (opt-in) ----

#[tauri::command]
fn set_discord_enabled(state: State<'_, AppState>, enabled: bool) -> Result<String, String> {
    state.discord.set_enabled(enabled);
    Ok(if enabled {
        "discord status on".into()
    } else {
        "discord status off".into()
    })
}

#[tauri::command]
fn set_discord_app_id(state: State<'_, AppState>, app_id: String) -> Result<String, String> {
    state.discord.set_app_id(app_id);
    Ok("discord app id saved".into())
}

#[tauri::command]
fn update_discord_presence(
    state: State<'_, AppState>,
    payload: PresencePayload,
) -> Result<(), String> {
    state.discord.update(&payload);
    Ok(())
}

#[tauri::command]
fn clear_discord_presence(state: State<'_, AppState>) -> Result<(), String> {
    state.discord.clear();
    Ok(())
}

/// Linux/WebKitGTK render defaults (evaluated before GTK init, first call in
/// `main`). Two failure modes observed, both NVIDIA/GBM related:
/// - NVIDIA present but `nvidia-drm.modeset` off: DMA-BUF fails (white
///   viewport, `Failed to create GBM buffer`) and can crash Wayland clients.
///   Fall back to software rendering — but only when no AMD card drives the
///   display (see below).
/// - AMD + NVIDIA hybrid with the display on AMD: EGL may pick NVIDIA and hit
///   the same GBM failure. Prefer Mesa EGL so WebKit renders on the display
///   GPU. Healthy systems (and any explicit user env) are left untouched.
#[cfg(target_os = "linux")]
fn apply_linux_webview_env_defaults() {
    let dmabuf_user = std::env::var_os("WEBKIT_DISABLE_DMABUF_RENDERER").is_some()
        || std::env::var_os("WEBKIT_DMABUF_RENDERER").is_some();
    let egl_user = std::env::var_os("__EGL_VENDOR_LIBRARY_FILENAMES").is_some();
    let cards = drm_cards();
    let amd_display = cards
        .iter()
        .any(|c| c.vendor == PCI_VENDOR_AMD && (c.boot_vga || c.connected));
    let nvidia_present = cards.iter().any(|c| c.vendor == PCI_VENDOR_NVIDIA)
        || std::path::Path::new("/proc/driver/nvidia/version").exists();
    let nvidia_modeset = nvidia_drm_modeset_on();
    if !egl_user && amd_display && nvidia_present && !nvidia_modeset {
        if let Some(mesa) = mesa_egl_vendor_file() {
            std::env::set_var("__EGL_VENDOR_LIBRARY_FILENAMES", mesa);
        }
    }
    if !dmabuf_user && nvidia_present && !nvidia_modeset && !amd_display {
        std::env::set_var("WEBKIT_DISABLE_DMABUF_RENDERER", "1");
    }
}

#[cfg(target_os = "linux")]
const PCI_VENDOR_AMD: &str = "0x1002";
#[cfg(target_os = "linux")]
const PCI_VENDOR_NVIDIA: &str = "0x10de";

/// DRM render devices present, with vendor + display role, from sysfs.
#[cfg(target_os = "linux")]
struct DrmCard {
    vendor: String,
    boot_vga: bool,
    connected: bool,
}

#[cfg(target_os = "linux")]
fn drm_cards() -> Vec<DrmCard> {
    let mut out = Vec::new();
    let Ok(dir) = std::fs::read_dir("/sys/class/drm") else {
        return out;
    };
    for entry in dir.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.starts_with("card") || name["card".len()..].contains('-') {
            continue; // skip cardN-connector status entries
        }
        let base = entry.path();
        let read = |p: &str| {
            std::fs::read_to_string(base.join(p))
                .map(|s| s.trim().to_ascii_lowercase())
                .unwrap_or_default()
        };
        let vendor = read("device/vendor");
        let boot_vga = read("device/boot_vga") == "1";
        let mut connected = false;
        if let Ok(outputs) = std::fs::read_dir(&base) {
            for o in outputs.flatten() {
                let oname = o.file_name().to_string_lossy().into_owned();
                if !oname.contains('-') || oname.contains("VIRTUAL") {
                    continue;
                }
                if let Ok(st) = std::fs::read_to_string(o.path().join("status")) {
                    if st.trim() == "connected" {
                        connected = true;
                        break;
                    }
                }
            }
        }
        out.push(DrmCard {
            vendor,
            boot_vga,
            connected,
        });
    }
    out
}

/// True when `nvidia-drm.modeset` is on (GBM/DMA-BUF viable on NVIDIA).
#[cfg(target_os = "linux")]
fn nvidia_drm_modeset_on() -> bool {
    std::fs::read_to_string("/sys/module/nvidia_drm/parameters/modeset")
        .map(|s| s.trim().eq_ignore_ascii_case("y"))
        .unwrap_or(false)
}

/// First existing Mesa EGL vendor file (GLVND layouts differ by distro).
#[cfg(target_os = "linux")]
fn mesa_egl_vendor_file() -> Option<std::path::PathBuf> {
    [
        "/usr/share/glvnd/egl_vendor.d/50_mesa.json",
        "/usr/share/egl/egl_vendor.d/50_mesa.json",
    ]
    .into_iter()
    .map(std::path::PathBuf::from)
    .find(|p| p.is_file())
}

fn main() {
    #[cfg(target_os = "linux")]
    apply_linux_webview_env_defaults();
    let tokens = EnvTokenProvider::new("APPLE_MUSIC_DEVELOPER_TOKEN");
    // Preload persisted MUT so restarts don't wipe the login.
    if let Some(path) = mut_cache_path() {
        if let Ok(raw) = std::fs::read_to_string(&path) {
            let t = raw.trim().to_string();
            if !t.is_empty() {
                let _ = tokens.set_music_user_token(t);
            }
        }
    }
    let native = NativePlayer::new();
    tauri::Builder::default()
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_opener::init())
        .manage(AppState {
            tokens,
            web_token_cache: Mutex::new(None),
            storefront_cache: Mutex::new(None),
            native,
            auth_server: Mutex::new(None),
            discord: DiscordManager::new(),
        })
        .setup(|app| {
            if let Ok(dir) = app.path().app_config_dir() {
                paths::init_config_dir(dir);
            }
            let session_config = {
                #[cfg(windows)]
                {
                    let mut session_config = media_session::MediaSessionConfig::new();
                    if let Some(window) = app
                        .get_webview_window("main")
                        .or_else(|| app.webview_windows().values().next().cloned())
                    {
                        session_config.hwnd = window.hwnd().ok().map(|h| h.0 as isize);
                    }
                    session_config
                }
                #[cfg(not(windows))]
                {
                    media_session::MediaSessionConfig::new()
                }
            };
            let player = app.state::<AppState>().native.clone();
            tauri::async_runtime::spawn(async move {
                media_session::run(player, session_config).await;
            });
            // Hub refresh loop, spawned ONCE here (runtime context guaranteed).
            // Transport methods stay spawn-free so OS bridge threads — which
            // have no Tokio reactor — can call them without panicking.
            let publisher = app.state::<AppState>().native.clone();
            tauri::async_runtime::spawn(async move {
                loop {
                    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                    publisher.publish_now();
                }
            });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            search_catalog,
            browse_charts,
            get_artist,
            get_album,
            motion_artwork,
            get_playlist,
            library_playlists,
            add_to_playlist,
            remove_from_playlist,
            resolve_track_id,
            add_to_favorites,
            create_playlist,
            rename_playlist,
            delete_playlist,
            share_playlist,
            unshare_playlist,
            get_lyrics,
            authorize_url,
            token_status,
            open_auth_url,
            submit_user_token,
            submit_auth_url,
            start_signin,
            cancel_signin,
            auth_state,
            logout,
            player_play,
            player_resume,
            player_pause,
            player_next,
            player_previous,
            player_seek,
            player_status,
            player_stop,
            player_volume,
            player_append,
            player_play_next,
            player_clear,
            similar_songs,
            playlist_recommendations,
            set_player_notifications,
            player_notifications,
            player_warmup,
            set_discord_enabled,
            set_discord_app_id,
            update_discord_presence,
            clear_discord_presence
        ])
        .run(tauri::generate_context!())
        .expect("failed to run tauri app");
}
