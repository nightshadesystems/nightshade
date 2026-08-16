//! Checking a config against the schema.
//!
//! Three entry points, and the split between them is the commit pipeline's:
//!
//! - [`Schema::validate_set`] -- one node, one value. What `set` calls, so a
//!   mistake is rejected as it is typed rather than at commit.
//! - [`Schema::validate_tree`] -- structure and types over a whole config.
//!   Run again at commit, because a candidate can outlive the schema that was
//!   loaded when it was written: configd restarts, the sessions under `/run`
//!   survive, and the box may have been upgraded in between.
//! - [`Schema::check_constraints`] -- everything that involves more than one
//!   node. Only meaningful over a complete config, so it runs at commit and
//!   not at `set`: half-typed configs are supposed to be inconsistent.
//!
//! All of it runs in configd. The CLI reads the schema for completion and help
//! and nothing else -- a check a client performs is a check a different client
//! omits.

use std::collections::BTreeMap;

use crate::config::{ConfigTree, Node};
use crate::model::{
    Constraint, GlobalConstraint, Location, NodeKind, PathPattern, PatternSegment, Schema,
    SchemaNode,
};
use crate::path::Path;
use crate::value::ValueError;

/// Something wrong with a config, and where.
///
/// One flat type for every pass. An operator does not care which of our
/// internal phases rejected their config, only what to change; and configd
/// passes the text straight through to them.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{path}: {message}")]
pub struct ConstraintViolation {
    pub path: Path,
    pub message: String,
}

impl ConstraintViolation {
    fn new(path: Path, message: impl Into<String>) -> Self {
        Self {
            path,
            message: message.into(),
        }
    }
}

/// Why a single `set` was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SetError {
    #[error("`{path}` is not a configuration path")]
    UnknownPath { path: Path },

    #[error("`{path}` takes a value: {expected}")]
    ValueRequired { path: Path, expected: String },

    #[error("`{path}` takes no value")]
    UnexpectedValue { path: Path },

    #[error("`{path}`: {source}")]
    BadValue {
        path: Path,
        #[source]
        source: ValueError,
    },

    #[error("`{path}`: {source}")]
    BadName {
        path: Path,
        #[source]
        source: ValueError,
    },

    #[error("`{path}` groups other settings; set one of them instead")]
    NotSettable { path: Path },
}

impl Schema {
    /// A config holding every default that applies unconditionally.
    ///
    /// Only defaults outside tag nodes: `mtu 1500` is a default *per
    /// interface*, and there are no interfaces until one is configured, so
    /// there is nowhere to put it. What comes back is what configd boots with
    /// when `config.boot` will not parse -- a host name and a time zone, not
    /// a network.
    pub fn defaults(&self) -> ConfigTree {
        let mut tree = ConfigTree::new();
        collect_defaults(&self.root, &Path::root(), &mut tree);
        tree
    }

    /// Validate one `set`.
    ///
    /// `value` is `None` for a flag (`set ... disable`) and for creating a bare
    /// tag instance (`set interfaces ethernet eth0`).
    pub fn validate_set(&self, path: &Path, value: Option<&str>) -> Result<(), SetError> {
        // Walk by hand rather than through `resolve`, so that every tag key
        // along the way is checked. `set interfaces ethernet "eth 0" mtu 1500`
        // should complain about the interface name, not about the MTU.
        let mut location = Location::Node(&self.root);
        for (i, segment) in path.segments().iter().enumerate() {
            let so_far = Path::from_segments(&path.segments()[..=i]);
            location = match location {
                Location::Node(node) => match &node.kind {
                    NodeKind::Container => match node.children.get(segment) {
                        Some(child) => Location::Node(child),
                        None => return Err(SetError::UnknownPath { path: so_far }),
                    },
                    NodeKind::Tag(tag) => {
                        tag.value.check(segment).map_err(|source| SetError::BadName {
                            path: so_far,
                            source,
                        })?;
                        Location::Instance(node)
                    }
                    NodeKind::Leaf(_) | NodeKind::MultiLeaf(_) | NodeKind::Flag => {
                        return Err(SetError::UnknownPath { path: so_far });
                    }
                },
                Location::Instance(tag) => match tag.children.get(segment) {
                    Some(child) => Location::Node(child),
                    None => return Err(SetError::UnknownPath { path: so_far }),
                },
                Location::Value(_) => return Err(SetError::UnknownPath { path: so_far }),
            };
        }

        match (location, value) {
            (Location::Node(node), Some(value)) => match &node.kind {
                NodeKind::Leaf(spec) | NodeKind::MultiLeaf(spec) => {
                    spec.check(value).map_err(|source| SetError::BadValue {
                        path: path.clone(),
                        source,
                    })
                }
                NodeKind::Flag => Err(SetError::UnexpectedValue { path: path.clone() }),
                NodeKind::Container | NodeKind::Tag(_) => {
                    Err(SetError::UnknownPath { path: path.child(value) })
                }
            },
            (Location::Node(node), None) => match &node.kind {
                NodeKind::Flag => Ok(()),
                NodeKind::Leaf(spec) | NodeKind::MultiLeaf(spec) => Err(SetError::ValueRequired {
                    path: path.clone(),
                    expected: spec.placeholder(),
                }),
                NodeKind::Container => Err(SetError::NotSettable { path: path.clone() }),
                // A bare tag node with no key: `set interfaces ethernet`.
                NodeKind::Tag(tag) => Err(SetError::ValueRequired {
                    path: path.clone(),
                    expected: tag.value.placeholder(),
                }),
            },
            // A tag instance. `set interfaces ethernet eth0` creates it; a
            // value after it would be a child that does not exist.
            (Location::Instance(_), None) => Ok(()),
            (Location::Instance(_), Some(value)) => Err(SetError::UnknownPath {
                path: path.child(value),
            }),
            (Location::Value(_), _) => Err(SetError::UnknownPath { path: path.clone() }),
        }
    }

    /// Structure and types over a whole config.
    ///
    /// Every violation is reported rather than the first, because an operator
    /// loading a hand-edited file wants the list, not one error per attempt.
    pub fn validate_tree(&self, config: &ConfigTree) -> Vec<ConstraintViolation> {
        let mut out = Vec::new();
        check_types(
            Location::Node(&self.root),
            config.root(),
            &Path::root(),
            &mut out,
        );
        out
    }

    /// Everything that involves more than one node.
    pub fn check_constraints(&self, config: &ConfigTree) -> Vec<ConstraintViolation> {
        let mut out = Vec::new();
        check_cross(
            Location::Node(&self.root),
            config.root(),
            &Path::root(),
            None,
            config,
            &mut out,
        );
        for global in &self.globals {
            check_global(global, config, &mut out);
        }
        out.sort_by(|a, b| a.path.cmp(&b.path).then_with(|| a.message.cmp(&b.message)));
        out.dedup();
        out
    }
}

fn collect_defaults(node: &SchemaNode, at: &Path, tree: &mut ConfigTree) {
    match &node.kind {
        NodeKind::Container => {
            for (name, child) in &node.children {
                collect_defaults(child, &at.child(name), tree);
            }
        }
        NodeKind::Tag(_) | NodeKind::Flag => {}
        NodeKind::Leaf(_) | NodeKind::MultiLeaf(_) => {
            if let Some(default) = &node.default {
                // Cannot fail: every path here is fresh and interior above.
                let _ = tree.set(at, default.clone());
            }
        }
    }
}

// ---------------------------------------------------------------------------
// structure and types
// ---------------------------------------------------------------------------

fn check_types(
    location: Location<'_>,
    config: &Node,
    at: &Path,
    out: &mut Vec<ConstraintViolation>,
) {
    let (node, in_instance) = match location {
        Location::Node(node) => (node, false),
        Location::Instance(node) => (node, true),
        Location::Value(_) => return,
    };

    if in_instance {
        descend(&node.children, config, at, out, |_| true);
        return;
    }

    match &node.kind {
        NodeKind::Container => descend(&node.children, config, at, out, |_| true),

        NodeKind::Tag(tag) => {
            let Some(instances) = require_interior(config, at, out) else {
                return;
            };
            for (key, instance) in instances {
                let path = at.child(key);
                if let Err(e) = tag.value.check(key) {
                    out.push(ConstraintViolation::new(path.clone(), e.to_string()));
                    continue;
                }
                check_types(Location::Instance(node), instance, &path, out);
            }
        }

        NodeKind::Leaf(spec) | NodeKind::MultiLeaf(spec) => {
            let Some(values) = config.value_set() else {
                out.push(ConstraintViolation::new(
                    at.clone(),
                    "takes a value, but has child nodes",
                ));
                return;
            };
            let multi = matches!(node.kind, NodeKind::MultiLeaf(_));
            if values.is_empty() {
                out.push(ConstraintViolation::new(
                    at.clone(),
                    format!("needs a value: {}", spec.placeholder()),
                ));
            } else if !multi && values.len() > 1 {
                out.push(ConstraintViolation::new(
                    at.clone(),
                    format!(
                        "takes one value but has {}: {}",
                        values.len(),
                        values.iter().cloned().collect::<Vec<_>>().join(", ")
                    ),
                ));
            }
            for value in values {
                if let Err(e) = spec.check(value) {
                    out.push(ConstraintViolation::new(at.clone(), e.to_string()));
                }
            }
        }

        NodeKind::Flag => match config.value_set() {
            Some(values) if values.is_empty() => {}
            Some(values) => out.push(ConstraintViolation::new(
                at.clone(),
                format!(
                    "takes no value, but was given {}",
                    values.iter().cloned().collect::<Vec<_>>().join(", ")
                ),
            )),
            None => out.push(ConstraintViolation::new(
                at.clone(),
                "takes no value and has no settings under it",
            )),
        },
    }
}

fn descend(
    schema_children: &BTreeMap<String, SchemaNode>,
    config: &Node,
    at: &Path,
    out: &mut Vec<ConstraintViolation>,
    accept: impl Fn(&str) -> bool,
) {
    let Some(children) = require_interior(config, at, out) else {
        return;
    };
    for (name, child) in children {
        let path = at.child(name);
        match schema_children.get(name) {
            Some(schema_child) if accept(name) => {
                check_types(Location::Node(schema_child), child, &path, out);
            }
            _ => out.push(ConstraintViolation::new(
                path,
                "is not a configuration path",
            )),
        }
    }
}

fn require_interior<'a>(
    config: &'a Node,
    at: &Path,
    out: &mut Vec<ConstraintViolation>,
) -> Option<&'a BTreeMap<String, Node>> {
    match config.children() {
        Some(children) => Some(children),
        None => {
            out.push(ConstraintViolation::new(
                at.clone(),
                "groups other settings, so it cannot hold a value",
            ));
            None
        }
    }
}

// ---------------------------------------------------------------------------
// cross-node constraints
// ---------------------------------------------------------------------------

fn check_cross(
    location: Location<'_>,
    config: &Node,
    at: &Path,
    current: Option<&str>,
    tree: &ConfigTree,
    out: &mut Vec<ConstraintViolation>,
) {
    let node = match location {
        Location::Node(node) | Location::Instance(node) => node,
        Location::Value(_) => return,
    };

    for constraint in &node.constraints {
        check_constraint(constraint, config, at, current, tree, out);
    }

    let Some(children) = config.children() else {
        return;
    };

    match location {
        // A tag node: its own rules are about its instances taken together.
        Location::Node(tag_node) if tag_node.is_tag() => {
            check_unique(tag_node, children, at, out);
            for (key, instance) in children {
                check_cross(
                    Location::Instance(tag_node),
                    instance,
                    &at.child(key),
                    Some(key),
                    tree,
                    out,
                );
            }
        }

        // A container, or one instance of a tag node: `required` and
        // `mutually-exclusive` are about the children present here.
        _ => {
            check_required(node, children, at, out);
            check_exclusive(node, children, at, out);
            for (name, child) in children {
                if let Some(schema_child) = node.children.get(name) {
                    check_cross(
                        Location::Node(schema_child),
                        child,
                        &at.child(name),
                        current,
                        tree,
                        out,
                    );
                }
            }
        }
    }
}

fn check_constraint(
    constraint: &Constraint,
    config: &Node,
    at: &Path,
    current: Option<&str>,
    tree: &ConfigTree,
    out: &mut Vec<ConstraintViolation>,
) {
    match constraint {
        Constraint::ValueInPathSet { paths, message } => {
            let Some(values) = config.value_set() else {
                return;
            };
            let allowed: Vec<String> = paths
                .iter()
                .flat_map(|p| values_at(tree, p, current))
                .collect();
            for value in values {
                if !allowed.contains(value) {
                    out.push(ConstraintViolation::new(
                        at.clone(),
                        fill(message, &[("value", value), ("path", &at.to_string())]),
                    ));
                }
            }
        }

        Constraint::PathExists { path, message } => {
            if expand(tree, path, current).is_empty() {
                out.push(ConstraintViolation::new(
                    at.clone(),
                    fill(message, &[("path", &path.to_string())]),
                ));
            }
        }

        Constraint::PathHasValue {
            path,
            value,
            message,
        } => {
            let found = expand(tree, path, current)
                .iter()
                .any(|p| tree.values_at(p).is_some_and(|v| v.contains(value)));
            if !found {
                out.push(ConstraintViolation::new(
                    at.clone(),
                    fill(message, &[("path", &path.to_string()), ("value", value)]),
                ));
            }
        }
    }
}

fn check_required(
    node: &SchemaNode,
    children: &BTreeMap<String, Node>,
    at: &Path,
    out: &mut Vec<ConstraintViolation>,
) {
    for (name, schema_child) in &node.children {
        if schema_child.required && !children.contains_key(name) {
            out.push(ConstraintViolation::new(
                at.child(name),
                "is required but is not set",
            ));
        }
    }
}

fn check_exclusive(
    node: &SchemaNode,
    children: &BTreeMap<String, Node>,
    at: &Path,
    out: &mut Vec<ConstraintViolation>,
) {
    for group in &node.exclusive {
        let set: Vec<&String> = group.iter().filter(|n| children.contains_key(*n)).collect();
        if set.len() > 1 {
            let names: Vec<&str> = set.iter().map(|s| s.as_str()).collect();
            out.push(ConstraintViolation::new(
                at.clone(),
                format!("{} cannot both be set", names.join(" and ")),
            ));
        }
    }
}

/// Tuples that must be unique across a tag node's instances.
fn check_unique(
    tag_node: &SchemaNode,
    instances: &BTreeMap<String, Node>,
    at: &Path,
    out: &mut Vec<ConstraintViolation>,
) {
    for tuple in &tag_node.unique {
        let mut seen: BTreeMap<Vec<String>, String> = BTreeMap::new();
        for (key, instance) in instances {
            // An instance missing one of the components cannot collide with
            // anything. `required` is what reports that it is missing; saying
            // so twice helps nobody.
            let Some(values) = tuple
                .iter()
                .map(|name| {
                    instance
                        .children()?
                        .get(name)?
                        .value()
                        .map(str::to_string)
                })
                .collect::<Option<Vec<_>>>()
            else {
                continue;
            };
            if let Some(other) = seen.get(&values) {
                out.push(ConstraintViolation::new(
                    at.child(key),
                    format!(
                        "has the same {} as {other} ({})",
                        tuple.join(" and "),
                        values.join(", ")
                    ),
                ));
            } else {
                seen.insert(values, key.clone());
            }
        }
    }
}

// ---------------------------------------------------------------------------
// global constraints
// ---------------------------------------------------------------------------

fn check_global(
    global: &GlobalConstraint,
    tree: &ConfigTree,
    out: &mut Vec<ConstraintViolation>,
) {
    match global {
        GlobalConstraint::UniqueAcross { paths, message } => {
            // value -> the paths that named it
            let mut claims: BTreeMap<String, Vec<Path>> = BTreeMap::new();
            for pattern in paths {
                for path in expand(tree, pattern, None) {
                    for value in contributed(tree, &path) {
                        claims.entry(value).or_default().push(path.clone());
                    }
                }
            }
            for (value, mut referrers) in claims {
                if referrers.len() < 2 {
                    continue;
                }
                referrers.sort();
                let names: Vec<String> = referrers.iter().map(owner_name).collect();
                for referrer in &referrers {
                    out.push(ConstraintViolation::new(
                        referrer.clone(),
                        fill(
                            message,
                            &[("value", &value), ("referrers", &names.join(", "))],
                        ),
                    ));
                }
            }
        }

        GlobalConstraint::ForbidChildOnReferenced {
            references,
            search,
            forbid,
            message,
        } => {
            for pattern in references {
                for referrer in expand(tree, pattern, None) {
                    let owner = owner_name(&referrer);
                    for value in contributed(tree, &referrer) {
                        for base in search {
                            for base_path in expand(tree, base, None) {
                                let target = base_path.child(&value).child(forbid);
                                if tree.contains(&target) {
                                    out.push(ConstraintViolation::new(
                                        target,
                                        fill(
                                            message,
                                            &[
                                                ("value", &value),
                                                ("referrer", &owner),
                                                ("referrer-path", &referrer.to_string()),
                                            ],
                                        ),
                                    ));
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// path patterns against a config
// ---------------------------------------------------------------------------

/// Expand a pattern into the concrete paths that exist in `tree`.
///
/// `current` binds `@`, and is the key of the tag instance the constraint is
/// being checked inside.
fn expand(tree: &ConfigTree, pattern: &PathPattern, current: Option<&str>) -> Vec<Path> {
    let mut here = vec![Path::root()];
    for segment in &pattern.segments {
        let mut next = Vec::new();
        for path in &here {
            match segment {
                PatternSegment::Literal(name) => {
                    let child = path.child(name);
                    if tree.contains(&child) {
                        next.push(child);
                    }
                }
                PatternSegment::Current => {
                    let Some(current) = current else { continue };
                    let child = path.child(current);
                    if tree.contains(&child) {
                        next.push(child);
                    }
                }
                PatternSegment::Any => {
                    if let Some(node) = tree.get(path)
                        && let Some(children) = node.children()
                    {
                        next.extend(children.keys().map(|k| path.child(k)));
                    }
                }
            }
        }
        here = next;
        if here.is_empty() {
            break;
        }
    }
    here
}

/// What a concrete path contributes to a value set.
///
/// A leaf contributes its values; anything else contributes its own last
/// segment. That is what makes `interfaces ethernet *` (a set of interface
/// names) and `interfaces bonding @ member` (a set of interface names)
/// comparable without the pattern having to say which it is.
fn contributed(tree: &ConfigTree, path: &Path) -> Vec<String> {
    match tree.get(path).and_then(Node::value_set) {
        Some(values) => values.iter().cloned().collect(),
        None => path.last().map(str::to_string).into_iter().collect(),
    }
}

fn values_at(tree: &ConfigTree, pattern: &PathPattern, current: Option<&str>) -> Vec<String> {
    expand(tree, pattern, current)
        .iter()
        .flat_map(|p| contributed(tree, p))
        .collect()
}

/// The instance a referring leaf belongs to: `interfaces bonding bond0 member`
/// is owned by `bond0`. Used in messages, where the bare name reads far better
/// than the full path.
fn owner_name(path: &Path) -> String {
    path.parent()
        .and_then(|p| p.last().map(str::to_string))
        .unwrap_or_else(|| path.to_string())
}

/// `{name}` substitution in schema-authored messages.
fn fill(template: &str, pairs: &[(&str, &str)]) -> String {
    let mut out = template.to_string();
    for (name, value) in pairs {
        out = out.replace(&format!("{{{name}}}"), value);
    }
    out
}
