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
    use symphonia::core::codecs::audio::AudioDecoderOptions;
    use symphonia::core::formats::probe::Hint;
    use symphonia::core::formats::{FormatOptions, TrackType};
    use symphonia::core::io::MediaSourceStream;
    use symphonia::core::meta::MetadataOptions;

    let cursor = Cursor::new(bytes.to_vec());
    let mss = MediaSourceStream::new(Box::new(cursor), Default::default());
    let mut hint = Hint::new();
    hint.with_extension("m4a");
    let mut format = symphonia::default::get_probe()
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
    let mut decoder = symphonia::default::get_codecs()
        .make_audio_decoder(audio_params, &AudioDecoderOptions::default())
        .map_err(|e| PlaybackError::Decode(format!("codec: {e}")))?;

    let mut pcm: Vec<f32> = Vec::new();
    while let Ok(Some(packet)) = format.next_packet() {
        if packet.track_id != track_id {
            continue;
        }
        match decoder.decode(&packet) {
            Ok(buf) => {
                let start = pcm.len();
                pcm.resize(start + buf.samples_interleaved(), 0.0);
                buf.copy_to_slice_interleaved(&mut pcm[start..]);
            }
            Err(_) => continue,
        }
    }
    if pcm.is_empty() {
        return Err(PlaybackError::Decode("decoded zero samples".into()));
    }
    let frames = pcm.len() as u64 / channels.max(1) as u64;
    let duration_ms = frames * 1000 / rate.max(1) as u64;
    Ok((pcm, rate, channels, duration_ms))
}

#[derive(Clone)]
enum Current {
    /// Fully-decoded single source (replay / prefetch-prime path).
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

/// Fully decoded track, shared by value via `Arc` (a 4-minute stereo track
/// is ~40MB of f32 — cloning it per play/seek would stall the UI thread).
#[derive(Clone)]
pub struct Decoded {
    pub pcm: Vec<f32>,
    pub rate: u32,
    pub channels: u16,
    pub duration_ms: u64,
}

/// Count-capped LRU of decoded tracks so replay / back / next-across-a-small
/// queue skips the symphonia decode entirely (decrypt + download are already
/// covered by the on-disk m4a cache in `cache.rs`).
pub struct DecodedCache {
    cap: usize,
    order: std::collections::VecDeque<String>,
    map: std::collections::HashMap<String, std::sync::Arc<Decoded>>,
}

impl DecodedCache {
    pub fn new(cap: usize) -> Self {
        Self {
            cap: cap.max(1),
            order: std::collections::VecDeque::new(),
            map: std::collections::HashMap::new(),
        }
    }

    pub fn get(&mut self, id: &str) -> Option<std::sync::Arc<Decoded>> {
        let hit = self.map.get(id).cloned();
        if hit.is_some() {
            self.order.retain(|k| k != id);
            self.order.push_back(id.to_string());
        }
        hit
    }

    pub fn put(&mut self, id: String, decoded: std::sync::Arc<Decoded>) {
        self.order.retain(|k| k != &id);
        self.order.push_back(id.clone());
        self.map.insert(id, decoded);
        while self.order.len() > self.cap {
            if let Some(old) = self.order.pop_front() {
                self.map.remove(&old);
            }
        }
    }
}

/// Recently decoded tracks kept in memory (default 3 ≈ 120MB worst case).
pub const DECODED_CACHE_SIZE: usize = 3;

pub struct NativeEngine {
    player: rodio::Player,
    // Owns the output stream; dropping it stops audio.
    _device: rodio::stream::MixerDeviceSink,
    current: Mutex<Option<Current>>,
    decoded: Mutex<DecodedCache>,
    volume: Mutex<f32>,
}

impl NativeEngine {
    pub fn new() -> Result<Self> {
        let device = rodio::stream::DeviceSinkBuilder::open_default_sink()
            .map_err(|e| PlaybackError::Audio(format!("output: {e}")))?;
        let player = rodio::Player::connect_new(device.mixer());
        Ok(Self {
            player,
            _device: device,
            current: Mutex::new(None),
            decoded: Mutex::new(DecodedCache::new(DECODED_CACHE_SIZE)),
            volume: Mutex::new(1.0),
        })
    }

    fn source_for(
        pcm: &[f32],
        rate: u32,
        channels: u16,
        skip_ms: u64,
    ) -> rodio::buffer::SamplesBuffer {
        let channels_nz = NonZeroU16::new(channels.max(1)).expect("channels clamped to >= 1");
        let rate_nz = NonZeroU32::new(rate.max(1)).expect("rate clamped to >= 1");
        let skip = (skip_ms as usize * rate as usize / 1000) * channels as usize;
        let from = skip.min(pcm.len());
        rodio::buffer::SamplesBuffer::new(channels_nz, rate_nz, pcm[from..].to_vec())
    }

    fn start_decoded(&self, track_id: String, decoded: std::sync::Arc<Decoded>) -> u64 {
        self.player.stop();
        self.player.append(Self::source_for(
            &decoded.pcm,
            decoded.rate,
            decoded.channels,
            0,
        ));
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

    /// Decode (or reuse the decoded cache) + play one track, stopping the
    /// previous. Returns `(duration_ms, from_cache)`.
    pub fn play_bytes(&self, track_id: String, bytes: &[u8]) -> Result<(u64, bool)> {
        let decoded = {
            let mut cache = self
                .decoded
                .lock()
                .map_err(|_| PlaybackError::Audio("lock".into()))?;
            if let Some(hit) = cache.get(&track_id) {
                (hit, true)
            } else {
                let (pcm, rate, channels, duration_ms) = decode_mem(bytes)?;
                let decoded = std::sync::Arc::new(Decoded {
                    pcm,
                    rate,
                    channels,
                    duration_ms,
                });
                cache.put(track_id.clone(), decoded.clone());
                (decoded, false)
            }
        };
        let duration_ms = self.start_decoded(track_id, decoded.0.clone());
        Ok((duration_ms, decoded.1))
    }

    /// Play from the decoded cache without decoding. Errors when absent —
    /// callers fall through to the progressive path then.
    pub fn play_cached(&self, track_id: &str) -> Result<u64> {
        let hit = self
            .decoded
            .lock()
            .map_err(|_| PlaybackError::Audio("lock".into()))?
            .get(track_id)
            .ok_or_else(|| PlaybackError::Audio("not in decoded cache".into()))?;
        Ok(self.start_decoded(track_id.to_string(), hit))
    }

    /// Start progressive playback with the first decoded chunklet.
    /// `total_ms` is the full track length (0 = unknown, span is shown).
    pub fn play_first(
        &self,
        track_id: String,
        first: std::sync::Arc<Decoded>,
        total_ms: u64,
    ) -> Result<()> {
        self.player.stop();
        self.player
            .append(Self::source_for(&first.pcm, first.rate, first.channels, 0));
        self.player.play();
        let duration_ms = first.duration_ms;
        *self
            .current
            .lock()
            .map_err(|_| PlaybackError::Audio("lock".into()))? = Some(Current::Prog(ProgState {
            id: track_id,
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
    /// for a stale track (superseded pipeline) instead of mixing audio.
    pub fn append_chunk(&self, track_id: &str, chunk: std::sync::Arc<Decoded>) -> Result<()> {
        let mut current = self
            .current
            .lock()
            .map_err(|_| PlaybackError::Audio("lock".into()))?;
        match current.as_mut() {
            Some(Current::Prog(p)) if p.id == track_id => {
                self.player
                    .append(Self::source_for(&chunk.pcm, chunk.rate, chunk.channels, 0));
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
    pub fn finish_chunks(&self, track_id: &str) -> Result<()> {
        let mut current = self
            .current
            .lock()
            .map_err(|_| PlaybackError::Audio("lock".into()))?;
        match current.as_mut() {
            Some(Current::Prog(p)) if p.id == track_id => {
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

    /// Decode into the cache without playing (background prefetch of
    /// upcoming tracks). Returns the duration without touching playback.
    pub fn prime(&self, track_id: String, bytes: &[u8]) -> Result<u64> {
        let mut cache = self
            .decoded
            .lock()
            .map_err(|_| PlaybackError::Audio("lock".into()))?;
        if let Some(hit) = cache.get(&track_id) {
            return Ok(hit.duration_ms);
        }
        let (pcm, rate, channels, duration_ms) = decode_mem(bytes)?;
        cache.put(
            track_id,
            std::sync::Arc::new(Decoded {
                pcm,
                rate,
                channels,
                duration_ms,
            }),
        );
        Ok(duration_ms)
    }

    pub fn is_decoded(&self, track_id: &str) -> bool {
        self.decoded
            .lock()
            .map(|mut c| c.get(track_id).is_some())
            .unwrap_or(false)
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
                // SamplesBuffer is seekable per-source; the player forwards it.
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
                    self.player.append(Self::source_for(
                        &chunk.pcm,
                        chunk.rate,
                        chunk.channels,
                        skip,
                    ));
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
            Some(Current::Single { id, duration_ms }) => {
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
    fn decoded_cache_evicts_oldest() {
        let mut c = DecodedCache::new(2);
        let mk = |n: usize| {
            std::sync::Arc::new(Decoded {
                pcm: vec![n as f32],
                rate: 44100,
                channels: 2,
                duration_ms: n as u64,
            })
        };
        c.put("a".into(), mk(1));
        c.put("b".into(), mk(2));
        assert!(c.get("a").is_some()); // refresh a
        c.put("c".into(), mk(3)); // evicts b, not a
        assert!(c.get("a").is_some());
        assert!(c.get("b").is_none());
        assert!(c.get("c").is_some());
    }
}
