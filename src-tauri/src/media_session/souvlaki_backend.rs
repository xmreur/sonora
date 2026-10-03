//! Windows SMTC and macOS Now Playing via souvlaki.

use std::sync::Arc;
use std::time::Duration;

use apple_music_core::playback::PlaybackCommand;
use souvlaki::{
    MediaControlEvent, MediaControls, MediaMetadata, MediaPlayback, MediaPosition, PlatformConfig,
};

use crate::sidecar::SidecarManager;

use super::MediaSessionConfig;

fn handle_event(sidecar: &SidecarManager, event: MediaControlEvent) {
    match event {
        MediaControlEvent::Play => {
            let _ = sidecar.enqueue(PlaybackCommand::Play);
        }
        MediaControlEvent::Pause => {
            let _ = sidecar.enqueue(PlaybackCommand::Pause);
        }
        MediaControlEvent::Toggle => {
            let playing = sidecar.status().map(|r| r.playing).unwrap_or(false);
            let _ = sidecar.enqueue(if playing {
                PlaybackCommand::Pause
            } else {
                PlaybackCommand::Play
            });
        }
        MediaControlEvent::Next => {
            let _ = sidecar.request_os_next();
        }
        MediaControlEvent::Previous => {
            let _ = sidecar.request_os_prev();
        }
        MediaControlEvent::Stop => {
            let _ = sidecar.enqueue(PlaybackCommand::Pause);
        }
        MediaControlEvent::Seek(pos) => {
            let _ = sidecar.enqueue(PlaybackCommand::Seek {
                position_ms: pos.as_millis() as u64,
            });
        }
        MediaControlEvent::SetVolume(vol) => {
            let _ = sidecar.enqueue(PlaybackCommand::SetVolume {
                level: vol.clamp(0.0, 1.0),
            });
        }
        MediaControlEvent::OpenUri(_) => {}
        MediaControlEvent::SetPosition(_) => {}
        MediaControlEvent::SetRate(_) => {}
        MediaControlEvent::SetShuffle(_) => {}
        MediaControlEvent::SetRepeat(_) => {}
        MediaControlEvent::Raise => {}
        MediaControlEvent::Quit => {}
    }
}

fn playback_for(rep: &crate::sidecar::PlayerReport) -> MediaPlayback {
    let progress = Some(MediaPosition(Duration::from_millis(rep.position_ms)));
    if rep.track_id.as_deref().is_none_or(|s| s.is_empty()) {
        MediaPlayback::Stopped
    } else if rep.playing {
        MediaPlayback::Playing { progress }
    } else {
        MediaPlayback::Paused { progress }
    }
}

pub async fn run(sidecar: SidecarManager, config: MediaSessionConfig) {
    let platform = PlatformConfig {
        dbus_name: "org.mpris.MediaPlayer2.sonora".into(),
        display_name: "Sonora".into(),
        hwnd: config.hwnd,
    };
    let controls = match MediaControls::new(platform) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("media_session: could not init OS controls ({e})");
            return;
        }
    };
    let sidecar = Arc::new(sidecar);
    let hook = sidecar.clone();
    if let Err(e) = controls.attach(move |event| handle_event(&hook, event)) {
        eprintln!("media_session: attach failed ({e})");
        return;
    }
    let mut controls = controls;
    loop {
        tokio::time::sleep(Duration::from_millis(500)).await;
        let rep = match sidecar.status() {
            Ok(r) => r,
            Err(_) => continue,
        };
        let duration = (rep.duration_ms > 0).then(|| Duration::from_millis(rep.duration_ms));
        let meta = MediaMetadata {
            title: rep.title.as_deref(),
            artist: rep.artist.as_deref(),
            album: rep.album.as_deref(),
            cover_url: rep.art_url.as_deref(),
            duration,
        };
        if let Err(e) = controls.set_metadata(meta) {
            eprintln!("media_session: set_metadata ({e})");
        }
        if let Err(e) = controls.set_playback(playback_for(&rep)) {
            eprintln!("media_session: set_playback ({e})");
        }
    }
}
