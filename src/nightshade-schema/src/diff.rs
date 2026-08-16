//! Structured differences between two configs.
//!
//! What `compare` shows, what `show` marks up with `+`/`-` against running,
//! and what the commit pipeline orders by priority before applying. One
//! implementation, because a diff that is computed one way for display and
//! another way for applying is a diff that will eventually disagree with
//! itself in front of an operator.
//!
//! # What a change is
//!
//! A line, not a node. `address` holding two prefixes is two lines, so adding
//! a third is one change rather than a wholesale replacement of the node --
//! which is both what an operator means and what the renderer will do.
//!
//! Nodes that hold nothing still count. An interface configured with no
//! settings under it, or a bare flag, has no values to compare, but creating
//! or deleting one is a change. So a node contributes a valueless line exactly
//! when nothing below it contributed one.

use std::collections::BTreeSet;
use std::fmt;

use crate::config::{Body, ConfigTree, Node};
use crate::path::Path;

/// Ordered so that a removal sorts before the addition that replaces it, which
/// is the order they are read in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
pub enum Op {
    Remove,
    Add,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
pub struct Change {
    pub path: Path,
    pub op: Op,
    /// The value added or removed, or `None` for a node that holds none.
    pub value: Option<String>,
}

impl fmt::Display for Change {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let marker = match self.op {
            Op::Add => '+',
            Op::Remove => '-',
        };
        write!(f, "{marker} {}", self.path)?;
        if let Some(value) = &self.value {
            write!(f, " {}", crate::lex::quote(value))?;
        }
        Ok(())
    }
}

/// Every line that is in `to` and not in `from`, and the reverse.
///
/// Sorted by path, then removals before additions, so the result is a function
/// of the two configs and not of how either was built.
pub fn diff(from: &ConfigTree, to: &ConfigTree) -> Vec<Change> {
    let before = lines(from);
    let after = lines(to);

    let mut changes: Vec<Change> = after
        .difference(&before)
        .map(|(path, value)| Change {
            path: path.clone(),
            op: Op::Add,
            value: value.clone(),
        })
        .chain(before.difference(&after).map(|(path, value)| Change {
            path: path.clone(),
            op: Op::Remove,
            value: value.clone(),
        }))
        // A placeholder line says "this node exists". It is not news about a
        // node that exists in both configs -- and it reads as news of exactly
        // the wrong kind: configuring an MTU on a bare interface would
        // otherwise report `- interfaces ethernet eth0`, which an operator
        // would quite reasonably read as deleting the interface.
        .filter(|change| change.value.is_some() || !survives(from, to, &change.path))
        .collect();

    changes.sort();
    changes
}

/// Whether `path` is an interior node in both configs, and so was neither
/// created nor deleted however its contents moved.
fn survives(from: &ConfigTree, to: &ConfigTree, path: &Path) -> bool {
    let interior = |tree: &ConfigTree| tree.get(path).is_some_and(Node::is_interior);
    interior(from) && interior(to)
}

type Line = (Path, Option<String>);

fn lines(tree: &ConfigTree) -> BTreeSet<Line> {
    let mut out = BTreeSet::new();
    collect(tree.root(), &Path::root(), &mut out);
    out
}

/// Returns whether anything at or below `at` produced a line.
fn collect(node: &Node, at: &Path, out: &mut BTreeSet<Line>) -> bool {
    match &node.body {
        Body::Values(values) if values.is_empty() => {
            out.insert((at.clone(), None));
            true
        }
        Body::Values(values) => {
            for value in values {
                out.insert((at.clone(), Some(value.clone())));
            }
            true
        }
        Body::Interior(children) => {
            let mut any = false;
            for (name, child) in children {
                any |= collect(child, &at.child(name), out);
            }
            // An interior node with nothing under it is still a thing that was
            // configured. The root is not: an empty config is no lines, not one
            // line for the root.
            if !any && !at.is_empty() {
                out.insert((at.clone(), None));
                return true;
            }
            any
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> Path {
        Path::parse(s).unwrap()
    }

    fn shown(changes: &[Change]) -> Vec<String> {
        changes.iter().map(ToString::to_string).collect()
    }

    #[test]
    fn no_change_is_no_changes() {
        let mut tree = ConfigTree::new();
        tree.set(&p("system host-name"), "fw").unwrap();
        assert_eq!(diff(&tree, &tree), []);
        assert_eq!(diff(&ConfigTree::new(), &ConfigTree::new()), []);
    }

    #[test]
    fn a_changed_leaf_reads_as_a_removal_then_an_addition() {
        let mut before = ConfigTree::new();
        before.set(&p("interfaces ethernet eth0 mtu"), "1500").unwrap();
        let mut after = before.clone();
        after.set(&p("interfaces ethernet eth0 mtu"), "9000").unwrap();

        assert_eq!(
            shown(&diff(&before, &after)),
            [
                "- interfaces ethernet eth0 mtu 1500",
                "+ interfaces ethernet eth0 mtu 9000",
            ]
        );
    }

    #[test]
    fn one_added_value_of_a_multi_leaf_is_one_change() {
        let mut before = ConfigTree::new();
        before.add(&p("system name-server"), "1.1.1.1").unwrap();
        let mut after = before.clone();
        after.add(&p("system name-server"), "9.9.9.9").unwrap();

        assert_eq!(shown(&diff(&before, &after)), ["+ system name-server 9.9.9.9"]);
    }

    #[test]
    fn an_added_subtree_is_reported_by_its_leaves() {
        let before = ConfigTree::new();
        let mut after = ConfigTree::new();
        after.set(&p("interfaces ethernet eth0 mtu"), "9000").unwrap();
        after.set_flag(&p("interfaces ethernet eth0 disable")).unwrap();

        assert_eq!(
            shown(&diff(&before, &after)),
            [
                "+ interfaces ethernet eth0 disable",
                "+ interfaces ethernet eth0 mtu 9000",
            ]
        );
    }

    #[test]
    fn a_node_holding_nothing_still_shows_up() {
        let before = ConfigTree::new();
        let mut after = ConfigTree::new();
        after.ensure_interior(&p("interfaces ethernet eth0")).unwrap();

        assert_eq!(shown(&diff(&before, &after)), ["+ interfaces ethernet eth0"]);
        assert_eq!(shown(&diff(&after, &before)), ["- interfaces ethernet eth0"]);
    }

    /// Configuring something on a bare interface adds a setting. It does not
    /// delete the interface, and must not say that it does.
    #[test]
    fn filling_in_an_empty_node_is_not_deleting_it() {
        let mut before = ConfigTree::new();
        before.ensure_interior(&p("interfaces ethernet eth0")).unwrap();
        let mut after = before.clone();
        after.set(&p("interfaces ethernet eth0 mtu"), "9000").unwrap();

        assert_eq!(shown(&diff(&before, &after)), ["+ interfaces ethernet eth0 mtu 9000"]);
        // And emptying it again is not creating it.
        assert_eq!(shown(&diff(&after, &before)), ["- interfaces ethernet eth0 mtu 9000"]);
    }

    /// A flag is also valueless, but it is a leaf: losing one is a real
    /// change and has to survive the filter that suppresses placeholders.
    #[test]
    fn a_flag_going_away_is_still_reported() {
        let mut before = ConfigTree::new();
        before.set_flag(&p("interfaces bridge br0 stp")).unwrap();
        before.set(&p("interfaces bridge br0 priority"), "4096").unwrap();

        let mut after = before.clone();
        assert!(after.remove(&p("interfaces bridge br0 stp")));

        assert_eq!(shown(&diff(&before, &after)), ["- interfaces bridge br0 stp"]);
    }

    #[test]
    fn a_diff_is_its_own_inverse() {
        let mut before = ConfigTree::new();
        before.set(&p("system host-name"), "old").unwrap();
        before.add(&p("system name-server"), "1.1.1.1").unwrap();
        let mut after = ConfigTree::new();
        after.set(&p("system host-name"), "new").unwrap();
        after.set(&p("system time-zone"), "UTC").unwrap();

        let forward = diff(&before, &after);
        let backward = diff(&after, &before);
        assert_eq!(forward.len(), backward.len());
        for change in &forward {
            let inverse = Change {
                path: change.path.clone(),
                op: match change.op {
                    Op::Add => Op::Remove,
                    Op::Remove => Op::Add,
                },
                value: change.value.clone(),
            };
            assert!(backward.contains(&inverse), "{change} has no inverse");
        }
    }

    #[test]
    fn values_needing_quotes_get_them_in_the_display_form() {
        let before = ConfigTree::new();
        let mut after = ConfigTree::new();
        after
            .set(&p("interfaces ethernet eth0 description"), "the uplink")
            .unwrap();
        assert_eq!(
            shown(&diff(&before, &after)),
            [r#"+ interfaces ethernet eth0 description "the uplink""#]
        );
    }
}
