//! Sonora-owned download library.
//!
//! `<root>/library.json` is the index. Audio and covers live under
//! `<root>/tracks/<sanitized id>/`. The map key is the catalog id (or the
//! library id when an upload has no catalog mapping). Directory names are
//! a sanitized form of that id so a hostile id cannot escape `tracks/`.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DownloadEntry {
    pub id: String,
    pub title: String,
    pub artist: String,
    pub album: String,
    #[serde(default)]
    pub album_id: Option<String>,
    #[serde(default)]
    pub duration_ms: u64,
    /// Library song id (`i.*`) when the download was requested that way.
    #[serde(default)]
    pub library_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct DownloadLibrary {
    #[serde(default)]
    pub tracks: BTreeMap<String, DownloadEntry>,
}

#[derive(Debug, Clone, Default)]
pub struct DownloadStatus {
    pub running: bool,
    pub done: u32,
    pub total: u32,
    pub failed: u32,
    pub label: String,
    pub last_error: Option<String>,
}

pub struct Downloads {
    root: Mutex<PathBuf>,
    library: Mutex<DownloadLibrary>,
    status: Mutex<DownloadStatus>,
    inflight: Mutex<HashSet<String>>,
    failed: Mutex<HashMap<String, String>>,
    generation: AtomicU64,
    tx: Mutex<Option<UnboundedSender<String>>>,
}

impl Downloads {
    pub fn new() -> Self {
        let root = default_downloads_dir();
        let library = load_library(&root);
        Self {
            root: Mutex::new(root),
            library: Mutex::new(library),
            status: Mutex::new(DownloadStatus::default()),
            inflight: Mutex::new(HashSet::new()),
            failed: Mutex::new(HashMap::new()),
            generation: AtomicU64::new(1),
            tx: Mutex::new(None),
        }
    }

    pub fn root(&self) -> PathBuf {
        self.root
            .lock()
            .map(|g| g.clone())
            .unwrap_or_else(|e| e.into_inner().clone())
    }

    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::Relaxed)
    }

    pub fn status(&self) -> DownloadStatus {
        self.status.lock().map(|g| g.clone()).unwrap_or_default()
    }

    /// Point at another tree. Does not move files.
    pub fn set_root(&self, dir: PathBuf) {
        let dir = if dir.as_os_str().is_empty() {
            default_downloads_dir()
        } else {
            dir
        };
        let mut root = self.root.lock().unwrap_or_else(|e| e.into_inner());
        if *root == dir {
            return;
        }
        *root = dir.clone();
        drop(root);
        let loaded = load_library(&dir);
        *self.library.lock().unwrap_or_else(|e| e.into_inner()) = loaded;
        self.generation.fetch_add(1, Ordering::Relaxed);
    }

    pub fn contains(&self, id: &str) -> bool {
        let root = self.root();
        let lib = self.library.lock().unwrap_or_else(|e| e.into_inner());
        lookup(&lib, id).is_some_and(|e| audio_path(&root, &e.id).is_file())
    }

    pub fn saved_ids(&self) -> HashSet<String> {
        let root = self.root();
        let lib = self.library.lock().unwrap_or_else(|e| e.into_inner());
        let mut ids = HashSet::new();
        for (id, entry) in &lib.tracks {
            if audio_path(&root, id).is_file() {
                ids.insert(id.clone());
                if let Some(library_id) = &entry.library_id {
                    ids.insert(library_id.clone());
                }
            }
        }
        ids
    }

    pub fn list(&self) -> Vec<DownloadEntry> {
        let root = self.root();
        let lib = self.library.lock().unwrap_or_else(|e| e.into_inner());
        lib.tracks
            .values()
            .filter(|e| audio_path(&root, &e.id).is_file())
            .cloned()
            .collect()
    }

    pub fn entry(&self, id: &str) -> Option<DownloadEntry> {
        let lib = self.library.lock().unwrap_or_else(|e| e.into_inner());
        let entry = lookup(&lib, id)?.clone();
        drop(lib);
        if audio_path(&self.root(), &entry.id).is_file() {
            Some(entry)
        } else {
            None
        }
    }

    pub fn read_audio(&self, id: &str) -> Option<(DownloadEntry, Vec<u8>)> {
        let entry = self.entry(id)?;
        let bytes = std::fs::read(audio_path(&self.root(), &entry.id)).ok()?;
        if bytes.is_empty() {
            None
        } else {
            Some((entry, bytes))
        }
    }

    pub fn cover_file(&self, id: &str) -> Option<PathBuf> {
        let entry = self.entry(id)?;
        let path = cover_path(&self.root(), &entry.id);
        path.is_file().then_some(path)
    }

    pub fn has_lyrics(&self, id: &str) -> bool {
        let Some(entry) = self.entry(id) else {
            return false;
        };
        lyrics_path(&self.root(), &entry.id).is_file()
    }

    pub fn read_lyrics(&self, id: &str) -> Option<apple_music_core::models::Lyrics> {
        let entry = self.entry(id)?;
        let raw = std::fs::read(lyrics_path(&self.root(), &entry.id)).ok()?;
        serde_json::from_slice(&raw).ok()
    }

    pub fn write_lyrics(
        &self,
        id: &str,
        lyrics: &apple_music_core::models::Lyrics,
    ) -> Result<(), String> {
        let entry = self
            .entry(id)
            .ok_or_else(|| "that track is not downloaded".to_string())?;
        let raw = serde_json::to_vec_pretty(lyrics).map_err(|e| e.to_string())?;
        write_atomic(&lyrics_path(&self.root(), &entry.id), &raw)
    }

    /// Insert `id` into the in-flight set. False when it is already queued
    /// or saved.
    pub fn claim(&self, id: &str) -> bool {
        if self.contains(id) && self.has_lyrics(id) {
            return false;
        }
        let inserted = self
            .inflight
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id.to_string());
        if inserted {
            self.failed
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(id);
            self.generation.fetch_add(1, Ordering::Relaxed);
        }
        inserted
    }

    pub fn release(&self, id: &str) {
        self.inflight
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(id);
        self.generation.fetch_add(1, Ordering::Relaxed);
    }

    pub fn inflight_ids(&self) -> HashSet<String> {
        self.inflight
            .lock()
            .map(|g| g.clone())
            .unwrap_or_else(|e| e.into_inner().clone())
    }

    pub fn failures(&self) -> HashMap<String, String> {
        self.failed
            .lock()
            .map(|g| g.clone())
            .unwrap_or_else(|e| e.into_inner().clone())
    }

    pub fn note_failure(&self, id: &str, err: &str) {
        self.failed
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id.to_string(), err.to_string());
        self.generation.fetch_add(1, Ordering::Relaxed);
    }

    pub fn clear_failure(&self, id: &str) {
        let removed = self
            .failed
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(id)
            .is_some();
        if removed {
            self.generation.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub fn begin_batch(&self, count: u32, label: String) {
        let mut status = self.status.lock().unwrap_or_else(|e| e.into_inner());
        if !status.running {
            *status = DownloadStatus::default();
        }
        status.running = true;
        status.total = status.total.saturating_add(count);
        status.label = label;
    }

    pub fn finish_one(&self, error: Option<String>) {
        let mut status = self.status.lock().unwrap_or_else(|e| e.into_inner());
        if error.is_some() {
            status.failed = status.failed.saturating_add(1);
            status.last_error = error;
        } else {
            status.done = status.done.saturating_add(1);
        }
        if status.done.saturating_add(status.failed) >= status.total {
            status.running = false;
        }
    }

    /// First call installs the worker channel and returns its receiver.
    pub fn install_worker(&self) -> Option<UnboundedReceiver<String>> {
        let mut tx = self.tx.lock().unwrap_or_else(|e| e.into_inner());
        if tx.is_some() {
            return None;
        }
        let (send, recv) = tokio::sync::mpsc::unbounded_channel();
        *tx = Some(send);
        Some(recv)
    }

    pub fn push(&self, id: String) -> Result<(), String> {
        let tx = self.tx.lock().unwrap_or_else(|e| e.into_inner());
        let tx = tx
            .as_ref()
            .ok_or_else(|| "download worker is not running".to_string())?;
        tx.send(id)
            .map_err(|_| "download worker stopped".to_string())
    }

    pub fn insert(&self, entry: DownloadEntry) -> Result<(), String> {
        let root = self.root();
        let mut lib = self.library.lock().unwrap_or_else(|e| e.into_inner());
        lib.tracks.insert(entry.id.clone(), entry);
        save_library(&root, &lib)?;
        drop(lib);
        self.generation.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    pub fn remove(&self, id: &str) -> Result<String, String> {
        let root = self.root();
        let mut lib = self.library.lock().unwrap_or_else(|e| e.into_inner());
        let key = lookup(&lib, id)
            .map(|e| e.id.clone())
            .ok_or_else(|| "that track is not downloaded".to_string())?;
        lib.tracks.remove(&key);
        save_library(&root, &lib)?;
        drop(lib);
        let dir = audio_path(&root, &key)
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| root.join("tracks").join(sanitize_id(&key)));
        if dir.starts_with(root.join("tracks")) {
            let _ = std::fs::remove_dir_all(&dir);
        }
        self.generation.fetch_add(1, Ordering::Relaxed);
        Ok(format!("Removed {key}"))
    }
}

impl Default for Downloads {
    fn default() -> Self {
        Self::new()
    }
}

pub fn default_downloads_dir() -> PathBuf {
    dirs::audio_dir()
        .or_else(|| dirs::home_dir().map(|h| h.join("Music")))
        .unwrap_or_else(std::env::temp_dir)
        .join("Sonora")
}

/// Directory-safe id. Slashes and `..` cannot survive this.
pub fn sanitize_id(id: &str) -> String {
    let safe: String = id
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        .collect();
    let trimmed = safe.trim_matches('.');
    if trimmed.is_empty() {
        "track".to_string()
    } else {
        trimmed.to_string()
    }
}

pub fn audio_path(root: &Path, id: &str) -> PathBuf {
    track_dir(root, id).join("audio.m4a")
}

pub fn cover_path(root: &Path, id: &str) -> PathBuf {
    track_dir(root, id).join("cover.jpg")
}

pub fn lyrics_path(root: &Path, id: &str) -> PathBuf {
    track_dir(root, id).join("lyrics.json")
}

fn track_dir(root: &Path, id: &str) -> PathBuf {
    root.join("tracks").join(sanitize_id(id))
}

pub fn lookup<'a>(lib: &'a DownloadLibrary, id: &str) -> Option<&'a DownloadEntry> {
    if let Some(entry) = lib.tracks.get(id) {
        return Some(entry);
    }
    lib.tracks
        .values()
        .find(|e| e.library_id.as_deref() == Some(id))
}

pub fn load_library(root: &Path) -> DownloadLibrary {
    let path = root.join("library.json");
    let Ok(raw) = std::fs::read_to_string(&path) else {
        return DownloadLibrary::default();
    };
    serde_json::from_str(&raw).unwrap_or_default()
}

pub fn save_library(root: &Path, lib: &DownloadLibrary) -> Result<(), String> {
    std::fs::create_dir_all(root).map_err(|e| e.to_string())?;
    let raw = serde_json::to_vec_pretty(lib).map_err(|e| e.to_string())?;
    write_atomic(&root.join("library.json"), &raw)
}

pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let staging = path.with_extension("part");
    std::fs::write(&staging, bytes).map_err(|e| e.to_string())?;
    std::fs::rename(&staging, path).map_err(|e| {
        let _ = std::fs::remove_file(&staging);
        e.to_string()
    })
}

/// Metadata fields pulled from a catalog `songs` payload.
pub fn entry_from_song(
    id: String,
    library_id: Option<String>,
    v: &serde_json::Value,
) -> DownloadEntry {
    let attrs = v
        .get("data")
        .and_then(|d| d.as_array())
        .and_then(|a| a.first())
        .and_then(|i| i.get("attributes"));
    let text = |key: &str| {
        attrs
            .and_then(|a| a.get(key))
            .and_then(|s| s.as_str())
            .unwrap_or("")
            .to_string()
    };
    let album_id = v
        .pointer("/data/0/relationships/albums/data/0/id")
        .and_then(|s| s.as_str())
        .map(str::to_string);
    DownloadEntry {
        title: {
            let name = text("name");
            if name.is_empty() {
                id.clone()
            } else {
                name
            }
        },
        artist: text("artistName"),
        album: text("albumName"),
        album_id,
        duration_ms: attrs
            .and_then(|a| a.get("durationInMillis"))
            .and_then(|d| d.as_u64())
            .unwrap_or(0),
        id,
        library_id,
    }
}

pub fn artwork_template(v: &serde_json::Value) -> Option<String> {
    v.pointer("/data/0/attributes/artwork/url")
        .and_then(|s| s.as_str())
        .map(str::to_string)
        .filter(|s| !s.is_empty())
}

/// Apple artwork template → a concrete 600px JPEG URL.
pub fn cover_url(template: &str) -> Option<String> {
    let mut template = template.trim().to_string();
    if template.is_empty() {
        return None;
    }
    if let Some(rest) = template.strip_prefix("//") {
        template = format!("https://{rest}");
    }
    if !(template.starts_with("http://") || template.starts_with("https://")) {
        return None;
    }
    Some(
        template
            .replace("{w}", "600")
            .replace("{h}", "600")
            .replace("{c}", "bb")
            .replace("{f}", "jpg")
            .replace("{q}", "60"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Component;

    #[test]
    fn sanitize_drops_path_separators() {
        assert_eq!(sanitize_id("../etc/passwd"), "etcpasswd");
        assert_eq!(sanitize_id(".."), "track");
        assert_eq!(sanitize_id("i.abc_1"), "i.abc_1");
        let root = Path::new("/tmp/sonora-dl");
        let path = audio_path(root, "../etc/passwd");
        assert_eq!(
            path,
            root.join("tracks").join("etcpasswd").join("audio.m4a")
        );
        assert!(path.starts_with(root));
        assert!(!path.components().any(|c| matches!(c, Component::ParentDir)));
    }

    #[test]
    fn library_round_trip() {
        let dir = std::env::temp_dir().join(format!("sonora-dl-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let entry = DownloadEntry {
            id: "123".into(),
            title: "Song".into(),
            artist: "Artist".into(),
            album: "Album".into(),
            album_id: Some("a1".into()),
            duration_ms: 1000,
            library_id: Some("i.abc".into()),
        };
        let mut lib = DownloadLibrary::default();
        lib.tracks.insert(entry.id.clone(), entry.clone());
        save_library(&dir, &lib).unwrap();
        let loaded = load_library(&dir);
        assert_eq!(loaded, lib);
        assert_eq!(lookup(&loaded, "i.abc").map(|e| e.id.as_str()), Some("123"));
        assert_eq!(
            lookup(&loaded, "123").map(|e| e.title.as_str()),
            Some("Song")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
