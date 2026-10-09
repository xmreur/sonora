//! Local audio engine: symphonia decode → rodio output.
//!
//! Single-track engine: each play call replaces the queue with one song.

use crate::error::{PlaybackError, Result};
use std::io::Cursor;
use std::num::{NonZeroU16, NonZeroU32};
use std::sync::Mutex;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Default)]
pub struct NativeStatus {
    pub playing: bool,
    pub track_id: Option<String>,
    pub position_ms: u64,
    pub duration_ms: u64,
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
    /// can never append to this variant, so it needs no generation.
    Single { id: String, duration_ms: u64 },
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
    /// Cumulative end-ms per chunk (absolute, never truncated).
    cum_ms: Vec<u64>,
    /// Index into `chunks` of the current queue head.
    base: usize,
    /// Sources appended since `base` was set (queue content is
    /// `chunks[base..base + appended]`).
    appended: usize,
    /// Full track length (metadata); falls back to decoded span when unknown.
    total_ms: u64,
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

    fn prefix_ms(&self, idx: usize) -> u64 {
        if idx == 0 {
            0
        } else {
            self.cum_ms[idx - 1]
        }
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
        let skip =
            (skip_ms as usize * decoded.rate as usize / 1000) * decoded.channels as usize;
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
}

impl NativeEngine {
    pub fn new() -> Result<Self> {
        let device = rodio::stream::DeviceSinkBuilder::open_default_sink()
            .map_err(|e| PlaybackError::Audio(format!("output: {e}")))?;
        let player = rodio::Player::connect_new(device.mixer());
        // Drop crash leftovers from a previous run; enforce the file cap.
        crate::pcm_cache::sweep();
        Ok(Self {
            player,
            _device: device,
            current: Mutex::new(None),
            volume: Mutex::new(1.0),
        })
    }

    fn source_for(decoded: &std::sync::Arc<Decoded>, skip_ms: u64) -> SharedPcm {
        SharedPcm::suffix(decoded.clone(), skip_ms)
    }

    /// Start playback of an already-decoded buffer (PCM-file hit path).
    /// Returns the duration for status reporting.
    pub fn play_decoded(&self, track_id: String, decoded: std::sync::Arc<Decoded>) -> u64 {
        self.player.stop();
        self.player.append(Self::source_for(&decoded, 0));
        self.player.play();
        let duration_ms = decoded.duration_ms;
        if let Ok(mut g) = self.current.lock() {
            *g = Some(Current::Single {
                id: track_id,
                duration_ms,
            });
        }
        duration_ms
    }

    /// Halt audio and drop the current track state, freeing its PCM before
    /// a slow file load — so the old and new buffers never overlap in RSS.
    /// Also makes skips go silent instantly instead of trailing ~200ms.
    pub fn drop_current(&self) {
        self.player.stop();
        if let Ok(mut g) = self.current.lock() {
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
        self.player.stop();
        self.player
            .append(Self::source_for(&first, 0));
        self.player.play();
        let duration_ms = first.duration_ms;
        *self
            .current
            .lock()
            .map_err(|_| PlaybackError::Audio("lock".into()))? = Some(Current::Prog(ProgState {
            id: track_id,
            gen,
            chunks: vec![first],
            cum_ms: vec![duration_ms],
            base: 0,
            appended: 1,
            total_ms,
            done: false,
            seek_pos: Some((0, 0, Instant::now())),
        }));
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
                Some(Current::Single { .. }) => (0, 0),
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
        // Single-state playback pins its PCM through the mixer source only
        // (one track); ProgState pins the chunklets (one track).
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
            Some(Current::Single { .. }) => {
                // SharedPcm is seekable per-source; the player forwards it.
                let _ = self.player.try_seek(Duration::from_millis(position_ms));
                Ok(SeekOutcome::Applied)
            }
            Some(Current::Prog(p)) => {
                let Some((i, offset)) = locate_chunk(&p.cum_ms, position_ms) else {
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
            Some(Current::Single { id, duration_ms, .. }) => {
                let playing = !self.player.is_paused() && !self.player.empty();
                NativeStatus {
                    playing,
                    track_id: Some(id.clone()),
                    position_ms: self.player.get_pos().as_millis() as u64,
                    duration_ms: *duration_ms,
                }
            }
            Some(Current::Prog(p)) => {
                let raw = self.player.get_pos().as_millis() as u64;
                let idx = p.live_index(self.player.len());
                let chunk_dur = p.cum_ms[idx] - p.prefix_ms(idx);
                // Landing spot just after seek/play: report it exactly for
                // 200ms (the mixer needs a tick to switch sources, so the
                // clock may still show the retired one).
                if let Some((si, so, at)) = p.seek_pos {
                    if si == idx && at.elapsed() < Duration::from_millis(200) {
                        let playing = !self.player.is_paused() && (!self.player.empty() || !p.done);
                        return NativeStatus {
                            playing,
                            track_id: Some(p.id.clone()),
                            position_ms: p.prefix_ms(idx) + so,
                            duration_ms: if p.total_ms > 0 {
                                p.total_ms
                            } else {
                                p.decoded_span_ms()
                            },
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
                let playing = !self.player.is_paused() && (!self.player.empty() || !p.done);
                NativeStatus {
                    playing,
                    track_id: Some(p.id.clone()),
                    position_ms: p.prefix_ms(idx) + off + raw,
                    duration_ms: if p.total_ms > 0 {
                        p.total_ms
                    } else {
                        p.decoded_span_ms()
                    },
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
            cum_ms: vec![10_000, 21_000, 33_000],
            base: 1,
            appended: 2,
            total_ms: 33_000,
            done: false,
            seek_pos: None,
        };
        assert_eq!(prog.live_index(2), 1); // nothing consumed yet
        assert_eq!(prog.live_index(1), 2); // one finished
        assert_eq!(prog.live_index(0), 2); // drained: hold last
        assert_eq!(prog.prefix_ms(2), 21_000);
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
