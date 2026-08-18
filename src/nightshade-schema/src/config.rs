//! The configuration tree.
//!
//! One data structure, three renderings: curly-brace for `config.boot`, JSON
//! for display and interchange, and serde's own for the internal state files
//! under `/run`. Every component works on this; nothing works on text.
//!
//! # Shape
//!
//! A node is either interior -- named children -- or a set of values. Both a
//! container (`system`) and a tag node (`ethernet`, whose children are `eth0`,
//! `eth1`) are interior; the difference between them lives in the schema, not
//! here, because it changes how a node is *written*, not what it holds.
//!
//! A value set covers all three leaf shapes without a third case:
//!
//! | schema kind  | value set          |
//! |--------------|--------------------|
//! | `flag`       | empty              |
//! | `leaf`       | exactly one        |
//! | `multi-leaf` | one or more        |
//!
//! Enforcing which is which is validation's job. The tree holds what it is
//! given, so that an invalid candidate can exist long enough to be reported
//! precisely rather than being rejected by a data structure that cannot
//! represent it.
//!
//! # Ordering
//!
//! `BTreeMap` and `BTreeSet` throughout, so two trees built by different
//! routes render byte-identically. That is what makes golden tests and
//! `parse(render(tree)) == tree` meaningful: without it, a diff would show
//! reordering as change, and the round-trip property would be a property of
//! insertion order.
//!
//! A value set also deduplicates, which is correct rather than convenient --
//! `address 10.0.0.1/8` twice is one address.

use std::collections::btree_map::Entry;
use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::path::Path;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TreeError {
    #[error("`{path}` holds a value, so `{full}` cannot exist below it")]
    NotInterior { path: Path, full: Path },

    #[error("`{path}` holds a value, so it cannot also have child nodes")]
    NotAContainer { path: Path },

    #[error("`{path}` has child nodes, so it cannot hold a value")]
    NotALeaf { path: Path },
}

/// A node's contents.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Body {
    Interior(BTreeMap<String, Node>),
    Values(BTreeSet<String>),
}

/// One node of the tree.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Node {
    /// Comment written above this node in `config.boot`.
    ///
    /// Stored without delimiters, newline-separated if it spanned lines. It
    /// belongs to the node, not to an individual value: a comment above the
    /// second of three `address` lines attaches to `address` as a whole, and
    /// re-renders above the group. That is the documented limit of preserving
    /// comments -- the alternative is a comment slot per value, which means a
    /// list rather than a set and loses the ordering guarantee that makes
    /// everything else here deterministic.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub comment: Option<String>,

    pub body: Body,
}

impl Node {
    pub fn interior() -> Self {
        Self {
            comment: None,
            body: Body::Interior(BTreeMap::new()),
        }
    }

    pub fn values<I, S>(values: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            comment: None,
            body: Body::Values(values.into_iter().map(Into::into).collect()),
        }
    }

    /// A valueless leaf -- `disable`, `stp`.
    pub fn flag() -> Self {
        Self::values(Vec::<String>::new())
    }

    pub fn with_comment(mut self, comment: impl Into<String>) -> Self {
        self.comment = Some(comment.into());
        self
    }

    pub fn children(&self) -> Option<&BTreeMap<String, Node>> {
        match &self.body {
            Body::Interior(c) => Some(c),
            Body::Values(_) => None,
        }
    }

    pub fn value_set(&self) -> Option<&BTreeSet<String>> {
        match &self.body {
            Body::Interior(_) => None,
            Body::Values(v) => Some(v),
        }
    }

    /// The single value of a leaf, or `None` for a flag, a multi-leaf holding
    /// more than one, or an interior node.
    pub fn value(&self) -> Option<&str> {
        let v = self.value_set()?;
        if v.len() == 1 {
            v.iter().next().map(String::as_str)
        } else {
            None
        }
    }

    pub fn is_interior(&self) -> bool {
        matches!(self.body, Body::Interior(_))
    }
}

/// A whole configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfigTree {
    root: Node,
}

impl Default for ConfigTree {
    fn default() -> Self {
        Self::new()
    }
}

impl ConfigTree {
    pub fn new() -> Self {
        Self {
            root: Node::interior(),
        }
    }

    /// Build a tree from top-level nodes.
    ///
    /// There is no `from_root`, because the root is interior by construction
    /// and a constructor that could be handed a leaf would need a failure case
    /// nobody would ever handle.
    pub fn from_children(children: BTreeMap<String, Node>) -> Self {
        Self {
            root: Node {
                comment: None,
                body: Body::Interior(children),
            },
        }
    }

    pub fn root(&self) -> &Node {
        &self.root
    }

    pub fn is_empty(&self) -> bool {
        self.root.children().is_some_and(BTreeMap::is_empty)
    }

    pub fn get(&self, path: &Path) -> Option<&Node> {
        let mut node = &self.root;
        for segment in path.segments() {
            node = node.children()?.get(segment)?;
        }
        Some(node)
    }

    pub fn get_mut(&mut self, path: &Path) -> Option<&mut Node> {
        let mut node = &mut self.root;
        for segment in path.segments() {
            node = match &mut node.body {
                Body::Interior(children) => children.get_mut(segment)?,
                Body::Values(_) => return None,
            };
        }
        Some(node)
    }

    pub fn contains(&self, path: &Path) -> bool {
        self.get(path).is_some()
    }

    /// The values at `path`, or `None` if it is absent or interior.
    pub fn values_at(&self, path: &Path) -> Option<&BTreeSet<String>> {
        self.get(path)?.value_set()
    }

    /// Walk to the *parent* of `path`, creating interior nodes along the way,
    /// and return the map of children it holds.
    ///
    /// Stopping at the parent is the point. If this created the final node
    /// too, it would have to guess a body for it, and every caller below would
    /// then be unable to tell a node it just created from one the operator
    /// wrote. That is not hypothetical: an empty container written as
    /// `hostname { }` and a placeholder created on the way to
    /// `hostname` are the same value, so `add` would happily convert the
    /// operator's container into a leaf instead of reporting the
    /// contradiction. The caller creates the last node, with the body it
    /// means.
    ///
    /// Fails rather than clobbering if the walk runs into a node holding
    /// values.
    fn parent_of(&mut self, path: &Path) -> Result<&mut BTreeMap<String, Node>, TreeError> {
        let segments = path.segments();
        let parents = &segments[..segments.len().saturating_sub(1)];

        let mut node = &mut self.root;
        for (i, segment) in parents.iter().enumerate() {
            node = match &mut node.body {
                Body::Interior(children) => {
                    children.entry(segment.clone()).or_insert_with(Node::interior)
                }
                Body::Values(_) => {
                    return Err(TreeError::NotInterior {
                        path: Path::from_segments(&parents[..i]),
                        full: path.clone(),
                    });
                }
            };
        }

        match &mut node.body {
            Body::Interior(children) => Ok(children),
            Body::Values(_) => Err(TreeError::NotInterior {
                path: Path::from_segments(parents),
                full: path.clone(),
            }),
        }
    }

    /// Interior node at `path`, created if absent.
    ///
    /// A path that already holds values is an error rather than a conversion.
    /// `disable` followed by `disable { ... }` is a contradiction in the
    /// source, and quietly resolving it in favour of whichever came last is
    /// how a config file ends up meaning something nobody wrote.
    pub fn ensure_interior(&mut self, path: &Path) -> Result<&mut Node, TreeError> {
        let Some(last) = path.last().map(str::to_string) else {
            return Ok(&mut self.root);
        };
        let children = self.parent_of(path)?;
        let node = children.entry(last).or_insert_with(Node::interior);
        if !node.is_interior() {
            return Err(TreeError::NotAContainer { path: path.clone() });
        }
        Ok(node)
    }

    /// Ensure a value-holding node exists at `path`, leaving any values it
    /// already has alone.
    ///
    /// This is what a bare `disable` in a config file means. Distinct from
    /// [`set_flag`](Self::set_flag), which asserts the node holds *no* values
    /// and so would discard them.
    pub fn declare_leaf(&mut self, path: &Path) -> Result<(), TreeError> {
        let Some(last) = path.last().map(str::to_string) else {
            return Err(TreeError::NotALeaf { path: path.clone() });
        };
        let children = self.parent_of(path)?;
        match children.entry(last) {
            Entry::Occupied(existing) if existing.get().is_interior() => {
                Err(TreeError::NotALeaf { path: path.clone() })
            }
            Entry::Occupied(_) => Ok(()),
            Entry::Vacant(slot) => {
                slot.insert(Node::flag());
                Ok(())
            }
        }
    }

    /// Replace whatever is at `path` with the single value `value`.
    ///
    /// Leaf semantics: `set ... mtu 1500` then `set ... mtu 9000` leaves one
    /// MTU. Any comment on the node survives -- the value changed, not the
    /// operator's note about it.
    pub fn set(&mut self, path: &Path, value: impl Into<String>) -> Result<(), TreeError> {
        let Some(last) = path.last().map(str::to_string) else {
            return Err(TreeError::NotALeaf { path: path.clone() });
        };
        let children = self.parent_of(path)?;
        match children.entry(last) {
            Entry::Occupied(existing) if existing.get().is_interior() => {
                Err(TreeError::NotALeaf { path: path.clone() })
            }
            Entry::Occupied(mut existing) => {
                existing.get_mut().body = Body::Values([value.into()].into_iter().collect());
                Ok(())
            }
            Entry::Vacant(slot) => {
                slot.insert(Node::values([value.into()]));
                Ok(())
            }
        }
    }

    /// Add `value` to the set at `path`.
    ///
    /// Multi-leaf semantics: two addresses are two addresses.
    pub fn add(&mut self, path: &Path, value: impl Into<String>) -> Result<(), TreeError> {
        let Some(last) = path.last().map(str::to_string) else {
            return Err(TreeError::NotALeaf { path: path.clone() });
        };
        let children = self.parent_of(path)?;
        match children.entry(last) {
            Entry::Occupied(mut existing) => match &mut existing.get_mut().body {
                Body::Values(values) => {
                    values.insert(value.into());
                    Ok(())
                }
                Body::Interior(_) => Err(TreeError::NotALeaf { path: path.clone() }),
            },
            Entry::Vacant(slot) => {
                slot.insert(Node::values([value.into()]));
                Ok(())
            }
        }
    }

    /// Create a valueless leaf at `path`, discarding any values there.
    pub fn set_flag(&mut self, path: &Path) -> Result<(), TreeError> {
        let Some(last) = path.last().map(str::to_string) else {
            return Err(TreeError::NotALeaf { path: path.clone() });
        };
        let children = self.parent_of(path)?;
        match children.entry(last) {
            Entry::Occupied(existing) if existing.get().is_interior() => {
                Err(TreeError::NotALeaf { path: path.clone() })
            }
            Entry::Occupied(mut existing) => {
                existing.get_mut().body = Body::Values(BTreeSet::new());
                Ok(())
            }
            Entry::Vacant(slot) => {
                slot.insert(Node::flag());
                Ok(())
            }
        }
    }

    /// Remove the node at `path` and everything under it.
    ///
    /// Removes exactly what was named. Empty parents are left behind, because
    /// whether an empty node should survive depends on what it is -- a tag
    /// instance with every leaf deleted is still a configured interface, an
    /// empty container is noise -- and that is a question only the schema can
    /// answer.
    pub fn remove(&mut self, path: &Path) -> bool {
        let Some((parent, last)) = path.split_last() else {
            // Removing the root means emptying the tree.
            self.root = Node::interior();
            return true;
        };
        let Some(node) = self.get_mut(&parent) else {
            return false;
        };
        match &mut node.body {
            Body::Interior(children) => children.remove(last).is_some(),
            Body::Values(_) => false,
        }
    }

    /// Remove one value from the set at `path`, and the node with it if that
    /// was the last one.
    pub fn remove_value(&mut self, path: &Path, value: &str) -> bool {
        let Some(node) = self.get_mut(path) else {
            return false;
        };
        let Body::Values(values) = &mut node.body else {
            return false;
        };
        if !values.remove(value) {
            return false;
        }
        if values.is_empty() {
            self.remove(path);
        }
        true
    }

    /// A config containing only `path` and everything below it, keeping the
    /// full path structure.
    ///
    /// What `show interfaces ethernet` sends back. Full paths rather than a
    /// tree rooted at the request, so the result renders through the ordinary
    /// renderer and reads as a fragment of the config rather than as a
    /// different config that happens to look similar.
    pub fn subtree(&self, path: &Path) -> Option<ConfigTree> {
        let node = self.get(path)?.clone();
        let Some((parent, last)) = path.split_last() else {
            return Some(self.clone());
        };
        let mut tree = ConfigTree::new();
        let parent_node = tree
            .ensure_interior(&parent)
            .expect("a fresh tree has no leaves to collide with");
        if let Body::Interior(children) = &mut parent_node.body {
            children.insert(last.to_string(), node);
        }
        Some(tree)
    }

    /// Attach or clear the comment on an existing node.
    ///
    /// Returns false if there is no node there. Comments are set on what
    /// exists rather than creating it: `comment` on an unconfigured path is a
    /// typo, not an instruction.
    pub fn set_comment(&mut self, path: &Path, comment: Option<String>) -> bool {
        match self.get_mut(path) {
            Some(node) => {
                node.comment = comment;
                true
            }
            None => false,
        }
    }

    /// Every value-holding node, depth first, in path order.
    ///
    /// The order is the tree's own, so two configs walked this way line up --
    /// which is what makes a structured diff a merge of two sorted streams
    /// rather than a search.
    pub fn leaves(&self) -> Vec<(Path, &BTreeSet<String>)> {
        let mut out = Vec::new();
        collect_leaves(&self.root, &Path::root(), &mut out);
        out
    }

    /// Every node, root excluded, depth first in path order.
    pub fn nodes(&self) -> Vec<(Path, &Node)> {
        let mut out = Vec::new();
        collect_nodes(&self.root, &Path::root(), &mut out);
        out
    }
}

fn collect_leaves<'a>(node: &'a Node, at: &Path, out: &mut Vec<(Path, &'a BTreeSet<String>)>) {
    match &node.body {
        Body::Values(values) => out.push((at.clone(), values)),
        Body::Interior(children) => {
            for (name, child) in children {
                collect_leaves(child, &at.child(name), out);
            }
        }
    }
}

fn collect_nodes<'a>(node: &'a Node, at: &Path, out: &mut Vec<(Path, &'a Node)>) {
    if let Body::Interior(children) = &node.body {
        for (name, child) in children {
            let path = at.child(name);
            out.push((path.clone(), child));
            collect_nodes(child, &path, out);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> Path {
        Path::parse(s).unwrap()
    }

    #[test]
    fn set_replaces_and_add_accumulates() {
        let mut t = ConfigTree::new();
        t.set(&p("interfaces ethernet eth0 mtu"), "1500").unwrap();
        t.set(&p("interfaces ethernet eth0 mtu"), "9000").unwrap();
        assert_eq!(t.get(&p("interfaces ethernet eth0 mtu")).unwrap().value(), Some("9000"));

        let addr = p("interfaces ethernet eth0 address");
        t.add(&addr, "192.168.1.1/24").unwrap();
        t.add(&addr, "10.0.0.1/8").unwrap();
        assert_eq!(t.values_at(&addr).unwrap().len(), 2);
    }

    #[test]
    fn adding_the_same_value_twice_is_one_value() {
        let mut t = ConfigTree::new();
        let addr = p("interfaces ethernet eth0 address");
        t.add(&addr, "192.168.1.1/24").unwrap();
        t.add(&addr, "192.168.1.1/24").unwrap();
        assert_eq!(t.values_at(&addr).unwrap().len(), 1);
    }

    #[test]
    fn a_flag_is_distinct_from_an_empty_container() {
        let mut t = ConfigTree::new();
        t.set_flag(&p("interfaces ethernet eth0 disable")).unwrap();
        t.ensure_interior(&p("interfaces ethernet eth1")).unwrap();

        let flag = t.get(&p("interfaces ethernet eth0 disable")).unwrap();
        let empty = t.get(&p("interfaces ethernet eth1")).unwrap();
        assert!(!flag.is_interior());
        assert_eq!(flag.value_set().unwrap().len(), 0);
        assert!(empty.is_interior());
        assert_ne!(flag, empty);
    }

    #[test]
    fn descending_through_a_leaf_is_refused() {
        let mut t = ConfigTree::new();
        t.set(&p("system host-name"), "nightshade").unwrap();
        let err = t.set(&p("system host-name extra"), "x").unwrap_err();
        assert!(matches!(err, TreeError::NotInterior { .. }));
        // The failed set must not have damaged what was there.
        assert_eq!(t.get(&p("system host-name")).unwrap().value(), Some("nightshade"));
    }

    #[test]
    fn setting_a_value_on_a_container_is_refused() {
        let mut t = ConfigTree::new();
        t.set(&p("interfaces ethernet eth0 mtu"), "1500").unwrap();
        let err = t.set(&p("interfaces ethernet eth0"), "x").unwrap_err();
        assert!(matches!(err, TreeError::NotALeaf { .. }));
    }

    #[test]
    fn remove_takes_the_subtree_and_leaves_the_parent() {
        let mut t = ConfigTree::new();
        t.set(&p("interfaces ethernet eth0 mtu"), "1500").unwrap();
        t.set(&p("interfaces ethernet eth1 mtu"), "1500").unwrap();

        assert!(t.remove(&p("interfaces ethernet eth0")));
        assert!(!t.contains(&p("interfaces ethernet eth0")));
        assert!(t.contains(&p("interfaces ethernet eth1")));
        assert!(!t.remove(&p("interfaces ethernet eth0")));
    }

    #[test]
    fn removing_the_last_value_removes_the_node() {
        let mut t = ConfigTree::new();
        let addr = p("interfaces ethernet eth0 address");
        t.add(&addr, "192.168.1.1/24").unwrap();
        t.add(&addr, "10.0.0.1/8").unwrap();

        assert!(t.remove_value(&addr, "10.0.0.1/8"));
        assert!(t.contains(&addr));
        assert!(t.remove_value(&addr, "192.168.1.1/24"));
        assert!(!t.contains(&addr));
        assert!(!t.remove_value(&addr, "192.168.1.1/24"));
    }

    #[test]
    fn leaves_come_out_in_path_order() {
        let mut t = ConfigTree::new();
        t.set(&p("system host-name"), "fw").unwrap();
        t.add(&p("interfaces ethernet eth0 address"), "10.0.0.1/8").unwrap();
        t.set_flag(&p("interfaces ethernet eth0 disable")).unwrap();

        let paths: Vec<String> = t.leaves().iter().map(|(p, _)| p.to_string()).collect();
        assert_eq!(
            paths,
            [
                "interfaces ethernet eth0 address",
                "interfaces ethernet eth0 disable",
                "system host-name",
            ]
        );
    }

    #[test]
    fn insertion_order_does_not_change_the_tree() {
        let mut a = ConfigTree::new();
        a.set(&p("system host-name"), "fw").unwrap();
        a.add(&p("interfaces ethernet eth0 address"), "10.0.0.1/8").unwrap();
        a.add(&p("interfaces ethernet eth0 address"), "192.168.1.1/24").unwrap();

        let mut b = ConfigTree::new();
        b.add(&p("interfaces ethernet eth0 address"), "192.168.1.1/24").unwrap();
        b.set(&p("system host-name"), "fw").unwrap();
        b.add(&p("interfaces ethernet eth0 address"), "10.0.0.1/8").unwrap();

        assert_eq!(a, b);
    }

    #[test]
    fn removing_the_root_empties_the_tree() {
        let mut t = ConfigTree::new();
        t.set(&p("system host-name"), "fw").unwrap();
        assert!(t.remove(&Path::root()));
        assert!(t.is_empty());
    }
}
