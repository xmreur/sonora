//! CENC sample decryption for Apple Music fMP4 assets.
//!
//! Layout: `moof/traf/senc` carries per-sample IVs + subsample
//! `(clear, cipher)` runs; the sample bytes live in the `mdat` following
//! each `moof`. Encrypted runs are AES-CTR via the CDM; clear bytes pass
//! through. Output is relabelled `enca → mp4a` so plain decoders accept it.

use crate::error::{PlaybackError, Result};

/// One sample's key material from `senc`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SampleKey {
    pub iv: Vec<u8>,
    /// `(clear_bytes, cipher_bytes)` runs covering the whole sample.
    pub subsamples: Vec<(u32, u32)>,
}

impl SampleKey {
    pub fn len(&self) -> usize {
        self.subsamples
            .iter()
            .map(|(c, e)| (*c as usize) + (*e as usize))
            .sum()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

fn u32be(b: &[u8]) -> u32 {
    u32::from_be_bytes([b[0], b[1], b[2], b[3]])
}

fn u16be(b: &[u8]) -> u16 {
    u16::from_be_bytes([b[0], b[1]])
}

fn child_boxes(data: &[u8], payload_off: usize, payload_len: usize) -> Vec<([u8; 4], &[u8])> {
    let mut out = Vec::new();
    let end = (payload_off + payload_len).min(data.len());
    let mut off = payload_off;
    while off + 8 <= end {
        let mut size = u32be(&data[off..off + 4]) as usize;
        let typ: [u8; 4] = [data[off + 4], data[off + 5], data[off + 6], data[off + 7]];
        let mut hdr = 8usize;
        if size == 1 {
            if off + 16 > end {
                break;
            }
            size = u64::from_be_bytes(data[off + 8..off + 16].try_into().unwrap()) as usize;
            hdr = 16;
        } else if size == 0 {
            size = end - off;
        }
        if size < hdr || off + size > end {
            break;
        }
        out.push((typ, &data[off + hdr..off + size]));
        if size == 0 {
            break;
        }
        off += size;
    }
    out
}

/// Track-encryption defaults from the `moov/.../schi/tenc` box.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TencInfo {
    pub version: u8,
    pub iv_size: usize,
    pub constant_iv: Option<Vec<u8>>,
}

/// Find the first `tenc` box, descending through `stsd` sample entries into
/// `sinf/schi`. Version-aware: v0 keeps `isProtected(1) + Per_Sample_IV_Size(1)`
/// right after the FullBox header; v1 inserts `crypt_byte_block +
/// skip_byte_block` first. Anything else (or a missing box) yields `None`
/// and the caller falls back to strict `[8, 16]` attempts.
pub fn find_tenc(mp4: &[u8]) -> Option<TencInfo> {
    const CONTAINERS: [&[u8; 4]; 7] = [
        b"moov", b"trak", b"mdia", b"minf", b"stbl", b"sinf", b"schi",
    ];

    fn search(data: &[u8], off: usize, len: usize) -> Option<TencInfo> {
        for (t, p) in child_boxes(data, off, len) {
            if &t == b"tenc" {
                if let Some(info) = parse_tenc(p) {
                    return Some(info);
                }
            } else if CONTAINERS.contains(&&t) {
                let base = p.as_ptr() as usize - data.as_ptr() as usize;
                if let Some(info) = search(data, base, p.len()) {
                    return Some(info);
                }
            } else if &t == b"stsd" {
                if let Some(info) = search_stsd(data, p) {
                    return Some(info);
                }
            }
        }
        None
    }

    /// Inside `stsd`, entries are boxes after version/flags + count; each
    /// audio entry may carry a `sinf` box at a version-dependent offset, so
    /// locate it with a sliding scan, then descend strictly.
    fn search_stsd(data: &[u8], stsd: &[u8]) -> Option<TencInfo> {
        let base = stsd.as_ptr() as usize - data.as_ptr() as usize;
        if stsd.len() < 8 {
            return None;
        }
        let mut off = base + 8;
        let end = base + stsd.len();
        while off + 8 <= end {
            let size = u32be(&data[off..off + 4]) as usize;
            if size < 8 || off + size > end {
                break;
            }
            // Slide for sinf inside the entry (offset varies by entry version).
            let estart = off + 8;
            let eend = off + size;
            let mut s = estart;
            while s + 8 <= eend {
                let ssize = u32be(&data[s..s + 4]) as usize;
                if &data[s + 4..s + 8] == b"sinf" && ssize >= 8 && s + ssize <= eend {
                    if let Some(info) = search(data, s + 8, ssize - 8) {
                        return Some(info);
                    }
                }
                s += 1;
            }
            if size == 0 {
                break;
            }
            off += size;
        }
        None
    }

    let len = mp4.len();
    search(mp4, 0, len)
}

fn parse_tenc(payload: &[u8]) -> Option<TencInfo> {
    if payload.len() < 6 {
        return None;
    }
    let version = payload[0];
    let (iv_off, kid_off) = match version {
        0 => (5usize, 6usize),
        1 => (7usize, 8usize),
        _ => return None,
    };
    if payload.len() < kid_off + 16 {
        return None;
    }
    let iv_size = payload[iv_off] as usize;
    if iv_size > 16 {
        return None;
    }
    let rest = &payload[kid_off + 16..];
    let constant_iv = if iv_size == 0 && !rest.is_empty() {
        if rest.len() == 8 || rest.len() == 16 {
            Some(rest.to_vec())
        } else {
            return None;
        }
    } else {
        None
    };
    Some(TencInfo {
        version,
        iv_size,
        constant_iv,
    })
}

/// IV sizes to attempt, most authoritative first. A found `tenc` leads;
/// without one, strict `[8, 16]` attempts disambiguate (exact-consumption
/// parsing rejects the wrong size instead of decrypting garbage).
fn iv_candidates(mp4: &[u8]) -> (Vec<usize>, Option<Vec<u8>>) {
    if let Some(t) = find_tenc(mp4) {
        let mut sizes = vec![t.iv_size];
        for alt in [8usize, 16] {
            if alt != t.iv_size {
                sizes.push(alt);
            }
        }
        (sizes, t.constant_iv)
    } else {
        (vec![8, 16], None)
    }
}

/// `senc` interpretation order. Tables first: they carry strictly more
/// structure, so a file that parses as tables *and* agrees with `trun`
/// is decrypted that way. Iv-only second: exact-length bare IVs.
/// A file that parses as tables but contradicts `trun` is corrupt or
/// unknown — that aborts loudly rather than falling through to a
/// different-IV reinterpretation (which would decrypt noise).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SencMode {
    Tables,
    IvOnly,
}

/// Parse one `senc` payload (after size+type) strictly: every byte must be
/// accounted for, otherwise the IV size is wrong and parsing — not silent
/// garbage — is reported. In [`SencMode::Tables`] each sample is
/// `IV + subsample_count + (clear, cipher)[]` (empty table allowed: lengths
/// come from `trun`). In [`SencMode::IvOnly`] the payload must be exactly
/// `8 + count × iv_size` bare-IV bytes — a table payload is always longer,
/// so the shapes never collide within one `iv_size`.
pub fn parse_senc(
    payload: &[u8],
    iv_size: usize,
    constant_iv: Option<&[u8]>,
    mode: SencMode,
) -> Result<Vec<SampleKey>> {
    if payload.len() < 8 {
        return Err(PlaybackError::Decode("senc too short".into()));
    }
    let version = payload[0];
    if version > 1 {
        return Err(PlaybackError::Decode(format!(
            "unsupported senc version {version}"
        )));
    }
    let count = u32be(&payload[4..8]) as usize;
    if count > 1_000_000 {
        return Err(PlaybackError::Decode(format!(
            "absurd senc sample count {count}"
        )));
    }
    if mode == SencMode::IvOnly {
        if count > 0 && payload.len() == 8 + count * iv_size {
            let mut out = Vec::with_capacity(count.min(8192));
            for i in 0..count {
                out.push(SampleKey {
                    iv: payload[8 + i * iv_size..8 + (i + 1) * iv_size].to_vec(),
                    subsamples: Vec::new(),
                });
            }
            return Ok(out);
        }
        return Err(PlaybackError::Decode(format!(
            "not iv-only shaped (len={} for count={count} iv_size={iv_size})",
            payload.len()
        )));
    }
    let mut off = 8usize;
    let mut out = Vec::with_capacity(count.min(8192));
    for _ in 0..count {
        let iv = if iv_size > 0 {
            if off + iv_size > payload.len() {
                return Err(PlaybackError::Decode("senc iv overrun".into()));
            }
            let iv = payload[off..off + iv_size].to_vec();
            off += iv_size;
            iv
        } else {
            constant_iv
                .filter(|c| !c.is_empty())
                .ok_or_else(|| {
                    PlaybackError::Decode("track uses constant IV but tenc carries none".into())
                })?
                .to_vec()
        };
        if off + 2 > payload.len() {
            return Err(PlaybackError::Decode("senc subsample overrun".into()));
        }
        let n = u16be(&payload[off..off + 2]) as usize;
        off += 2;
        let mut subs = Vec::with_capacity(n.min(64));
        for _ in 0..n {
            if off + 6 > payload.len() {
                return Err(PlaybackError::Decode("senc subsample overrun".into()));
            }
            subs.push((
                u16be(&payload[off..off + 2]) as u32,
                u32be(&payload[off + 2..off + 6]),
            ));
            off += 6;
        }
        out.push(SampleKey {
            iv,
            subsamples: subs,
        });
    }
    if off != payload.len() {
        return Err(PlaybackError::Decode(format!(
            "senc has {} trailing bytes (wrong iv_size={iv_size}?)",
            payload.len() - off
        )));
    }
    Ok(out)
}

/// One track-fragment run: `senc` keys plus optional `trun` sample sizes.
/// Table-less (fully encrypted) samples take their lengths from `sizes`.
#[derive(Debug)]
struct TrafData {
    sizes: Option<Vec<u32>>,
    keys: Vec<SampleKey>,
}

/// `trun` sample sizes, or `None` when the box carries none
/// (`flags & 0x200` unset) or is malformed.
fn parse_trun(payload: &[u8]) -> Option<Vec<u32>> {
    if payload.len() < 8 {
        return None;
    }
    let flags = u32be(&payload[0..4]) & 0x00FF_FFFF;
    if flags & 0x200 == 0 {
        return None;
    }
    let count = u32be(&payload[4..8]) as usize;
    if count > 1_000_000 {
        return None;
    }
    let mut off = 8usize;
    if flags & 0x1 != 0 {
        off += 4; // data_offset
    }
    if flags & 0x4 != 0 {
        off += 4; // first_sample_flags
    }
    let mut sizes = Vec::with_capacity(count.min(8192));
    for _ in 0..count {
        if flags & 0x100 != 0 {
            off += 4; // sample_duration
        }
        let mut size = 0u32;
        if flags & 0x200 != 0 {
            if off + 4 > payload.len() {
                return None;
            }
            size = u32be(&payload[off..off + 4]);
            off += 4;
        }
        if flags & 0x400 != 0 {
            off += 4; // sample_flags
        }
        if flags & 0x800 != 0 {
            off += 4; // sample_composition_time_offset
        }
        if off > payload.len() {
            return None;
        }
        sizes.push(size);
    }
    Some(sizes)
}

/// Collect `(mdat_payload_range, trafs)` per `moof` in file order.
/// Each `moof`'s trafs map onto the `mdat` that follows it, in order.
fn collect_fragments(
    mp4: &[u8],
    iv_size: usize,
    constant_iv: Option<&[u8]>,
    mode: SencMode,
) -> Result<Vec<(std::ops::Range<usize>, Vec<TrafData>)>> {
    // Top-level boxes with their file ranges.
    let mut tops: Vec<([u8; 4], usize, usize)> = Vec::new();
    let mut off = 0usize;
    while off + 8 <= mp4.len() {
        let mut size = u32be(&mp4[off..off + 4]) as usize;
        let typ: [u8; 4] = [mp4[off + 4], mp4[off + 5], mp4[off + 6], mp4[off + 7]];
        let mut hdr = 8usize;
        if size == 1 {
            if off + 16 > mp4.len() {
                break;
            }
            size = u64::from_be_bytes(mp4[off + 8..off + 16].try_into().unwrap()) as usize;
            hdr = 16;
        } else if size == 0 {
            size = mp4.len() - off;
        }
        if size < hdr || off + size > mp4.len() {
            break;
        }
        tops.push((typ, off + hdr, size - hdr));
        if size == 0 {
            break;
        }
        off += size;
    }

    let mut frags = Vec::new();
    let mut i = 0usize;
    while i < tops.len() {
        if &tops[i].0 == b"moof" {
            // Gather per-traf (trun sizes, senc keys) in file order.
            let mut trafs = Vec::new();
            let (moof_off, moof_len) = (tops[i].1, tops[i].2);
            for (t, p) in child_boxes(mp4, moof_off, moof_len) {
                if &t == b"traf" {
                    let base = p.as_ptr() as usize - mp4.as_ptr() as usize;
                    let mut sizes = None;
                    let mut keys = Vec::new();
                    for (t2, p2) in child_boxes(mp4, base, p.len()) {
                        if &t2 == b"trun" {
                            if let Some(s) = parse_trun(p2) {
                                sizes.get_or_insert(Vec::new()).extend(s);
                            }
                        } else if &t2 == b"senc" {
                            keys.extend(parse_senc(p2, iv_size, constant_iv, mode)?);
                        }
                    }
                    if !keys.is_empty() {
                        trafs.push(TrafData { sizes, keys });
                    }
                }
            }
            // The mdat that follows this moof holds its sample bytes.
            let mut j = i + 1;
            while j < tops.len() && &tops[j].0 != b"mdat" && &tops[j].0 != b"moof" {
                j += 1;
            }
            if !trafs.is_empty() {
                if j < tops.len() && &tops[j].0 == b"mdat" {
                    let (mo, ml) = (tops[j].1, tops[j].2);
                    frags.push((mo..mo + ml, trafs));
                } else {
                    return Err(PlaybackError::Decode(
                        "moof without a following mdat".into(),
                    ));
                }
            }
        }
        i += 1;
    }
    if frags.is_empty() {
        return Err(PlaybackError::Decode(
            "no encrypted fragments (no moof/senc)".into(),
        ));
    }
    Ok(frags)
}

/// Compact structural diagnosis for undecryptable tracks (no media bytes,
/// safe to paste into a bug report): top-level boxes, `tenc` findings, and
/// the first `senc`'s version/count/length plus its first bytes as hex.
pub fn describe_track(mp4: &[u8]) -> String {
    let len = mp4.len();
    let mut tops = Vec::new();
    let mut off = 0usize;
    while off + 8 <= len {
        let mut size = u32be(&mp4[off..off + 4]) as usize;
        let typ = String::from_utf8_lossy(&mp4[off + 4..off + 8]).into_owned();
        let mut hdr = 8usize;
        if size == 1 {
            if off + 16 > len {
                break;
            }
            size = u64::from_be_bytes(mp4[off + 8..off + 16].try_into().unwrap()) as usize;
            hdr = 16;
        } else if size == 0 {
            size = len - off;
        }
        if size < hdr || off + size > len {
            tops.push(format!("{typ}<?>"));
            break;
        }
        tops.push(format!("{typ}:{size}"));
        if size == 0 {
            break;
        }
        off += size;
        if tops.len() > 24 {
            tops.push("…".into());
            break;
        }
    }
    let tenc = match find_tenc(mp4) {
        Some(t) => format!(
            "v{} iv{} const={}",
            t.version,
            t.iv_size,
            t.constant_iv.as_ref().map(|c| c.len()).unwrap_or(0)
        ),
        None => "absent".to_string(),
    };
    let senc = first_senc_info(mp4).unwrap_or_else(|| "absent".to_string());
    format!(
        "file={len}B tops=[{}] tenc={tenc} senc={senc}",
        tops.join(" ")
    )
}

/// `version/count/payload-len + first-bytes-hex` of the first `senc` box.
fn first_senc_info(mp4: &[u8]) -> Option<String> {
    let len = mp4.len();
    let mut off = 0usize;
    while off + 8 <= len {
        let mut size = u32be(&mp4[off..off + 4]) as usize;
        let typ: [u8; 4] = [mp4[off + 4], mp4[off + 5], mp4[off + 6], mp4[off + 7]];
        let mut hdr = 8usize;
        if size == 1 {
            if off + 16 > len {
                return None;
            }
            size = u64::from_be_bytes(mp4[off + 8..off + 16].try_into().unwrap()) as usize;
            hdr = 16;
        } else if size == 0 {
            size = len - off;
        }
        if size < hdr || off + size > len {
            return None;
        }
        if &typ == b"moof" {
            for (t, p) in child_boxes(mp4, off + hdr, size - hdr) {
                if &t == b"traf" {
                    let base = p.as_ptr() as usize - mp4.as_ptr() as usize;
                    for (t2, p2) in child_boxes(mp4, base, p.len()) {
                        if &t2 == b"senc" && p2.len() >= 8 {
                            let head = &p2[..p2.len().min(48)];
                            return Some(format!(
                                "v{} count={} len={} hex={}",
                                p2[0],
                                u32be(&p2[4..8]),
                                p2.len(),
                                hex::encode(head),
                            ));
                        }
                    }
                }
            }
            return Some("moof without senc".to_string());
        }
        if size == 0 {
            break;
        }
        off += size;
    }
    None
}
/// Patch `stsd` sample entries `enca → mp4a` (box-aware: never touches media).
pub fn relabel_enca(data: &mut [u8]) {
    fn patch_in(data: &mut [u8], off: usize, len: usize) {
        for (t, start, plen) in {
            let mut v = Vec::new();
            let end = (off + len).min(data.len());
            let mut o = off;
            while o + 8 <= end {
                let size = u32be(&data[o..o + 4]) as usize;
                let typ: [u8; 4] = [data[o + 4], data[o + 5], data[o + 6], data[o + 7]];
                if size < 8 || o + size > end {
                    break;
                }
                v.push((typ, o + 8, size - 8));
                if size == 0 {
                    break;
                }
                o += size;
            }
            v
        } {
            if &t == b"stsd" && plen >= 8 {
                // version/flags(4) + entry_count(4), then entries.
                let mut o = start + 8;
                let end = start + plen;
                while o + 8 <= end {
                    let esize = u32be(&data[o..o + 4]) as usize;
                    if esize < 8 || o + esize > end {
                        break;
                    }
                    if &data[o + 4..o + 8] == b"enca" {
                        data[o + 4..o + 8].copy_from_slice(b"mp4a");
                    }
                    if esize == 0 {
                        break;
                    }
                    o += esize;
                }
            } else if matches!(&t, b"moov" | b"trak" | b"mdia" | b"minf" | b"stbl") {
                patch_in(data, start, plen);
            }
        }
    }
    let len = data.len();
    patch_in(data, 0, len);
}

/// One sample's place in the file, with everything decryption needs.
/// Lengths come from `trun` when present, else from subsample sums (both
/// describe the same samples — verified during selection).
#[derive(Debug, Clone)]
pub struct SampleExtent {
    /// Index of the fragment (moof group) holding this sample.
    pub frag: usize,
    pub start: usize,
    pub len: usize,
    pub iv: Vec<u8>,
    pub subs: Vec<(u32, u32)>,
}

/// A validated decryption plan: which interpretation won, plus the flat
/// sample table to decrypt (in file order). Built once; samples are then
/// decrypted in any prefix order, which is what makes progressive
/// playback possible.
#[derive(Debug, Clone)]
pub struct SelectedLayout {
    pub iv_size: usize,
    pub mode: SencMode,
    pub constant_iv: Option<Vec<u8>>,
    pub samples: Vec<SampleExtent>,
}

impl SelectedLayout {
    pub fn sample_count(&self) -> usize {
        self.samples.len()
    }

    /// Byte end of the prefix covering the first `n` samples (for
    /// prefix-decode: `plain[..prefix_end(n)]` is a valid fMP4 prefix).
    pub fn prefix_end(&self, n: usize) -> usize {
        if n == 0 {
            return 0;
        }
        self.samples
            .get(n - 1)
            .map(|s| s.start + s.len)
            .unwrap_or(0)
    }

    /// Round a wanted sample count up to whole fragments. Decodable
    /// prefixes must end at mdat boundaries: a truncated trailing mdat
    /// yields nothing (measured), so sample-granular cutoffs would stall
    /// the first chunklet forever.
    pub fn aligned_end(&self, want: usize) -> usize {
        if want == 0 {
            return 0;
        }
        let want = want.min(self.samples.len());
        if want >= self.samples.len() {
            return self.samples.len();
        }
        let f = self.samples[want - 1].frag;
        let mut end = want;
        while end < self.samples.len() && self.samples[end].frag == f {
            end += 1;
        }
        end
    }
}

/// Pick the winning interpretation (tables phase, then iv-only) and build
/// its flat sample table. Pure parse + validate — no CDM involved, so this
/// is fast and runs before any decryption starts.
pub fn select_layout(cipher: &[u8]) -> Result<SelectedLayout> {
    let (sizes, constant_iv) = iv_candidates(cipher);
    let mut errors = Vec::new();
    for mode in [SencMode::Tables, SencMode::IvOnly] {
        for iv_size in &sizes {
            match collect_fragments(cipher, *iv_size, constant_iv.as_deref(), mode) {
                Err(e) => errors.push(format!("{mode:?}/iv_size={iv_size}: {e}")),
                Ok(frags) => match flatten_samples(&frags) {
                    Err(e) => {
                        // Parsed, but contradicts trun: the file claims this
                        // shape and means it — stop, don't reinterpret under
                        // another IV size (that would decrypt noise).
                        return Err(PlaybackError::Decode(format!(
                            "{mode:?}/iv_size={iv_size}: {e}"
                        )));
                    }
                    Ok(samples) => {
                        return Ok(SelectedLayout {
                            iv_size: *iv_size,
                            mode,
                            constant_iv,
                            samples,
                        });
                    }
                },
            }
        }
    }
    eprintln!("sonora native: senc diagnosis: {}", describe_track(cipher));
    Err(PlaybackError::Decode(format!(
        "no IV size parsed this track ({})",
        errors.join("; ")
    )))
}

/// Flatten collected trafs to the file-order sample table, resolving each
/// sample's byte range (trun sizes preferred, subsample sums as fallback).
/// Contradictions with trun abort loudly.
fn flatten_samples(
    frags: &[(std::ops::Range<usize>, Vec<TrafData>)],
) -> std::result::Result<Vec<SampleExtent>, String> {
    let mut out = Vec::new();
    for (fi, (range, trafs)) in frags.iter().enumerate() {
        let mut pos = range.start;
        for traf in trafs {
            if let Some(sizes) = &traf.sizes {
                if sizes.len() != traf.keys.len() {
                    return Err(format!(
                        "senc/trun count mismatch (senc={} trun={})",
                        traf.keys.len(),
                        sizes.len()
                    ));
                }
            }
            for (i, key) in traf.keys.iter().enumerate() {
                let total = if key.subsamples.is_empty() {
                    match &traf.sizes {
                        Some(sizes) => sizes[i] as usize,
                        None => {
                            return Err("fully-encrypted sample without trun sizes".to_string());
                        }
                    }
                } else {
                    let sum = key.len();
                    if let Some(sizes) = &traf.sizes {
                        if sum != sizes[i] as usize {
                            return Err(format!(
                                "senc/trun size mismatch (senc={sum} trun={})",
                                sizes[i]
                            ));
                        }
                    }
                    sum
                };
                if total == 0 {
                    continue;
                }
                if pos + total > range.end {
                    return Err("senc sizes exceed mdat".to_string());
                }
                let subs = if key.subsamples.is_empty() {
                    vec![(0, total as u32)]
                } else {
                    key.subsamples.clone()
                };
                out.push(SampleExtent {
                    frag: fi,
                    start: pos,
                    len: total,
                    iv: key.iv.clone(),
                    subs,
                });
                pos += total;
            }
        }
    }
    if out.is_empty() {
        return Err("no samples collected".to_string());
    }
    Ok(out)
}

/// Decrypt one track. `decrypt` maps
/// `(ciphertext, key_id, iv, subsamples) -> plaintext`.
pub fn decrypt_track(
    data: Vec<u8>,
    key_id: &[u8],
    mut decrypt: impl FnMut(&[u8], &[u8], &[u8], &[(u32, u32)]) -> Result<Vec<u8>>,
) -> Result<Vec<u8>> {
    let layout = select_layout(&data)?;
    let mut buf = data;
    decrypt_samples(
        &mut buf,
        key_id,
        &layout,
        0..layout.samples.len(),
        &mut decrypt,
    )?;
    relabel_enca(&mut buf);
    Ok(buf)
}

/// Decrypt `layout.samples[range]` in place in `buf` (which must still hold
/// ciphertext there). Split from `decrypt_track` so progressive playback
/// can decrypt prefix-by-prefix with one layout selected up front.
pub(crate) fn decrypt_samples(
    buf: &mut [u8],
    key_id: &[u8],
    layout: &SelectedLayout,
    range: std::ops::Range<usize>,
    decrypt: &mut impl FnMut(&[u8], &[u8], &[u8], &[(u32, u32)]) -> Result<Vec<u8>>,
) -> Result<()> {
    for s in layout.samples.get(range).unwrap_or(&[]) {
        let plain = decrypt(&buf[s.start..s.start + s.len], key_id, &s.iv, &s.subs)?;
        if plain.len() != s.len {
            return Err(PlaybackError::Decode(
                "decryptor returned wrong length".into(),
            ));
        }
        buf[s.start..s.start + s.len].copy_from_slice(&plain);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn box_(typ: &[u8; 4], payload: &[u8]) -> Vec<u8> {
        let mut v = ((payload.len() + 8) as u32).to_be_bytes().to_vec();
        v.extend_from_slice(typ);
        v.extend_from_slice(payload);
        v
    }

    type Entry = (Vec<u8>, Vec<(u32, u32)>);

    fn senc_payload(entries: &[Entry]) -> Vec<u8> {
        let mut v = vec![0, 0, 0, 0];
        v.extend_from_slice(&(entries.len() as u32).to_be_bytes());
        for (iv, subs) in entries {
            v.extend_from_slice(iv);
            v.extend_from_slice(&(subs.len() as u16).to_be_bytes());
            for (c, e) in subs {
                v.extend_from_slice(&(*c as u16).to_be_bytes());
                v.extend_from_slice(&e.to_be_bytes());
            }
        }
        v
    }

    fn synthetic_mp4() -> (Vec<u8>, Vec<u8>) {
        // moof(traf(trun + senc[2 samples])) + mdat(sample bytes).
        let senc = box_(
            b"senc",
            &senc_payload(&[
                (vec![1, 2, 3, 4, 5, 6, 7, 8], vec![(2, 4)]),
                (vec![9, 9, 9, 9, 9, 9, 9, 9], vec![(0, 3)]),
            ]),
        );
        let mut traf_payload = trun_box(&[6, 3]);
        traf_payload.extend_from_slice(&senc);
        let traf = box_(b"traf", &traf_payload);
        let moof = box_(b"moof", &traf);
        let media = b"AAbbbbCCC".to_vec(); // 2+4, then 3
        let mut mp4 = moof;
        mp4.extend_from_slice(&box_(b"mdat", &media));
        (mp4, media)
    }

    fn trun_box(sizes: &[u32]) -> Vec<u8> {
        // version 0, flags 0x200 (per-sample sizes present).
        let mut p = vec![0, 0, 2, 0];
        p.extend_from_slice(&(sizes.len() as u32).to_be_bytes());
        for s in sizes {
            p.extend_from_slice(&s.to_be_bytes());
        }
        box_(b"trun", &p)
    }

    /// The reported failing shape: bare 8-byte IVs, no subsample tables,
    /// lengths from trun.
    fn iv_only_mp4() -> Vec<u8> {
        let mut p = vec![0, 0, 0, 0, 0, 0, 0, 2]; // v0, count 2
        p.extend_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8]);
        p.extend_from_slice(&[9, 9, 9, 9, 9, 9, 9, 9]);
        let senc = box_(b"senc", &p);
        let mut traf_payload = trun_box(&[4, 5]);
        traf_payload.extend_from_slice(&senc);
        let moof = box_(b"moof", &box_(b"traf", &traf_payload));
        let mut mp4 = moof;
        mp4.extend_from_slice(&box_(b"mdat", b"AAAABBBBB"));
        mp4
    }

    fn tenc_box(version: u8, iv_size: u8) -> Vec<u8> {
        // FullBox header + version-specific prefix + isProtected + iv_size + KID.
        let mut p = vec![version, 0, 0, 0];
        if version == 1 {
            p.extend_from_slice(&[0, 0]); // crypt/skip byte blocks
        }
        p.extend_from_slice(&[1, iv_size]); // isProtected, Per_Sample_IV_Size
        p.extend_from_slice(&[0xAB; 16]); // default_KID
        box_(b"tenc", &p)
    }

    fn mp4_with_tenc(version: u8, iv_size: u8) -> Vec<u8> {
        // moov/trak/mdia/minf/stbl/stsd/[entry(28 filler + sinf/schi/tenc)].
        let tenc = tenc_box(version, iv_size);
        let schi = box_(b"schi", &tenc);
        let sinf = box_(b"sinf", &schi);
        let mut entry = vec![0, 0, 0, 0];
        entry.extend_from_slice(b"enca");
        entry.extend_from_slice(&[0u8; 28]);
        entry.extend_from_slice(&sinf);
        let len = entry.len() as u32;
        entry[0..4].copy_from_slice(&len.to_be_bytes());
        let mut stsd = vec![0, 0, 0, 0, 0, 0, 0, 1];
        stsd.extend_from_slice(&entry);
        let stsd = box_(b"stsd", &stsd);
        let stbl = box_(b"stbl", &stsd);
        let minf = box_(b"minf", &stbl);
        let mdia = box_(b"mdia", &minf);
        let trak = box_(b"trak", &mdia);
        box_(b"moov", &trak)
    }

    #[test]
    fn tenc_v0_reads_iv_size_after_fullbox() {
        assert_eq!(
            find_tenc(&mp4_with_tenc(0, 8)),
            Some(TencInfo {
                version: 0,
                iv_size: 8,
                constant_iv: None
            })
        );
        assert_eq!(find_tenc(&mp4_with_tenc(0, 16)).unwrap().iv_size, 16);
    }

    #[test]
    fn tenc_v1_skips_crypt_skip_blocks() {
        assert_eq!(find_tenc(&mp4_with_tenc(1, 8)).unwrap().iv_size, 8);
    }

    #[test]
    fn tenc_missing_falls_back() {
        let (mp4, _) = synthetic_mp4();
        assert_eq!(find_tenc(&mp4), None);
        // No moov/tenc: strict 8-byte parse still succeeds via fallback.
        let frags = collect_fragments(&mp4, 8, None, SencMode::Tables).unwrap();
        assert_eq!(frags[0].1.len(), 1); // one traf
        assert_eq!(frags[0].1[0].keys.len(), 2);
    }

    #[test]
    fn wrong_iv_size_is_rejected_not_misparsed() {
        let (mp4, _) = synthetic_mp4();
        // 8-byte IVs parsed as 16 in Tables mode: strict parsing must fail
        // loudly (overrun or trailing bytes — never silent garbage).
        let err = collect_fragments(&mp4, 16, None, SencMode::Tables).expect_err("must not parse");
        let msg = err.to_string();
        assert!(msg.contains("trailing") || msg.contains("overrun"), "{msg}");
    }

    #[test]
    fn decrypt_falls_back_to_working_iv_size() {
        // 16-byte-IV senc, no tenc: candidate 8 fails strict, 16 wins.
        let senc = box_(b"senc", &senc_payload(&[(vec![7; 16], vec![(1, 2)])]));
        let moof = box_(b"moof", &box_(b"traf", &senc));
        let mut mp4 = moof;
        mp4.extend_from_slice(&box_(b"mdat", b"Abb"));
        let out = decrypt_track(mp4, &[0xAA; 16], |buf, _, _, subs| {
            let mut o = buf.to_vec();
            for (c, e) in subs {
                for i in 0..*e as usize {
                    o[*c as usize + i] ^= 0xFF;
                }
            }
            Ok(o)
        })
        .unwrap();
        let mdat_off = out.windows(4).position(|w| w == b"mdat").unwrap() + 4;
        assert_eq!(&out[mdat_off..mdat_off + 3], b"A\x9d\x9d");
    }

    #[test]
    fn empty_subsample_table_parses_to_empty() {
        // subsample_count == 0: parses (lengths come from trun later).
        let mut p = vec![0, 0, 0, 0, 0, 0, 0, 1];
        p.extend_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8]); // iv
        p.extend_from_slice(&[0, 0]); // subsample_count = 0
        let keys = parse_senc(&p, 8, None, SencMode::Tables).unwrap();
        assert_eq!(keys.len(), 1);
        assert!(keys[0].subsamples.is_empty());
    }

    #[test]
    fn iv_only_decrypts_via_trun_sizes() {
        // The failing track's shape: bare IVs, lengths from trun.
        let out = decrypt_track(iv_only_mp4(), &[0xAA; 16], |buf, _, _, subs| {
            // Whole-sample single run expected.
            assert_eq!(subs.len(), 1);
            assert_eq!(subs[0].0, 0);
            Ok(buf.iter().map(|b| b ^ 0xFF).collect())
        })
        .unwrap();
        let mdat_off = out.windows(4).position(|w| w == b"mdat").unwrap() + 4;
        assert_eq!(
            &out[mdat_off..mdat_off + 9],
            b"\xbe\xbe\xbe\xbe\xbd\xbd\xbd\xbd\xbd"
        );
    }

    #[test]
    fn aligned_end_rounds_up_to_fragments() {
        // Two moofs: [2 samples] + [1 sample]. A midpoint cutoff must
        // extend to the enclosing fragment's end (decoders need whole
        // moof+mdat units — measured with symphonia).
        let senc1 = box_(
            b"senc",
            &senc_payload(&[(vec![1; 8], vec![(0, 2)]), (vec![2; 8], vec![(0, 2)])]),
        );
        let mut t1 = trun_box(&[2, 2]);
        t1.extend_from_slice(&senc1);
        let senc2 = box_(b"senc", &senc_payload(&[(vec![3; 8], vec![(0, 5)])]));
        let mut t2 = trun_box(&[5]);
        t2.extend_from_slice(&senc2);
        let mut mp4 = box_(b"moof", &box_(b"traf", &t1));
        mp4.extend_from_slice(&box_(b"mdat", b"AAAA"));
        mp4.extend_from_slice(&box_(b"moof", &box_(b"traf", &t2)));
        mp4.extend_from_slice(&box_(b"mdat", b"BBBBB"));
        let layout = select_layout(&mp4).unwrap();
        assert_eq!(layout.sample_count(), 3);
        assert_eq!(layout.aligned_end(0), 0);
        assert_eq!(layout.aligned_end(1), 2);
        assert_eq!(layout.aligned_end(2), 2);
        assert_eq!(layout.aligned_end(3), 3);
        assert_eq!(layout.aligned_end(99), 3);
    }

    #[test]
    fn select_layout_picks_tables_for_tabled_files() {
        let (mp4, _) = synthetic_mp4();
        let layout = select_layout(&mp4).unwrap();
        assert_eq!(layout.mode, SencMode::Tables);
        assert_eq!(layout.iv_size, 8);
        assert_eq!(layout.sample_count(), 2);
        assert_eq!(
            layout.prefix_end(1),
            layout.samples[0].start + layout.samples[0].len
        );
    }

    #[test]
    fn select_layout_picks_ivonly_for_bare_ivs() {
        let mp4 = iv_only_mp4();
        let layout = select_layout(&mp4).unwrap();
        assert_eq!(layout.mode, SencMode::IvOnly);
        assert_eq!(layout.iv_size, 8);
        assert_eq!(layout.sample_count(), 2);
        // Lengths resolved from trun: 4 + 5.
        assert_eq!(layout.samples[0].len, 4);
        assert_eq!(layout.samples[1].len, 5);
        assert_eq!(
            layout.prefix_end(2),
            layout.samples[1].start + layout.samples[1].len
        );
    }

    #[test]
    fn iv_only_without_trun_is_an_explicit_error() {
        // Same iv-only senc but no trun: lengths unknowable — loud error.
        let mut p = vec![0, 0, 0, 0, 0, 0, 0, 1];
        p.extend_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8]);
        let senc = box_(b"senc", &p);
        let moof = box_(b"moof", &box_(b"traf", &senc));
        let mut mp4 = moof;
        mp4.extend_from_slice(&box_(b"mdat", b"AAAA"));
        let err = decrypt_track(mp4, &[0xAA; 16], |buf, _, _, _| Ok(buf.to_vec()))
            .expect_err("no lengths available");
        assert!(err.to_string().contains("without trun sizes"), "{err}");
    }

    #[test]
    fn senc_trun_count_mismatch_is_an_explicit_error() {
        let senc = box_(
            b"senc",
            &senc_payload(&[(vec![1; 8], vec![(0, 2)]), (vec![2; 8], vec![(0, 2)])]),
        );
        let mut traf_payload = trun_box(&[2]); // only 1 size for 2 samples
        traf_payload.extend_from_slice(&senc);
        let moof = box_(b"moof", &box_(b"traf", &traf_payload));
        let mut mp4 = moof;
        mp4.extend_from_slice(&box_(b"mdat", b"AAAA"));
        let err = decrypt_track(mp4, &[0xAA; 16], |buf, _, _, _| Ok(buf.to_vec()))
            .expect_err("counts differ");
        assert!(err.to_string().contains("count mismatch"), "{err}");
    }

    #[test]
    fn senc_trun_size_mismatch_is_an_explicit_error() {
        let senc = box_(b"senc", &senc_payload(&[(vec![1; 8], vec![(1, 2)])]));
        let mut traf_payload = trun_box(&[9]); // subs say 1+2=3
        traf_payload.extend_from_slice(&senc);
        let moof = box_(b"moof", &box_(b"traf", &traf_payload));
        let mut mp4 = moof;
        mp4.extend_from_slice(&box_(b"mdat", b"AAAAAAAAA"));
        let err = decrypt_track(mp4, &[0xAA; 16], |buf, _, _, _| Ok(buf.to_vec()))
            .expect_err("sizes differ");
        assert!(err.to_string().contains("size mismatch"), "{err}");
    }

    #[test]
    fn describe_summarizes_structure() {
        let (mp4, _) = synthetic_mp4();
        let d = describe_track(&mp4);
        assert!(d.contains("moof"), "{d}");
        assert!(d.contains("mdat"), "{d}");
        assert!(d.contains("tenc=absent"), "{d}");
        assert!(d.contains("count=2"), "{d}");
        let with_tenc = mp4_with_tenc(0, 8);
        let d2 = describe_track(&with_tenc);
        assert!(d2.contains("tenc=v0 iv8"), "{d2}");
    }

    #[test]
    fn collects_and_decrypts_with_fake_cdm() {
        let (mp4, _) = synthetic_mp4();
        let frags = collect_fragments(&mp4, 8, None, SencMode::Tables).unwrap();
        assert_eq!(frags.len(), 1);
        assert_eq!(frags[0].1.len(), 1); // one traf
        assert_eq!(frags[0].1[0].keys.len(), 2);
        assert_eq!(frags[0].1[0].keys[0].len(), 6);

        // Fake decryptor: xor cipher runs with 0xFF, pass clear through.
        let out = decrypt_track(mp4, &[0xAA; 16], |buf, _kid, _iv, subs| {
            let mut o = buf.to_vec();
            let mut p = 0usize;
            for (c, e) in subs {
                p += *c as usize;
                for i in 0..*e as usize {
                    o[p + i] ^= 0xFF;
                }
                p += *e as usize;
            }
            Ok(o)
        })
        .unwrap();
        // mdat payload starts after moof + mdat header.
        let mdat_off = out.windows(4).position(|w| w == b"mdat").unwrap() + 4;
        assert_eq!(
            &out[mdat_off..mdat_off + 9],
            b"AA\x9d\x9d\x9d\x9d\xbc\xbc\xbc"
        );
    }

    #[test]
    fn relabel_only_touches_stsd() {
        let mut entry = vec![0, 0, 0, 32];
        entry.extend_from_slice(b"enca");
        entry.extend_from_slice(&[0u8; 24]);
        let mut stsd = vec![0, 0, 0, 0, 0, 0, 0, 1];
        stsd.extend_from_slice(&entry);
        let stsd_box = box_(b"stsd", &stsd);
        let stbl = box_(b"stbl", &stsd_box);
        let minf = box_(b"minf", &stbl);
        let mdia = box_(b"mdia", &minf);
        let trak = box_(b"trak", &mdia);
        let mut mp4 = box_(b"moov", &trak);
        mp4.extend_from_slice(&box_(b"mdat", b"xxencayy"));
        relabel_enca(&mut mp4);
        assert!(mp4.windows(4).any(|w| w == b"mp4a"));
        assert!(&mp4[mp4.len() - 8..] == b"xxencayy");
    }
}
