//! The part that has to be running all the time.
//!
//! Three things cannot be answered by looking at the kernel once:
//!
//! - **Rates.** A counter is a total. A rate is two counters and the time
//!   between them, so something has to have taken the first one.
//! - **Link history.** `2 link status changes since last clear` and
//!   `Up 12 days, 4 hours` are facts about what happened while nobody was
//!   looking. The kernel keeps neither.
//! - **Cleared counters.** `clear counters` cannot zero a kernel counter, so
//!   it records the value at the time and everything after subtracts it.
//!
//! So this is a ring buffer of samples plus a little history, updated by
//! configd every [`SAMPLE_INTERVAL`], and persisted under `/run` so that
//! restarting the daemon does not reset an operator's link-flap count.
//!
//! # Why the rate is a window and not an EWMA
//!
//! Because the load interval is per-interface and configurable. An
//! exponentially weighted average has the interval baked into its state, so
//! changing `load-interval` from five minutes to thirty seconds would mean
//! throwing the average away and waiting five minutes for a number. Keeping
//! the samples instead means the rate over any window up to
//! [`HISTORY`] is exact and available immediately, and the cost is a hundred
//! and fifty small structs per port.

use std::collections::{BTreeMap, VecDeque};

use nightshade_ifstate::model::Rates;
use nightshade_ifstate::units;
use serde::{Deserialize, Serialize};

use crate::netlink::{self, Link, Stats};

/// How often configd takes a sample.
pub const SAMPLE_INTERVAL: u64 = 5;

/// How far back samples are kept, in seconds. Longer than the largest load
/// interval an operator can configure, with room for a sampler that was late.
pub const HISTORY: u64 = 900;

/// One reading of an interface's counters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Sample {
    /// Seconds since the epoch, as the sampler saw them.
    pub at: u64,
    pub in_octets: u64,
    pub in_packets: u64,
    pub out_octets: u64,
    pub out_packets: u64,
}

impl Sample {
    fn from(at: u64, stats: &Stats) -> Self {
        Self {
            at,
            in_octets: stats.rx_bytes,
            in_packets: stats.rx_packets,
            out_octets: stats.tx_bytes,
            out_packets: stats.tx_packets,
        }
    }
}

/// What has happened to a link, as against what it is doing now.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct History {
    /// Transitions since the counters were last cleared.
    pub changes: u64,
    /// When the link last entered the state it is in, in seconds since the
    /// epoch.
    pub since: Option<u64>,
    /// The last operational state seen, for spotting the next transition.
    pub operstate: u8,
    /// When `clear counters` was last run for this interface.
    pub cleared_at: Option<u64>,
    /// The counters at that moment. Everything reported is this subtracted
    /// from what the kernel says now.
    pub baseline: Option<Baseline>,
}

/// A counter snapshot taken by `clear counters`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Baseline {
    pub rx_packets: u64,
    pub tx_packets: u64,
    pub rx_bytes: u64,
    pub tx_bytes: u64,
    pub rx_errors: u64,
    pub tx_errors: u64,
    pub rx_dropped: u64,
    pub tx_dropped: u64,
    pub multicast: u64,
    pub collisions: u64,
    pub rx_crc_errors: u64,
    pub rx_frame_errors: u64,
    pub rx_length_errors: u64,
    pub rx_over_errors: u64,
}

impl Baseline {
    fn of(stats: &Stats) -> Self {
        Self {
            rx_packets: stats.rx_packets,
            tx_packets: stats.tx_packets,
            rx_bytes: stats.rx_bytes,
            tx_bytes: stats.tx_bytes,
            rx_errors: stats.rx_errors,
            tx_errors: stats.tx_errors,
            rx_dropped: stats.rx_dropped,
            tx_dropped: stats.tx_dropped,
            multicast: stats.multicast,
            collisions: stats.collisions,
            rx_crc_errors: stats.rx_crc_errors,
            rx_frame_errors: stats.rx_frame_errors,
            rx_length_errors: stats.rx_length_errors,
            rx_over_errors: stats.rx_over_errors,
        }
    }

    /// `stats` with this baseline taken off.
    ///
    /// Saturating, not wrapping. A driver reload resets the kernel's counters
    /// to zero, which leaves the baseline above them; reporting the difference
    /// as eighteen quintillion is worse than reporting it as nothing.
    pub fn applied(&self, stats: &Stats) -> Stats {
        Stats {
            rx_packets: stats.rx_packets.saturating_sub(self.rx_packets),
            tx_packets: stats.tx_packets.saturating_sub(self.tx_packets),
            rx_bytes: stats.rx_bytes.saturating_sub(self.rx_bytes),
            tx_bytes: stats.tx_bytes.saturating_sub(self.tx_bytes),
            rx_errors: stats.rx_errors.saturating_sub(self.rx_errors),
            tx_errors: stats.tx_errors.saturating_sub(self.tx_errors),
            rx_dropped: stats.rx_dropped.saturating_sub(self.rx_dropped),
            tx_dropped: stats.tx_dropped.saturating_sub(self.tx_dropped),
            multicast: stats.multicast.saturating_sub(self.multicast),
            collisions: stats.collisions.saturating_sub(self.collisions),
            rx_crc_errors: stats.rx_crc_errors.saturating_sub(self.rx_crc_errors),
            rx_frame_errors: stats.rx_frame_errors.saturating_sub(self.rx_frame_errors),
            rx_length_errors: stats.rx_length_errors.saturating_sub(self.rx_length_errors),
            rx_over_errors: stats.rx_over_errors.saturating_sub(self.rx_over_errors),
        }
    }
}

/// Everything the daemon remembers between samples.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Tracker {
    /// Recent counter readings, per interface, oldest first.
    samples: BTreeMap<String, VecDeque<Sample>>,
    history: BTreeMap<String, History>,
}

impl Tracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record one round of counters, and notice anything that changed state.
    ///
    /// `now` is passed in rather than read here so the whole round shares one
    /// timestamp -- two interfaces sampled a millisecond apart must not end up
    /// with rates measured over different windows.
    pub fn sample(&mut self, links: &[Link], now: u64) {
        for link in links {
            let history = self.history.entry(link.name.clone()).or_default();

            // A device that has just appeared has been in its state since it
            // appeared, and has changed state no times.
            if history.since.is_none() {
                history.since = Some(now);
                history.operstate = link.operstate;
            } else if history.operstate != link.operstate {
                history.operstate = link.operstate;
                history.changes += 1;
                history.since = Some(now);
            }

            if let Some(stats) = &link.stats {
                let samples = self.samples.entry(link.name.clone()).or_default();
                samples.push_back(Sample::from(now, stats));
                while samples
                    .front()
                    .is_some_and(|oldest| now.saturating_sub(oldest.at) > HISTORY)
                {
                    samples.pop_front();
                }
            }
        }

        // An interface that has gone stops being sampled, but its history is
        // kept: a port that flapped out of existence and back is the same port
        // to the operator looking for why.
        let present: std::collections::BTreeSet<&str> =
            links.iter().map(|link| link.name.as_str()).collect();
        self.samples.retain(|name, _| present.contains(name.as_str()));
    }

    /// What is known about a link's history.
    pub fn history(&self, name: &str) -> Option<&History> {
        self.history.get(name)
    }

    /// Seconds the link has been in its current state.
    pub fn since(&self, name: &str, now: u64) -> Option<u64> {
        let since = self.history.get(name)?.since?;
        Some(now.saturating_sub(since))
    }

    /// Seconds since `clear counters`, or `None` for never.
    pub fn last_clear(&self, name: &str, now: u64) -> Option<u64> {
        let cleared = self.history.get(name)?.cleared_at?;
        Some(now.saturating_sub(cleared))
    }

    /// Take the baseline that `clear counters` subtracts from here on.
    pub fn clear(&mut self, name: &str, stats: &Stats, now: u64) {
        let history = self.history.entry(name.to_string()).or_default();
        history.baseline = Some(Baseline::of(stats));
        history.cleared_at = Some(now);
        history.changes = 0;
    }

    /// The baseline for an interface, if its counters have been cleared.
    pub fn baseline(&self, name: &str) -> Option<Baseline> {
        self.history.get(name)?.baseline
    }

    /// The rate over `interval` seconds, ending at the newest sample.
    ///
    /// `None` until there are two samples far enough apart to divide by. That
    /// is the first ten seconds after a restart, and printing a rate computed
    /// over an interval of nothing would be printing an infinity.
    pub fn rates(&self, name: &str, interval: u32, speed_mbps: Option<u64>) -> Option<Rates> {
        let samples = self.samples.get(name)?;
        let newest = *samples.back()?;

        // The oldest sample still inside the window, or the oldest there is.
        // Using the oldest available rather than refusing means a port that
        // has only been up for a minute reports its first minute rather than
        // nothing at all.
        let cutoff = newest.at.saturating_sub(interval as u64);
        let oldest = samples
            .iter()
            .find(|sample| sample.at >= cutoff)
            .copied()
            .or_else(|| samples.front().copied())?;

        let elapsed = newest.at.saturating_sub(oldest.at);
        if elapsed == 0 {
            return None;
        }
        let elapsed = elapsed as f64;

        // A counter that went backwards is a counter that was reset -- a
        // driver reload, or `clear counters` moving the baseline underneath
        // the window. Neither is a negative rate.
        let delta = |new: u64, old: u64| new.saturating_sub(old) as f64;

        let in_bps = delta(newest.in_octets, oldest.in_octets) * 8.0 / elapsed;
        let in_pps = delta(newest.in_packets, oldest.in_packets) / elapsed;
        let out_bps = delta(newest.out_octets, oldest.out_octets) * 8.0 / elapsed;
        let out_pps = delta(newest.out_packets, oldest.out_packets) / elapsed;

        Some(Rates {
            interval,
            in_bps,
            in_pps,
            in_percent: units::utilisation(in_bps, in_pps, speed_mbps),
            out_bps,
            out_pps,
            out_percent: units::utilisation(out_bps, out_pps, speed_mbps),
        })
    }

    /// Note a link event that arrived between samples.
    ///
    /// The event is not trusted for anything but "something moved": the next
    /// dump is what the state is read from. What it is trusted for is the
    /// count, because a port that flaps twice between two samples has flapped
    /// twice and a sampler that only sees the endpoints would say nothing
    /// happened.
    pub fn note_event(&mut self, link: &Link, now: u64) {
        let history = self.history.entry(link.name.clone()).or_default();
        if history.since.is_some() && history.operstate != link.operstate {
            history.changes += 1;
            history.since = Some(now);
        }
        history.operstate = link.operstate;
        history.since.get_or_insert(now);
    }

    /// Whether this state counts as the link being up, for the history.
    pub fn is_up(operstate: u8) -> bool {
        operstate == netlink::IF_OPER_UP
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn link(name: &str, operstate: u8, rx_bytes: u64, rx_packets: u64) -> Link {
        Link {
            name: name.to_string(),
            operstate,
            stats: Some(Stats {
                rx_bytes,
                rx_packets,
                ..Stats::default()
            }),
            ..Link::default()
        }
    }

    #[test]
    fn a_rate_is_the_difference_between_two_samples_over_the_time_between_them() {
        let mut tracker = Tracker::new();
        // 1 250 000 bytes in ten seconds is a megabit a second.
        tracker.sample(&[link("eth0", netlink::IF_OPER_UP, 0, 0)], 1_000);
        tracker.sample(
            &[link("eth0", netlink::IF_OPER_UP, 1_250_000, 1_000)],
            1_010,
        );

        let rates = tracker.rates("eth0", 300, Some(1_000)).expect("a rate");
        assert!((rates.in_bps - 1_000_000.0).abs() < 1.0, "{rates:?}");
        assert!((rates.in_pps - 100.0).abs() < 0.01, "{rates:?}");
        assert_eq!(rates.interval, 300);
        // 1 Mbps plus framing on a 1G port.
        assert!(rates.in_percent > 0.1 && rates.in_percent < 0.11, "{rates:?}");
    }

    #[test]
    fn one_sample_is_not_a_rate() {
        let mut tracker = Tracker::new();
        tracker.sample(&[link("eth0", netlink::IF_OPER_UP, 0, 0)], 1_000);
        assert_eq!(tracker.rates("eth0", 300, Some(1_000)), None);
        assert_eq!(tracker.rates("eth9", 300, None), None);
    }

    /// The one that produces an eighteen-quintillion-bit-per-second uplink.
    #[test]
    fn a_counter_that_was_reset_gives_no_rate_rather_than_a_vast_one() {
        let mut tracker = Tracker::new();
        tracker.sample(&[link("eth0", netlink::IF_OPER_UP, 1_000_000, 1_000)], 1_000);
        tracker.sample(&[link("eth0", netlink::IF_OPER_UP, 0, 0)], 1_010);
        let rates = tracker.rates("eth0", 300, Some(1_000)).expect("a rate");
        assert_eq!(rates.in_bps, 0.0);
        assert_eq!(rates.in_pps, 0.0);
    }

    #[test]
    fn the_window_is_the_load_interval_and_not_everything_kept() {
        let mut tracker = Tracker::new();
        for step in 0..60u64 {
            // A megabyte every five seconds, then nothing for the last thirty.
            let bytes = if step < 30 { step * 1_000_000 } else { 29_000_000 };
            tracker.sample(
                &[link("eth0", netlink::IF_OPER_UP, bytes, step)],
                1_000 + step * 5,
            );
        }
        // Over the last thirty seconds nothing moved.
        let recent = tracker.rates("eth0", 30, Some(10_000)).expect("a rate");
        assert_eq!(recent.in_bps, 0.0);
        // Over five minutes, plenty did.
        let long = tracker.rates("eth0", 300, Some(10_000)).expect("a rate");
        assert!(long.in_bps > 0.0, "{long:?}");
    }

    #[test]
    fn samples_older_than_the_history_are_dropped() {
        let mut tracker = Tracker::new();
        for step in 0..400u64 {
            tracker.sample(
                &[link("eth0", netlink::IF_OPER_UP, step * 100, step)],
                1_000 + step * 5,
            );
        }
        let kept = tracker.samples.get("eth0").expect("samples").len();
        assert!(kept <= (HISTORY / SAMPLE_INTERVAL) as usize + 1, "{kept}");
    }

    #[test]
    fn a_link_that_changes_state_is_counted_and_its_clock_restarts() {
        let mut tracker = Tracker::new();
        tracker.sample(&[link("eth0", netlink::IF_OPER_UP, 0, 0)], 1_000);
        assert_eq!(tracker.history("eth0").expect("history").changes, 0);
        assert_eq!(tracker.since("eth0", 1_100), Some(100));

        tracker.sample(&[link("eth0", netlink::IF_OPER_DOWN, 0, 0)], 1_200);
        let history = tracker.history("eth0").expect("history");
        assert_eq!(history.changes, 1);
        assert_eq!(tracker.since("eth0", 1_250), Some(50));

        tracker.sample(&[link("eth0", netlink::IF_OPER_DOWN, 0, 0)], 1_300);
        assert_eq!(tracker.history("eth0").expect("history").changes, 1);
    }

    /// A port that flaps twice between two samples has flapped twice.
    #[test]
    fn an_event_between_samples_is_counted() {
        let mut tracker = Tracker::new();
        tracker.sample(&[link("eth0", netlink::IF_OPER_UP, 0, 0)], 1_000);
        tracker.note_event(&link("eth0", netlink::IF_OPER_DOWN, 0, 0), 1_001);
        tracker.note_event(&link("eth0", netlink::IF_OPER_UP, 0, 0), 1_002);
        assert_eq!(tracker.history("eth0").expect("history").changes, 2);
        // And the next sample, which sees the same state, adds nothing.
        tracker.sample(&[link("eth0", netlink::IF_OPER_UP, 0, 0)], 1_005);
        assert_eq!(tracker.history("eth0").expect("history").changes, 2);
    }

    #[test]
    fn clearing_records_a_baseline_that_later_counters_are_measured_from() {
        let mut tracker = Tracker::new();
        let stats = Stats {
            rx_packets: 1_000,
            rx_bytes: 64_000,
            ..Stats::default()
        };
        assert_eq!(tracker.last_clear("eth0", 2_000), None);

        tracker.clear("eth0", &stats, 1_000);
        assert_eq!(tracker.last_clear("eth0", 1_600), Some(600));

        let baseline = tracker.baseline("eth0").expect("a baseline");
        let later = Stats {
            rx_packets: 1_500,
            rx_bytes: 96_000,
            ..Stats::default()
        };
        let shown = baseline.applied(&later);
        assert_eq!(shown.rx_packets, 500);
        assert_eq!(shown.rx_bytes, 32_000);
    }

    /// A driver reload puts the kernel's counters below the baseline.
    #[test]
    fn a_baseline_above_the_counters_shows_nothing_rather_than_wrapping() {
        let baseline = Baseline::of(&Stats {
            rx_packets: 1_000,
            ..Stats::default()
        });
        let shown = baseline.applied(&Stats::default());
        assert_eq!(shown.rx_packets, 0);
    }

    #[test]
    fn history_outlives_the_interface_and_samples_do_not() {
        let mut tracker = Tracker::new();
        tracker.sample(&[link("eth0", netlink::IF_OPER_UP, 0, 0)], 1_000);
        tracker.sample(&[], 1_005);
        assert!(tracker.history("eth0").is_some());
        assert!(!tracker.samples.contains_key("eth0"));
    }

    #[test]
    fn the_tracker_survives_the_round_trip_it_is_persisted_through() {
        let mut tracker = Tracker::new();
        tracker.sample(&[link("eth0", netlink::IF_OPER_UP, 10, 1)], 1_000);
        tracker.sample(&[link("eth0", netlink::IF_OPER_DOWN, 20, 2)], 1_005);

        let json = serde_json::to_string(&tracker).expect("it serialises");
        let back: Tracker = serde_json::from_str(&json).expect("it deserialises");
        assert_eq!(back, tracker);
        assert_eq!(back.history("eth0").expect("history").changes, 1);
    }
}
