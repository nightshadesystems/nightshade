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

use std::collections::{BTreeMap, VecDeque};
use std::io::Read;
use std::sync::Arc;

use nightshade_common::paths::Paths;
use nightshade_proto::message::{
    FailureKind, LoadSource, OpTarget, Report, Request, Response, RevisionInfo, SessionId,
};
use nightshade_render::{Host, Renderer};
use nightshade_schema::config::ConfigTree;
use nightshade_schema::diff;
use nightshade_schema::model::Schema;
use nightshade_schema::path::Path;
use tokio::sync::Mutex;
use tracing::{error, info, warn};

use crate::archive::{self, Archive};
use crate::netif;
use crate::confirm::{self, Marker, Pending};
use crate::commit;
use crate::peer::Actor;
use crate::session::{Session, Store};

/// Running configs kept for the sake of telling a session that has fallen
/// behind exactly what moved underneath it.
///
/// Small on purpose. This is for the operator staring at a refused commit, not
/// a history -- the archive is the history.
const HISTORY: usize = 8;

pub struct Configd {
    schema: &'static Schema,
    paths: Paths,
    store: Store,
    archive: Archive,
    marker: Marker,
    renderers: Vec<Box<dyn Renderer>>,
    state: Mutex<State>,
}

struct State {
    running: ConfigTree,
    generation: u64,
    history: VecDeque<(u64, ConfigTree)>,
    sessions: BTreeMap<SessionId, Session>,
    /// A commit that is applied and waiting to be kept.
    ///
    /// At most one. It is a lock on the whole configuration: a second commit
    /// while one is pending would leave nothing coherent to roll back to.
    pending: Option<Pending>,
    /// The task that will roll `pending` back. Aborted on confirmation.
    timer: Option<tokio::task::JoinHandle<()>>,
}

impl Configd {
    /// Build the daemon, recovering any sessions left by a previous run.
    pub fn start(
        schema: &'static Schema,
        paths: Paths,
        host: Arc<dyn Host>,
    ) -> Result<Self, crate::session::SessionError> {
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

        // Running state lives under /run, so it survives a configd restart and
        // not a reboot. That is exactly right: what is running is what was
        // applied since boot, and a box that came up believing it had applied
        // something it had not is a box nobody can reason about.
        let (running, generation) = load_running(&paths);

        Ok(Self {
            schema,
            store,
            archive: Archive::new(paths.archive_dir()),
            marker: Marker::new(paths.clone()),
            renderers: nightshade_render::all(paths.clone(), host),
            paths,
            state: Mutex::new(State {
                running,
                generation,
                history: VecDeque::new(),
                sessions,
                pending: None,
                timer: None,
            }),
        })
    }

    /// Pick up a commit-confirm that was in flight when configd stopped.
    ///
    /// Separate from `start` because arming a timer needs an `Arc<Self>` and
    /// `start` has not produced one yet. Called once, immediately after.
    ///
    /// This is the half of commit-confirm that makes it worth having. A
    /// firewall whose config daemon was restarted -- by an upgrade, by a
    /// crash, by the change that was being confirmed -- must still roll back.
    pub async fn resume(self: &Arc<Self>) {
        let pending = match self.marker.read() {
            Ok(Some(pending)) => pending,
            Ok(None) => return,
            Err(e) => {
                // Cannot roll back to a configuration we cannot read. Say so
                // as loudly as possible: something is applied that nobody
                // confirmed, and now nothing will undo it.
                error!(
                    error = %e,
                    "a commit was awaiting confirmation and the marker is unreadable; \
                     the unconfirmed configuration will NOT be rolled back"
                );
                return;
            }
        };

        if pending.expired() {
            warn!(
                actor = %pending.actor,
                "a commit was awaiting confirmation and its deadline has passed; rolling back now"
            );
            let mut state = self.state.lock().await;
            state.pending = Some(pending);
            drop(state);
            self.roll_back().await;
            return;
        }

        let remaining = pending.remaining();
        warn!(
            actor = %pending.actor,
            seconds = remaining,
            "a commit is still awaiting confirmation; the timer has been armed again"
        );
        let mut state = self.state.lock().await;
        state.pending = Some(pending);
        state.timer = Some(self.arm_timer(remaining));
    }

    /// The task that undoes an unconfirmed commit.
    fn arm_timer(self: &Arc<Self>, seconds: u64) -> tokio::task::JoinHandle<()> {
        let configd = Arc::clone(self);
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_secs(seconds)).await;
            configd.roll_back().await;
        })
    }

    /// Undo the pending commit, through the same pipeline that applied it.
    ///
    /// The same pipeline, not a shortcut: a rollback is a configuration change
    /// like any other, and one that took a different route to the machine
    /// would be a second implementation to get wrong -- on the path that only
    /// runs when something has already gone wrong.
    async fn roll_back(self: &Arc<Self>) {
        let mut state = self.state.lock().await;
        let Some(pending) = state.pending.take() else {
            return;
        };
        state.timer = None;

        error!(
            actor = %pending.actor,
            minutes = pending.minutes,
            "commit was not confirmed; rolling back"
        );

        match commit::apply(&self.renderers, &pending.previous) {
            Ok(()) => {
                let generation = state.generation + 1;
                state.running = pending.previous;
                state.generation = generation;
                if let Err(e) = save_running(&self.paths, &state.running, generation) {
                    warn!(error = %e, "could not record the running configuration");
                }
                info!(generation, "rolled back");
            }
            Err(e) => {
                // The box is running an unconfirmed change and cannot be put
                // back. Nothing here can fix that; being unmistakable about it
                // is the whole remaining job.
                error!(
                    error = %e,
                    "ROLLING BACK AN UNCONFIRMED COMMIT FAILED; \
                     this system is running a configuration nobody confirmed"
                );
            }
        }

        if let Err(e) = self.marker.disarm() {
            warn!(error = %e, "could not remove the pending-confirm marker");
        }
    }

    pub fn paths(&self) -> &Paths {
        &self.paths
    }

    pub async fn session_count(&self) -> usize {
        self.state.lock().await.sessions.len()
    }

    pub async fn handle(self: &Arc<Self>, request: Request, actor: &Actor) -> Response {
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
            Request::Commit {
                session,
                comment,
                confirm_minutes,
            } => {
                self.commit(&session, actor, comment.as_deref(), confirm_minutes)
                    .await
            }
            Request::Confirm { session } => self.confirm(&session, actor).await,
            Request::Save => self.save().await,
            Request::Load { session, source } => self.load(&session, actor, source).await,
            Request::CommitLog => self.commit_log(),
            Request::OpShow { target } => self.op_show(target).await,
            Request::ShellSession { entering } => self.shell_session(entering, actor),
        }
    }

    // -- live state ---------------------------------------------------------

    async fn op_show(&self, target: OpTarget) -> Response {
        let report = match target {
            OpTarget::Version => Report::Version {
                version: nightshade_common::VERSION.to_string(),
            },
            OpTarget::Interfaces => {
                let state = self.state.lock().await;
                Report::Interfaces {
                    interfaces: netif::interfaces(&self.paths, &state.running),
                }
            }
            OpTarget::Interface { name } => {
                let state = self.state.lock().await;
                match netif::interface(&self.paths, &state.running, &name) {
                    Some(status) => Report::Interfaces {
                        interfaces: vec![status],
                    },
                    None => {
                        return Response::bad_request(format!(
                            "{name} is neither configured nor present on this system"
                        ));
                    }
                }
            }
        };
        Response::Operational { report }
    }

    /// Decide whether this operator may have a shell, and record that they
    /// asked.
    ///
    /// Recorded here rather than by the CLI because the uid comes from
    /// `SO_PEERCRED` and cannot be forged -- an audit line written by the
    /// process being audited is worth very little. Permission is decided here
    /// for the same reason, and so that restricting it later is a change to
    /// the daemon rather than to a program running as the operator.
    fn shell_session(&self, entering: bool, actor: &Actor) -> Response {
        // Everyone who reached this socket is already in the admin group.
        // Stated rather than assumed, so the day it narrows there is one line
        // to change.
        warn!(
            actor = %actor.describe(),
            uid = actor.uid,
            event = if entering { "shell-entered" } else { "shell-left" },
            "operator shell"
        );
        Response::Ok
    }

    // -- committing ---------------------------------------------------------

    async fn commit(
        self: &Arc<Self>,
        id: &SessionId,
        actor: &Actor,
        comment: Option<&str>,
        confirm_minutes: Option<u16>,
    ) -> Response {
        let mut state = self.state.lock().await;
        if let Err(response) = owned(&state, id, actor) {
            return response;
        }

        // One unconfirmed change at a time. A second commit on top of one
        // would leave the marker pointing at a configuration two changes back,
        // so a rollback would undo work nobody asked it to.
        if let Some(pending) = &state.pending {
            return Response::failed(
                FailureKind::Conflict,
                format!(
                    "{} committed a change {} minutes ago that is still waiting to be confirmed \
                     ({} seconds left).\nconfirm it, or wait for it to roll back.",
                    pending.actor,
                    pending.minutes,
                    pending.remaining()
                ),
            );
        }

        let session = state.sessions.get(id).expect("checked");
        let candidate = session.candidate.clone();
        let base = session.base_generation;

        // Somebody else committed while this session was being edited. Refused
        // rather than merged: the two operators disagree about what the box
        // looks like, and only they can settle it.
        if base != state.generation {
            return changed_underneath(&state, base, &candidate);
        }

        // Steps 1 and 2.
        if let Err(e) = commit::validate(self.schema, &candidate) {
            return Response::invalid(e.to_string());
        }

        // Steps 3 to 5.
        let changes = commit::order(self.schema, diff::diff(&state.running, &candidate));
        if changes.is_empty() {
            // Not a failure. The box is already how the operator wants it.
            return Response::Committed {
                generation: state.generation,
                changes,
                confirm_within: None,
            };
        }

        info!(
            session = %id,
            actor = %actor.describe(),
            changes = changes.len(),
            comment = comment.unwrap_or(""),
            "committing"
        );

        // Steps 6 to 9.
        if let Err(e) = commit::apply(&self.renderers, &candidate) {
            let kind = match e {
                commit::CommitError::Invalid(_) | commit::CommitError::Check { .. } => {
                    FailureKind::Validation
                }
                _ => FailureKind::Internal,
            };
            return Response::failed(kind, e.to_string());
        }

        // Step 10.
        let was = state.generation;
        let generation = was + 1;
        let previous = std::mem::replace(&mut state.running, candidate);
        state.history.push_back((was, previous));
        while state.history.len() > HISTORY {
            state.history.pop_front();
        }
        state.generation = generation;

        if let Err(e) = save_running(&self.paths, &state.running, generation) {
            // Applied and promoted in memory; only the record under /run is
            // missing. A restart would come up believing less had been applied
            // than has been, so this is worth shouting about.
            warn!(error = %e, "could not record the running configuration");
        }

        // The committing session is now level with running, so the operator
        // can carry straight on rather than having to reopen.
        if let Some(session) = state.sessions.get_mut(id) {
            session.base_generation = generation;
            if let Err(e) = self.store.save(session) {
                warn!(session = %id, error = %e, "could not save the session");
            }
        }

        // A confirmed commit is a revision; an unconfirmed one is not yet.
        // Archiving now would put a configuration into the history that is
        // about to be undone.
        let confirm_within = match confirm_minutes {
            None => {
                self.record(&state.running, actor, comment, generation, &changes);
                None
            }
            Some(minutes) => {
                let pending = Pending {
                    deadline: confirm::deadline_in(minutes),
                    minutes,
                    session: id.clone(),
                    actor_uid: actor.uid,
                    actor: actor.username.clone(),
                    comment: comment.map(str::to_string),
                    previous: state
                        .history
                        .back()
                        .map(|(_, config)| config.clone())
                        .unwrap_or_default(),
                    previous_generation: was,
                    committed: state.running.clone(),
                    generation,
                };
                let remaining = pending.remaining();

                // The marker before the timer. A configd that dies between
                // these two leaves a marker with no timer, which the next
                // startup recovers -- the other order leaves a timer with no
                // marker, which nothing recovers.
                if let Err(e) = self.marker.arm(&pending) {
                    error!(error = %e, "could not arm the confirm marker");
                    return Response::failed(
                        FailureKind::Internal,
                        format!(
                            "the change was applied, but the rollback timer could not be armed: {e}\n\
                             confirm or revert this by hand"
                        ),
                    );
                }
                state.pending = Some(pending);
                state.timer = Some(self.arm_timer(remaining));

                warn!(
                    session = %id,
                    actor = %actor.describe(),
                    minutes,
                    "committed, awaiting confirmation"
                );
                Some(remaining)
            }
        };

        info!(
            session = %id,
            actor = %actor.describe(),
            generation,
            "committed"
        );
        Response::Committed {
            generation,
            changes,
            confirm_within,
        }
    }

    async fn confirm(&self, id: &SessionId, actor: &Actor) -> Response {
        let mut state = self.state.lock().await;
        if let Err(response) = owned(&state, id, actor) {
            return response;
        }

        let Some(pending) = state.pending.take() else {
            return Response::bad_request(
                "nothing is waiting to be confirmed".to_string(),
            );
        };

        // Anyone in the admin group may confirm. The operator who committed
        // may be exactly the one who has just lost their session, and refusing
        // their colleague would turn a recoverable situation into an outage
        // with a countdown on it.
        if pending.actor_uid != actor.uid {
            info!(
                confirmed_by = %actor.describe(),
                committed_by = %pending.actor,
                "confirming somebody else's commit"
            );
        }

        if let Some(timer) = state.timer.take() {
            timer.abort();
        }
        if let Err(e) = self.marker.disarm() {
            warn!(error = %e, "could not remove the pending-confirm marker");
        }

        let changes = commit::order(
            self.schema,
            diff::diff(&pending.previous, &pending.committed),
        );
        self.record(
            &pending.committed,
            actor,
            pending.comment.as_deref(),
            pending.generation,
            &changes,
        );

        info!(
            actor = %actor.describe(),
            generation = pending.generation,
            "confirmed"
        );
        Response::Committed {
            generation: pending.generation,
            changes,
            confirm_within: None,
        }
    }

    /// Append a revision to the archive.
    ///
    /// Failing to archive does not fail the commit. The change is applied and
    /// working; losing the history entry is bad and is not worth undoing a
    /// good configuration for.
    fn record(
        &self,
        config: &ConfigTree,
        actor: &Actor,
        comment: Option<&str>,
        revision: u64,
        changes: &[nightshade_schema::diff::Change],
    ) {
        let info = RevisionInfo {
            revision,
            timestamp: archive::stamp(),
            actor: actor.username.clone(),
            actor_uid: actor.uid,
            comment: comment.map(str::to_string),
            changes: changes.to_vec(),
        };
        if let Err(e) = self.archive.write(&info, config, self.schema) {
            warn!(revision, error = %e, "could not archive this revision");
        }
    }

    // -- saved configuration and the archive --------------------------------

    async fn save(&self) -> Response {
        let state = self.state.lock().await;
        let path = self.paths.config_boot();

        if let Some(parent) = path.parent()
            && let Err(e) = std::fs::create_dir_all(parent)
        {
            return internal(format!("creating {}", parent.display()), e);
        }

        // Write and rename. A truncated `config.boot` is a box that boots to
        // defaults, and it would be truncated at exactly the moment somebody
        // was making sure their change survived a reboot.
        let text = nightshade_schema::curly::render(&state.running, self.schema);
        let temporary = path.with_extension("boot.new");
        if let Err(e) = std::fs::write(&temporary, &text) {
            return internal(format!("writing {}", temporary.display()), e);
        }
        if let Err(e) = std::fs::rename(&temporary, &path) {
            return internal(format!("renaming to {}", path.display()), e);
        }

        info!(path = %path.display(), bytes = text.len(), "saved");
        Response::Ok
    }

    async fn load(&self, id: &SessionId, actor: &Actor, source: LoadSource) -> Response {
        let mut state = self.state.lock().await;
        if let Err(response) = owned(&state, id, actor) {
            return response;
        }

        let (config, described) = match source {
            LoadSource::Saved => {
                let path = self.paths.config_boot();
                let text = match std::fs::read_to_string(&path) {
                    Ok(text) => text,
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                        return Response::bad_request(format!(
                            "{} does not exist; nothing has been saved on this system",
                            path.display()
                        ));
                    }
                    Err(e) => return internal(format!("reading {}", path.display()), e),
                };
                match nightshade_schema::curly::parse(&text) {
                    Ok(config) => (config, path.display().to_string()),
                    Err(e) => {
                        return Response::failed(
                            FailureKind::Validation,
                            format!("{}: {e}", path.display()),
                        );
                    }
                }
            }
            LoadSource::Archive { revision } => match self.archive.read(revision, self.schema) {
                Ok(config) => (config, format!("revision {revision}")),
                Err(e) => return Response::bad_request(e.to_string()),
            },
        };

        // Checked now rather than at commit. A hand-edited file wants its
        // mistakes pointed out while the operator still has the editor open,
        // and loading rubbish into the candidate only moves the complaint.
        let violations = self.schema.validate_tree(&config);
        if !violations.is_empty() {
            let mut message = format!("{described} is not a valid configuration:\n");
            for violation in &violations {
                message.push_str(&format!("  {violation}\n"));
            }
            message.push_str("\nthe candidate configuration has not been changed.");
            return Response::failed(FailureKind::Validation, message);
        }

        let session = state.sessions.get_mut(id).expect("checked");
        session.candidate = config;
        if let Err(e) = self.store.save(session) {
            return internal("saving the session", e);
        }

        info!(session = %id, actor = %actor.describe(), source = %described, "loaded");
        Response::Ok
    }

    fn commit_log(&self) -> Response {
        match self.archive.list() {
            Ok(revisions) => Response::Revisions { revisions },
            Err(e) => internal("reading the archive", e),
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
            ..
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

/// The commit was refused because running moved since the session started.
///
/// Says what moved. "The configuration has changed" on its own leaves an
/// operator no move but to discard their work and start again; the diff lets
/// them decide whether the two changes are compatible.
fn changed_underneath(state: &State, base: u64, candidate: &ConfigTree) -> Response {
    let mut message = format!(
        "the running configuration has changed since this session started \
         (it was revision {base}, it is now revision {}).\n",
        state.generation
    );

    match state.history.iter().find(|(generation, _)| *generation == base) {
        Some((_, was)) => {
            message.push_str("\nwhat changed underneath you:\n");
            for change in diff::diff(was, &state.running) {
                message.push_str(&format!("  {change}\n"));
            }
        }
        None => {
            // Fallen further behind than the history goes.
            message.push_str(
                "\nthat was too long ago to show what changed; \
                 `compare` against running to see where you stand.\n",
            );
        }
    }

    message.push_str("\nyour own uncommitted changes:\n");
    for change in diff::diff(&state.running, candidate) {
        message.push_str(&format!("  {change}\n"));
    }
    message.push_str("\nuse `discard` to start again from the current configuration.");

    Response::failed(FailureKind::Conflict, message)
}

/// Read the running config recorded under `/run`.
///
/// A missing file is a box that has not committed since boot, which is the
/// normal state of a freshly booted appliance and not an error. A damaged one
/// is treated the same way and said so loudly: the alternative is refusing to
/// start, and a config daemon that will not start is a box nobody can log into
/// and fix.
fn load_running(paths: &Paths) -> (ConfigTree, u64) {
    #[derive(serde::Serialize, serde::Deserialize)]
    struct Recorded {
        generation: u64,
        config: ConfigTree,
    }

    let path = paths.running();
    match std::fs::read(&path) {
        Ok(bytes) => match serde_json::from_slice::<Recorded>(&bytes) {
            Ok(recorded) => {
                info!(generation = recorded.generation, "recovered the running configuration");
                (recorded.config, recorded.generation)
            }
            Err(e) => {
                warn!(
                    path = %path.display(),
                    error = %e,
                    "the running configuration is unreadable; starting as though nothing is applied"
                );
                (ConfigTree::new(), 0)
            }
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (ConfigTree::new(), 0),
        Err(e) => {
            warn!(path = %path.display(), error = %e, "could not read the running configuration");
            (ConfigTree::new(), 0)
        }
    }
}

fn save_running(paths: &Paths, config: &ConfigTree, generation: u64) -> std::io::Result<()> {
    #[derive(serde::Serialize)]
    struct Recorded<'a> {
        generation: u64,
        config: &'a ConfigTree,
    }

    let path = paths.running();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let temporary = path.with_extension("json.new");
    let encoded = serde_json::to_vec(&Recorded { generation, config })
        .expect("a config always serialises");
    std::fs::write(&temporary, encoded)?;
    std::fs::rename(&temporary, &path)
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
