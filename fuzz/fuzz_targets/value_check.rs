//! Every value validator in the shipped schema, against arbitrary input.
//!
//! Driven from the compiled schema rather than a hand-written list of types,
//! so it covers the regex patterns and the `accepts` keywords as they are
//! actually configured -- and picks up any type a future schema node
//! introduces without this file being touched.

#![no_main]

use std::sync::OnceLock;

use libfuzzer_sys::fuzz_target;
use nightshade_schema::model::{NodeKind, Schema, SchemaNode};
use nightshade_schema::value::ValueSpec;

fn specs() -> &'static [ValueSpec] {
    static SPECS: OnceLock<Vec<ValueSpec>> = OnceLock::new();
    SPECS.get_or_init(|| {
        let mut out = Vec::new();
        collect(&Schema::compiled().root, &mut out);
        out
    })
}

fn collect(node: &SchemaNode, out: &mut Vec<ValueSpec>) {
    match &node.kind {
        NodeKind::Leaf(spec) | NodeKind::MultiLeaf(spec) => out.push(spec.clone()),
        // A tag key is operator input too, and the one that names a device.
        NodeKind::Tag(tag) => out.push(tag.value.clone()),
        NodeKind::Container | NodeKind::Flag => {}
    }
    for child in node.children.values() {
        collect(child, out);
    }
}

fuzz_target!(|value: &str| {
    for spec in specs() {
        let _ = spec.check(value);
    }
});
