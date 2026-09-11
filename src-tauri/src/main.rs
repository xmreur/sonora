//! Tauri shell: thin IPC layer over `apple-music-core`.
//! Audio itself plays in the hidden minimal MusicKit page (ui/player.html)
//! rendered by the selected engine (Gecko default, Chromium fallback).

use apple_music_core::{
    api::ApiClient,
    auth,
    playback::*,
    token::{EnvTokenProvider, TokenProvider},
};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use tauri::{Manager, State};
use tauri_plugin_opener::OpenerExt;

mod sidecar;
use sidecar::{PlayerReport, SidecarManager};

mod discord;
use discord::{DiscordManager, PresencePayload};

struct AppState {
    tokens: EnvTokenProvider,
    /// Cache for the auto-fetched web-player token (subscription-only path).
    web_token_cache: Mutex<Option<String>>,
    engine_kind: Mutex<EngineKind>,
    /// Firefox sidecar for full-track (DRM) playback.
    sidecar: SidecarManager,
    /// Discord Rich Presence (opt-in via Settings → Discord).
    discord: DiscordManager,
}

pub(crate) fn app_config_dir() -> Option<PathBuf> {
    let home = std::env::var("HOME").ok()?;
    let new = PathBuf::from(&home).join(".config/sonora");
    let old = PathBuf::from(&home).join(".config/apple-music-linux");
    migrate_legacy_config(&old, &new);
    Some(new)
}

/// One-shot move from the pre-Sonora config dir. Existing MUT, token cache,
/// and Firefox profile come along so a rename doesn't log you out.
fn migrate_legacy_config(old: &Path, new: &Path) {
    if !old.exists() {
        return;
    }
    // Sidecar Firefox holding the old profile would block a rename.
    let old_profile = old.join("firefox-profile");
    let needle = old_profile.to_string_lossy();
    if !needle.is_empty() {
        let _ = std::process::Command::new("pkill")
            .args(["-9", "-f", needle.as_ref()])
            .status();
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    if !new.exists() {
        if std::fs::rename(old, new).is_ok() {
            return;
        }
        if copy_dir_all(old, new).is_ok() {
            let _ = std::fs::remove_dir_all(old);
        }
        return;
    }
    let _ = copy_dir_all(old, new);
    let _ = std::fs::remove_dir_all(old);
}

fn copy_dir_all(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let to = dst.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir_all(&entry.path(), &to)?;
        } else if !to.exists() {
            std::fs::copy(entry.path(), &to)?;
        }
    }
    Ok(())
}

fn web_token_cache_path() -> Option<std::path::PathBuf> {
    app_config_dir().map(|d| d.join("web_token_cache"))
}

fn mut_cache_path() -> Option<std::path::PathBuf> {
    app_config_dir().map(|d| d.join("music_user_token"))
}

/// Write the MUT to disk (0600) so it survives restarts.
fn persist_mut(token: &str) -> Result<(), String> {
    let path = mut_cache_path().ok_or("no HOME for MUT cache")?;
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
async fn resolve_storefront(provider: &ResolvedProvider) -> String {
    if provider.music_user_token().is_some() {
        if let Ok(probe) = ApiClient::new(provider, "us") {
            if let Ok(sf) = probe.user_storefront().await {
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
    let dev = resolve_developer_token(&state).await?;
    let provider = ResolvedProvider {
        dev,
        mut_token: current_mut(&state),
    };
    let storefront = resolve_storefront(&provider).await;
    let client = ApiClient::new(&provider, &storefront).map_err(|e| e.to_string())?;
    client.search(&term, 25).await.map_err(|e| e.to_string())
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
    let storefront = resolve_storefront(&provider).await;
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
    let storefront = resolve_storefront(&provider).await;
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
    let storefront = resolve_storefront(&provider).await;
    let client = ApiClient::new(&provider, &storefront).map_err(|e| e.to_string())?;
    let n = client
        .add_to_playlist(&playlist_id, &song_ids)
        .await
        .map_err(|e| e.to_string())?;
    Ok(format!("added {n} track(s)"))
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
    let storefront = resolve_storefront(&provider).await;
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
    let storefront = resolve_storefront(&provider).await;
    let client = ApiClient::new(&provider, &storefront).map_err(|e| e.to_string())?;
    client.get_album(&id).await.map_err(|e| e.to_string())
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
    let storefront = resolve_storefront(&provider).await;
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
    let storefront = resolve_storefront(&provider).await;
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

#[tauri::command]
fn set_engine(state: State<'_, AppState>, engine: String) -> Result<String, String> {
    let kind: EngineKind = engine
        .parse()
        .map_err(|e: apple_music_core::CoreError| e.to_string())?;
    let cfg = SidecarConfig::for_engine(kind, "tauri://localhost/player.html");
    // WebKit is allowed for browsing metadata but warn for playback.
    let note = cfg
        .check_supported()
        .err()
        .map(|e| e.to_string())
        .unwrap_or_default();
    *state.engine_kind.lock().map_err(|e| e.to_string())? = kind;
    Ok(if note.is_empty() {
        "ok".into()
    } else {
        format!("selected with warning: {note}")
    })
}

#[tauri::command]
fn playback_command(state: State<'_, AppState>, cmd: PlaybackCommand) -> Result<(), String> {
    let kind = *state.engine_kind.lock().map_err(|e| e.to_string())?;
    let engine = NoopEngine::new(kind); // replaced by real sidecar sender in Phase 3
    engine.send(&cmd).map_err(|e| e.to_string())
}

// ---- Full-track Firefox sidecar ----

#[tauri::command]
async fn sidecar_play(
    state: State<'_, AppState>,
    items: Vec<QueueItem>,
    start_index: Option<u32>,
) -> Result<String, String> {
    let dev = resolve_developer_token(&state).await?;
    let mut_ = current_mut(&state);
    if mut_.is_none() {
        return Err("no MUT saved — paste the authorize redirect URL into 2b first".into());
    }
    let port = state.sidecar.ensure_running(dev, mut_).await?;
    // One atomic command: separate SetQueue+Play race across poll ticks and
    // leave the new item queued-but-paused.
    state.sidecar.enqueue(PlaybackCommand::PlayNow {
        items,
        start_index: start_index.unwrap_or(0),
    })?;
    Ok(format!(
        "sent to Firefox sidecar (port {port}); approve once in its window if asked"
    ))
}

/// Resume without touching the queue (pause → play path).
#[tauri::command]
async fn sidecar_resume(state: State<'_, AppState>) -> Result<(), String> {
    state.sidecar.enqueue(PlaybackCommand::Play)
}

#[tauri::command]
async fn sidecar_pause(state: State<'_, AppState>) -> Result<(), String> {
    state.sidecar.enqueue(PlaybackCommand::Pause)
}

#[tauri::command]
async fn sidecar_next(state: State<'_, AppState>) -> Result<(), String> {
    state.sidecar.enqueue(PlaybackCommand::Next)
}

#[tauri::command]
async fn sidecar_previous(state: State<'_, AppState>) -> Result<(), String> {
    state.sidecar.enqueue(PlaybackCommand::Previous)
}

#[tauri::command]
async fn sidecar_seek(state: State<'_, AppState>, position_ms: u64) -> Result<(), String> {
    state.sidecar.enqueue(PlaybackCommand::Seek { position_ms })
}

#[tauri::command]
async fn sidecar_status(state: State<'_, AppState>) -> Result<PlayerReport, String> {
    state.sidecar.status()
}

#[tauri::command]
fn sidecar_stop(state: State<'_, AppState>) -> Result<(), String> {
    state.sidecar.stop()
}

#[tauri::command]
async fn sidecar_volume(state: State<'_, AppState>, level: f32) -> Result<(), String> {
    let level = level.clamp(0.0, 1.0);
    state.sidecar.enqueue(PlaybackCommand::SetVolume { level })
}

#[tauri::command]
async fn sidecar_append(state: State<'_, AppState>, items: Vec<QueueItem>) -> Result<(), String> {
    state.sidecar.enqueue(PlaybackCommand::Append { items })
}

#[tauri::command]
async fn sidecar_play_next(
    state: State<'_, AppState>,
    items: Vec<QueueItem>,
) -> Result<(), String> {
    state.sidecar.enqueue(PlaybackCommand::PlayNext { items })
}

#[tauri::command]
async fn sidecar_clear(state: State<'_, AppState>) -> Result<(), String> {
    state.sidecar.enqueue(PlaybackCommand::Clear)
}

#[tauri::command]
async fn similar_songs(
    state: State<'_, AppState>,
    song_id: String,
) -> Result<Vec<apple_music_core::models::Track>, String> {
    let dev = resolve_developer_token(&state).await?;
    let provider = ResolvedProvider {
        dev,
        mut_token: current_mut(&state),
    };
    let storefront = resolve_storefront(&provider).await;
    let client = ApiClient::new(&provider, &storefront).map_err(|e| e.to_string())?;
    client
        .similar_songs(&song_id, 15)
        .await
        .map_err(|e| e.to_string())
}

/// Show/hide the Firefox window. Takes effect on next sidecar launch —
/// call `sidecar_relaunch` (or stop + play) to apply immediately.
#[tauri::command]
fn set_sidecar_headless(state: State<'_, AppState>, headless: bool) -> Result<String, String> {
    state.sidecar.set_headless(headless)?;
    Ok(if headless {
        "headless on (applies on relaunch)".into()
    } else {
        "windowed (applies on relaunch)".into()
    })
}

#[tauri::command]
fn sidecar_headless(state: State<'_, AppState>) -> Result<bool, String> {
    Ok(state.sidecar.is_headless())
}

/// Kill the sidecar so the next Play relaunches it (new port, fresh page).
#[tauri::command]
fn sidecar_relaunch(state: State<'_, AppState>) -> Result<(), String> {
    state.sidecar.relaunch()
}

/// Allow/block explicit content in the sidecar (applies on relaunch).
#[tauri::command]
fn set_sidecar_explicit(state: State<'_, AppState>, explicit: bool) -> Result<String, String> {
    state.sidecar.set_explicit(explicit)?;
    Ok(if explicit {
        "explicit allowed (applies on relaunch)".into()
    } else {
        "explicit blocked (applies on relaunch)".into()
    })
}

#[tauri::command]
fn sidecar_explicit(state: State<'_, AppState>) -> Result<bool, String> {
    Ok(state.sidecar.is_explicit())
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

fn main() {
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
    tauri::Builder::default()
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_opener::init())
        .manage(AppState {
            tokens,
            web_token_cache: Mutex::new(None),
            engine_kind: Mutex::new(EngineKind::Gecko),
            sidecar: SidecarManager::new(),
            discord: DiscordManager::new(),
        })
        // The sidecar Firefox is ours: take it down with the app window so it
        // never lingers as an orphan (kill_on_drop covers the rest).
        .on_window_event(|window, event| {
            if matches!(event, tauri::WindowEvent::CloseRequested { .. }) {
                if let Some(state) = window.try_state::<AppState>() {
                    let _ = state.sidecar.stop();
                }
            }
        })
        .invoke_handler(tauri::generate_handler![
            search_catalog,
            browse_charts,
            get_artist,
            get_album,
            get_playlist,
            library_playlists,
            add_to_playlist,
            remove_from_playlist,
            add_to_favorites,
            create_playlist,
            get_lyrics,
            authorize_url,
            token_status,
            open_auth_url,
            submit_user_token,
            submit_auth_url,
            set_engine,
            playback_command,
            sidecar_play,
            sidecar_resume,
            sidecar_pause,
            sidecar_next,
            sidecar_previous,
            sidecar_seek,
            sidecar_status,
            sidecar_stop,
            sidecar_volume,
            sidecar_append,
            sidecar_play_next,
            sidecar_clear,
            similar_songs,
            set_sidecar_headless,
            sidecar_headless,
            set_sidecar_explicit,
            sidecar_explicit,
            sidecar_relaunch,
            set_discord_enabled,
            set_discord_app_id,
            update_discord_presence,
            clear_discord_presence
        ])
        .run(tauri::generate_context!())
        .expect("failed to run tauri app");
}
