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

pub async fn run(sidecar: crate::sidecar::SidecarManager, config: MediaSessionConfig) {
    #[cfg(target_os = "linux")]
    {
        let _ = config;
        linux::run(sidecar).await;
    }

    #[cfg(any(windows, target_os = "macos"))]
    {
        let notify_sc = sidecar.clone();
        tokio::spawn(async move {
            common::notification_poll_loop(notify_sc).await;
        });
        souvlaki_backend::run(sidecar, config).await;
    }
}
