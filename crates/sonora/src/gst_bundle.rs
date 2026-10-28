//! GStreamer runtime discovery for packaged installs.
//!
//! The release bundles carry the GStreamer runtime next to the binary:
//! the Windows installer lays the runtime DLLs beside `sonora.exe` (the
//! loader finds those on its own) with the plugin objects in `plugins\`,
//! and the macOS `.app` carries its Homebrew-built dylibs in
//! `Contents/Frameworks` with the plugin objects in
//! `Contents/Resources/gstreamer/plugins`. On a dev machine none of that
//! exists and the system GStreamer owns the registry, so the env var is
//! only set when a bundle layout is actually detected — and never when
//! the user already pointed GStreamer somewhere themselves.
//!
//! (The Linux AppImage doesn't need this: its `AppRun` exports
//! `GST_PLUGIN_SYSTEM_PATH_1_0` before the binary starts.)

use std::path::{Path, PathBuf};

/// Candidate plugin directories relative to the running executable:
/// the Windows installer layout first, then the macOS `.app` layout
/// (exe = `Contents/MacOS/sonora`).
fn candidate_plugin_dirs(exe_dir: &Path) -> [PathBuf; 2] {
    [
        exe_dir.join("plugins"),
        exe_dir.join("../Resources/gstreamer/plugins"),
    ]
}

/// The bundled plugin directory for this executable, if one exists.
pub fn bundled_plugin_dir(exe: &Path) -> Option<PathBuf> {
    let exe_dir = exe.parent()?;
    candidate_plugin_dirs(exe_dir)
        .into_iter()
        .find(|dir| dir.is_dir())
        // Resolve the `..` in the macOS candidate so the env var carries
        // a clean path (canonicalize cannot fail: the dir just existed).
        .and_then(|dir| dir.canonicalize().ok())
}

/// Point `GST_PLUGIN_SYSTEM_PATH_1_0` at the bundled plugins when we run
/// from a packaged install and the user hasn't set the variable already.
/// Must run before the first `gstreamer::init()`.
pub fn ensure_plugin_path() {
    if std::env::var_os("GST_PLUGIN_SYSTEM_PATH_1_0").is_some() {
        return;
    }
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    if let Some(dir) = bundled_plugin_dir(&exe) {
        std::env::set_var("GST_PLUGIN_SYSTEM_PATH_1_0", dir);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn scratch(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "sonora-gst-bundle-{}-{nanos}-{name}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn no_bundle_layout_yields_none() {
        let dir = scratch("plain");
        assert_eq!(bundled_plugin_dir(&dir.join("sonora")), None);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn windows_installer_layout_is_found() {
        let dir = scratch("windows");
        fs::create_dir_all(dir.join("plugins")).unwrap();
        // Canonicalized on both sides: temp dirs can be symlinked (macOS
        // /tmp → /private/tmp) and bundled_plugin_dir canonicalizes.
        assert_eq!(
            bundled_plugin_dir(&dir.join("sonora.exe")),
            dir.join("plugins").canonicalize().ok()
        );
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn macos_app_layout_is_found() {
        let dir = scratch("macos");
        let macos_dir = dir.join("Contents/MacOS");
        let plugins = dir.join("Contents/Resources/gstreamer/plugins");
        fs::create_dir_all(&macos_dir).unwrap();
        fs::create_dir_all(&plugins).unwrap();
        assert_eq!(
            bundled_plugin_dir(&macos_dir.join("sonora")),
            plugins.canonicalize().ok()
        );
        fs::remove_dir_all(&dir).ok();
    }
}
