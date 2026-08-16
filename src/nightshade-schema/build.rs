//! Compiles `schema/` into Rust.
//!
//! The library's own modules are pulled in with `#[path]` rather than
//! reimplemented, so the schema is read at build time by exactly the loader
//! that reads it at runtime. That is what makes
//! `generated_matches_the_source_files` a real test rather than a test of two
//! parsers that happen to agree today.
//!
//! Mirroring the module names at the build-script root is what makes it work:
//! a `crate::lex` inside `path.rs` resolves to the `mod lex` below when this
//! file is the crate root, and to the library's when `lib.rs` is.
//!
//! A schema that does not load fails the build here, which is the whole point
//! of generating anything -- the alternative is a binary that starts, reads a
//! broken schema, and has to decide what to do about it on a box somebody
//! needs.

#![allow(dead_code, unused_imports)]

#[path = "src/lex.rs"]
mod lex;
#[path = "src/path.rs"]
mod path;
#[path = "src/value.rs"]
mod value;
#[path = "src/model.rs"]
mod model;
#[path = "src/loader.rs"]
mod loader;

#[path = "codegen.rs"]
mod codegen;

fn main() {
    let dir = loader::source_dir();

    // The directory, and every file under it: cargo walks a directory given
    // here, but naming the files too means a rename is noticed as well as an
    // edit.
    println!("cargo::rerun-if-changed={}", dir.display());
    println!("cargo::rerun-if-changed=codegen.rs");
    for file in walk(&dir) {
        println!("cargo::rerun-if-changed={}", file.display());
    }

    let schema = match loader::load_dir(&dir) {
        Ok(schema) => schema,
        Err(e) => panic!("the schema in {} does not load: {e}", dir.display()),
    };

    let out = std::path::PathBuf::from(std::env::var_os("OUT_DIR").expect("cargo sets OUT_DIR"))
        .join("schema.rs");
    if let Err(e) = std::fs::write(&out, codegen::emit(&schema)) {
        panic!("writing {}: {e}", out.display());
    }
}

fn walk(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            out.extend(walk(&path));
        } else {
            out.push(path);
        }
    }
    out.sort();
    out
}
