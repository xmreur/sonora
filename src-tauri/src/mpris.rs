//! MPRIS bridge (`org.mpris.MediaPlayer2.sonora`) + opt-in track notifications.
//!
//! Fed by the existing `PlayerReport` stream (`POST /state` →
//! `SidecarManager::status()`): publishes metadata/playback status/volume,
//! accepts transport/seek/volume from any MPRIS controller, and fires a
//! desktop notification on track change only when the user opts in
//! (`Settings → Notifications`). A missing session bus degrades to
//! today's behavior — [`run`] logs to stderr and returns.

use std::time::Duration;

use apple_music_core::playback::PlaybackCommand;
use mpris_server::{
    zbus::{fdo, Result},
    LoopStatus, Metadata, PlaybackStatus, PlayerInterface, Property, RootInterface, Server, Signal,
    Time, TrackId, Volume,
};

use crate::sidecar::{PlayerReport, SidecarManager};

/// D-Bus-safe path segment: keep alphanumerics/`_`, map everything else
/// (D-Bus object paths allow only `[A-Za-z0-9_/]`, so `-` becomes `_` too),
/// cap at 64 chars, never empty.
fn sanitize_id(id: &str) -> String {
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

/// `/org/mpris/MediaPlayer2/Track/<sanitized-id>`; `"0"` segment when none.
pub fn trackid_for(id: Option<&str>) -> TrackId {
    let seg = match id {
        Some(raw) if !raw.is_empty() => sanitize_id(raw),
        _ => "0".to_string(),
    };
    TrackId::try_from(format!("/org/mpris/MediaPlayer2/Track/{seg}")).unwrap_or(TrackId::NO_TRACK)
}

/// No `track_id` → `Stopped`; `playing` → `Playing`; else `Paused`.
pub fn playback_status_for(rep: &PlayerReport) -> PlaybackStatus {
    if rep.track_id.as_deref().is_none_or(|s| s.is_empty()) {
        PlaybackStatus::Stopped
    } else if rep.playing {
        PlaybackStatus::Playing
    } else {
        PlaybackStatus::Paused
    }
}

fn millis_to_time(ms: u64) -> Time {
    Time::from_millis(ms.min(i64::MAX as u64 / 1000) as i64)
}

/// Trackid always set; title/artist/album/length/art attached when known.
pub fn report_to_metadata(rep: &PlayerReport) -> Metadata {
    let mut b = Metadata::builder().trackid(trackid_for(rep.track_id.as_deref()));
    if let Some(t) = rep.title.as_deref().filter(|t| !t.is_empty()) {
        b = b.title(t);
    }
    if let Some(a) = rep.artist.as_deref().filter(|a| !a.is_empty()) {
        b = b.artist([a]);
    }
    if let Some(al) = rep.album.as_deref().filter(|al| !al.is_empty()) {
        b = b.album(al);
    }
    if rep.duration_ms > 0 {
        b = b.length(millis_to_time(rep.duration_ms));
    }
    if let Some(u) = rep.art_url.as_deref().filter(|u| !u.is_empty()) {
        b = b.art_url(u);
    }
    b.build()
}

/// Apply a signed microsecond seek offset to a millisecond position.
/// Saturates on overflow; clamps at zero.
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

#[derive(Clone)]
pub struct MprisBridge {
    sidecar: SidecarManager,
}

impl MprisBridge {
    pub fn new(sidecar: SidecarManager) -> Self {
        Self { sidecar }
    }

    fn report(&self) -> fdo::Result<PlayerReport> {
        self.sidecar.status().map_err(fdo::Error::Failed)
    }
}

impl RootInterface for MprisBridge {
    async fn raise(&self) -> fdo::Result<()> {
        Ok(())
    }

    async fn quit(&self) -> fdo::Result<()> {
        Ok(())
    }

    async fn can_quit(&self) -> fdo::Result<bool> {
        Ok(false)
    }

    async fn fullscreen(&self) -> fdo::Result<bool> {
        Ok(false)
    }

    async fn set_fullscreen(&self, _fullscreen: bool) -> Result<()> {
        Ok(())
    }

    async fn can_set_fullscreen(&self) -> fdo::Result<bool> {
        Ok(false)
    }

    async fn can_raise(&self) -> fdo::Result<bool> {
        Ok(true)
    }

    async fn has_track_list(&self) -> fdo::Result<bool> {
        Ok(false)
    }

    async fn identity(&self) -> fdo::Result<String> {
        Ok("Sonora".to_string())
    }

    async fn desktop_entry(&self) -> fdo::Result<String> {
        Ok("com.xmreur.sonora".to_string())
    }

    async fn supported_uri_schemes(&self) -> fdo::Result<Vec<String>> {
        Ok(vec![])
    }

    async fn supported_mime_types(&self) -> fdo::Result<Vec<String>> {
        Ok(vec![])
    }
}

impl PlayerInterface for MprisBridge {
    /// Never drive MusicKit directly: bump a counter the UI consumes on
    /// its next `sidecar_status` poll, so OS skips behave exactly like the
    /// in-app buttons and UI state never diverges.
    async fn next(&self) -> fdo::Result<()> {
        self.sidecar.request_os_next().map_err(fdo::Error::Failed)
    }

    async fn previous(&self) -> fdo::Result<()> {
        self.sidecar.request_os_prev().map_err(fdo::Error::Failed)
    }
    async fn pause(&self) -> fdo::Result<()> {
        self.sidecar
            .enqueue(PlaybackCommand::Pause)
            .map_err(fdo::Error::Failed)
    }

    async fn play_pause(&self) -> fdo::Result<()> {
        let playing = self.report().map(|r| r.playing).unwrap_or(false);
        self.sidecar
            .enqueue(if playing {
                PlaybackCommand::Pause
            } else {
                PlaybackCommand::Play
            })
            .map_err(fdo::Error::Failed)
    }

    /// Deliberately `Pause`, not `Clear`: Stop must not destroy the queue.
    async fn stop(&self) -> fdo::Result<()> {
        self.sidecar
            .enqueue(PlaybackCommand::Pause)
            .map_err(fdo::Error::Failed)
    }

    async fn play(&self) -> fdo::Result<()> {
        self.sidecar
            .enqueue(PlaybackCommand::Play)
            .map_err(fdo::Error::Failed)
    }

    async fn seek(&self, offset: Time) -> fdo::Result<()> {
        let pos = self.report()?.position_ms;
        self.sidecar
            .enqueue(PlaybackCommand::Seek {
                position_ms: seek_target(pos, offset.as_micros()),
            })
            .map_err(fdo::Error::Failed)
    }

    async fn set_position(&self, _track_id: TrackId, position: Time) -> fdo::Result<()> {
        self.sidecar
            .enqueue(PlaybackCommand::Seek {
                position_ms: position.as_millis().max(0) as u64,
            })
            .map_err(fdo::Error::Failed)
    }

    async fn open_uri(&self, _uri: String) -> fdo::Result<()> {
        Err(fdo::Error::NotSupported("open_uri".to_string()))
    }

    async fn playback_status(&self) -> fdo::Result<PlaybackStatus> {
        Ok(playback_status_for(&self.report()?))
    }

    async fn loop_status(&self) -> fdo::Result<LoopStatus> {
        Ok(LoopStatus::None)
    }

    async fn set_loop_status(&self, _loop_status: LoopStatus) -> Result<()> {
        Ok(())
    }

    async fn rate(&self) -> fdo::Result<f64> {
        Ok(1.0)
    }

    async fn set_rate(&self, _rate: f64) -> Result<()> {
        Ok(())
    }

    async fn shuffle(&self) -> fdo::Result<bool> {
        Ok(false)
    }

    async fn set_shuffle(&self, _shuffle: bool) -> Result<()> {
        Ok(())
    }

    async fn metadata(&self) -> fdo::Result<Metadata> {
        Ok(report_to_metadata(&self.report()?))
    }

    async fn volume(&self) -> fdo::Result<Volume> {
        Ok(self.sidecar.volume() as f64)
    }

    async fn set_volume(&self, volume: Volume) -> Result<()> {
        // `enqueue` stores the level (Step-1 path); no second write here.
        self.sidecar
            .enqueue(PlaybackCommand::SetVolume {
                level: (volume as f32).clamp(0.0, 1.0),
            })
            .map_err(fdo::Error::Failed)?;
        Ok(())
    }

    async fn position(&self) -> fdo::Result<Time> {
        Ok(millis_to_time(self.report()?.position_ms))
    }

    async fn minimum_rate(&self) -> fdo::Result<f64> {
        Ok(1.0)
    }

    async fn maximum_rate(&self) -> fdo::Result<f64> {
        Ok(1.0)
    }

    async fn can_go_next(&self) -> fdo::Result<bool> {
        Ok(true)
    }

    async fn can_go_previous(&self) -> fdo::Result<bool> {
        Ok(true)
    }

    async fn can_play(&self) -> fdo::Result<bool> {
        Ok(true)
    }

    async fn can_pause(&self) -> fdo::Result<bool> {
        Ok(true)
    }

    async fn can_seek(&self) -> fdo::Result<bool> {
        Ok(true)
    }

    async fn can_control(&self) -> fdo::Result<bool> {
        Ok(true)
    }
}
/// Serve `org.mpris.MediaPlayer2.sonora`, polling the sidecar every 500ms:
/// changed Metadata/PlaybackStatus/Volume properties, `Seeked` on same-track
/// position discontinuities > 2000ms, and — only when notifications are
/// enabled — a notification on track change.
/// No session bus (headless/CI) → log to stderr and return; app unaffected.
pub async fn run(sidecar: SidecarManager) {
    let server = match Server::new("sonora", MprisBridge::new(sidecar.clone())).await {
        Ok(s) => s,
        Err(e) => {
            eprintln!("mpris: session bus unavailable ({e}); media keys disabled");
            return;
        }
    };
    type Sig = (
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
        u64,
    );
    let mut last_sig: Option<Sig> = None;
    let mut last_status = PlaybackStatus::Stopped;
    let mut last_volume = sidecar.volume();
    let mut last_pos: u64 = 0;
    let mut last_notified: Option<String> = None;
    loop {
        tokio::time::sleep(Duration::from_millis(500)).await;
        let rep = match sidecar.status() {
            Ok(r) => r,
            Err(_) => continue,
        };
        let vol = sidecar.volume();
        let sig: Sig = (
            rep.track_id.clone(),
            rep.title.clone(),
            rep.artist.clone(),
            rep.album.clone(),
            rep.art_url.clone(),
            rep.duration_ms,
        );
        let status = playback_status_for(&rep);
        let mut changed = Vec::new();
        if last_sig.as_ref() != Some(&sig) {
            changed.push(Property::Metadata(report_to_metadata(&rep)));
        }
        if last_sig.is_none() || status != last_status {
            changed.push(Property::PlaybackStatus(status));
        }
        if last_sig.is_none() || (vol - last_volume).abs() > f32::EPSILON {
            changed.push(Property::Volume(vol as f64));
        }
        if !changed.is_empty() {
            let _ = server.properties_changed(changed).await;
        }
        if let (Some(cur), Some(prev)) = (
            rep.track_id.as_deref(),
            last_sig.as_ref().and_then(|s| s.0.as_deref()),
        ) {
            if cur == prev && rep.position_ms.abs_diff(last_pos) > 2000 {
                let _ = server
                    .emit(Signal::Seeked {
                        position: millis_to_time(rep.position_ms),
                    })
                    .await;
            }
        }
        if sidecar.notifications_enabled() && should_notify(last_notified.as_deref(), &rep) {
            send_notification(&rep).await;
            last_notified = rep.track_id.clone();
        }
        if rep.track_id.is_none() {
            last_notified = None;
        }
        last_sig = Some(sig);
        last_status = status;
        last_volume = vol;
        last_pos = rep.position_ms;
    }
}

static ART_HTTP: std::sync::LazyLock<reqwest::Client> = std::sync::LazyLock::new(|| {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(3))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new())
});

/// Fetch `art_url` into `<config>/mpris-art/<sanitized-track-id>` (cached —
/// skip download when present). Any failure → `None` (text-only notification).
async fn cached_artwork(track_id: Option<&str>, url: &str) -> Option<String> {
    let path = crate::app_config_dir()?
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

async fn send_notification(rep: &PlayerReport) {
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

#[cfg(test)]
mod tests {
    use super::*;

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
    fn trackid_format_always_valid() {
        assert_eq!(
            trackid_for(Some("123")).to_string(),
            "/org/mpris/MediaPlayer2/Track/123"
        );
        assert_eq!(
            trackid_for(None).to_string(),
            "/org/mpris/MediaPlayer2/Track/0"
        );
        assert_eq!(
            trackid_for(Some("")).to_string(),
            "/org/mpris/MediaPlayer2/Track/0"
        );
        // D-Bus-hostile chars map to `_`, never rejected.
        assert_eq!(
            trackid_for(Some("a/b c-d")).to_string(),
            "/org/mpris/MediaPlayer2/Track/a_b_c_d"
        );
    }

    #[test]
    fn metadata_mapping() {
        let m = report_to_metadata(&rep());
        assert_eq!(
            m.trackid().map(|t| t.to_string()),
            Some("/org/mpris/MediaPlayer2/Track/123".to_string())
        );
        assert_eq!(m.title(), Some("Song"));
        assert_eq!(m.artist(), Some(vec!["Artist".to_string()]));
        assert_eq!(m.album(), Some("Album"));
        assert_eq!(m.length().map(|t| t.as_millis()), Some(180_000));
        assert_eq!(
            m.art_url().map(|u| u.to_string()),
            Some("https://example.com/a.jpg".to_string())
        );
    }

    #[test]
    fn metadata_absent_art_and_track() {
        let mut r = rep();
        r.art_url = None;
        r.duration_ms = 0;
        let m = report_to_metadata(&r);
        assert!(m.art_url().is_none());
        assert!(m.length().is_none());

        let mut r = rep();
        r.track_id = None;
        let m = report_to_metadata(&r);
        // Trackid always present, even with no track.
        assert_eq!(
            m.trackid().map(|t| t.to_string()),
            Some("/org/mpris/MediaPlayer2/Track/0".to_string())
        );
    }

    #[test]
    fn status_mapping() {
        let mut r = rep();
        r.track_id = None;
        assert_eq!(playback_status_for(&r), PlaybackStatus::Stopped);
        r.track_id = Some("1".into());
        r.playing = true;
        assert_eq!(playback_status_for(&r), PlaybackStatus::Playing);
        r.playing = false;
        assert_eq!(playback_status_for(&r), PlaybackStatus::Paused);
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
        // Same id or empty title stays silent.
        let mut no_title = rep();
        no_title.title = None;
        assert!(!should_notify(None, &no_title));
        let mut cleared = rep();
        cleared.track_id = None;
        cleared.title = None;
        assert!(!should_notify(Some("123"), &cleared));
    }

    #[test]
    fn os_skip_counters_surface_in_status() {
        let m = SidecarManager::new();
        let s = m.status().unwrap();
        assert_eq!((s.os_next, s.os_prev), (0, 0));
        m.request_os_next().unwrap();
        m.request_os_next().unwrap();
        m.request_os_prev().unwrap();
        let s = m.status().unwrap();
        assert_eq!((s.os_next, s.os_prev), (2, 1));
    }

    #[test]
    fn notifications_default_off() {
        let m = SidecarManager::new();
        assert!(!m.notifications_enabled());
        m.set_notifications(true).unwrap();
        assert!(m.notifications_enabled());
    }

    #[test]
    fn volume_round_trip_through_enqueue() {
        let m = SidecarManager::new();
        assert_eq!(m.volume(), 1.0);
        m.enqueue(PlaybackCommand::SetVolume { level: 0.42 })
            .unwrap();
        assert!((m.volume() - 0.42).abs() < f32::EPSILON);
    }
}
