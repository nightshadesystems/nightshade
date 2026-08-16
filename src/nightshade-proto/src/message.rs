//! Requests and responses.
//!
//! One definition, compiled into both sides, so a field the CLI sends and a
//! field configd reads cannot drift apart.
//!
//! # Growing this
//!
//! Variants are added as the operations behind them are built. An enum arm
//! configd answers with "not implemented" is worse than an absent one: the CLI
//! would offer a command that does nothing, and the wire would carry a promise
//! nothing keeps. CBOR is self-describing, so a request configd does not know
//! decodes to a clean error rather than to something else -- which is what
//! makes adding variants safe, and what will make an API frontend additive
//! rather than a protocol break.

use nightshade_schema::config::ConfigTree;
use nightshade_schema::diff::Change;
use nightshade_schema::path::Path;
use serde::{Deserialize, Serialize};

mod session_id;
pub use session_id::{SessionId, SessionIdError};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Request {
    /// Start a candidate config. The reply carries the id every later request
    /// in this session quotes.
    SessionOpen,

    /// Discard the candidate and forget the session.
    SessionClose { session: SessionId },

    /// `value` is `None` for a flag, and for creating a bare tag instance --
    /// `set interfaces ethernet eth0`. Which of those it is depends on the
    /// schema, and configd is the one holding it.
    Set {
        session: SessionId,
        path: Path,
        value: Option<String>,
    },

    /// `value` names one value of a multi-leaf to remove; `None` removes the
    /// node and everything below it.
    Delete {
        session: SessionId,
        path: Path,
        value: Option<String>,
    },

    ShowCandidate { session: SessionId, path: Path },

    ShowRunning { path: Path },

    ShowSaved { path: Path },

    /// The candidate against running.
    Compare { session: SessionId },

    /// Throw the candidate away and start again from running.
    Discard { session: SessionId },

    /// Validate, render, apply and promote the candidate.
    Commit {
        session: SessionId,
        /// Recorded against the revision. What the operator was doing, in
        /// their own words, which is the part no diff can reconstruct.
        comment: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Response {
    Ok,
    Session { id: SessionId },
    /// A config, or the part of one that was asked for. Empty when the path
    /// names nothing configured.
    Config { tree: ConfigTree },
    Changes { changes: Vec<Change> },
    /// A commit that took effect. `changes` is empty when the candidate
    /// already matched running, which is a success and not an error.
    Committed {
        generation: u64,
        changes: Vec<Change>,
    },
    Failed { kind: FailureKind, message: String },
}

/// Why a request failed, coarsely enough to be worth carrying.
///
/// The *text* is configd's and is shown verbatim: it is the only side that
/// knows the schema, the running config and who asked. A client that
/// paraphrases produces a second wording of every error.
///
/// Exit codes are not decided here. `ns` chooses one from the command and the
/// kind together, because the same kind means different things after `set` and
/// after `commit`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FailureKind {
    /// Malformed, or about a session that is not there.
    Request,
    /// The config was refused: a bad value, an unknown path, a constraint.
    Validation,
    /// Someone else moved first.
    Conflict,
    /// configd could not do it, and it is not the caller's fault.
    Internal,
}

impl Response {
    pub fn failed(kind: FailureKind, message: impl Into<String>) -> Self {
        Response::Failed {
            kind,
            message: message.into(),
        }
    }

    pub fn invalid(message: impl Into<String>) -> Self {
        Self::failed(FailureKind::Validation, message)
    }

    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::failed(FailureKind::Request, message)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame;

    fn round_trip(request: &Request) {
        let frame = frame::encode(request).unwrap();
        let back: Request = frame::decode(&frame[frame::HEADER..]).unwrap();
        assert_eq!(&back, request);
    }

    #[test]
    fn requests_survive_the_wire() {
        let session = SessionId::parse("0123456789abcdef").unwrap();
        round_trip(&Request::SessionOpen);
        round_trip(&Request::SessionClose {
            session: session.clone(),
        });
        round_trip(&Request::Set {
            session: session.clone(),
            path: Path::parse("interfaces ethernet eth0 address").unwrap(),
            value: Some("192.168.1.1/24".into()),
        });
        round_trip(&Request::Set {
            session: session.clone(),
            path: Path::parse("interfaces ethernet eth0 disable").unwrap(),
            value: None,
        });
        round_trip(&Request::Compare { session });
    }

    #[test]
    fn responses_survive_the_wire() {
        let mut tree = ConfigTree::new();
        tree.set(&Path::parse("system host-name").unwrap(), "fw").unwrap();

        for response in [
            Response::Ok,
            Response::Session {
                id: SessionId::parse("0123456789abcdef").unwrap(),
            },
            Response::Config { tree },
            Response::invalid("`system hostname` is not a configuration path"),
        ] {
            let frame = frame::encode(&response).unwrap();
            let back: Response = frame::decode(&frame[frame::HEADER..]).unwrap();
            assert_eq!(back, response);
        }
    }

    /// A request from a newer client must fail to decode rather than decode
    /// into a different request. This is the property that makes adding
    /// variants safe.
    #[test]
    fn an_unknown_variant_is_an_error_and_not_a_misreading() {
        #[derive(Serialize)]
        enum Future {
            #[allow(dead_code)]
            SessionOpen,
            Teleport { session: SessionId },
        }
        let frame = frame::encode(&Future::Teleport {
            session: SessionId::parse("0123456789abcdef").unwrap(),
        })
        .unwrap();
        let back: Result<Request, _> = frame::decode(&frame[frame::HEADER..]);
        assert!(back.is_err(), "an unknown request decoded as {back:?}");
    }
}
