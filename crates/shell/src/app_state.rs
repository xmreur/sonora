//! Shared application state: credentials, caches, engine, presence.

use crate::commands::{load_dead_ids, mut_cache_path};
use crate::discord::DiscordManager;
use crate::downloads::Downloads;
use crate::files_lib::FilesLib;
use crate::native_player::NativePlayer;
use apple_music_core::token::{EnvTokenProvider, TokenProvider};
use std::sync::Mutex;

pub struct AppState {
    pub tokens: EnvTokenProvider,
    /// Cache for the auto-fetched web-player token (subscription-only path).
    pub web_token_cache: Mutex<Option<String>>,
    /// Cached (MUT, storefront) so warm plays skip the per-play
    /// `/v1/me/storefront` probe. Overwritten whenever the MUT differs.
    pub storefront_cache: Mutex<Option<(String, String)>>,
    /// In-process playback engine + shared status hub.
    pub native: NativePlayer,
    /// Store-dead track ids (requested + resolved forms), persisted across
    /// restarts. Listings stamp them `unavailable` without replaying the
    /// failure; a later successful play clears the id again.
    pub dead_ids: Mutex<std::collections::HashSet<String>>,
    /// Pending automatic sign-in server (aborted on cancel/logout/timeout).
    pub auth_server: Mutex<Option<tokio::task::JoinHandle<()>>>,
    /// Discord Rich Presence (opt-in via Settings → Discord).
    pub discord: DiscordManager,
    /// Saved Apple Music tracks (offline playback of the same catalog id).
    pub downloads: Downloads,
    /// Watched folder of the user's own audio files.
    pub files: FilesLib,
}

impl AppState {
    /// Build the shared state: env token provider, persisted MUT
    /// preloaded into memory, a fresh engine, the persisted dead-id set,
    /// no pending sign-in, presence off.
    pub fn new() -> Self {
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
        Self {
            tokens,
            web_token_cache: Mutex::new(None),
            storefront_cache: Mutex::new(None),
            native: NativePlayer::new(),
            dead_ids: Mutex::new(load_dead_ids()),
            auth_server: Mutex::new(None),
            discord: DiscordManager::new(),
            downloads: Downloads::new(),
            files: FilesLib::new(),
        }
    }
}

impl Default for AppState {
    fn default() -> Self {
        Self::new()
    }
}
