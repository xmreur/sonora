//! File-backed decoded-PCM cache.
//!
//! Fully decoded tracks (30–55MB of s16 each) live here instead of RAM:
//! `<cache>/sonora/pcm/<adam>.pcm`. The playing track still retains its
//! chunklets in memory (gapless playback + seek need them), but the idle
//! cache slot drops from ~100MB RAM to zero. Next/replay reads the file
//! back (~50–150ms) instead of re-decoding, still skipping network,
//! license, decrypt, and decode.
//!
//! Format (little-endian): magic `SNRPCM02` (8B) + rate u32 + channels u16 +
//! reserved u16 + frames u64, then interleaved s16 samples. Writes go to a
//! per-generation `.part.<gen>` file and are atomically renamed on
//! completion, so a crash or superseded pipeline can never leave a readable
//! partial entry. Any validation failure is a cache miss, never a playback
//! failure.

use std::io::{Read, Seek, SeekFrom, Write};
use std::path::PathBuf;

use crate::audio::Decoded;
use crate::error::{PlaybackError, Result};

const MAGIC: &[u8; 8] = b"SNRPCM02";
pub const HEADER_LEN: usize = 32;
/// How many decoded tracks to keep on disk (~8 × 100MB worst case).
pub const FILE_CACHE_SIZE: usize = 8;

pub fn dir() -> PathBuf {
    directories::ProjectDirs::from("", "sonora", "sonora")
        .map(|d| d.cache_dir().join("pcm"))
        .unwrap_or_else(|| std::env::temp_dir().join("sonora-pcm"))
}

fn safe_id(adam_id: &str) -> String {
    adam_id
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        .collect()
}

pub fn final_path(adam_id: &str) -> PathBuf {
    dir().join(format!("{}.pcm", safe_id(adam_id)))
}

fn part_path(adam_id: &str, gen: u64) -> PathBuf {
    dir().join(format!(".{}.part.{gen}", safe_id(adam_id)))
}

fn encode_header(rate: u32, channels: u16, frames: u64) -> [u8; HEADER_LEN] {
    let mut h = [0u8; HEADER_LEN];
    h[0..8].copy_from_slice(MAGIC);
    h[8..12].copy_from_slice(&rate.to_le_bytes());
    h[12..14].copy_from_slice(&channels.to_le_bytes());
    h[24..32].copy_from_slice(&frames.to_le_bytes());
    h
}

fn parse_header(h: &[u8; HEADER_LEN]) -> Result<(u32, u16, u64)> {
    if &h[0..8] != MAGIC {
        return Err(PlaybackError::Audio("pcm cache: bad magic".into()));
    }
    let rate = u32::from_le_bytes(h[8..12].try_into().unwrap());
    let channels = u16::from_le_bytes(h[12..14].try_into().unwrap());
    let frames = u64::from_le_bytes(h[24..32].try_into().unwrap());
    if !(8_000..=192_000).contains(&rate) || !(1..=8).contains(&channels) || frames == 0 {
        return Err(PlaybackError::Audio("pcm cache: bad header".into()));
    }
    Ok((rate, channels, frames))
}

/// Streaming writer: chunklets append as they decode, so caching never holds
/// more than one chunklet transiently. Sync by design — callers run this on a
/// blocking thread (pipeline) or in open_decoder-style one-shot decodes.
pub struct PartWriter {
    file: std::fs::File,
    id: String,
    gen: u64,
    rate: u32,
    channels: u16,
    frames: u64,
}

impl PartWriter {
    pub fn create(adam_id: &str, gen: u64, rate: u32, channels: u16) -> Result<Self> {
        let dir = dir();
        std::fs::create_dir_all(&dir)
            .map_err(|e| PlaybackError::Audio(format!("pcm cache: mkdir ({e})")))?;
        let path = part_path(adam_id, gen);
        let mut file = std::fs::File::create(&path)
            .map_err(|e| PlaybackError::Audio(format!("pcm cache: create ({e})")))?;
        file.write_all(&[0u8; HEADER_LEN])
            .map_err(|e| PlaybackError::Audio(format!("pcm cache: header ({e})")))?;
        Ok(Self {
            file,
            id: adam_id.to_string(),
            gen,
            rate,
            channels,
            frames: 0,
        })
    }

    /// Append interleaved f32 samples, stored as s16 (see [`crate::audio::Decoded`]).
    /// Native little-endian throughout; writer and reader always run on the
    /// same host.
    pub fn append(&mut self, pcm: &[f32]) -> Result<()> {
        const _: () = assert!(cfg!(target_endian = "little"));
        if !pcm.len().is_multiple_of(self.channels.max(1) as usize) {
            return Err(PlaybackError::Audio("pcm cache: ragged chunk".into()));
        }
        let s16 = crate::audio::f32_to_s16(pcm);
        // SAFETY: i16 and u8 are both plain data; the byte view borrows
        // `s16` for exactly this write call, same endianness both ends.
        let bytes = unsafe {
            std::slice::from_raw_parts(
                s16.as_ptr() as *const u8,
                std::mem::size_of_val(s16.as_slice()),
            )
        };
        self.file
            .write_all(bytes)
            .map_err(|e| PlaybackError::Audio(format!("pcm cache: write ({e})")))?;
        self.frames += s16.len() as u64 / self.channels.max(1) as u64;
        Ok(())
    }

    /// Patch the header, publish atomically, enforce the file cap.
    /// Returns the cached PCM byte size.
    pub fn finish(self) -> Result<usize> {
        let part = part_path(&self.id, self.gen);
        let (id, rate, channels, frames) = (self.id.clone(), self.rate, self.channels, self.frames);
        if frames == 0 {
            drop(self);
            abort(&part);
            return Err(PlaybackError::Audio("pcm cache: empty".into()));
        }
        let header = encode_header(rate, channels, frames);
        let mut file = self.file;
        file.seek(SeekFrom::Start(0))
            .and_then(|_| file.write_all(&header))
            .and_then(|_| file.flush())
            .map_err(|e| PlaybackError::Audio(format!("pcm cache: finalize ({e})")))?;
        drop(file);
        let bytes = (frames * channels.max(1) as u64 * 2) as usize;
        let bytes_on_disk = HEADER_LEN + bytes;
        let _ = std::fs::rename(&part, final_path(&id));
        eprintln!("sonora native: pcm file cached {id} ({bytes_on_disk} bytes)");
        // Same drop-behind for the just-written pages (replay re-reads from
        // disk at budgeted file-hit latency).
        if let Ok(f) = std::fs::File::open(final_path(&id)) {
            dontneed(&f);
        }
        evict_old(Some(&id));
        Ok(bytes_on_disk)
    }

    /// Abandon an incomplete entry (superseded pipeline, IO error).
    pub fn abort(self) {
        let path = part_path(&self.id, self.gen);
        let id = self.id.clone();
        drop(self.file);
        abort(&path);
        eprintln!("sonora native: pcm part dropped {id}");
    }
}

fn abort(path: &PathBuf) {
    let _ = std::fs::remove_file(path);
}

/// Advise the kernel we won't re-read these pages soon (bulk sequential
/// file access otherwise sits in page cache counting toward RSS until
/// memory pressure reclaims it). Best-effort; Linux-only.
#[cfg(target_os = "linux")]
fn dontneed(file: &std::fs::File) {
    use std::os::unix::io::AsRawFd;
    // SAFETY: fd is a valid open file; DONTNEED only affects clean page
    // cache and is advisory — safe on any file.
    unsafe {
        libc::posix_fadvise(file.as_raw_fd(), 0, 0, libc::POSIX_FADV_DONTNEED);
    }
}

/// Advise the kernel we won't re-read these pages soon (bulk sequential
/// file access otherwise sits in page cache counting toward RSS until
/// memory pressure reclaims it). Best-effort; Linux-only.
#[cfg(not(target_os = "linux"))]
fn dontneed(_file: &std::fs::File) {}

/// Drop a path's pages from page cache (bulk cache IO shouldn't inflate
/// RSS until memory pressure). Best-effort; Linux-only under the hood.
pub fn drop_pages(path: &std::path::Path) {
    let Ok(f) = std::fs::File::open(path) else {
        return;
    };
    dontneed(&f);
}

/// Load, deleting stale/invalid entries so the next decode rewrites them
/// (e.g., previous format versions). Returns None on any miss.
pub fn load_valid(adam_id: &str) -> Option<Decoded> {
    match load(adam_id) {
        Ok(d) => Some(d),
        Err(_) => {
            let _ = std::fs::remove_file(final_path(adam_id));
            None
        }
    }
}
/// full validation happens on load, and any failure is a miss).
/// True when a COMPLETE entry exists on disk (cheap: existence only;
/// full validation happens on load, and any failure is a miss).
pub fn is_cached(adam_id: &str) -> bool {
    final_path(adam_id).is_file()
}

/// Load a complete entry into RAM for playback. Any error (missing,
/// corrupt, truncated) is a miss — callers fall through to progressive.
pub fn load(adam_id: &str) -> Result<Decoded> {
    let path = final_path(adam_id);
    let mut file =
        std::fs::File::open(&path).map_err(|_| PlaybackError::Audio("pcm miss".into()))?;
    let mut header = [0u8; HEADER_LEN];
    file.read_exact(&mut header)
        .map_err(|_| PlaybackError::Audio("pcm cache: short header".into()))?;
    let (rate, channels, frames) = parse_header(&header)?;
    // Bound the allocation before touching the heap: corrupt headers must
    // be a miss, never a panic/OOM (debug builds overflow-check).
    let seconds = frames / rate.max(1) as u64;
    if seconds > 6 * 3600 {
        return Err(PlaybackError::Audio("pcm cache: absurd duration".into()));
    }
    let samples = (frames as usize)
        .checked_mul(channels as usize)
        .ok_or_else(|| PlaybackError::Audio("pcm cache: absurd size".into()))?;
    // ~1GB ceiling: far above any real track, far below abort territory.
    if samples > 500_000_000 {
        return Err(PlaybackError::Audio("pcm cache: absurd size".into()));
    }
    // Read straight into the s16 buffer (one allocation, no conversion):
    // writing its bytes as u8 is sound for plain integer data.
    let mut pcm = vec![0i16; samples];
    let bytes = unsafe { std::slice::from_raw_parts_mut(pcm.as_mut_ptr() as *mut u8, samples * 2) };
    file.read_exact(bytes)
        .map_err(|_| PlaybackError::Audio("pcm cache: truncated".into()))?;
    // Reject trailing garbage (a torn rename would fail earlier, but be strict).
    let mut tail = [0u8; 1];
    if file.read(&mut tail).unwrap_or(0) != 0 {
        return Err(PlaybackError::Audio("pcm cache: trailing data".into()));
    }
    // Bulk sequential read done: drop the pages so a 100MB file hit doesn't
    // sit in page cache doubling its RSS cost (heap copy remains).
    dontneed(&file);
    let frames_ms = frames * 1000 / rate.max(1) as u64;
    Ok(Decoded {
        pcm,
        rate,
        channels,
        duration_ms: frames_ms,
    })
}

/// Drop leftover `.part.*` files and enforce the entry cap (oldest mtime
/// first, never the `except` id). Best-effort throughout.
pub fn sweep() {
    let dir = dir();
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        // Part files always start with '.'; complete entries never do.
        if name.starts_with('.') {
            let _ = std::fs::remove_file(entry.path());
        }
    }
    evict_old(None);
}

fn evict_old(except: Option<&str>) {
    let dir = dir();
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return;
    };
    let mut finals: Vec<(std::time::SystemTime, PathBuf, u64)> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|e| e != "pcm") {
            continue;
        }
        let (mtime, len) = entry
            .metadata()
            .map(|m| (m.modified().unwrap_or(std::time::UNIX_EPOCH), m.len()))
            .unwrap_or((std::time::UNIX_EPOCH, 0));
        finals.push((mtime, path, len));
    }
    finals.sort_by_key(|a| a.0);
    let over = finals.len().saturating_sub(FILE_CACHE_SIZE);
    let mut removed = 0usize;
    for (_, path, len) in finals {
        if removed >= over {
            break;
        }
        if let Some(keep) = except {
            if path
                .file_stem()
                .is_some_and(|s| s.to_string_lossy() == keep)
            {
                continue;
            }
        }
        if std::fs::remove_file(&path).is_ok() {
            removed += 1;
            eprintln!(
                "sonora native: pcm file evicted {} ({len} bytes)",
                path.file_name().unwrap_or_default().to_string_lossy()
            );
        }
    }
}

/// (count, bytes) of complete entries, for retention telemetry.
pub fn stats() -> (usize, usize) {
    let dir = dir();
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return (0, 0);
    };
    let mut n = 0usize;
    let mut bytes = 0usize;
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.extension().is_some_and(|e| e == "pcm") {
            continue;
        }
        if let Ok(m) = entry.metadata() {
            n += 1;
            bytes += m.len() as usize;
        }
    }
    (n, bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_round_trips() {
        let h = encode_header(48_000, 2, 123456);
        let (rate, ch, frames) = parse_header(&h).unwrap();
        assert_eq!((rate, ch, frames), (48_000, 2, 123456));
        let mut bad = h;
        bad[0] = b'X';
        assert!(parse_header(&bad).is_err());
        let mut bad2 = h;
        bad2[8..12].copy_from_slice(&7u32.to_le_bytes());
        assert!(parse_header(&bad2).is_err());
    }

    #[test]
    fn part_write_load_round_trips() {
        let id = format!("test-roundtrip-{}", std::process::id());
        // Exact s16 values (f32 source chosen to convert losslessly).
        let src: Vec<f32> = (0..8192).map(|i| i as f32 / 32768.0).collect();
        let want: Vec<i16> = (0..8192).collect();
        let mut w = PartWriter::create(&id, 1, 48_000, 2).unwrap();
        w.append(&src[..4096]).unwrap();
        w.append(&src[4096..]).unwrap();
        let bytes = w.finish().unwrap();
        assert_eq!(bytes, HEADER_LEN + 8192 * 2);
        let back = load(&id).unwrap();
        assert_eq!((back.rate, back.channels), (48_000, 2));
        assert_eq!(back.pcm, want);
        assert!(is_cached(&id));
        let _ = std::fs::remove_file(final_path(&id));
        assert!(!is_cached(&id));
    }

    #[test]
    fn load_rejects_garbage_and_partial() {
        assert!(load("test-no-such-id-xyz").is_err());
        // A stale .part file is never readable as complete.
        let id = format!("test-partial-{}", std::process::id());
        let mut w = PartWriter::create(&id, 2, 48_000, 2).unwrap();
        w.append(&[1.0, 2.0]).unwrap();
        // Intentionally NOT finished: final path must not exist.
        assert!(!is_cached(&id));
        assert!(load(&id).is_err());
        // Abort cleans up the part file.
        let part = part_path(&id, 2);
        w.abort();
        assert!(!part.exists());
    }

    #[test]
    fn load_valid_heals_stale_entries() {
        // A previous-format (or corrupt) file is deleted and reported as a
        // miss, so prefetch/playback re-decode and rewrite it instead of
        // skipping the track forever.
        let id = format!("test-stale-{}", std::process::id());
        std::fs::create_dir_all(dir()).unwrap();
        std::fs::write(final_path(&id), b"v1 garbage, wrong magic").unwrap();
        assert!(is_cached(&id));
        assert!(load_valid(&id).is_none());
        assert!(!final_path(&id).exists());
    }
}
