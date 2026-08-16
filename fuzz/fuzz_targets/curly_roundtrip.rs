//! `parse(render(tree)) == tree`, with coverage guidance behind it.
//!
//! The property tests generate trees from a strategy, which explores what the
//! strategy was written to explore. This starts from arbitrary *text*, so the
//! trees it round-trips are whatever the parser can be talked into producing
//! -- including shapes nobody thought to generate.
//!
//! Two assertions, and the first is the sharper one: text that parsed, once
//! rendered, must parse again. A tree the renderer can write but the parser
//! cannot read is a config that saves and then will not boot.

#![no_main]

use libfuzzer_sys::fuzz_target;
use nightshade_schema::curly;

fuzz_target!(|text: &str| {
    let Ok(tree) = curly::parse(text) else {
        return;
    };

    let rendered = curly::render(&tree, &curly::Nested);
    let reparsed = match curly::parse(&rendered) {
        Ok(tree) => tree,
        Err(e) => panic!("a rendered tree did not parse: {e}\n--- rendered ---\n{rendered}"),
    };
    assert_eq!(reparsed, tree, "round trip changed the tree:\n{rendered}");

    // And rendering is a function of the tree, not of how it was reached.
    assert_eq!(curly::render(&reparsed, &curly::Nested), rendered);
});
