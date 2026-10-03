//! Shared media-session helpers (notifications, seek math, track-change gate).

use std::time::Duration;

use crate::sidecar::PlayerReport;
#[cfg(any(windows, target_os = "macos"))]
use crate::sidecar::SidecarManager;

/// D-Bus-safe / filesystem-safe id segment.
pub fn sanitize_id(id: &str) -> String {
    let mut s: String = id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' {
                c
            } else {
                '_'
            }
        })
        .take(64)
        .collect();
    if s.is_empty() {
        s.push('0');
    }
    s
}

/// Apply a signed microsecond seek offset to a millisecond position.
pub fn seek_target(position_ms: u64, offset_micros: i64) -> u64 {
    let target = position_ms as i128 * 1000 + offset_micros as i128;
    (target.max(0) / 1000).min(u64::MAX as i128) as u64
}

/// Fire only when the track actually changed to something with a title.
pub fn should_notify(last: Option<&str>, rep: &PlayerReport) -> bool {
    rep.track_id.as_deref().is_some_and(|s| !s.is_empty())
        && rep.track_id.as_deref() != last
        && rep.title.as_deref().is_some_and(|t| !t.is_empty())
}

static ART_HTTP: std::sync::LazyLock<reqwest::Client> = std::sync::LazyLock::new(|| {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(3))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new())
});

async fn cached_artwork(track_id: Option<&str>, url: &str) -> Option<String> {
    let path = crate::paths::app_config_dir()?
        .join("mpris-art")
        .join(sanitize_id(track_id.unwrap_or("unknown")));
    if path.exists() {
        return Some(path.to_string_lossy().into_owned());
    }
    let bytes = ART_HTTP.get(url).send().await.ok()?.bytes().await.ok()?;
    std::fs::create_dir_all(path.parent()?).ok()?;
    std::fs::write(&path, &bytes).ok()?;
    Some(path.to_string_lossy().into_owned())
}

pub async fn send_notification(rep: &PlayerReport) {
    let Some(title) = rep.title.as_deref().filter(|t| !t.is_empty()) else {
        return;
    };
    let mut body = rep.artist.clone().unwrap_or_default();
    if let Some(album) = rep.album.as_deref().filter(|a| !a.is_empty()) {
        if !body.is_empty() {
            body.push_str(" — ");
        }
        body.push_str(album);
    }
    let mut n = notify_rust::Notification::new();
    n.summary(title).appname("Sonora");
    if !body.is_empty() {
        n.body(&body);
    }
    if let Some(url) = rep.art_url.as_deref().filter(|u| !u.is_empty()) {
        if let Some(path) = cached_artwork(rep.track_id.as_deref(), url).await {
            n.image_path(&path);
        }
    }
    let _ = n.show();
}

/// Poll sidecar state and fire opt-in track notifications (Win/macOS; Linux uses MPRIS loop).
#[cfg(any(windows, target_os = "macos"))]
pub async fn notification_poll_loop(sidecar: SidecarManager) {
    let mut last_notified: Option<String> = None;
    loop {
        tokio::time::sleep(Duration::from_millis(500)).await;
        if !sidecar.notifications_enabled() {
            continue;
        }
        let rep = match sidecar.status() {
            Ok(r) => r,
            Err(_) => continue,
        };
        if should_notify(last_notified.as_deref(), &rep) {
            send_notification(&rep).await;
            last_notified = rep.track_id.clone();
        }
        if rep.track_id.is_none() {
            last_notified = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sidecar::PlayerReport;

    fn rep() -> PlayerReport {
        PlayerReport {
            playing: true,
            track_id: Some("123".into()),
            title: Some("Song".into()),
            artist: Some("Artist".into()),
            album: Some("Album".into()),
            art_url: Some("https://example.com/a.jpg".into()),
            os_next: 0,
            os_prev: 0,
            position_ms: 1000,
            duration_ms: 180_000,
            detail: String::new(),
        }
    }

    #[test]
    fn seek_math() {
        assert_eq!(seek_target(10_000, 5_000_000), 15_000);
        assert_eq!(seek_target(1_000, -5_000_000), 0);
        assert_eq!(seek_target(0, -1), 0);
        assert_eq!(seek_target(u64::MAX, i64::MAX), u64::MAX);
    }

    #[test]
    fn notify_gate() {
        let r = rep();
        assert!(should_notify(None, &r));
        assert!(!should_notify(Some("123"), &r));
        let mut other = rep();
        other.track_id = Some("456".into());
        assert!(should_notify(Some("123"), &other));
        let mut no_title = rep();
        no_title.title = None;
        assert!(!should_notify(None, &no_title));
        let mut cleared = rep();
        cleared.track_id = None;
        cleared.title = None;
        assert!(!should_notify(Some("123"), &cleared));
    }
}
