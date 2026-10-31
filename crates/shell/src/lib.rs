//! Service layer for Sonora: app state, the command layer, the native
//! playback engine, auth, and the OS media-session bridge.
//!
//! Called directly by the native egui UI (crates/sonora) — no webview
//! and no IPC shim in between.

pub mod app_state;
pub mod auth_flow;
pub mod commands;
pub mod discord;
pub mod downloads;
pub mod files_lib;
pub mod media_session;
pub mod native_player;
pub mod paths;
pub mod thumb_cache;
pub mod updater;

pub use app_state::AppState;
pub use discord::{DiscordManager, PresencePayload};
pub use native_player::{NativePlayer, PlayerReport};

/// Spawn the OS media-session bridge (MPRIS on Linux, souvlaki elsewhere)
/// and the 250ms status publish loop. Must be called from inside a Tokio
/// runtime context — the egui app calls it from its own runtime.
///
/// The hub refresh loop is spawned ONCE here (runtime context guaranteed).
/// Transport methods stay spawn-free so OS bridge threads — which have no
/// Tokio reactor — can call them without panicking. 250ms: track-change
/// confirmation and auto-advance react within one UI poll tick instead of
/// lagging a full beat behind.
pub fn spawn_background_tasks(player: NativePlayer, config: media_session::MediaSessionConfig) {
    let publisher = player.clone();
    tokio::spawn(async move {
        media_session::run(player, config).await;
    });
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
            publisher.publish_now();
        }
    });
}

/// Open a URL in the system browser (sign-in flow, share links).
pub fn open_url(url: impl Into<String>) -> Result<(), String> {
    open::that_detached(url.into()).map_err(|e| e.to_string())
}
