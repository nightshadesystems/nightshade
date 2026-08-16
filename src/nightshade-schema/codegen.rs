//! Emitting the loaded schema as Rust source.
//!
//! Compiled only by `build.rs`, never by the library -- it sits beside the
//! build script rather than under `src/` for that reason.
//!
//! It emits real, indented Rust rather than a serialised blob the binary
//! decodes at startup. A blob would be less code here, but the whole reason
//! for generating anything is that when the schema does something surprising
//! you can open `target/.../schema.rs` and read what it actually became. That
//! only works if the output is meant to be read.

use std::collections::BTreeMap;
use std::fmt::Write;

use crate::model::{
    Constraint, GlobalConstraint, NodeKind, PathPattern, PatternSegment, Schema, SchemaNode,
    TagSpec,
};
use crate::value::{Range, ValueSpec, ValueType};

pub fn emit(schema: &Schema) -> String {
    let mut out = String::new();
    out.push_str(
        "// Generated from schema/ by build.rs. Do not edit; edit the YAML.\n\
         //\n\
         // Included by lib.rs as `mod generated`, so the paths below are\n\
         // crate-relative.\n\n\
         use std::collections::BTreeMap;\n\n\
         use crate::model::{\n    \
             Constraint, GlobalConstraint, NodeKind, PathPattern, PatternSegment, Schema,\n    \
             SchemaNode, TagSpec,\n\
         };\n\
         use crate::value::{Range, ValueSpec, ValueType};\n\n\
         pub fn schema() -> Schema {\n    \
             Schema {\n        \
                 root: ",
    );
    node(&mut out, &schema.root, 2);
    out.push_str(",\n        globals: ");
    list(&mut out, &schema.globals, 2, global);
    out.push_str(",\n    }\n}\n");
    out
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

fn pad(out: &mut String, depth: usize) {
    for _ in 0..depth {
        out.push_str("    ");
    }
}

/// A Rust string literal for `s`.
///
/// `{:?}` on a `str` is exactly that -- escapes and all -- which is both
/// shorter and more correct than hand-rolling the escaping.
fn text(s: &str) -> String {
    format!("{s:?}")
}

fn owned(s: &str) -> String {
    format!("{}.to_string()", text(s))
}

fn maybe(value: &Option<String>) -> String {
    match value {
        Some(v) => format!("Some({})", owned(v)),
        None => "None".to_string(),
    }
}

/// `vec![...]`, one item per line, or `Vec::new()` when empty.
fn list<T>(out: &mut String, items: &[T], depth: usize, mut item: impl FnMut(&mut String, &T, usize)) {
    if items.is_empty() {
        out.push_str("Vec::new()");
        return;
    }
    out.push_str("vec![\n");
    for entry in items {
        pad(out, depth + 1);
        item(out, entry, depth + 1);
        out.push_str(",\n");
    }
    pad(out, depth);
    out.push(']');
}

fn strings(out: &mut String, items: &[String]) {
    if items.is_empty() {
        out.push_str("Vec::new()");
        return;
    }
    let _ = write!(
        out,
        "vec![{}]",
        items.iter().map(|s| owned(s)).collect::<Vec<_>>().join(", ")
    );
}

// ---------------------------------------------------------------------------
// the model
// ---------------------------------------------------------------------------

fn node(out: &mut String, node: &SchemaNode, depth: usize) {
    out.push_str("SchemaNode {\n");

    pad(out, depth + 1);
    out.push_str("kind: ");
    kind(out, &node.kind, depth + 1);
    out.push_str(",\n");

    pad(out, depth + 1);
    let _ = writeln!(out, "help: {},", owned(&node.help));
    pad(out, depth + 1);
    let _ = writeln!(out, "default: {},", maybe(&node.default));
    pad(out, depth + 1);
    let _ = writeln!(out, "secret: {},", node.secret);
    pad(out, depth + 1);
    let _ = writeln!(out, "required: {},", node.required);
    pad(out, depth + 1);
    let _ = writeln!(out, "priority: {},", node.priority);

    pad(out, depth + 1);
    out.push_str("children: ");
    children(out, &node.children, depth + 1);
    out.push_str(",\n");

    pad(out, depth + 1);
    out.push_str("constraints: ");
    list(out, &node.constraints, depth + 1, constraint);
    out.push_str(",\n");

    pad(out, depth + 1);
    out.push_str("unique: ");
    tuples(out, &node.unique, depth + 1);
    out.push_str(",\n");

    pad(out, depth + 1);
    out.push_str("exclusive: ");
    tuples(out, &node.exclusive, depth + 1);
    out.push_str(",\n");

    pad(out, depth);
    out.push('}');
}

fn children(out: &mut String, entries: &BTreeMap<String, SchemaNode>, depth: usize) {
    if entries.is_empty() {
        out.push_str("BTreeMap::new()");
        return;
    }
    out.push_str("BTreeMap::from([\n");
    for (name, child) in entries {
        pad(out, depth + 1);
        let _ = write!(out, "({}, ", owned(name));
        node(out, child, depth + 1);
        out.push_str("),\n");
    }
    pad(out, depth);
    out.push_str("])");
}

fn tuples(out: &mut String, groups: &[Vec<String>], depth: usize) {
    list(out, groups, depth, |out, group, _| strings(out, group));
}

fn kind(out: &mut String, kind: &NodeKind, depth: usize) {
    match kind {
        NodeKind::Container => out.push_str("NodeKind::Container"),
        NodeKind::Flag => out.push_str("NodeKind::Flag"),
        NodeKind::Leaf(spec) => {
            out.push_str("NodeKind::Leaf(");
            value_spec(out, spec);
            out.push(')');
        }
        NodeKind::MultiLeaf(spec) => {
            out.push_str("NodeKind::MultiLeaf(");
            value_spec(out, spec);
            out.push(')');
        }
        NodeKind::Tag(TagSpec { value, help }) => {
            out.push_str("NodeKind::Tag(TagSpec {\n");
            pad(out, depth + 1);
            out.push_str("value: ");
            value_spec(out, value);
            out.push_str(",\n");
            pad(out, depth + 1);
            let _ = writeln!(out, "help: {},", owned(help));
            pad(out, depth);
            out.push_str("})");
        }
    }
}

fn value_spec(out: &mut String, spec: &ValueSpec) {
    out.push_str("ValueSpec { ty: ");
    value_type(out, &spec.ty);
    out.push_str(", accepts: ");
    strings(out, &spec.accepts);
    out.push_str(", pattern: ");
    match &spec.pattern {
        // Compiled once, behind the `OnceLock` in `Schema::compiled`. The
        // pattern came through the loader at build time, so it is known to
        // compile; a panic here would mean the generator wrote it out wrong.
        Some(regex) => {
            let _ = write!(
                out,
                "Some(regex::Regex::new({}).expect(\"checked at build time\"))",
                text(regex.as_str())
            );
        }
        None => out.push_str("None"),
    }
    out.push_str(" }");
}

fn value_type(out: &mut String, ty: &ValueType) {
    let simple = match ty {
        ValueType::Text => "Text",
        ValueType::Bool => "Bool",
        ValueType::Ipv4Address => "Ipv4Address",
        ValueType::Ipv6Address => "Ipv6Address",
        ValueType::IpAddress => "IpAddress",
        ValueType::Ipv4Prefix => "Ipv4Prefix",
        ValueType::Ipv6Prefix => "Ipv6Prefix",
        ValueType::IpPrefix => "IpPrefix",
        ValueType::IpOrPrefix => "IpOrPrefix",
        ValueType::MulticastAddress => "MulticastAddress",
        ValueType::MacAddress => "MacAddress",
        ValueType::Port => "Port",
        ValueType::PortRange => "PortRange",
        ValueType::InterfaceName => "InterfaceName",
        ValueType::Hostname => "Hostname",
        ValueType::TimeZone => "TimeZone",

        ValueType::Number(Range { min, max, step }) => {
            let step = match step {
                Some(step) => format!("Some({step})"),
                None => "None".to_string(),
            };
            let _ = write!(
                out,
                "ValueType::Number(Range {{ min: {min}, max: {max}, step: {step} }})"
            );
            return;
        }
        ValueType::Enum(values) => {
            out.push_str("ValueType::Enum(");
            strings(out, values);
            out.push(')');
            return;
        }
    };
    let _ = write!(out, "ValueType::{simple}");
}

// ---------------------------------------------------------------------------
// constraints
// ---------------------------------------------------------------------------

fn pattern(out: &mut String, pattern: &PathPattern) {
    let segments: Vec<String> = pattern
        .segments
        .iter()
        .map(|segment| match segment {
            PatternSegment::Literal(name) => format!("PatternSegment::Literal({})", owned(name)),
            PatternSegment::Any => "PatternSegment::Any".to_string(),
            PatternSegment::Current => "PatternSegment::Current".to_string(),
        })
        .collect();
    let _ = write!(
        out,
        "PathPattern {{ segments: vec![{}] }}",
        segments.join(", ")
    );
}

fn patterns(out: &mut String, items: &[PathPattern], depth: usize) {
    list(out, items, depth, |out, item, _| pattern(out, item));
}

fn constraint(out: &mut String, constraint: &Constraint, depth: usize) {
    match constraint {
        Constraint::ValueInPathSet { paths, message } => {
            out.push_str("Constraint::ValueInPathSet {\n");
            pad(out, depth + 1);
            out.push_str("paths: ");
            patterns(out, paths, depth + 1);
            out.push_str(",\n");
            pad(out, depth + 1);
            let _ = writeln!(out, "message: {},", owned(message));
            pad(out, depth);
            out.push('}');
        }
        Constraint::PathExists { path, message } => {
            out.push_str("Constraint::PathExists { path: ");
            pattern(out, path);
            let _ = write!(out, ", message: {} }}", owned(message));
        }
        Constraint::PathHasValue {
            path,
            value,
            message,
        } => {
            out.push_str("Constraint::PathHasValue { path: ");
            pattern(out, path);
            let _ = write!(
                out,
                ", value: {}, message: {} }}",
                owned(value),
                owned(message)
            );
        }
    }
}

fn global(out: &mut String, global: &GlobalConstraint, depth: usize) {
    match global {
        GlobalConstraint::UniqueAcross { paths, message } => {
            out.push_str("GlobalConstraint::UniqueAcross {\n");
            pad(out, depth + 1);
            out.push_str("paths: ");
            patterns(out, paths, depth + 1);
            out.push_str(",\n");
            pad(out, depth + 1);
            let _ = writeln!(out, "message: {},", owned(message));
            pad(out, depth);
            out.push('}');
        }
        GlobalConstraint::ForbidChildOnReferenced {
            references,
            search,
            forbid,
            message,
        } => {
            out.push_str("GlobalConstraint::ForbidChildOnReferenced {\n");
            pad(out, depth + 1);
            out.push_str("references: ");
            patterns(out, references, depth + 1);
            out.push_str(",\n");
            pad(out, depth + 1);
            out.push_str("search: ");
            patterns(out, search, depth + 1);
            out.push_str(",\n");
            pad(out, depth + 1);
            let _ = writeln!(out, "forbid: {},", owned(forbid));
            pad(out, depth + 1);
            let _ = writeln!(out, "message: {},", owned(message));
            pad(out, depth);
            out.push('}');
        }
    }
}
