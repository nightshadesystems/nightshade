//! The long-running half: a sampler and a netlink watcher.
//!
//! Deliberately not async. configd is a tokio program, but neither of these
//! two loops has anything to overlap -- one sleeps and then does a netlink
//! dump, the other blocks in `recv` -- and both of them are blocking syscalls
//! that would have to be moved onto a blocking pool anyway. Two threads and a
//! mutex is the whole of it, and it is a shape a reader can hold in their head
//! at three in the morning.
//!
//! configd starts this once and holds the [`Monitor`]; every `show interfaces`
//! borrows the tracker out of it for as long as it takes to read.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use nightshade_common::paths::Paths;

use crate::netlink;
use crate::probe::Probe;
use crate::tracker::{SAMPLE_INTERVAL, Tracker};

/// The sampler and the netlink watcher, and the state they share.
pub struct Monitor {
    tracker: Arc<Mutex<Tracker>>,
    paths: Paths,
    stopping: Arc<AtomicBool>,
    threads: Vec<std::thread::JoinHandle<()>>,
}

impl Monitor {
    /// Load whatever the last run left behind, and start both loops.
    pub fn start(paths: Paths) -> Self {
        let tracker = Arc::new(Mutex::new(load(&paths)));
        let stopping = Arc::new(AtomicBool::new(false));

        let mut monitor = Self {
            tracker: Arc::clone(&tracker),
            paths: paths.clone(),
            stopping: Arc::clone(&stopping),
            threads: Vec::new(),
        };

        monitor.threads.push(spawn_sampler(
            Arc::clone(&tracker),
            paths.clone(),
            Arc::clone(&stopping),
        ));
        monitor
            .threads
            .push(spawn_watcher(Arc::clone(&tracker), Arc::clone(&stopping)));
        monitor
    }

    /// A monitor that samples nothing, for tests and for a box where netlink
    /// is unavailable. Every rate is `None` and every history is empty, which
    /// the renderers already handle.
    pub fn idle(paths: Paths) -> Self {
        Self {
            tracker: Arc::new(Mutex::new(Tracker::new())),
            paths,
            stopping: Arc::new(AtomicBool::new(true)),
            threads: Vec::new(),
        }
    }

    /// Borrow the tracker.
    ///
    /// A poisoned mutex means a sampler thread panicked mid-update. The
    /// tracker is a cache of measurements and nothing downstream trusts it for
    /// correctness, so the contents are taken as they are rather than turning
    /// one panicked thread into a `show interfaces` that never works again.
    pub fn tracker(&self) -> MutexGuard<'_, Tracker> {
        match self.tracker.lock() {
            Ok(tracker) => tracker,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    /// Record the baseline `clear counters` measures from.
    pub fn clear(&self, probe: &Probe, names: &[String]) {
        let Ok(links) = probe.links() else {
            return;
        };
        let now = crate::now();
        let mut tracker = self.tracker();
        for link in &links {
            if !names.is_empty() && !names.contains(&link.name) {
                continue;
            }
            if let Some(stats) = &link.stats {
                tracker.clear(&link.name, stats, now);
            }
        }
        drop(tracker);
        self.persist();
    }

    /// Write the tracker out, so a configd restart does not lose it.
    pub fn persist(&self) {
        let tracker = self.tracker().clone();
        let path = self.paths.ifstate();
        let Ok(text) = serde_json::to_string(&tracker) else {
            return;
        };
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        // Written whole and renamed into place: the file is read at startup
        // and a half-written one would be a configd that will not start.
        let temporary = path.with_extension("json.new");
        if std::fs::write(&temporary, text).is_ok() {
            let _ = std::fs::rename(&temporary, &path);
        }
    }

    /// Stop both loops and write the tracker out.
    pub fn stop(&mut self) {
        self.stopping.store(true, Ordering::Relaxed);
        self.persist();
        // Not joined. The watcher is blocked in `recv` on a netlink socket
        // with a timeout, so joining would hold shutdown up for as long as
        // that timeout; the threads own nothing that outliving the process
        // would damage, and everything worth keeping is already on disk.
        self.threads.clear();
    }
}

impl Drop for Monitor {
    fn drop(&mut self) {
        self.stop();
    }
}

fn load(paths: &Paths) -> Tracker {
    std::fs::read_to_string(paths.ifstate())
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

/// Take a counter sample every [`SAMPLE_INTERVAL`] seconds.
fn spawn_sampler(
    tracker: Arc<Mutex<Tracker>>,
    paths: Paths,
    stopping: Arc<AtomicBool>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let probe = Probe::new(paths.clone());
        // How many samples between writes to disk. Every sample would be a
        // write every five seconds forever, and what is being protected is a
        // flap count, not a transaction.
        const PERSIST_EVERY: u32 = 12;
        let mut since_persist = 0;

        while !stopping.load(Ordering::Relaxed) {
            if let Ok(links) = probe.links() {
                let now = crate::now();
                if let Ok(mut tracker) = tracker.lock() {
                    tracker.sample(&links, now);
                }
            }

            since_persist += 1;
            if since_persist >= PERSIST_EVERY {
                since_persist = 0;
                if let Ok(tracker) = tracker.lock() {
                    write(&paths, &tracker);
                }
            }

            // Slept in short steps so that stopping does not wait five
            // seconds for a thread that has already been told to stop.
            for _ in 0..SAMPLE_INTERVAL * 4 {
                if stopping.load(Ordering::Relaxed) {
                    return;
                }
                std::thread::sleep(Duration::from_millis(250));
            }
        }
    })
}

/// Watch for links changing state between samples.
fn spawn_watcher(
    tracker: Arc<Mutex<Tracker>>,
    stopping: Arc<AtomicBool>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        while !stopping.load(Ordering::Relaxed) {
            let Ok(socket) = netlink::Socket::open_monitor() else {
                // No netlink: the sampler still runs and the counts are just
                // the ones two samples apart can see.
                return;
            };
            // So that a stopping daemon is not held up by a quiet network.
            let _ = socket.set_timeout(1);

            while !stopping.load(Ordering::Relaxed) {
                match socket.wait_for_link_event() {
                    Ok(Some(links)) => {
                        let now = crate::now();
                        if let Ok(mut tracker) = tracker.lock() {
                            for link in &links {
                                tracker.note_event(link, now);
                            }
                        }
                    }
                    // A timeout, which is what a network where nothing is
                    // happening looks like.
                    Ok(None) => {}
                    // The socket died. Rebuilding it is the right move: the
                    // usual cause is a receive buffer overrun from a burst of
                    // events, and the next dump will resynchronise anyway.
                    Err(_) => break,
                }
            }
        }
    })
}

fn write(paths: &Paths, tracker: &Tracker) {
    let Ok(text) = serde_json::to_string(tracker) else {
        return;
    };
    let path = paths.ifstate();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let temporary = path.with_extension("json.new");
    if std::fs::write(&temporary, text).is_ok() {
        let _ = std::fs::rename(&temporary, &path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_idle_monitor_answers_with_an_empty_tracker() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let monitor = Monitor::idle(Paths::under(dir.path()));
        assert_eq!(monitor.tracker().history("eth0"), None);
    }

    #[test]
    fn what_is_persisted_is_what_comes_back() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let paths = Paths::under(dir.path());

        let monitor = Monitor::idle(paths.clone());
        {
            let mut tracker = monitor.tracker();
            tracker.clear(
                "eth0",
                &crate::netlink::Stats {
                    rx_packets: 1_000,
                    ..crate::netlink::Stats::default()
                },
                1_700,
            );
        }
        monitor.persist();
        assert!(paths.ifstate().exists());

        let back = load(&paths);
        assert_eq!(back.baseline("eth0").expect("a baseline").rx_packets, 1_000);
    }

    /// A file left half written by a machine losing power must not stop the
    /// daemon starting.
    #[test]
    fn an_unreadable_file_is_started_from_rather_than_failed_on() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let paths = Paths::under(dir.path());
        std::fs::create_dir_all(paths.run_dir()).expect("a directory");
        std::fs::write(paths.ifstate(), "{ not json").expect("a file");
        assert_eq!(load(&paths), Tracker::new());
    }
}
