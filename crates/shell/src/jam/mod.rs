//! Serverless Jams: a host-authoritative room over iroh.
//!
//! No process here listens on a public port. iroh punches UDP and, when
//! that fails, falls back to a public relay. The relay only forwards
//! ciphertext — the QUIC handshake is end to end.

mod clock;
mod node;
mod protocol;
mod ticket;

pub use clock::{expected_position_ms, unix_ms, ClockSync, DriftGate};
pub use node::{JamEvent, JamManager, PublishUpdate};
pub use protocol::{
    aggregate_path, authorize, JamAction, JamRole, JamSnapshot, JamTrack, Participant, PathKind,
    PlaybackAnchor, MAX_QUEUE,
};
pub use ticket::{decode_ticket, encode_ticket};
