//! Candidate configs, one per session.
//!
//! A session is an operator part way through an edit. It holds a candidate
//! config branched from running, and the generation of running it branched
//! from -- which is what lets a commit notice that somebody else moved first
//! rather than quietly overwriting them.
//!
//! # Backed to /run
//!
//! Each candidate is mirrored to `/run/nightshade/sessions/<id>.json` on every
//! change. configd restarting -- a crash, an upgrade, a `systemctl restart`
//! -- should not throw away edits an operator has been typing for ten minutes.
//! A reboot should, and does, because `/run` is a tmpfs: what survives a
//! reboot is `config.boot` and nothing else.
//!
//! The serialisation is serde's own shape and nobody outside configd reads it.
//! The format an operator sees is the curly one.

use std::collections::BTreeMap;

use nightshade_common::paths::Paths;
use nightshade_proto::SessionId;
use nightshade_schema::config::ConfigTree;
use serde::{Deserialize, Serialize};

use crate::peer::Actor;

#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error("{action} {path}: {source}")]
    Io {
        action: &'static str,
        path: String,
        #[source]
        source: std::io::Error,
    },

    #[error("reading {path}: {source}")]
    Malformed {
        path: String,
        #[source]
        source: serde_json::Error,
    },
}

fn io(action: &'static str, path: &std::path::Path) -> impl FnOnce(std::io::Error) -> SessionError {
    let path = path.display().to_string();
    move |source| SessionError::Io {
        action,
        path,
        source,
    }
}

/// One operator's work in progress.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Session {
    pub id: SessionId,
    /// Only this uid may use the session. A session id is a name, not a
    /// capability: two administrators logged in at once each get their own
    /// candidate, and neither can drive the other's.
    pub uid: u32,
    pub username: String,
    pub candidate: ConfigTree,
    /// The running-config generation this candidate started from.
    pub base_generation: u64,
}

impl Session {
    pub fn new(id: SessionId, actor: &Actor, running: ConfigTree, generation: u64) -> Self {
        Self {
            id,
            uid: actor.uid,
            username: actor.username.clone(),
            candidate: running,
            base_generation: generation,
        }
    }

    pub fn owned_by(&self, actor: &Actor) -> bool {
        self.uid == actor.uid
    }
}

/// The session files under `/run`.
pub struct Store {
    paths: Paths,
}

impl Store {
    pub fn new(paths: Paths) -> Self {
        Self { paths }
    }

    pub fn prepare(&self) -> Result<(), SessionError> {
        let dir = self.paths.sessions_dir();
        std::fs::create_dir_all(&dir).map_err(io("creating", &dir))?;
        Ok(())
    }

    /// Write a session out.
    ///
    /// Write-then-rename, so a configd killed mid-write leaves the previous
    /// candidate rather than half of the new one. A truncated JSON file is a
    /// session that will not load, which is an operator's edits gone for a
    /// reason that has nothing to do with them.
    pub fn save(&self, session: &Session) -> Result<(), SessionError> {
        let final_path = self.paths.session_file(session.id.as_str());
        let temp_path = final_path.with_extension("json.new");

        let encoded = serde_json::to_vec(session).expect("a session always serialises");
        std::fs::write(&temp_path, &encoded).map_err(io("writing", &temp_path))?;
        std::fs::rename(&temp_path, &final_path).map_err(io("renaming", &temp_path))?;
        Ok(())
    }

    pub fn forget(&self, id: &SessionId) -> Result<(), SessionError> {
        let path = self.paths.session_file(id.as_str());
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(io("removing", &path)(e)),
        }
    }

    /// Everything under the sessions directory, and whatever could not be read.
    ///
    /// Returns both rather than failing on the first bad file: one unreadable
    /// session should cost that operator their candidate, not cost everyone
    /// else theirs.
    pub fn load_all(&self) -> Result<(BTreeMap<SessionId, Session>, Vec<SessionError>), SessionError> {
        let dir = self.paths.sessions_dir();
        let mut sessions = BTreeMap::new();
        let mut problems = Vec::new();

        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok((sessions, problems)),
            Err(e) => return Err(io("reading", &dir)(e)),
        };

        for entry in entries {
            let path = entry.map_err(io("reading", &dir))?.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            match load_one(&path) {
                Ok(session) => {
                    sessions.insert(session.id.clone(), session);
                }
                Err(e) => problems.push(e),
            }
        }
        Ok((sessions, problems))
    }
}

fn load_one(path: &std::path::Path) -> Result<Session, SessionError> {
    let text = std::fs::read(path).map_err(io("reading", path))?;
    serde_json::from_slice(&text).map_err(|source| SessionError::Malformed {
        path: path.display().to_string(),
        source,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use nightshade_schema::path::Path;

    fn actor() -> Actor {
        Actor {
            uid: 1000,
            gid: 1000,
            pid: Some(42),
            username: "nightshade".into(),
        }
    }

    fn session(id: &str) -> Session {
        let mut candidate = ConfigTree::new();
        candidate
            .set(&Path::parse("system host-name").unwrap(), "fw")
            .unwrap();
        Session::new(SessionId::parse(id).unwrap(), &actor(), candidate, 7)
    }

    #[test]
    fn a_session_survives_a_save_and_a_load() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(Paths::under(dir.path()));
        store.prepare().unwrap();

        let original = session("0123456789abcdef");
        store.save(&original).unwrap();

        let (loaded, problems) = store.load_all().unwrap();
        assert!(problems.is_empty());
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[&original.id], original);
    }

    #[test]
    fn forgetting_a_session_removes_its_file_and_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(Paths::under(dir.path()));
        store.prepare().unwrap();

        let one = session("0123456789abcdef");
        store.save(&one).unwrap();
        store.forget(&one.id).unwrap();
        store.forget(&one.id).unwrap();

        assert!(store.load_all().unwrap().0.is_empty());
    }

    /// One corrupt file must not take the others with it.
    #[test]
    fn a_damaged_session_is_reported_and_the_rest_still_load() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::under(dir.path());
        let store = Store::new(paths.clone());
        store.prepare().unwrap();

        let good = session("0123456789abcdef");
        store.save(&good).unwrap();
        std::fs::write(paths.session_file("aaaaaaaaaaaaaaaa"), b"{ truncated").unwrap();

        let (loaded, problems) = store.load_all().unwrap();
        assert_eq!(loaded.len(), 1);
        assert!(loaded.contains_key(&good.id));
        assert_eq!(problems.len(), 1);
        assert!(matches!(problems[0], SessionError::Malformed { .. }));
    }

    #[test]
    fn a_session_belongs_to_the_uid_that_opened_it() {
        let one = session("0123456789abcdef");
        assert!(one.owned_by(&actor()));

        let other = Actor {
            uid: 1001,
            ..actor()
        };
        assert!(!one.owned_by(&other));
    }

    #[test]
    fn a_missing_sessions_directory_is_no_sessions_and_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(Paths::under(dir.path()));
        let (loaded, problems) = store.load_all().unwrap();
        assert!(loaded.is_empty() && problems.is_empty());
    }
}
