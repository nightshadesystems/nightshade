//! `ns` -- the operator's view of the box.
//!
//! A modal network-device CLI, and a thin client. It edits no config, applies
//! nothing and validates nothing: it turns a typed line into a request, sends
//! it to configd over `/run/nightshade/configd.sock`, and prints the response.
//! Error text is passed through verbatim, because configd is the only thing
//! that knows why something was refused and two wordings of one error is one
//! wording too many.
//!
//! The one thing it reads directly is the schema, for tab completion and `?`
//! help. That is metadata, not judgement -- `children_of`, never
//! `validate_set`.
//!
//! # Modes
//!
//! ```text
//! user@hostname>    operational     show, ping, traceroute, configure, shell
//! user@hostname#    configuration   set, delete, compare, commit, save, ...
//! ```
//!
//! The host name in the prompt is live: committing a change to
//! `system host-name` changes the prompt, because a prompt that lies about
//! which box you are on is a prompt that gets somebody's production firewall
//! reconfigured.
//!
//! # This is a login shell
//!
//! `ns` is set as the login shell for members of `nightshade-admin`, so an
//! operator lands in operational mode and never sees a bare prompt. That makes
//! the shell-escape surface the security property of this crate:
//!
//! - no `!cmd`, no backticks, no `|` to a program, no `$(...)`
//! - the pipe modifiers -- `| match`, `| count`, `| no-more`,
//!   `| display json` -- are post-processing implemented in Rust over a
//!   response we already hold. They are not pipes. Nothing is spawned.
//! - `ping` and `traceroute` exec their binaries directly with an argv, never
//!   through a shell
//!
//! Any code path that hands operator input to `sh -c` is a bug of the same
//! severity as an authentication bypass, and is tested for as one.
//!
//! # `shell`, deliberately
//!
//! The escape hatch is a command, not an accident: `shell` from operational
//! mode drops to `/bin/bash` as the operator's own uid, gated on
//! `nightshade-admin`, logged to the journal on entry and exit. It is a real
//! shell -- the point is that there is exactly one door to it and it is
//! audited. `exit` or `operational` returns to the same `ns` process with its
//! history and mode intact. It is not available from configuration mode; leave
//! the candidate config first.

pub mod client;
pub mod complete;
pub mod output;
pub mod pipe;
pub mod repl;
pub mod shell;
