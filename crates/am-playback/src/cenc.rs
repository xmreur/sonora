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

/// Parse one `senc` payload (after size+type) strictly: every byte must be
/// accounted for, otherwise the IV size is wrong and parsing — not silent
/// garbage — is reported. `subsample_count == 0` (whole sample encrypted,
/// no table) is rejected explicitly; resolving those needs `trun` sizes.
pub fn parse_senc(
    payload: &[u8],
    iv_size: usize,
    constant_iv: Option<&[u8]>,
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
        if n == 0 {
            return Err(PlaybackError::Decode(
                "fully-encrypted sample without subsample table (needs trun sizes)".into(),
            ));
        }
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

/// Collect `(mdat_payload_range, samples)` per `moof` in file order.
/// Each `moof`'s samples map onto the `mdat` that follows it.
pub fn collect_fragments(
    mp4: &[u8],
    iv_size: usize,
    constant_iv: Option<&[u8]>,
) -> Result<Vec<(std::ops::Range<usize>, Vec<SampleKey>)>> {
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
            // Gather senc samples from every traf inside.
            let mut samples = Vec::new();
            let (moof_off, moof_len) = (tops[i].1, tops[i].2);
            for (t, p) in child_boxes(mp4, moof_off, moof_len) {
                if &t == b"traf" {
                    let base = p.as_ptr() as usize - mp4.as_ptr() as usize;
                    for (t2, p2) in child_boxes(mp4, base, p.len()) {
                        if &t2 == b"senc" {
                            samples.extend(parse_senc(p2, iv_size, constant_iv)?);
                        }
                    }
                }
            }
            // The mdat that follows this moof holds its sample bytes.
            let mut j = i + 1;
            while j < tops.len() && &tops[j].0 != b"mdat" && &tops[j].0 != b"moof" {
                j += 1;
            }
            if !samples.is_empty() {
                if j < tops.len() && &tops[j].0 == b"mdat" {
                    let (mo, ml) = (tops[j].1, tops[j].2);
                    frags.push((mo..mo + ml, samples));
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

/// Decrypt one track. The IV size comes from `tenc` when present, else
/// strict `[8, 16]` attempts (exact-consumption parsing rejects the wrong
/// size). `decrypt` maps `(ciphertext, key_id, iv, subsamples) -> plaintext`.
pub fn decrypt_track(
    data: Vec<u8>,
    key_id: &[u8],
    mut decrypt: impl FnMut(&[u8], &[u8], &[u8], &[(u32, u32)]) -> Result<Vec<u8>>,
) -> Result<Vec<u8>> {
    let (sizes, constant_iv) = iv_candidates(&data);
    let mut errors = Vec::new();
    for iv_size in sizes {
        match try_decrypt(&data, key_id, iv_size, constant_iv.as_deref(), &mut decrypt) {
            Ok(out) => return Ok(out),
            Err(e) => errors.push(format!("iv_size={iv_size}: {e}")),
        }
    }
    eprintln!("sonora native: senc diagnosis: {}", describe_track(&data));
    Err(PlaybackError::Decode(format!(
        "no IV size parsed this track ({})",
        errors.join("; ")
    )))
}

fn try_decrypt(
    data: &[u8],
    key_id: &[u8],
    iv_size: usize,
    constant_iv: Option<&[u8]>,
    decrypt: &mut impl FnMut(&[u8], &[u8], &[u8], &[(u32, u32)]) -> Result<Vec<u8>>,
) -> Result<Vec<u8>> {
    let mut data = data.to_vec();
    let frags = collect_fragments(&data, iv_size, constant_iv)?;
    for (range, samples) in frags {
        let mut pos = range.start;
        for s in &samples {
            if s.subsamples.is_empty() {
                return Err(PlaybackError::Decode(
                    "fully-encrypted sample without subsample table (needs trun sizes)".into(),
                ));
            }
            let total = s.len();
            if pos + total > range.end {
                return Err(PlaybackError::Decode("senc sizes exceed mdat".into()));
            }
            let plain = decrypt(&data[pos..pos + total], key_id, &s.iv, &s.subsamples)?;
            if plain.len() != total {
                return Err(PlaybackError::Decode(
                    "decryptor returned wrong length".into(),
                ));
            }
            data[pos..pos + total].copy_from_slice(&plain);
            pos += total;
        }
    }
    relabel_enca(&mut data);
    Ok(data)
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
        // moof(traf(senc[2 samples])) + mdat(sample bytes).
        let senc = box_(
            b"senc",
            &senc_payload(&[
                (vec![1, 2, 3, 4, 5, 6, 7, 8], vec![(2, 4)]),
                (vec![9, 9, 9, 9, 9, 9, 9, 9], vec![(0, 3)]),
            ]),
        );
        let traf = box_(b"traf", &senc);
        let moof = box_(b"moof", &traf);
        let media = b"AAbbbbCCC".to_vec(); // 2+4, then 3
        let mut mp4 = moof;
        mp4.extend_from_slice(&box_(b"mdat", &media));
        (mp4, media)
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
        let frags = collect_fragments(&mp4, 8, None).unwrap();
        assert_eq!(frags[0].1.len(), 2);
    }

    #[test]
    fn wrong_iv_size_is_rejected_not_misparsed() {
        let (mp4, _) = synthetic_mp4();
        // 8-byte IVs parsed as 16: strict parsing must fail loudly
        // (overrun or trailing bytes — never silent garbage).
        let err = collect_fragments(&mp4, 16, None).expect_err("must not parse");
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
    fn empty_subsample_table_is_an_explicit_error() {
        // subsample_count == 0: fail loudly instead of desyncing.
        let mut p = vec![0, 0, 0, 0, 0, 0, 0, 1];
        p.extend_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8]); // iv
        p.extend_from_slice(&[0, 0]); // subsample_count = 0
        let err = parse_senc(&p, 8, None).expect_err("n==0 must fail");
        assert!(err.to_string().contains("fully-encrypted"), "{err}");
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
        let frags = collect_fragments(&mp4, 8, None).unwrap();
        assert_eq!(frags.len(), 1);
        assert_eq!(frags[0].1.len(), 2);
        assert_eq!(frags[0].1[0].len(), 6);

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
