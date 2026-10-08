//! OS media session: MPRIS (Linux), SMTC (Windows), Now Playing (macOS).

pub mod common;

#[cfg(target_os = "linux")]
mod linux;

#[cfg(any(windows, target_os = "macos"))]
mod souvlaki_backend;

/// Windows needs the main window HWND for System Media Transport Controls.
pub struct MediaSessionConfig {
    #[cfg(windows)]
    pub hwnd: Option<isize>,
}

impl MediaSessionConfig {
    pub fn new() -> Self {
        Self {
            #[cfg(windows)]
            hwnd: None,
        }
    }
}

pub async fn run(player: crate::native_player::NativePlayer, config: MediaSessionConfig) {
    #[cfg(target_os = "linux")]
    {
        let _ = config;
        linux::run(player).await;
    }

    #[cfg(any(windows, target_os = "macos"))]
    {
        let notify_player = player.clone();
        tokio::spawn(async move {
            common::notification_poll_loop(notify_player).await;
        });
        souvlaki_backend::run(player, config).await;
    }
}
