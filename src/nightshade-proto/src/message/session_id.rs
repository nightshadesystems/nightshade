//! Session identifiers.
//!
//! # Why this is a validated type and not a `String`
//!
//! A session id becomes a filename: configd backs each candidate to
//! `/run/nightshade/sessions/<id>.json`, as root. A client that could send
//! `../../etc/nightshade/config` as its session id would have a write
//! primitive into `/etc`.
//!
//! So the constraint is enforced in `Deserialize`, not at the point of use. A
//! check at the point of use is a check that has to be repeated at the next
//! point of use, and the one that gets forgotten is the one that matters. By
//! the time a `SessionId` exists at all, it is sixteen lowercase hex digits
//! and cannot be anything else.
//!
//! An id is not a capability either. configd records which uid opened a
//! session and refuses requests for it from any other, so knowing one is not
//! enough to drive it.

use std::fmt;

use serde::{Deserialize, Deserializer, Serialize};

/// Hex digits in an id. Sixty-four bits: enough that ids do not collide,
/// short enough to read out over a phone while someone is looking at a log.
const LEN: usize = 16;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("not a session id")]
pub struct SessionIdError;

#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
#[serde(transparent)]
pub struct SessionId(String);

impl SessionId {
    /// Bytes of randomness behind an id.
    pub const ENTROPY_BYTES: usize = LEN / 2;

    pub fn parse(text: &str) -> Result<Self, SessionIdError> {
        if text.len() == LEN && text.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            Ok(Self(text.to_string()))
        } else {
            Err(SessionIdError)
        }
    }

    /// Build an id from random bytes. The caller supplies the randomness so
    /// this crate does not need an opinion about where it comes from.
    pub fn from_bytes(bytes: [u8; Self::ENTROPY_BYTES]) -> Self {
        let mut text = String::with_capacity(LEN);
        for byte in bytes {
            text.push(char::from_digit((byte >> 4) as u32, 16).expect("nibble"));
            text.push(char::from_digit((byte & 0xf) as u32, 16).expect("nibble"));
        }
        Self(text)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for SessionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Shown in full. An id is not a secret, and a redacted one in a log is a log
/// that cannot be matched against a session.
impl fmt::Debug for SessionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "SessionId({})", self.0)
    }
}

impl<'de> Deserialize<'de> for SessionId {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        SessionId::parse(&text).map_err(|_| {
            // Deliberately does not quote the offending value back. It came
            // off a socket and ends up in the journal.
            serde::de::Error::custom(format!(
                "a session id is {LEN} lowercase hex digits"
            ))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame;

    #[test]
    fn random_bytes_become_an_id() {
        let id = SessionId::from_bytes([0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef]);
        assert_eq!(id.as_str(), "0123456789abcdef");
        assert_eq!(SessionId::parse(id.as_str()).unwrap(), id);
    }

    #[test]
    fn every_byte_pattern_produces_a_parseable_id() {
        for byte in 0u8..=255 {
            let id = SessionId::from_bytes([byte; SessionId::ENTROPY_BYTES]);
            SessionId::parse(id.as_str())
                .unwrap_or_else(|_| panic!("{byte:#04x} produced {id}, which does not parse"));
        }
    }

    #[test]
    fn anything_that_is_not_sixteen_hex_digits_is_refused() {
        for bad in [
            "",
            "0123456789abcde",           // short
            "0123456789abcdef0",         // long
            "0123456789ABCDEF",          // upper case
            "../../etc/nightshade/con",  // traversal
            "0123456789abcde/",
            "0123456789abcde.",
            // Right length, embedded NUL -- what a truncation attack against
            // anything that later hands the name to a C API would look like.
            "\u{0}123456789abcdef",
            "zzzzzzzzzzzzzzzz",
        ] {
            assert!(SessionId::parse(bad).is_err(), "{bad:?} was accepted");
        }
    }

    /// The check has to be in `Deserialize`, or a crafted frame walks straight
    /// past it into a filename.
    #[test]
    fn a_crafted_id_does_not_survive_the_wire() {
        for bad in ["../../../etc/nightshade/config", "..", "/etc/passwd", ""] {
            let frame = frame::encode(&bad.to_string()).unwrap();
            let decoded: Result<SessionId, _> = frame::decode(&frame[frame::HEADER..]);
            assert!(decoded.is_err(), "{bad:?} decoded into a SessionId");
        }
    }

    #[test]
    fn ids_survive_the_wire() {
        let id = SessionId::from_bytes([0xde, 0xad, 0xbe, 0xef, 0x00, 0x11, 0x22, 0x33]);
        let frame = frame::encode(&id).unwrap();
        let back: SessionId = frame::decode(&frame[frame::HEADER..]).unwrap();
        assert_eq!(back, id);
    }
}
