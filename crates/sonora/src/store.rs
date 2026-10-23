//! UI settings + session persistence — native JSON in the config dir
//! (the old web UI kept these in `localStorage`, which cannot migrate
//! across toolkits; a fresh start at upgrade was an accepted gap).

use std::path::PathBuf;

use apple_music_core::playback::QueueItem;
use serde::{Deserialize, Serialize};

use crate::app::Settings;
use crate::player::MirrorEntry;

pub fn settings_path() -> Option<PathBuf> {
    shell::paths::app_config_dir().map(|d| d.join("ui-settings.json"))
}

/// Load persisted settings, falling back to defaults on a missing or
/// corrupt file.
pub fn load() -> Settings {
    settings_path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

/// Write settings to disk; returns `true` when the file changed.
pub fn save_if_changed(settings: &Settings, last_saved: &mut Option<String>) -> bool {
    let Ok(json) = serde_json::to_string(settings) else {
        return false;
    };
    if last_saved.as_ref() == Some(&json) {
        return false;
    }
    let Some(path) = settings_path() else {
        return false;
    };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if std::fs::write(&path, &json).is_ok() {
        *last_saved = Some(json);
        true
    } else {
        false
    }
}

// ---------------------------------------------------------------------------
// Session (queue + position) — old `sonora-queue-v1` localStorage store.
// ---------------------------------------------------------------------------

/// Persisted queue cap (old `QUEUE_STORE_MAX`).
pub const SESSION_MAX: usize = 300;

/// One persisted queue entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionEntry {
    pub id: String,
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub artist: Option<String>,
    #[serde(default)]
    pub radio: bool,
    #[serde(default)]
    pub art: Option<String>,
    #[serde(default)]
    pub duration_ms: Option<u64>,
    #[serde(default)]
    pub genres: Vec<String>,
}

impl From<&MirrorEntry> for SessionEntry {
    fn from(e: &MirrorEntry) -> Self {
        Self {
            id: e.item.id.clone(),
            kind: e.item.kind.clone(),
            title: e.item.title.clone(),
            artist: e.item.artist.clone(),
            radio: e.radio,
            art: e.art.clone(),
            duration_ms: e.duration_ms,
            genres: e.genres.clone(),
        }
    }
}

impl SessionEntry {
    pub fn into_mirror(self) -> MirrorEntry {
        MirrorEntry {
            item: QueueItem {
                id: self.id,
                kind: self.kind,
                title: self.title,
                artist: self.artist,
            },
            radio: self.radio,
            art: self.art,
            duration_ms: self.duration_ms,
            genres: self.genres,
        }
    }
}

/// Persisted session: the queue mirror + listening position, restored
/// paused on the next launch (never auto-plays).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub v: u32,
    /// Old `queueIndex`; `-1` when nothing was current.
    #[serde(default)]
    pub queue_index: i64,
    #[serde(default)]
    pub queue: Vec<SessionEntry>,
    /// Silent restores resume near here (live player reports win when
    /// present).
    #[serde(default)]
    pub position_ms: u64,
}

pub fn session_path() -> Option<PathBuf> {
    shell::paths::app_config_dir().map(|d| d.join("ui-session.json"))
}

/// Load the persisted session, `None` on a missing or corrupt file.
pub fn load_session() -> Option<Session> {
    let text = std::fs::read_to_string(session_path()?).ok()?;
    let session: Session = serde_json::from_str(&text).ok()?;
    (session.v == 1).then_some(session)
}

pub fn save_session(session: &Session) {
    let Ok(json) = serde_json::to_string(session) else {
        return;
    };
    let Some(path) = session_path() else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(path, json);
}
