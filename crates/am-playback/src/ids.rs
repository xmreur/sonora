//! Catalog vs library id dispatch + playback constants.
//!
//! The web playback API only understands numeric catalog (Adam) ids.
//! Library ids (`i.*`, `l.*`, `a.*`, `p.*` + alphanumeric tail) must be
//! resolved to catalog ids first — the caller does that with the existing
//! `ApiClient::catalog_id_for_library_song`; these helpers only classify.

use serde_json::Value;

/// The only catalog encode our Widevine CDM can open (`cbcp` flavours use
/// Apple's own `skd://` key delivery).
pub const CTR_FLAVOR: &str = "28:ctrp256";

/// True for iCloud Music Library ids, as opposed to numeric catalog Adam ids.
pub fn is_library_id(id: &str) -> bool {
    let mut chars = id.chars();
    if !matches!(chars.next(), Some('a' | 'i' | 'l' | 'p')) || chars.next() != Some('.') {
        return false;
    }
    let tail = &id[2..];
    !tail.is_empty() && tail.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}

/// The `webPlayback` request body for an id.
pub fn web_playback_body(id: &str) -> Value {
    if is_library_id(id) {
        serde_json::json!({ "universalLibraryId": id })
    } else {
        serde_json::json!({ "salableAdamId": id })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_ids() {
        assert!(is_library_id("i.ZOMr5KaurEbG7lz"));
        assert!(is_library_id("l.abc123"));
        assert!(is_library_id("p.playlist-1"));
        assert!(is_library_id("a.Album2"));
        assert!(!is_library_id("1811922756"));
        assert!(!is_library_id("i."));
        assert!(!is_library_id("x.abc"));
        assert!(!is_library_id(""));
    }

    #[test]
    fn body_dispatch() {
        assert_eq!(
            web_playback_body("i.ZOMr5KaurEbG7lz"),
            serde_json::json!({ "universalLibraryId": "i.ZOMr5KaurEbG7lz" })
        );
        assert_eq!(
            web_playback_body("1811922756"),
            serde_json::json!({ "salableAdamId": "1811922756" })
        );
    }
}
