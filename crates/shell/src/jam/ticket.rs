//! Copy-paste invite codes.
//!
//! `sonora-jam:1:<base64url>` carries the host [`EndpointAddr`] (relay and
//! direct addresses, when iroh has learned them) plus a 128-bit room
//! secret. The secret dies with the host's ephemeral endpoint key, so an
//! old code cannot rejoin a later jam.

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use iroh::EndpointAddr;
use serde::{Deserialize, Serialize};

const PREFIX: &str = "sonora-jam:1:";
const MAX_TICKET_BYTES: usize = 8 * 1024;

#[derive(Debug, Serialize, Deserialize)]
struct TicketBody {
    addr: EndpointAddr,
    room: [u8; 16],
}

pub fn encode_ticket(addr: &EndpointAddr, room: &[u8; 16]) -> Result<String, String> {
    let body = TicketBody {
        addr: addr.clone(),
        room: *room,
    };
    let bytes = serde_json::to_vec(&body).map_err(|e| e.to_string())?;
    if bytes.len() > MAX_TICKET_BYTES {
        return Err("invite is too large".into());
    }
    Ok(format!("{PREFIX}{}", URL_SAFE_NO_PAD.encode(bytes)))
}

pub fn decode_ticket(raw: &str) -> Result<(EndpointAddr, [u8; 16]), String> {
    let raw = raw.trim();
    let Some(rest) = raw.strip_prefix(PREFIX) else {
        return Err("that is not a Sonora jam code".into());
    };
    if rest.is_empty() || rest.len() > MAX_TICKET_BYTES * 2 {
        return Err("jam code is invalid".into());
    }
    let bytes = URL_SAFE_NO_PAD
        .decode(rest)
        .map_err(|_| "jam code is invalid".to_string())?;
    if bytes.len() > MAX_TICKET_BYTES {
        return Err("jam code is invalid".into());
    }
    let body: TicketBody =
        serde_json::from_slice(&bytes).map_err(|_| "jam code is invalid".to_string())?;
    Ok((body.addr, body.room))
}

pub fn room_hex(room: &[u8; 16]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(32);
    for b in room {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0xf) as usize] as char);
    }
    out
}

pub fn parse_room_hex(s: &str) -> Option<[u8; 16]> {
    if s.len() != 32 || !s.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let mut out = [0u8; 16];
    for (i, chunk) in s.as_bytes().chunks(2).enumerate() {
        let hi = hex_val(chunk[0])?;
        let lo = hex_val(chunk[1])?;
        out[i] = (hi << 4) | lo;
    }
    Some(out)
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Length-checked compare. A mismatched length still walks the longer
/// side so the timing does not advertise which half failed.
pub fn room_matches(expected: &[u8; 16], presented: &[u8]) -> bool {
    let mut diff = expected.len() ^ presented.len();
    let n = expected.len().max(presented.len());
    for i in 0..n {
        let a = expected.get(i).copied().unwrap_or(0);
        let b = presented.get(i).copied().unwrap_or(0);
        diff |= (a ^ b) as usize;
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use iroh::{EndpointId, SecretKey};

    fn sample_addr() -> EndpointAddr {
        let id = EndpointId::from(SecretKey::generate().public());
        EndpointAddr::new(id).with_ip_addr("127.0.0.1:9".parse().unwrap())
    }

    #[test]
    fn ticket_round_trips() {
        let addr = sample_addr();
        let room = [7u8; 16];
        let code = encode_ticket(&addr, &room).unwrap();
        assert!(code.starts_with("sonora-jam:1:"));
        let (back, secret) = decode_ticket(&code).unwrap();
        assert_eq!(back, addr);
        assert_eq!(secret, room);
        assert!(decode_ticket(&format!("  {code}\n")).is_ok());
    }

    #[test]
    fn malformed_and_tampered_codes_fail() {
        assert!(decode_ticket("").is_err());
        assert!(decode_ticket("https://example.invalid/jam").is_err());
        assert!(decode_ticket("sonora-jam:1:").is_err());
        assert!(decode_ticket("sonora-jam:1:!!!!").is_err());
        let code = encode_ticket(&sample_addr(), &[1u8; 16]).unwrap();
        let mut tampered = code.clone();
        let idx = tampered.len() - 2;
        let ch = tampered.as_bytes()[idx];
        let flip = if ch == b'A' { b'B' } else { b'A' };
        tampered.replace_range(idx..idx + 1, std::str::from_utf8(&[flip]).unwrap());
        assert!(decode_ticket(&tampered).is_err() || tampered == code);
        // A flipped payload byte that still base64-decodes must not yield
        // the original room secret.
        if let Ok((_, room)) = decode_ticket(&tampered) {
            assert_ne!(room, [1u8; 16]);
        }
    }

    #[test]
    fn room_hex_and_constant_compare() {
        let room = [0x0au8, 0x1b, 0xff, 0x00, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12];
        let hex = room_hex(&room);
        assert_eq!(parse_room_hex(&hex), Some(room));
        assert!(parse_room_hex("abcd").is_none());
        assert!(parse_room_hex(&"zz".repeat(16)).is_none());
        assert!(room_matches(&room, &room));
        assert!(!room_matches(&room, &[]));
        let mut wrong = room;
        wrong[15] ^= 1;
        assert!(!room_matches(&room, &wrong));
    }
}
