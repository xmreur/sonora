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
    let bytes = reqwest::Client::new()
        .get(url)
        .header("User-Agent", "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36")
        .header("x-apple-music-user-token", media_user_token)
        .header("Cookie", format!("media-user-token={media_user_token}"))
        .send()
        .await
        .map_err(|e| PlaybackError::Network(format!("download asset: {e}")))?
        .error_for_status()
        .map_err(|e| PlaybackError::Network(format!("download asset: {e}")))?
        .bytes()
        .await
        .map_err(|e| PlaybackError::Network(format!("read asset: {e}")))?
        .to_vec();
    if bytes.is_empty() {
        return Err(PlaybackError::Network("asset download is empty".into()));
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
    let body = reqwest::Client::new()
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

/// Resolve + download + decrypt one catalog track. Returns playable m4a bytes.
pub async fn resolve_and_decrypt(adam_id: &str, media_user_token: &str) -> Result<Vec<u8>> {
    if adam_id.trim().is_empty() {
        return Err(PlaybackError::Resolve("empty track id".into()));
    }
    if is_library_id(adam_id) {
        return Err(PlaybackError::Resolve(format!(
            "library id {adam_id} needs catalog resolution first"
        )));
    }
    if let Some(cached) = crate::cache::load(adam_id) {
        return Ok(cached);
    }
    let t0 = std::time::Instant::now();
    let bearer = crate::bearer::get_bearer_token().await?;
    let bearer_ms = t0.elapsed().as_millis();
    let playback = webplayback::get_web_playback(adam_id, &bearer, media_user_token).await?;
    let resolve_ms = t0.elapsed().as_millis();
    let bytes = match playback {
        WebPlayback::Plain { file_url } => download_asset(&file_url, media_user_token).await?,
        WebPlayback::Encrypted(info) => {
            let key_id = webplayback::decode_kid(&info.kid_base64)?;
            // The asset download and the CDM open + challenge are
            // independent: run them together, not back to back.
            let download = download_asset(&info.file_url, media_user_token);
            let licence = async {
                let cdm = Cdm::open_system().await.map_err(PlaybackError::Cdm)?;
                let session = cdm.begin_license().await;
                let init = widevine::build_pssh(&key_id);
                let (challenge, cdm_session) = cdm
                    .challenge(&session, &init)
                    .map_err(PlaybackError::License)?;
                Ok::<_, PlaybackError>((cdm, session, challenge, cdm_session))
            };
            let t1 = std::time::Instant::now();
            let (cipher, lic) = tokio::join!(download, licence);
            let cipher = cipher?;
            let (cdm, session, challenge, cdm_session) = lic?;
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
            let t3 = std::time::Instant::now();
            let plain = cenc::decrypt_track(cipher, &key_id, |buf, kid, iv, subs| {
                cdm.decrypt(buf, kid, iv, subs)
                    .map_err(PlaybackError::Decrypt)
            })?;
            eprintln!(
                "sonora native: {adam_id} bearer={bearer_ms}ms webplayback={}ms download+cdm={parallel_ms}ms license={license_ms}ms decrypt={}ms",
                resolve_ms - bearer_ms,
                t3.elapsed().as_millis(),
            );
            plain
        }
    };
    crate::cache::store(adam_id, &bytes);
    Ok(bytes)
}
