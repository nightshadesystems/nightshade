//! The commit pipeline.
//!
//! Ten steps, and the order is the design:
//!
//! ```text
//!  1  schema and type validation of the whole candidate
//!  2  cross-node constraints
//!  3  has anything changed since this session started
//!  4  structured diff, candidate against running
//!  5  order the changes by node priority
//!  6  render every subsystem's complete target state
//!  7  check     <- last point at which nothing has been touched
//!  8  apply
//!  9  verify, and restore from last-applied on failure
//! 10  promote the candidate to running
//! ```
//!
//! Steps 6 and 7 before step 8 is the load-bearing part. Everything that can
//! be known without touching the machine is decided first, so the common
//! failures -- a typo, a dangling reference, an inconsistent file set -- cost
//! an error message rather than a half-configured firewall.
//!
//! # Validating again
//!
//! Steps 1 and 2 re-run checks `set` already did. That is not belt and braces:
//! a candidate lives in `/run` and outlives configd, so it can be written
//! under one schema and committed under another after an upgrade. The
//! candidate an operator resumes may contain a node that no longer exists.
//!
//! # Ordering
//!
//! Renderers run in the order of the schema priority of the subtree each owns,
//! so `system` is applied before `interfaces` because the schema says 100
//! before 200. The change list is sorted the same way, which is what the
//! commit log and the operator see.
//!
//! Note what this does *not* mean. Renderers are handed the complete target
//! state, not a list of changes, so within a subsystem there is no ordering to
//! get wrong -- networkd is told what the box should look like and works out
//! the rest. Ordering matters between subsystems, and that is where it is
//! applied.
//!
//! # When apply fails
//!
//! Every renderer that had been applied is restored from its last-applied
//! artifacts, in reverse order, and the commit fails. Renderers that had not
//! run yet are untouched by definition. A restore that itself fails is logged
//! at error and reported: at that point the box is in a state that matches no
//! config, and saying so plainly is the only useful thing left to do.
//!
//! Which is why the restore points are written only once *every* subsystem has
//! applied and verified. Recording each one as it succeeded would mean that
//! when a later subsystem failed, the earlier ones would faithfully restore
//! themselves to the configuration being rolled back.

use nightshade_render::{ApplyError, Artifacts, Renderer};
use nightshade_schema::config::ConfigTree;
use nightshade_schema::diff::Change;
use nightshade_schema::model::Schema;
use tracing::{error, info, warn};

/// Why a commit did not happen.
#[derive(Debug, thiserror::Error)]
pub enum CommitError {
    #[error("{}", .0.iter().map(ToString::to_string).collect::<Vec<_>>().join("\n"))]
    Invalid(Vec<nightshade_schema::validate::ConstraintViolation>),

    #[error("{subsystem} could not render this configuration: {message}")]
    Render {
        subsystem: &'static str,
        message: String,
    },

    /// Failed before anything was touched.
    #[error("{subsystem}: {message}")]
    Check {
        subsystem: &'static str,
        message: String,
    },

    /// Failed part way through, and the previous configuration is back.
    #[error("{subsystem} could not apply this configuration: {source}\nthe previous configuration has been restored")]
    Applied {
        subsystem: &'static str,
        #[source]
        source: ApplyError,
    },

    /// Failed part way through, and putting it back failed too.
    #[error(
        "{subsystem} could not apply this configuration: {source}\n\
         RESTORING THE PREVIOUS CONFIGURATION ALSO FAILED: {restore}\n\
         this system is not in a state that matches any saved configuration"
    )]
    Unrecovered {
        subsystem: &'static str,
        #[source]
        source: ApplyError,
        restore: String,
    },
}

/// Steps 1 and 2. Everything the schema can say about a config on its own.
pub fn validate(schema: &Schema, candidate: &ConfigTree) -> Result<(), CommitError> {
    let mut violations = schema.validate_tree(candidate);
    if violations.is_empty() {
        // Only worth asking once the config is structurally sound: cross-node
        // rules over a tree with unknown paths in it produce noise about the
        // consequences of a typo rather than about the typo.
        violations = schema.check_constraints(candidate);
    }
    if violations.is_empty() {
        Ok(())
    } else {
        Err(CommitError::Invalid(violations))
    }
}

/// Step 5. Sort changes by the priority of the node they touch.
///
/// The order things are reported in should be the order they take effect, or
/// the report teaches an operator something untrue about the system.
pub fn order(schema: &Schema, mut changes: Vec<Change>) -> Vec<Change> {
    let priority = |change: &Change| {
        // Walk up to the nearest node the schema knows. A change at
        // `interfaces ethernet eth0 mtu` is priority-ordered by the interface
        // type, because that is the level the schema assigns priorities at.
        let mut path = change.path.clone();
        loop {
            if let Some(node) = schema.node_at(&path) {
                return node.priority;
            }
            match path.parent() {
                Some(parent) => path = parent,
                None => return u32::MAX,
            }
        }
    };
    changes.sort_by_key(|change| (priority(change), change.path.clone(), change.op));
    changes
}

/// Steps 6 to 9.
///
/// Renders everything, checks everything, and only then applies anything.
pub fn apply(
    renderers: &[Box<dyn Renderer>],
    candidate: &ConfigTree,
) -> Result<(), CommitError> {
    // 6: render.
    let mut plan: Vec<(&dyn Renderer, Artifacts)> = Vec::new();
    for renderer in renderers {
        let artifacts = renderer
            .render(candidate)
            .map_err(|e| CommitError::Render {
                subsystem: renderer.name(),
                message: e.to_string(),
            })?;
        plan.push((renderer.as_ref(), artifacts));
    }

    // 7: check. All of them, before any of them is applied -- an inconsistency
    // in the second subsystem should not be discovered after the first has
    // already changed the box.
    for (renderer, artifacts) in &plan {
        renderer.check(artifacts).map_err(|e| CommitError::Check {
            subsystem: renderer.name(),
            message: e.to_string(),
        })?;
    }

    // 8 and 9: apply and verify, one subsystem at a time.
    let mut applied: Vec<&dyn Renderer> = Vec::new();
    for (renderer, artifacts) in &plan {
        let outcome = renderer
            .apply(artifacts)
            .and_then(|()| renderer.verify(artifacts));

        match outcome {
            Ok(()) => {
                applied.push(*renderer);
                info!(subsystem = renderer.name(), "applied");
            }
            Err(source) => {
                applied.push(*renderer);
                error!(subsystem = renderer.name(), error = %source, "apply failed, restoring");
                return Err(match restore(&applied) {
                    Ok(()) => CommitError::Applied {
                        subsystem: renderer.name(),
                        source,
                    },
                    Err(restore) => CommitError::Unrecovered {
                        subsystem: renderer.name(),
                        source,
                        restore,
                    },
                });
            }
        }
    }

    // Only now. "Last applied" has to mean the last state the box was fully
    // in, not the last thing each renderer happened to write -- recording it
    // per renderer as it succeeded would mean that when a later subsystem
    // failed, the earlier ones would restore to the configuration that was
    // being rolled back.
    for (renderer, artifacts) in &plan {
        if let Err(e) = renderer.remember(artifacts) {
            // Applied and working. Not being able to record it costs the
            // *next* failure its restore point, which is worth shouting about
            // and not worth undoing a good commit for.
            warn!(
                subsystem = renderer.name(),
                error = %e,
                "applied, but could not record it as the restore point"
            );
        }
    }

    Ok(())
}

/// Put back what was there, most recently applied first.
///
/// Reverse order for the same reason the applies were forward: a subsystem
/// applied later may depend on one applied earlier, so it is undone first.
fn restore(applied: &[&dyn Renderer]) -> Result<(), String> {
    let mut problems = Vec::new();
    for renderer in applied.iter().rev() {
        let Some(previous) = renderer.previous() else {
            // Nothing has ever been applied for this subsystem, so there is
            // nothing to go back to. That is the first commit on a fresh box:
            // whatever was written is what a fresh box would have had anyway.
            info!(
                subsystem = renderer.name(),
                "nothing to restore -- this was its first apply"
            );
            continue;
        };
        if let Err(e) = renderer.apply(&previous) {
            problems.push(format!("{}: {e}", renderer.name()));
        } else {
            info!(subsystem = renderer.name(), "restored");
        }
    }
    if problems.is_empty() {
        Ok(())
    } else {
        Err(problems.join("; "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nightshade_schema::path::Path;

    fn schema() -> &'static Schema {
        Schema::compiled()
    }

    fn config(pairs: &[(&str, &str)]) -> ConfigTree {
        let mut tree = ConfigTree::new();
        for (path, value) in pairs {
            let path = Path::parse(path).unwrap();
            let value = (!value.is_empty()).then_some(*value);
            schema().apply_set(&mut tree, &path, value).unwrap();
        }
        tree
    }

    #[test]
    fn validation_reports_structure_before_constraints() {
        // A vlan whose parent is not configured is a constraint violation,
        // and the tree is otherwise sound.
        let candidate = config(&[
            ("interfaces vlan vlan100 parent", "eth7"),
            ("interfaces vlan vlan100 id", "100"),
        ]);
        let err = validate(schema(), &candidate).unwrap_err();
        assert!(err.to_string().contains("eth7"), "{err}");
    }

    #[test]
    fn a_sound_config_validates() {
        let candidate = config(&[
            ("system host-name", "fw"),
            ("interfaces ethernet eth0 address", "10.0.0.1/24"),
        ]);
        assert!(validate(schema(), &candidate).is_ok());
    }

    #[test]
    fn changes_are_ordered_by_the_schema_s_priorities() {
        let before = ConfigTree::new();
        let after = config(&[
            ("interfaces vxlan vxlan1 vni", "1"),
            ("interfaces ethernet eth0 mtu", "9000"),
            ("system host-name", "fw"),
            ("interfaces bonding bond0 mode", "802.3ad"),
        ]);

        let ordered = order(schema(), nightshade_schema::diff::diff(&before, &after));
        let paths: Vec<String> = ordered.iter().map(|c| c.path.to_string()).collect();

        // system (100) < ethernet (200) < bonding (210) < vxlan (240), which
        // is the order these actually have to be applied in.
        assert_eq!(
            paths,
            [
                "system host-name",
                "interfaces ethernet eth0 mtu",
                "interfaces bonding bond0 mode",
                "interfaces vxlan vxlan1 vni",
            ]
        );
    }
}
