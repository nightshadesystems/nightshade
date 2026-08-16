//! The daemon's state, and what it does with a request.
//!
//! Everything a client can ask for goes through [`Configd::handle`], which is
//! deliberately free of I/O on the socket: it takes a decoded request and an
//! authenticated actor and returns a response. That makes the whole surface
//! testable without a socket, and leaves the transport with nothing to decide.
//!
//! # Generations
//!
//! `running` carries a counter that moves every time it changes. A session
//! records the generation it branched from, and a commit compares the two.
//! That is how "the config changed since your session started" gets noticed
//! instead of one operator's candidate silently reverting another's work.
//!
//! Nothing changes `running` yet -- commit arrives with the pipeline -- but
//! the counter is what the check is built on, so sessions record it from the
//! start rather than acquiring it later and being wrong about every session
//! opened before then.

use std::collections::BTreeMap;
use std::io::Read;

use nightshade_common::paths::Paths;
use nightshade_proto::message::{FailureKind, Request, Response, SessionId};
use nightshade_schema::config::ConfigTree;
use nightshade_schema::diff;
use nightshade_schema::model::Schema;
use nightshade_schema::path::Path;
use tokio::sync::Mutex;
use tracing::{info, warn};

use crate::peer::Actor;
use crate::session::{Session, Store};

pub struct Configd {
    schema: &'static Schema,
    paths: Paths,
    store: Store,
    state: Mutex<State>,
}

struct State {
    running: ConfigTree,
    generation: u64,
    sessions: BTreeMap<SessionId, Session>,
}

impl Configd {
    /// Build the daemon, recovering any sessions left by a previous run.
    pub fn start(schema: &'static Schema, paths: Paths) -> Result<Self, crate::session::SessionError> {
        let store = Store::new(paths.clone());
        store.prepare()?;

        let (sessions, problems) = store.load_all()?;
        for problem in &problems {
            // Loud, because it is somebody's unsaved work going missing and
            // they are about to wonder why.
            warn!(error = %problem, "discarding an unreadable session");
        }
        if !sessions.is_empty() {
            info!(count = sessions.len(), "recovered candidate sessions");
        }

        Ok(Self {
            schema,
            paths,
            store,
            state: Mutex::new(State {
                running: ConfigTree::new(),
                generation: 0,
                sessions,
            }),
        })
    }

    pub fn paths(&self) -> &Paths {
        &self.paths
    }

    pub async fn session_count(&self) -> usize {
        self.state.lock().await.sessions.len()
    }

    pub async fn handle(&self, request: Request, actor: &Actor) -> Response {
        match request {
            Request::SessionOpen => self.open(actor).await,
            Request::SessionClose { session } => self.close(&session, actor).await,
            Request::Set {
                session,
                path,
                value,
            } => self.set(&session, actor, &path, value.as_deref()).await,
            Request::Delete {
                session,
                path,
                value,
            } => self.delete(&session, actor, &path, value.as_deref()).await,
            Request::ShowCandidate { session, path } => self.show_candidate(&session, actor, &path).await,
            Request::ShowRunning { path } => self.show_running(&path).await,
            Request::ShowSaved { path } => self.show_saved(&path),
            Request::Compare { session } => self.compare(&session, actor).await,
            Request::Discard { session } => self.discard(&session, actor).await,
        }
    }

    // -- sessions -----------------------------------------------------------

    async fn open(&self, actor: &Actor) -> Response {
        let bytes = match random_id_bytes() {
            Ok(bytes) => bytes,
            Err(e) => return internal("generating a session id", e),
        };
        let id = SessionId::from_bytes(bytes);

        let mut state = self.state.lock().await;
        let session = Session::new(id.clone(), actor, state.running.clone(), state.generation);
        if let Err(e) = self.store.save(&session) {
            return internal("saving the session", e);
        }
        state.sessions.insert(id.clone(), session);

        info!(session = %id, actor = %actor.describe(), "session opened");
        Response::Session { id }
    }

    async fn close(&self, id: &SessionId, actor: &Actor) -> Response {
        let mut state = self.state.lock().await;
        if let Err(response) = owned(&state, id, actor) {
            return response;
        }
        state.sessions.remove(id);
        if let Err(e) = self.store.forget(id) {
            // The session is gone from memory either way; a leftover file will
            // be reloaded on restart, so this is worth saying out loud.
            warn!(session = %id, error = %e, "could not remove the session file");
        }
        info!(session = %id, actor = %actor.describe(), "session closed");
        Response::Ok
    }

    async fn discard(&self, id: &SessionId, actor: &Actor) -> Response {
        let mut state = self.state.lock().await;
        if let Err(response) = owned(&state, id, actor) {
            return response;
        }
        let State {
            running,
            generation,
            sessions,
        } = &mut *state;
        let session = sessions.get_mut(id).expect("checked");
        session.candidate = running.clone();
        session.base_generation = *generation;

        if let Err(e) = self.store.save(session) {
            return internal("saving the session", e);
        }
        info!(session = %id, actor = %actor.describe(), "candidate discarded");
        Response::Ok
    }

    // -- editing ------------------------------------------------------------

    async fn set(
        &self,
        id: &SessionId,
        actor: &Actor,
        path: &Path,
        value: Option<&str>,
    ) -> Response {
        let mut state = self.state.lock().await;
        if let Err(response) = owned(&state, id, actor) {
            return response;
        }
        let session = state.sessions.get_mut(id).expect("checked");

        // Applied to a copy. A `set` that the schema accepts but the tree
        // refuses must leave the candidate as it was, not half-edited.
        let mut candidate = session.candidate.clone();
        if let Err(e) = self.schema.apply_set(&mut candidate, path, value) {
            return Response::invalid(e.to_string());
        }
        session.candidate = candidate;

        if let Err(e) = self.store.save(session) {
            return internal("saving the session", e);
        }
        info!(
            session = %id,
            actor = %actor.describe(),
            path = %path,
            value = value.unwrap_or(""),
            "set"
        );
        Response::Ok
    }

    async fn delete(
        &self,
        id: &SessionId,
        actor: &Actor,
        path: &Path,
        value: Option<&str>,
    ) -> Response {
        let mut state = self.state.lock().await;
        if let Err(response) = owned(&state, id, actor) {
            return response;
        }
        let session = state.sessions.get_mut(id).expect("checked");

        let mut candidate = session.candidate.clone();
        if let Err(e) = self.schema.apply_delete(&mut candidate, path, value) {
            return Response::invalid(e.to_string());
        }
        session.candidate = candidate;

        if let Err(e) = self.store.save(session) {
            return internal("saving the session", e);
        }
        info!(
            session = %id,
            actor = %actor.describe(),
            path = %path,
            value = value.unwrap_or(""),
            "delete"
        );
        Response::Ok
    }

    // -- reading ------------------------------------------------------------

    async fn show_candidate(&self, id: &SessionId, actor: &Actor, path: &Path) -> Response {
        let state = self.state.lock().await;
        if let Err(response) = owned(&state, id, actor) {
            return response;
        }
        let session = state.sessions.get(id).expect("checked");
        Response::Config {
            tree: session.candidate.subtree(path).unwrap_or_default(),
        }
    }

    async fn show_running(&self, path: &Path) -> Response {
        let state = self.state.lock().await;
        Response::Config {
            tree: state.running.subtree(path).unwrap_or_default(),
        }
    }

    /// `config.boot`, read from disk on every request rather than cached.
    ///
    /// It is the one config an operator is invited to edit by hand, so a cache
    /// would show them the file as it was before they opened their editor.
    fn show_saved(&self, path: &Path) -> Response {
        let file = self.paths.config_boot();
        let text = match std::fs::read_to_string(&file) {
            Ok(text) => text,
            // A box that has never been saved has no saved config. That is a
            // fact about it, not a failure.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(e) => return internal(format!("reading {}", file.display()), e),
        };
        match nightshade_schema::curly::parse(&text) {
            Ok(tree) => Response::Config {
                tree: tree.subtree(path).unwrap_or_default(),
            },
            Err(e) => Response::failed(
                FailureKind::Validation,
                format!("{} does not parse: {e}", file.display()),
            ),
        }
    }

    async fn compare(&self, id: &SessionId, actor: &Actor) -> Response {
        let state = self.state.lock().await;
        if let Err(response) = owned(&state, id, actor) {
            return response;
        }
        let session = state.sessions.get(id).expect("checked");
        Response::Changes {
            changes: diff::diff(&state.running, &session.candidate),
        }
    }
}

/// Check a session exists and belongs to this actor.
///
/// Says which it is. These are all administrators on one appliance, so hiding
/// whether a session exists protects nothing and costs an operator with two
/// terminals open the one piece of information that explains what happened.
fn owned(state: &State, id: &SessionId, actor: &Actor) -> Result<(), Response> {
    match state.sessions.get(id) {
        Some(session) if session.owned_by(actor) => Ok(()),
        Some(session) => Err(Response::failed(
            FailureKind::Request,
            format!(
                "session {id} belongs to {}; open your own with `configure`",
                session.username
            ),
        )),
        None => Err(Response::failed(
            FailureKind::Request,
            format!("session {id} has expired or was never opened"),
        )),
    }
}

fn internal(context: impl std::fmt::Display, error: impl std::fmt::Display) -> Response {
    // Logged as well as returned: the client sees why their request failed,
    // and the journal keeps it for whoever reads it afterwards.
    warn!(%context, %error, "request failed");
    Response::failed(FailureKind::Internal, format!("{context}: {error}"))
}

/// Session id entropy, straight from the kernel.
///
/// `/dev/urandom` rather than a crate: it is the same source anything else
/// would use, it is always present on the appliance, and sessions are opened
/// rarely enough that the open costs nothing worth measuring.
fn random_id_bytes() -> std::io::Result<[u8; SessionId::ENTROPY_BYTES]> {
    let mut bytes = [0u8; SessionId::ENTROPY_BYTES];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(bytes)
}
