//! Commit-confirm.
//!
//! `commit confirm 5` is an operator saying "I am about to change the routing
//! on the box I am reachable through". It only means anything if the rollback
//! happens when they lose the session -- which is the case it exists for.
//!
//! So the timer is not in the CLI, and it is not a detached process. It is a
//! task inside configd plus a marker file recording the configuration to go
//! back to and the deadline to go back at. Three failure modes, all covered:
//!
//! - **the CLI dies** -- the timer is in configd and knows nothing about it
//! - **configd restarts** -- the marker is read on startup and the timer is
//!   armed again for whatever is left of the window
//! - **configd is down when the deadline passes** -- the marker is read on
//!   startup, the deadline is in the past, and the rollback happens at once
//!
//! # The deadline is wall-clock
//!
//! Stored as a unix timestamp rather than a duration, because a duration
//! cannot survive a restart -- there would be nothing to measure it from. The
//! cost is that a large backward jump in the system clock extends the window.
//! That is the right trade for an appliance: the alternative loses the
//! rollback entirely on the one failure it is there for.
//!
//! # What is applied during the window
//!
//! The new configuration, and `running` says so. What is pending is not
//! whether the change took effect -- it did -- but whether it stays. The
//! archive is only written on confirmation, so a revision that was rolled back
//! was never a revision.

use nightshade_common::paths::Paths;
use nightshade_proto::message::SessionId;
use nightshade_schema::config::ConfigTree;
use serde::{Deserialize, Serialize};

#[derive(Debug, thiserror::Error)]
pub enum ConfirmError {
    #[error("{action} {path}: {source}")]
    Io {
        action: &'static str,
        path: String,
        #[source]
        source: std::io::Error,
    },

    #[error("the pending-confirm marker is unreadable: {0}")]
    Malformed(String),
}

/// A commit that has been applied and is waiting to be kept.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pending {
    /// Unix seconds. Past this, the rollback happens.
    pub deadline: i64,
    pub minutes: u16,
    /// Who committed, so the rollback can say whose change it undid.
    pub session: SessionId,
    pub actor_uid: u32,
    pub actor: String,
    pub comment: Option<String>,
    /// The configuration to go back to.
    pub previous: ConfigTree,
    /// The generation `previous` was, so a rollback restores the numbering
    /// too rather than inventing one.
    pub previous_generation: u64,
    /// What was committed, kept so confirmation can archive it without the
    /// session still being around.
    pub committed: ConfigTree,
    pub generation: u64,
}

impl Pending {
    /// Seconds left, floored at zero.
    pub fn remaining(&self) -> u64 {
        (self.deadline - now()).max(0) as u64
    }

    pub fn expired(&self) -> bool {
        self.remaining() == 0
    }
}

pub fn now() -> i64 {
    jiff::Timestamp::now().as_second()
}

pub fn deadline_in(minutes: u16) -> i64 {
    now() + i64::from(minutes) * 60
}

/// The marker file.
pub struct Marker {
    paths: Paths,
}

impl Marker {
    pub fn new(paths: Paths) -> Self {
        Self { paths }
    }

    /// Write the marker before the timer is armed.
    ///
    /// Order matters. A configd that dies between applying and writing this
    /// leaves a box running an unconfirmed change with nothing to roll it
    /// back -- so the marker goes down first, and a marker with no timer is
    /// recovered on the next startup.
    pub fn arm(&self, pending: &Pending) -> Result<(), ConfirmError> {
        let path = self.paths.pending_confirm();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|source| ConfirmError::Io {
                action: "creating",
                path: parent.display().to_string(),
                source,
            })?;
        }
        let temporary = path.with_extension("json.new");
        let encoded = serde_json::to_vec(pending).expect("a pending confirm always serialises");
        std::fs::write(&temporary, encoded).map_err(|source| ConfirmError::Io {
            action: "writing",
            path: temporary.display().to_string(),
            source,
        })?;
        std::fs::rename(&temporary, &path).map_err(|source| ConfirmError::Io {
            action: "renaming",
            path: path.display().to_string(),
            source,
        })
    }

    pub fn disarm(&self) -> Result<(), ConfirmError> {
        let path = self.paths.pending_confirm();
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(source) => Err(ConfirmError::Io {
                action: "removing",
                path: path.display().to_string(),
                source,
            }),
        }
    }

    pub fn read(&self) -> Result<Option<Pending>, ConfirmError> {
        let path = self.paths.pending_confirm();
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(source) => {
                return Err(ConfirmError::Io {
                    action: "reading",
                    path: path.display().to_string(),
                    source,
                });
            }
        };
        serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|e| ConfirmError::Malformed(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nightshade_schema::path::Path;

    fn pending(deadline: i64) -> Pending {
        let mut previous = ConfigTree::new();
        previous
            .set(&Path::parse("system host-name").unwrap(), "before")
            .unwrap();
        let mut committed = ConfigTree::new();
        committed
            .set(&Path::parse("system host-name").unwrap(), "after")
            .unwrap();

        Pending {
            deadline,
            minutes: 5,
            session: SessionId::parse("0123456789abcdef").unwrap(),
            actor_uid: 1000,
            actor: "nightshade".into(),
            comment: Some("routing change".into()),
            previous,
            previous_generation: 3,
            committed,
            generation: 4,
        }
    }

    #[test]
    fn a_marker_survives_a_write_and_a_read() {
        let dir = tempfile::tempdir().unwrap();
        let marker = Marker::new(Paths::under(dir.path()));

        assert_eq!(marker.read().unwrap(), None);

        let armed = pending(deadline_in(5));
        marker.arm(&armed).unwrap();
        assert_eq!(marker.read().unwrap(), Some(armed));
    }

    #[test]
    fn disarming_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let marker = Marker::new(Paths::under(dir.path()));
        marker.arm(&pending(deadline_in(5))).unwrap();
        marker.disarm().unwrap();
        marker.disarm().unwrap();
        assert_eq!(marker.read().unwrap(), None);
    }

    #[test]
    fn a_deadline_in_the_past_has_already_expired() {
        let past = pending(now() - 1);
        assert!(past.expired());
        assert_eq!(past.remaining(), 0);

        let future = pending(deadline_in(5));
        assert!(!future.expired());
        assert!(future.remaining() > 290 && future.remaining() <= 300);
    }

    /// The marker is what makes the whole thing work across a restart, so a
    /// damaged one has to be a loud error rather than a silent absence --
    /// silently absent means a box that quietly keeps an unconfirmed change.
    #[test]
    fn a_damaged_marker_is_an_error_and_not_an_absence() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::under(dir.path());
        std::fs::create_dir_all(paths.run_dir()).unwrap();
        std::fs::write(paths.pending_confirm(), b"{ truncated").unwrap();

        let marker = Marker::new(paths);
        assert!(matches!(marker.read(), Err(ConfirmError::Malformed(_))));
    }
}
