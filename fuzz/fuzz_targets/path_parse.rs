//! The path parser, which sees every line an operator types.
//!
//! Also checks that `Display` is the inverse of `parse`. The CLI renders paths
//! back into error messages, diffs and the saved config; a path that does not
//! survive that trip is a `delete` aimed at the wrong node.

#![no_main]

use libfuzzer_sys::fuzz_target;
use nightshade_schema::path::Path;

fuzz_target!(|text: &str| {
    let Ok(path) = Path::parse(text) else {
        return;
    };
    let shown = path.to_string();
    match Path::parse(&shown) {
        Ok(again) => assert_eq!(again, path, "path did not survive Display: {shown:?}"),
        Err(e) => panic!("a rendered path did not parse: {e}\nrendered: {shown:?}"),
    }
});
