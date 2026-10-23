//! Tauri shell: thin IPC layer over the shared `shell` service crate.
//! Audio plays in-process (webPlayback resolve → Widevine license → CENC
//! decrypt → local output); the webview is UI only. All real logic —
//! state, commands, playback engine, auth, media session — lives in
//! `shell`; each command below is a one-line forwarding wrapper.

use apple_music_core::{
    models::{
        AlbumDetail, ArtistDetail, Lyrics, MotionArtwork, Playlist, PlaylistDetail, SearchResults,
        Track,
    },
    playback::QueueItem,
};
use shell::{
    commands::{self, PlayResult},
    discord::PresencePayload,
    native_player::PlayerReport,
    paths, AppState,
};
use tauri::{Manager, State};

// ---- command wrappers ----

#[tauri::command]
async fn search_catalog(state: State<'_, AppState>, term: String) -> Result<SearchResults, String> {
    commands::search_catalog(&state, term).await
}

#[tauri::command]
async fn browse_charts(state: State<'_, AppState>) -> Result<SearchResults, String> {
    commands::browse_charts(&state).await
}

#[tauri::command]
async fn get_artist(state: State<'_, AppState>, id: String) -> Result<ArtistDetail, String> {
    commands::get_artist(&state, id).await
}

#[tauri::command]
async fn get_album(state: State<'_, AppState>, id: String) -> Result<AlbumDetail, String> {
    commands::get_album(&state, id).await
}

#[tauri::command]
async fn motion_artwork(
    state: State<'_, AppState>,
    song_id: Option<String>,
    album_id: Option<String>,
) -> Result<Option<MotionArtwork>, String> {
    commands::motion_artwork(&state, song_id, album_id).await
}

#[tauri::command]
async fn get_playlist(state: State<'_, AppState>, id: String) -> Result<PlaylistDetail, String> {
    commands::get_playlist(&state, id).await
}

#[tauri::command]
async fn library_playlists(state: State<'_, AppState>) -> Result<Vec<Playlist>, String> {
    commands::library_playlists(&state).await
}

#[tauri::command]
async fn add_to_playlist(
    state: State<'_, AppState>,
    playlist_id: String,
    song_ids: Vec<String>,
) -> Result<String, String> {
    commands::add_to_playlist(&state, playlist_id, song_ids).await
}

#[tauri::command]
async fn remove_from_playlist(
    state: State<'_, AppState>,
    playlist_id: String,
    song_ids: Vec<String>,
) -> Result<String, String> {
    commands::remove_from_playlist(&state, playlist_id, song_ids).await
}

#[tauri::command]
async fn resolve_track_id(state: State<'_, AppState>, track_id: String) -> Result<String, String> {
    commands::resolve_track_id(&state, track_id).await
}

#[tauri::command]
async fn add_to_favorites(
    state: State<'_, AppState>,
    song_ids: Vec<String>,
) -> Result<String, String> {
    commands::add_to_favorites(&state, song_ids).await
}

#[tauri::command]
async fn create_playlist(state: State<'_, AppState>, name: String) -> Result<String, String> {
    commands::create_playlist(&state, name).await
}

#[tauri::command]
async fn rename_playlist(
    state: State<'_, AppState>,
    playlist_id: String,
    name: String,
) -> Result<String, String> {
    commands::rename_playlist(&state, playlist_id, name).await
}

#[tauri::command]
async fn delete_playlist(
    state: State<'_, AppState>,
    playlist_id: String,
) -> Result<String, String> {
    commands::delete_playlist(&state, playlist_id).await
}

#[tauri::command]
async fn share_playlist(state: State<'_, AppState>, playlist_id: String) -> Result<String, String> {
    commands::share_playlist(&state, playlist_id).await
}

#[tauri::command]
async fn unshare_playlist(
    state: State<'_, AppState>,
    playlist_id: String,
) -> Result<String, String> {
    commands::unshare_playlist(&state, playlist_id).await
}

#[tauri::command]
async fn get_lyrics(
    state: State<'_, AppState>,
    song_id: String,
    artist: String,
    title: String,
) -> Result<Lyrics, String> {
    commands::get_lyrics(&state, song_id, artist, title).await
}

#[tauri::command]
async fn authorize_url(state: State<'_, AppState>) -> Result<String, String> {
    commands::authorize_url(&state).await
}

#[tauri::command]
async fn token_status(state: State<'_, AppState>) -> Result<String, String> {
    commands::token_status(&state).await
}

#[tauri::command]
fn open_auth_url(url: String) -> Result<(), String> {
    commands::open_auth_url(url)
}

#[tauri::command]
fn submit_user_token(state: State<'_, AppState>, token: String) -> Result<(), String> {
    commands::submit_user_token(&state, token)
}

#[tauri::command]
fn submit_auth_url(state: State<'_, AppState>, url: String) -> Result<String, String> {
    commands::submit_auth_url(&state, url)
}

#[tauri::command]
async fn start_signin(state: State<'_, AppState>) -> Result<String, String> {
    commands::start_signin(&state).await
}

#[tauri::command]
fn cancel_signin(state: State<'_, AppState>) -> Result<String, String> {
    commands::cancel_signin(&state)
}

#[tauri::command]
fn auth_state(state: State<'_, AppState>) -> Result<bool, String> {
    commands::auth_state(&state)
}

#[tauri::command]
fn logout(state: State<'_, AppState>) -> Result<String, String> {
    commands::logout(&state)
}

#[tauri::command]
async fn player_play(
    state: State<'_, AppState>,
    items: Vec<QueueItem>,
    start_index: Option<u32>,
) -> Result<PlayResult, String> {
    commands::player_play(&state, items, start_index).await
}

#[tauri::command]
async fn player_warmup(state: State<'_, AppState>) -> Result<u16, String> {
    commands::player_warmup(&state).await
}

#[tauri::command]
async fn player_resume(state: State<'_, AppState>) -> Result<(), String> {
    commands::player_resume(&state).await
}

#[tauri::command]
async fn player_pause(state: State<'_, AppState>) -> Result<(), String> {
    commands::player_pause(&state).await
}

#[tauri::command]
async fn player_next(state: State<'_, AppState>) -> Result<(), String> {
    commands::player_next(&state).await
}

#[tauri::command]
async fn player_previous(state: State<'_, AppState>) -> Result<(), String> {
    commands::player_previous(&state).await
}

#[tauri::command]
async fn player_seek(state: State<'_, AppState>, position_ms: u64) -> Result<(), String> {
    commands::player_seek(&state, position_ms).await
}

#[tauri::command]
async fn player_status(state: State<'_, AppState>) -> Result<PlayerReport, String> {
    commands::player_status(&state).await
}

#[tauri::command]
fn player_stop(state: State<'_, AppState>) -> Result<(), String> {
    commands::player_stop(&state)
}

#[tauri::command]
async fn player_volume(state: State<'_, AppState>, level: f32) -> Result<(), String> {
    commands::player_volume(&state, level).await
}

#[tauri::command]
async fn player_append(state: State<'_, AppState>, items: Vec<QueueItem>) -> Result<(), String> {
    commands::player_append(&state, items).await
}

#[tauri::command]
async fn player_play_next(state: State<'_, AppState>, items: Vec<QueueItem>) -> Result<(), String> {
    commands::player_play_next(&state, items).await
}

#[tauri::command]
async fn player_clear(state: State<'_, AppState>) -> Result<(), String> {
    commands::player_clear(&state).await
}

#[tauri::command]
async fn playlist_recommendations(
    state: State<'_, AppState>,
    seed_ids: Vec<String>,
    exclude_ids: Option<Vec<String>>,
    limit: Option<u8>,
    page: Option<u32>,
) -> Result<Vec<Track>, String> {
    commands::playlist_recommendations(&state, seed_ids, exclude_ids, limit, page).await
}

#[tauri::command]
async fn similar_songs(
    state: State<'_, AppState>,
    song_id: String,
    exclude_ids: Option<Vec<String>>,
    depth: Option<u32>,
) -> Result<Vec<Track>, String> {
    commands::similar_songs(&state, song_id, exclude_ids, depth).await
}

#[tauri::command]
fn set_player_notifications(state: State<'_, AppState>, enabled: bool) -> Result<String, String> {
    commands::set_player_notifications(&state, enabled)
}

#[tauri::command]
fn player_notifications(state: State<'_, AppState>) -> Result<bool, String> {
    commands::player_notifications(&state)
}

#[tauri::command]
fn dead_ids(state: State<'_, AppState>) -> Result<Vec<String>, String> {
    commands::dead_ids(&state)
}

#[tauri::command]
fn set_discord_enabled(state: State<'_, AppState>, enabled: bool) -> Result<String, String> {
    commands::set_discord_enabled(&state, enabled)
}

#[tauri::command]
fn set_discord_app_id(state: State<'_, AppState>, app_id: String) -> Result<String, String> {
    commands::set_discord_app_id(&state, app_id)
}

#[tauri::command]
fn update_discord_presence(
    state: State<'_, AppState>,
    payload: PresencePayload,
) -> Result<(), String> {
    commands::update_discord_presence(&state, payload)
}

#[tauri::command]
fn clear_discord_presence(state: State<'_, AppState>) -> Result<(), String> {
    commands::clear_discord_presence(&state)
}

#[tauri::command]
async fn thumb_cache(url: String, size: u32) -> Result<String, String> {
    shell::thumb_cache::thumb_cache(url, size).await
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
    let state = AppState::new();
    tauri::Builder::default()
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_opener::init())
        .manage(state)
        .setup(|app| {
            if let Ok(dir) = app.path().app_config_dir() {
                paths::init_config_dir(dir);
            }
            let session_config = {
                #[cfg(windows)]
                {
                    let mut session_config = shell::media_session::MediaSessionConfig::new();
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
                    shell::media_session::MediaSessionConfig::new()
                }
            };
            let player = app.state::<AppState>().native.clone();
            tauri::async_runtime::spawn(async move {
                shell::spawn_background_tasks(player, session_config);
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
            dead_ids,
            player_warmup,
            thumb_cache,
            set_discord_enabled,
            set_discord_app_id,
            update_discord_presence,
            clear_discord_presence
        ])
        .run(tauri::generate_context!())
        .expect("failed to run tauri app");
}
