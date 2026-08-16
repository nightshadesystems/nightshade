//! Configuration paths.
//!
//! `interfaces ethernet eth0 address` -- the address of a node in the config
//! tree, and the thing an operator types. Space separated, quoted where a
//! segment needs it, and the same tokeniser as the config file so
//! `description "my uplink"` means the same on a command line as in
//! `config.boot`.
//!
//! A path is only a location. It carries no notion of whether the node it
//! names exists, what type it holds or whether the last segment is really a
//! value -- all of that needs the schema, and this type is used in places that
//! do not have one.

use std::fmt;

use crate::lex::{self, LexError, Lexer, Token};

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PathError {
    #[error("{0}")]
    Lex(#[from] LexError),

    #[error("{pos}: `{ch}` is not allowed in a path")]
    Brace { pos: lex::Pos, ch: char },

    #[error("{pos}: comments are not allowed in a path")]
    Comment { pos: lex::Pos },
}

/// A sequence of path segments, from the root.
///
/// Ordered and comparable so paths sort predictably in diffs and error lists;
/// hashable so they can key a map of, say, pending changes.
#[derive(Debug, Clone, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Path {
    segments: Vec<String>,
}

impl Path {
    /// The root. `show` with no arguments is asking about this.
    pub fn root() -> Self {
        Self::default()
    }

    /// Tokenise a typed path.
    ///
    /// Empty input is the root rather than an error: the CLI reaches here with
    /// whatever was typed, and "nothing" is a real answer.
    pub fn parse(s: &str) -> Result<Self, PathError> {
        let mut lexer = Lexer::new(s);
        let mut segments = Vec::new();
        while let Some(spanned) = lexer.next_token()? {
            match spanned.token {
                Token::Value(v) => segments.push(v),
                Token::Open => {
                    return Err(PathError::Brace {
                        pos: spanned.pos,
                        ch: '{',
                    });
                }
                Token::Close => {
                    return Err(PathError::Brace {
                        pos: spanned.pos,
                        ch: '}',
                    });
                }
                Token::Comment(_) => return Err(PathError::Comment { pos: spanned.pos }),
            }
        }
        Ok(Self { segments })
    }

    pub fn from_segments<I, S>(segments: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            segments: segments.into_iter().map(Into::into).collect(),
        }
    }

    pub fn segments(&self) -> &[String] {
        &self.segments
    }

    pub fn len(&self) -> usize {
        self.segments.len()
    }

    pub fn is_empty(&self) -> bool {
        self.segments.is_empty()
    }

    pub fn last(&self) -> Option<&str> {
        self.segments.last().map(String::as_str)
    }

    /// This path with `segment` appended.
    pub fn child(&self, segment: impl Into<String>) -> Self {
        let mut segments = self.segments.clone();
        segments.push(segment.into());
        Self { segments }
    }

    /// This path with its last segment removed, or `None` at the root.
    pub fn parent(&self) -> Option<Self> {
        if self.segments.is_empty() {
            return None;
        }
        Some(Self {
            segments: self.segments[..self.segments.len() - 1].to_vec(),
        })
    }

    /// Split into everything but the last segment, and the last segment.
    pub fn split_last(&self) -> Option<(Self, &str)> {
        let (last, head) = self.segments.split_last()?;
        Some((
            Self {
                segments: head.to_vec(),
            },
            last.as_str(),
        ))
    }

    pub fn starts_with(&self, prefix: &Path) -> bool {
        self.segments.starts_with(&prefix.segments)
    }

    pub fn push(&mut self, segment: impl Into<String>) {
        self.segments.push(segment.into());
    }
}

impl fmt::Display for Path {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut first = true;
        for segment in &self.segments {
            if !first {
                f.write_str(" ")?;
            }
            first = false;
            f.write_str(&lex::quote(segment))?;
        }
        Ok(())
    }
}

impl<S: Into<String>> FromIterator<S> for Path {
    fn from_iter<I: IntoIterator<Item = S>>(iter: I) -> Self {
        Self::from_segments(iter)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn segs(s: &str) -> Vec<String> {
        Path::parse(s).unwrap().segments.clone()
    }

    #[test]
    fn splits_on_whitespace() {
        assert_eq!(
            segs("interfaces ethernet eth0 address"),
            ["interfaces", "ethernet", "eth0", "address"]
        );
        assert_eq!(segs("  system   host-name  "), ["system", "host-name"]);
    }

    #[test]
    fn root_is_empty_not_an_error() {
        assert_eq!(Path::parse("").unwrap(), Path::root());
        assert_eq!(Path::parse("   ").unwrap(), Path::root());
        assert!(Path::root().is_empty());
        assert_eq!(Path::root().parent(), None);
    }

    #[test]
    fn quoted_segments_survive_spaces() {
        assert_eq!(
            segs(r#"interfaces ethernet eth0 description "the uplink""#),
            ["interfaces", "ethernet", "eth0", "description", "the uplink"]
        );
    }

    #[test]
    fn braces_and_comments_are_rejected() {
        assert!(matches!(
            Path::parse("interfaces {").unwrap_err(),
            PathError::Brace { ch: '{', .. }
        ));
        assert!(matches!(
            Path::parse("interfaces }").unwrap_err(),
            PathError::Brace { ch: '}', .. }
        ));
        assert!(matches!(
            Path::parse("interfaces # nope").unwrap_err(),
            PathError::Comment { .. }
        ));
    }

    #[test]
    fn lex_errors_pass_through_with_their_position() {
        let err = Path::parse(r#"system host-name "unclosed"#).unwrap_err();
        assert!(matches!(err, PathError::Lex(LexError::UnterminatedString { .. })));
        assert!(err.to_string().contains("column 18"), "{err}");
    }

    #[test]
    fn display_round_trips() {
        for p in [
            "",
            "system host-name",
            "interfaces ethernet eth0 address 192.168.1.1/24",
            r#"interfaces ethernet eth0 description "two words""#,
        ] {
            let parsed = Path::parse(p).unwrap();
            assert_eq!(Path::parse(&parsed.to_string()).unwrap(), parsed);
        }
    }

    #[test]
    fn display_quotes_only_what_needs_it() {
        let p = Path::from_segments(["system", "host-name", "fw 01"]);
        assert_eq!(p.to_string(), r#"system host-name "fw 01""#);
    }

    #[test]
    fn navigation() {
        let p = Path::parse("interfaces ethernet eth0").unwrap();
        assert_eq!(p.last(), Some("eth0"));
        assert_eq!(p.parent().unwrap().to_string(), "interfaces ethernet");
        assert_eq!(p.child("mtu").to_string(), "interfaces ethernet eth0 mtu");
        assert!(p.starts_with(&Path::parse("interfaces").unwrap()));
        assert!(!p.starts_with(&Path::parse("system").unwrap()));

        let (head, last) = p.split_last().unwrap();
        assert_eq!(head.to_string(), "interfaces ethernet");
        assert_eq!(last, "eth0");
    }
}
