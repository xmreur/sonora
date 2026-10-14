//! Full pipeline: bearer → webPlayback → asset → license → decrypt.
//!
//! The caller passes a **catalog** Adam id (library `i.*` ids must be
//! resolved first via the existing catalog API). Desktop caches decrypted
//! bytes; a replay costs one disk read and no license round-trip.

use crate::cenc;
use crate::error::{PlaybackError, Result};
use crate::ids::is_library_id;
use crate::webplayback::{self, WebPlayback};
use crate::widevine::{self, Cdm};
use base64::Engine as _;

async fn download_asset(url: &str, media_user_token: &str) -> Result<Vec<u8>> {
    let client = crate::error::shared_client();
    // 1-byte probe: learns the length and whether ranges work, in one RTT.
    // Apple throttles single connections (~2MB/s observed); fanning big
    // files out over parallel ranges typically halves download time.
    let probe = asset_request(&client, url, media_user_token)
        .header("Range", "bytes=0-0")
        .send()
        .await
        .map_err(|e| PlaybackError::Network(format!("download asset: {e}")))?;
    if probe.status() == reqwest::StatusCode::PARTIAL_CONTENT {
        let total = probe
            .headers()
            .get("content-range")
            .and_then(|v| v.to_str().ok())
            .and_then(parse_total_from_content_range);
        match total {
            // Big file, ranges work: fan out (falls back to one GET below
            // when any part misbehaves).
            Some(n) if n >= DOWNLOAD_RANGE_MIN => {
                return download_ranged(&client, url, media_user_token, n).await;
            }
            // Tiny file or unknown length: one plain GET.
            _ => return download_single(&client, url, media_user_token).await,
        }
    }
    if probe.status().is_success() {
        // Server ignored Range: this response already holds the full body.
        return read_asset_body(probe).await;
    }
    Err(PlaybackError::Network(format!(
        "download asset HTTP {}",
        probe.status()
    )))
}

/// Concurrent byte ranges per asset download.
const DOWNLOAD_RANGES: u64 = 4;
// `download_ranged` fans out over exactly this many parts (see the
// `tokio::join!` there) — the two must stay in sync.
const _: () = assert!(DOWNLOAD_RANGES == 4);
/// Below this size the probe + fan-out costs more than it saves.
const DOWNLOAD_RANGE_MIN: u64 = 2 * 1024 * 1024;

fn asset_request(
    client: &reqwest::Client,
    url: &str,
    media_user_token: &str,
) -> reqwest::RequestBuilder {
    client
        .get(url)
        .header("User-Agent", "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36")
        .header("x-apple-music-user-token", media_user_token)
        .header("Cookie", format!("media-user-token={media_user_token}"))
}

/// `bytes 0-0/12345678` → `12345678`.
fn parse_total_from_content_range(v: &str) -> Option<u64> {
    v.rsplit('/').next()?.trim().parse().ok()
}

async fn download_single(
    client: &reqwest::Client,
    url: &str,
    media_user_token: &str,
) -> Result<Vec<u8>> {
    let resp = asset_request(client, url, media_user_token)
        .send()
        .await
        .map_err(|e| PlaybackError::Network(format!("download asset: {e}")))?
        .error_for_status()
        .map_err(|e| PlaybackError::Network(format!("download asset: {e}")))?;
    read_asset_body(resp).await
}

async fn read_asset_body(resp: reqwest::Response) -> Result<Vec<u8>> {
    let bytes = resp
        .bytes()
        .await
        .map_err(|e| PlaybackError::Network(format!("read asset: {e}")))?
        .to_vec();
    if bytes.is_empty() {
        return Err(PlaybackError::Network("asset download is empty".into()));
    }
    Ok(bytes)
}

async fn download_range(
    client: &reqwest::Client,
    url: &str,
    media_user_token: &str,
    start: u64,
    end: u64,
) -> Result<Vec<u8>> {
    let resp = asset_request(client, url, media_user_token)
        .header("Range", format!("bytes={start}-{end}"))
        .send()
        .await
        .map_err(|e| PlaybackError::Network(format!("download asset: {e}")))?;
    if resp.status() != reqwest::StatusCode::PARTIAL_CONTENT {
        return Err(PlaybackError::Network(format!(
            "range request unsupported (HTTP {})",
            resp.status()
        )));
    }
    let bytes = resp
        .bytes()
        .await
        .map_err(|e| PlaybackError::Network(format!("read asset: {e}")))?
        .to_vec();
    if bytes.len() as u64 != end - start + 1 {
        return Err(PlaybackError::Network("short range part".into()));
    }
    Ok(bytes)
}

async fn download_ranged(
    client: &reqwest::Client,
    url: &str,
    media_user_token: &str,
    total: u64,
) -> Result<Vec<u8>> {
    let chunk = total / DOWNLOAD_RANGES;
    let end_of = |i: u64| {
        if i + 1 == DOWNLOAD_RANGES {
            total - 1
        } else {
            (i + 1) * chunk - 1
        }
    };
    let (a, b, c, d) = tokio::join!(
        download_range(client, url, media_user_token, 0, end_of(0)),
        download_range(client, url, media_user_token, chunk, end_of(1)),
        download_range(client, url, media_user_token, 2 * chunk, end_of(2)),
        download_range(client, url, media_user_token, 3 * chunk, end_of(3)),
    );
    // Any part failing (flaky range support, short read) falls back to one
    // plain GET, whose error — if any — is the real one (auth, 404, ...).
    let (a, b, c, d) = match (a, b, c, d) {
        (Ok(a), Ok(b), Ok(c), Ok(d)) => (a, b, c, d),
        _ => return download_single(client, url, media_user_token).await,
    };
    let mut out = Vec::with_capacity(total as usize);
    out.extend_from_slice(&a);
    out.extend_from_slice(&b);
    out.extend_from_slice(&c);
    out.extend_from_slice(&d);
    if out.len() as u64 != total || out.is_empty() {
        return Err(PlaybackError::Network("ranged download mismatch".into()));
    }
    Ok(out)
}

/// License round-trip inputs.
struct LicenseArgs<'a> {
    cdm: &'a Cdm,
    session: &'a widevine::LicenseSession,
    cdm_session: &'a widevine::CdmSession,
    challenge: &'a [u8],
    adam_id: &'a str,
    uri_prefix: &'a str,
    kid_base64: &'a str,
    bearer_token: &'a str,
    media_user_token: &'a str,
}

async fn load_license(args: LicenseArgs<'_>) -> Result<()> {
    let LicenseArgs {
        cdm,
        session,
        cdm_session,
        challenge,
        adam_id,
        uri_prefix,
        kid_base64,
        bearer_token,
        media_user_token,
    } = args;
    let envelope = webplayback::license_envelope(
        &base64::engine::general_purpose::STANDARD.encode(challenge),
        uri_prefix,
        kid_base64,
        adam_id,
        is_library_id(adam_id),
    );
    let body = crate::error::shared_client()
        .post(webplayback::LICENSE_URL)
        .header("Content-Type", "application/json")
        .header("Origin", "https://music.apple.com")
        .header("Referer", "https://music.apple.com/")
        .header("Authorization", format!("Bearer {bearer_token}"))
        .header("x-apple-music-user-token", media_user_token)
        .header("Cookie", format!("media-user-token={media_user_token}"))
        .json(&envelope)
        .send()
        .await
        .map_err(|e| PlaybackError::License(format!("license request: {e}")))?
        .text()
        .await
        .map_err(|e| PlaybackError::License(format!("read license: {e}")))?;
    let v: serde_json::Value =
        serde_json::from_str(&body).map_err(|e| PlaybackError::License(format!("parse: {e}")))?;
    if let Some(code) = v["errorCode"].as_i64() {
        if code != 0 {
            return Err(PlaybackError::License(format!("license error code {code}")));
        }
    }
    let license_b64 = v["license"]
        .as_str()
        .ok_or_else(|| PlaybackError::License("no license in response".into()))?;
    let license = base64::engine::general_purpose::STANDARD
        .decode(license_b64)
        .map_err(|e| PlaybackError::License(format!("decode license: {e}")))?;
    cdm.update(session, cdm_session, &license)
        .map_err(PlaybackError::License)
}

/// Incrementally-decryptable track: everything fast (bearer, webPlayback,
/// download, license, layout selection) happens in `begin` (~1s); sample
/// decryption then proceeds caller-driven via `decrypt_next`, so playback
/// can start after the first chunklets while the rest decrypts behind it.
/// The CDM session (and its keys) lives as long as this struct.
pub struct ProgressiveCtx {
    adam_id: String,
    plain: Vec<u8>,
    kind: CtxKind,
    done: usize,
    from_cache: bool,
}

enum CtxKind {
    Encrypted {
        layout: cenc::SelectedLayout,
        live: Option<LiveDecryptor>,
    },
    /// Already-plain bytes (user upload): single chunk, no CDM involved.
    Plain,
}

struct LiveDecryptor {
    key_id: Vec<u8>,
    cdm: Cdm,
    _session: widevine::CdmSession,
}

impl ProgressiveCtx {
    /// Fast prefix of the pipeline: resolve, download, license, select
    /// layout. No per-sample decryption yet.
    ///
    /// Both catalog (Adam) ids and library (`i.*`/`l.*`/`a.*`/`p.*`) ids
    /// are accepted. Library ids dispatch via `universalLibraryId`: the
    /// only route for uploaded/unmatched iCloud tracks, which have no
    /// catalog mapping at all (their assets come back DRM-free).
    pub async fn begin(adam_id: &str, media_user_token: &str) -> Result<Self> {
        if adam_id.trim().is_empty() {
            return Err(PlaybackError::Resolve("empty track id".into()));
        }
        if let Some(mut cached) = crate::cache::load(adam_id) {
            cenc::relabel_enca(&mut cached);
            let layout = cenc::select_layout(&cached)?;
            return Ok(Self {
                adam_id: adam_id.to_string(),
                plain: cached,
                kind: CtxKind::Encrypted { layout, live: None },
                done: 0,
                from_cache: true,
            });
        }
        let t0 = std::time::Instant::now();
        let bearer = crate::bearer::get_bearer_token().await?;
        let bearer_ms = t0.elapsed().as_millis();
        let playback = webplayback::get_web_playback(adam_id, &bearer, media_user_token).await?;
        let resolve_ms = t0.elapsed().as_millis();
        match playback {
            WebPlayback::Plain { file_url } => {
                let plain = download_asset(&file_url, media_user_token).await?;
                eprintln!(
                    "sonora native: {adam_id} bearer={bearer_ms}ms webplayback={}ms plain download",
                    resolve_ms - bearer_ms,
                );
                Ok(Self {
                    adam_id: adam_id.to_string(),
                    plain,
                    kind: CtxKind::Plain,
                    done: 0,
                    from_cache: false,
                })
            }
            WebPlayback::Encrypted(info) => {
                let key_id = webplayback::decode_kid(&info.kid_base64)?;
                // The asset download and the CDM open + challenge are
                // independent: run them together, not back to back. Timed
                // separately so slow starts attribute to network vs CDM.
                let download = async {
                    let t = std::time::Instant::now();
                    let r = download_asset(&info.file_url, media_user_token).await;
                    (r, t.elapsed().as_millis())
                };
                let licence = async {
                    let t = std::time::Instant::now();
                    let cdm = Cdm::open_system().await.map_err(PlaybackError::Cdm)?;
                    let session = cdm.begin_license().await;
                    let init = widevine::build_pssh(&key_id);
                    let (challenge, cdm_session) = cdm
                        .challenge(&session, &init)
                        .map_err(PlaybackError::License)?;
                    Ok::<_, PlaybackError>((
                        cdm,
                        session,
                        challenge,
                        cdm_session,
                        t.elapsed().as_millis(),
                    ))
                };
                let t1 = std::time::Instant::now();
                let (dl_out, lic_out) = tokio::join!(download, licence);
                let (cipher, download_ms) = dl_out;
                let mut cipher = cipher?;
                let (cdm, session, challenge, cdm_session, cdm_ms) = lic_out?;
                let parallel_ms = t1.elapsed().as_millis();
                let t2 = std::time::Instant::now();
                load_license(LicenseArgs {
                    cdm: &cdm,
                    session: &session,
                    cdm_session: &cdm_session,
                    challenge: &challenge,
                    adam_id,
                    uri_prefix: &info.uri_prefix,
                    kid_base64: &info.kid_base64,
                    bearer_token: &bearer,
                    media_user_token,
                })
                .await?;
                let license_ms = t2.elapsed().as_millis();
                drop(session);
                cenc::relabel_enca(&mut cipher);
                let layout = cenc::select_layout(&cipher)?;
                eprintln!(
                    "sonora native: {adam_id} bearer={bearer_ms}ms webplayback={}ms download={download_ms}ms cdm={cdm_ms}ms (wall {parallel_ms}ms) license={license_ms}ms layout={:?}/iv{}",
                    resolve_ms - bearer_ms,
                    layout.mode,
                    layout.iv_size,
                );
                Ok(Self {
                    adam_id: adam_id.to_string(),
                    plain: cipher,
                    kind: CtxKind::Encrypted {
                        layout,
                        live: Some(LiveDecryptor {
                            key_id,
                            cdm,
                            _session: cdm_session,
                        }),
                    },
                    done: 0,
                    from_cache: false,
                })
            }
        }
    }

    pub fn total_samples(&self) -> usize {
        match &self.kind {
            CtxKind::Plain => 1,
            CtxKind::Encrypted { layout, .. } => layout.sample_count(),
        }
    }

    pub fn done_samples(&self) -> usize {
        self.done
    }

    /// Byte end of the prefix covering the first `n` samples (a valid fMP4
    /// prefix for decode).
    pub fn prefix_end(&self, n: usize) -> usize {
        match &self.kind {
            CtxKind::Plain => self.plain.len(),
            CtxKind::Encrypted { layout, .. } => layout.prefix_end(n),
        }
    }

    pub fn plaintext_prefix(&self, end: usize) -> &[u8] {
        &self.plain[..end.min(self.plain.len())]
    }

    pub fn adam_id(&self) -> &str {
        &self.adam_id
    }

    pub fn from_cache(&self) -> bool {
        self.from_cache
    }

    /// Decrypt up to `n` more samples in place, rounded up to whole
    /// fragments (see `aligned_end`). Sync and blocking per CDM call —
    /// callers run this on a blocking thread. No-op once complete
    /// (disk-cached and plain paths).
    pub fn decrypt_next(&mut self, n: usize) -> Result<()> {
        let total = self.total_samples();
        if self.done >= total {
            return Ok(());
        }
        let end = match &self.kind {
            CtxKind::Plain => total,
            CtxKind::Encrypted { layout, .. } => layout.aligned_end(self.done + n),
        }
        .min(total);
        match &mut self.kind {
            CtxKind::Plain => {}
            CtxKind::Encrypted { layout, live } => match live {
                None => {}
                Some(state) => {
                    let buf = &mut self.plain;
                    let cdm = &state.cdm;
                    let key_id = &state.key_id;
                    cenc::decrypt_samples(
                        buf,
                        key_id,
                        layout,
                        self.done..end,
                        &mut |data, kid, iv, subs| {
                            cdm.decrypt(data, kid, iv, subs)
                                .map_err(PlaybackError::Decrypt)
                        },
                    )?;
                }
            },
        }
        self.done = end;
        Ok(())
    }

    /// Full plaintext (only complete once `done_samples == total_samples`).
    /// Callers persist it unless `from_cache`.
    pub fn finish(self) -> Vec<u8> {
        self.plain
    }
}

/// Resolve + download + decrypt one catalog track. Returns playable m4a bytes.
pub async fn resolve_and_decrypt(adam_id: &str, media_user_token: &str) -> Result<Vec<u8>> {
    let mut ctx = ProgressiveCtx::begin(adam_id, media_user_token).await?;
    let total = ctx.total_samples();
    ctx.decrypt_next(total)?;
    let store = !ctx.from_cache();
    let id = ctx.adam_id().to_string();
    let plain = ctx.finish();
    if store {
        crate::cache::store(&id, &plain);
    }
    Ok(plain)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_range_total_parsed() {
        assert_eq!(
            parse_total_from_content_range("bytes 0-0/12345678"),
            Some(12345678)
        );
        assert_eq!(parse_total_from_content_range("bytes */42"), Some(42));
        assert_eq!(parse_total_from_content_range("bytes 0-0/*"), None);
        assert_eq!(parse_total_from_content_range("garbage"), None);
        assert_eq!(parse_total_from_content_range("bytes 0-0/"), None);
    }
}
