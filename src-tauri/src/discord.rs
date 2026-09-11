//! Discord Rich Presence ("Listening to …" status).
//!
//! Opt-in and silent by design: without an app id, with the toggle off, or
//! when Discord isn't running, every entry point is a no-op that never
//! surfaces errors to the UI. The frontend pushes playback snapshots (see
//! `update`) and owns the toggle/app-id settings; this module only talks
//! to Discord's local IPC socket.
//!
//! Setup for users: create an application at https://discord.com/developers,
//! upload an image asset named `sonora` (plus optional `play` / `pause`
//! icons), then paste the application ID into Settings → Discord.

use discord_rich_presence::{
    activity::{Activity, Assets, Timestamps},
    DiscordIpc, DiscordIpcClient,
};
use serde::Deserialize;
use std::sync::Mutex;

/// Snapshot pushed by the UI playback poll.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct PresencePayload {
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub artist: String,
    #[serde(default)]
    pub album: String,
    #[serde(default)]
    pub playing: bool,
    #[serde(default)]
    pub position_ms: u64,
    #[serde(default)]
    pub duration_ms: u64,
}

/// Start/end unix seconds for the progress bar. `None` end when the
/// duration is unknown (live/unspecified length).
pub fn presence_window(position_ms: u64, duration_ms: u64, now_unix: i64) -> (i64, Option<i64>) {
    let start = (now_unix - (position_ms / 1000) as i64).max(0);
    let end = (duration_ms > 0).then(|| start.saturating_add((duration_ms / 1000) as i64));
    (start, end)
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

struct Inner {
    enabled: bool,
    app_id: String,
    client: Option<DiscordIpcClient>,
}

pub struct DiscordManager {
    inner: Mutex<Inner>,
}

impl DiscordManager {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(Inner {
                enabled: false,
                app_id: String::new(),
                client: None,
            }),
        }
    }

    pub fn set_enabled(&self, enabled: bool) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.enabled = enabled;
            if !enabled {
                Self::clear_locked(&mut inner);
            }
        }
    }

    pub fn set_app_id(&self, app_id: String) {
        if let Ok(mut inner) = self.inner.lock() {
            let app_id = app_id.trim().to_string();
            if inner.app_id != app_id {
                inner.app_id = app_id;
                // Reconnect lazily with the new id on the next update.
                Self::disconnect_locked(&mut inner);
            }
        }
    }

    pub fn clear(&self) {
        if let Ok(mut inner) = self.inner.lock() {
            Self::clear_locked(&mut inner);
        }
    }

    /// Push a playback snapshot. Paused/empty states clear the status.
    /// Never fails: IPC errors drop the client (reconnect on next update).
    pub fn update(&self, payload: &PresencePayload) {
        let mut inner = match self.inner.lock() {
            Ok(g) => g,
            Err(_) => return,
        };
        if !inner.enabled || inner.app_id.trim().is_empty() {
            Self::disconnect_locked(&mut inner);
            return;
        }
        if !payload.playing || payload.title.trim().is_empty() {
            Self::clear_locked(&mut inner);
            return;
        }
        if inner.client.is_none() {
            let id = inner.app_id.clone();
            let mut client = DiscordIpcClient::new(&id);
            if client.connect().is_err() {
                return; // Discord not running; retry on a later update.
            }
            inner.client = Some(client);
        }
        let (start, end) = presence_window(payload.position_ms, payload.duration_ms, unix_now());
        let mut timestamps = Timestamps::new().start(start);
        if let Some(end) = end {
            timestamps = timestamps.end(end);
        }
        let activity = Activity::new()
            .details(payload.title.trim())
            .state(if payload.artist.trim().is_empty() {
                "Sonora".to_string()
            } else {
                payload.artist.trim().to_string()
            })
            .assets(
                Assets::new()
                    .large_image("sonora")
                    .large_text(if payload.album.trim().is_empty() {
                        "Sonora".to_string()
                    } else {
                        payload.album.trim().to_string()
                    })
                    .small_image("play")
                    .small_text("Playing"),
            )
            .timestamps(timestamps);
        if let Some(client) = inner.client.as_mut() {
            if client.set_activity(activity).is_err() {
                Self::disconnect_locked(&mut inner);
            }
        }
    }

    fn clear_locked(inner: &mut Inner) {
        if let Some(client) = inner.client.as_mut() {
            let _ = client.clear_activity();
        }
    }

    fn disconnect_locked(inner: &mut Inner) {
        if let Some(mut client) = inner.client.take() {
            let _ = client.close();
        }
    }
}

impl Default for DiscordManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_math() {
        // 90s into a 240s track at t=1000 → start 910, end 1150.
        assert_eq!(presence_window(90_000, 240_000, 1000), (910, Some(1150)));
        // Unknown duration → start only.
        assert_eq!(presence_window(5_000, 0, 100), (95, None));
        // Position past now saturates instead of going negative.
        assert_eq!(presence_window(9_999_000, 0, 10), (0, None));
    }

    #[test]
    fn disabled_is_noop_without_discord() {
        let m = DiscordManager::new();
        m.update(&PresencePayload {
            title: "T".into(),
            playing: true,
            ..Default::default()
        });
        m.clear();
        // No app id / not enabled and Discord absent: must not panic or block.
    }

    #[test]
    fn empty_title_clears() {
        let m = DiscordManager::new();
        m.set_app_id("123".into());
        m.set_enabled(true);
        m.update(&PresencePayload {
            playing: true,
            ..Default::default()
        });
    }
}
