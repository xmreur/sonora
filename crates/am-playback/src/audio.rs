//! Local audio engine: symphonia decode → rodio output.
//!
//! Single-track engine: each play call replaces the queue with one song.

use crate::error::{PlaybackError, Result};
use std::io::Cursor;
use std::num::{NonZeroU16, NonZeroU32};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use symphonia::core::codecs::audio::AudioDecoderOptions;
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, SeekMode, SeekTo, TrackType};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::units::{Time, TimeBase};

/// True only while the output callback is alive, the player is unpaused,
/// and either the mixer still has samples or a progressive track is allowed
/// to report playing with an empty queue (not finished yet). A dead stream
/// is silence even if the queue is non-empty.
fn audible(stream_dead: bool, paused: bool, queue_empty: bool, allow_empty: bool) -> bool {
    !stream_dead && !paused && (!queue_empty || allow_empty)
}

#[derive(Debug, Clone, Default)]
pub struct NativeStatus {
    pub playing: bool,
    pub track_id: Option<String>,
    pub position_ms: u64,
    pub duration_ms: u64,
    /// Live loudness 0..1 (chunk RMS for progressive tracks, ±0.75s window
    /// for single-source ones). Drives the dynamic-bg animation speed.
    pub energy: f32,
}

/// Decode audio bytes to interleaved f32 PCM + `(rate, channels, duration_ms)`.
pub fn decode_mem(bytes: &[u8]) -> Result<(Vec<f32>, u32, u16, u64)> {
    let mut pcm: Vec<f32> = Vec::new();
    let (rate, channels, duration_ms) = decode_each(bytes, &mut |buf: &[f32]| {
        pcm.extend_from_slice(buf);
        Ok(())
    })?;
    if pcm.is_empty() {
        return Err(PlaybackError::Decode("decoded zero samples".into()));
    }
    Ok((pcm, rate, channels, duration_ms))
}

/// Decode, invoking `f` with each decoded packet's interleaved f32 samples.
/// One probe + one persistent decoder (unlike the old per-prefix decode);
/// lets prefetch stream straight to the PCM file with no full-size transient.
pub fn decode_each(
    bytes: &[u8],
    f: &mut impl FnMut(&[f32]) -> Result<()>,
) -> Result<(u32, u16, u64)> {
    let (mut format, track_id, rate, channels, mut decoder) = open_decoder(bytes, Some("m4a"))?;

    let mut samples = 0u64;
    while let Ok(Some(packet)) = format.next_packet() {
        if packet.track_id != track_id {
            continue;
        }
        match decoder.decode(&packet) {
            Ok(buf) => {
                let mut tmp: Vec<f32> = Vec::new();
                tmp.resize(buf.samples_interleaved(), 0.0);
                buf.copy_to_slice_interleaved(&mut tmp);
                samples += tmp.len() as u64;
                f(&tmp)?;
            }
            Err(_) => continue,
        }
    }
    let frames = samples / channels.max(1) as u64;
    Ok((rate, channels, frames * 1000 / rate.max(1) as u64))
}

/// Probe `bytes` and build an audio decoder for its default audio track.
/// `ext` is an advisory container hint (`Some("m4a")` for decrypted tracks,
/// `None` to sniff purely by content).
type OpenedDecoder = (
    Box<dyn symphonia::core::formats::FormatReader>,
    u32,
    u32,
    u16,
    Box<dyn symphonia::core::codecs::audio::AudioDecoder>,
);

fn open_decoder(bytes: &[u8], ext: Option<&str>) -> Result<OpenedDecoder> {
    use symphonia::core::codecs::audio::AudioDecoderOptions;
    use symphonia::core::formats::probe::Hint;
    use symphonia::core::formats::{FormatOptions, TrackType};
    use symphonia::core::io::MediaSourceStream;
    use symphonia::core::meta::MetadataOptions;

    let cursor = Cursor::new(bytes.to_vec());
    let mss = MediaSourceStream::new(Box::new(cursor), Default::default());
    let mut hint = Hint::new();
    if let Some(ext) = ext {
        hint.with_extension(ext);
    }
    let format = symphonia::default::get_probe()
        .probe(
            &hint,
            mss,
            FormatOptions::default(),
            MetadataOptions::default(),
        )
        .map_err(|e| PlaybackError::Decode(format!("probe: {e}")))?;
    let track = format
        .default_track(TrackType::Audio)
        .ok_or_else(|| PlaybackError::Decode("no audio track".into()))?;
    let track_id = track.id;
    let audio_params = track
        .codec_params
        .as_ref()
        .and_then(|p| p.audio())
        .ok_or_else(|| PlaybackError::Decode("no audio params".into()))?;
    let rate = audio_params.sample_rate.unwrap_or(44_100);
    let channels = audio_params
        .channels
        .as_ref()
        .map(|c| c.count() as u16)
        .unwrap_or(2);
    let decoder = symphonia::default::get_codecs()
        .make_audio_decoder(audio_params, &AudioDecoderOptions::default())
        .map_err(|e| PlaybackError::Decode(format!("codec: {e}")))?;
    Ok((format, track_id, rate, channels, decoder))
}

/// Streaming decoder for a file on disk. Used by local-file playback so a
/// long FLAC is not loaded into one buffer. The reader stays on the thread
/// that opened it.
pub struct FileDecoder {
    format: Box<dyn symphonia::core::formats::FormatReader>,
    decoder: Box<dyn symphonia::core::codecs::audio::AudioDecoder>,
    track_id: u32,
    rate: u32,
    channels: u16,
    time_base: Option<TimeBase>,
}

impl FileDecoder {
    pub fn open(path: &std::path::Path) -> Result<Self> {
        let file = std::fs::File::open(path).map_err(|e| PlaybackError::Decode(e.to_string()))?;
        let mss = MediaSourceStream::new(Box::new(file), Default::default());
        let mut hint = Hint::new();
        if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
            hint.with_extension(ext);
        }
        let format = symphonia::default::get_probe()
            .probe(
                &hint,
                mss,
                FormatOptions::default(),
                MetadataOptions::default(),
            )
            .map_err(|e| PlaybackError::Decode(format!("probe: {e}")))?;
        let track = format
            .default_track(TrackType::Audio)
            .ok_or_else(|| PlaybackError::Decode("no audio track".into()))?;
        let track_id = track.id;
        let time_base = track.time_base;
        let audio_params = track
            .codec_params
            .as_ref()
            .and_then(|p| p.audio())
            .ok_or_else(|| PlaybackError::Decode("no audio params".into()))?;
        let rate = audio_params.sample_rate.unwrap_or(44_100);
        let channels = audio_params
            .channels
            .as_ref()
            .map(|c| c.count() as u16)
            .unwrap_or(2);
        let decoder = symphonia::default::get_codecs()
            .make_audio_decoder(audio_params, &AudioDecoderOptions::default())
            .map_err(|e| PlaybackError::Decode(format!("codec: {e}")))?;
        Ok(Self {
            format,
            decoder,
            track_id,
            rate,
            channels,
            time_base,
        })
    }

    /// Jump the demuxer. Returns the timestamp actually landed on (a codec
    /// keyframe may sit slightly before the request).
    pub fn seek_ms(&mut self, ms: u64) -> Result<u64> {
        let seeked = self
            .format
            .seek(
                // Coarse lands on the previous keyframe. Accurate then
                // decodes forward to the exact sample, which on a long VBR
                // file scans so far that the seek gives up first.
                SeekMode::Coarse,
                SeekTo::Time {
                    time: Time::from_millis_u64(ms),
                    track_id: Some(self.track_id),
                },
            )
            .map_err(|e| PlaybackError::Decode(format!("seek: {e}")))?;
        self.decoder.reset();
        let actual = self
            .time_base
            .and_then(|tb| tb.calc_time(seeked.actual_ts))
            .map(|t| t.as_millis().max(0) as u64)
            .unwrap_or(ms);
        Ok(actual)
    }

    pub fn rate(&self) -> u32 {
        self.rate
    }

    pub fn channels(&self) -> u16 {
        self.channels
    }

    /// Decode up to `max_frames` audio frames. `Ok(None)` is end of file.
    pub fn next_chunk(&mut self, max_frames: u64) -> Result<Option<Vec<f32>>> {
        let max_frames = max_frames.max(1);
        let mut fresh: Vec<f32> = Vec::new();
        let width = self.channels.max(1) as u64;
        loop {
            let frames = fresh.len() as u64 / width;
            if frames >= max_frames {
                break;
            }
            let packet = match self.format.next_packet() {
                Ok(Some(packet)) => packet,
                Ok(None) | Err(_) => break,
            };
            if packet.track_id != self.track_id {
                continue;
            }
            match self.decoder.decode(&packet) {
                Ok(buf) => {
                    let start = fresh.len();
                    fresh.resize(start + buf.samples_interleaved(), 0.0);
                    buf.copy_to_slice_interleaved(&mut fresh[start..]);
                }
                Err(_) => continue,
            }
        }
        if fresh.is_empty() {
            Ok(None)
        } else {
            Ok(Some(fresh))
        }
    }
}

/// Incremental prefix decoder for the progressive pipeline.
///
/// The old per-chunklet `decode_mem(prefix)` re-decoded the whole prefix
/// from scratch every iteration: O(N²) total decode work per track, and a
/// full-track-size `pcm_full` transient alive on every iteration — that
/// transient is what spiked RSS by ~100MB+ mid-track. This keeps one
/// persistent decoder and feeds it only packets it hasn't seen, so each
/// chunklet decodes O(new). The format reader is re-probed per call (cheap
/// demux-only work); packet order from a growing prefix is deterministic,
/// so skipping `fed` audio packets lands exactly on the new tail.
pub struct PrefixDecoder {
    track_id: u32,
    rate: u32,
    channels: u16,
    decoder: Box<dyn symphonia::core::codecs::audio::AudioDecoder>,
    /// Audio packets already consumed (decoded or skipped after failure).
    /// Failed packets count as consumed — decode failures are deterministic
    /// for fixed bytes, so retrying them would stall the pipeline while
    /// duplicating later chunks (matches `decode_mem`'s omit-on-error).
    fed: usize,
}

impl PrefixDecoder {
    pub fn new(prefix: &[u8]) -> Result<Self> {
        let (_, track_id, rate, channels, decoder) = open_decoder(prefix, None)?;
        Ok(Self {
            track_id,
            rate,
            channels,
            decoder,
            fed: 0,
        })
    }

    pub fn rate(&self) -> u32 {
        self.rate
    }

    pub fn channels(&self) -> u16 {
        self.channels
    }

    /// Decode only the packets beyond those already fed. Returns the fresh
    /// interleaved f32 tail + its duration; empty means "nothing new yet".
    pub fn decode_new(&mut self, prefix: &[u8]) -> Result<(Vec<f32>, u64)> {
        let (mut format, track_id, rate, channels, _) = open_decoder(prefix, None)?;
        if track_id != self.track_id || rate != self.rate || channels != self.channels {
            return Err(PlaybackError::Decode("track changed mid-stream".into()));
        }
        let mut fresh: Vec<f32> = Vec::new();
        // 1-based position of the current audio packet in this probe; packets
        // at positions <= fed were already fed to the decoder earlier.
        let mut pos = 0usize;
        while let Ok(Some(packet)) = format.next_packet() {
            if packet.track_id != self.track_id {
                continue;
            }
            pos += 1;
            if pos <= self.fed {
                continue;
            }
            self.fed = pos;
            match self.decoder.decode(&packet) {
                Ok(buf) => {
                    let start = fresh.len();
                    fresh.resize(start + buf.samples_interleaved(), 0.0);
                    buf.copy_to_slice_interleaved(&mut fresh[start..]);
                }
                Err(_) => continue,
            }
        }
        let frames = fresh.len() as u64 / self.channels.max(1) as u64;
        let duration_ms = frames * 1000 / self.rate.max(1) as u64;
        Ok((fresh, duration_ms))
    }
}

#[derive(Clone)]
enum Current {
    /// Fully-decoded single source (replay / prefetch-prime path). pipelines
    /// can never append to this variant, so it needs no generation. Holds
    /// the buffer (shared with the mixer — no extra copy) for windowed
    /// energy reads in `status`.
    Single {
        id: String,
        duration_ms: u64,
        decoded: std::sync::Arc<Decoded>,
    },
    /// Progressively-appended chunks (streaming decrypt path).
    Prog(ProgState),
}

/// Position reported by the player, in milliseconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SeekOutcome {
    Applied,
    /// Target lies past the decoded span (pipeline hasn't covered it yet).
    BeyondSpan,
}

#[derive(Clone)]
struct ProgState {
    id: String,
    /// Pipeline generation that owns this state. A superseded pipeline
    /// replaying the SAME track id must still be rejected — track-id checks
    /// alone can't tell it apart, and its stale chunks would corrupt both
    /// audio order and position accounting.
    gen: u64,
    chunks: Vec<std::sync::Arc<Decoded>>,
    /// Per-chunk RMS loudness, parallel to `chunks` (computed once at
    /// append; `status` reads the live one for bg animation).
    energies: Vec<f32>,
    /// Cumulative end-ms per chunk (absolute, never truncated).
    cum_ms: Vec<u64>,
    /// Index into `chunks` of the current queue head.
    base: usize,
    /// Sources appended since `base` was set (queue content is
    /// `chunks[base..base + appended]`).
    appended: usize,
    /// Full track length (metadata); falls back to decoded span when unknown.
    total_ms: u64,
    /// Timestamp of `chunks[0]`. Zero for a track played from the start.
    /// A file seek replaces the chunk list and sets this to the landing spot.
    origin_ms: u64,
    /// All chunklets appended.
    done: bool,
    /// Seek landing spot: position counts `offset` into chunk `idx`.
    /// For 200ms after landing, the exact spot is reported (the mixer
    /// needs a tick to switch sources, so the clock may still show the
    /// retired source). Cleared on new playback; ignored once playback
    /// moves past it.
    seek_pos: Option<(usize, u64, Instant)>,
}

impl ProgState {
    fn decoded_span_ms(&self) -> u64 {
        self.cum_ms.last().copied().unwrap_or(0)
    }

    /// Length the UI should show. While chunks are still arriving this is
    /// the metadata length, so the bar is the whole song. Once the file is
    /// fully decoded it is the real span: a catalog duration that runs past
    /// the audio would otherwise leave seek and loop waiting for samples
    /// that do not exist.
    fn report_duration(&self) -> u64 {
        if self.done {
            let span = self.decoded_span_ms();
            if span > 0 {
                return span;
            }
        }
        if self.total_ms > 0 {
            self.total_ms
        } else {
            self.decoded_span_ms()
        }
    }

    fn prefix_ms(&self, idx: usize) -> u64 {
        if idx == 0 {
            self.origin_ms
        } else {
            self.cum_ms[idx - 1]
        }
    }

    /// `(chunk, offset)` for an absolute timestamp. `None` when `ms` is
    /// outside the decoded window (before `origin_ms`, or past the span).
    fn locate(&self, ms: u64) -> Option<(usize, u64)> {
        if self.cum_ms.is_empty() {
            return None;
        }
        // A keyframe may land slightly after the request. Play from the
        // start of the window instead of waiting for audio that was skipped.
        if ms < self.origin_ms {
            return (ms + 3_000 >= self.origin_ms).then_some((0, 0));
        }
        self.cum_ms
            .iter()
            .enumerate()
            .find(|(_, end)| ms < **end)
            .map(|(i, _)| (i, ms.saturating_sub(self.prefix_ms(i))))
    }

    /// Currently-playing chunk from the live queue length. Recently-stopped
    /// predecessors may still be draining, which only lags the index
    /// briefly — it heals as they drop out, with no observation state
    /// that a status gap could strand.
    fn live_index(&self, remaining: usize) -> usize {
        let consumed = self.appended.saturating_sub(remaining);
        (self.base + consumed).min(self.chunks.len().saturating_sub(1))
    }
}

/// Locate `(chunk_index, offset_in_chunk)` for `ms` over cumulative ends.
/// Pure helper, unit-tested.
pub fn locate_chunk(cum_ms: &[u64], ms: u64) -> Option<(usize, u64)> {
    cum_ms
        .iter()
        .enumerate()
        .find(|(_, end)| ms < **end)
        .map(|(i, _)| {
            let base = if i == 0 { 0 } else { cum_ms[i - 1] };
            (i, ms - base)
        })
}

/// Fully decoded track, shared by value via `Arc`.
/// Stored as s16: a 5-min stereo track is ~55MB instead of ~110MB of f32,
/// and 16-bit (-96dB noise floor) is transparent for playback. The mixer
/// pulls f32 via `SharedPcm` (one multiply per sample); the file cache
/// stores the same s16 bytes with no further conversion.
#[derive(Clone)]
pub struct Decoded {
    pub pcm: Vec<i16>,
    pub rate: u32,
    pub channels: u16,
    pub duration_ms: u64,
}

/// f32 ([-1,1]) → s16, clamped and rounded. Decoder overshoots are clamped;
/// the -96dB quantization floor is inaudible under real recordings.
pub fn f32_to_s16(samples: &[f32]) -> Vec<i16> {
    samples
        .iter()
        .map(|v| (v.clamp(-1.0, 1.0) * 32767.0).round() as i16)
        .collect()
}

/// RMS loudness of s16 PCM in 0..1. Music lands ~0.03 (quiet) to ~0.4
/// (slammed masters); silence is 0. One pass; decode dwarfs the cost.
pub fn rms_i16(pcm: &[i16]) -> f32 {
    if pcm.is_empty() {
        return 0.0;
    }
    let sum: f64 = pcm
        .iter()
        .map(|&v| {
            let x = v as f64 / 32768.0;
            x * x
        })
        .sum();
    (sum / pcm.len() as f64).sqrt() as f32
}

/// RMS over a ±0.75s window around `pos_ms` (single-source tracks, which
/// keep no chunk table). Alignment is irrelevant to RMS.
fn window_energy(decoded: &Decoded, pos_ms: u64) -> f32 {
    let ch = decoded.channels.max(1) as usize;
    let rate = decoded.rate.max(1) as usize;
    let center = (pos_ms as usize).saturating_mul(rate).saturating_mul(ch) / 1000;
    let half = rate * ch * 3 / 4;
    let start = center.saturating_sub(half).min(decoded.pcm.len());
    let end = center.saturating_add(half).min(decoded.pcm.len());
    rms_i16(decoded.pcm.get(start..end).unwrap_or(&[]))
}

/// Zero-copy PCM source: shares the decoded buffer via `Arc` instead of
/// cloning it into every queued rodio source. A 3-min stereo track is
/// ~69MB of f32 — the old `pcm[from..].to_vec()` per append kept a second
/// full copy alive in the mixer (cache copy + mixer copy + in-flight
/// chunks), and every seek rebuild duplicated the queue again.
pub struct SharedPcm {
    decoded: std::sync::Arc<Decoded>,
    /// Absolute sample index into `decoded.pcm`.
    pos: usize,
    /// Start of this source's window (seek offsets land here, not at 0).
    start: usize,
    /// Window length in samples.
    len: usize,
}

impl SharedPcm {
    fn suffix(decoded: std::sync::Arc<Decoded>, skip_ms: u64) -> Self {
        let skip = (skip_ms as usize * decoded.rate as usize / 1000) * decoded.channels as usize;
        let start = skip.min(decoded.pcm.len());
        let len = decoded.pcm.len() - start;
        Self {
            decoded,
            pos: start,
            start,
            len,
        }
    }
}

impl Iterator for SharedPcm {
    type Item = f32;

    fn next(&mut self) -> Option<f32> {
        if self.pos >= self.start + self.len {
            return None;
        }
        // s16 → f32 on the pull path (one multiply per sample).
        let v = *self.decoded.pcm.get(self.pos)? as f32 / 32768.0;
        self.pos += 1;
        Some(v)
    }
}

impl rodio::Source for SharedPcm {
    fn current_span_len(&self) -> Option<usize> {
        let end = self.start + self.len;
        if self.pos >= end {
            Some(0)
        } else {
            Some(end - self.pos)
        }
    }

    fn channels(&self) -> NonZeroU16 {
        NonZeroU16::new(self.decoded.channels.max(1)).expect("channels clamped to >= 1")
    }

    fn sample_rate(&self) -> NonZeroU32 {
        NonZeroU32::new(self.decoded.rate.max(1)).expect("rate clamped to >= 1")
    }

    fn total_duration(&self) -> Option<Duration> {
        // Same math as rodio's SamplesBuffer so duration reports match.
        const NANOS_PER_SEC: u64 = 1_000_000_000;
        let duration_ns = NANOS_PER_SEC.checked_mul(self.len as u64)?
            / self.decoded.rate.max(1) as u64
            / self.decoded.channels.max(1) as u64;
        Some(Duration::new(
            duration_ns / NANOS_PER_SEC,
            (duration_ns % NANOS_PER_SEC) as u32,
        ))
    }

    fn try_seek(&mut self, pos: Duration) -> std::result::Result<(), rodio::source::SeekError> {
        // Single-source tracks seek here (same contract SamplesBuffer had:
        // saturate at the window end, stay frame-aligned).
        let frames = (pos.as_secs_f64() * self.decoded.rate.max(1) as f64) as usize;
        let ch = self.decoded.channels.max(1) as usize;
        let rel = frames.saturating_mul(ch).min(self.len);
        self.pos = self.start + rel;
        Ok(())
    }
}

/// Point-in-time retention snapshot: every f32 PCM buffer the engine pins.
/// Logged per track so RSS growth can be attributed (heap-pinned vs
/// allocator/native retention) instead of guessed at.
#[derive(Debug, Default)]
pub struct Retention {
    pub cache_entries: usize,
    pub cache_bytes: usize,
    pub current_chunks: usize,
    pub current_bytes: usize,
    /// Mixer backlog (queued SharedPcm sources share the counted buffers).
    pub pending: usize,
}

/// Return freed heap pages to the OS (glibc arenas otherwise hold freed
/// blocks indefinitely, which reads as ever-growing RSS). No-op when there
/// is nothing to release, and a no-op stub off glibc-Linux.
pub fn trim_allocator() {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    {
        // SAFETY: malloc_trim(0) releases all reclaimable pages; it takes
        // the allocator lock internally and touches no Rust state.
        unsafe {
            libc::malloc_trim(0);
        }
    }
}
pub struct NativeEngine {
    player: rodio::Player,
    // Owns the output stream; dropping it stops audio.
    _device: rodio::stream::MixerDeviceSink,
    current: Mutex<Option<Current>>,
    volume: Mutex<f32>,
    /// Set from the cpal error callback. The device is not rebuilt here;
    /// the next `ensure_engine` drops this engine and opens a new sink.
    stream_dead: Arc<AtomicBool>,
}

impl NativeEngine {
    pub fn new() -> Result<Self> {
        let stream_dead = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&stream_dead);
        let device = rodio::stream::DeviceSinkBuilder::from_default_device()
            .and_then(|builder| {
                builder
                    .with_error_callback(move |err| {
                        eprintln!("audio stream error: {err}");
                        flag.store(true, Ordering::Relaxed);
                    })
                    .open_stream()
            })
            .or_else(|e| {
                eprintln!("audio: sink with error callback failed ({e}); trying fallback");
                rodio::stream::DeviceSinkBuilder::open_default_sink()
            })
            .map_err(|e| PlaybackError::Audio(format!("output: {e}")))?;
        let player = rodio::Player::connect_new(device.mixer());
        // Drop crash leftovers from a previous run; enforce the file cap.
        crate::pcm_cache::sweep();
        Ok(Self {
            player,
            _device: device,
            current: Mutex::new(None),
            volume: Mutex::new(1.0),
            stream_dead,
        })
    }

    /// The output callback has stopped. Later `play` / `append` only edit
    /// rodio flags until the engine is dropped and reopened.
    pub fn stream_dead(&self) -> bool {
        self.stream_dead.load(Ordering::Relaxed)
    }

    /// A decoded source is queued and the output callback is still alive.
    pub fn has_source(&self) -> bool {
        if self.stream_dead() {
            return false;
        }
        self.current_id().is_some()
    }

    fn source_for(decoded: &std::sync::Arc<Decoded>, skip_ms: u64) -> SharedPcm {
        SharedPcm::suffix(decoded.clone(), skip_ms)
    }

    /// Start playback of an already-decoded buffer (PCM-file hit path).
    /// Returns the duration for status reporting.
    pub fn play_decoded(&self, track_id: String, decoded: std::sync::Arc<Decoded>) -> Result<u64> {
        if self.stream_dead() {
            return Err(PlaybackError::Audio("audio stream dead".into()));
        }
        // Own `current` before touching the mixer so a seek aimed at the
        // previous track cannot stop the source this call just queued.
        let mut current = self
            .current
            .lock()
            .map_err(|_| PlaybackError::Audio("lock".into()))?;
        self.player.stop();
        self.player.append(Self::source_for(&decoded, 0));
        self.player.play();
        let duration_ms = decoded.duration_ms;
        *current = Some(Current::Single {
            id: track_id,
            duration_ms,
            decoded,
        });
        Ok(duration_ms)
    }

    /// Halt audio and drop the current track state, freeing its PCM before
    /// a slow file load — so the old and new buffers never overlap in RSS.
    /// Also makes skips go silent instantly instead of trailing ~200ms.
    pub fn drop_current(&self) {
        if let Ok(mut g) = self.current.lock() {
            self.player.stop();
            *g = None;
        }
    }

    /// Start progressive playback with the first decoded chunklet.
    /// `total_ms` is the full track length (0 = unknown, span is shown).
    pub fn play_first(
        &self,
        track_id: String,
        first: std::sync::Arc<Decoded>,
        total_ms: u64,
        gen: u64,
    ) -> Result<()> {
        if self.stream_dead() {
            return Err(PlaybackError::Audio("audio stream dead".into()));
        }
        // Own `current` before `stop` so this queues under the same lock
        // `append_chunk` and `seek` already hold.
        let mut current = self
            .current
            .lock()
            .map_err(|_| PlaybackError::Audio("lock".into()))?;
        self.player.stop();
        self.player.append(Self::source_for(&first, 0));
        self.player.play();
        let duration_ms = first.duration_ms;
        let first_energy = rms_i16(&first.pcm);
        *current = Some(Current::Prog(ProgState {
            id: track_id,
            gen,
            chunks: vec![first],
            energies: vec![first_energy],
            cum_ms: vec![duration_ms],
            base: 0,
            appended: 1,
            total_ms,
            done: false,
            origin_ms: 0,
            seek_pos: Some((0, 0, Instant::now())),
        }));
        Ok(())
    }

    /// Like [`Self::play_first`], but the first chunk begins at `origin_ms`
    /// (a file seek that did not start at zero).
    pub fn play_first_at(
        &self,
        track_id: String,
        first: std::sync::Arc<Decoded>,
        total_ms: u64,
        gen: u64,
        origin_ms: u64,
    ) -> Result<()> {
        self.play_first(track_id, first, total_ms, gen)?;
        if let Ok(mut current) = self.current.lock() {
            if let Some(Current::Prog(p)) = current.as_mut() {
                let dur = p.cum_ms.first().copied().unwrap_or(0);
                p.origin_ms = origin_ms;
                if let Some(end) = p.cum_ms.first_mut() {
                    *end = origin_ms + dur;
                }
            }
        }
        Ok(())
    }

    /// Append a decoded chunklet to the in-progress track. Rejects chunks
    /// for a stale track or a superseded generation (same-track replay):
    /// the generation check above in the pipeline is thread-racy, so the
    /// engine re-verifies ownership under its own lock here.
    pub fn append_chunk(
        &self,
        track_id: &str,
        gen: u64,
        chunk: std::sync::Arc<Decoded>,
    ) -> Result<()> {
        let mut current = self
            .current
            .lock()
            .map_err(|_| PlaybackError::Audio("lock".into()))?;
        match current.as_mut() {
            Some(Current::Prog(p)) if p.id == track_id && p.gen == gen => {
                self.player.append(Self::source_for(&chunk, 0));
                let end = p.decoded_span_ms() + chunk.duration_ms;
                p.energies.push(rms_i16(&chunk.pcm));
                p.chunks.push(chunk);
                p.cum_ms.push(end);
                p.appended += 1;
                Ok(())
            }
            _ => Err(PlaybackError::Audio("stale pipeline chunk dropped".into())),
        }
    }

    /// Mark all chunklets appended (end-of-track detection enabled).
    pub fn finish_chunks(&self, track_id: &str, gen: u64) -> Result<()> {
        let mut current = self
            .current
            .lock()
            .map_err(|_| PlaybackError::Audio("lock".into()))?;
        match current.as_mut() {
            Some(Current::Prog(p)) if p.id == track_id && p.gen == gen => {
                p.done = true;
                Ok(())
            }
            _ => Err(PlaybackError::Audio("stale pipeline finish dropped".into())),
        }
    }

    pub fn current_id(&self) -> Option<String> {
        self.current.lock().ok().and_then(|g| match g.as_ref() {
            Some(Current::Single { id, .. }) => Some(id.clone()),
            Some(Current::Prog(p)) => Some(p.id.clone()),
            None => None,
        })
    }

    /// Decoded span in ms (full duration for single-source tracks).
    pub fn decoded_span_ms(&self) -> u64 {
        self.current
            .lock()
            .ok()
            .and_then(|g| match g.as_ref() {
                Some(Current::Single { duration_ms, .. }) => Some(*duration_ms),
                Some(Current::Prog(p)) => Some(p.decoded_span_ms()),
                None => None,
            })
            .unwrap_or(0)
    }

    /// Decode into the on-disk PCM cache without playing (background
    /// prefetch of upcoming tracks). Streams straight to the file — no
    /// full-size transient. Returns the duration without touching playback.
    /// NOTE: blocking; callers run this on a blocking thread.
    pub fn prime_file(&self, track_id: String, bytes: &[u8]) -> Result<u64> {
        if let Some(d) = crate::pcm_cache::load_valid(&track_id) {
            return Ok(d.duration_ms);
        }
        // Probe for the format first (headers only, cheap) so the streaming
        // writer exists before the first packet decodes.
        let (_, _, rate, channels, _) = open_decoder(bytes, Some("m4a"))?;
        let mut writer = crate::pcm_cache::PartWriter::create(&track_id, 0, rate, channels)?;
        let result = decode_each(bytes, &mut |buf: &[f32]| writer.append(buf));
        match result {
            Ok((_, _, duration_ms)) => {
                writer.finish()?;
                trim_allocator();
                Ok(duration_ms)
            }
            Err(e) => {
                writer.abort();
                Err(e)
            }
        }
    }

    pub fn is_decoded(&self, track_id: &str) -> bool {
        crate::pcm_cache::is_cached(track_id)
    }

    /// Queued-but-unplayed source count (mixer backlog). The decrypt pipeline
    /// waits when this is high so it can't pile a whole track of PCM into
    /// rodio faster than playback drains it.
    pub fn pending(&self) -> usize {
        self.player.len()
    }

    /// Every PCM buffer the engine currently pins (live track only — the
    /// idle cache lives on disk now). The second tuple is (file entries,
    /// file bytes) for telemetry.
    pub fn retention(&self) -> Retention {
        let (cache_entries, cache_bytes) = crate::pcm_cache::stats();
        let (current_chunks, current_bytes) = self
            .current
            .lock()
            .map(|g| match g.as_ref() {
                Some(Current::Single { decoded, .. }) => {
                    (1, decoded.pcm.len() * std::mem::size_of::<i16>())
                }
                Some(Current::Prog(p)) => (
                    p.chunks.len(),
                    p.chunks
                        .iter()
                        .map(|c| c.pcm.len() * std::mem::size_of::<i16>())
                        .sum(),
                ),
                None => (0, 0),
            })
            .unwrap_or_default();
        // Single-state playback pins its PCM through the mixer source plus
        // one shared Arc (same buffer, no copy — for energy reads);
        // ProgState pins the chunklets (one track).
        Retention {
            cache_entries,
            cache_bytes,
            current_chunks,
            current_bytes,
            pending: self.player.len(),
        }
    }

    pub fn pause(&self) -> Result<()> {
        self.player.pause();
        Ok(())
    }

    pub fn resume(&self) -> Result<()> {
        self.player.play();
        Ok(())
    }

    pub fn stop(&self) -> Result<()> {
        self.player.stop();
        *self
            .current
            .lock()
            .map_err(|_| PlaybackError::Audio("lock".into()))? = None;
        Ok(())
    }

    pub fn seek(&self, position_ms: u64) -> Result<SeekOutcome> {
        let mut current = self
            .current
            .lock()
            .map_err(|_| PlaybackError::Audio("lock".into()))?;
        match current.as_mut() {
            None => Ok(SeekOutcome::Applied),
            Some(Current::Single { decoded, .. }) => {
                // Rebuild the source at the new offset. rodio's try_seek
                // reports success when nothing is queued, so a finished or
                // paused buffer used to ignore the seek with no error.
                let was_paused = self.player.is_paused();
                self.player.stop();
                self.player.append(Self::source_for(decoded, position_ms));
                if was_paused {
                    self.player.pause();
                } else {
                    self.player.play();
                }
                Ok(SeekOutcome::Applied)
            }
            Some(Current::Prog(p)) => {
                let mut target = position_ms;
                if p.done {
                    let span = p.decoded_span_ms();
                    if span == 0 {
                        return Ok(SeekOutcome::Applied);
                    }
                    // `locate` treats the span end as outside the audio.
                    // A finished track has nothing more to decode, so a
                    // seek there used to wait until the timeout.
                    if target >= span {
                        target = span - 1;
                    }
                }
                let Some((i, offset)) = p.locate(target) else {
                    return Ok(SeekOutcome::BeyondSpan);
                };
                // Rebuild the queue from the target chunk: stop, then append
                // the tail. Synchronous throughout (no awaits) so a pipeline
                // append cannot interleave mid-rebuild. The queue then holds
                // exactly chunks[i..], recorded via base/appended.
                let was_paused = self.player.is_paused();
                self.player.stop();
                for (j, chunk) in p.chunks.iter().enumerate().skip(i) {
                    let skip = if j == i { offset } else { 0 };
                    self.player.append(Self::source_for(chunk, skip));
                }
                if was_paused {
                    self.player.pause();
                } else {
                    self.player.play();
                }
                p.base = i;
                p.appended = p.chunks.len() - i;
                p.seek_pos = Some((i, offset, Instant::now()));
                Ok(SeekOutcome::Applied)
            }
        }
    }

    pub fn set_volume(&self, level: f32) -> Result<()> {
        let level = level.clamp(0.0, 1.0);
        *self
            .volume
            .lock()
            .map_err(|_| PlaybackError::Audio("lock".into()))? = level;
        self.player.set_volume(level);
        Ok(())
    }

    pub fn volume(&self) -> f32 {
        self.volume.lock().map(|v| *v).unwrap_or(1.0)
    }

    pub fn status(&self) -> NativeStatus {
        let current = match self.current.lock() {
            Ok(g) => g,
            Err(_) => return NativeStatus::default(),
        };
        match current.as_ref() {
            Some(Current::Single {
                id,
                duration_ms,
                decoded,
            }) => {
                let playing = audible(
                    self.stream_dead(),
                    self.player.is_paused(),
                    self.player.empty(),
                    false,
                );
                let position_ms = self.player.get_pos().as_millis() as u64;
                NativeStatus {
                    playing,
                    track_id: Some(id.clone()),
                    position_ms,
                    duration_ms: *duration_ms,
                    energy: window_energy(decoded, position_ms),
                }
            }
            Some(Current::Prog(p)) => {
                let raw = self.player.get_pos().as_millis() as u64;
                let idx = p.live_index(self.player.len());
                let energy = p.energies.get(idx).copied().unwrap_or(0.0);
                let chunk_dur = p.cum_ms[idx] - p.prefix_ms(idx);
                // Landing spot just after seek/play: report it exactly for
                // 200ms (the mixer needs a tick to switch sources, so the
                // clock may still show the retired one).
                if let Some((si, so, at)) = p.seek_pos {
                    if si == idx && at.elapsed() < Duration::from_millis(200) {
                        let playing = audible(
                            self.stream_dead(),
                            self.player.is_paused(),
                            self.player.empty(),
                            !p.done,
                        );
                        return NativeStatus {
                            playing,
                            track_id: Some(p.id.clone()),
                            position_ms: p.prefix_ms(idx) + so,
                            duration_ms: p.report_duration(),
                            energy,
                        };
                    }
                }
                // Seek offset into the current chunk (0 unless landed here).
                let off = match p.seek_pos {
                    Some((si, so, _)) if si == idx => so,
                    _ => 0,
                };
                // Clamp the tail of a stale reading (queue advanced a tick
                // ahead of the clock); normal readings pass through untouched.
                let raw = raw.min(chunk_dur.saturating_sub(off));
                let playing = audible(
                    self.stream_dead(),
                    self.player.is_paused(),
                    self.player.empty(),
                    !p.done,
                );
                NativeStatus {
                    playing,
                    track_id: Some(p.id.clone()),
                    position_ms: p.prefix_ms(idx) + off + raw,
                    duration_ms: p.report_duration(),
                    energy,
                }
            }
            None => NativeStatus::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dead_stream_is_not_playing() {
        assert!(!audible(true, false, false, false));
        assert!(!audible(true, false, true, true));
        assert!(audible(false, false, false, false));
        assert!(audible(false, false, true, true));
        assert!(!audible(false, false, true, false));
        assert!(!audible(false, true, false, true));
    }

    #[test]
    fn chunk_helpers() {
        // locate: boundaries belong to the next chunk.
        let cum = [10_000, 21_000, 33_000];
        assert_eq!(locate_chunk(&cum, 0), Some((0, 0)));
        assert_eq!(locate_chunk(&cum, 9_999), Some((0, 9_999)));
        assert_eq!(locate_chunk(&cum, 10_000), Some((1, 0)));
        assert_eq!(locate_chunk(&cum, 32_999), Some((2, 11_999)));
        assert_eq!(locate_chunk(&cum, 33_000), None);
        assert_eq!(locate_chunk(&[], 5), None);
        // live_index: consumed = appended - remaining, from base.
        let chunk = std::sync::Arc::new(Decoded {
            pcm: vec![],
            rate: 44100,
            channels: 2,
            duration_ms: 0,
        });
        let prog = ProgState {
            id: "t".into(),
            gen: 7,
            chunks: vec![chunk.clone(), chunk.clone(), chunk],
            energies: vec![0.1, 0.2, 0.3],
            cum_ms: vec![10_000, 21_000, 33_000],
            base: 1,
            appended: 2,
            total_ms: 33_000,
            done: false,
            origin_ms: 0,
            seek_pos: None,
        };
        assert_eq!(prog.live_index(2), 1); // nothing consumed yet
        assert_eq!(prog.live_index(1), 2); // one finished
        assert_eq!(prog.live_index(0), 2); // drained: hold last
        assert_eq!(prog.prefix_ms(2), 21_000);
        assert_eq!(prog.locate(10_000), Some((1, 0)));
    }

    #[test]
    fn rms_measures_loudness() {
        assert_eq!(rms_i16(&[]), 0.0);
        assert_eq!(rms_i16(&[0, 0, 0]), 0.0);
        // Full-scale square wave ≈ 1.0.
        let full = vec![32767i16; 1000];
        assert!((rms_i16(&full) - 1.0).abs() < 0.01);
        // Half amplitude ≈ 0.5; quiet ≈ small.
        let half = vec![16384i16; 1000];
        assert!((rms_i16(&half) - 0.5).abs() < 0.01);
        // Windowed energy follows the loud part, clamped to the buffer.
        let mut pcm = vec![0i16; 44100 * 2 * 2];
        for v in pcm.iter_mut().skip(44100 * 2).take(44100) {
            *v = 16384;
        }
        let decoded = Decoded {
            pcm,
            rate: 44100,
            channels: 2,
            duration_ms: 2000,
        };
        assert!(window_energy(&decoded, 1500) > 0.2);
        assert_eq!(window_energy(&decoded, 100), 0.0);
        assert_eq!(
            window_energy(
                &Decoded {
                    pcm: vec![],
                    rate: 44100,
                    channels: 2,
                    duration_ms: 0
                },
                5000
            ),
            0.0
        );
    }

    #[test]
    fn status_empty_by_default() {
        let s = NativeStatus::default();
        assert!(!s.playing);
        assert_eq!(s.position_ms, 0);
    }

    #[test]
    fn decode_rejects_garbage() {
        assert!(decode_mem(b"not audio at all").is_err());
    }

    #[test]
    fn shared_pcm_streams_suffix_without_copy() {
        use rodio::Source;
        let decoded = std::sync::Arc::new(Decoded {
            // 1s stereo @48kHz: frame f holds s16 value f in both channels.
            pcm: (0..48_000).flat_map(|f| [f as i16, f as i16]).collect(),
            rate: 48_000,
            channels: 2,
            duration_ms: 1000,
        });
        // Skip 250ms -> starts at frame 12000.
        let mut src = SharedPcm::suffix(decoded.clone(), 250);
        assert_eq!(src.channels().get(), 2);
        assert_eq!(src.sample_rate().get(), 48_000);
        assert_eq!(src.total_duration(), Some(Duration::from_millis(750)));
        let s = |v: i16| v as f32 / 32768.0;
        let first: Vec<f32> = src.by_ref().take(4).collect();
        assert_eq!(first, vec![s(12000), s(12000), s(12001), s(12001)]);
        // Seek back to 0ms of the window, then past the end (saturates).
        src.try_seek(Duration::from_millis(0)).unwrap();
        let back: Vec<f32> = src.by_ref().take(2).collect();
        assert_eq!(back, vec![s(12000), s(12000)]);
        src.try_seek(Duration::from_secs(3600)).unwrap();
        assert!(src.next().is_none());
        // The source shares the buffer: no sample copy was made.
        assert_eq!(std::sync::Arc::strong_count(&decoded), 2); // local + source
    }

    /// Minimal stereo s16le WAV: 8 frames at 48kHz, sample value = index*1000.
    fn tiny_wav() -> Vec<u8> {
        wav_bytes(8)
    }

    /// Longer stereo s16le WAV (4800 frames — the demuxer splits these into
    /// multiple packets, which is what exercises the skip logic).
    fn multi_wav() -> Vec<u8> {
        wav_bytes(4800)
    }

    fn wav_bytes(frames: usize) -> Vec<u8> {
        let samples: Vec<i16> = (0..frames as i16 * 2)
            .map(|i| ((i as i32 * 7) % 30000) as i16)
            .collect();
        let data_len = samples.len() * 2;
        let mut v = Vec::new();
        v.extend_from_slice(b"RIFF");
        v.extend_from_slice(&((36 + data_len) as u32).to_le_bytes());
        v.extend_from_slice(b"WAVEfmt ");
        v.extend_from_slice(&16u32.to_le_bytes());
        v.extend_from_slice(&1u16.to_le_bytes()); // PCM
        v.extend_from_slice(&2u16.to_le_bytes()); // stereo
        v.extend_from_slice(&48_000u32.to_le_bytes());
        v.extend_from_slice(&(48_000u32 * 2 * 2).to_le_bytes()); // byte rate
        v.extend_from_slice(&4u16.to_le_bytes()); // block align
        v.extend_from_slice(&16u16.to_le_bytes()); // bits
        v.extend_from_slice(b"data");
        v.extend_from_slice(&(data_len as u32).to_le_bytes());
        for s in samples {
            v.extend_from_slice(&s.to_le_bytes());
        }
        v
    }

    #[test]
    fn prefix_decoder_rejects_garbage() {
        assert!(PrefixDecoder::new(b"not audio at all").is_err());
    }

    #[test]
    fn prefix_decoder_feeds_each_packet_once() {
        let wav = tiny_wav();
        let mut d = PrefixDecoder::new(&wav).expect("wav probes");
        assert_eq!((d.rate(), d.channels()), (48_000, 2));
        // First call yields the whole content with exact sample values.
        let (fresh, _) = d.decode_new(&wav).expect("decodes");
        assert_eq!(fresh.len(), 16);
        for (i, v) in fresh.iter().enumerate() {
            let want = ((i as i32 * 7) % 30000) as f32 / 32768.0;
            assert!((v - want).abs() < 1e-6, "sample {i}: {v} != {want}");
        }
        // Same prefix again: nothing new (skip logic lands on the tail).
        let (fresh2, dur2) = d.decode_new(&wav).expect("re-probe ok");
        assert!(fresh2.is_empty());
        assert_eq!(dur2, 0);
    }

    #[test]
    fn prefix_decoder_split_feed_matches_oneshot() {
        // Multi-packet content in one call must equal the one-shot decode
        // exactly. (A two-counter skip bug here once dropped every other
        // packet — songs played at 2x speed.)
        let wav = multi_wav();
        let (all, _, _, _) = decode_mem(&wav).expect("one-shot wav decode");
        assert!(all.len() > 1000);
        let mut d = PrefixDecoder::new(&wav).expect("probes");
        let (fresh, _) = d.decode_new(&wav).expect("decodes");
        assert_eq!(fresh.len(), all.len());
        for (i, (a, b)) in fresh.iter().zip(all.iter()).enumerate() {
            assert!((a - b).abs() < 1e-6, "sample {i}: {a} != {b}");
        }
        // Re-probe of the same input yields nothing new.
        let (again, _) = d.decode_new(&wav).expect("re-probe ok");
        assert!(again.is_empty());
    }

    #[test]
    fn decode_each_matches_decode_mem() {
        let wav = multi_wav();
        let (all, rate, ch, dur) = decode_mem(&wav).expect("one-shot");
        let mut acc = Vec::new();
        let mut calls = 0usize;
        // Streaming callback sees the same samples in order (packets may be
        // grouped differently than the accumulating loop — only the concat
        // must match).
        let (r2, c2, d2) = decode_each(&wav, &mut |buf: &[f32]| {
            calls += 1;
            acc.extend_from_slice(buf);
            Ok(())
        })
        .expect("streaming");
        assert!(calls > 1, "multi-packet input must stream in pieces");
        assert_eq!((r2, c2, d2), (rate, ch, dur));
        assert_eq!(acc, all);
    }

    #[test]
    fn f32_to_s16_clamps_and_rounds() {
        assert_eq!(f32_to_s16(&[0.0, 1.0, -1.0]), vec![0, 32767, -32767]);
        // Decoder overshoots clamp instead of wrapping.
        assert_eq!(f32_to_s16(&[1.5, -2.0]), vec![32767, -32767]);
        // Rounds to nearest, error bounded by half an LSB.
        let v = f32_to_s16(&[0.5]);
        assert_eq!(v, vec![16384]);
        assert!((v[0] as f32 / 32768.0 - 0.5).abs() <= 0.5 / 32768.0);
    }

    #[test]
    fn trim_allocator_does_not_panic() {
        // Releases reclaimable heap pages (glibc-Linux) or no-ops elsewhere.
        trim_allocator();
    }
}
