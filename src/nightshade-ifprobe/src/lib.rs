//! Where the numbers in `show interfaces` come from.
//!
//! `nightshade-ifstate` holds the model and the renderers and knows nothing
//! about Linux. This crate is the other half: netlink, ethtool ioctls, sysfs,
//! SFF decoding, and the running history that a command reading the kernel
//! once could not produce.
//!
//! Only configd depends on it. `ns` is somebody's login shell and asks configd
//! for its answer, so there is one place that knows what an interface is.
//!
//! # The two halves of the daemon's job
//!
//! [`Monitor`] is the part that has to be running: it takes a counter sample
//! every [`tracker::SAMPLE_INTERVAL`] seconds, watches netlink for links going
//! up and down between samples, and keeps the whole of it in a [`Tracker`]
//! that is written to `/run` so a configd restart does not reset an operator's
//! flap counts.
//!
//! [`Probe`] is the part that runs when somebody asks: it dumps the links,
//! asks the driver whatever the command needs and no more, lays the
//! configuration over the result and returns a [`Snapshot`].
//!
//! [`Snapshot`]: nightshade_ifstate::Snapshot

pub mod driverstats;
pub mod ethtool;
pub mod netlink;
pub mod probe;
pub mod sff;
pub mod sysfs;
pub mod tracker;

mod monitor;

pub use monitor::Monitor;
pub use probe::Probe;
pub use tracker::Tracker;

/// Seconds since the epoch.
///
/// The one clock everything here shares. Wall clock rather than monotonic
/// because the durations it produces are persisted across a configd restart
/// and compared against each other afterwards, which a boot-relative clock
/// cannot survive.
pub fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.as_secs())
        .unwrap_or(0)
}
