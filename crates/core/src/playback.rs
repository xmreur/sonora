use serde::{Deserialize, Serialize};

/// One queued entry for playback IPC. `kind` is `"song"` (a catalog or
/// library song id), `"album"`, or `"playlist"` (expanded server-side).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct QueueItem {
    pub id: String,
    #[serde(default)]
    pub kind: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queue_item_defaults_kind() {
        let item: QueueItem = serde_json::from_value(serde_json::json!({"id": "1"})).unwrap();
        assert_eq!(item.kind, "");
        let back = serde_json::to_value(&QueueItem {
            id: "1".into(),
            kind: "song".into(),
        })
        .unwrap();
        assert_eq!(back.get("kind").and_then(|k| k.as_str()), Some("song"));
    }
}
