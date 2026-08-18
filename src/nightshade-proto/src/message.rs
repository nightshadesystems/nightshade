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
        /// Apply, but roll back automatically unless confirmed within this
        /// many minutes.
        confirm_minutes: Option<u16>,
    },

    /// Keep a change that is waiting on confirmation.
    Confirm { session: SessionId },

    /// Write the running configuration to `config.boot`.
    ///
    /// No session: this saves what is applied, not what somebody is editing.
    Save,

    /// Replace the candidate with a configuration from somewhere else.
    Load {
        session: SessionId,
        source: LoadSource,
    },

    /// The archive, newest first.
    CommitLog,

    /// Live state, rather than configuration.
    OpShow { target: OpTarget },

    /// Ask to drop to a shell, and have it recorded.
    ///
    /// The CLI asks rather than deciding, so that restricting shell access to
    /// some administrators later is a change here and not a change to a
    /// program running as the operator. And configd knows the uid from
    /// `SO_PEERCRED`, which a client cannot lie about -- an audit line the
    /// audited process wrote is worth very little.
    ShellSession { entering: bool },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum OpTarget {
    Version,
    Interfaces,
    Interface { name: String },
}

/// What a live-state request came back with.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Report {
    Version { version: String },
    Interfaces { interfaces: Vec<InterfaceStatus> },
}

/// One interface, as the kernel and the configuration together describe it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InterfaceStatus {
    pub name: String,
    /// The configured type, or `unconfigured` for a device the kernel has and
    /// the configuration does not mention.
    pub kind: String,
    /// `up`, `down`, `unknown` -- from the kernel, not from the config.
    pub state: String,
    pub mac: Option<String>,
    pub mtu: Option<u32>,
    /// Addresses from the running configuration.
    pub addresses: Vec<String>,
    pub description: Option<String>,
    /// Whether the kernel has this device at all. A configured interface that
    /// is missing is the single most useful thing this command can show.
    pub present: bool,
}

/// Where a `Load` gets its configuration.
///
/// Deliberately not a path. configd runs as root, and a client-supplied
/// filename would be an arbitrary-read primitive dressed as a convenience --
/// the error message from failing to parse `/etc/shadow` would contain a line
/// of it. Both real sources are named rather than described.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum LoadSource {
    /// `/etc/nightshade/config.boot`, including whatever an operator edited
    /// into it by hand.
    Saved,
    /// An archived revision. This is what `rollback` is.
    Archive { revision: u64 },
}

/// One entry in the commit archive.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RevisionInfo {
    pub revision: u64,
    /// `YYYYMMDDTHHMMSSZ`.
    pub timestamp: String,
    /// Resolved when the commit happened. A log that resolves uids when it is
    /// read stops making sense the moment a user is deleted.
    pub actor: String,
    pub actor_uid: u32,
    pub comment: Option<String>,
    pub changes: Vec<Change>,
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
        /// Seconds left to confirm, when the commit was made with
        /// `confirm`. The change is applied either way; this is how long
        /// before it is undone again.
        confirm_within: Option<u64>,
    },
    Revisions {
        revisions: Vec<RevisionInfo>,
    },
    Operational {
        report: Report,
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
            Response::invalid("`system host-name` is not a configuration path"),
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
