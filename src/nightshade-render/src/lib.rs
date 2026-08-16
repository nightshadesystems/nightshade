//! Config tree in, subsystem state out.
//!
//! A library, called in-process by configd. It is the only place in Nightshade
//! that knows what a `.netdev` file looks like or that `timedatectl` exists.
//!
//! # The trait
//!
//! ```text
//! render(config) -> Artifacts      pure; no side effects, deterministic bytes
//! check(artifacts) -> Result<()>   before anything is touched
//! apply(artifacts) -> Result<()>   the only method that changes the machine
//! previous() -> Option<Artifacts>  what apply last succeeded with
//! ```
//!
//! Two implementations in this phase -- systemd-networkd and system settings
//! (host name, resolvers, time zone). The second is small, and that is the
//! point of it: it is the proof that the trait describes a renderer rather
//! than describing networkd.
//!
//! # What `check` can and cannot do
//!
//! networkd has no dry-run. There is no `networkctl --check` to hand a
//! directory to, so `check` cannot tell us the kernel will accept the result.
//!
//! What it can do, and does, is assert the artifact set is internally
//! consistent: every `.network` that references a netdev has that netdev's
//! file present, every member named by a bond or bridge got a member
//! `.network`, no two files claim the same interface. Individual values were
//! already validated against the schema before rendering was reached, so the
//! remaining failure mode is a set that is each-file-valid and
//! collectively-wrong, and that is what is checked for.
//!
//! Anything past that is caught by the verify step after apply, and recovered
//! by restoring the previous artifacts.
//!
//! # Only ours
//!
//! Every rendered file is prefixed `ns-`, and the sync step will only ever
//! create, overwrite or delete files with that prefix in
//! `/run/systemd/network`. A file Nightshade did not write is not Nightshade's
//! to remove, even when it conflicts.
//!
//! # Deterministic bytes
//!
//! Stable file ordering, stable key ordering within a file, no timestamps in
//! output. Golden tests compare byte for byte, which is only a meaningful test
//! if identical input cannot produce two different renderings -- and only a
//! usable one if a real diff shows the change rather than a reshuffle.
//!
//! # Recreate, do not guess
//!
//! Some netdev properties -- a bond's mode is the usual one -- are fixed at
//! creation and `networkctl reload` will not move them. Those are listed in a
//! table, and a change to one deletes the netdev so networkd rebuilds it.
//! A table because the alternative is guessing, and guessing wrong here means
//! reporting success on a change that did not happen.
