//! Animated covers (Apple Motion) — old `<video>` HLS loops → GStreamer.
//!
//! Some albums ship motion renditions from the catalog's `editorialVideo`
//! field: short muted HLS loops. The old web build played one `<video>`
//! element per visible surface — fullscreen cover, immersive cover,
//! immersive background, album-detail head (the bar tile was deliberately
//! static: one less decoder for a 200px surface) — with the static cover
//! underneath showing through every failure.
//!
//! This port keeps that shape: one `playbin3` pipeline per distinct HLS
//! URL (fullscreen, immersive cover, and immersive background share it),
//! hardware decode where available, capped at 512px / 15fps into an
//! `appsink`, drained by the frame tick into one egui texture. Render sites
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
use gstreamer_video as gst_video;

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

/// One cached manifest (or a negative "no motion art" result), timestamped
/// for the 30-day disk TTL.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct CacheEntry {
    m: Option<MotionArtwork>,
    at: u64,
}

/// A surface that wants a frame. Several slots showing the same URL share
/// one [`Decoder`] instead of each running a playbin.
struct SlotView {
    /// Cache key (`s:<song>` / `a:<album>`). A mismatch restarts the slot.
    key: String,
    url: String,
    /// Set by render sites each frame, cleared by the next sweep.
    demanded: bool,
    /// Album-detail head keeps looping while the track is paused.
    free_run: bool,
}

/// One decode of one HLS URL. Slots that show it clone `tex`.
struct Decoder {
    /// Wrapper we poll for bus messages (EOS → loop, Error → teardown).
    wrap: gst::Pipeline,
    /// The `playbin3` child (uri + sinks live here).
    play: gst::Element,
    sink: gst_app::AppSink,
    /// Last applied run state, so a steady frame does not call `set_state`.
    playing: bool,
    tex: Option<TextureHandle>,
    /// Texture pixel size (cover-crop math at paint time).
    size: [usize; 2],
    /// Reused RGBA scratch. Frames no longer allocate a fresh buffer each tick.
    scratch: Vec<u8>,
}

impl Decoder {
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
    slots: HashMap<Slot, SlotView>,
    /// Running pipelines, keyed by the HLS URL.
    decoders: HashMap<String, Decoder>,
    /// Session manifest cache, seeded from disk at startup; `None` = the
    /// catalog has no motion art for this key (also cached, like the old
    /// `motionCache.set(key, null)`).
    art: HashMap<String, CacheEntry>,
    inflight: HashSet<String>,
    tx: Sender<(String, Option<MotionArtwork>)>,
    rx: Receiver<(String, Option<MotionArtwork>)>,
    /// Last song the fetch driver saw (old `motionTrackId`).
    track: Option<String>,
    /// A UI frame ran since the last tick. Logic-only ticks (minimized or
    /// occluded) must not treat "not demanded yet this frame" as "gone".
    ui_since_tick: bool,
}

impl MotionUi {
    /// Load the manifest cache and initialize GStreamer. Must run on the
    /// main thread (gstreamer-rs thread assertions, eframe's UI thread).
    pub fn new() -> Self {
        // Packaged installs (Windows setup, macOS .app) ship the runtime;
        // point GStreamer at their plugin dirs before the first init.
        crate::gst_bundle::ensure_plugin_path();
        let ok = gstreamer::init().is_ok();
        let (tx, rx) = channel();
        Self {
            ok,
            slots: HashMap::new(),
            decoders: HashMap::new(),
            art: load_disk(),
            inflight: HashSet::new(),
            tx,
            rx,
            track: None,
            ui_since_tick: false,
        }
    }

    /// The UI pass is about to demand slots. `tick` only sweeps after this.
    pub fn note_ui_frame(&mut self) {
        self.ui_since_tick = true;
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

    /// Drop a slot. The shared decoder goes with it only when nothing else
    /// is still showing that URL.
    fn release(&mut self, slot: Slot) {
        let Some(view) = self.slots.remove(&slot) else {
            return;
        };
        if self.slots.values().any(|v| v.url == view.url) {
            return;
        }
        if let Some(dec) = self.decoders.remove(&view.url) {
            dec.stop();
        }
    }

    /// Record that a surface wants `slot` running for `key`. A different
    /// key/url detaches the slot; a missing url leaves the static cover.
    /// Slots that ask for the same URL share one pipeline.
    pub fn demand(&mut self, slot: Slot, key: &str, url: Option<&str>) {
        if !self.ok {
            return;
        }
        let Some(url) = url else {
            self.release(slot);
            return;
        };
        if let Some(view) = self.slots.get_mut(&slot) {
            if view.key == key && view.url == url {
                view.demanded = true;
                return;
            }
        }
        self.release(slot);
        if !self.decoders.contains_key(url) {
            match start(url) {
                Ok(dec) => {
                    self.decoders.insert(url.to_owned(), dec);
                }
                Err(e) => {
                    eprintln!("motion: {slot:?} start failed: {e}");
                    return;
                }
            }
        }
        self.slots.insert(
            slot,
            SlotView {
                key: key.to_owned(),
                url: url.to_owned(),
                demanded: true,
                free_run: slot == Slot::Detail,
            },
        );
    }

    /// The slot's current frame for `key` — `None` while decoding, after a
    /// failure, or when the slot belongs to a different key (a stale frame
    /// from the previous track must never paint).
    pub fn tex(&self, slot: Slot, key: &str) -> Option<TextureHandle> {
        let view = self.slots.get(&slot).filter(|st| st.key == key)?;
        self.decoders.get(&view.url).and_then(|dec| dec.tex.clone())
    }

    /// Pixel size of the slot's texture, for cover-UV painting.
    pub fn tex_size(&self, slot: Slot, key: &str) -> Option<[usize; 2]> {
        let view = self.slots.get(&slot).filter(|st| st.key == key)?;
        self.decoders.get(&view.url).map(|dec| dec.size)
    }

    /// Per-frame maintenance, before the UI renders: sweep slots no
    /// surface demanded last frame, land fetched manifests, drain bus
    /// messages (loop on EOS, tear down on error), pull fresh frames into
    /// textures, follow playback state, and keep the repaint cadence.
    pub fn tick(&mut self, ctx: &egui::Context, playing: bool) {
        if !self.ok {
            return;
        }
        // Sweep only after a UI frame. A logic-only tick (window hidden)
        // never demands, and clearing the flag there would delete the
        // pipeline before the next paint.
        if self.ui_since_tick {
            let stale: Vec<Slot> = self
                .slots
                .iter()
                .filter(|(_, st)| !st.demanded)
                .map(|(s, _)| *s)
                .collect();
            for s in stale {
                self.release(s);
            }
            for st in self.slots.values_mut() {
                st.demanded = false;
            }
            self.ui_since_tick = false;
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

        let mut dead: Vec<String> = Vec::new();
        for (url, dec) in self.decoders.iter_mut() {
            // Bus: loop the short at EOS; a stream error drops the decoder
            // to the static fallback (the next demand builds a fresh one).
            if let Some(bus) = dec.wrap.bus() {
                while let Some(msg) = bus.pop() {
                    match msg.view() {
                        gstreamer::message::MessageView::Eos(_) => {
                            let _ = dec.play.seek_simple(
                                gst::SeekFlags::FLUSH | gst::SeekFlags::KEY_UNIT,
                                gst::ClockTime::ZERO,
                            );
                        }
                        gstreamer::message::MessageView::Error(e) => {
                            eprintln!(
                                "motion: stream error: {} | debug: {}",
                                e.error().message(),
                                e.debug().unwrap_or_default()
                            );
                            dead.push(url.clone());
                        }
                        _ => {}
                    }
                }
            }
            if dead.iter().any(|d| d == url) {
                continue;
            }

            // Latest sample only. Earlier ones are dropped without a copy;
            // the paint shows a single frame.
            let mut last = None;
            while let Some(sample) = dec.sink.try_pull_sample(gst::ClockTime::ZERO) {
                last = Some(sample);
            }
            if last.is_none() {
                last = dec.sink.try_pull_preroll(gst::ClockTime::ZERO);
            }
            if let Some(sample) = last {
                if let Some((w, h)) = copy_frame(&sample, &mut dec.scratch) {
                    let (cw, ch, owned) = if w == h {
                        (w, h, None)
                    } else {
                        let (cw, ch, cropped) = square_crop(w, h, &dec.scratch);
                        (cw, ch, Some(cropped))
                    };
                    let rgba = owned.as_deref().unwrap_or(dec.scratch.as_slice());
                    let img = ColorImage::from_rgba_unmultiplied([cw, ch], rgba);
                    dec.size = [cw, ch];
                    if let Some(tex) = dec.tex.as_mut() {
                        tex.set(img, TextureOptions::default());
                    } else {
                        dec.tex = Some(ctx.load_texture("motion", img, TextureOptions::default()));
                    }
                }
            }

            // Follow playback like the old videos did. Detail free-runs,
            // so a shared URL plays when any attached slot wants motion.
            let desired = self
                .slots
                .values()
                .any(|view| view.url == *url && (view.free_run || playing));
            if dec.playing != desired {
                dec.playing = desired;
                let state = if desired {
                    gst::State::Playing
                } else {
                    gst::State::Paused
                };
                let _ = dec.wrap.set_state(state);
            }
        }
        for url in dead {
            if let Some(dec) = self.decoders.remove(&url) {
                dec.stop();
            }
            self.slots.retain(|_, view| view.url != url);
        }

        // While anything runs, keep a 15fps-ish repaint cadence (paused
        // slots show their preroll frame and need no repaints).
        if self.decoders.values().any(|dec| dec.playing) {
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
fn start(url: &str) -> Result<Decoder, String> {
    let make = |n: &str| {
        gst::ElementFactory::make(n)
            .build()
            .map_err(|e| format!("{n}: {e}"))
    };
    // Stills are Apple's `{w}x{h}bb` center square. Tall motion (3:4) must
    // be cropped the same way before scale, or videoscale letterboxes it
    // and the cover looks narrower than the still.
    let crop = gst::ElementFactory::make("aspectratiocrop")
        .property("aspect-ratio", gst::Fraction::new(1, 1))
        .build()
        .map_err(|e| format!("aspectratiocrop: {e}"))?;
    let caps_sq = gst::Caps::builder("video/x-raw")
        .field("width", FRAME_HEIGHT)
        .field("height", FRAME_HEIGHT)
        .field("pixel-aspect-ratio", gst::Fraction::new(1, 1))
        .build();
    let caps_r = gst::Caps::builder("video/x-raw")
        .field("framerate", gst::Fraction::new(FRAME_RATE.0, FRAME_RATE.1))
        .build();
    let caps_c = gst::Caps::builder("video/x-raw")
        .field("format", "RGBA")
        .build();
    let scale = make("videoscale")?;
    scale.set_property("add-borders", false);
    let f_sq = make("capsfilter")?;
    f_sq.set_property("caps", caps_sq);
    let rate = make("videorate")?;
    let f_r = make("capsfilter")?;
    f_r.set_property("caps", caps_r);
    let conv = make("videoconvert")?;
    let f_c = make("capsfilter")?;
    f_c.set_property("caps", caps_c);
    let sink = gst_app::AppSink::builder()
        .name("motion-sink")
        .max_buffers(1)
        .drop(true)
        .enable_last_sample(false)
        .sync(true)
        .build();

    let video = gst::Bin::new();
    for el in [&crop, &scale, &f_sq, &rate, &f_r, &conv, &f_c] {
        video.add(el).map_err(|e| format!("bin add: {e}"))?;
    }
    video.add(&sink).map_err(|e| format!("bin add: {e}"))?;
    crop.link(&scale)
        .and_then(|_| scale.link(&f_sq))
        .and_then(|_| f_sq.link(&rate))
        .and_then(|_| rate.link(&f_r))
        .and_then(|_| f_r.link(&conv))
        .and_then(|_| conv.link(&f_c))
        .and_then(|_| f_c.link(&sink))
        .map_err(|e| format!("link: {e}"))?;
    let target = crop
        .static_pad("sink")
        .ok_or_else(|| "aspectratiocrop has no sink pad".to_string())?;
    let ghost = gst::GhostPad::with_target(&target).map_err(|e| format!("ghost pad: {e}"))?;
    video.add_pad(&ghost).map_err(|e| format!("add pad: {e}"))?;

    let audio = make("fakesink")?;
    audio.set_property("sync", false);
    audio.set_property("async", false);
    let play = gst::ElementFactory::make("playbin3")
        .property("uri", url)
        .property("video-sink", &video)
        .property("audio-sink", &audio)
        .build()
        .map_err(|e| format!("playbin3: {e}"))?;
    // Video only: the cover is muted, and an audio decoder plus its queue
    // is pure overhead. Missing properties are skipped so an older
    // playbin3 still starts.
    set_str(&play, "flags", "video");
    set_str(&play, "buffer-duration", "1000000000");
    set_str(&play, "buffer-size", "1048576");
    // element-setup runs on the streaming thread, so the handler has to be
    // the thread-safe connect, not connect_local.
    play.connect("element-setup", false, |values| {
        let element = values.get(1)?.get::<gst::Element>().ok()?;
        limit_pipeline_memory(&element);
        None
    });
    let wrap = gst::Pipeline::new();
    wrap.add(&play).map_err(|e| format!("pipeline add: {e}"))?;
    wrap.set_state(gst::State::Paused)
        .map_err(|e| format!("state: {e}"))?;

    Ok(Decoder {
        wrap,
        play,
        sink,
        playing: false,
        tex: None,
        size: [0, 0],
        scratch: Vec::new(),
    })
}

/// Set a GObject property from a string when the element actually has it.
fn set_str(element: &gst::Element, name: &str, value: &str) {
    if element.find_property(name).is_some() {
        element.set_property_from_str(name, value);
    }
}

/// Shrink the queues playbin inserts. Adaptive demuxers keep 30s of
/// segments by default; a cover loop only needs a short lookahead.
fn limit_pipeline_memory(element: &gst::Element) {
    if element.factory().is_some_and(|f| f.name() == "multiqueue") {
        set_str(element, "max-size-buffers", "4");
        set_str(element, "max-size-bytes", "2097152");
        set_str(element, "max-size-time", "1000000000");
    }
    set_str(element, "max-buffering-time", "3000000000");
}

/// Copy one RGBA frame into `rgba`, reusing its allocation.
fn copy_frame(sample: &gst::Sample, rgba: &mut Vec<u8>) -> Option<(usize, usize)> {
    let caps = sample.caps()?;
    let structure = caps.structure(0)?;
    let w = structure.get::<i32>("width").ok()? as usize;
    let h = structure.get::<i32>("height").ok()? as usize;
    let buf = sample.buffer()?;
    let map = buf.map_readable().ok()?;
    let row = w.checked_mul(4)?;
    if h == 0 || map.len() < h * row {
        return None;
    }
    // Row stride comes from the video meta. Guessing it from the buffer
    // length shears the frame whenever the allocator adds tail padding.
    let stride = buf
        .meta::<gst_video::VideoMeta>()
        .and_then(|m| m.stride().first().copied())
        .map(|s| s.unsigned_abs() as usize)
        .filter(|s| *s >= row && h * *s <= map.len())
        .unwrap_or(row);
    rgba.clear();
    rgba.reserve(w * h * 4);
    for y in 0..h {
        let start = y * stride;
        let end = start + row;
        if end > map.len() {
            rgba.clear();
            return None;
        }
        rgba.extend_from_slice(&map[start..end]);
    }
    Some((w, h))
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
