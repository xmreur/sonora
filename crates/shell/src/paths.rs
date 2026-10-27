//! Application config directory (tokens, Firefox profile, notification art cache).

#[cfg(target_os = "linux")]
use std::path::Path;
use std::path::PathBuf;
use std::sync::OnceLock;

static CONFIG_DIR: OnceLock<PathBuf> = OnceLock::new();

/// Linux keeps the pre–cross-platform `~/.config/sonora` tree (MUT, Firefox
/// profile, caches). Other platforms use the per-app config directory
/// handed in by [`init_default_config_dir`].
pub fn init_config_dir(fallback: PathBuf) {
    let dir = canonical_config_dir(fallback);
    #[cfg(target_os = "linux")]
    migrate_linux_legacy(&dir);
    let _ = CONFIG_DIR.set(dir);
}

fn canonical_config_dir(fallback: PathBuf) -> PathBuf {
    #[cfg(target_os = "linux")]
    {
        dirs::config_dir()
            .map(|d| d.join("sonora"))
            .unwrap_or(fallback)
    }
    #[cfg(not(target_os = "linux"))]
    {
        fallback
    }
}

/// Initialize from the app itself: the platform's per-app config
/// directory (`~/.config/sonora` on Linux, `%APPDATA%\com.xmreur.sonora`
/// on Windows, `~/Library/Application Support/com.xmreur.sonora` on
/// macOS), canonicalized the same way on every caller.
pub fn init_default_config_dir() {
    let identifier = dirs::config_dir()
        .map(|d| d.join("com.xmreur.sonora"))
        .unwrap_or_default();
    init_config_dir(identifier);
}

pub fn app_config_dir() -> Option<PathBuf> {
    CONFIG_DIR
        .get()
        .cloned()
        .or_else(|| dirs::config_dir().map(|d| d.join("sonora")))
}

#[cfg(target_os = "linux")]
fn migrate_linux_legacy(new: &Path) {
    if let Ok(home) = std::env::var("HOME") {
        let old = PathBuf::from(home).join(".config/apple-music-linux");
        migrate_legacy_config(&old, new);
    }
}

#[cfg(target_os = "linux")]
fn migrate_legacy_config(old: &Path, new: &Path) {
    if !old.exists() {
        return;
    }
    let old_profile = old.join("firefox-profile");
    let needle = old_profile.to_string_lossy();
    if !needle.is_empty() {
        let _ = std::process::Command::new("pkill")
            .args(["-9", "-f", needle.as_ref()])
            .status();
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    if !new.exists() {
        if std::fs::rename(old, new).is_ok() {
            return;
        }
        if copy_dir_all(old, new).is_ok() {
            let _ = std::fs::remove_dir_all(old);
        }
        return;
    }
    let _ = copy_dir_all(old, new);
    let _ = std::fs::remove_dir_all(old);
}

#[cfg(target_os = "linux")]
fn copy_dir_all(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let to = dst.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir_all(&entry.path(), &to)?;
        } else if !to.exists() {
            std::fs::copy(entry.path(), &to)?;
        }
    }
    Ok(())
}
