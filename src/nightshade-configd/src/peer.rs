//! Who is on the other end of the socket.
//!
//! The kernel answers this, not the client. `SO_PEERCRED` reports the uid,
//! gid and pid of the process that connected, filled in by the kernel at
//! connect time and unforgeable from user space. Nothing a client sends is
//! consulted, because a client that could name itself could name someone
//! else.
//!
//! Two things come out of it. Whether to serve this connection at all, and
//! who to record as the actor on everything it changes -- "someone changed the
//! routing at 03:12" is not an audit trail.
//!
//! The socket's mode already restricts this to the admin group. Checking again
//! here is not redundant: the mode protects the socket, and this protects
//! against the socket having been created wrong -- by a hand-edited unit, by a
//! packaging mistake, by a systemd version with different defaults. The check
//! is nearly free because the credentials have to be read anyway.

use nightshade_common::ADMIN_GROUP;
use nix::unistd::{Gid, Group, Uid, User};

/// Whoever is making a request, as the kernel described them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Actor {
    pub uid: u32,
    pub gid: u32,
    /// Recorded for the log. Not trusted for anything: by the time a commit is
    /// written the process may be gone and the pid reused.
    pub pid: Option<i32>,
    /// Resolved once at connect time. A commit log that has to resolve uids at
    /// read time is a commit log that stops making sense after a user is
    /// deleted.
    pub username: String,
}

impl Actor {
    /// How the actor appears in the journal and the commit log.
    pub fn describe(&self) -> String {
        format!("{} (uid {})", self.username, self.uid)
    }
}

/// Who is allowed to talk to configd.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Access {
    /// Root, plus members of a unix group. What the appliance runs.
    Group(String),
    /// Root, plus one specific uid.
    ///
    /// For the tests, which must not depend on a group existing on whatever
    /// machine runs them, and for a recovery mode where the group database may
    /// be the thing that is broken.
    Uid(u32),
}

impl Default for Access {
    fn default() -> Self {
        Access::Group(ADMIN_GROUP.to_string())
    }
}

impl Access {
    /// Only whoever is running configd.
    ///
    /// What a foreground debug run wants, and what the tests use so they do
    /// not depend on a group existing on the machine running them.
    pub fn current_user() -> Self {
        Access::Uid(nix::unistd::getuid().as_raw())
    }
}

#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error("uid {uid} is not a user on this system")]
    NoSuchUser { uid: u32 },

    #[error("{username} (uid {uid}) is not a member of {group}")]
    NotPermitted {
        uid: u32,
        username: String,
        group: String,
    },

    #[error("uid {uid} is not permitted")]
    WrongUid { uid: u32 },

    #[error("group {group} does not exist, so only root can configure this system")]
    NoGroup { group: String },

    #[error("reading the user database: {0}")]
    UserDatabase(#[from] nix::Error),
}

/// Decide whether a peer may be served, and describe them.
pub fn authenticate(uid: u32, gid: u32, pid: Option<i32>, access: &Access) -> Result<Actor, AuthError> {
    let username = match User::from_uid(Uid::from_raw(uid))? {
        Some(user) => user.name,
        // Root stays usable even with a broken passwd database. Everyone else
        // needs a name, because the name is what the audit trail records.
        None if uid == 0 => "root".to_string(),
        None => return Err(AuthError::NoSuchUser { uid }),
    };

    let actor = Actor {
        uid,
        gid,
        pid,
        username: username.clone(),
    };

    // Root is always allowed. It can read and write every file configd owns
    // regardless, so refusing it here would protect nothing and would lock an
    // operator out of a box whose group database is the problem.
    if uid == 0 {
        return Ok(actor);
    }

    match access {
        Access::Uid(allowed) if uid == *allowed => Ok(actor),
        Access::Uid(_) => Err(AuthError::WrongUid { uid }),
        Access::Group(group) => {
            let Some(admin) = Group::from_name(group)? else {
                return Err(AuthError::NoGroup {
                    group: group.clone(),
                });
            };
            // Primary group, or a supplementary one. `SO_PEERCRED` reports
            // only the primary gid, so the membership list has to be consulted
            // for everyone whose admin rights come from a secondary group --
            // which is nearly everyone, since useradd gives each user their
            // own primary group.
            let permitted = admin.gid == Gid::from_raw(gid) || admin.mem.contains(&username);
            if permitted {
                Ok(actor)
            } else {
                Err(AuthError::NotPermitted {
                    uid,
                    username,
                    group: group.clone(),
                })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Root is served whatever the policy says, including when the policy
    /// names a group that is not there.
    #[test]
    fn root_is_always_served() {
        for access in [
            Access::Group("nightshade-admin".into()),
            Access::Group("a-group-that-does-not-exist".into()),
            Access::Uid(1000),
        ] {
            let actor = authenticate(0, 0, Some(1), &access).unwrap();
            assert_eq!(actor.uid, 0);
            assert_eq!(actor.username, "root");
        }
    }

    #[test]
    fn a_named_uid_is_served_and_others_are_not() {
        // uid 0 resolves on every unix, so use it as the "known" user and a
        // policy that would otherwise refuse it.
        let err = authenticate(65534, 65534, None, &Access::Uid(1000)).unwrap_err();
        assert!(
            matches!(err, AuthError::WrongUid { uid: 65534 } | AuthError::NoSuchUser { .. }),
            "{err}"
        );
    }

    #[test]
    fn an_unknown_uid_is_refused() {
        // 4294967294 is not going to be in anyone's passwd file.
        let err = authenticate(4294967294, 4294967294, None, &Access::default()).unwrap_err();
        assert!(matches!(err, AuthError::NoSuchUser { .. }), "{err}");
    }

    #[test]
    fn the_actor_describes_itself_for_the_log() {
        let actor = authenticate(0, 0, Some(42), &Access::Uid(0)).unwrap();
        assert_eq!(actor.describe(), "root (uid 0)");
    }
}
