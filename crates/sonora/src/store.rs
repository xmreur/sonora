//! UI settings persistence — native JSON in the config dir (the old web
//! UI kept these in `localStorage`, which cannot migrate across toolkits;
//! a fresh start at upgrade was an accepted gap).

use std::path::PathBuf;

use crate::app::Settings;

pub fn settings_path() -> Option<PathBuf> {
    shell::paths::app_config_dir().map(|d| d.join("ui-settings.json"))
}

/// Load persisted settings, falling back to defaults on a missing or
/// corrupt file.
pub fn load() -> Settings {
    settings_path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

/// Write settings to disk; returns `true` when the file changed.
pub fn save_if_changed(settings: &Settings, last_saved: &mut Option<String>) -> bool {
    let Ok(json) = serde_json::to_string(settings) else {
        return false;
    };
    if last_saved.as_ref() == Some(&json) {
        return false;
    }
    let Some(path) = settings_path() else {
        return false;
    };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if std::fs::write(&path, &json).is_ok() {
        *last_saved = Some(json);
        true
    } else {
        false
    }
}
