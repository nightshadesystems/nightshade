//! The wire between `ns` and configd.
//!
//! One definition of every request and response, compiled into both sides, so
//! a field the CLI sends and a field the daemon reads cannot drift apart.
//!
//! # Why not gRPC, or HTTP, or anything with a schema compiler
//!
//! The peer on the other end of this socket is root. Every byte of parser
//! reachable before authentication is attack surface, and an HTTP stack is a
//! great deal of parser: header folding, chunked encoding, compression,
//! multiplexing, TLS. None of that buys anything on a unix socket where the
//! kernel already tells us who is calling. What is here instead is a
//! length-prefixed frame and a CBOR decode, and the frame cap is checked
//! before a single byte is buffered.
//!
//! # Shape
//!
//! - length-prefixed frames, hard cap 4 MiB, enforced on the length itself
//!   rather than after reading
//! - CBOR body via `ciborium` -- self-describing, so an unknown variant is a
//!   clean error rather than a misread one
//! - per-connection read and idle timeouts, applied by configd
//!
//! # Room for the API frontend
//!
//! A later phase puts an HTTP API in front of the same operations. That should
//! be a second frontend translating into these types, never a second set of
//! operations. So requests carry everything an operation needs and nothing
//! about how it was typed: no terminal width, no colour, no partial paths.
//! Presentation is the CLI's problem, and it is the CLI's alone.
//!
//! Nothing in this phase builds that frontend.

pub mod frame;
pub mod message;

pub use frame::{FrameError, MAX_BODY};
pub use message::{
    FailureKind, LoadSource, OpTarget, Report, Request, Response, RevisionInfo,
    SessionId,
};
