//! Animated covers (Apple Motion) — old `<video>` HLS loops → GStreamer.
//!
//! Some albums ship motion renditions from the catalog's `editorialVideo`
//! field: short muted HLS loops. The old web build played one `<video>`
//! element per visible surface — fullscreen cover, immersive cover,
//! immersive background, album-detail head (the bar tile was deliberately
//! static: one less decoder for a 200px surface) — with the static cover
//! underneath showing through every failure.
//!
//! This port keeps that shape: one `playbin3` pipeline per demanded slot
//! (hardware decode where available), capped at 512px / 15fps into an
//! `appsink`, drained by the frame tick into egui textures. Render sites
//! *demand* their slot each frame; `tick` sweeps what nobody asked for,
//! drains bus messages and frames, and follows playback state (the album
//! detail head free-runs, like the old detail video). Any failure — no
//! GStreamer, no HLS demux, no decoder, a stream error — tears the slot
//! down and the static cover shows through (`motionFailed` in the old app).
//!
//! Manifest URLs are session-cached and persisted for 30 days
//! (`sonora-motion-urls-v1` in the old localStorage), so repeat albums
//! skip the catalog fetch; segments still stream, exactly as before.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::time::Duration;

use apple_music_core::models::MotionArtwork;
use egui::{ColorImage, TextureHandle, TextureOptions};
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;

use crate::backend::Backend;

/// Manifest cache TTL (old `MOTION_TTL_MS`): 30 days.
const TTL_MS: u64 = 30 * 24 * 3600 * 1000;
/// Decode target height (width follows the source aspect) and frame rate.
const FRAME_HEIGHT: i32 = 512;
const FRAME_RATE: (i32, i32) = (15, 1);
/// Repaint cadence while a slot is running so 15fps frames land on time.
const FRAME_MS: u64 = 66;

/// The surfaces that can show motion (old slot names minus `np`/`detail`'s
/// ad-hoc element: everything here is demand-driven).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Slot {
    /// Classic fullscreen cover (`#fsCoverVideo`).
    Fs,
    /// Immersive foreground cover (`#imCoverVideo`).
    Im,
    /// Immersive background (`#imBgVideo`).
    ImBg,
    /// Album-detail head (`#detailMotionVideo`).
    Detail,
}

impl Slot {
    fn name(self) -> &'static str {
        match self {
            Self::Fs => "motion-fs",
            Self::Im => "motion-im",
            Self::ImBg => "motion-imbg",
            Self::Detail => "motion-detail",
        }
    }
}

/// One cached manifest (or a negative "no motion art" result), timestamped
/// for the 30-day disk TTL.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct CacheEntry {
    m: Option<MotionArtwork>,
    at: u64,
}

/// A running decode pipeline for one slot.
struct SlotState {
    /// Cache key the pipeline was built for (`s:<song>` / `a:<album>`) —
    /// a mismatch means the track/detail changed and it must restart.
    key: String,
    url: String,
    /// Wrapper we poll for bus messages (EOS → loop, Error → teardown).
    wrap: gst::Pipeline,
    /// The `playbin3` child (uri + sinks live here).
    play: gst::Element,
    sink: gst_app::AppSink,
    /// Desired run state; track slots follow playback, detail free-runs.
    desired: bool,
    /// Set by render sites each frame, cleared by the next sweep.
    demanded: bool,
    tex: Option<TextureHandle>,
    /// Texture pixel size (cover-crop math at paint time).
    size: [usize; 2],
}

impl SlotState {
    /// Tear the pipeline down cleanly (GStreamer wants NULL before free).
    fn stop(self) {
        let _ = self.wrap.set_state(gst::State::Null);
    }
}

/// Animated-cover state for the whole app (fetch cache + running slots).
pub struct MotionUi {
    /// GStreamer usable (init succeeded). Latched once: when false every
    /// surface silently keeps its static cover.
    ok: bool,
    slots: HashMap<Slot, SlotState>,
    /// Session manifest cache, seeded from disk at startup; `None` = the
    /// catalog has no motion art for this key (also cached, like the old
    /// `motionCache.set(key, null)`).
    art: HashMap<String, CacheEntry>,
    inflight: HashSet<String>,
    tx: Sender<(String, Option<MotionArtwork>)>,
    rx: Receiver<(String, Option<MotionArtwork>)>,
    /// Last song the fetch driver saw (old `motionTrackId`).
    track: Option<String>,
}

impl MotionUi {
    /// Load the manifest cache and initialize GStreamer. Must run on the
    /// main thread (gstreamer-rs thread assertions, eframe's UI thread).
    pub fn new() -> Self {
        let ok = gstreamer::init().is_ok();
        let (tx, rx) = channel();
        Self {
            ok,
            slots: HashMap::new(),
            art: load_disk(),
            inflight: HashSet::new(),
            tx,
            rx,
            track: None,
        }
    }

    /// Manifest url for a surface, square-first with the tall fallback
    /// (old `motionUrlFor`: detail only ever took the square rendition).
    pub fn url(&self, key: &str, slot: Slot) -> Option<String> {
        if !self.ok {
            return None;
        }
        self.art
            .get(key)
            .and_then(|e| e.m.as_ref())
            .and_then(|m| pick_url(m, slot))
    }

    /// Kick the catalog fetch for `key` unless it is cached or already in
    /// flight (old `fetchMotion`, session- and disk-cached). Results land
    /// through the channel and are drained on the next `tick`.
    pub fn ensure_art(
        &mut self,
        ctx: &egui::Context,
        backend: &Backend,
        key: &str,
        song: Option<String>,
        album: Option<String>,
    ) {
        if !self.ok || self.art.contains_key(key) || !self.inflight.insert(key.to_owned()) {
            return;
        }
        let tx = self.tx.clone();
        let state = backend.state.clone();
        let ctx = ctx.clone();
        let key = key.to_owned();
        backend.spawn(async move {
            let m = shell::commands::motion_artwork(&state, song, album)
                .await
                .ok()
                .flatten();
            let _ = tx.send((key, m));
            ctx.request_repaint();
        });
    }

    /// Track-change driver (old `autoFetchMotion`): adopt the new id and,
    /// when animated covers are on, fetch its manifest for the overlay
    /// slots. Toggling the setting never fetches — the URL must already be
    /// in cache, exactly like the old "re-offer when we hold the URL".
    pub fn note_track(
        &mut self,
        ctx: &egui::Context,
        backend: &Backend,
        id: Option<&str>,
        fetch: bool,
    ) {
        let key = id.map(|t| format!("s:{t}"));
        if self.track == key {
            return;
        }
        self.track = key;
        if fetch {
            if let Some(t) = id {
                let key = format!("s:{t}");
                self.ensure_art(ctx, backend, &key, Some(t.to_owned()), None);
            }
        }
    }

    /// Tear a slot's pipeline down (old `stopMotionSlot`).
    fn stop(&mut self, slot: Slot) {
        if let Some(st) = self.slots.remove(&slot) {
            st.stop();
        }
    }

    /// Record that a surface wants `slot` running for `key`. A different
    /// key/url tears any old pipeline down and starts the new one; a
    /// missing url just leaves the static cover in place.
    pub fn demand(&mut self, slot: Slot, key: &str, url: Option<&str>) {
        if !self.ok {
            return;
        }
        let stale = match self.slots.get(&slot) {
            Some(st) => st.key != key || Some(st.url.as_str()) != url,
            None => url.is_some(),
        };
        if stale {
            self.stop(slot);
        }
        if let Some(st) = self.slots.get_mut(&slot) {
            // Same url, still wanted — refresh the sweep flag.
            st.demanded = true;
            return;
        }
        if let Some(u) = url {
            match start(key, u) {
                Ok(mut st) => {
                    st.demanded = true;
                    self.slots.insert(slot, st);
                }
                Err(e) => eprintln!("motion: {slot:?} start failed: {e}"),
            }
        }
    }

    /// The slot's current frame for `key` — `None` while decoding, after a
    /// failure, or when the slot belongs to a different key (a stale frame
    /// from the previous track must never paint).
    pub fn tex(&self, slot: Slot, key: &str) -> Option<TextureHandle> {
        self.slots
            .get(&slot)
            .filter(|st| st.key == key)
            .and_then(|st| st.tex.clone())
    }

    /// Pixel size of the slot's texture, for cover-UV painting.
    pub fn tex_size(&self, slot: Slot, key: &str) -> Option<[usize; 2]> {
        self.slots
            .get(&slot)
            .filter(|st| st.key == key)
            .map(|st| st.size)
    }

    /// Per-frame maintenance, before the UI renders: sweep slots no
    /// surface demanded last frame, land fetched manifests, drain bus
    /// messages (loop on EOS, tear down on error), pull fresh frames into
    /// textures, follow playback state, and keep the repaint cadence.
    pub fn tick(&mut self, ctx: &egui::Context, playing: bool) {
        if !self.ok {
            return;
        }
        // Sweep: render sites demand during the frame that follows this
        // one, so anything undemanded since the last tick is gone (old
        // stopMotionSlot on every mode switch).
        let stale: Vec<Slot> = self
            .slots
            .iter()
            .filter(|(_, st)| !st.demanded)
            .map(|(s, _)| *s)
            .collect();
        for s in stale {
            if let Some(st) = self.slots.remove(&s) {
                st.stop();
            }
        }
        for st in self.slots.values_mut() {
            st.demanded = false;
        }

        // Land finished manifest fetches (and persist the cache).
        let mut landed = false;
        while let Ok((key, m)) = self.rx.try_recv() {
            self.inflight.remove(&key);
            let now = now_ms();
            self.art.insert(key, CacheEntry { m, at: now });
            landed = true;
        }
        if landed {
            save_disk(&self.art);
        }

        let mut dead: Vec<Slot> = Vec::new();
        for (slot, st) in self.slots.iter_mut() {
            // Bus: loop the short at EOS; a stream error drops the slot to
            // its static fallback (and the next frame retries a fresh one
            // only when the surface still demands it).
            if let Some(bus) = st.wrap.bus() {
                while let Some(msg) = bus.pop() {
                    match msg.view() {
                        gstreamer::message::MessageView::Eos(_) => {
                            let _ = st.play.seek_simple(
                                gst::SeekFlags::FLUSH | gst::SeekFlags::KEY_UNIT,
                                gst::ClockTime::ZERO,
                            );
                        }
                        gstreamer::message::MessageView::Error(e) => {
                            eprintln!(
                                "motion: {} stream error: {} | debug: {}",
                                slot.name(),
                                e.error().message(),
                                e.debug().unwrap_or_default()
                            );
                            dead.push(*slot);
                        }
                        _ => {}
                    }
                }
            }
            if dead.contains(slot) {
                continue;
            }

            // Frames: latest wins (intermediate frames are dropped — the
            // paint only ever shows one).
            let mut frame = None;
            while let Some(sample) = st.sink.try_pull_sample(gst::ClockTime::ZERO) {
                if let Some(f) = sample_frame(&sample) {
                    frame = Some(f);
                }
            }
            if frame.is_none() {
                if let Some(sample) = st.sink.try_pull_preroll(gst::ClockTime::ZERO) {
                    frame = sample_frame(&sample);
                }
            }
            if let Some((w, h, rgba)) = frame {
                let (cw, ch, rgba) = square_crop(w, h, &rgba);
                let img = ColorImage::from_rgba_unmultiplied([cw, ch], &rgba);
                st.size = [cw, ch];
                st.tex = Some(ctx.load_texture(slot.name(), img, TextureOptions::default()));
            }

            // Follow playback like the old videos did (detail free-runs).
            let desired = *slot == Slot::Detail || playing;
            if st.desired != desired {
                st.desired = desired;
                let state = if desired {
                    gst::State::Playing
                } else {
                    gst::State::Paused
                };
                let _ = st.wrap.set_state(state);
            }
        }
        for s in dead {
            if let Some(st) = self.slots.remove(&s) {
                st.stop();
            }
        }

        // While anything runs, keep a 15fps-ish repaint cadence (paused
        // slots show their preroll frame and need no repaints).
        if self.slots.values().any(|st| st.desired) {
            ctx.request_repaint_after(Duration::from_millis(FRAME_MS));
        }
    }
}

impl Default for MotionUi {
    fn default() -> Self {
        Self::new()
    }
}

/// Square-first url per slot (old `motionUrlFor` minus the dead `np`).
fn pick_url(m: &MotionArtwork, slot: Slot) -> Option<String> {
    match slot {
        Slot::Detail => m.square_hls.clone(),
        _ => m.square_hls.clone().or_else(|| m.tall_hls.clone()),
    }
}

/// Build one `playbin3` pipeline: decoded video → 512px / 15fps RGBA into
/// a paced appsink, audio discarded. Starts paused; `tick` promotes it.
fn start(key: &str, url: &str) -> Result<SlotState, String> {
    let make = |n: &str| {
        gst::ElementFactory::make(n)
            .build()
            .map_err(|e| format!("{n}: {e}"))
    };
    // videoscale keeps the source aspect while pinning the height, so a
    // square rendition stays square and the tall fallback just crops
    // narrower (`square_crop` finishes the job at delivery).
    let caps_h = gst::Caps::builder("video/x-raw")
        .field("height", FRAME_HEIGHT)
        .build();
    let caps_r = gst::Caps::builder("video/x-raw")
        .field("framerate", gst::Fraction::new(FRAME_RATE.0, FRAME_RATE.1))
        .build();
    let caps_c = gst::Caps::builder("video/x-raw")
        .field("format", "RGBA")
        .build();
    let scale = make("videoscale")?;
    let f_h = make("capsfilter")?;
    f_h.set_property("caps", caps_h);
    let rate = make("videorate")?;
    let f_r = make("capsfilter")?;
    f_r.set_property("caps", caps_r);
    let conv = make("videoconvert")?;
    let f_c = make("capsfilter")?;
    f_c.set_property("caps", caps_c);
    let sink = gst_app::AppSink::builder()
        .name("motion-sink")
        .max_buffers(2)
        .drop(true)
        .sync(true)
        .build();

    let video = gst::Bin::new();
    for el in [&scale, &f_h, &rate, &f_r, &conv, &f_c] {
        video.add(el).map_err(|e| format!("bin add: {e}"))?;
    }
    video.add(&sink).map_err(|e| format!("bin add: {e}"))?;
    scale
        .link(&f_h)
        .and_then(|_| f_h.link(&rate))
        .and_then(|_| rate.link(&f_r))
        .and_then(|_| f_r.link(&conv))
        .and_then(|_| conv.link(&f_c))
        .and_then(|_| f_c.link(&sink))
        .map_err(|e| format!("link: {e}"))?;
    let target = scale
        .static_pad("sink")
        .ok_or_else(|| "videoscale has no sink pad".to_string())?;
    let ghost = gst::GhostPad::with_target(&target).map_err(|e| format!("ghost pad: {e}"))?;
    video.add_pad(&ghost).map_err(|e| format!("add pad: {e}"))?;

    let audio = make("fakesink")?;
    let play = gst::ElementFactory::make("playbin3")
        .property("uri", url)
        .property("video-sink", &video)
        .property("audio-sink", &audio)
        .build()
        .map_err(|e| format!("playbin3: {e}"))?;
    let wrap = gst::Pipeline::new();
    wrap.add(&play).map_err(|e| format!("pipeline add: {e}"))?;
    wrap.set_state(gst::State::Paused)
        .map_err(|e| format!("state: {e}"))?;

    Ok(SlotState {
        key: key.to_owned(),
        url: url.to_owned(),
        wrap,
        play,
        sink,
        desired: false,
        demanded: false,
        tex: None,
        size: [0, 0],
    })
}

/// Read one RGBA frame out of an appsink sample (latest wins upstream).
fn sample_frame(sample: &gst::Sample) -> Option<(usize, usize, Vec<u8>)> {
    let caps = sample.caps()?;
    let structure = caps.structure(0)?;
    let w = structure.get::<i32>("width").ok()? as usize;
    let h = structure.get::<i32>("height").ok()? as usize;
    let buf = sample.buffer()?;
    let map = buf.map_readable().ok()?;
    let need = w.checked_mul(h)?.checked_mul(4)?;
    let data = &map[..];
    if data.len() < need {
        return None;
    }
    Some((w, h, data[..need].to_vec()))
}

/// Center-crop to a square — the old CSS `object-fit: cover` step, done on
/// delivery so every paint site can treat motion like a square still.
fn square_crop(w: usize, h: usize, rgba: &[u8]) -> (usize, usize, Vec<u8>) {
    let s = w.min(h);
    if s == w && s == h {
        return (w, h, rgba.to_vec());
    }
    let ox = (w - s) / 2;
    let oy = (h - s) / 2;
    let mut out = Vec::with_capacity(s * s * 4);
    for y in 0..s {
        let start = ((oy + y) * w + ox) * 4;
        out.extend_from_slice(&rgba[start..start + s * 4]);
    }
    (s, s, out)
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// `~/.config/sonora/motion-urls.json` (old `sonora-motion-urls-v1`).
fn disk_path() -> Option<PathBuf> {
    shell::paths::app_config_dir().map(|d| d.join("motion-urls.json"))
}

fn load_disk() -> HashMap<String, CacheEntry> {
    let Some(p) = disk_path() else {
        return HashMap::new();
    };
    let Ok(raw) = std::fs::read_to_string(p) else {
        return HashMap::new();
    };
    let Ok(map) = serde_json::from_str::<HashMap<String, CacheEntry>>(&raw) else {
        return HashMap::new();
    };
    let now = now_ms();
    map.into_iter().filter(|(_, e)| fresh(e.at, now)).collect()
}

/// A disk entry survives until the old 30-day TTL passes.
fn fresh(at: u64, now: u64) -> bool {
    now.saturating_sub(at) <= TTL_MS
}

fn save_disk(art: &HashMap<String, CacheEntry>) {
    let Some(p) = disk_path() else {
        return;
    };
    let Ok(raw) = serde_json::to_string_pretty(art) else {
        return;
    };
    let _ = std::fs::write(p, raw);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn art(square: Option<&str>, tall: Option<&str>, small: Option<&str>) -> MotionArtwork {
        MotionArtwork {
            square_hls: square.map(str::to_owned),
            tall_hls: tall.map(str::to_owned),
            square_small_hls: small.map(str::to_owned),
        }
    }

    #[test]
    fn url_prefers_square_and_falls_back_to_tall() {
        let both = art(Some("sq"), Some("tall"), Some("sm"));
        assert_eq!(pick_url(&both, Slot::Fs).as_deref(), Some("sq"));
        assert_eq!(pick_url(&both, Slot::Im).as_deref(), Some("sq"));
        assert_eq!(pick_url(&both, Slot::ImBg).as_deref(), Some("sq"));
        let tall_only = art(None, Some("tall"), None);
        assert_eq!(pick_url(&tall_only, Slot::Fs).as_deref(), Some("tall"));
        // The detail head never took the tall rendition (old detail video
        // used `m.square` only).
        assert_eq!(pick_url(&tall_only, Slot::Detail), None);
        assert_eq!(pick_url(&both, Slot::Detail).as_deref(), Some("sq"));
    }

    #[test]
    fn square_crop_centers_wide_and_tall() {
        // 4×2 wide frame (16 bytes/row): crop x∈{1,2} of both rows.
        let wide: Vec<u8> = [
            [
                0, 0, 0, 255, 10, 10, 10, 255, 20, 20, 20, 255, 255, 255, 255, 255,
            ],
            [
                0, 0, 0, 255, 30, 30, 30, 255, 40, 40, 40, 255, 255, 255, 255, 255,
            ],
        ]
        .concat();
        let (w, h, out) = square_crop(4, 2, &wide);
        assert_eq!((w, h), (2, 2));
        assert_eq!(
            out,
            vec![10, 10, 10, 255, 20, 20, 20, 255, 30, 30, 30, 255, 40, 40, 40, 255]
        );

        // Tall: 2×4, crop the center rows.
        let tall: Vec<u8> = [0u8; 8]
            .iter()
            .chain([1u8, 1, 1, 255, 2, 2, 2, 255].iter())
            .chain([3u8, 3, 3, 255, 4, 4, 4, 255].iter())
            .chain([0u8; 8].iter())
            .copied()
            .collect();
        let (w, h, out) = square_crop(2, 4, &tall);
        assert_eq!((w, h), (2, 2));
        assert_eq!(
            out,
            vec![1, 1, 1, 255, 2, 2, 2, 255, 3, 3, 3, 255, 4, 4, 4, 255]
        );
    }

    #[test]
    fn square_crop_passes_through_square() {
        let px = vec![7u8; 2 * 2 * 4];
        let (w, h, out) = square_crop(2, 2, &px);
        assert_eq!((w, h), (2, 2));
        assert_eq!(out, px);
    }

    #[test]
    fn disk_entries_expire_after_30_days() {
        assert!(fresh(1_000, 1_000 + TTL_MS - 1));
        assert!(!fresh(1_000, 1_000 + TTL_MS + 1));
        // Zero timestamps and forward clock skew never panic; a future
        // `at` just reads as not-yet-expired (old `now - at <= TTL`).
        assert!(fresh(0, 0));
        assert!(fresh(TTL_MS + 5, 0));
    }

    #[test]
    fn cache_entry_roundtrips_json() {
        let mut map = HashMap::new();
        map.insert(
            "s:abc".to_owned(),
            CacheEntry {
                m: Some(art(Some("sq"), None, Some("sm"))),
                at: 42,
            },
        );
        map.insert("a:xyz".to_owned(), CacheEntry { m: None, at: 43 });
        let raw = serde_json::to_string(&map).unwrap();
        let back: HashMap<String, CacheEntry> = serde_json::from_str(&raw).unwrap();
        assert_eq!(back["s:abc"].at, 42);
        assert!(back["s:abc"].m.as_ref().unwrap().square_hls.is_some());
        assert!(back["a:xyz"].m.is_none());
    }
}
