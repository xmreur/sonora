//! Local audio engine: symphonia decode → rodio output.
//!
//! Single-track engine (parity with the sidecar's `PlayNow`, which plays
//! one song per command). Queue/Next/Previous stay in the Tauri layer,
//! which calls `play_bytes` per track.

use crate::error::{PlaybackError, Result};
use std::io::Cursor;
use std::num::{NonZeroU16, NonZeroU32};
use std::sync::Mutex;
use std::time::Duration;

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
struct Current {
    id: String,
    duration_ms: u64,
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
        self.player.stop();
        self.player.append(Self::source_for(
            &decoded.0.pcm,
            decoded.0.rate,
            decoded.0.channels,
            0,
        ));
        self.player.play();
        *self
            .current
            .lock()
            .map_err(|_| PlaybackError::Audio("lock".into()))? = Some(Current {
            id: track_id,
            duration_ms: decoded.0.duration_ms,
        });
        Ok((decoded.0.duration_ms, decoded.1))
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

    pub fn seek(&self, position_ms: u64) -> Result<()> {
        // SamplesBuffer is seekable per-source; the player forwards it.
        let _ = self.player.try_seek(Duration::from_millis(position_ms));
        Ok(())
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
        let current = self.current.lock().ok().and_then(|g| g.clone());
        match current {
            Some(c) => {
                let playing = !self.player.is_paused() && !self.player.empty();
                NativeStatus {
                    playing,
                    track_id: Some(c.id),
                    position_ms: self.player.get_pos().as_millis() as u64,
                    duration_ms: c.duration_ms,
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
