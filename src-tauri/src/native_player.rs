//! Native Apple Music playback (no browser engine).
//!
//! In-process pipeline: `webPlayback` resolve → Widevine license → CENC
//! decrypt → local audio. This player also owns the shared status hub
//! that the UI poll, MPRIS/SMTC bridges, and notifications read.
//!
//! Queue/Next/Previous live here; per-track resolve + metadata stays in
//! the Tauri commands (they own the API client).

use apple_music_core::playback::QueueItem;
use serde::{Deserialize, Serialize};
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc, Mutex,
};
use std::time::{Duration, Instant};

use am_playback::audio::{Decoded, SeekOutcome};
use am_playback::stream::ProgressiveCtx;

/// Samples per progressive chunklet (~9s of AAC audio each).
const CHUNK_SAMPLES: usize = 400;

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

/// Player status snapshot: the single source of truth the UI poll and
/// the OS media bridges (MPRIS/SMTC/Now Playing) read.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PlayerReport {
    #[serde(default)]
    pub playing: bool,
    #[serde(default)]
    pub track_id: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub artist: Option<String>,
    #[serde(default)]
    pub album: Option<String>,
    #[serde(default)]
    pub art_url: Option<String>,
    #[serde(default)]
    pub os_next: u64,
    #[serde(default)]
    pub os_prev: u64,
    #[serde(default)]
    pub position_ms: u64,
    #[serde(default)]
    pub duration_ms: u64,
    #[serde(default)]
    pub detail: String,
}

struct Inner {
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
    /// Progressive-pipeline generation: bumped on every new play/stop so a
    /// superseded background decrypt task exits at its next chunklet.
    pipe_gen: AtomicU64,
    /// Last pipeline failure, surfaced in the hub report detail.
    pipe_error: Mutex<Option<String>>,
    /// Shared status hub (UI poll + media bridges read this).
    report: Mutex<PlayerReport>,
    /// Last requested output level (mirrored for the MPRIS bridge).
    volume: Mutex<f32>,
    /// Track-change desktop notifications, opt-in via Settings.
    notify_enabled: Mutex<bool>,
    /// OS-media-key skip requests, consumed by the UI as queue jumps.
    os_next: Mutex<u64>,
    os_prev: Mutex<u64>,
}

#[derive(Clone, Default)]
pub struct NativePlayer {
    inner: Option<Arc<Inner>>,
}

impl NativePlayer {
    pub fn new() -> Self {
        Self {
            inner: Some(Arc::new(Inner {
                engine: Mutex::new(None),
                engine_error: Mutex::new(None),
                queue: Mutex::new(Vec::new()),
                index: Mutex::new(0),
                meta: Mutex::new(TrackMeta::default()),
                publish_task: Mutex::new(None),
                generation: AtomicU64::new(0),
                last_log: Mutex::new((None, false)),
                inflight: Mutex::new(std::collections::HashSet::new()),
                pipe_gen: AtomicU64::new(0),
                pipe_error: Mutex::new(None),
                report: Mutex::new(PlayerReport::default()),
                volume: Mutex::new(1.0),
                notify_enabled: Mutex::new(false),
                os_next: Mutex::new(0),
                os_prev: Mutex::new(0),
            })),
        }
    }

    /// Shared status hub read by the UI poll and the media bridges.
    /// Never fails: a poisoned lock reads as silence.
    pub fn status(&self) -> PlayerReport {
        let Some(inner) = self.inner.as_ref() else {
            return PlayerReport::default();
        };
        let mut rep = inner.report.lock().map(|g| g.clone()).unwrap_or_default();
        // OS skip counters live outside published reports so a report
        // overwrite can never clobber them — serve live.
        rep.os_next = inner.os_next.lock().map(|g| *g).unwrap_or(0);
        rep.os_prev = inner.os_prev.lock().map(|g| *g).unwrap_or(0);
        rep
    }

    /// Publish a report into the shared hub.
    pub fn publish_report(&self, rep: PlayerReport) {
        if let Some(inner) = self.inner.as_ref() {
            if let Ok(mut g) = inner.report.lock() {
                *g = rep;
            }
        }
    }

    /// Remember the last requested output level for the MPRIS bridge.
    pub fn set_volume_level(&self, level: f32) {
        if let Some(inner) = self.inner.as_ref() {
            if let Ok(mut g) = inner.volume.lock() {
                *g = level.clamp(0.0, 1.0);
            }
        }
    }

    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub fn volume(&self) -> f32 {
        self.inner
            .as_ref()
            .and_then(|i| i.volume.lock().ok().map(|g| *g))
            .unwrap_or(1.0)
    }

    pub fn set_notifications(&self, enabled: bool) -> Result<bool, String> {
        let inner = self.inner()?;
        *inner.notify_enabled.lock().map_err(|e| e.to_string())? = enabled;
        Ok(enabled)
    }

    pub fn notifications_enabled(&self) -> bool {
        self.inner
            .as_ref()
            .and_then(|i| i.notify_enabled.lock().ok().map(|g| *g))
            .unwrap_or(false)
    }

    pub fn request_os_next(&self) -> Result<(), String> {
        let inner = self.inner()?;
        let mut g = inner.os_next.lock().map_err(|e| e.to_string())?;
        *g = g.wrapping_add(1);
        Ok(())
    }

    pub fn request_os_prev(&self) -> Result<(), String> {
        let inner = self.inner()?;
        let mut g = inner.os_prev.lock().map_err(|e| e.to_string())?;
        *g = g.wrapping_add(1);
        Ok(())
    }

    /// Single synchronous seek attempt (no wait for decode): used by OS
    /// media-key handlers that cannot block on the async runtime. Targets
    /// past the decoded span are dropped.
    /// Only used on Windows/macOS (souvlaki callbacks run off-runtime).
    #[cfg(any(windows, target_os = "macos"))]
    pub fn try_seek_sync(&self, position_ms: u64) -> Result<(), String> {
        let inner = self.inner()?;
        Self::ensure_engine(&inner)?;
        {
            let g = inner.engine.lock().map_err(|e| e.to_string())?;
            let engine = g.as_ref().ok_or_else(|| "audio engine gone".to_string())?;
            match engine.seek(position_ms).map_err(|e| e.to_string())? {
                SeekOutcome::Applied => {}
                SeekOutcome::BeyondSpan => return Ok(()),
            }
        }
        self.publish_now();
        Ok(())
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
                    let msg = format!("no audio output ({e}); check ALSA/PipeWire output");
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

    /// Start progressive playback: fast path serves fully-decoded tracks
    /// from memory with no network at all; otherwise `begin` the fast
    /// prefix (resolve/download/license/layout, ~1s) and stream chunklets
    /// behind first audio. Returns immediately once the pipeline is
    /// running — the UI confirms off the published reports as usual.
    pub async fn play_progressive(
        &self,
        track_id: String,
        meta: TrackMeta,
        mut_token: String,
    ) -> Result<(), String> {
        let inner = self.inner()?;
        Self::ensure_engine(&inner)?;
        let gen = inner.pipe_gen.fetch_add(1, Ordering::Relaxed) + 1;
        *inner.pipe_error.lock().map_err(|e| e.to_string())? = None;
        *inner.meta.lock().map_err(|e| e.to_string())? = meta.clone();
        // Fast path: fully decoded in memory already.
        if let Ok(g) = inner.engine.lock() {
            if let Some(e) = g.as_ref() {
                let hit = e.play_cached(&track_id).map_err(|e| e.to_string()).is_ok();
                drop(g);
                if hit {
                    eprintln!("sonora native: playing {track_id} from decoded cache");
                    self.publish_now();
                    self.start_publish_loop();
                    return Ok(());
                }
            }
        }
        let prog = ProgressiveCtx::begin(&track_id, &mut_token)
            .await
            .map_err(|e| {
                // Honest failure: stop stale audio, publish reset.
                let _ = self.stop();
                e.to_string()
            })?;
        let this = self.clone();
        let task_inner = inner.clone();
        let tid = track_id.clone();
        let total_ms = meta.duration_ms;
        tokio::spawn(async move {
            pipeline_task(this, task_inner, prog, gen, tid, total_ms).await;
        });
        Ok(())
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
        inner.pipe_gen.fetch_add(1, Ordering::Relaxed);
        if let Ok(g) = inner.engine.lock() {
            if let Some(e) = g.as_ref() {
                let _ = e.stop();
            }
        }
        *inner.meta.lock().map_err(|e| e.to_string())? = TrackMeta::default();
        self.publish_now();
        Ok(())
    }

    /// Seek, waiting for the pipeline to decode the target first when it
    /// lies past the decoded span. Stale seeks (track changed underneath)
    /// are ignored.
    pub async fn seek(&self, position_ms: u64) -> Result<(), String> {
        let inner = self.inner()?;
        Self::ensure_engine(&inner)?;
        let id0 = Self::engine_id(&inner);
        let gen0 = inner.pipe_gen.load(Ordering::Relaxed);
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if Self::engine_id(&inner) != id0 || inner.pipe_gen.load(Ordering::Relaxed) != gen0 {
                // Stopped or superseded meanwhile: the new play owns the UI.
                return Ok(());
            }
            match Self::engine_seek(&inner, position_ms)? {
                SeekOutcome::Applied => {
                    self.publish_now();
                    return Ok(());
                }
                SeekOutcome::BeyondSpan => {
                    if let Some(err) = inner.pipe_error.lock().ok().and_then(|g| g.clone()) {
                        return Err(err);
                    }
                    if Instant::now() > deadline {
                        return Err("seek timed out waiting for decode".into());
                    }
                    tokio::time::sleep(Duration::from_millis(500)).await;
                }
            }
        }
    }

    fn engine_id(inner: &Arc<Inner>) -> Option<String> {
        inner.engine.lock().ok()?.as_ref()?.current_id()
    }

    fn engine_seek(inner: &Arc<Inner>, position_ms: u64) -> Result<SeekOutcome, String> {
        inner
            .engine
            .lock()
            .map_err(|e| e.to_string())?
            .as_ref()
            .ok_or_else(|| "audio engine gone".to_string())?
            .seek(position_ms)
            .map_err(|e| e.to_string())
    }

    /// Set the output level (mirrored for the MPRIS bridge).
    pub fn set_volume(&self, level: f32) -> Result<(), String> {
        let inner = self.inner()?;
        let level = level.clamp(0.0, 1.0);
        if let Ok(g) = inner.engine.lock() {
            if let Some(e) = g.as_ref() {
                let _ = e.set_volume(level);
            }
        }
        self.set_volume_level(level);
        self.publish_now();
        Ok(())
    }

    /// Build the current report from engine state + stored metadata.
    fn engine_report(&self) -> Option<PlayerReport> {
        let inner = self.inner.as_ref()?;
        let status = inner.engine.lock().ok()?.as_ref().map(|e| e.status())?;
        let meta = inner.meta.lock().ok()?;
        let detail = inner
            .pipe_error
            .lock()
            .ok()
            .and_then(|g| g.clone())
            .or_else(|| self.engine_error())
            .unwrap_or_default();
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
            detail,
        })
    }

    /// Publish one fresh report into the shared hub (UI poll + bridges read it).
    pub fn publish_now(&self) {
        if let (Some(inner), Some(rep)) = (self.inner.as_ref(), self.engine_report()) {
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
            self.publish_report(rep);
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

/// Background decrypt → decode → append loop. Generation-gated at every
/// chunklet, and every engine mutation re-verifies the track id, so a
/// superseded pipeline can never mix audio into a newer track.
async fn pipeline_task(
    this: NativePlayer,
    inner: Arc<Inner>,
    mut prog: ProgressiveCtx,
    gen: u64,
    track_id: String,
    total_ms: u64,
) {
    let total = prog.total_samples();
    let mut prev_len = 0usize;
    let mut first = true;
    let fail = |msg: String| {
        eprintln!("sonora native: pipeline {track_id} failed ({msg})");
        *inner.pipe_error.lock().unwrap_or_else(|e| e.into_inner()) = Some(msg);
        this.publish_now();
    };
    // NOTE: `inner`/`this` are borrowed by the closure; the loop below
    // only uses them through shared references, so the task stays alive
    // exactly as long as its generation is current.
    loop {
        if inner.pipe_gen.load(Ordering::Relaxed) != gen {
            break;
        }
        if prog.done_samples() >= total {
            break;
        }
        // Blocking: decrypt the next chunklet, then decode the prefix it
        // completes. The prefix re-decodes from scratch each time (simple
        // and deterministic); only the new tail frames are kept.
        let step = tokio::task::spawn_blocking(move || {
            if let Err(e) = prog.decrypt_next(CHUNK_SAMPLES) {
                return (prog, Err(e.to_string()));
            }
            let end = prog.prefix_end(prog.done_samples());
            let prefix = prog.plaintext_prefix(end).to_vec();
            match am_playback::audio::decode_mem(&prefix) {
                Ok((pcm, rate, channels, _)) => (prog, Ok((pcm, rate, channels))),
                Err(e) => (prog, Err(e.to_string())),
            }
        })
        .await;
        let (prog_back, step) = match step {
            Err(e) => {
                fail(format!("pipeline task: {e}"));
                break;
            }
            Ok(v) => v,
        };
        prog = prog_back;
        let (pcm_full, rate, channels) = match step {
            Err(e) => {
                fail(e);
                break;
            }
            Ok(v) => v,
        };
        let fresh = &pcm_full[prev_len.min(pcm_full.len())..];
        prev_len = pcm_full.len();
        if fresh.is_empty() {
            // Nothing new yet (degenerate truncation); keep going unless
            // the track is fully covered, then finish what we have.
            if prog.done_samples() >= total {
                break;
            }
            continue;
        }
        if inner.pipe_gen.load(Ordering::Relaxed) != gen {
            break;
        }
        let frames = fresh.len() as u64 / channels.max(1) as u64;
        let chunk = Arc::new(Decoded {
            pcm: fresh.to_vec(),
            rate,
            channels,
            duration_ms: frames * 1000 / rate.max(1) as u64,
        });
        // Synchronous stretch (no awaits): a newer play cannot interleave
        // between the generation check above and these engine calls.
        let engine_op = {
            let g = match inner.engine.lock() {
                Ok(g) => g,
                Err(_) => {
                    fail("engine lock".to_string());
                    break;
                }
            };
            let Some(e) = g.as_ref() else {
                fail("audio engine gone".to_string());
                break;
            };
            if first {
                e.play_first(track_id.clone(), chunk, total_ms)
                    .map_err(|e| e.to_string())
            } else {
                e.append_chunk(&track_id, chunk).map_err(|e| e.to_string())
            }
        };
        if let Err(e) = engine_op {
            // Stale-track guard tripped (or engine error): stop quietly.
            eprintln!("sonora native: pipeline {track_id} chunk dropped ({e})");
            break;
        }
        if first {
            first = false;
            eprintln!("sonora native: first audio for {track_id}");
            this.publish_now();
            this.start_publish_loop();
        }
        if prog.done_samples() >= total {
            // All decrypted and appended: mark complete and persist.
            let done = {
                match inner.engine.lock() {
                    Ok(g) => g
                        .as_ref()
                        .map(|e| e.finish_chunks(&track_id).map_err(|e| e.to_string()))
                        .unwrap_or(Ok(())),
                    Err(e) => Err(e.to_string()),
                }
            };
            if let Err(e) = done {
                eprintln!("sonora native: pipeline {track_id} finish dropped ({e})");
            }
            if !prog.from_cache() {
                let id = prog.adam_id().to_string();
                let plain = prog.finish();
                if let Err(e) = tokio::fs::write(am_playback::cache::cache_path(&id), &plain).await
                {
                    eprintln!("sonora native: cache store failed ({e})");
                }
                // Prime the decoded cache too, so replay is instant.
                let _ = prime_full(&inner, &track_id, &plain);
            }
            break;
        }
    }
}

/// Prime the decoded cache from full plaintext (best-effort).
fn prime_full(inner: &Arc<Inner>, track_id: &str, plain: &[u8]) -> Result<(), String> {
    let g = inner.engine.lock().map_err(|e| e.to_string())?;
    let engine = g.as_ref().ok_or_else(|| "audio engine gone".to_string())?;
    engine
        .prime(track_id.to_string(), plain)
        .map_err(|e| e.to_string())?;
    Ok(())
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
        let n = NativePlayer::new();
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
        let n = NativePlayer::new();
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

    #[test]
    fn hub_status_serves_live_counters() {
        let n = NativePlayer::new();
        let s = n.status();
        assert_eq!((s.os_next, s.os_prev), (0, 0));
        n.request_os_next().unwrap();
        n.request_os_next().unwrap();
        n.request_os_prev().unwrap();
        let s = n.status();
        assert_eq!((s.os_next, s.os_prev), (2, 1));
        assert!(!n.notifications_enabled());
        n.set_notifications(true).unwrap();
        assert!(n.notifications_enabled());
        assert_eq!(n.volume(), 1.0);
        n.set_volume_level(0.42);
        assert!((n.volume() - 0.42).abs() < f32::EPSILON);
        // Published reports round-trip through the hub.
        let rep = PlayerReport {
            playing: true,
            track_id: Some("1".into()),
            ..Default::default()
        };
        n.publish_report(rep);
        let s = n.status();
        assert!(s.playing);
        assert_eq!(s.track_id.as_deref(), Some("1"));
    }
}
