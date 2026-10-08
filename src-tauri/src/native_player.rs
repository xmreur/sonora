//! Native Apple Music playback (no browser sidecar).
//!
//! Replaces the Firefox + MusicKit page with an in-process pipeline:
//! `webPlayback` resolve → Widevine license → CENC decrypt → local audio.
//! The existing [`SidecarManager`] stays as the status hub: this player
//! publishes [`PlayerReport`]s into it, so the UI poll, MPRIS/SMTC bridges,
//! and notifications keep working unchanged.
//!
//! Backend selection is `SONORA_PLAYER`: `native` (default) or `firefox`
//! (legacy sidecar). Queue/Next/Previous live here; per-track resolve +
//! metadata stays in the Tauri commands (they own the API client).

use apple_music_core::playback::QueueItem;
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc, Mutex,
};

use crate::sidecar::{PlayerReport, SidecarManager};

/// `true` unless `SONORA_PLAYER=firefox` explicitly opts into the legacy sidecar.
pub fn use_native() -> bool {
    std::env::var("SONORA_PLAYER")
        .map(|v| !v.eq_ignore_ascii_case("firefox"))
        .unwrap_or(true)
}

/// Now-playing metadata attached to native reports.
#[derive(Debug, Clone, Default)]
pub struct TrackMeta {
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub art_url: Option<String>,
    pub duration_ms: u64,
}

/// Pull now-playing fields out of a catalog `songs` response
/// (`data[0].attributes`). Pure, unit-tested.
pub fn meta_from_song(v: &serde_json::Value) -> TrackMeta {
    let attrs = v
        .get("data")
        .and_then(|d| d.as_array())
        .and_then(|a| a.first())
        .and_then(|i| i.get("attributes"));
    let str_field = |k: &str| {
        attrs
            .and_then(|a| a.get(k))
            .and_then(|s| s.as_str())
            .map(str::to_string)
    };
    let art = attrs
        .and_then(|a| a.get("artwork"))
        .and_then(|a| a.get("url"))
        .and_then(|u| u.as_str())
        .map(|t| {
            t.replace("{w}", "512")
                .replace("{h}", "512")
                .replace("{c}", "bb")
                .replace("{f}", "jpg")
                .replace("{q}", "60")
        });
    TrackMeta {
        title: str_field("name"),
        artist: str_field("artistName"),
        album: str_field("albumName"),
        art_url: art,
        duration_ms: attrs
            .and_then(|a| a.get("durationInMillis"))
            .and_then(|d| d.as_u64())
            .unwrap_or(0),
    }
}

struct Inner {
    sidecar: SidecarManager,
    engine: Mutex<Option<am_playback::audio::NativeEngine>>,
    engine_error: Mutex<Option<String>>,
    queue: Mutex<Vec<QueueItem>>,
    index: Mutex<usize>,
    meta: Mutex<TrackMeta>,
    publish_task: Mutex<Option<tokio::task::JoinHandle<()>>>,
    generation: AtomicU64,
    /// Last logged `(track_id, playing)` so the 500ms publish loop stays quiet.
    last_log: Mutex<(Option<String>, bool)>,
    /// Prefetch flights in progress (dedup: one fetch per id at a time).
    inflight: Mutex<std::collections::HashSet<String>>,
}

#[derive(Clone, Default)]
pub struct NativePlayer {
    inner: Option<Arc<Inner>>,
}

impl NativePlayer {
    pub fn new(sidecar: SidecarManager) -> Self {
        Self {
            inner: Some(Arc::new(Inner {
                sidecar,
                engine: Mutex::new(None),
                engine_error: Mutex::new(None),
                queue: Mutex::new(Vec::new()),
                index: Mutex::new(0),
                meta: Mutex::new(TrackMeta::default()),
                publish_task: Mutex::new(None),
                generation: AtomicU64::new(0),
                last_log: Mutex::new((None, false)),
                inflight: Mutex::new(std::collections::HashSet::new()),
            })),
        }
    }

    fn inner(&self) -> Result<Arc<Inner>, String> {
        self.inner
            .clone()
            .ok_or_else(|| "native player not initialised".into())
    }

    /// Lazily create the audio engine (no audio device in CI/headless until play).
    fn ensure_engine(inner: &Arc<Inner>) -> Result<(), String> {
        let mut g = inner.engine.lock().map_err(|e| e.to_string())?;
        if g.is_none() {
            match am_playback::audio::NativeEngine::new() {
                Ok(e) => {
                    *g = Some(e);
                    *inner.engine_error.lock().map_err(|e| e.to_string())? = None;
                }
                Err(e) => {
                    let msg = format!("no audio output ({e}); install ALSA/PipeWire output or use SONORA_PLAYER=firefox");
                    *inner.engine_error.lock().map_err(|e| e.to_string())? = Some(msg.clone());
                    return Err(msg);
                }
            }
        }
        Ok(())
    }

    pub fn engine_error(&self) -> Option<String> {
        self.inner
            .as_ref()
            .and_then(|i| i.engine_error.lock().ok().and_then(|g| g.clone()))
    }

    pub fn set_queue(&self, items: Vec<QueueItem>, start_index: usize) -> Result<(), String> {
        let inner = self.inner()?;
        *inner.queue.lock().map_err(|e| e.to_string())? = items;
        *inner.index.lock().map_err(|e| e.to_string())? = start_index;
        Ok(())
    }

    pub fn queue_next(&self, items: Vec<QueueItem>) -> Result<(), String> {
        let inner = self.inner()?;
        let mut q = inner.queue.lock().map_err(|e| e.to_string())?;
        let idx = *inner.index.lock().map_err(|e| e.to_string())?;
        let at = (idx + 1).min(q.len());
        q.splice(at..at, items);
        Ok(())
    }

    pub fn queue_append(&self, items: Vec<QueueItem>) -> Result<(), String> {
        let inner = self.inner()?;
        inner.queue.lock().map_err(|e| e.to_string())?.extend(items);
        Ok(())
    }

    pub fn queue_clear(&self) -> Result<(), String> {
        let inner = self.inner()?;
        inner.queue.lock().map_err(|e| e.to_string())?.clear();
        *inner.index.lock().map_err(|e| e.to_string())? = 0;
        Ok(())
    }

    /// The next `n` queued items after the cursor (prefetch candidates).
    pub fn upcoming(&self, n: usize) -> Vec<QueueItem> {
        let Some(inner) = self.inner.as_ref() else {
            return Vec::new();
        };
        let (q, idx) = match (inner.queue.lock(), inner.index.lock()) {
            (Ok(q), Ok(idx)) => (q, *idx),
            _ => return Vec::new(),
        };
        q.iter().skip(idx + 1).take(n).cloned().collect()
    }

    /// Move the queue cursor; returns the newly targeted item, if any.
    pub fn step(&self, delta: isize) -> Result<Option<QueueItem>, String> {
        let inner = self.inner()?;
        let q = inner.queue.lock().map_err(|e| e.to_string())?;
        if q.is_empty() {
            return Ok(None);
        }
        let mut idx = inner.index.lock().map_err(|e| e.to_string())?;
        let next = (*idx as isize + delta).clamp(0, q.len() as isize - 1) as usize;
        *idx = next;
        Ok(q.get(next).cloned())
    }

    /// Decode + play already-resolved audio bytes as the current track.
    /// CPU-heavy decode runs on a blocking thread; status is published on completion.
    pub async fn play_bytes(
        &self,
        track_id: String,
        meta: TrackMeta,
        bytes: Vec<u8>,
    ) -> Result<u64, String> {
        let inner = self.inner()?;
        Self::ensure_engine(&inner)?;
        *inner.meta.lock().map_err(|e| e.to_string())? = meta;
        let task_inner = inner.clone();
        let (duration_ms, from_cache) = tokio::task::spawn_blocking(move || {
            let g = task_inner.engine.lock().map_err(|e| e.to_string())?;
            let engine = g.as_ref().ok_or_else(|| "audio engine gone".to_string())?;
            engine
                .play_bytes(track_id, &bytes)
                .map_err(|e| e.to_string())
        })
        .await
        .map_err(|e| format!("decode task: {e}"))??;
        eprintln!(
            "sonora native: ready ({} decode)",
            if from_cache { "cached" } else { "fresh" }
        );
        self.publish_now();
        self.start_publish_loop();
        Ok(duration_ms)
    }

    /// Background-prefetch `ids` (resolve → decrypt → decode) so the next
    /// tracks start instantly. Fire-and-forget, capped, deduped: skips ids
    /// already decoded or already in flight. Never touches playback state.
    pub fn prefetch(&self, dev: String, mut_token: String, storefront: String, ids: Vec<String>) {
        let Some(inner) = self.inner.clone() else {
            return;
        };
        tokio::spawn(async move {
            let mut fetched = 0usize;
            for id in ids.into_iter() {
                if fetched >= 2 {
                    break;
                }
                let decoded = inner
                    .engine
                    .lock()
                    .map(|g| g.as_ref().is_some_and(|e| e.is_decoded(&id)))
                    .unwrap_or(false);
                if decoded {
                    continue;
                }
                let fresh = inner
                    .inflight
                    .lock()
                    .map(|mut g| g.insert(id.clone()))
                    .unwrap_or(false);
                if !fresh {
                    continue;
                }
                let outcome = prefetch_one(&inner, &dev, &mut_token, &storefront, &id).await;
                inner.inflight.lock().map(|mut g| g.remove(&id)).ok();
                match outcome {
                    Ok(ms) => {
                        fetched += 1;
                        eprintln!("sonora native: prefetched {id} ({ms}ms)")
                    }
                    Err(e) => eprintln!("sonora native: prefetch {id} skipped ({e})"),
                }
            }
        });
    }

    pub fn pause(&self) -> Result<(), String> {
        let inner = self.inner()?;
        Self::ensure_engine(&inner)?;
        inner
            .engine
            .lock()
            .map_err(|e| e.to_string())?
            .as_ref()
            .ok_or_else(|| "audio engine gone".to_string())?
            .pause()
            .map_err(|e| e.to_string())?;
        self.publish_now();
        Ok(())
    }

    pub fn resume(&self) -> Result<(), String> {
        let inner = self.inner()?;
        Self::ensure_engine(&inner)?;
        inner
            .engine
            .lock()
            .map_err(|e| e.to_string())?
            .as_ref()
            .ok_or_else(|| "audio engine gone".to_string())?
            .resume()
            .map_err(|e| e.to_string())?;
        self.publish_now();
        self.start_publish_loop();
        Ok(())
    }

    pub fn stop(&self) -> Result<(), String> {
        let inner = self.inner()?;
        inner.generation.fetch_add(1, Ordering::Relaxed);
        if let Ok(g) = inner.engine.lock() {
            if let Some(e) = g.as_ref() {
                let _ = e.stop();
            }
        }
        *inner.meta.lock().map_err(|e| e.to_string())? = TrackMeta::default();
        self.publish_now();
        Ok(())
    }

    pub fn seek(&self, position_ms: u64) -> Result<(), String> {
        let inner = self.inner()?;
        Self::ensure_engine(&inner)?;
        inner
            .engine
            .lock()
            .map_err(|e| e.to_string())?
            .as_ref()
            .ok_or_else(|| "audio engine gone".to_string())?
            .seek(position_ms)
            .map_err(|e| e.to_string())?;
        self.publish_now();
        Ok(())
    }

    pub fn set_volume(&self, level: f32) -> Result<(), String> {
        let inner = self.inner()?;
        let level = level.clamp(0.0, 1.0);
        if let Ok(g) = inner.engine.lock() {
            if let Some(e) = g.as_ref() {
                let _ = e.set_volume(level);
            }
        }
        inner.sidecar.set_volume_level(level);
        self.publish_now();
        Ok(())
    }

    /// Build the current hub report from engine state + stored metadata.
    fn report(&self) -> Option<PlayerReport> {
        let inner = self.inner.as_ref()?;
        let status = inner.engine.lock().ok()?.as_ref().map(|e| e.status())?;
        let meta = inner.meta.lock().ok()?;
        Some(PlayerReport {
            playing: status.playing,
            track_id: status.track_id,
            title: meta.title.clone(),
            artist: meta.artist.clone(),
            album: meta.album.clone(),
            art_url: meta.art_url.clone(),
            os_next: 0,
            os_prev: 0,
            position_ms: status.position_ms,
            duration_ms: if meta.duration_ms > 0 {
                meta.duration_ms
            } else {
                status.duration_ms
            },
            detail: self.engine_error().unwrap_or_default(),
        })
    }

    /// Publish one fresh report into the shared hub (UI poll + bridges read it).
    pub fn publish_now(&self) {
        if let (Some(inner), Some(rep)) = (self.inner.as_ref(), self.report()) {
            // The 500ms loop would spam stderr: log only on track/play flip.
            let key = (rep.track_id.clone(), rep.playing);
            let changed = inner
                .last_log
                .lock()
                .map(|mut g| {
                    let changed = *g != key;
                    *g = key;
                    changed
                })
                .unwrap_or(true);
            if changed {
                eprintln!(
                    "sonora native: publish playing={} track={:?} pos={} dur={}",
                    rep.playing, rep.track_id, rep.position_ms, rep.duration_ms
                );
            }
            inner.sidecar.publish_report(rep);
        } else {
            eprintln!("sonora native: publish skipped (no engine/track yet)");
        }
    }

    /// Keep the hub report fresh (position clock) while audio is active.
    /// Only one loop runs: older generations exit on their next tick.
    fn start_publish_loop(&self) {
        let inner = match self.inner.clone() {
            Some(inner) => inner,
            None => return,
        };
        let generation = inner.generation.fetch_add(1, Ordering::Relaxed) + 1;
        let this = self.clone();
        let loop_inner = inner.clone();
        let handle = tokio::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                if loop_inner.generation.load(Ordering::Relaxed) != generation {
                    break;
                }
                this.publish_now();
                let alive = loop_inner
                    .engine
                    .lock()
                    .map(|g| {
                        g.as_ref()
                            .map(|e| !e.status().track_id.is_none())
                            .unwrap_or(false)
                    })
                    .unwrap_or(false);
                if !alive {
                    break;
                }
            }
        });
        replace_publish_task(&inner.publish_task, handle);
    }
}

/// Swap the publish-loop task, aborting its predecessor. Split out so the
/// mutex guard temporary never outlives the owning `Arc`.
fn replace_publish_task(
    slot: &Mutex<Option<tokio::task::JoinHandle<()>>>,
    handle: tokio::task::JoinHandle<()>,
) {
    if let Ok(mut guard) = slot.lock() {
        if let Some(old) = guard.take() {
            old.abort();
        }
        *guard = Some(handle);
    }
}

/// One prefetch unit: catalog-resolve → decrypt (disk-cached) → decode
/// into the engine cache. Returns the track duration.
async fn prefetch_one(
    inner: &Arc<Inner>,
    dev: &str,
    mut_token: &str,
    storefront: &str,
    id: &str,
) -> Result<u64, String> {
    let provider = PrefetchProvider {
        dev: dev.to_string(),
        mut_token: mut_token.to_string(),
    };
    let client =
        apple_music_core::api::ApiClient::new(&provider, storefront).map_err(|e| e.to_string())?;
    let catalog = if apple_music_core::api::ApiClient::is_library_song_id(id) {
        client
            .catalog_id_for_library_song(id)
            .await
            .map_err(|e| e.to_string())?
    } else {
        id.to_string()
    };
    let bytes = am_playback::stream::resolve_and_decrypt(&catalog, mut_token)
        .await
        .map_err(|e| e.to_string())?;
    NativePlayer::ensure_engine(inner)?;
    let task_inner = inner.clone();
    tokio::task::spawn_blocking(move || {
        let g = task_inner.engine.lock().map_err(|e| e.to_string())?;
        let engine = g.as_ref().ok_or_else(|| "audio engine gone".to_string())?;
        engine.prime(catalog, &bytes).map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| format!("prime task: {e}"))?
}

#[derive(Clone)]
struct PrefetchProvider {
    dev: String,
    mut_token: String,
}

impl apple_music_core::token::TokenProvider for PrefetchProvider {
    fn developer_token(&self) -> apple_music_core::Result<String> {
        Ok(self.dev.clone())
    }
    fn music_user_token(&self) -> Option<String> {
        Some(self.mut_token.clone())
    }
    fn set_music_user_token(&self, _token: String) -> apple_music_core::Result<()> {
        Err(apple_music_core::CoreError::Unsupported(
            "prefetch is read-only".into(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backend_defaults_to_native() {
        // Env-dependent; only assert the firefox opt-out parses.
        std::env::set_var("SONORA_PLAYER", "firefox");
        assert!(!use_native());
        std::env::set_var("SONORA_PLAYER", "native");
        assert!(use_native());
        std::env::remove_var("SONORA_PLAYER");
        assert!(use_native());
    }

    #[test]
    fn meta_from_catalog_song() {
        let v = serde_json::json!({
            "data": [{
                "id": "1811922756",
                "attributes": {
                    "name": "Kool-Aid",
                    "artistName": "Bring Me The Horizon",
                    "albumName": "POST HUMAN: NeX GEn",
                    "durationInMillis": 208000,
                    "artwork": { "url": "https://x/{w}x{h}bb.jpg" }
                }
            }]
        });
        let m = meta_from_song(&v);
        assert_eq!(m.title.as_deref(), Some("Kool-Aid"));
        assert_eq!(m.artist.as_deref(), Some("Bring Me The Horizon"));
        assert_eq!(m.duration_ms, 208000);
        assert_eq!(m.art_url.as_deref(), Some("https://x/512x512bb.jpg"));
    }

    #[test]
    fn meta_empty_without_data() {
        let m = meta_from_song(&serde_json::json!({}));
        assert_eq!(m.duration_ms, 0);
        assert!(m.title.is_none());
    }

    #[test]
    fn upcoming_returns_items_after_cursor() {
        let sidecar = SidecarManager::new();
        let n = NativePlayer::new(sidecar);
        let qi = |id: &str| QueueItem {
            id: id.into(),
            kind: "song".into(),
        };
        assert!(n.upcoming(2).is_empty());
        n.set_queue(vec![qi("a"), qi("b"), qi("c"), qi("d")], 1)
            .unwrap();
        let up = n.upcoming(2);
        assert_eq!(up.len(), 2);
        assert_eq!(up[0].id, "c");
        assert_eq!(up[1].id, "d");
        assert_eq!(n.upcoming(10).len(), 2);
    }

    #[test]
    fn queue_step_clamps() {
        let sidecar = SidecarManager::new();
        let n = NativePlayer::new(sidecar);
        let qi = |id: &str| QueueItem {
            id: id.into(),
            kind: "song".into(),
        };
        n.set_queue(vec![qi("a"), qi("b")], 0).unwrap();
        assert_eq!(n.step(1).unwrap().unwrap().id, "b");
        assert_eq!(n.step(1).unwrap().unwrap().id, "b");
        assert_eq!(n.step(-5).unwrap().unwrap().id, "a");
        n.queue_append(vec![qi("c")]).unwrap();
        n.queue_next(vec![qi("x")]).unwrap();
        // Cursor still on "a": stepping back clamps to the head.
        assert_eq!(n.step(-1).unwrap().unwrap().id, "a");
    }
}
