//! The boundary between the install engine and whatever is drawing it.
//!
//! The engine owns the flow -- the order of the screens, the validation, the
//! destructive steps -- and calls out through this trait to ask questions and
//! report progress. A frontend answers questions and draws; it makes no
//! decisions. That is what lets the plain line-based prompt flow and the TUI
//! be genuinely interchangeable, including the fallback where the TUI fails to
//! initialise on a serial console and the engine keeps running against the
//! plain frontend instead.

use std::path::Path;

use crate::config::{InstallConfig, Layout};
use crate::disk::Disk;
use crate::error::{Error, Result};
use crate::secret::SecretString;

pub mod plain;

#[cfg(feature = "tui")]
pub mod tui;

/// What the operator wants to do once the installer is finished.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FinalAction {
    Reboot,
    Shell,
}

/// Progress reported during the destructive phase.
pub enum Progress {
    /// Beginning a numbered step.
    Step {
        index: usize,
        total: usize,
        name: String,
    },
    /// A line of detail under the current step.
    Detail(String),
    /// Something non-fatal the operator should still see.
    Warning(String),
}

pub trait Frontend {
    /// Branding and the destructive-operation warning. Err(Aborted) to quit.
    fn welcome(&mut self) -> Result<()>;

    /// Pick one disk, or exactly two for a mirror. Returns indices into
    /// `disks`. The engine validates the count and the size mismatch; a
    /// frontend only has to return what was picked.
    fn select_disks(&mut self, disks: &[Disk]) -> Result<Vec<usize>>;

    /// Surface a non-fatal concern and ask whether to continue anyway. Used
    /// for the mirror size mismatch, which is allowed but worth a look.
    fn confirm_warning(&mut self, message: &str) -> Result<()>;

    /// Show exactly what is about to be destroyed and require the literal word
    /// ERASE. Implementations must not offer a default-yes of any kind.
    fn confirm_destruction(&mut self, disks: &[&Disk], layout: Layout) -> Result<()>;

    /// Collect the account password. Must be entered twice and must satisfy
    /// `validate::password`; there is no skip path.
    fn collect_password(&mut self, user: &str) -> Result<SecretString>;

    fn collect_hostname(&mut self, default: &str) -> Result<String>;

    /// Last confirmation before anything is written.
    fn confirm_summary(&mut self, config: &InstallConfig) -> Result<()>;

    /// Called as installation proceeds. Must not block.
    fn progress(&mut self, progress: Progress);

    /// Installation failed. `err` carries the failing command and its output.
    fn failed(&mut self, err: &Error, log_path: &Path);

    /// Installation succeeded; ask what to do next.
    fn finished(&mut self, log_path: &Path) -> Result<FinalAction>;
}

/// Shared text so both frontends say the same thing.
pub mod text {
    pub const TITLE: &str = "Nightshade OS installer";

    pub const WELCOME_WARNING: &str = "\
This installs Nightshade OS on this machine.

Installation is DESTRUCTIVE. Every disk you select will be erased
completely: existing partitions, filesystems and data are lost, and
there is no undo.

Nothing is written to any disk until you have confirmed twice.";

    pub const DISK_HELP: &str = "\
Select ONE disk for a single-disk install, or TWO disks for a ZFS
RAID1 mirror (for example: 1 2).";

    pub const PASSWORD_HELP: &str = "\
This is the only account on the system. The root account is locked;
administrative access is through sudo from this account.";
}

/// Turn a raw selection of disk indices into a validated list.
///
/// Lives here rather than in a frontend so both frontends reject the same
/// things with the same words.
pub fn validate_selection(selection: &[usize], available: usize) -> Result<()> {
    if selection.is_empty() {
        return Err(Error::invalid("Select at least one disk."));
    }
    if selection.len() > 2 {
        return Err(Error::invalid(
            "Select one disk, or exactly two for a mirror.",
        ));
    }
    if selection.iter().any(|i| *i >= available) {
        return Err(Error::invalid("That disk number is not in the list."));
    }
    if selection.len() == 2 && selection[0] == selection[1] {
        return Err(Error::invalid("Select two different disks for a mirror."));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selection_rules() {
        assert!(validate_selection(&[0], 2).is_ok());
        assert!(validate_selection(&[0, 1], 2).is_ok());

        assert!(validate_selection(&[], 2).is_err(), "empty");
        assert!(validate_selection(&[0, 1, 0], 2).is_err(), "three disks");
        assert!(validate_selection(&[5], 2).is_err(), "out of range");
        assert!(validate_selection(&[1, 1], 2).is_err(), "same disk twice");
    }
}
