//! `webPlayback` resolve + license envelope.
//!
//! Flow:
//!
//! * `POST play.music.apple.com/.../webPlayback` with the scraped bearer
//!   + MUT → `songList[0].assets` → pick the `28:ctrp256` fMP4 asset
//! * fetch its M3U8 → `KEY URI="uriPrefix,kidBase64"` + MAP segment
//! * license the KID with the CDM → decrypt the fMP4 (see `cenc`).

use crate::error::{PlaybackError, Result};
use crate::ids::{web_playback_body, CTR_FLAVOR};
use base64::Engine as _;

pub const LICENSE_URL: &str =
    "https://play.itunes.apple.com/WebObjects/MZPlay.woa/wa/acquireWebPlaybackLicense";
const WEBPLAYBACK_URL: &str = "https://play.music.apple.com/WebObjects/MZPlay.woa/wa/webPlayback";

#[derive(Debug, Clone)]
pub struct WebPlaybackInfo {
    pub file_url: String,
    pub kid_base64: String,
    pub uri_prefix: String,
}

#[derive(Debug)]
pub enum WebPlayback {
    Encrypted(WebPlaybackInfo),
    Plain { file_url: String },
}

#[derive(Debug)]
pub enum Asset {
    Flavored(String),
    Uploaded(String),
}

/// Pick the playable asset out of a `songList` entry.
pub fn select_asset(song: &serde_json::Value) -> Result<Asset> {
    if song["hls-playlist-url"]
        .as_str()
        .is_some_and(|u| !u.is_empty())
    {
        return Err(PlaybackError::Unsupported(
            "track is served as an HLS playlist, which native playback can't play yet".into(),
        ));
    }
    let assets = song["assets"]
        .as_array()
        .ok_or_else(|| PlaybackError::Resolve("no assets in songList entry".into()))?;
    if let [only] = assets.as_slice() {
        if only["flavor"].as_str().is_none() {
            if let Some(url) = only["URL"].as_str().filter(|u| !u.is_empty()) {
                return Ok(Asset::Uploaded(url.to_string()));
            }
        }
    }
    assets
        .iter()
        .find(|a| a["flavor"].as_str() == Some(CTR_FLAVOR))
        .and_then(|a| a["URL"].as_str())
        .map(|u| Asset::Flavored(u.to_string()))
        .ok_or_else(|| PlaybackError::Resolve(format!("no {CTR_FLAVOR} asset found")))
}

/// Pull `(uri_prefix, kid_base64, file_url)` out of an M3U8 media playlist.
/// `Ok(None)` = not an encrypted media playlist (plain file / unreadable).
pub fn parse_encrypted_playlist(asset_url: &str, body: &str) -> Result<Option<WebPlaybackInfo>> {
    let Ok((_, playlist)) = m3u8_rs::parse_media_playlist(body.as_bytes()) else {
        return Ok(None);
    };
    let key_uri = playlist
        .segments
        .first()
        .and_then(|s| s.key.as_ref())
        .and_then(|k| k.uri.as_deref());
    let Some((uri_prefix, kid_base64)) = key_uri.and_then(|u| u.split_once(',')) else {
        return Ok(None);
    };
    let base = asset_url
        .rsplit_once('/')
        .map(|(b, _)| b)
        .unwrap_or(asset_url);
    let map_uri = playlist
        .segments
        .first()
        .and_then(|s| s.map.as_ref())
        .map(|m| m.uri.as_str())
        .unwrap_or("");
    let file_url = if map_uri.starts_with("http") {
        map_uri.to_string()
    } else {
        format!("{base}/{map_uri}")
    };
    Ok(Some(WebPlaybackInfo {
        file_url,
        kid_base64: kid_base64.to_string(),
        uri_prefix: uri_prefix.to_string(),
    }))
}

/// License request envelope for `acquireWebPlaybackLicense`.
pub fn license_envelope(
    challenge_b64: &str,
    uri_prefix: &str,
    kid_base64: &str,
    adam_id: &str,
    is_library: bool,
) -> serde_json::Value {
    serde_json::json!({
        "challenge": challenge_b64,
        "key-system": "com.widevine.alpha",
        "uri": format!("{uri_prefix},{kid_base64}"),
        "adamId": adam_id,
        "isLibrary": is_library,
        "user-initiated": true,
    })
}

pub(crate) fn decode_kid(kid_base64: &str) -> Result<Vec<u8>> {
    base64::engine::general_purpose::STANDARD
        .decode(kid_base64)
        .map_err(|e| PlaybackError::Resolve(format!("decode KID: {e}")))
}

pub async fn get_web_playback(
    adam_id: &str,
    bearer_token: &str,
    media_user_token: &str,
) -> Result<WebPlayback> {
    let client = crate::error::http_client()?;
    let body = web_playback_body(adam_id);
    let resp = client
        .post(WEBPLAYBACK_URL)
        .header("Content-Type", "application/json")
        .header("Origin", "https://music.apple.com")
        .header("Referer", "https://music.apple.com/")
        .header("Authorization", format!("Bearer {bearer_token}"))
        .header("x-apple-music-user-token", media_user_token)
        .header("Cookie", format!("media-user-token={media_user_token}"))
        .json(&body)
        .send()
        .await
        .map_err(|e| PlaybackError::Network(format!("webPlayback request: {e}")))?;
    let status = resp.status();
    if !status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        let head: String = text.chars().take(300).collect();
        return Err(PlaybackError::Resolve(format!(
            "webPlayback HTTP {status}: {head}"
        )));
    }
    let json: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| PlaybackError::Resolve(format!("parse webPlayback: {e}")))?;
    // Diagnose unexpected shapes (library dispatch, new API variants):
    // keys + truncated body, safe to paste into a bug report.
    let describe = |tag: &str| {
        let keys = json
            .as_object()
            .map(|o| o.keys().take(12).cloned().collect::<Vec<_>>().join(","))
            .unwrap_or_default();
        let head: String = json.to_string().chars().take(500).collect();
        eprintln!("sonora native: webPlayback {tag} for {adam_id} (keys: {keys}): {head}");
        format!("{tag} (keys: {keys}): {head}")
    };
    let Some(list) = json["songList"].as_array() else {
        return Err(PlaybackError::Resolve(format!(
            "no songList in response: {}",
            describe("missing-songList")
        )));
    };
    if list.is_empty() {
        return Err(PlaybackError::Resolve(format!(
            "empty songList: {}",
            describe("empty-songList")
        )));
    }
    match select_asset(&list[0])? {
        Asset::Flavored(url) => {
            let m3u8 = client
                .get(&url)
                .send()
                .await
                .map_err(|e| PlaybackError::Network(format!("fetch M3U8: {e}")))?
                .text()
                .await
                .map_err(|e| PlaybackError::Network(format!("read M3U8: {e}")))?;
            match parse_encrypted_playlist(&url, &m3u8)? {
                Some(info) => Ok(WebPlayback::Encrypted(info)),
                None => Err(PlaybackError::Resolve(
                    "catalog asset carried no Widevine KEY".into(),
                )),
            }
        }
        Asset::Uploaded(url) => {
            // Uploaded (iCloud) tracks may be plain files: probe the URL —
            // if it parses as an encrypted playlist, treat as encrypted.
            let probe = client.get(&url).send().await;
            match probe {
                Ok(r) => {
                    let text = r.text().await.unwrap_or_default();
                    match parse_encrypted_playlist(&url, &text)? {
                        Some(info) => Ok(WebPlayback::Encrypted(info)),
                        None => Ok(WebPlayback::Plain { file_url: url }),
                    }
                }
                Err(_) => Ok(WebPlayback::Plain { file_url: url }),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_picks_ctr() {
        let song = serde_json::json!({
            "assets": [
                { "flavor": "32:cbcp64", "URL": "https://x/cbcp.m3u8" },
                { "flavor": CTR_FLAVOR, "URL": "https://x/ctr.m3u8" },
            ]
        });
        match select_asset(&song).unwrap() {
            Asset::Flavored(u) => assert_eq!(u, "https://x/ctr.m3u8"),
            Asset::Uploaded(_) => panic!("not an upload"),
        }
    }

    #[test]
    fn lone_flavourless_is_upload() {
        let song = serde_json::json!({ "assets": [{ "URL": "https://x/up.m4a" }] });
        match select_asset(&song).unwrap() {
            Asset::Uploaded(u) => assert_eq!(u, "https://x/up.m4a"),
            Asset::Flavored(_) => panic!("not an encode"),
        }
    }

    #[test]
    fn hls_is_unsupported() {
        let song = serde_json::json!({ "hls-playlist-url": "https://x/p.m3u8", "assets": [] });
        assert!(matches!(
            select_asset(&song),
            Err(PlaybackError::Unsupported(_))
        ));
    }

    #[test]
    fn m3u8_key_parsed() {
        let asset = "https://example.com/a/stream.m3u8";
        let body = "#EXTM3U\n#EXT-X-TARGETDURATION:10\n#EXT-X-KEY:METHOD=SAMPLE-AES-CTR,URI=\"skd://itunes.apple.com/P000/e1,q83vAA==\",IV=0x0001\n#EXT-X-MAP:URI=\"init.mp4\"\n#EXTINF:10,\nseg1.m4s\n";
        let info = parse_encrypted_playlist(asset, body)
            .unwrap()
            .expect("encrypted");
        assert_eq!(info.uri_prefix, "skd://itunes.apple.com/P000/e1");
        assert_eq!(info.kid_base64, "q83vAA==");
        assert_eq!(info.file_url, "https://example.com/a/init.mp4");
    }

    #[test]
    fn plain_playlist_is_none() {
        let body = "#EXTM3U\n#EXTINF:10,\nseg1.ts\n";
        assert!(parse_encrypted_playlist("https://x/p.m3u8", body)
            .unwrap()
            .is_none());
    }

    #[test]
    fn envelope_shape() {
        let e = license_envelope("Q0g=", "skd://x", "q83vAA==", "1811922756", false);
        assert_eq!(e["key-system"], "com.widevine.alpha");
        assert_eq!(e["adamId"], "1811922756");
        assert_eq!(e["isLibrary"], false);
        assert_eq!(e["uri"], "skd://x,q83vAA==");
    }
}
