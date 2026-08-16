//! What a Nightshade config *is*: the schema that defines it, the tree that
//! holds it, and the curly-brace format it is written in.
//!
//! # Everything comes from `schema/`
//!
//! The YAML under `schema/` is the only place a config node is defined. From
//! it come the validators, the defaults, the help text shown by `?`, the tab
//! completion tables and the CLI's command tree. Adding a config node must be
//! an edit to `schema/` and nothing else -- plus a renderer, if the node is
//! the first of a new subsystem.
//!
//! That rule is the whole reason this crate exists. The moment a node's type
//! is also written down in a `match` arm somewhere in configd, the two drift,
//! and the schema becomes documentation instead of the definition.
//!
//! # Two ways in, one tree out
//!
//! A `build.rs` compiles `schema/` into Rust so the shipped binaries carry no
//! YAML parser and cannot start with a broken schema. The same files can also
//! be loaded at runtime, which is what tooling and most tests use. Both paths
//! must produce an identical tree, and there is a test that says so; without
//! it the generated path is free to quietly diverge from the one every test
//! exercises.
//!
//! `build.rs` emitting Rust, rather than a proc-macro: the output is a file
//! you can open and read when the schema does something surprising.
//!
//! # Two renderings, one tree
//!
//! The typed config tree is the truth. It renders two ways:
//!
//! - **curly-brace**, VyOS/JunOS style -- this is `config.boot`, the canonical
//!   on-disk form, and what the archive stores. It is meant to be read, hand
//!   edited and diffed, which means it needs a real parser: strict grammar,
//!   errors carrying line and column, comments preserved where practical, and
//!   `parse(render(tree)) == tree` for every valid config. That property is
//!   property-tested and the parser is fuzzed, because it is the one component
//!   that reads a file an operator was invited to edit.
//! - **JSON**, key-sorted and stable -- interchange and display only. Never
//!   the on-disk config.
//!
//! # Public surface
//!
//! `validate_set`, `children_of`, `defaults`, `check_constraints`.
//!
//! `children_of` is the one the CLI is allowed to call for completion and `?`
//! help. Reading the schema for metadata is not the same as validating in the
//! client: validation answers happen in configd, once, where they cannot be
//! bypassed by a caller that is not the CLI.
