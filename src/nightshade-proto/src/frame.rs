//! Framing.
//!
//! ```text
//! [u32 big-endian length][CBOR body]
//! ```
//!
//! That is the whole wire format. The peer on the other end of this socket
//! talks to a daemon running as root, so the amount of parser reachable before
//! anything is authenticated is a number worth keeping small.
//!
//! # The cap is checked on the length, not on the body
//!
//! [`body_len`] rejects an oversized frame from the four header bytes, before
//! a buffer is allocated for it. Reading first and checking afterwards would
//! mean a client could ask for four gigabytes and get it -- on a firewall
//! whose job is to keep running.
//!
//! # Why the rule is here and the reads are not
//!
//! configd reads with tokio and the CLI reads with `std::io`. Both need the
//! same three answers: how long is the header, is this length allowed, and
//! what does the body decode to. Those live here once; the loops that call
//! them are four lines each and belong with their own I/O.

use serde::Serialize;
use serde::de::DeserializeOwned;

/// Bytes of length prefix.
pub const HEADER: usize = 4;

/// Largest body accepted, in bytes.
///
/// A whole config is tens of kilobytes. Four megabytes is far past anything
/// legitimate and far short of anything that hurts, which is the correct shape
/// for a limit whose purpose is to bound the damage rather than to be tuned.
pub const MAX_BODY: usize = 4 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum FrameError {
    #[error("message of {len} bytes exceeds the {MAX_BODY} byte limit")]
    TooLarge { len: usize },

    #[error("empty message")]
    Empty,

    #[error("malformed message: {0}")]
    Malformed(String),

    #[error("encoding a message: {0}")]
    Encode(String),

    #[error("{0}")]
    Io(#[from] std::io::Error),
}

/// Validate a length prefix and return the body length it promises.
pub fn body_len(header: [u8; HEADER]) -> Result<usize, FrameError> {
    let len = u32::from_be_bytes(header) as usize;
    if len == 0 {
        return Err(FrameError::Empty);
    }
    if len > MAX_BODY {
        return Err(FrameError::TooLarge { len });
    }
    Ok(len)
}

/// Serialise `message` into a complete frame, prefix included.
pub fn encode<T: Serialize>(message: &T) -> Result<Vec<u8>, FrameError> {
    let mut body = Vec::new();
    ciborium::into_writer(message, &mut body).map_err(|e| FrameError::Encode(e.to_string()))?;
    if body.len() > MAX_BODY {
        return Err(FrameError::TooLarge { len: body.len() });
    }
    let mut frame = Vec::with_capacity(HEADER + body.len());
    frame.extend_from_slice(&(body.len() as u32).to_be_bytes());
    frame.extend_from_slice(&body);
    Ok(frame)
}

/// Deserialise a frame body.
pub fn decode<T: DeserializeOwned>(body: &[u8]) -> Result<T, FrameError> {
    ciborium::from_reader(body).map_err(|e| FrameError::Malformed(e.to_string()))
}

/// Blocking read of one message. What the CLI uses.
pub fn read_blocking<R: std::io::Read, T: DeserializeOwned>(
    reader: &mut R,
) -> Result<T, FrameError> {
    let mut header = [0u8; HEADER];
    reader.read_exact(&mut header)?;
    let len = body_len(header)?;
    let mut body = vec![0u8; len];
    reader.read_exact(&mut body)?;
    decode(&body)
}

/// Blocking write of one message.
pub fn write_blocking<W: std::io::Write, T: Serialize>(
    writer: &mut W,
    message: &T,
) -> Result<(), FrameError> {
    writer.write_all(&encode(message)?)?;
    writer.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_frame_round_trips() {
        let frame = encode(&"hello".to_string()).unwrap();
        let len = body_len(frame[..HEADER].try_into().unwrap()).unwrap();
        assert_eq!(len, frame.len() - HEADER);
        let back: String = decode(&frame[HEADER..]).unwrap();
        assert_eq!(back, "hello");
    }

    #[test]
    fn the_limit_is_enforced_on_the_header_alone() {
        let header = (MAX_BODY as u32 + 1).to_be_bytes();
        assert!(matches!(
            body_len(header),
            Err(FrameError::TooLarge { .. })
        ));
        // The largest allowed length is allowed.
        assert_eq!(body_len((MAX_BODY as u32).to_be_bytes()).unwrap(), MAX_BODY);
    }

    #[test]
    fn a_zero_length_frame_is_refused() {
        assert!(matches!(body_len([0; HEADER]), Err(FrameError::Empty)));
    }

    #[test]
    fn rubbish_bodies_are_errors_and_not_panics() {
        for body in [
            b"".as_slice(),
            b"\xff\xff\xff\xff",
            b"not cbor at all",
            &[0x9f; 64],
        ] {
            let _: Result<String, _> = decode(body);
        }
    }

    #[test]
    fn blocking_read_and_write_agree() {
        let mut buffer = Vec::new();
        write_blocking(&mut buffer, &vec![1u32, 2, 3]).unwrap();
        let back: Vec<u32> = read_blocking(&mut buffer.as_slice()).unwrap();
        assert_eq!(back, [1, 2, 3]);
    }

    #[test]
    fn a_truncated_frame_is_an_error() {
        let frame = encode(&"hello".to_string()).unwrap();
        let truncated = &frame[..frame.len() - 1];
        let result: Result<String, _> = read_blocking(&mut { truncated });
        assert!(matches!(result, Err(FrameError::Io(_))));
    }
}
