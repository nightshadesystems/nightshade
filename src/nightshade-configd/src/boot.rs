//! Applying the saved configuration at startup.
//!
//! ```text
//! read /etc/nightshade/config.boot
//!   -> parse -> validate -> render -> check -> apply -> running
//! ```
//!
//! and if any of that fails, the box comes up on schema defaults with the
//! reason recorded where somebody will find it.
//!
//! # Never boot to a dead box
//!
//! A `config.boot` that will not load is not hypothetical: the file is
//! hand-editable by design, and a schema can move under a config written by an
//! older build. Refusing to start would leave an appliance that cannot be
//! logged into and fixed, which is a worse outcome than any configuration.
//!
//! # What "defaults" does and does not include
//!
//! Schema defaults only -- a host name and a time zone. Deliberately *not* a
//! network.
//!
//! The obvious reading of "come up with management access" is to DHCP on every
//! port. On a firewall that is the wrong instinct: a box whose policy failed
//! to load, bringing up addresses on interfaces whose trust level is exactly
//! what did not load, is a worse failure than having no network. Access comes
//! from the console, where `ns` is the login shell, and the first thing it
//! prints is why this happened.
//!
//! # Only on a real boot
//!
//! `/run` is a tmpfs, so a running configuration recorded there means configd
//! has restarted rather than the box having rebooted -- and the machine is
//! already configured. Re-applying `config.boot` over it would undo whatever
//! had been committed since boot and not yet saved.

use std::sync::Arc;

use nightshade_schema::config::ConfigTree;
use nightshade_schema::model::Schema;
use tracing::{error, info, warn};

use crate::commit;

/// What startup did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// The box was already configured; configd had restarted.
    AlreadyRunning { generation: u64 },
    /// `config.boot` was applied.
    Applied { changes: usize },
    /// There was nothing saved. A box out of the installer.
    NothingSaved,
    /// Something went wrong and the box came up on defaults.
    Defaults { reason: String },
}

impl Outcome {
    pub fn failed(&self) -> Option<&str> {
        match self {
            Outcome::Defaults { reason } => Some(reason),
            _ => None,
        }
    }
}

/// Read `config.boot` and turn it into a configuration, or say why not.
pub fn saved(
    schema: &'static Schema,
    path: &std::path::Path,
) -> Result<Option<ConfigTree>, String> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("{} could not be read: {e}", path.display())),
    };

    let config = nightshade_schema::curly::parse(&text)
        .map_err(|e| format!("{} does not parse: {e}", path.display()))?;

    // Both passes, and the whole list. An operator staring at a box that came
    // up on defaults wants every reason at once, not one per reboot.
    let mut violations = schema.validate_tree(&config);
    violations.extend(schema.check_constraints(&config));
    if !violations.is_empty() {
        let mut reason = format!("{} is not a valid configuration:\n", path.display());
        for violation in &violations {
            reason.push_str(&format!("  {violation}\n"));
        }
        return Err(reason);
    }

    Ok(Some(config))
}

impl crate::state::Configd {
    /// Apply the saved configuration, or come up on defaults and say why.
    pub async fn boot(self: &Arc<Self>) -> Outcome {
        if let Some(generation) = self.recovered_generation().await {
            info!(
                generation,
                "the running configuration was recovered from /run; not re-applying config.boot"
            );
            return Outcome::AlreadyRunning { generation };
        }

        let path = self.paths().config_boot();
        let outcome = match saved(self.schema(), &path) {
            Ok(Some(config)) => self.apply_at_boot(config).await,
            Ok(None) => {
                // Nothing saved means nothing to apply -- not "apply the
                // defaults". A box out of the installer already has the host
                // name the image gave it, and applying the system renderer
                // over it would overwrite /etc/resolv.conf with an empty
                // managed file before anybody had configured anything.
                //
                // So the running configuration is empty, which is the true
                // statement: Nightshade has applied nothing to this box yet.
                info!(
                    path = %path.display(),
                    "nothing has been saved; this system has no Nightshade configuration yet"
                );
                Outcome::NothingSaved
            }
            Err(reason) => {
                error!(%reason, "the saved configuration could not be used");
                self.fall_back(reason).await
            }
        };

        // The reason, where an operator will meet it: `ns` reads this on
        // startup and says so before the first prompt. A journal entry is
        // correct and is not where somebody logging in to fix a box looks.
        let marker = self.paths().boot_failure();
        match outcome.failed() {
            Some(reason) => {
                if let Some(parent) = marker.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                let _ = std::fs::write(&marker, reason);
            }
            None => {
                let _ = std::fs::remove_file(&marker);
            }
        }

        outcome
    }

    async fn apply_at_boot(self: &Arc<Self>, config: ConfigTree) -> Outcome {
        let changes = nightshade_schema::diff::diff(&ConfigTree::new(), &config).len();
        match commit::apply(self.renderers(), &config) {
            Ok(()) => {
                let _ = self.adopt(config).await;
                info!(changes, "applied the saved configuration");
                Outcome::Applied { changes }
            }
            Err(e) => {
                error!(error = %e, "the saved configuration could not be applied");
                self.fall_back(format!("the saved configuration could not be applied: {e}"))
                    .await
            }
        }
    }

    /// Come up on defaults, and record why.
    async fn fall_back(self: &Arc<Self>, reason: String) -> Outcome {
        let defaults = self.schema().defaults();
        if let Err(e) = commit::apply(self.renderers(), &defaults) {
            // Applying a host name and a time zone failed. Nothing else here
            // can help; the socket is still coming up, so an operator can.
            error!(
                error = %e,
                "even the default configuration could not be applied; \
                 this system is running whatever it came up with"
            );
        }
        let _ = self.adopt(defaults).await;
        warn!("started with default configuration only; there is no network configuration");
        Outcome::Defaults { reason }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nightshade_schema::path::Path;

    fn schema() -> &'static Schema {
        Schema::compiled()
    }

    fn write(dir: &tempfile::TempDir, text: &str) -> std::path::PathBuf {
        let path = dir.path().join("config.boot");
        std::fs::write(&path, text).unwrap();
        path
    }

    #[test]
    fn a_good_config_loads() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(
            &dir,
            "system {\n    host-name fw-01\n}\ninterfaces {\n    ethernet eth0 {\n        address 10.0.0.1/24\n    }\n}\n",
        );
        let config = saved(schema(), &path).unwrap().unwrap();
        assert_eq!(
            config.get(&Path::parse("system host-name").unwrap()).unwrap().value(),
            Some("fw-01")
        );
    }

    #[test]
    fn nothing_saved_is_not_a_failure() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            saved(schema(), &dir.path().join("config.boot")).unwrap(),
            None
        );
    }

    #[test]
    fn a_file_that_does_not_parse_says_where() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(&dir, "system {\n    host-name fw\n");
        let reason = saved(schema(), &path).unwrap_err();
        assert!(reason.contains("does not parse"), "{reason}");
        assert!(reason.contains("line 1"), "{reason}");
    }

    /// The case this exists for: the schema moved under a config written by an
    /// older build.
    #[test]
    fn a_config_the_schema_no_longer_describes_lists_every_reason() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(
            &dir,
            "system {\n    host-name fw\n    nameserver 1.1.1.1\n    fax-number 555\n}\n",
        );
        let reason = saved(schema(), &path).unwrap_err();
        assert!(reason.contains("nameserver"), "{reason}");
        assert!(reason.contains("fax-number"), "{reason}");
    }

    #[test]
    fn a_config_that_breaks_a_constraint_is_refused_too() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(
            &dir,
            "interfaces {\n    vlan vlan100 {\n        parent eth7\n        id 100\n    }\n}\n",
        );
        let reason = saved(schema(), &path).unwrap_err();
        assert!(reason.contains("eth7"), "{reason}");
    }
}
