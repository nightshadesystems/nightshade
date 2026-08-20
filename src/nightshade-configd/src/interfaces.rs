//! What `show interfaces` is answered from, on configd's side.
//!
//! The collector lives in `nightshade-ifprobe`; this is the thin piece that
//! owns it, keeps the sampler running for the life of the daemon, and turns a
//! request into a snapshot. The CLI does not read `/sys` or open a netlink
//! socket -- it asks here, like it does for everything else, so there is one
//! answer on the box to what an interface looks like.
//!
//! # The time in the header
//!
//! `show interfaces phy detail` prints the system clock, and it is formatted
//! here rather than in the CLI because this is the side that knows the
//! configured time zone. A support bundle read three days later needs the
//! `... ago` durations under it to be relative to something.

use nightshade_common::paths::Paths;
use nightshade_ifprobe::{Monitor, Probe};
use nightshade_ifstate::Snapshot;
use nightshade_ifstate::query::Query;
use nightshade_schema::config::ConfigTree;
use nightshade_schema::path::Path;

/// The interface half of the daemon.
pub struct Interfaces {
    probe: Probe,
    monitor: Monitor,
}

impl Interfaces {
    /// Start sampling. Called once, at daemon startup.
    pub fn start(paths: Paths) -> Self {
        Self {
            probe: Probe::new(paths.clone()),
            monitor: Monitor::start(paths),
        }
    }

    /// A collector that samples nothing, for tests and for a box with no
    /// netlink. Every rate is absent and every history is empty, which the
    /// renderers already handle.
    pub fn idle(paths: Paths) -> Self {
        Self {
            probe: Probe::new(paths.clone()),
            monitor: Monitor::idle(paths),
        }
    }

    /// Answer one `show interfaces ...`.
    pub fn snapshot(&self, query: &Query, running: &ConfigTree) -> Snapshot {
        let now = nightshade_ifprobe::now();
        let mut snapshot = {
            let tracker = self.monitor.tracker();
            self.probe.snapshot(query, running, &tracker, now)
        };
        snapshot.system.time = Some(local_time(running));
        snapshot
    }

    /// Move the baselines that everything reported is measured from.
    pub fn clear(&self, names: &[String]) {
        self.monitor.clear(&self.probe, names);
    }
}

/// `Thu Aug 20 14:02:11 2026`, in the configured zone.
///
/// The C `asctime` shape, because that is what EOS prints and an operator
/// comparing two boxes' support bundles should not have to reconcile two date
/// formats as well as everything else.
fn local_time(running: &ConfigTree) -> String {
    let zone = running
        .get(&Path::from_segments(["system", "time-zone"]))
        .and_then(|node| node.value().map(str::to_string))
        .and_then(|name| jiff::tz::TimeZone::get(&name).ok())
        .unwrap_or(jiff::tz::TimeZone::UTC);

    let now = jiff::Timestamp::now().to_zoned(zone);
    // `%e` rather than `%d`: the day is space-padded in this format, so the
    // second of the month is `Aug  2` and not `Aug 02`.
    now.strftime("%a %b %e %H:%M:%S %Y").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use nightshade_ifstate::query::View;

    fn config(pairs: &[(&str, &str)]) -> ConfigTree {
        let schema = nightshade_schema::model::Schema::compiled();
        let mut tree = ConfigTree::new();
        for (path, value) in pairs {
            let path = Path::parse(path).expect("a path");
            let value = (!value.is_empty()).then_some(*value);
            schema.apply_set(&mut tree, &path, value).expect("a set");
        }
        tree
    }

    #[test]
    fn the_clock_is_formatted_the_way_the_output_prints_it() {
        let text = local_time(&config(&[("system time-zone", "UTC")]));
        // `Thu Aug 20 14:02:11 2026` -- five fields, and the last is a year.
        let fields: Vec<&str> = text.split_whitespace().collect();
        assert_eq!(fields.len(), 5, "{text}");
        assert_eq!(fields[3].split(':').count(), 3, "{text}");
        assert!(fields[4].parse::<u32>().unwrap_or(0) >= 2024, "{text}");
    }

    /// A time zone that is not on the box must not stop the command.
    #[test]
    fn an_unknown_time_zone_falls_back_rather_than_failing() {
        let mut tree = ConfigTree::new();
        tree.set(
            &Path::parse("system time-zone").expect("a path"),
            "Mars/Olympus_Mons",
        )
        .expect("a set");
        assert!(!local_time(&tree).is_empty());
        assert!(!local_time(&ConfigTree::new()).is_empty());
    }

    /// The whole point of the idle collector: every command answers, on a box
    /// where nothing can be sampled.
    #[test]
    fn an_idle_collector_still_answers_every_command() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let interfaces = Interfaces::idle(Paths::under(dir.path()));
        let running = config(&[("interfaces ethernet eth9 description", "not here")]);

        for view in [View::Detail, View::Description, View::Status(None)] {
            let query = Query {
                view: view.clone(),
                names: Vec::new(),
            };
            let snapshot = interfaces.snapshot(&query, &running);
            let text = nightshade_ifstate::render(&snapshot, &view);
            assert!(text.contains("eth9"), "{view:?}: {text}");
        }
    }

    /// A configured interface the kernel does not have is the single most
    /// useful thing this command says, and it must survive the rewrite.
    #[test]
    fn a_configured_interface_the_kernel_lacks_is_reported_as_not_present() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let interfaces = Interfaces::idle(Paths::under(dir.path()));
        let running = config(&[("interfaces ethernet eth9", "")]);

        let snapshot = interfaces.snapshot(&Query::default(), &running);
        let eth9 = snapshot.get("eth9").expect("eth9 is in the snapshot");
        assert!(!eth9.present);

        let text = nightshade_ifstate::render(&snapshot, &View::Detail);
        assert!(text.contains("line protocol is notpresent"), "{text}");
    }
}
