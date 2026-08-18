//! Reading `schema/`.
//!
//! Every `.yaml` under the schema directory is parsed, the whole set is
//! deep-merged into one document, templates are expanded, and the result is
//! converted into the [`Schema`](crate::model::Schema) model. Files are
//! visited in sorted order, so the merge is a function of the directory's
//! contents and not of the order the filesystem happened to return them in.
//!
//! # File shape
//!
//! ```yaml
//! templates:
//!   interface-common:
//!     address: { kind: multi-leaf, type: ip-prefix, accepts: [dhcp], help: "..." }
//!
//! nodes:
//!   interfaces:
//!     kind: container
//!     help: "Network interfaces"
//!     children:
//!       ethernet:
//!         kind: tag
//!         tag: { type: interface-name, help: "Interface name" }
//!         include: [interface-common]
//!         children:
//!           speed: { kind: leaf, type: enum, values: [auto, 10, 100], help: "..." }
//!
//! global-constraints:
//!   - unique-across: { paths: [...], message: "..." }
//! ```
//!
//! An envelope with named sections rather than nodes at the top level, so a
//! config node called `templates` stays possible.
//!
//! # Merging
//!
//! Definition fields -- `kind`, `type`, `help`, `default` and the rest -- may
//! be set once. Two files setting the same one is an error, not a
//! last-one-wins: the whole point of splitting the schema across files is that
//! each file owns a subtree, and silently overwriting is how one file starts
//! quietly changing another.
//!
//! Additive fields -- `constraints`, `unique`, `mutually-exclusive` -- collect
//! from every file that mentions them, and `children` merge recursively.
//!
//! Template expansion is the one place where overwriting is intended:
//! `include` pulls a template's children in, and the node's own `children`
//! then overlay them, so `address: { accepts: [] }` narrows the common
//! `address` for loopback without restating it.
//!
//! # Strictness
//!
//! `deny_unknown_fields` throughout. A mistyped `helo:` that is ignored is a
//! node shipped with no help text, discovered by an operator pressing `?` in
//! an outage.

use std::collections::BTreeMap;
use std::path::{Path as FsPath, PathBuf};

use serde::Deserialize;

use crate::model::{
    Constraint, GlobalConstraint, NodeKind, PathPattern, Schema, SchemaNode, TagSpec,
};
use crate::value::{Range, ValueSpec, ValueType};

/// Priority a node inherits if neither it nor any ancestor sets one.
const DEFAULT_PRIORITY: u32 = 500;

#[derive(Debug, thiserror::Error)]
pub enum SchemaError {
    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("{path}: {source}")]
    Yaml {
        path: PathBuf,
        #[source]
        source: serde_yaml_ng::Error,
    },

    #[error("schema node `{at}`: `{field}` is defined twice with different values")]
    Conflict { at: String, field: &'static str },

    #[error("schema node `{at}`: {message}")]
    Invalid { at: String, message: String },
}

impl SchemaError {
    fn invalid(at: &str, message: impl Into<String>) -> Self {
        Self::Invalid {
            at: at.to_string(),
            message: message.into(),
        }
    }
}

/// `schema/` in this source tree.
///
/// Tests and tooling. The shipped binaries carry the schema compiled in and
/// never look at the filesystem for it; this exists so there is one definition
/// of where the directory is rather than a relative path in every test.
pub fn source_dir() -> PathBuf {
    FsPath::new(env!("CARGO_MANIFEST_DIR")).join("../../schema")
}

/// Load and merge every schema file under `dir`.
pub fn load_dir(dir: &FsPath) -> Result<Schema, SchemaError> {
    let mut files = Vec::new();
    collect_files(dir, &mut files)?;
    files.sort();

    let mut merged = RawFile::default();
    for file in &files {
        let text = std::fs::read_to_string(file).map_err(|source| SchemaError::Io {
            path: file.clone(),
            source,
        })?;
        let parsed: RawFile =
            serde_yaml_ng::from_str(&text).map_err(|source| SchemaError::Yaml {
                path: file.clone(),
                source,
            })?;
        merged.absorb(parsed)?;
    }
    build(merged)
}

/// Load and merge schema files given as `(name, text)` pairs.
///
/// What the tests use, and what `build.rs` will use once the schema is
/// compiled in, so neither has to go through the filesystem to exercise
/// exactly the same merge and conversion.
pub fn load_sources(sources: &[(&str, &str)]) -> Result<Schema, SchemaError> {
    let mut ordered: Vec<_> = sources.iter().collect();
    ordered.sort_by_key(|(name, _)| *name);

    let mut merged = RawFile::default();
    for (name, text) in ordered {
        let parsed: RawFile =
            serde_yaml_ng::from_str(text).map_err(|source| SchemaError::Yaml {
                path: PathBuf::from(name),
                source,
            })?;
        merged.absorb(parsed)?;
    }
    build(merged)
}

fn collect_files(dir: &FsPath, out: &mut Vec<PathBuf>) -> Result<(), SchemaError> {
    let entries = std::fs::read_dir(dir).map_err(|source| SchemaError::Io {
        path: dir.to_path_buf(),
        source,
    })?;
    for entry in entries {
        let entry = entry.map_err(|source| SchemaError::Io {
            path: dir.to_path_buf(),
            source,
        })?;
        let path = entry.path();
        if path.is_dir() {
            collect_files(&path, out)?;
        } else if matches!(path.extension().and_then(|e| e.to_str()), Some("yaml" | "yml")) {
            out.push(path);
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// the YAML shape
// ---------------------------------------------------------------------------

/// A YAML scalar, as written.
///
/// `default: 1500` and `default: "1500"` mean the same thing, and so do
/// `values: [10, 100]` and `values: ["10", "100"]`. Config values are strings
/// in the tree, so they become strings here; requiring an author to quote
/// every number in an enum of link speeds would be a rule with no purpose
/// except to be forgotten.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(untagged)]
enum Scalar {
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
}

impl Scalar {
    fn into_string(self) -> String {
        match self {
            Scalar::Bool(b) => b.to_string(),
            Scalar::Int(i) => i.to_string(),
            Scalar::Float(f) => f.to_string(),
            Scalar::Str(s) => s,
        }
    }
}

fn strings(values: Option<Vec<Scalar>>) -> Option<Vec<String>> {
    values.map(|v| v.into_iter().map(Scalar::into_string).collect())
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
struct RawFile {
    #[serde(default)]
    templates: BTreeMap<String, BTreeMap<String, RawNode>>,
    #[serde(default)]
    nodes: BTreeMap<String, RawNode>,
    #[serde(default)]
    global_constraints: Vec<RawGlobal>,
}

impl RawFile {
    fn absorb(&mut self, other: RawFile) -> Result<(), SchemaError> {
        for (name, children) in other.templates {
            match self.templates.entry(name.clone()) {
                std::collections::btree_map::Entry::Vacant(slot) => {
                    slot.insert(children);
                }
                std::collections::btree_map::Entry::Occupied(_) => {
                    return Err(SchemaError::Conflict {
                        at: format!("templates {name}"),
                        field: "template",
                    });
                }
            }
        }
        merge_children(&mut self.nodes, other.nodes, "", Mode::Strict)?;
        self.global_constraints.extend(other.global_constraints);
        Ok(())
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
struct RawNode {
    kind: Option<String>,
    help: Option<String>,
    #[serde(rename = "type")]
    ty: Option<String>,
    range: Option<RawRange>,
    values: Option<Vec<Scalar>>,
    regex: Option<String>,
    accepts: Option<Vec<Scalar>>,
    default: Option<Scalar>,
    secret: Option<bool>,
    required: Option<bool>,
    priority: Option<u32>,
    tag: Option<RawTag>,
    include: Option<Vec<String>>,
    exclude: Option<Vec<String>>,
    children: Option<BTreeMap<String, RawNode>>,
    constraints: Option<Vec<RawConstraint>>,
    unique: Option<Vec<Vec<String>>>,
    mutually_exclusive: Option<Vec<Vec<String>>>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRange {
    min: i64,
    max: i64,
    step: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
struct RawTag {
    #[serde(rename = "type")]
    ty: String,
    help: String,
    range: Option<RawRange>,
    values: Option<Vec<Scalar>>,
    regex: Option<String>,
}

/// Tagged by a `rule:` key rather than by the variant name being the only key.
///
/// ```yaml
/// constraints:
///   - rule: value-in-path-set
///     paths: ["interfaces ethernet *"]
///     message: "..."
/// ```
///
/// An externally tagged enum would need YAML's `!value-in-path-set` tag
/// syntax, which almost nobody writes by hand and which reads as though
/// something clever is happening.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "rule", rename_all = "kebab-case", deny_unknown_fields)]
enum RawConstraint {
    ValueInPathSet { paths: Vec<String>, message: String },
    PathExists { path: String, message: String },
    PathHasValue {
        path: String,
        value: Scalar,
        message: String,
    },
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "rule", rename_all = "kebab-case", deny_unknown_fields)]
enum RawGlobal {
    UniqueAcross { paths: Vec<String>, message: String },
    ForbidChildOnReferenced {
        references: Vec<String>,
        search: Vec<String>,
        forbid: String,
        message: String,
    },
}

// ---------------------------------------------------------------------------
// merging
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    /// Two files defining the same field is a mistake.
    Strict,
    /// A node's own children deliberately narrowing an included template.
    Overlay,
}

fn merge_children(
    into: &mut BTreeMap<String, RawNode>,
    from: BTreeMap<String, RawNode>,
    at: &str,
    mode: Mode,
) -> Result<(), SchemaError> {
    for (name, node) in from {
        let child_at = if at.is_empty() {
            name.clone()
        } else {
            format!("{at} {name}")
        };
        match into.get_mut(&name) {
            Some(existing) => merge_node(existing, node, &child_at, mode)?,
            None => {
                into.insert(name, node);
            }
        }
    }
    Ok(())
}

fn merge_node(into: &mut RawNode, from: RawNode, at: &str, mode: Mode) -> Result<(), SchemaError> {
    macro_rules! definition {
        ($($field:ident),* $(,)?) => {$(
            if let Some(value) = from.$field {
                match &into.$field {
                    Some(existing) if mode == Mode::Strict && *existing != value => {
                        return Err(SchemaError::Conflict { at: at.to_string(), field: stringify!($field) });
                    }
                    _ => into.$field = Some(value),
                }
            }
        )*};
    }
    definition!(
        kind, help, ty, range, values, regex, accepts, default, secret, required, priority, tag
    );

    macro_rules! additive {
        ($($field:ident),* $(,)?) => {$(
            if let Some(value) = from.$field {
                into.$field.get_or_insert_default().extend(value);
            }
        )*};
    }
    additive!(include, exclude, constraints, unique, mutually_exclusive);

    if let Some(children) = from.children {
        merge_children(
            into.children.get_or_insert_default(),
            children,
            at,
            mode,
        )?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// conversion
// ---------------------------------------------------------------------------

fn build(raw: RawFile) -> Result<Schema, SchemaError> {
    let RawFile {
        templates,
        nodes,
        global_constraints,
    } = raw;

    let mut children = BTreeMap::new();
    for (name, node) in nodes {
        children.insert(
            name.clone(),
            convert(node, &name, DEFAULT_PRIORITY, &templates)?,
        );
    }

    let globals = global_constraints
        .into_iter()
        .map(|g| global(g, "global-constraints"))
        .collect::<Result<_, _>>()?;

    Ok(Schema {
        root: SchemaNode {
            kind: NodeKind::Container,
            help: "Nightshade configuration".into(),
            default: None,
            secret: false,
            required: false,
            priority: DEFAULT_PRIORITY,
            children,
            constraints: Vec::new(),
            unique: Vec::new(),
            exclusive: Vec::new(),
        },
        globals,
    })
}

fn convert(
    mut raw: RawNode,
    at: &str,
    inherited_priority: u32,
    templates: &BTreeMap<String, BTreeMap<String, RawNode>>,
) -> Result<SchemaNode, SchemaError> {
    // Templates first, so a node's own children overlay what they pulled in.
    if let Some(includes) = raw.include.take() {
        let mut expanded = BTreeMap::new();
        for name in includes {
            let template = templates
                .get(&name)
                .ok_or_else(|| SchemaError::invalid(at, format!("no template named `{name}`")))?;
            merge_children(&mut expanded, template.clone(), at, Mode::Strict)?;
        }
        if let Some(own) = raw.children.take() {
            merge_children(&mut expanded, own, at, Mode::Overlay)?;
        }
        raw.children = Some(expanded);
    }

    if let Some(excluded) = raw.exclude.take() {
        let children = raw.children.get_or_insert_default();
        for name in excluded {
            if children.remove(&name).is_none() {
                return Err(SchemaError::invalid(
                    at,
                    format!("`exclude` names `{name}`, which is not one of its children"),
                ));
            }
        }
    }

    let priority = raw.priority.unwrap_or(inherited_priority);
    let help = raw
        .help
        .clone()
        .ok_or_else(|| SchemaError::invalid(at, "no `help` text"))?;
    let kind_name = raw
        .kind
        .clone()
        .ok_or_else(|| SchemaError::invalid(at, "no `kind`"))?;

    let kind = match kind_name.as_str() {
        "container" => NodeKind::Container,
        "tag" => {
            let tag = raw
                .tag
                .clone()
                .ok_or_else(|| SchemaError::invalid(at, "a tag node needs a `tag` block"))?;
            NodeKind::Tag(TagSpec {
                value: value_spec(
                    &tag.ty,
                    tag.range.clone(),
                    strings(tag.values.clone()),
                    tag.regex.clone(),
                    None,
                    at,
                )?,
                help: tag.help,
            })
        }
        "leaf" => NodeKind::Leaf(leaf_spec(&raw, at)?),
        "multi-leaf" => NodeKind::MultiLeaf(leaf_spec(&raw, at)?),
        "flag" => NodeKind::Flag,
        other => {
            return Err(SchemaError::invalid(
                at,
                format!(
                    "unknown kind `{other}`; expected container, tag, leaf, multi-leaf or flag"
                ),
            ));
        }
    };

    let takes_children = matches!(kind, NodeKind::Container | NodeKind::Tag(_));
    let raw_children = raw.children.take().unwrap_or_default();
    if !takes_children && !raw_children.is_empty() {
        return Err(SchemaError::invalid(
            at,
            format!("a `{kind_name}` cannot have children"),
        ));
    }
    if takes_children && raw_children.is_empty() {
        return Err(SchemaError::invalid(
            at,
            format!("a `{kind_name}` with no children can never be configured"),
        ));
    }

    let mut children = BTreeMap::new();
    for (name, child) in raw_children {
        let child_at = format!("{at} {name}");
        children.insert(name, convert(child, &child_at, priority, templates)?);
    }

    let unique = raw.unique.take().unwrap_or_default();
    let exclusive = raw.mutually_exclusive.take().unwrap_or_default();
    for name in unique.iter().chain(exclusive.iter()).flatten() {
        if !children.contains_key(name) {
            return Err(SchemaError::invalid(
                at,
                format!("`{name}` is named in a rule but is not one of its children"),
            ));
        }
    }
    if !unique.is_empty() && !matches!(kind, NodeKind::Tag(_)) {
        return Err(SchemaError::invalid(
            at,
            "`unique` compares tag instances, so it only means something on a tag node",
        ));
    }

    let constraints = raw
        .constraints
        .take()
        .unwrap_or_default()
        .into_iter()
        .map(|c| constraint(c, at))
        .collect::<Result<_, _>>()?;

    let node = SchemaNode {
        kind,
        help,
        default: raw.default.clone().map(Scalar::into_string),
        secret: raw.secret.unwrap_or(false),
        required: raw.required.unwrap_or(false),
        priority,
        children,
        constraints,
        unique,
        exclusive,
    };

    // A default that the node's own type would reject is a schema bug that
    // would otherwise surface as an unconfigurable box.
    if let (Some(default), Some(spec)) = (&node.default, node.value_spec())
        && let Err(e) = spec.check(default)
    {
        return Err(SchemaError::invalid(at, format!("its default is invalid: {e}")));
    }

    Ok(node)
}

fn leaf_spec(raw: &RawNode, at: &str) -> Result<ValueSpec, SchemaError> {
    let ty = raw
        .ty
        .clone()
        .ok_or_else(|| SchemaError::invalid(at, "a leaf needs a `type`"))?;
    value_spec(
        &ty,
        raw.range.clone(),
        strings(raw.values.clone()),
        raw.regex.clone(),
        strings(raw.accepts.clone()),
        at,
    )
}

fn value_spec(
    ty: &str,
    range: Option<RawRange>,
    values: Option<Vec<String>>,
    regex: Option<String>,
    accepts: Option<Vec<String>>,
    at: &str,
) -> Result<ValueSpec, SchemaError> {
    // Checked before the type is built, so a `range` on a hostname is caught
    // as the mistake it is rather than silently ignored.
    if ty != "enum" && values.is_some() {
        return Err(SchemaError::invalid(at, "`values` only applies to an `enum`"));
    }
    if !matches!(ty, "uint" | "int") && range.is_some() {
        return Err(SchemaError::invalid(at, "`range` only applies to `uint` or `int`"));
    }

    let numeric = |default_min: i64, default_max: i64| -> ValueType {
        let range = range.clone().unwrap_or(RawRange {
            min: default_min,
            max: default_max,
            step: None,
        });
        ValueType::Number(Range {
            min: range.min,
            max: range.max,
            step: range.step,
        })
    };

    let ty = match ty {
        "string" => ValueType::Text,
        "bool" => ValueType::Bool,
        "uint" => numeric(0, u32::MAX as i64),
        "int" => numeric(i32::MIN as i64, i32::MAX as i64),
        "ipv4-address" => ValueType::Ipv4Address,
        "ipv6-address" => ValueType::Ipv6Address,
        "ip-address" => ValueType::IpAddress,
        "ipv4-prefix" => ValueType::Ipv4Prefix,
        "ipv6-prefix" => ValueType::Ipv6Prefix,
        "ip-prefix" => ValueType::IpPrefix,
        "ip-or-prefix" => ValueType::IpOrPrefix,
        "multicast-address" => ValueType::MulticastAddress,
        "mac-address" => ValueType::MacAddress,
        "port" => ValueType::Port,
        "port-range" => ValueType::PortRange,
        "interface-name" => ValueType::InterfaceName,
        "hostname" => ValueType::Hostname,
        "time-zone" => ValueType::TimeZone,
        "user-name" => ValueType::UserName,
        "encrypted-password" => ValueType::EncryptedPassword,
        "enum" => {
            let values = values.ok_or_else(|| {
                SchemaError::invalid(at, "an `enum` needs the `values` it accepts")
            })?;
            if values.is_empty() {
                return Err(SchemaError::invalid(at, "an `enum` with no values accepts nothing"));
            }
            ValueType::Enum(values)
        }
        other => return Err(SchemaError::invalid(at, format!("unknown type `{other}`"))),
    };

    let pattern = regex
        .map(|p| {
            regex::Regex::new(&p)
                .map_err(|e| SchemaError::invalid(at, format!("its `regex` does not compile: {e}")))
        })
        .transpose()?;

    Ok(ValueSpec {
        ty,
        accepts: accepts.unwrap_or_default(),
        pattern,
    })
}

fn pattern(source: &str, at: &str) -> Result<PathPattern, SchemaError> {
    PathPattern::parse(source)
        .map_err(|e| SchemaError::invalid(at, format!("bad path pattern `{source}`: {e}")))
}

fn patterns(sources: Vec<String>, at: &str) -> Result<Vec<PathPattern>, SchemaError> {
    if sources.is_empty() {
        return Err(SchemaError::invalid(at, "a constraint with no paths checks nothing"));
    }
    sources.iter().map(|s| pattern(s, at)).collect()
}

fn constraint(raw: RawConstraint, at: &str) -> Result<Constraint, SchemaError> {
    Ok(match raw {
        RawConstraint::ValueInPathSet { paths, message } => Constraint::ValueInPathSet {
            paths: patterns(paths, at)?,
            message,
        },
        RawConstraint::PathExists { path, message } => Constraint::PathExists {
            path: pattern(&path, at)?,
            message,
        },
        RawConstraint::PathHasValue {
            path,
            value,
            message,
        } => Constraint::PathHasValue {
            path: pattern(&path, at)?,
            value: value.into_string(),
            message,
        },
    })
}

fn global(raw: RawGlobal, at: &str) -> Result<GlobalConstraint, SchemaError> {
    Ok(match raw {
        RawGlobal::UniqueAcross { paths, message } => GlobalConstraint::UniqueAcross {
            paths: patterns(paths, at)?,
            message,
        },
        RawGlobal::ForbidChildOnReferenced {
            references,
            search,
            forbid,
            message,
        } => GlobalConstraint::ForbidChildOnReferenced {
            references: patterns(references, at)?,
            search: patterns(search, at)?,
            forbid,
            message,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn load(yaml: &str) -> Result<Schema, SchemaError> {
        load_sources(&[("test.yaml", yaml)])
    }

    const MINIMAL: &str = r#"
nodes:
  system:
    kind: container
    help: "System parameters"
    children:
      host-name:
        kind: leaf
        type: hostname
        help: "System host name"
        default: nightshade
"#;

    #[test]
    fn loads_a_minimal_schema() {
        let schema = load(MINIMAL).unwrap();
        let system = &schema.root.children["system"];
        assert_eq!(system.kind, NodeKind::Container);
        let host_name = &system.children["host-name"];
        assert_eq!(host_name.default.as_deref(), Some("nightshade"));
        assert!(matches!(host_name.kind, NodeKind::Leaf(_)));
    }

    #[test]
    fn numbers_do_not_have_to_be_quoted() {
        let schema = load(
            r#"
nodes:
  system:
    kind: container
    help: "s"
    children:
      mtu:
        kind: leaf
        type: uint
        range: {min: 68, max: 9216}
        default: 1500
        help: "m"
      speed:
        kind: leaf
        type: enum
        values: [auto, 10, 1000]
        help: "s"
"#,
        )
        .unwrap();
        let system = &schema.root.children["system"];
        assert_eq!(system.children["mtu"].default.as_deref(), Some("1500"));
        assert_eq!(
            system.children["speed"].kind,
            NodeKind::Leaf(ValueSpec::new(ValueType::Enum(vec![
                "auto".into(),
                "10".into(),
                "1000".into()
            ])))
        );
    }

    #[test]
    fn templates_expand_and_can_be_narrowed() {
        let schema = load(
            r#"
templates:
  common:
    address:
      kind: multi-leaf
      type: ip-prefix
      accepts: [dhcp]
      help: "Address"
    mtu:
      kind: leaf
      type: uint
      range: {min: 68, max: 9216}
      help: "MTU"

nodes:
  interfaces:
    kind: container
    help: "Interfaces"
    children:
      ethernet:
        kind: tag
        help: "Ethernet"
        tag: {type: interface-name, help: "Name"}
        include: [common]
        children:
          speed:
            kind: leaf
            type: enum
            values: [auto, 1000]
            help: "Speed"
      loopback:
        kind: tag
        help: "Loopback"
        tag: {type: enum, values: [lo], help: "Name"}
        include: [common]
        exclude: [mtu]
        children:
          address:
            accepts: []
"#,
        )
        .unwrap();

        let interfaces = &schema.root.children["interfaces"];
        let ethernet = &interfaces.children["ethernet"];
        // Template members, plus the node's own.
        assert!(ethernet.children.contains_key("address"));
        assert!(ethernet.children.contains_key("mtu"));
        assert!(ethernet.children.contains_key("speed"));

        let loopback = &interfaces.children["loopback"];
        assert!(!loopback.children.contains_key("mtu"), "exclude did not apply");
        // The overlay narrowed `accepts` without restating the type.
        let address = loopback.children["address"].value_spec().unwrap();
        assert!(address.accepts.is_empty());
        assert_eq!(address.ty, ValueType::IpPrefix);
        // And left ethernet's alone.
        assert_eq!(
            ethernet.children["address"].value_spec().unwrap().accepts,
            ["dhcp"]
        );
    }

    #[test]
    fn separate_files_merge_into_one_tree() {
        let schema = load_sources(&[
            (
                "a.yaml",
                r#"
nodes:
  interfaces:
    kind: container
    help: "Interfaces"
    children:
      ethernet:
        kind: tag
        help: "Ethernet"
        tag: {type: interface-name, help: "Name"}
        children:
          mtu: {kind: leaf, type: uint, help: "MTU"}
"#,
            ),
            (
                "b.yaml",
                r#"
nodes:
  interfaces:
    children:
      loopback:
        kind: tag
        help: "Loopback"
        tag: {type: enum, values: [lo], help: "Name"}
        children:
          description: {kind: leaf, type: string, help: "Description"}
"#,
            ),
        ])
        .unwrap();

        let interfaces = &schema.root.children["interfaces"];
        assert_eq!(interfaces.children.len(), 2);
        assert_eq!(interfaces.help, "Interfaces");
    }

    #[test]
    fn file_order_does_not_change_the_result() {
        let a = (
            "a.yaml",
            "nodes:\n  system:\n    kind: container\n    help: \"S\"\n    children:\n      x: {kind: flag, help: \"X\"}\n",
        );
        let b = (
            "b.yaml",
            "nodes:\n  system:\n    children:\n      y: {kind: flag, help: \"Y\"}\n",
        );
        assert_eq!(
            load_sources(&[a, b]).unwrap(),
            load_sources(&[b, a]).unwrap()
        );
    }

    #[test]
    fn two_files_defining_the_same_field_is_an_error() {
        let err = load_sources(&[
            ("a.yaml", "nodes:\n  system: {kind: container, help: \"One\", children: {x: {kind: flag, help: \"X\"}}}\n"),
            ("b.yaml", "nodes:\n  system: {help: \"Two\"}\n"),
        ])
        .unwrap_err();
        assert!(matches!(err, SchemaError::Conflict { field: "help", .. }), "{err}");
    }

    #[test]
    fn a_mistyped_key_fails_the_load() {
        let err = load(
            "nodes:\n  system:\n    kind: container\n    helo: \"typo\"\n    children:\n      x: {kind: flag, help: \"X\"}\n",
        )
        .unwrap_err();
        assert!(matches!(err, SchemaError::Yaml { .. }), "{err}");
        assert!(err.to_string().contains("helo"), "{err}");
    }

    #[test]
    fn schema_mistakes_are_caught_at_load() {
        let cases: &[(&str, &str)] = &[
            (
                "nodes:\n  a: {kind: leaf, type: hostname, help: \"A\", default: \"not a host name\"}\n",
                "default is invalid",
            ),
            (
                "nodes:\n  a: {kind: leaf, type: nonsense, help: \"A\"}\n",
                "unknown type",
            ),
            (
                "nodes:\n  a: {kind: nonsense, help: \"A\"}\n",
                "unknown kind",
            ),
            (
                "nodes:\n  a: {kind: leaf, type: string}\n",
                "no `help`",
            ),
            (
                "nodes:\n  a: {kind: container, help: \"A\"}\n",
                "can never be configured",
            ),
            (
                "nodes:\n  a: {kind: flag, help: \"A\", children: {b: {kind: flag, help: \"B\"}}}\n",
                "cannot have children",
            ),
            (
                "nodes:\n  a: {kind: leaf, type: enum, help: \"A\"}\n",
                "needs the `values`",
            ),
            (
                "nodes:\n  a: {kind: container, help: \"A\", include: [missing], children: {b: {kind: flag, help: \"B\"}}}\n",
                "no template named",
            ),
            (
                "nodes:\n  a: {kind: leaf, type: string, help: \"A\", regex: \"([\"}\n",
                "does not compile",
            ),
            (
                "nodes:\n  a: {kind: container, help: \"A\", children: {b: {kind: flag, help: \"B\"}}, mutually-exclusive: [[b, c]]}\n",
                "not one of its children",
            ),
            (
                "nodes:\n  a: {kind: container, help: \"A\", children: {b: {kind: flag, help: \"B\"}}, unique: [[b]]}\n",
                "only means something on a tag node",
            ),
            (
                "nodes:\n  a:\n    kind: leaf\n    type: string\n    help: \"A\"\n    constraints:\n      - {rule: value-in-path-set, paths: [\"a { b\"], message: \"m\"}\n",
                "bad path pattern",
            ),
        ];
        for (yaml, expected) in cases {
            let err = load(yaml).unwrap_err();
            assert!(
                err.to_string().contains(expected),
                "expected {expected:?} in {err}"
            );
        }
    }

    #[test]
    fn errors_name_the_node() {
        let err = load(
            "nodes:\n  interfaces:\n    kind: container\n    help: \"I\"\n    children:\n      ethernet:\n        kind: leaf\n        type: bogus\n        help: \"E\"\n",
        )
        .unwrap_err();
        assert!(err.to_string().contains("interfaces ethernet"), "{err}");
    }
}
