//! Full pipeline: bearer → webPlayback → asset → license → decrypt.
//!
//! The caller passes a **catalog** Adam id (library `i.*` ids must be
//! resolved first via the existing catalog API). Desktop caches decrypted
//! bytes; a replay costs one disk read and no license round-trip.
//!
//! Encrypted assets are not buffered whole before playback. The head is
//! pulled in small ranges until the first complete fragment is in hand;
//! decryption starts then, and the rest of the file keeps downloading
//! behind it.

use crate::cenc;
use crate::error::{PlaybackError, Result};
use crate::ids::is_library_id;
use crate::webplayback::{self, WebPlayback};
use crate::widevine::{self, Cdm};
use base64::Engine as _;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::Notify;

/// Bytes per sequential head request. Small enough that the first fragment
/// (init + one moof/mdat) usually lands in a couple of round-trips.
const HEAD_CHUNK: u64 = 256 * 1024;

/// Concurrent byte ranges for the tail of a large asset.
/// `fill_parallel` joins exactly this many range futures.
const DOWNLOAD_RANGES: u64 = 4;
const _: () = assert!(DOWNLOAD_RANGES == 4);
/// Below this size the probe + fan-out costs more than it saves.
const DOWNLOAD_RANGE_MIN: u64 = 2 * 1024 * 1024;

/// What `begin` is allowed to return on.
#[derive(Clone, Copy)]
enum FetchMode {
    /// Signal the head gate once a playable fragment is buffered, then
    /// keep filling. Playback starts while the tail is still in flight.
    UntilFragment,
    /// Do not signal early. Plain uploads have no fragment table, so the
    /// caller waits until every byte is in.
    All,
}

enum Gate {
    Head,
    Done,
}

struct AssetInner {
    bytes: Vec<u8>,
    /// Merged half-open spans that have been written. The playable prefix
    /// is only the span that starts at 0 — a later range finishing first
    /// must not look like a contiguous file.
    spans: Vec<(u64, u64)>,
    total: Option<u64>,
    error: Option<PlaybackError>,
    head_ready: bool,
    done: bool,
}

struct AssetBuf {
    inner: Mutex<AssetInner>,
    notify: Notify,
    cancel: AtomicBool,
}

fn clone_playback_error(err: &PlaybackError) -> PlaybackError {
    match err {
        PlaybackError::Network(s) => PlaybackError::Network(s.clone()),
        PlaybackError::Auth(s) => PlaybackError::Auth(s.clone()),
        PlaybackError::Resolve(s) => PlaybackError::Resolve(s.clone()),
        PlaybackError::License(s) => PlaybackError::License(s.clone()),
        PlaybackError::Decrypt(s) => PlaybackError::Decrypt(s.clone()),
        PlaybackError::Decode(s) => PlaybackError::Decode(s.clone()),
        PlaybackError::Audio(s) => PlaybackError::Audio(s.clone()),
        PlaybackError::Unsupported(s) => PlaybackError::Unsupported(s.clone()),
        PlaybackError::Cdm(s) => PlaybackError::Cdm(s.clone()),
    }
}

/// Insert `[start, end)` and merge overlaps. Order of arrival does not matter.
fn insert_span(spans: &mut Vec<(u64, u64)>, start: u64, end: u64) {
    if start >= end {
        return;
    }
    spans.push((start, end));
    spans.sort_unstable();
    let mut merged: Vec<(u64, u64)> = Vec::with_capacity(spans.len());
    for (start, end) in spans.drain(..) {
        if let Some(last) = merged.last_mut() {
            if start <= last.1 {
                last.1 = last.1.max(end);
                continue;
            }
        }
        merged.push((start, end));
    }
    *spans = merged;
}

/// End offset of the filled span that begins at byte 0.
fn contiguous_ready(spans: &[(u64, u64)]) -> u64 {
    match spans.first() {
        Some(&(0, end)) => end,
        _ => 0,
    }
}

/// Inclusive ranges covering `[start, total)` in `parts` pieces.
fn split_inclusive(start: u64, total: u64, parts: u64) -> Vec<(u64, u64)> {
    if start >= total || parts == 0 {
        return Vec::new();
    }
    let len = total - start;
    let parts = parts.min(len);
    let chunk = len / parts;
    let mut out = Vec::with_capacity(parts as usize);
    for i in 0..parts {
        let s = start + i * chunk;
        let e = if i + 1 == parts {
            total - 1
        } else {
            start + (i + 1) * chunk - 1
        };
        out.push((s, e));
    }
    out
}

impl AssetBuf {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(AssetInner {
                bytes: Vec::new(),
                spans: Vec::new(),
                total: None,
                error: None,
                head_ready: false,
                done: false,
            }),
            notify: Notify::new(),
            cancel: AtomicBool::new(false),
        })
    }

    fn complete(bytes: Vec<u8>) -> Arc<Self> {
        let n = bytes.len() as u64;
        Arc::new(Self {
            inner: Mutex::new(AssetInner {
                bytes,
                spans: if n == 0 { Vec::new() } else { vec![(0, n)] },
                total: Some(n),
                error: None,
                head_ready: true,
                done: true,
            }),
            notify: Notify::new(),
            cancel: AtomicBool::new(false),
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, AssetInner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn wake(&self) {
        self.notify.notify_waiters();
    }

    fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }

    fn allocate(&self, total: u64) {
        let mut g = self.lock();
        g.total = Some(total);
        if g.bytes.len() < total as usize {
            g.bytes.resize(total as usize, 0);
        }
    }

    fn reset(&self) {
        let mut g = self.lock();
        g.bytes.clear();
        g.spans.clear();
        g.total = None;
        g.done = false;
        g.head_ready = false;
    }

    fn fail(&self, err: PlaybackError) {
        let mut g = self.lock();
        if g.error.is_none() {
            g.error = Some(err);
        }
        drop(g);
        self.wake();
    }

    fn pull_error(&self) -> Result<()> {
        let g = self.lock();
        match &g.error {
            Some(err) => Err(clone_playback_error(err)),
            None => Ok(()),
        }
    }

    fn is_complete(&self) -> bool {
        self.lock().done
    }

    fn head_is_ready(&self) -> bool {
        let g = self.lock();
        g.head_ready || g.done
    }

    fn set_head_ready(&self) {
        let mut g = self.lock();
        g.head_ready = true;
        drop(g);
        self.wake();
    }

    fn mark_done(&self) -> Result<()> {
        let mut g = self.lock();
        if let Some(err) = &g.error {
            return Err(clone_playback_error(err));
        }
        let total = g.total.unwrap_or(0);
        let ready = contiguous_ready(&g.spans);
        if total == 0 || ready < total {
            return Err(PlaybackError::Network(format!(
                "asset short ({ready}/{total})"
            )));
        }
        g.done = true;
        g.head_ready = true;
        drop(g);
        self.wake();
        Ok(())
    }

    /// Unknown-length body finished. `len` is the number of bytes received.
    fn finish_len(&self, len: u64) {
        let mut g = self.lock();
        g.total = Some(len);
        if g.bytes.len() > len as usize {
            g.bytes.truncate(len as usize);
        }
        g.done = true;
        g.head_ready = true;
        drop(g);
        self.wake();
    }

    fn write_at(&self, start: u64, data: &[u8]) -> Result<()> {
        if data.is_empty() {
            return Ok(());
        }
        let mut g = self.lock();
        if let Some(err) = &g.error {
            return Err(clone_playback_error(err));
        }
        let end = start + data.len() as u64;
        if let Some(total) = g.total {
            if end > total {
                return Err(PlaybackError::Network("range past end of asset".into()));
            }
        }
        let need = end as usize;
        if g.bytes.len() < need {
            g.bytes.resize(need, 0);
        }
        g.bytes[start as usize..need].copy_from_slice(data);
        insert_span(&mut g.spans, start, end);
        let ready = contiguous_ready(&g.spans);
        if g.total.is_some_and(|total| total > 0 && ready >= total) {
            g.done = true;
            g.head_ready = true;
        }
        drop(g);
        self.wake();
        Ok(())
    }

    fn write_range(&self, start: usize, data: &[u8]) -> Result<()> {
        self.write_at(start as u64, data)
    }

    fn copy_range(&self, start: usize, len: usize) -> Result<Vec<u8>> {
        let g = self.lock();
        let ready = contiguous_ready(&g.spans) as usize;
        if start.saturating_add(len) > ready || start.saturating_add(len) > g.bytes.len() {
            return Err(PlaybackError::Decode(
                "sample extends past downloaded prefix".into(),
            ));
        }
        Ok(g.bytes[start..start + len].to_vec())
    }

    fn copy_prefix(&self, end: usize) -> Vec<u8> {
        let g = self.lock();
        let ready = contiguous_ready(&g.spans) as usize;
        let end = end.min(ready).min(g.bytes.len());
        g.bytes[..end].to_vec()
    }

    fn snapshot(&self) -> (Vec<u8>, bool) {
        let g = self.lock();
        let ready = contiguous_ready(&g.spans) as usize;
        let ready = ready.min(g.bytes.len());
        (g.bytes[..ready].to_vec(), g.done)
    }

    fn progress(&self) -> (u64, u64) {
        let g = self.lock();
        let ready = contiguous_ready(&g.spans);
        let total = g.total.unwrap_or(ready);
        (ready, total)
    }

    fn relabel_ready(&self) {
        let mut g = self.lock();
        let ready = contiguous_ready(&g.spans) as usize;
        let ready = ready.min(g.bytes.len());
        cenc::relabel_enca(&mut g.bytes[..ready]);
    }

    /// Next unfilled half-open hole that begins at the contiguous prefix.
    fn next_hole(&self) -> Option<(u64, u64)> {
        let g = self.lock();
        let total = g.total?;
        let ready = contiguous_ready(&g.spans);
        if ready >= total {
            return None;
        }
        let end = g
            .spans
            .iter()
            .find(|(start, _)| *start > ready)
            .map(|(start, _)| *start)
            .unwrap_or(total);
        Some((ready, end))
    }

    async fn wait(&self, gate: Gate) -> Result<()> {
        loop {
            let notified = self.notify.notified();
            tokio::pin!(notified);
            {
                let g = self.lock();
                if let Some(err) = &g.error {
                    return Err(clone_playback_error(err));
                }
                let open = match gate {
                    Gate::Head => g.head_ready || g.done,
                    Gate::Done => g.done,
                };
                if open {
                    return Ok(());
                }
            }
            notified.await;
        }
    }
}

/// Armed separately from [`ProgressiveCtx`] so a download wait does not
/// hold a borrow across `poll_prepare`.
pub struct ProgressWait {
    buf: Arc<AssetBuf>,
}

impl ProgressWait {
    pub fn notified(&self) -> tokio::sync::futures::Notified<'_> {
        self.buf.notify.notified()
    }
}

struct FetchRun {
    buf: Arc<AssetBuf>,
    handle: tokio::task::JoinHandle<()>,
}

impl Drop for FetchRun {
    fn drop(&mut self) {
        self.buf.cancel.store(true, Ordering::Relaxed);
        self.buf.wake();
        self.handle.abort();
    }
}

fn spawn_fetch(url: &str, media_user_token: &str, mode: FetchMode) -> FetchRun {
    let buf = AssetBuf::new();
    let url = url.to_string();
    let token = media_user_token.to_string();
    let task_buf = Arc::clone(&buf);
    let handle = tokio::spawn(async move {
        if let Err(err) = fetch_body(&task_buf, &url, &token, mode).await {
            task_buf.fail(err);
        }
    });
    FetchRun { buf, handle }
}

async fn fetch_body(buf: &Arc<AssetBuf>, url: &str, token: &str, mode: FetchMode) -> Result<()> {
    let client = crate::error::shared_client();
    // 1-byte probe: learns the length and whether ranges work, in one RTT.
    // Apple throttles single connections (~2MB/s observed); the tail of a
    // big file is still fanned out over parallel ranges.
    let probe = asset_request(&client, url, token)
        .header("Range", "bytes=0-0")
        .send()
        .await
        .map_err(|e| PlaybackError::Network(format!("download asset: {e}")))?;
    if probe.status() == reqwest::StatusCode::PARTIAL_CONTENT {
        if let Some(total) = probe
            .headers()
            .get("content-range")
            .and_then(|v| v.to_str().ok())
            .and_then(parse_total_from_content_range)
            .filter(|n| *n > 0)
        {
            buf.allocate(total);
            if matches!(mode, FetchMode::All) && total >= DOWNLOAD_RANGE_MIN {
                return fill_parallel(buf, &client, url, token, 0, total, true).await;
            }
            return pull_head(buf, &client, url, token, total, mode).await;
        }
        // 206 without a usable length: the body is the 1-byte probe, not the
        // asset. Fall through to a normal GET.
        return stream_single(buf, &client, url, token, mode).await;
    }
    if probe.status().is_success() {
        // Server ignored Range: this response already holds the full body.
        let bytes = read_asset_body(probe).await?;
        let n = bytes.len() as u64;
        buf.allocate(n);
        buf.write_at(0, &bytes)?;
        return buf.mark_done();
    }
    if buf.cancelled() {
        return Ok(());
    }
    stream_single(buf, &client, url, token, mode).await
}

/// Sequential head chunks. `UntilFragment` returns the head gate as soon as
/// one moof/mdat pair is complete and lets the tail continue in parallel.
async fn pull_head(
    buf: &Arc<AssetBuf>,
    client: &reqwest::Client,
    url: &str,
    token: &str,
    total: u64,
    mode: FetchMode,
) -> Result<()> {
    let mut cursor = 0u64;
    let mut stop_early = matches!(mode, FetchMode::UntilFragment);
    while cursor < total {
        if buf.cancelled() {
            return Ok(());
        }
        let end = (cursor + HEAD_CHUNK).min(total) - 1;
        match download_range(client, url, token, cursor, end).await {
            Ok(bytes) => {
                buf.write_at(cursor, &bytes)?;
                cursor += bytes.len() as u64;
            }
            Err(_) if !buf.head_is_ready() => {
                // Ranges looked fine on the probe and then failed. Nothing
                // has been decrypted yet, so start over with one GET.
                buf.reset();
                return stream_single(buf, client, url, token, mode).await;
            }
            Err(err) => return Err(err),
        }
        if stop_early && head_available(buf)? {
            buf.set_head_ready();
            stop_early = false;
            if cursor < total && total >= DOWNLOAD_RANGE_MIN {
                return fill_parallel(buf, client, url, token, cursor, total, false).await;
            }
        }
    }
    buf.mark_done()
}

fn head_available(buf: &AssetBuf) -> Result<bool> {
    let (prefix, done) = buf.snapshot();
    Ok(cenc::layout_prefix(&prefix, done, None)?.is_some())
}

async fn fill_parallel(
    buf: &Arc<AssetBuf>,
    client: &reqwest::Client,
    url: &str,
    token: &str,
    start: u64,
    total: u64,
    allow_reset: bool,
) -> Result<()> {
    if start >= total {
        return buf.mark_done();
    }
    let mut parts = split_inclusive(start, total, DOWNLOAD_RANGES).into_iter();
    let (a, b, c, d) = tokio::join!(
        maybe_range(client, url, token, parts.next()),
        maybe_range(client, url, token, parts.next()),
        maybe_range(client, url, token, parts.next()),
        maybe_range(client, url, token, parts.next()),
    );
    let mut failed = false;
    for part in [a, b, c, d] {
        match part {
            Ok(Some((at, bytes))) => buf.write_at(at, &bytes)?,
            Ok(None) => {}
            Err(_) => failed = true,
        }
    }
    match fill_holes(buf, client, url, token).await {
        Ok(()) => Ok(()),
        Err(_) if failed && allow_reset && !buf.head_is_ready() => {
            buf.reset();
            stream_single(buf, client, url, token, FetchMode::All).await
        }
        Err(err) => Err(err),
    }
}

async fn maybe_range(
    client: &reqwest::Client,
    url: &str,
    token: &str,
    part: Option<(u64, u64)>,
) -> Result<Option<(u64, Vec<u8>)>> {
    let Some((start, end)) = part else {
        return Ok(None);
    };
    let bytes = download_range(client, url, token, start, end).await?;
    Ok(Some((start, bytes)))
}

async fn fill_holes(
    buf: &Arc<AssetBuf>,
    client: &reqwest::Client,
    url: &str,
    token: &str,
) -> Result<()> {
    while let Some((start, end_excl)) = buf.next_hole() {
        if buf.cancelled() {
            return Ok(());
        }
        let mut cursor = start;
        while cursor < end_excl {
            let end = (cursor + HEAD_CHUNK).min(end_excl) - 1;
            let bytes = download_range(client, url, token, cursor, end).await?;
            buf.write_at(cursor, &bytes)?;
            cursor += bytes.len() as u64;
        }
    }
    buf.mark_done()
}

async fn stream_single(
    buf: &Arc<AssetBuf>,
    client: &reqwest::Client,
    url: &str,
    token: &str,
    mode: FetchMode,
) -> Result<()> {
    let mut resp = asset_request(client, url, token)
        .send()
        .await
        .map_err(|e| PlaybackError::Network(format!("download asset: {e}")))?
        .error_for_status()
        .map_err(|e| PlaybackError::Network(format!("download asset: {e}")))?;
    if let Some(total) = resp.content_length().filter(|n| *n > 0) {
        buf.allocate(total);
    }
    let mut off = 0u64;
    while let Some(chunk) = resp
        .chunk()
        .await
        .map_err(|e| PlaybackError::Network(format!("read asset: {e}")))?
    {
        if buf.cancelled() {
            return Ok(());
        }
        buf.write_at(off, &chunk)?;
        off += chunk.len() as u64;
        if matches!(mode, FetchMode::UntilFragment) && !buf.head_is_ready() && head_available(buf)?
        {
            buf.set_head_ready();
        }
    }
    if off == 0 {
        return Err(PlaybackError::Network("asset download is empty".into()));
    }
    buf.finish_len(off);
    Ok(())
}

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

/// How far the next decrypt step can go.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Prepare {
    /// Need more downloaded bytes before another chunklet exists.
    Pending,
    /// `decrypt_next` can take the next chunklet.
    Ready,
    /// Download finished and every sample is decrypted.
    Drained,
}

/// Incrementally-decryptable track. `begin` returns once the first complete
/// fragment is downloaded and licensed; sample decryption then proceeds via
/// `decrypt_next` while the rest of the asset keeps arriving. The CDM
/// session (and its keys) lives as long as this struct. Dropping it aborts
/// an in-flight download.
pub struct ProgressiveCtx {
    /// Dropped first so the fetch task is aborted while `buf` is still alive.
    fetch: Option<FetchRun>,
    buf: Arc<AssetBuf>,
    adam_id: String,
    kind: CtxKind,
    done: usize,
    from_cache: bool,
    /// Set once `layout` was built from the finished file and will not grow.
    layout_final: bool,
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
    /// Resolve, license, and download until the first playable fragment.
    /// The rest of an encrypted asset keeps downloading after this returns.
    /// No per-sample decryption yet.
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
                fetch: None,
                buf: AssetBuf::complete(cached),
                adam_id: adam_id.to_string(),
                kind: CtxKind::Encrypted { layout, live: None },
                done: 0,
                from_cache: true,
                layout_final: true,
            });
        }
        let t0 = std::time::Instant::now();
        let bearer = crate::bearer::get_bearer_token().await?;
        let bearer_ms = t0.elapsed().as_millis();
        let playback = webplayback::get_web_playback(adam_id, &bearer, media_user_token).await?;
        let resolve_ms = t0.elapsed().as_millis();
        match playback {
            WebPlayback::Plain { file_url } => {
                // No fragment table: playback still waits for the whole file.
                let fetch = spawn_fetch(&file_url, media_user_token, FetchMode::All);
                let t_dl = std::time::Instant::now();
                fetch.buf.wait(Gate::Done).await?;
                let bytes = fetch.buf.copy_prefix(usize::MAX);
                drop(fetch);
                eprintln!(
                    "sonora native: {adam_id} bearer={bearer_ms}ms webplayback={}ms plain download={}ms",
                    resolve_ms - bearer_ms,
                    t_dl.elapsed().as_millis(),
                );
                Ok(Self {
                    fetch: None,
                    buf: AssetBuf::complete(bytes),
                    adam_id: adam_id.to_string(),
                    kind: CtxKind::Plain,
                    done: 0,
                    from_cache: false,
                    layout_final: true,
                })
            }
            WebPlayback::Encrypted(info) => {
                let key_id = webplayback::decode_kid(&info.kid_base64)?;
                // The head download and the CDM open + challenge are
                // independent: run them together, not back to back. Timed
                // separately so slow starts attribute to network vs CDM.
                // The tail keeps downloading after `begin` returns.
                let fetch = spawn_fetch(&info.file_url, media_user_token, FetchMode::UntilFragment);
                let download = async {
                    let t = std::time::Instant::now();
                    let r = fetch.buf.wait(Gate::Head).await;
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
                let (head, download_ms) = dl_out;
                head?;
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
                fetch.buf.relabel_ready();
                let (prefix, complete) = fetch.buf.snapshot();
                let layout = cenc::layout_prefix(&prefix, complete, None)?
                    .ok_or_else(|| PlaybackError::Decode("no playable fragment in asset".into()))?;
                let (ready, total) = fetch.buf.progress();
                eprintln!(
                    "sonora native: {adam_id} bearer={bearer_ms}ms webplayback={}ms download={download_ms}ms cdm={cdm_ms}ms (wall {parallel_ms}ms) license={license_ms}ms layout={:?}/iv{} samples={} ready={ready}/{total}",
                    resolve_ms - bearer_ms,
                    layout.mode,
                    layout.iv_size,
                    layout.sample_count(),
                );
                Ok(Self {
                    buf: Arc::clone(&fetch.buf),
                    fetch: Some(fetch),
                    adam_id: adam_id.to_string(),
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
                    layout_final: complete,
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
            CtxKind::Plain => self.buf.progress().0 as usize,
            CtxKind::Encrypted { layout, .. } => layout.prefix_end(n),
        }
    }

    pub fn plaintext_prefix(&self, end: usize) -> Vec<u8> {
        self.buf.copy_prefix(end)
    }

    pub fn adam_id(&self) -> &str {
        &self.adam_id
    }

    pub fn from_cache(&self) -> bool {
        self.from_cache
    }

    /// Download finished and every currently-known sample has been decrypted.
    /// The layout must already be the final one (`extend_layout` / `poll_prepare`).
    pub fn is_drained(&self) -> bool {
        self.layout_final && self.buf.is_complete() && self.done >= self.total_samples()
    }

    /// Owned waiter so the pipeline can arm `Notify` and then mutably poll.
    /// The waiter does not borrow this context.
    pub fn watcher(&self) -> ProgressWait {
        ProgressWait {
            buf: Arc::clone(&self.buf),
        }
    }

    /// Block until the asset is fully downloaded. Prefetch uses this so it
    /// still decrypts a complete file; playback does not.
    pub async fn wait_complete(&self) -> Result<()> {
        self.buf.wait(Gate::Done).await
    }

    /// Extend the sample table over whatever new complete fragments are in
    /// the contiguous prefix. Keeps the IV/`senc` mode chosen in `begin`.
    pub fn extend_layout(&mut self) -> Result<()> {
        if self.layout_final {
            return Ok(());
        }
        if matches!(self.kind, CtxKind::Plain) {
            if self.buf.is_complete() {
                self.layout_final = true;
            }
            return Ok(());
        }
        let (prefix, complete) = self.buf.snapshot();
        let lock = match &self.kind {
            CtxKind::Encrypted { layout, .. } => Some(cenc::LayoutLock::from(layout)),
            CtxKind::Plain => None,
        };
        match cenc::layout_prefix(&prefix, complete, lock.as_ref())? {
            None => Ok(()),
            Some(layout) => self.install_layout(layout, complete),
        }
    }

    fn install_layout(&mut self, layout: cenc::SelectedLayout, complete: bool) -> Result<()> {
        if let CtxKind::Encrypted { layout: old, .. } = &self.kind {
            if layout.sample_count() < old.sample_count() {
                return Err(PlaybackError::Decode("layout shrank".into()));
            }
            for (prev, next) in old.samples.iter().zip(layout.samples.iter()) {
                if prev.start != next.start || prev.len != next.len || prev.iv != next.iv {
                    return Err(PlaybackError::Decode("layout sample moved".into()));
                }
            }
        }
        if let CtxKind::Encrypted { layout: slot, .. } = &mut self.kind {
            *slot = layout;
        }
        if complete {
            self.layout_final = true;
        }
        Ok(())
    }

    /// `Ready` once the layout covers `extra` more samples (or the file has
    /// finished and anything remains). `Pending` when the download has to
    /// catch up. Registers no waiter — pair with [`Self::notified`].
    pub fn poll_prepare(&mut self, extra: usize) -> Result<Prepare> {
        self.buf.pull_error()?;
        self.extend_layout()?;
        if self.is_drained() {
            return Ok(Prepare::Drained);
        }
        let have = self.total_samples();
        let want = self.done.saturating_add(extra);
        if have >= want || (self.layout_final && have > self.done) {
            return Ok(Prepare::Ready);
        }
        Ok(Prepare::Pending)
    }

    /// Decrypt up to `n` more samples, rounded up to whole fragments (see
    /// `aligned_end`). Sync and blocking per CDM call — callers run this on
    /// a blocking thread. Ciphertext is copied out so the CDM call does not
    /// hold the download lock. No-op once this prefix is decrypted (disk
    /// cache and plain paths just advance the cursor).
    pub fn decrypt_next(&mut self, n: usize) -> Result<()> {
        self.buf.pull_error()?;
        let total = self.total_samples();
        if self.done >= total {
            return Ok(());
        }
        let end = match &self.kind {
            CtxKind::Plain => total,
            CtxKind::Encrypted { layout, .. } => layout.aligned_end(self.done + n),
        }
        .min(total);
        let live = matches!(self.kind, CtxKind::Encrypted { live: Some(_), .. });
        if !live {
            self.done = end;
            return Ok(());
        }
        let samples = match &self.kind {
            CtxKind::Encrypted { layout, .. } => layout.samples[self.done..end].to_vec(),
            CtxKind::Plain => Vec::new(),
        };
        let mut ciphers = Vec::with_capacity(samples.len());
        for sample in &samples {
            ciphers.push(self.buf.copy_range(sample.start, sample.len)?);
        }
        let mut plains = Vec::with_capacity(samples.len());
        if let CtxKind::Encrypted {
            live: Some(state), ..
        } = &self.kind
        {
            for (sample, cipher) in samples.iter().zip(ciphers) {
                let plain = state
                    .cdm
                    .decrypt(&cipher, &state.key_id, &sample.iv, &sample.subs)
                    .map_err(PlaybackError::Decrypt)?;
                if plain.len() != sample.len {
                    return Err(PlaybackError::Decode(
                        "decryptor returned wrong length".into(),
                    ));
                }
                plains.push(plain);
            }
        }
        for (sample, plain) in samples.iter().zip(plains) {
            self.buf.write_range(sample.start, &plain)?;
        }
        self.done = end;
        Ok(())
    }

    /// Full plaintext (only complete once drained). Callers persist it
    /// unless `from_cache`.
    pub fn finish(self) -> Vec<u8> {
        self.buf.copy_prefix(usize::MAX)
    }
}

impl Drop for ProgressiveCtx {
    fn drop(&mut self) {
        // Abort before `buf` is dropped. Taking the handle also marks the
        // field read; the task's own `Arc` keeps the buffer alive until the
        // aborted future unwinds.
        drop(self.fetch.take());
    }
}

/// Resolve + download + decrypt one catalog track. Returns playable m4a bytes.
///
/// Waits for the whole asset before decrypting. Prefetch wants a finished
/// file in the disk cache, not a head start.
pub async fn resolve_and_decrypt(adam_id: &str, media_user_token: &str) -> Result<Vec<u8>> {
    let mut ctx = ProgressiveCtx::begin(adam_id, media_user_token).await?;
    ctx.wait_complete().await?;
    ctx.extend_layout()?;
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

    #[test]
    fn ready_watermark_ignores_a_hole() {
        let mut spans = Vec::new();
        insert_span(&mut spans, 100, 200);
        assert_eq!(contiguous_ready(&spans), 0);
        insert_span(&mut spans, 0, 50);
        assert_eq!(contiguous_ready(&spans), 50);
        // Overlap with the later range does not jump the hole at 50..100.
        insert_span(&mut spans, 150, 180);
        assert_eq!(contiguous_ready(&spans), 50);
        insert_span(&mut spans, 50, 100);
        assert_eq!(contiguous_ready(&spans), 200);
    }

    #[test]
    fn split_ranges_cover_the_tail_without_gaps() {
        let parts = split_inclusive(100, 1_100, 4);
        assert_eq!(parts.len(), 4);
        assert_eq!(parts[0].0, 100);
        assert_eq!(parts[3].1, 1_099);
        for window in parts.windows(2) {
            assert_eq!(window[0].1 + 1, window[1].0);
        }
    }
}
