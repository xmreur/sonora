//! Application config directory (tokens, Firefox profile, notification art cache).

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

static CONFIG_DIR: OnceLock<PathBuf> = OnceLock::new();

/// Linux keeps the pre–cross-platform `~/.config/sonora` tree (MUT, Firefox
/// profile, caches). Other platforms use Tauri's per-app config directory.
pub fn init_config_dir(tauri_dir: PathBuf) {
    let dir = canonical_config_dir(tauri_dir);
    #[cfg(target_os = "linux")]
    migrate_linux_legacy(&dir);
    let _ = CONFIG_DIR.set(dir);
}

fn canonical_config_dir(tauri_dir: PathBuf) -> PathBuf {
    #[cfg(target_os = "linux")]
    {
        dirs::config_dir()
            .map(|d| d.join("sonora"))
            .unwrap_or(tauri_dir)
    }
    #[cfg(not(target_os = "linux"))]
    {
        tauri_dir
    }
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
