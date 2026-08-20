//! `show interfaces`, from the model down to the last space.
//!
//! This crate holds three things and no others:
//!
//! - [`model`] -- what an interface is, as a set of serde types. These are the
//!   wire types `nightshade-proto` carries and the JSON `| display json`
//!   emits, so the text and the structured output are the same data rather
//!   than two renderings that have to be kept in step.
//! - [`query`] -- what was asked. `show interfaces eth0-3 counters errors` is
//!   parsed once, here, and both the CLI and the daemon read the result.
//! - [`render`] -- the text. One module per command family, over the shared
//!   column machinery in [`layout`] and [`block`].
//!
//! What it does not hold is any way of finding out what the interfaces are.
//! That is `nightshade-ifprobe`, which talks netlink and ethtool and which
//! only configd depends on. `ns` is somebody's login shell; linking the
//! collector into it would put a netlink parser in every session on the box
//! for the sake of a program that never calls it.
//!
//! # Why the output looks like Arista's
//!
//! Because an operator's eyes are trained. The layouts, the column widths, the
//! phrasing and the order of the sections are EOS's, down to the quirks --
//! `Loopback Mode :` with the space before the colon, `0:05` meaning five
//! minutes, the transceiver table whose values sit one character right of the
//! rule above them. Where EOS's look and Linux's reality conflict, the look
//! wins and the data source is adapted; where they cannot both be had, the
//! deviation is documented at the place it happens.
//!
//! Two things are deliberately not EOS's, because they are Nightshade's:
//! interfaces are called what Linux calls them (`eth0`, never `Ethernet1`),
//! and MAC addresses are written `2c:dd:e9:12:00:a1` rather than in dotted
//! quads.

pub mod block;
pub mod layout;
pub mod model;
pub mod query;
pub mod render;
pub mod units;

pub use model::{Interface, Kind, Snapshot};
pub use query::{Query, QueryError, View};
pub use render::render;
