//! The daemon that owns the configuration.
//!
//! Schema, validation, the three config states, the commit pipeline, rollback,
//! rendering and applying all happen here. Every client -- the CLI today, an
//! API later -- is a thin thing that sends a request and prints what comes
//! back. If a rule can be enforced in configd, it is enforced in configd,
//! because that is the only place a client cannot skip.
//!
//! # Three states
//!
//! - **candidate** -- one per session, in memory, mirrored to
//!   `/run/nightshade/sessions/<id>.json` so a configd restart does not throw
//!   away an operator's unsaved edits
//! - **running** -- what is applied, in memory and in
//!   `/run/nightshade/running.json`
//! - **saved** -- `/etc/nightshade/config.boot`, curly-brace, written only by
//!   `save`, and the only one of the three the next boot will read
//!
//! # Boot
//!
//! Parse `config.boot`, validate, render, apply. If any of that fails --
//! because the schema moved under a config written by an older build, or
//! because the file was hand edited into something invalid -- boot with
//! defaults plus management access and say so loudly in the journal. A
//! firewall that will not come up is worse than a firewall that comes up
//! wrong, because one of those you can log into and fix.
//!
//! # Commit, in order
//!
//! 1. schema and type validation of every changed node (again -- `set`
//!    already did it; this catches a candidate restored from disk under a
//!    schema that has since changed)
//! 2. cross-node constraints over the whole candidate
//! 3. structured diff, candidate against running
//! 4. order the operations by node priority
//! 5. render the complete target state
//! 6. `check` -- nothing on the machine has been touched up to here
//! 7. apply
//! 8. verify, and on failure restore the artifacts from `last-applied/` and
//!    re-apply them
//! 9. arm the confirm timer, or promote
//! 10. promote candidate to running, append a revision to the archive
//!
//! Steps 5 and 6 before step 7 is the load-bearing part: the last opportunity
//! to fail without having changed anything is before the first file is
//! written.
//!
//! # Commit-confirm outlives its client
//!
//! `commit confirm 5` is the operator saying "I am about to change the routing
//! on the box I am reachable through". It only means anything if the rollback
//! happens when they lose the session -- so the timer is a task in configd
//! plus a marker file recording the pre-commit config and the deadline. Kill
//! the CLI and the rollback still fires. Kill configd and it reads the marker
//! on startup, resuming the timer or rolling back at once if the deadline has
//! already gone by.
//!
//! # Who did it
//!
//! Peer credentials come from `SO_PEERCRED` on every connection: uid, gid,
//! pid, checked against `nightshade-admin`, and the uid is recorded as the
//! actor on every commit. Not "someone changed the firewall at 03:12".
//!
//! # Locking
//!
//! Candidates are per session and free to diverge. Commit is exclusive, and a
//! commit from a session whose candidate was taken before the running config
//! last changed is refused -- with the diff of what changed underneath it,
//! because "config changed since session start" without saying what changed
//! leaves the operator no move but to discard.

pub mod archive;
pub mod commit;
pub mod confirm;
pub mod logging;
pub mod netif;
pub mod peer;
pub mod server;
pub mod session;
pub mod state;

pub use peer::{Access, Actor};
pub use server::{Bound, Server};
pub use state::Configd;
