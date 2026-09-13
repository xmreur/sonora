use crate::error::{CoreError, Result};
use serde::{Deserialize, Serialize};

/// Which browser/webview engine renders the hidden minimal MusicKit player.
/// Tauri on Linux = WebKitGTK (no Widevine EME) so it cannot play DRM audio;
/// Gecko (Firefox + Widevine L3) is the primary non-Chromium path;
/// Chromium is a fallback using a *minimal* player page (not music.apple.com).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum EngineKind {
    #[default]
    Gecko,
    Chromium,
    WebKit,
}

impl std::str::FromStr for EngineKind {
    type Err = CoreError;
    fn from_str(s: &str) -> Result<Self> {
        match s.to_lowercase().as_str() {
            "gecko" | "firefox" => Ok(Self::Gecko),
            "chromium" | "chrome" | "electron" => Ok(Self::Chromium),
            "webkit" | "tauri" => Ok(Self::WebKit),
            _ => Err(CoreError::Unsupported(format!("engine {s}"))),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct QueueItem {
    pub id: String,
    #[serde(default)]
    pub kind: String, // "song" | "album" | "playlist"
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PlaybackState {
    pub playing: bool,
    #[serde(default)]
    pub track_id: Option<String>,
    #[serde(default)]
    pub position_ms: u64,
    #[serde(default)]
    pub queue: Vec<QueueItem>,
}

/// Commands the UI sends to the hidden player (over Tauri IPC / WebSocket).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "kebab-case")]
pub enum PlaybackCommand {
    SetQueue {
        items: Vec<QueueItem>,
    },
    /// Atomic replace-queue-then-play in ONE player poll tick. Using two
    /// separate commands races: Play can run while setQueue is still loading,
    /// leaving the new item queued but paused.
    PlayNow {
        items: Vec<QueueItem>,
        #[serde(default)]
        start_index: u32,
    },
    Play,
    Pause,
    Next,
    Previous,
    Seek {
        position_ms: u64,
    },
    SetVolume {
        level: f32,
    },
    /// Append songs to the tail of the current MusicKit queue.
    Append {
        items: Vec<QueueItem>,
    },
    /// Insert songs immediately after the now-playing item.
    PlayNext {
        items: Vec<QueueItem>,
    },
    /// Stop playback and clear the MusicKit queue.
    Clear,
}

/// Sidecar config: how to launch the external engine for the hidden player.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SidecarConfig {
    pub engine: EngineKind,
    pub player_url: String,
    pub binary_hint: String,
}

impl SidecarConfig {
    pub fn for_engine(engine: EngineKind, player_url: &str) -> Self {
        let binary_hint = match engine {
            EngineKind::Gecko => "firefox --kiosk".to_string(),
            EngineKind::Chromium => "chromium --app".to_string(),
            EngineKind::WebKit => "tauri-webview".to_string(),
        };
        Self {
            engine,
            player_url: player_url.to_string(),
            binary_hint,
        }
    }

    /// WebKitGTK cannot do Widevine EME on Linux — surface early.
    pub fn check_supported(&self) -> Result<()> {
        match self.engine {
            EngineKind::WebKit => Err(CoreError::Engine {
                engine: "webkit".into(),
                message: "WebKitGTK lacks Widevine EME; use gecko (default) or chromium fallback"
                    .into(),
            }),
            _ => Ok(()),
        }
    }
}

pub trait PlaybackEngine: Send + Sync {
    fn kind(&self) -> EngineKind;
    fn send(&self, cmd: &PlaybackCommand) -> Result<()>;
    fn state(&self) -> PlaybackState;
}

/// Test/placeholder engine until a real sidecar is wired.
pub struct NoopEngine {
    pub kind: EngineKind,
    pub last: std::sync::Mutex<Option<PlaybackCommand>>,
}

impl NoopEngine {
    pub fn new(kind: EngineKind) -> Self {
        Self {
            kind,
            last: std::sync::Mutex::new(None),
        }
    }
}

impl PlaybackEngine for NoopEngine {
    fn kind(&self) -> EngineKind {
        self.kind
    }
    fn send(&self, cmd: &PlaybackCommand) -> Result<()> {
        *self.last.lock().map_err(|_| CoreError::Engine {
            engine: format!("{:?}", self.kind),
            message: "lock".into(),
        })? = Some(cmd.clone());
        Ok(())
    }
    fn state(&self) -> PlaybackState {
        PlaybackState::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    #[test]
    fn engine_from_str() {
        assert_eq!(EngineKind::from_str("firefox").unwrap(), EngineKind::Gecko);
        assert_eq!(
            EngineKind::from_str("chromium").unwrap(),
            EngineKind::Chromium
        );
        assert!(EngineKind::from_str("nope").is_err());
    }

    #[test]
    fn webkit_rejected_for_playback() {
        let c = SidecarConfig::for_engine(EngineKind::WebKit, "http://localhost/player.html");
        assert!(c.check_supported().is_err());
        let g = SidecarConfig::for_engine(EngineKind::Gecko, "http://localhost/player.html");
        assert!(g.check_supported().is_ok());
    }

    #[test]
    fn noop_records_command() {
        let e = NoopEngine::new(EngineKind::Gecko);
        e.send(&PlaybackCommand::Play).unwrap();
        assert!(e.last.lock().unwrap().is_some());
    }

    #[test]
    fn play_now_wire_shape() {
        let cmd = PlaybackCommand::PlayNow {
            items: vec![QueueItem {
                id: "1".into(),
                kind: "song".into(),
            }],
            start_index: 0,
        };
        let v = serde_json::to_value(&cmd).unwrap();
        assert_eq!(v.get("cmd").and_then(|s| s.as_str()), Some("play-now"));
        let back: PlaybackCommand = serde_json::from_value(v).unwrap();
        assert!(matches!(back, PlaybackCommand::PlayNow { .. }));
    }
}
