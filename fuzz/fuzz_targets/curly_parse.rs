//! The config file parser, fed arbitrary text.
//!
//! It reads `/etc/nightshade/config.boot`, a file operators are invited to
//! hand-edit, inside a daemon built with `panic = "abort"`. It may reject
//! anything at all; it may not panic, and it may not run the stack out.

#![no_main]

use libfuzzer_sys::fuzz_target;
use nightshade_schema::curly;

fuzz_target!(|text: &str| {
    let _ = curly::parse(text);
});
