//! Every file Nightshade reads or writes, in one place.
//!
//! Three trees, and the split between them is a durability decision rather
//! than tidiness:
//!
//! | tree                   | lifetime            | holds                        |
//! |------------------------|---------------------|------------------------------|
//! | `/etc/nightshade`      | survives reboot     | `config.boot` -- the saved config |
//! | `/run/nightshade`      | dies at reboot      | candidate sessions, running state, the confirm marker |
//! | `/var/lib/nightshade`  | survives reboot     | commit archive, last-applied artifacts |
//!
//! Running state lives under `/run` on purpose. What is running is by
//! definition what was applied since boot; persisting it would let a box come
//! up believing it is in a state nothing has actually been applied for. Only
//! `save` writes `/etc`, and only what `/etc` holds is what the next boot gets.
//!
//! Everything is expressed relative to a root so tests can point the whole
//! layout at a `tempfile::TempDir`. The root is a constructor argument and not
//! an environment variable: configd runs as root out of a systemd unit, and a
//! path where the config comes from should not be a thing you can move by
//! setting a variable.

use std::path::{Path, PathBuf};

/// Resolved filesystem layout.
///
/// Construct once at startup and pass it down; nothing below should be
/// building paths out of string literals.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paths {
    root: PathBuf,
}

impl Default for Paths {
    fn default() -> Self {
        Self::system()
    }
}

impl Paths {
    /// The real layout, rooted at `/`.
    pub fn system() -> Self {
        Self {
            root: PathBuf::from("/"),
        }
    }

    /// The same layout relocated under `root`, for tests and for rendering
    /// into a staging directory before it is synced into place.
    pub fn under(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Single join from the root, so a relocated layout composes cleanly and
    /// the separator handling stays in one place.
    fn at(&self, rel: &str) -> PathBuf {
        self.root.join(rel)
    }

    // -- /etc/nightshade: what survives a reboot ---------------------------

    pub fn etc_dir(&self) -> PathBuf {
        self.at("etc/nightshade")
    }

    /// The saved config, in curly-brace format. Canonical on disk: `save`
    /// writes it, boot loads it, and it is what an operator is allowed to open
    /// in an editor.
    pub fn config_boot(&self) -> PathBuf {
        self.at("etc/nightshade/config.boot")
    }

    /// Owned outright by the system renderer, header and all. This phase runs
    /// no `systemd-resolved`, so there is no stub file and no symlink to
    /// respect -- the file is ours.
    pub fn resolv_conf(&self) -> PathBuf {
        self.at("etc/resolv.conf")
    }

    /// Not owned outright. The system renderer maintains the one line that
    /// maps the configured host name and leaves the rest of the file alone.
    pub fn hosts(&self) -> PathBuf {
        self.at("etc/hosts")
    }

    // -- /run/nightshade: what a reboot is supposed to clear ---------------

    pub fn run_dir(&self) -> PathBuf {
        self.at("run/nightshade")
    }

    pub fn socket(&self) -> PathBuf {
        self.at("run/nightshade/configd.sock")
    }

    pub fn sessions_dir(&self) -> PathBuf {
        self.at("run/nightshade/sessions")
    }

    /// A session's candidate config. Backed to disk so a configd restart does
    /// not silently discard edits an operator has been typing for ten minutes;
    /// under `/run` so a reboot does.
    pub fn session_file(&self, id: &str) -> PathBuf {
        self.sessions_dir().join(format!("{id}.json"))
    }

    pub fn running(&self) -> PathBuf {
        self.at("run/nightshade/running.json")
    }

    /// Marker for an armed commit-confirm: the pre-commit running config and
    /// the deadline. This file is the entire reason commit-confirm survives
    /// configd dying mid-window -- on startup configd either resumes the timer
    /// or, if the deadline has already passed, rolls back immediately.
    pub fn pending_confirm(&self) -> PathBuf {
        self.at("run/nightshade/pending-confirm.json")
    }

    /// Interface counters, rates and link history, as the monitor keeps them.
    ///
    /// Under `/run` rather than `/var/lib`, and that is the correct half of
    /// the split: every number in it is measured against the kernel's own
    /// counters, and those are zeroed by a reboot. Carrying a flap count
    /// across a reboot would be carrying a count of flaps that the counters it
    /// sits beside no longer know anything about. Across a configd restart --
    /// which is what `/run` does survive -- it is exactly right.
    pub fn ifstate(&self) -> PathBuf {
        self.at("run/nightshade/ifstate.json")
    }

    /// Why the box came up on defaults, if it did.
    ///
    /// Written by configd at startup and read by `ns`, which says so before
    /// the first prompt. The journal has it too, and the journal is not where
    /// somebody logging in to fix a firewall looks first.
    pub fn boot_failure(&self) -> PathBuf {
        self.at("run/nightshade/boot-failed")
    }

    // -- /var/lib/nightshade: state that is not config ---------------------

    pub fn state_dir(&self) -> PathBuf {
        self.at("var/lib/nightshade")
    }

    /// Committed revisions, `<seq>-<timestamp>.boot.gz` plus a metadata
    /// sidecar naming the actor.
    pub fn archive_dir(&self) -> PathBuf {
        self.at("var/lib/nightshade/archive")
    }

    /// The rendered artifacts of the last successful apply. An apply that
    /// fails part way through is restored from here; without it a failed
    /// commit leaves the box in a state that matches no config at all.
    pub fn last_applied_dir(&self) -> PathBuf {
        self.at("var/lib/nightshade/last-applied")
    }

    // -- systemd ------------------------------------------------------------

    /// Where the interfaces renderer writes. `/run` rather than
    /// `/etc/systemd/network` because a rendered network config is derived
    /// state: it should not outlive a boot that did not apply it, and an
    /// operator editing it by hand should find it gone next boot rather than
    /// silently overriding `config.boot`.
    ///
    /// Only files marked with [`MANAGED_MARKER`](crate::MANAGED_MARKER) here
    /// are ours.
    pub fn networkd_dir(&self) -> PathBuf {
        self.at("run/systemd/network")
    }

    /// Where the `.link` files that name physical ports live.
    ///
    /// `/etc`, not `/run`, and the difference is the whole reason renaming
    /// works. A `.link` is read by udev when the device appears, which is long
    /// before configd is running -- so a file configd writes into the tmpfs at
    /// commit time is a file that has already been missed. Persisting them
    /// means the names are in effect from the first moment of the boot, and a
    /// configd that fails to start leaves the ports named as they were rather
    /// than reverting the box to kernel names.
    ///
    /// Only files marked with [`MANAGED_MARKER`](crate::MANAGED_MARKER) here
    /// are ours; the directory is shared with whatever else ships `.link`s.
    pub fn link_dir(&self) -> PathBuf {
        self.at("etc/systemd/network")
    }

    // -- the kernel ---------------------------------------------------------

    /// What the kernel says it has. Read-only, and read by configd rather than
    /// by any client, so there is one answer to what an interface looks like.
    pub fn sys_class_net(&self) -> PathBuf {
        self.at("sys/class/net")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_layout_is_absolute() {
        let p = Paths::system();
        assert_eq!(p.config_boot(), Path::new("/etc/nightshade/config.boot"));
        assert_eq!(p.socket(), Path::new("/run/nightshade/configd.sock"));
        assert_eq!(p.running(), Path::new("/run/nightshade/running.json"));
        assert_eq!(
            p.archive_dir(),
            Path::new("/var/lib/nightshade/archive")
        );
        assert_eq!(p.networkd_dir(), Path::new("/run/systemd/network"));
        assert_eq!(p.link_dir(), Path::new("/etc/systemd/network"));
        assert_eq!(p.resolv_conf(), Path::new("/etc/resolv.conf"));
    }

    #[test]
    fn relocating_moves_every_tree() {
        let p = Paths::under("/tmp/ns-test");
        assert_eq!(
            p.config_boot(),
            Path::new("/tmp/ns-test/etc/nightshade/config.boot")
        );
        assert_eq!(
            p.pending_confirm(),
            Path::new("/tmp/ns-test/run/nightshade/pending-confirm.json")
        );
        assert_eq!(
            p.last_applied_dir(),
            Path::new("/tmp/ns-test/var/lib/nightshade/last-applied")
        );
        assert_eq!(
            p.networkd_dir(),
            Path::new("/tmp/ns-test/run/systemd/network")
        );
    }

    /// A relocated layout must not leak an absolute path from anywhere. If one
    /// accessor ever joins an absolute string, `join` silently discards the
    /// root and a test writes to the real `/etc`.
    #[test]
    fn nothing_escapes_the_root() {
        let root = Path::new("/tmp/ns-test");
        let p = Paths::under(root);
        for path in [
            p.etc_dir(),
            p.config_boot(),
            p.resolv_conf(),
            p.hosts(),
            p.run_dir(),
            p.socket(),
            p.sessions_dir(),
            p.session_file("abc123"),
            p.running(),
            p.pending_confirm(),
            p.ifstate(),
            p.state_dir(),
            p.archive_dir(),
            p.last_applied_dir(),
            p.networkd_dir(),
        ] {
            assert!(
                path.starts_with(root),
                "{} escaped the relocated root",
                path.display()
            );
        }
    }

    #[test]
    fn session_files_are_named_by_id() {
        let p = Paths::under("/tmp/ns-test");
        assert_eq!(
            p.session_file("7f3a"),
            Path::new("/tmp/ns-test/run/nightshade/sessions/7f3a.json")
        );
    }
}
