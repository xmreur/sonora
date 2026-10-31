//! Watched folder of the user's own audio files.
//!
//! The folder is the source of truth. `files-index.json` in the config dir
//! is a cache so the view can paint before a rescan finishes. Ids are the
//! first 16 hex chars of SHA-256(canonical path).

use lofty::file::{AudioFile, TaggedFileExt};
use lofty::tag::Accessor;
use notify::Watcher;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const EXTS: &[&str] = &[
    "flac", "mp3", "m4a", "mp4", "aac", "ogg", "opus", "wav", "aiff", "aif",
];

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LocalTrack {
    pub id: String,
    pub path: PathBuf,
    pub title: String,
    pub artist: String,
    pub album: String,
    #[serde(default)]
    pub track_number: Option<u32>,
    #[serde(default)]
    pub duration_ms: u64,
    #[serde(default)]
    pub has_art: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct DiskIndex {
    root: PathBuf,
    tracks: Vec<LocalTrack>,
}

pub struct FilesLib {
    dir: Mutex<PathBuf>,
    index: Mutex<Vec<LocalTrack>>,
    generation: AtomicU64,
    dirty: Arc<AtomicBool>,
    scanning: AtomicBool,
    last_event: Arc<Mutex<Option<Instant>>>,
    watcher: Mutex<Option<notify::RecommendedWatcher>>,
}

impl FilesLib {
    pub fn new() -> Self {
        let lib = Self {
            dir: Mutex::new(PathBuf::new()),
            index: Mutex::new(Vec::new()),
            generation: AtomicU64::new(0),
            dirty: Arc::new(AtomicBool::new(false)),
            scanning: AtomicBool::new(false),
            last_event: Arc::new(Mutex::new(None)),
            watcher: Mutex::new(None),
        };
        if let Some(disk) = load_index() {
            *lib.dir.lock().unwrap_or_else(|e| e.into_inner()) = disk.root;
            *lib.index.lock().unwrap_or_else(|e| e.into_inner()) = disk.tracks;
            lib.generation.store(1, Ordering::Relaxed);
        }
        lib
    }

    pub fn dir(&self) -> PathBuf {
        self.dir.lock().map(|g| g.clone()).unwrap_or_default()
    }

    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::Relaxed)
    }

    pub fn tracks(&self) -> Vec<LocalTrack> {
        self.index.lock().map(|g| g.clone()).unwrap_or_default()
    }

    pub fn get(&self, id: &str) -> Option<LocalTrack> {
        let index = self.index.lock().unwrap_or_else(|e| e.into_inner());
        resolve_track(&index, id)
    }

    /// Switch the watched folder. An empty path clears the library.
    pub fn set_dir(&self, dir: PathBuf) {
        let dir = if dir.as_os_str().is_empty() {
            PathBuf::new()
        } else {
            dir
        };
        let same = {
            let mut current = self.dir.lock().unwrap_or_else(|e| e.into_inner());
            let same = *current == dir;
            *current = dir.clone();
            same
        };
        let watching = self
            .watcher
            .lock()
            .ok()
            .is_some_and(|g| g.is_some() || dir.as_os_str().is_empty());
        if same && watching {
            return;
        }
        self.rewatch(&dir);
        self.request_rescan();
    }

    pub fn request_rescan(&self) {
        if let Ok(mut last) = self.last_event.lock() {
            *last = None;
        }
        self.dirty.store(true, Ordering::Relaxed);
    }

    pub fn dirty(&self) -> bool {
        self.dirty.load(Ordering::Relaxed)
    }

    /// True when a scan should start. The caller must call [`Self::finish_scan`]
    /// after it, including on failure.
    pub fn begin_scan(&self) -> bool {
        if self.scanning.swap(true, Ordering::AcqRel) {
            return false;
        }
        if !self.dirty.swap(false, Ordering::AcqRel) {
            self.scanning.store(false, Ordering::Release);
            return false;
        }
        let recent = self
            .last_event
            .lock()
            .ok()
            .and_then(|g| *g)
            .is_some_and(|t| t.elapsed() < Duration::from_millis(400));
        if recent {
            self.dirty.store(true, Ordering::Release);
            self.scanning.store(false, Ordering::Release);
            return false;
        }
        true
    }

    pub fn finish_scan(&self) {
        self.scanning.store(false, Ordering::Release);
    }

    pub fn scan_now(&self, downloads_root: &Path) {
        let dir = self.dir();
        let tracks = if dir.as_os_str().is_empty() || !dir.is_dir() {
            Vec::new()
        } else {
            scan_dir(&dir, Some(downloads_root))
        };
        save_index(&dir, &tracks);
        *self.index.lock().unwrap_or_else(|e| e.into_inner()) = tracks;
        self.generation.fetch_add(1, Ordering::Relaxed);
    }

    fn rewatch(&self, dir: &Path) {
        let mut slot = self.watcher.lock().unwrap_or_else(|e| e.into_inner());
        *slot = None;
        if dir.as_os_str().is_empty() || !dir.is_dir() {
            return;
        }
        let dirty = self.dirty.clone();
        let last = self.last_event.clone();
        let mut watcher =
            match notify::recommended_watcher(move |res: Result<notify::Event, notify::Error>| {
                if res.is_ok() {
                    dirty.store(true, Ordering::Relaxed);
                    if let Ok(mut stamp) = last.lock() {
                        *stamp = Some(Instant::now());
                    }
                }
            }) {
                Ok(w) => w,
                Err(e) => {
                    eprintln!("sonora files: watcher unavailable ({e})");
                    return;
                }
            };
        if let Err(e) = watcher.watch(dir, notify::RecursiveMode::Recursive) {
            eprintln!("sonora files: not watching {} ({e})", dir.display());
            return;
        }
        *slot = Some(watcher);
    }
}

impl Default for FilesLib {
    fn default() -> Self {
        Self::new()
    }
}

pub fn file_id(path: &Path) -> String {
    let canon = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let mut hasher = Sha256::new();
    hasher.update(canon.to_string_lossy().as_bytes());
    hex16(&hasher.finalize())
}

fn hex16(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(16);
    for byte in bytes.iter().take(8) {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0xf) as usize] as char);
    }
    out
}

pub fn art_path(id: &str) -> PathBuf {
    art_dir().join(format!("{id}.jpg"))
}

fn art_dir() -> PathBuf {
    dirs::cache_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("sonora")
        .join("file-art")
}

pub fn ensure_art(track: &LocalTrack) -> Option<PathBuf> {
    if !track.has_art {
        return None;
    }
    let dest = art_path(&track.id);
    if dest.is_file() {
        return Some(dest);
    }
    let tagged = lofty::probe::Probe::open(&track.path).ok()?.read().ok()?;
    let bytes = tagged.primary_tag()?.pictures().iter().find_map(|pic| {
        let data = pic.data();
        (!data.is_empty()).then(|| data.to_vec())
    })?;
    std::fs::create_dir_all(art_dir()).ok()?;
    std::fs::write(&dest, bytes).ok()?;
    Some(dest)
}

/// A track whose path still exists. A vanished file is a miss.
pub fn resolve_track(tracks: &[LocalTrack], id: &str) -> Option<LocalTrack> {
    tracks
        .iter()
        .find(|t| t.id == id)
        .filter(|t| t.path.is_file())
        .cloned()
}

/// Recursive scan. `skip` (the downloads root) and everything under it is ignored.
pub fn scan_dir(root: &Path, skip: Option<&Path>) -> Vec<LocalTrack> {
    let skip = skip
        .map(|p| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf()))
        .filter(|p| !p.as_os_str().is_empty());
    let mut out = Vec::new();
    walk(root, skip.as_deref(), &mut out);
    out.sort_by(|a, b| {
        (&a.album, &a.artist, a.track_number, &a.title).cmp(&(
            &b.album,
            &b.artist,
            b.track_number,
            &b.title,
        ))
    });
    out
}

fn walk(dir: &Path, skip: Option<&Path>, out: &mut Vec<LocalTrack>) {
    if skip.is_some_and(|s| covered_by(dir, s)) {
        return;
    }
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for ent in rd.flatten() {
        let path = ent.path();
        let name = ent.file_name();
        let name = name.to_string_lossy();
        if name.starts_with('.') {
            continue;
        }
        if path.is_dir() {
            walk(&path, skip, out);
            continue;
        }
        if !is_audio(&path) {
            continue;
        }
        if let Some(track) = read_track(&path) {
            out.push(track);
        }
    }
}

/// True when `dir` is `skip` or a child of it.
///
/// Both sides are canonicalized first. A raw `starts_with` misses the
/// downloads tree on macOS (`/var` → `/private/var`) and Windows (`C:\`
/// vs the `\\?\` prefix `canonicalize` adds), so those files were scanned.
fn covered_by(dir: &Path, skip: &Path) -> bool {
    let dir = std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
    dir.starts_with(skip)
}

fn is_audio(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|ext| EXTS.iter().any(|c| ext.eq_ignore_ascii_case(c)))
}

fn read_track(path: &Path) -> Option<LocalTrack> {
    if std::fs::metadata(path).ok()?.len() == 0 {
        return None;
    }
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("Track")
        .to_string();
    let mut title = stem;
    let mut artist = String::new();
    let mut album = String::new();
    let mut track_number = None;
    let mut duration_ms = 0u64;
    let mut has_art = false;
    if let Ok(tagged) = lofty::probe::Probe::open(path).and_then(|p| p.read()) {
        duration_ms = u64::try_from(tagged.properties().duration().as_millis()).unwrap_or(0);
        if let Some(tag) = tagged.primary_tag() {
            if let Some(value) = tag.title() {
                let value = value.trim();
                if !value.is_empty() {
                    title = value.to_string();
                }
            }
            if let Some(value) = tag.artist() {
                artist = value.trim().to_string();
            }
            if let Some(value) = tag.album() {
                album = value.trim().to_string();
            }
            track_number = tag.track();
            has_art = tag.pictures().iter().any(|pic| !pic.data().is_empty());
        }
    }
    Some(LocalTrack {
        id: file_id(path),
        path: path.to_path_buf(),
        title,
        artist,
        album,
        track_number,
        duration_ms,
        has_art,
    })
}

fn index_file() -> Option<PathBuf> {
    crate::paths::app_config_dir().map(|d| d.join("files-index.json"))
}

fn load_index() -> Option<DiskIndex> {
    let path = index_file()?;
    let raw = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&raw).ok()
}

fn save_index(root: &Path, tracks: &[LocalTrack]) {
    let Some(path) = index_file() else {
        return;
    };
    let disk = DiskIndex {
        root: root.to_path_buf(),
        tracks: tracks.to_vec(),
    };
    let Ok(raw) = serde_json::to_vec_pretty(&disk) else {
        return;
    };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let staging = path.with_extension("part");
    if std::fs::write(&staging, raw).is_ok() {
        let _ = std::fs::rename(&staging, &path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_wav(path: &Path) {
        let frames = 100u32;
        let rate = 8000u32;
        let channels = 1u16;
        let bits = 16u16;
        let data_len = frames * u32::from(channels) * u32::from(bits) / 8;
        let mut body = Vec::new();
        body.extend_from_slice(b"fmt ");
        body.extend_from_slice(&16u32.to_le_bytes());
        body.extend_from_slice(&1u16.to_le_bytes());
        body.extend_from_slice(&channels.to_le_bytes());
        body.extend_from_slice(&rate.to_le_bytes());
        let byte_rate = rate * u32::from(channels) * u32::from(bits) / 8;
        body.extend_from_slice(&byte_rate.to_le_bytes());
        body.extend_from_slice(&(channels * bits / 8).to_le_bytes());
        body.extend_from_slice(&bits.to_le_bytes());
        body.extend_from_slice(b"data");
        body.extend_from_slice(&data_len.to_le_bytes());
        body.extend(std::iter::repeat_n(0u8, data_len as usize));
        let mut data = Vec::new();
        data.extend_from_slice(b"RIFF");
        data.extend_from_slice(&(4 + body.len() as u32).to_le_bytes());
        data.extend_from_slice(b"WAVE");
        data.extend(body);
        std::fs::write(path, data).unwrap();
    }

    #[test]
    fn scan_skips_downloads_and_non_audio() {
        let root = std::env::temp_dir().join(format!("sonora-files-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("keep")).unwrap();
        std::fs::create_dir_all(root.join("Sonora").join("tracks").join("1")).unwrap();
        write_wav(&root.join("keep").join("Tune.wav"));
        write_wav(&root.join("Sonora").join("inside.wav"));
        std::fs::write(root.join("notes.txt"), b"nope").unwrap();
        std::fs::write(root.join("empty.mp3"), b"").unwrap();
        let tracks = scan_dir(&root, Some(&root.join("Sonora")));
        assert_eq!(tracks.len(), 1);
        assert_eq!(tracks[0].title, "Tune");
        assert!(tracks[0].path.ends_with("Tune.wav"));
        let missing = LocalTrack {
            id: tracks[0].id.clone(),
            path: root.join("gone.wav"),
            title: tracks[0].title.clone(),
            artist: String::new(),
            album: String::new(),
            track_number: None,
            duration_ms: 0,
            has_art: false,
        };
        assert!(resolve_track(std::slice::from_ref(&missing), &missing.id).is_none());
        assert!(resolve_track(&tracks, &tracks[0].id).is_some());
        let _ = std::fs::remove_dir_all(&root);
    }
}
