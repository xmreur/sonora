use serde::{Deserialize, Serialize};

/// One queued entry for playback IPC. `kind` is `"song"` (a catalog or
/// library song id), `"album"`, or `"playlist"` (expanded server-side).
/// `title`/`artist` ride along when the UI knows them so a dead id can be
/// re-found by metadata search instead of failing outright.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct QueueItem {
    pub id: String,
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub artist: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queue_item_defaults_kind() {
        let item: QueueItem = serde_json::from_value(serde_json::json!({"id": "1"})).unwrap();
        assert_eq!(item.kind, "");
        assert_eq!(item.title, None);
        let back = serde_json::to_value(&QueueItem {
            id: "1".into(),
            kind: "song".into(),
            title: Some("T".into()),
            artist: None,
        })
        .unwrap();
        assert_eq!(back.get("kind").and_then(|k| k.as_str()), Some("song"));
        assert_eq!(back.get("title").and_then(|t| t.as_str()), Some("T"));
    }
}
