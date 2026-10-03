//! Windows SMTC and macOS Now Playing via souvlaki.

#[cfg(windows)]
use std::ffi::c_void;
use std::sync::Arc;
use std::time::Duration;

use apple_music_core::playback::PlaybackCommand;
use souvlaki::{
    MediaControlEvent, MediaControls, MediaMetadata, MediaPlayback, MediaPosition, PlatformConfig,
    SeekDirection,
};

use crate::sidecar::SidecarManager;

use super::MediaSessionConfig;

const SEEK_STEP_MS: u64 = 5_000;

fn seek_by(sidecar: &SidecarManager, dir: SeekDirection, delta: Duration) {
    let pos = sidecar.status().map(|r| r.position_ms).unwrap_or(0);
    let ms = delta.as_millis().min(u64::MAX as u128) as u64;
    let position_ms = match dir {
        SeekDirection::Forward => pos.saturating_add(ms),
        SeekDirection::Backward => pos.saturating_sub(ms),
    };
    let _ = sidecar.enqueue(PlaybackCommand::Seek { position_ms });
}

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
        MediaControlEvent::Seek(dir) => {
            seek_by(sidecar, dir, Duration::from_millis(SEEK_STEP_MS));
        }
        MediaControlEvent::SeekBy(dir, delta) => {
            seek_by(sidecar, dir, delta);
        }
        MediaControlEvent::SetPosition(pos) => {
            let _ = sidecar.enqueue(PlaybackCommand::Seek {
                position_ms: pos.0.as_millis().max(0) as u64,
            });
        }
        MediaControlEvent::SetVolume(vol) => {
            let _ = sidecar.enqueue(PlaybackCommand::SetVolume {
                level: vol.clamp(0.0, 1.0) as f32,
            });
        }
        MediaControlEvent::OpenUri(_) => {}
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

fn platform_config(config: &MediaSessionConfig) -> PlatformConfig<'static> {
    #[cfg(windows)]
    let hwnd = config.hwnd.map(|h| h as *mut c_void);
    #[cfg(not(windows))]
    let hwnd = {
        let _ = config;
        None
    };
    PlatformConfig {
        dbus_name: "org.mpris.MediaPlayer2.sonora",
        display_name: "Sonora",
        hwnd,
    }
}

pub async fn run(sidecar: SidecarManager, config: MediaSessionConfig) {
    let platform = platform_config(&config);
    let mut controls = match MediaControls::new(platform) {
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
