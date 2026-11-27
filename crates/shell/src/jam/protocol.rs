//! Jam messages. One length-prefixed JSON frame per message, capped so a
//! peer cannot pin memory with a huge length.

use serde::{Deserialize, Serialize};

use super::ticket::{parse_room_hex, room_matches};

pub const ALPN: &[u8] = b"sonora/jam/1";
pub const PROTOCOL_VERSION: u32 = 1;
pub const MAX_FRAME: usize = 256 * 1024;
pub const MAX_QUEUE: usize = 300;
pub const MAX_GUESTS: usize = 15;
pub const MAX_NAME: usize = 32;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JamRole {
    Host,
    Guest,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PathKind {
    Unknown,
    Direct,
    Relayed,
}

impl PathKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Unknown => "Connecting",
            Self::Direct => "Direct P2P",
            Self::Relayed => "Relayed",
        }
    }
}

/// Direct wins over relay only when every connected guest is direct.
/// One relayed guest makes the room "Relayed" so the host does not
/// claim a path the whole room does not have.
pub fn aggregate_path(kinds: &[PathKind]) -> PathKind {
    if kinds.iter().any(|k| *k == PathKind::Relayed) {
        PathKind::Relayed
    } else if kinds.iter().any(|k| *k == PathKind::Direct) {
        PathKind::Direct
    } else {
        PathKind::Unknown
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct JamTrack {
    pub uid: u64,
    pub catalog_id: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub artist: String,
    #[serde(default)]
    pub album: String,
    #[serde(default)]
    pub duration_ms: u64,
    #[serde(default)]
    pub art_url: Option<String>,
    #[serde(default)]
    pub isrc: Option<String>,
    #[serde(default)]
    pub added_by: String,
    /// Endpoint id of whoever added the row. Removal rights key off this,
    /// not the display name.
    #[serde(default)]
    pub added_by_id: String,
    #[serde(default)]
    pub radio: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlaybackAnchor {
    pub playing: bool,
    pub position_ms: u64,
    /// Host wall clock (`unix_ms`) when `position_ms` was true.
    pub anchor_host_ms: u64,
    #[serde(default)]
    pub track_uid: Option<u64>,
    #[serde(default)]
    pub loop_one: bool,
    #[serde(default)]
    pub shuffle: bool,
}

impl Default for PlaybackAnchor {
    fn default() -> Self {
        Self {
            playing: false,
            position_ms: 0,
            anchor_host_ms: 0,
            track_uid: None,
            loop_one: false,
            shuffle: false,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Participant {
    pub peer_id: String,
    pub name: String,
    pub host: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct JamSnapshot {
    pub revision: u64,
    pub guests_can_control: bool,
    pub participants: Vec<Participant>,
    pub queue: Vec<JamTrack>,
    #[serde(default)]
    pub current_index: Option<usize>,
    pub playback: PlaybackAnchor,
}

impl JamSnapshot {
    pub fn empty(host_id: String, host_name: String, guests_can_control: bool) -> Self {
        Self {
            revision: 0,
            guests_can_control,
            participants: vec![Participant {
                peer_id: host_id,
                name: host_name,
                host: true,
            }],
            queue: Vec::new(),
            current_index: None,
            playback: PlaybackAnchor::default(),
        }
    }
}

/// What a guest asks the host to do. The host's UI is the only thing
/// that mutates playback; the node only decides whether the ask is allowed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum JamAction {
    QueueAppend { tracks: Vec<JamTrack> },
    QueuePlayNext { tracks: Vec<JamTrack> },
    QueueRemove { uid: u64 },
    PlayPause,
    Next,
    Prev,
    JumpTo { uid: u64 },
    Seek { position_ms: u64 },
    QueueClear,
    /// Guest with control replaces the room queue and starts at `start`.
    Replace { tracks: Vec<JamTrack>, start: usize },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum ClientMsg {
    Hello {
        protocol_version: u32,
        room_secret: String,
        display_name: String,
    },
    Ping {
        seq: u64,
        t1_guest_ms: u64,
    },
    Leave,
    QueueAppend {
        tracks: Vec<JamTrack>,
    },
    QueuePlayNext {
        tracks: Vec<JamTrack>,
    },
    QueueRemove {
        uid: u64,
    },
    PlayPause,
    Next,
    Prev,
    JumpTo {
        uid: u64,
    },
    Seek {
        position_ms: u64,
    },
    QueueClear,
    Replace {
        tracks: Vec<JamTrack>,
        start: usize,
    },
}

impl From<JamAction> for ClientMsg {
    fn from(action: JamAction) -> Self {
        match action {
            JamAction::QueueAppend { tracks } => Self::QueueAppend { tracks },
            JamAction::QueuePlayNext { tracks } => Self::QueuePlayNext { tracks },
            JamAction::QueueRemove { uid } => Self::QueueRemove { uid },
            JamAction::PlayPause => Self::PlayPause,
            JamAction::Next => Self::Next,
            JamAction::Prev => Self::Prev,
            JamAction::JumpTo { uid } => Self::JumpTo { uid },
            JamAction::Seek { position_ms } => Self::Seek { position_ms },
            JamAction::QueueClear => Self::QueueClear,
            JamAction::Replace { tracks, start } => Self::Replace { tracks, start },
        }
    }
}

impl ClientMsg {
    pub fn action(self) -> Option<JamAction> {
        match self {
            Self::QueueAppend { tracks } => Some(JamAction::QueueAppend { tracks }),
            Self::QueuePlayNext { tracks } => Some(JamAction::QueuePlayNext { tracks }),
            Self::QueueRemove { uid } => Some(JamAction::QueueRemove { uid }),
            Self::PlayPause => Some(JamAction::PlayPause),
            Self::Next => Some(JamAction::Next),
            Self::Prev => Some(JamAction::Prev),
            Self::JumpTo { uid } => Some(JamAction::JumpTo { uid }),
            Self::Seek { position_ms } => Some(JamAction::Seek { position_ms }),
            Self::QueueClear => Some(JamAction::QueueClear),
            Self::Replace { tracks, start } => Some(JamAction::Replace { tracks, start }),
            Self::Hello { .. } | Self::Ping { .. } | Self::Leave => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum HostMsg {
    Welcome {
        peer_id: String,
        snapshot: JamSnapshot,
        host_now_ms: u64,
    },
    Reject {
        reason: String,
    },
    Pong {
        seq: u64,
        t1_guest_ms: u64,
        t2_host_ms: u64,
        t3_host_ms: u64,
    },
    StateSync {
        snapshot: JamSnapshot,
    },
    Heartbeat {
        host_now_ms: u64,
        playing: bool,
        position_ms: u64,
        track_uid: Option<u64>,
    },
    Denied {
        reason: String,
    },
    JamEnded {
        reason: String,
    },
}

pub fn clean_name(name: &str) -> String {
    let s: String = name
        .chars()
        .filter(|c| !c.is_control())
        .take(MAX_NAME)
        .collect();
    let s = s.trim().to_string();
    if s.is_empty() {
        "Listener".to_string()
    } else {
        s
    }
}

pub fn frame_len_ok(len: usize) -> bool {
    len <= MAX_FRAME
}

/// `Ok(())` when this guest may ask for `action` against the last
/// snapshot the host published. Own-row removal stays allowed after the
/// host turns control off.
pub fn authorize(peer_id: &str, action: &JamAction, snap: &JamSnapshot) -> Result<(), &'static str> {
    match action {
        JamAction::QueueAppend { tracks } | JamAction::QueuePlayNext { tracks } => {
            if tracks.is_empty() {
                return Err("nothing to add");
            }
            if tracks.len() > MAX_QUEUE {
                return Err("that is too many tracks");
            }
            if tracks
                .iter()
                .any(|t| t.catalog_id.is_empty() || t.catalog_id.starts_with("file:"))
            {
                return Err("local files cannot be shared");
            }
            if snap.queue.len().saturating_add(tracks.len()) > MAX_QUEUE {
                return Err("the jam queue is full");
            }
            Ok(())
        }
        JamAction::QueueRemove { uid } => {
            let Some(track) = snap.queue.iter().find(|t| t.uid == *uid) else {
                return Err("that track is not in the jam");
            };
            if snap.guests_can_control || track.added_by_id == peer_id {
                Ok(())
            } else {
                Err("only the host can remove that track")
            }
        }
        JamAction::Replace { tracks, .. } => {
            if !snap.guests_can_control {
                return Err("only the host can control playback");
            }
            if tracks.len() > MAX_QUEUE {
                return Err("that is too many tracks");
            }
            if tracks
                .iter()
                .any(|t| t.catalog_id.is_empty() || t.catalog_id.starts_with("file:"))
            {
                return Err("local files cannot be shared");
            }
            Ok(())
        }
        JamAction::PlayPause
        | JamAction::Next
        | JamAction::Prev
        | JamAction::JumpTo { .. }
        | JamAction::Seek { .. }
        | JamAction::QueueClear => {
            if snap.guests_can_control {
                Ok(())
            } else {
                Err("only the host can control playback")
            }
        }
    }
}

pub fn hello_room_ok(expected: &[u8; 16], presented_hex: &str) -> bool {
    match parse_room_hex(presented_hex) {
        Some(bytes) => room_matches(expected, &bytes),
        None => false,
    }
}

/// Force attribution to the connection that actually sent the tracks.
/// A guest cannot claim someone else's rows.
pub fn stamp_action(peer_id: &str, name: &str, action: JamAction) -> JamAction {
    let stamp_tracks = |tracks: Vec<JamTrack>| {
        tracks
            .into_iter()
            .map(|mut t| {
                t.uid = 0;
                t.added_by = name.to_string();
                t.added_by_id = peer_id.to_string();
                t
            })
            .collect()
    };
    match action {
        JamAction::QueueAppend { tracks } => JamAction::QueueAppend {
            tracks: stamp_tracks(tracks),
        },
        JamAction::QueuePlayNext { tracks } => JamAction::QueuePlayNext {
            tracks: stamp_tracks(tracks),
        },
        JamAction::Replace { tracks, start } => JamAction::Replace {
            tracks: stamp_tracks(tracks),
            start,
        },
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(control: bool) -> JamSnapshot {
        let mut s = JamSnapshot::empty("host".into(), "Ada".into(), control);
        s.queue.push(JamTrack {
            uid: 1,
            catalog_id: "111".into(),
            title: "Own".into(),
            artist: String::new(),
            album: String::new(),
            duration_ms: 0,
            art_url: None,
            isrc: None,
            added_by: "Bea".into(),
            added_by_id: "guest".into(),
            radio: false,
        });
        s.queue.push(JamTrack {
            uid: 2,
            catalog_id: "222".into(),
            added_by: "Ada".into(),
            added_by_id: "host".into(),
            ..s.queue[0].clone()
        });
        s
    }

    fn track(id: &str) -> JamTrack {
        JamTrack {
            uid: 0,
            catalog_id: id.into(),
            title: id.into(),
            artist: String::new(),
            album: String::new(),
            duration_ms: 1,
            art_url: None,
            isrc: None,
            added_by: String::new(),
            added_by_id: String::new(),
            radio: false,
        }
    }

    #[test]
    fn guests_add_freely_but_control_is_gated() {
        let locked = snap(false);
        assert!(authorize("guest", &JamAction::QueueAppend { tracks: vec![track("9")] }, &locked).is_ok());
        assert!(authorize("guest", &JamAction::Seek { position_ms: 10 }, &locked).is_err());
        assert!(authorize("guest", &JamAction::QueueClear, &locked).is_err());
        assert!(authorize("guest", &JamAction::QueueRemove { uid: 1 }, &locked).is_ok());
        assert!(authorize("guest", &JamAction::QueueRemove { uid: 2 }, &locked).is_err());
        let open = snap(true);
        assert!(authorize("guest", &JamAction::Next, &open).is_ok());
        assert!(authorize("guest", &JamAction::QueueRemove { uid: 2 }, &open).is_ok());
    }

    #[test]
    fn queue_cap_and_empty_adds() {
        let mut full = snap(true);
        full.queue = (0..MAX_QUEUE).map(|i| track(&i.to_string())).collect();
        assert!(authorize(
            "guest",
            &JamAction::QueueAppend {
                tracks: vec![track("x")]
            },
            &full
        )
        .is_err());
        assert!(authorize("guest", &JamAction::QueueAppend { tracks: vec![] }, &snap(true)).is_err());
        assert!(!frame_len_ok(MAX_FRAME + 1));
        assert!(frame_len_ok(MAX_FRAME));
    }

    #[test]
    fn messages_round_trip_and_paths_aggregate() {
        let msg = ClientMsg::from(JamAction::JumpTo { uid: 4 });
        let back: ClientMsg = serde_json::from_str(&serde_json::to_string(&msg).unwrap()).unwrap();
        assert_eq!(back.action(), Some(JamAction::JumpTo { uid: 4 }));
        let host = HostMsg::Reject {
            reason: "no".into(),
        };
        let back: HostMsg = serde_json::from_slice(&serde_json::to_vec(&host).unwrap()).unwrap();
        assert_eq!(back, host);
        assert_eq!(
            aggregate_path(&[PathKind::Direct, PathKind::Direct]),
            PathKind::Direct
        );
        assert_eq!(
            aggregate_path(&[PathKind::Direct, PathKind::Relayed]),
            PathKind::Relayed
        );
        assert_eq!(aggregate_path(&[]), PathKind::Unknown);
        assert_eq!(clean_name("  "), "Listener");
        assert_eq!(clean_name("Ada\nBee"), "AdaBee");
    }

    #[test]
    fn stamp_overwrites_spoofed_attribution() {
        let action = stamp_action(
            "real",
            "Bea",
            JamAction::QueueAppend {
                tracks: vec![JamTrack {
                    added_by: "Ada".into(),
                    added_by_id: "host".into(),
                    uid: 99,
                    ..track("5")
                }],
            },
        );
        match action {
            JamAction::QueueAppend { tracks } => {
                assert_eq!(tracks[0].added_by_id, "real");
                assert_eq!(tracks[0].added_by, "Bea");
                assert_eq!(tracks[0].uid, 0);
            }
            other => panic!("unexpected {other:?}"),
        }
    }
}
