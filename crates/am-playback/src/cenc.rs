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

/// `Per_Sample_IV_Size` from the first `tenc` box, defaulting to 8.
pub fn default_iv_size(mp4: &[u8]) -> usize {
    fn find_tenc(data: &[u8], off: usize, len: usize) -> Option<usize> {
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
            if &t == b"tenc" && plen > 4 + 3 + 1 {
                let v = data[start + 4 + 3 + 1];
                if v != 0 {
                    return Some(v as usize);
                }
            }
            if matches!(
                &t,
                b"moov" | b"trak" | b"mdia" | b"minf" | b"stbl" | b"stsd"
            ) {
                if let Some(v) = find_tenc(data, start, plen) {
                    return Some(v);
                }
            }
        }
        None
    }
    let len = mp4.len();
    find_tenc(mp4, 0, len).unwrap_or(8)
}

/// Parse one `senc` payload (after size+type) with the given IV size.
pub fn parse_senc(payload: &[u8], iv_size: usize) -> Result<Vec<SampleKey>> {
    if payload.len() < 4 + 4 {
        return Err(PlaybackError::Decode("senc too short".into()));
    }
    let _flags = u32be(&payload[0..4]);
    let count = u32be(&payload[4..8]) as usize;
    let mut off = 8usize;
    let mut out = Vec::with_capacity(count.min(4096));
    for _ in 0..count {
        if off + iv_size > payload.len() {
            return Err(PlaybackError::Decode("senc iv overrun".into()));
        }
        let iv = payload[off..off + iv_size].to_vec();
        off += iv_size;
        let mut subs = vec![(0u32, 0u32)];
        // version 0 carries an explicit subsample table.
        if off + 2 <= payload.len() {
            // Heuristic: if remaining bytes exactly match iv-only layout
            // (no subsample counts), treat whole sample as encrypted.
            let remaining_samples = count - out.len() - 1;
            let rest_for_ivs = remaining_samples * iv_size;
            if payload.len() - off - 2 == rest_for_ivs {
                // No subsample table at all — whole sample encrypted.
                subs = vec![(0, u32::MAX)];
            } else {
                let n = u16be(&payload[off..off + 2]) as usize;
                off += 2;
                subs.clear();
                for _ in 0..n {
                    if off + 6 > payload.len() {
                        return Err(PlaybackError::Decode("senc subsample overrun".into()));
                    }
                    let clear = u16be(&payload[off..off + 2]) as u32;
                    let cipher = u32be(&payload[off + 2..off + 6]);
                    off += 6;
                    subs.push((clear, cipher));
                }
            }
        }
        out.push(SampleKey {
            iv,
            subsamples: subs,
        });
    }
    Ok(out)
}

/// Collect `(mdat_payload_range, samples)` per `moof` in file order.
/// Each `moof`'s samples map onto the `mdat` that follows it.
pub fn collect_fragments(
    mp4: &[u8],
    iv_size: usize,
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
                            samples.extend(parse_senc(p2, iv_size)?);
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

/// Decrypt one track in place. `decrypt` maps
/// `(ciphertext, key_id, iv, subsamples) -> plaintext`.
pub fn decrypt_track(
    mut data: Vec<u8>,
    key_id: &[u8],
    iv_size: usize,
    mut decrypt: impl FnMut(&[u8], &[u8], &[u8], &[(u32, u32)]) -> Result<Vec<u8>>,
) -> Result<Vec<u8>> {
    let frags = collect_fragments(&data, iv_size)?;
    for (range, samples) in frags {
        let mut pos = range.start;
        for s in &samples {
            let total = s.len();
            if pos + total > range.end {
                return Err(PlaybackError::Decode("senc sizes exceed mdat".into()));
            }
            // Resolve whole-sample-encrypted marker.
            let subs: Vec<(u32, u32)> = if s.subsamples == [(0, u32::MAX)] {
                vec![(0, total as u32)]
            } else {
                s.subsamples.clone()
            };
            let plain = decrypt(&data[pos..pos + total], key_id, &s.iv, &subs)?;
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

    #[test]
    fn collects_and_decrypts_with_fake_cdm() {
        let (mp4, _) = synthetic_mp4();
        let frags = collect_fragments(&mp4, 8).unwrap();
        assert_eq!(frags.len(), 1);
        assert_eq!(frags[0].1.len(), 2);
        assert_eq!(frags[0].1[0].len(), 6);

        // Fake decryptor: xor cipher runs with 0xFF, pass clear through.
        let out = decrypt_track(mp4, &[0xAA; 16], 8, |buf, _kid, _iv, subs| {
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
        // stsd with one enca entry + mdat containing the literal bytes "enca".
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
