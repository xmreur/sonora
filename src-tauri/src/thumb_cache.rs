//! Disk cache for static artwork thumbnails.
//!
//! Sized artwork variants are fetched once and served back as file paths
//! (the UI maps them to asset URLs). Repeat visits then skip network and
//! re-decode churn. Animated (HLS) covers stay session-cached in the UI —
//! segment-level offline caching would need a custom hls.js loader.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::PathBuf;

const MAX_SIZE: u32 = 1024;
const MIN_SIZE: u32 = 64;
const MAX_BYTES: usize = 8 << 20;

pub fn clamp_size(size: u32) -> u32 {
    size.clamp(MIN_SIZE, MAX_SIZE)
}

/// Stable filename for one artwork URL + size (hex hash, no URL debris).
pub fn file_name(url: &str, size: u32) -> String {
    let mut h = DefaultHasher::new();
    url.hash(&mut h);
    clamp_size(size).hash(&mut h);
    format!("{:016x}_{}.jpg", h.finish(), clamp_size(size))
}

pub fn cache_path(url: &str, size: u32) -> Option<PathBuf> {
    crate::paths::app_config_dir().map(|d| d.join("thumbcache").join(file_name(url, size)))
}

/// Fetch (or reuse) a sized artwork thumbnail. Returns the local file path;
/// the UI maps it through `convertFileSrc` (guarded, falls back to remote).
#[tauri::command]
pub async fn thumb_cache(url: String, size: u32) -> Result<String, String> {
    let url = url.trim().to_string();
    if !(url.starts_with("https://") || url.starts_with("http://")) {
        return Err("refusing non-http artwork url".into());
    }
    let path = cache_path(&url, size).ok_or("no config dir for thumbnail cache")?;
    if path.is_file() {
        return Ok(path.to_string_lossy().into_owned());
    }
    let bytes = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .user_agent("Sonora/1.0 (artwork cache)")
        .build()
        .map_err(|e| format!("http client: {e}"))?
        .get(&url)
        .send()
        .await
        .map_err(|e| format!("artwork fetch: {e}"))?
        .error_for_status()
        .map_err(|e| format!("artwork fetch: {e}"))?;
    let ctype = bytes
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    if !ctype.starts_with("image/") {
        return Err(format!("not artwork (content-type: {ctype})"));
    }
    let bytes = bytes
        .bytes()
        .await
        .map_err(|e| format!("artwork body: {e}"))?;
    if bytes.len() > MAX_BYTES || bytes.is_empty() {
        return Err(format!("artwork size out of range ({} bytes)", bytes.len()));
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let staging = path.with_extension("part");
    std::fs::write(&staging, &bytes).map_err(|e| e.to_string())?;
    std::fs::rename(&staging, &path).map_err(|e| {
        let _ = std::fs::remove_file(&staging);
        e.to_string()
    })?;
    Ok(path.to_string_lossy().into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_clamp() {
        assert_eq!(clamp_size(0), MIN_SIZE);
        assert_eq!(clamp_size(200), 200);
        assert_eq!(clamp_size(4096), MAX_SIZE);
    }

    #[test]
    fn filenames_stable_and_scoped() {
        let a = file_name("https://x/a.jpg", 200);
        assert_eq!(a, file_name("https://x/a.jpg", 200));
        assert_ne!(a, file_name("https://x/a.jpg", 400));
        assert_ne!(a, file_name("https://x/b.jpg", 200));
        assert!(a.ends_with("_200.jpg"));
    }

    #[test]
    fn cache_path_sits_under_thumbcache() {
        let p = cache_path("https://x/a.jpg", 200).expect("config dir");
        assert!(p.to_string_lossy().contains("thumbcache"));
    }

    #[tokio::test]
    async fn rejects_non_http() {
        assert!(thumb_cache("file:///etc/passwd".into(), 200).await.is_err());
        assert!(thumb_cache("".into(), 200).await.is_err());
    }
}
