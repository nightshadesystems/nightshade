//! The curly-brace configuration format.
//!
//! ```text
//! system {
//!     host-name nightshade
//!     name-server 1.1.1.1
//!     name-server 9.9.9.9
//! }
//! interfaces {
//!     /* the uplink */
//!     ethernet eth0 {
//!         address 192.168.1.1/24
//!         mtu 9000
//!         disable
//!     }
//! }
//! ```
//!
//! This is `config.boot` -- the canonical on-disk config, the thing `save`
//! writes and boot reads, and the format an operator is allowed to open in an
//! editor. So it gets a real parser: a strict grammar, an error with a line
//! and a column on every failure, and comments carried into the tree rather
//! than discarded.
//!
//! # Grammar
//!
//! ```text
//! document  := statement*
//! statement := comment* (block | leaf)
//! block     := VALUE VALUE* '{' statement* '}'      -- all before '{' on one line
//! leaf      := VALUE VALUE?                         -- on one line
//! ```
//!
//! Newlines terminate statements; there is no separator to forget. A block's
//! header and its `{` must share a line, which is what makes
//! `ethernet eth0 {` unambiguous next to `address 192.168.1.1/24`.
//!
//! Everything before the `{` is a path: `ethernet eth0 {` opens `ethernet`
//! and then `eth0` inside it. That is why the parser needs no schema. It
//! produces a tree from syntax alone, and whether `eth0` is a legal tag value
//! for `ethernet` is a question asked afterwards, by validation, which can
//! then say something useful about it. Two passes, two precise errors, and a
//! parser that can be fuzzed on its own.
//!
//! # Round-trip
//!
//! `parse(render(tree)) == tree` holds for every tree, and is property-tested
//! against generated ones. Three things buy it:
//!
//! - values are quoted whenever a bare word would not read back (see
//!   [`crate::lex`]), so no value can be mistaken for syntax
//! - the tree's maps and value sets are ordered, so rendering is a function of
//!   the tree and not of how it was built
//! - both comment syntaxes normalise to the same text on the way in, and only
//!   one is written on the way out
//!
//! The reverse -- `render(parse(text)) == text` -- deliberately does not hold.
//! Saving normalises: `/* ... */` becomes `#`, indentation is regularised,
//! multi-leaf values sort. A file is reformatted the first time it is saved,
//! and identical thereafter.

use std::collections::BTreeMap;

use crate::config::{Body, ConfigTree, Node, TreeError};
use crate::lex::{self, LexError, Lexer, Pos, Spanned, Token};
use crate::path::Path;

/// Nesting the parser will accept.
///
/// Real configs are four or five deep. This exists because the parser
/// recurses and the input is a file: `a { a { a {` repeated far enough is a
/// stack overflow, which in a daemon is an abort, and reached by a file rather
/// than by a socket only makes it a slower way in.
pub const MAX_DEPTH: usize = 64;

const INDENT: &str = "    ";

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ParseError {
    #[error("{0}")]
    Lex(#[from] LexError),

    #[error("{pos}: `{{` must follow a node name on the same line")]
    UnexpectedOpen { pos: Pos },

    #[error("{pos}: `}}` here closes nothing")]
    UnmatchedClose { pos: Pos },

    #[error("{pos}: `{{` opened here is never closed")]
    Unclosed { pos: Pos },

    #[error(
        "{pos}: `{name}` was given {count} values on one line; a leaf takes at \
         most one, and a nested node needs braces"
    )]
    TooManyValues { pos: Pos, name: String, count: usize },

    #[error("{pos}: nested more than {MAX_DEPTH} levels deep")]
    TooDeep { pos: Pos },

    #[error("{pos}: {source}")]
    Tree { pos: Pos, source: TreeError },
}

impl ParseError {
    pub fn pos(&self) -> Pos {
        match self {
            ParseError::Lex(e) => e.pos(),
            ParseError::UnexpectedOpen { pos }
            | ParseError::UnmatchedClose { pos }
            | ParseError::Unclosed { pos }
            | ParseError::TooManyValues { pos, .. }
            | ParseError::TooDeep { pos }
            | ParseError::Tree { pos, .. } => *pos,
        }
    }
}

/// Parse a config document.
pub fn parse(src: &str) -> Result<ConfigTree, ParseError> {
    let mut parser = Parser {
        lexer: Lexer::new(src),
        peeked: None,
    };
    let mut tree = ConfigTree::new();
    parser.body(&mut tree, &Path::root(), 0, None)?;
    Ok(tree)
}

struct Parser {
    lexer: Lexer,
    peeked: Option<Spanned>,
}

impl Parser {
    /// Cloned rather than borrowed: a peek that borrows the parser cannot be
    /// followed by a consume, and the tokens are one small string each.
    fn peek(&mut self) -> Result<Option<Spanned>, LexError> {
        if self.peeked.is_none() {
            self.peeked = self.lexer.next_token()?;
        }
        Ok(self.peeked.clone())
    }

    fn advance(&mut self) -> Result<Option<Spanned>, LexError> {
        match self.peeked.take() {
            Some(s) => Ok(Some(s)),
            None => self.lexer.next_token(),
        }
    }

    /// Statements until `}` (when `opened` is set) or end of input.
    fn body(
        &mut self,
        tree: &mut ConfigTree,
        prefix: &Path,
        depth: usize,
        opened: Option<Pos>,
    ) -> Result<(), ParseError> {
        if depth > MAX_DEPTH {
            return Err(ParseError::TooDeep {
                pos: opened.unwrap_or(Pos { line: 1, column: 1 }),
            });
        }

        // Comments seen since the last statement, waiting for the next one.
        let mut pending: Option<String> = None;
        // The statement just completed and the line it was on, so a comment
        // written after it on that same line attaches to it rather than
        // drifting onto whatever comes next.
        let mut previous: Option<(Path, u32)> = None;

        loop {
            let Some(Spanned { token, pos }) = self.peek()? else {
                return match opened {
                    Some(pos) => Err(ParseError::Unclosed { pos }),
                    None => Ok(()),
                };
            };

            match token {
                Token::Comment(text) => {
                    self.advance()?;
                    let trailing = pending.is_none()
                        && previous.as_ref().is_some_and(|(_, line)| *line == pos.line);
                    if trailing {
                        let (path, _) = previous.as_ref().expect("checked");
                        if let Some(node) = tree.get_mut(path)
                            && node.comment.is_none()
                        {
                            node.comment = Some(text);
                            continue;
                        }
                    }
                    pending = Some(match pending {
                        Some(prev) => format!("{prev}\n{text}"),
                        None => text,
                    });
                }

                Token::Close => {
                    self.advance()?;
                    return match opened {
                        Some(_) => Ok(()),
                        None => Err(ParseError::UnmatchedClose { pos }),
                    };
                }

                Token::Open => return Err(ParseError::UnexpectedOpen { pos }),

                Token::Value(_) => {
                    let run = self.run(pos.line)?;
                    let opens = matches!(
                        self.peek()?,
                        Some(s) if s.token == Token::Open && s.pos.line == pos.line
                    );

                    if opens {
                        let open = self.advance()?.expect("peeked").pos;
                        let mut path = prefix.clone();
                        for segment in run {
                            path.push(segment);
                        }
                        tree.ensure_interior(&path)
                            .map_err(|source| ParseError::Tree { pos, source })?;
                        attach(tree, &path, pending.take());
                        self.body(tree, &path, depth + 1, Some(open))?;
                        previous = None;
                    } else {
                        let path = self.leaf(tree, prefix, run, pos)?;
                        attach(tree, &path, pending.take());
                        previous = Some((path, pos.line));
                    }
                }
            }
        }
    }

    /// Consecutive values starting on `line`. Stops at the first token that is
    /// not a value, or that starts a new line.
    fn run(&mut self, line: u32) -> Result<Vec<String>, LexError> {
        let mut run = Vec::new();
        while let Some(Spanned { token, pos }) = self.peek()? {
            match token {
                Token::Value(v) if pos.line == line => {
                    run.push(v);
                    self.advance()?;
                }
                _ => break,
            }
        }
        Ok(run)
    }

    fn leaf(
        &mut self,
        tree: &mut ConfigTree,
        prefix: &Path,
        mut run: Vec<String>,
        pos: Pos,
    ) -> Result<Path, ParseError> {
        if run.len() > 2 {
            return Err(ParseError::TooManyValues {
                pos,
                name: run.swap_remove(0),
                count: run.len(),
            });
        }
        let mut run = run.into_iter();
        let name = run.next().expect("entered on a value");
        let path = prefix.child(name);
        let result = match run.next() {
            Some(value) => tree.add(&path, value),
            None => tree.declare_leaf(&path),
        };
        result.map_err(|source| ParseError::Tree { pos, source })?;
        Ok(path)
    }
}

/// Attach a pending comment, if the node has not already got one.
///
/// First occurrence wins, which matters for a multi-leaf: three `address`
/// lines are one node, so they have one comment between them.
fn attach(tree: &mut ConfigTree, path: &Path, comment: Option<String>) {
    let Some(comment) = comment else { return };
    if let Some(node) = tree.get_mut(path)
        && node.comment.is_none()
    {
        node.comment = Some(comment);
    }
}

// ---------------------------------------------------------------------------
// rendering
// ---------------------------------------------------------------------------

/// Which paths hold tag instances, and so may be written in the compact form.
///
/// The renderer needs this and the parser does not, because it is only a
/// question of how a node is *written*: `ethernet eth0 { }` and
/// `ethernet { eth0 { } }` are the same tree. The schema implements this; the
/// round-trip property and the fuzz target use [`Nested`], which keeps the
/// format testable without one.
pub trait TagOracle {
    fn is_tag_node(&self, path: &Path) -> bool;
}

/// Everything on its own line. Correct, verbose, schema-free.
pub struct Nested;

impl TagOracle for Nested {
    fn is_tag_node(&self, _path: &Path) -> bool {
        false
    }
}

/// Render a config document.
pub fn render(tree: &ConfigTree, tags: &dyn TagOracle) -> String {
    let mut out = String::new();
    children(&mut out, tree.root(), &Path::root(), 0, tags);
    out
}

fn children(out: &mut String, node: &Node, at: &Path, depth: usize, tags: &dyn TagOracle) {
    let Some(entries) = node.children() else {
        return;
    };
    for (name, child) in entries {
        entry(out, name, child, &at.child(name), depth, tags);
    }
}

fn entry(
    out: &mut String,
    name: &str,
    node: &Node,
    path: &Path,
    depth: usize,
    tags: &dyn TagOracle,
) {
    match &node.body {
        Body::Values(values) => {
            comment(out, depth, node.comment.as_deref());
            if values.is_empty() {
                indent(out, depth);
                lex::quote_into(out, name);
                out.push('\n');
            }
            for value in values {
                indent(out, depth);
                lex::quote_into(out, name);
                out.push(' ');
                lex::quote_into(out, value);
                out.push('\n');
            }
        }

        Body::Interior(entries) if compact(node, entries, path, tags) => {
            for (key, instance) in entries {
                comment(out, depth, instance.comment.as_deref());
                indent(out, depth);
                lex::quote_into(out, name);
                out.push(' ');
                lex::quote_into(out, key);
                out.push_str(" {\n");
                children(out, instance, &path.child(key), depth + 1, tags);
                indent(out, depth);
                out.push_str("}\n");
            }
        }

        Body::Interior(_) => {
            comment(out, depth, node.comment.as_deref());
            indent(out, depth);
            lex::quote_into(out, name);
            out.push_str(" {\n");
            children(out, node, path, depth + 1, tags);
            indent(out, depth);
            out.push_str("}\n");
        }
    }
}

/// Whether to write `ethernet eth0 { ... }` rather than nesting.
///
/// The last three conditions are what keeps the round-trip exact. A comment on
/// the tag node itself has nowhere to go in the compact form -- reading
/// `ethernet eth0 {` back puts a comment above it on `eth0` -- so a tag node
/// carrying one is written nested instead, where it survives. An empty tag
/// node would vanish entirely, and a child holding values would render as a
/// three-token line the parser rejects.
///
/// The parser only ever puts comments on the innermost node, so a config that
/// came from a file always takes the compact path. The other cases are
/// reachable by building a tree in code, and are handled rather than
/// forbidden.
fn compact(
    node: &Node,
    entries: &BTreeMap<String, Node>,
    path: &Path,
    tags: &dyn TagOracle,
) -> bool {
    tags.is_tag_node(path)
        && node.comment.is_none()
        && !entries.is_empty()
        && entries.values().all(Node::is_interior)
}

fn indent(out: &mut String, depth: usize) {
    for _ in 0..depth {
        out.push_str(INDENT);
    }
}

fn comment(out: &mut String, depth: usize, comment: Option<&str>) {
    let Some(comment) = comment else { return };
    for line in comment.split('\n') {
        indent(out, depth);
        if line.is_empty() {
            out.push_str("#\n");
        } else {
            out.push_str("# ");
            out.push_str(line);
            out.push('\n');
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use std::collections::BTreeSet;

    fn p(s: &str) -> Path {
        Path::parse(s).unwrap()
    }

    /// A `TagOracle` that answers yes for a fixed list, so the compact form
    /// can be tested before the schema exists.
    struct Tags(&'static [&'static str]);

    impl TagOracle for Tags {
        fn is_tag_node(&self, path: &Path) -> bool {
            self.0.contains(&path.to_string().as_str())
        }
    }

    const IFACE_TAGS: Tags = Tags(&["interfaces ethernet", "interfaces vlan"]);

    #[test]
    fn parses_a_realistic_document() {
        let tree = parse(
            r#"
system {
    host-name nightshade
    name-server 1.1.1.1
    name-server 9.9.9.9
}
interfaces {
    ethernet eth0 {
        address 192.168.1.1/24
        description "the uplink"
        disable
    }
}
"#,
        )
        .unwrap();

        assert_eq!(tree.get(&p("system host-name")).unwrap().value(), Some("nightshade"));
        assert_eq!(tree.values_at(&p("system name-server")).unwrap().len(), 2);
        assert_eq!(
            tree.get(&p("interfaces ethernet eth0 description")).unwrap().value(),
            Some("the uplink")
        );
        // A flag is a leaf with no values, not an empty container.
        let disable = tree.get(&p("interfaces ethernet eth0 disable")).unwrap();
        assert_eq!(disable.value_set(), Some(&BTreeSet::new()));
    }

    #[test]
    fn a_block_header_is_a_path() {
        // These two documents are the same tree.
        let compact = parse("interfaces { ethernet eth0 { mtu 9000 } }").unwrap();
        let nested = parse("interfaces { ethernet { eth0 { mtu 9000 } } }").unwrap();
        assert_eq!(compact, nested);
        assert_eq!(
            compact.get(&p("interfaces ethernet eth0 mtu")).unwrap().value(),
            Some("9000")
        );
    }

    #[test]
    fn tag_nodes_render_compactly_and_read_back_the_same() {
        let tree = parse("interfaces { ethernet eth0 { mtu 9000 } ethernet eth1 { } }").unwrap();
        let text = render(&tree, &IFACE_TAGS);
        assert_eq!(
            text,
            "interfaces {\n    \
                 ethernet eth0 {\n        mtu 9000\n    }\n    \
                 ethernet eth1 {\n    }\n\
             }\n"
        );
        assert_eq!(parse(&text).unwrap(), tree);
    }

    #[test]
    fn without_an_oracle_everything_nests() {
        let tree = parse("interfaces { ethernet eth0 { mtu 9000 } }").unwrap();
        assert_eq!(
            render(&tree, &Nested),
            "interfaces {\n    ethernet {\n        eth0 {\n            mtu 9000\n        }\n    }\n}\n"
        );
    }

    #[test]
    fn multi_leaf_values_render_one_per_line_and_sort() {
        let tree = parse("system { name-server 9.9.9.9\nname-server 1.1.1.1 }").unwrap();
        assert_eq!(
            render(&tree, &Nested),
            "system {\n    name-server 1.1.1.1\n    name-server 9.9.9.9\n}\n"
        );
    }

    #[test]
    fn values_needing_quotes_get_them() {
        let mut tree = ConfigTree::new();
        tree.set(&p("interfaces ethernet eth0 description"), "the # uplink")
            .unwrap();
        let text = render(&tree, &Nested);
        assert!(text.contains(r#"description "the # uplink""#), "{text}");
        assert_eq!(parse(&text).unwrap(), tree);
    }

    #[test]
    fn comments_attach_to_the_node_below_them() {
        let tree = parse(
            r#"
interfaces {
    /* the uplink
       to the ISP */
    ethernet eth0 {
        # not routed yet
        disable
    }
}
"#,
        )
        .unwrap();
        assert_eq!(
            tree.get(&p("interfaces ethernet eth0")).unwrap().comment.as_deref(),
            Some("the uplink\nto the ISP")
        );
        assert_eq!(
            tree.get(&p("interfaces ethernet eth0 disable")).unwrap().comment.as_deref(),
            Some("not routed yet")
        );
    }

    #[test]
    fn a_trailing_comment_stays_with_its_own_line() {
        let tree = parse("system {\n    host-name fw   # the edge box\n    time-zone UTC\n}").unwrap();
        assert_eq!(
            tree.get(&p("system host-name")).unwrap().comment.as_deref(),
            Some("the edge box")
        );
        assert_eq!(tree.get(&p("system time-zone")).unwrap().comment, None);
    }

    #[test]
    fn both_comment_syntaxes_normalise_to_one_output_syntax() {
        let a = parse("# note\nsystem { }").unwrap();
        let b = parse("/* note */\nsystem { }").unwrap();
        assert_eq!(a, b);
        assert_eq!(render(&a, &Nested), "# note\nsystem {\n}\n");
    }

    #[test]
    fn consecutive_comments_are_one_comment() {
        let tree = parse("# first\n# second\nsystem { }").unwrap();
        assert_eq!(
            tree.get(&p("system")).unwrap().comment.as_deref(),
            Some("first\nsecond")
        );
    }

    #[test]
    fn a_tag_node_with_its_own_comment_falls_back_to_nesting() {
        // Not reachable by parsing -- the parser puts comments on the
        // innermost node -- but reachable in code, and it must still survive
        // a save and a load.
        let mut tree = parse("interfaces { ethernet eth0 { } }").unwrap();
        tree.get_mut(&p("interfaces ethernet")).unwrap().comment = Some("all the ports".into());
        let text = render(&tree, &IFACE_TAGS);
        assert!(text.contains("# all the ports"), "{text}");
        assert_eq!(parse(&text).unwrap(), tree);
    }

    #[test]
    fn empty_document_is_an_empty_tree() {
        assert!(parse("").unwrap().is_empty());
        assert!(parse("\n\n   \n").unwrap().is_empty());
        assert!(parse("# just a comment").unwrap().is_empty());
        assert_eq!(render(&ConfigTree::new(), &Nested), "");
    }

    #[test]
    fn errors_point_at_the_problem() {
        let cases: &[(&str, u32, u32)] = &[
            ("system {\n    host-name fw\n", 1, 8),          // unclosed
            ("system {\n}\n}\n", 3, 1),                       // unmatched close
            ("system {\n    a b c\n}\n", 2, 5),               // too many values
            ("{\n", 1, 1),                                    // nothing to open
            ("system {\n    host-name \"fw\n}\n", 2, 15),     // unterminated string
        ];
        for (src, line, column) in cases {
            let err = parse(src).unwrap_err();
            assert_eq!(
                err.pos(),
                Pos { line: *line, column: *column },
                "{src:?} reported {err}"
            );
        }
    }

    #[test]
    fn contradictory_documents_are_refused() {
        // A leaf cannot also be a container.
        let err = parse("system { host-name fw\nhost-name { } }").unwrap_err();
        assert!(matches!(err, ParseError::Tree { .. }), "{err}");

        // Nor a container a leaf.
        let err = parse("system { host-name { }\nhost-name fw }").unwrap_err();
        assert!(matches!(err, ParseError::Tree { .. }), "{err}");
    }

    #[test]
    fn nesting_is_bounded() {
        let deep = "a {\n".repeat(MAX_DEPTH + 2) + &"}\n".repeat(MAX_DEPTH + 2);
        assert!(matches!(parse(&deep).unwrap_err(), ParseError::TooDeep { .. }));
    }

    // -- properties ---------------------------------------------------------

    fn arb_comment() -> impl Strategy<Value = String> {
        prop_oneof![
            3 => "[a-z ]{0,20}",
            1 => any::<String>(),
        ]
    }

    fn arb_name() -> impl Strategy<Value = String> {
        prop_oneof![
            4 => "[a-z][a-z0-9-]{0,8}",
            1 => any::<String>(),
        ]
    }

    fn arb_value() -> impl Strategy<Value = String> {
        prop_oneof![
            4 => "[a-zA-Z0-9./:_+%-]{0,20}",
            1 => any::<String>(),
        ]
    }

    fn arb_node() -> impl Strategy<Value = Node> {
        let leaf = (
            proptest::option::of(arb_comment()),
            prop::collection::btree_set(arb_value(), 0..3),
        )
            .prop_map(|(comment, values)| Node {
                comment,
                body: Body::Values(values),
            });

        leaf.prop_recursive(4, 24, 3, |inner| {
            (
                proptest::option::of(arb_comment()),
                prop::collection::btree_map(arb_name(), inner, 0..3),
            )
                .prop_map(|(comment, entries)| Node {
                    comment,
                    body: Body::Interior(entries),
                })
        })
    }

    fn arb_tree() -> impl Strategy<Value = ConfigTree> {
        prop::collection::btree_map(arb_name(), arb_node(), 0..4).prop_map(ConfigTree::from_children)
    }

    proptest! {
        /// The property the on-disk format lives or dies by. Anything that
        /// can be configured has to survive a save and a load unchanged --
        /// including its comments, its quoting, and the difference between a
        /// flag and an empty container.
        #[test]
        fn render_then_parse_is_identity(tree in arb_tree()) {
            let text = render(&tree, &Nested);
            let back = parse(&text).map_err(|e| TestCaseError::fail(format!("{e}\n--- rendered ---\n{text}")))?;
            prop_assert_eq!(back, tree, "rendered as:\n{}", text);
        }

        /// Saving twice must produce the same bytes. Without this, every
        /// commit would show a diff against the previous save.
        #[test]
        fn rendering_is_stable(tree in arb_tree()) {
            let once = render(&tree, &Nested);
            let twice = render(&parse(&once).unwrap(), &Nested);
            prop_assert_eq!(once, twice);
        }

        /// The parser is handed a file. It may reject anything, but it may not
        /// panic, overflow the stack or hang on it.
        #[test]
        fn arbitrary_input_never_panics(src in ".{0,400}") {
            let _ = parse(&src);
        }

        /// Same, over input that looks much more like a config file, so the
        /// interesting paths are actually reached.
        #[test]
        fn config_shaped_input_never_panics(
            src in prop::collection::vec(
                prop_oneof![
                    Just("{".to_string()), Just("}".to_string()),
                    Just("\n".to_string()), Just(" ".to_string()),
                    Just("#c\n".to_string()), Just("/*c*/".to_string()),
                    Just("\"q\"".to_string()), Just("\\".to_string()),
                    "[a-z]{1,4}",
                ],
                0..60,
            ).prop_map(|parts| parts.concat())
        ) {
            let _ = parse(&src);
        }
    }
}
