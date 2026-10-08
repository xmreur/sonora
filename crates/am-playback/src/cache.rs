//! Decrypted-track disk cache.
//!
//! `<cache>/sonora/applemusic/<adam>.m4a`, published via `.part` + rename
//! so a crash mid-write can never be replayed as a valid hit.

use std::path::PathBuf;

pub fn cache_path(adam_id: &str) -> PathBuf {
    let safe: String = adam_id
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        .collect();
    directories::ProjectDirs::from("", "sonora", "sonora")
        .map(|d| d.cache_dir().join("applemusic"))
        .unwrap_or_else(|| std::env::temp_dir().join("sonora-applemusic"))
        .join(format!("{safe}.m4a"))
}

pub fn load(adam_id: &str) -> Option<Vec<u8>> {
    let bytes = std::fs::read(cache_path(adam_id)).ok()?;
    if bytes.is_empty() {
        None
    } else {
        Some(bytes)
    }
}

pub fn store(adam_id: &str, bytes: &[u8]) {
    let path = cache_path(adam_id);
    let Some(dir) = path.parent() else { return };
    if std::fs::create_dir_all(dir).is_err() {
        return;
    }
    let staging = path.with_extension("part");
    if std::fs::write(&staging, bytes).is_err() {
        return;
    }
    if std::fs::rename(&staging, &path).is_err() {
        let _ = std::fs::remove_file(&staging);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips() {
        let id = format!("test-{}", std::process::id());
        store(&id, b"hello");
        assert_eq!(load(&id).as_deref(), Some(b"hello".as_slice()));
        let _ = std::fs::remove_file(cache_path(&id));
    }

    #[test]
    fn empty_is_miss() {
        let id = format!("empty-{}", std::process::id());
        store(&id, b"");
        // Empty write still publishes an empty file; load treats it as miss.
        assert_eq!(load(&id), None);
        let _ = std::fs::remove_file(cache_path(&id));
    }
}
