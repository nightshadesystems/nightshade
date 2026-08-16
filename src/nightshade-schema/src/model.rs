//! What the schema *is*, once the YAML has been read.
//!
//! Plain data with no behaviour and no dependency on the rest of the crate,
//! for two reasons. It is the type the generated schema will be built as, so
//! `build.rs` has to be able to compile this file on its own. And keeping the
//! rules as data rather than as code is what lets `schema/` be the only place
//! a config node is defined -- the moment a node's meaning lives in a `match`
//! arm somewhere, the YAML becomes documentation.

use std::collections::BTreeMap;
use std::fmt;

use crate::path::Path;
use crate::value::ValueSpec;

/// What a node is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NodeKind {
    /// Fixed named children. `system`, `interfaces`.
    Container,
    /// Keyed instances. `ethernet`, whose instances are `eth0`, `eth1`; the
    /// node's `children` describe one instance.
    Tag(TagSpec),
    /// Exactly one value.
    Leaf(ValueSpec),
    /// Any number of values. `address`, `name-server`, `member`.
    MultiLeaf(ValueSpec),
    /// Present or absent, no value. `disable`, `stp`.
    Flag,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TagSpec {
    /// What an instance key may be. `interface-name`, narrowed by a pattern
    /// for the types whose names are conventional (`vlanN`, `bondN`).
    pub value: ValueSpec,
    pub help: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchemaNode {
    pub kind: NodeKind,
    pub help: String,
    pub default: Option<String>,
    /// Masked as `****` by `show` unless root asks for `| display secrets`.
    pub secret: bool,
    /// If the enclosing tag instance exists, this leaf must be set. A vlan
    /// without a `parent` is not an under-specified vlan, it is not a vlan.
    pub required: bool,
    /// Apply ordering, lower first. A bond has to exist before the vlan on top
    /// of it does.
    pub priority: u32,
    pub children: BTreeMap<String, SchemaNode>,
    pub constraints: Vec<Constraint>,
    /// Tuples of child names that must be unique across this tag node's
    /// instances. `[[parent, id]]` on `vlan`; `[[vni]]` on `vxlan`.
    pub unique: Vec<Vec<String>>,
    /// Sets of children of which at most one may be set. `[[remote, group]]`.
    pub exclusive: Vec<Vec<String>>,
}

impl SchemaNode {
    pub fn value_spec(&self) -> Option<&ValueSpec> {
        match &self.kind {
            NodeKind::Leaf(spec) | NodeKind::MultiLeaf(spec) => Some(spec),
            _ => None,
        }
    }

    pub fn is_tag(&self) -> bool {
        matches!(self.kind, NodeKind::Tag(_))
    }

    pub fn takes_children(&self) -> bool {
        matches!(self.kind, NodeKind::Container | NodeKind::Tag(_))
    }
}

// ---------------------------------------------------------------------------
// path patterns
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PatternSegment {
    Literal(String),
    /// `*` -- every instance key, or every value.
    Any,
    /// `@` -- the key of the tag instance the constraint is being checked in.
    /// `interfaces bonding @ member` means "this bond's members".
    Current,
}

/// An absolute path with wildcards, used by constraints to name other parts of
/// the config.
///
/// Absolute always, and only two wildcards. A relative form would need a rule
/// for what it is relative to, and a third wildcard would be the start of an
/// expression language -- at which point a schema author can write a
/// constraint nobody can explain to the operator it fires at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathPattern {
    pub segments: Vec<PatternSegment>,
}

impl PathPattern {
    /// Split on whitespace, not through the config lexer.
    ///
    /// A pattern is written by a schema author in YAML, not typed by an
    /// operator, and it uses two characters -- `*` and `@` -- that the config
    /// lexer refuses on purpose so that `/*` can only ever open a comment.
    /// Running these through it would mean either weakening that rule for
    /// every config file, or quoting every wildcard in the schema.
    ///
    /// Segments are checked against what a schema node name can be, so a
    /// mistyped pattern fails the load rather than silently matching nothing.
    pub fn parse(s: &str) -> Result<Self, String> {
        let mut segments = Vec::new();
        for token in s.split_whitespace() {
            segments.push(match token {
                "*" => PatternSegment::Any,
                "@" => PatternSegment::Current,
                literal => {
                    if !literal
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
                    {
                        return Err(format!("`{literal}` is not a node name"));
                    }
                    PatternSegment::Literal(literal.to_string())
                }
            });
        }
        if segments.is_empty() {
            return Err("a path pattern cannot be empty".into());
        }
        Ok(Self { segments })
    }

    /// The literal prefix, for messages: `interfaces bonding * member` reads
    /// better in an error as `interfaces bonding`.
    pub fn literal_prefix(&self) -> Path {
        Path::from_segments(self.segments.iter().map_while(|s| match s {
            PatternSegment::Literal(l) => Some(l.clone()),
            _ => None,
        }))
    }
}

impl fmt::Display for PathPattern {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, segment) in self.segments.iter().enumerate() {
            if i > 0 {
                f.write_str(" ")?;
            }
            match segment {
                PatternSegment::Literal(l) => f.write_str(l)?,
                PatternSegment::Any => f.write_str("*")?,
                PatternSegment::Current => f.write_str("@")?,
            }
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// constraints
// ---------------------------------------------------------------------------

/// A rule attached to one node, checked when that node is set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Constraint {
    /// This node's value must appear among the values found at these
    /// patterns. How every interface cross-reference is expressed: a vlan's
    /// `parent` must be in `interfaces ethernet *`, a bond's `primary` must be
    /// in `interfaces bonding @ member`.
    ValueInPathSet {
        paths: Vec<PathPattern>,
        message: String,
    },
    /// If this node is set, something must exist at this path. `group`
    /// requires `source-interface`, because a multicast VXLAN has to know
    /// which interface to join the group on.
    PathExists { path: PathPattern, message: String },
    /// If this node is set, the node at this path must hold this value.
    /// `primary` is only meaningful when `mode` is `active-backup`.
    PathHasValue {
        path: PathPattern,
        value: String,
        message: String,
    },
}

/// A rule about the config as a whole, checked once per commit.
///
/// These exist because two of the required interface rules are not properties
/// of any single node. "An interface cannot be a member of two masters" is a
/// statement about the union of two multi-leaves in different subtrees; there
/// is no node to hang it on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GlobalConstraint {
    /// The values found across all these patterns, taken together, must have
    /// no duplicates.
    UniqueAcross {
        paths: Vec<PathPattern>,
        message: String,
    },
    /// Anything named by a value at `references` must not have `forbid` set.
    /// The referent is looked for under each path in `search`.
    ///
    /// This is the enslaved-interface rule: an interface that is a member of a
    /// bond or a bridge must not carry an address of its own, because the
    /// address belongs on the master.
    ForbidChildOnReferenced {
        references: Vec<PathPattern>,
        search: Vec<PathPattern>,
        forbid: String,
        message: String,
    },
}

// ---------------------------------------------------------------------------
// the schema
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Schema {
    /// An implicit container. Its children are `system`, `interfaces`.
    pub root: SchemaNode,
    pub globals: Vec<GlobalConstraint>,
}

/// Where a path lands in the schema.
///
/// Three cases rather than one, because "the node at this path" is ambiguous
/// for a tag node: `interfaces ethernet` is the tag node, and
/// `interfaces ethernet eth0` is an instance of it. They take different things
/// next, so completion and validation both need to tell them apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Location<'a> {
    /// The node itself.
    Node(&'a SchemaNode),
    /// One instance of a tag node. The tag node's children come next.
    Instance(&'a SchemaNode),
    /// A value of a leaf. Nothing comes after it.
    Value(&'a SchemaNode),
}

impl Schema {
    /// Resolve a path structurally.
    ///
    /// Structurally: any segment in a tag-value position resolves, whether or
    /// not it is a legal instance key. That is deliberate. It lets validation
    /// answer with `"eth 0" is not a valid interface name` instead of
    /// `unknown path`, which is the difference between an operator fixing a
    /// typo and an operator wondering whether the feature exists.
    pub fn resolve(&self, path: &Path) -> Option<Location<'_>> {
        let mut location = Location::Node(&self.root);
        for segment in path.segments() {
            location = match location {
                Location::Node(node) => match &node.kind {
                    NodeKind::Container => Location::Node(node.children.get(segment)?),
                    NodeKind::Tag(_) => Location::Instance(node),
                    NodeKind::Leaf(_) | NodeKind::MultiLeaf(_) => Location::Value(node),
                    NodeKind::Flag => return None,
                },
                Location::Instance(tag) => Location::Node(tag.children.get(segment)?),
                Location::Value(_) => return None,
            };
        }
        Some(location)
    }

    /// The schema node a path names, ignoring the tag-node/instance
    /// distinction.
    pub fn node_at(&self, path: &Path) -> Option<&SchemaNode> {
        match self.resolve(path)? {
            Location::Node(node) | Location::Instance(node) | Location::Value(node) => Some(node),
        }
    }

    /// Whether a path names a tag node, so the renderer knows to write
    /// `ethernet eth0 { ... }` rather than nesting.
    pub fn is_tag_node(&self, path: &Path) -> bool {
        matches!(self.resolve(path), Some(Location::Node(node)) if node.is_tag())
    }

    /// The schema-declared default at a path, if it has one.
    pub fn default_for(&self, path: &Path) -> Option<&str> {
        match self.resolve(path)? {
            Location::Node(node) => node.default.as_deref(),
            _ => None,
        }
    }

    /// What can be typed next after `path`.
    ///
    /// Drives tab completion and `?`. The CLI is allowed to call this: it
    /// returns metadata, not judgement, and a completion list that had to make
    /// a round trip for every keystroke would be a completion list nobody
    /// waits for.
    ///
    /// Where the operator invents the next word -- a tag key, or a leaf's
    /// value -- the result is one entry with `placeholder` set, holding the
    /// `<interface>`-style hint. Real instance keys come from the candidate
    /// config, which the schema does not have and the CLI does.
    pub fn children_of(&self, path: &Path) -> Vec<NodeInfo> {
        let Some(location) = self.resolve(path) else {
            return Vec::new();
        };
        match location {
            Location::Node(node) => match &node.kind {
                NodeKind::Container => node.children.iter().map(|(n, c)| info(n, c)).collect(),
                NodeKind::Tag(tag) => vec![NodeInfo {
                    name: tag.value.placeholder(),
                    help: tag.help.clone(),
                    placeholder: true,
                    value: None,
                    multi: false,
                    default: None,
                    secret: false,
                }],
                NodeKind::Leaf(spec) | NodeKind::MultiLeaf(spec) => vec![NodeInfo {
                    name: spec.placeholder(),
                    help: node.help.clone(),
                    placeholder: true,
                    value: None,
                    multi: matches!(node.kind, NodeKind::MultiLeaf(_)),
                    default: node.default.clone(),
                    secret: node.secret,
                }],
                NodeKind::Flag => Vec::new(),
            },
            Location::Instance(tag) => tag.children.iter().map(|(n, c)| info(n, c)).collect(),
            Location::Value(_) => Vec::new(),
        }
    }
}

fn info(name: &str, node: &SchemaNode) -> NodeInfo {
    NodeInfo {
        name: name.to_string(),
        help: node.help.clone(),
        placeholder: false,
        value: match &node.kind {
            NodeKind::Leaf(spec) | NodeKind::MultiLeaf(spec) => Some(spec.placeholder()),
            NodeKind::Tag(tag) => Some(tag.value.placeholder()),
            NodeKind::Container | NodeKind::Flag => None,
        },
        multi: matches!(node.kind, NodeKind::MultiLeaf(_)),
        default: node.default.clone(),
        secret: node.secret,
    }
}

/// One completion candidate, with everything the CLI needs to draw a `?` help
/// line for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeInfo {
    /// Something to type, or -- when `placeholder` is set -- a description of
    /// what the operator invents here, like `<interface>`.
    pub name: String,
    pub help: String,
    pub placeholder: bool,
    /// What follows the name, if anything.
    pub value: Option<String>,
    /// Whether it may be given more than once.
    pub multi: bool,
    pub default: Option<String>,
    pub secret: bool,
}
