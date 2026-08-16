//! The one tokeniser.
//!
//! Both the config file format and the `set interfaces ethernet eth0 ...`
//! path syntax are the same small language: bare words, quoted strings, and --
//! in a file -- braces and comments. Writing that twice would mean two answers
//! to "is `layer2+3` a word", and the two would eventually disagree.
//!
//! Every error carries a line and a column, because this reads a file an
//! operator was invited to edit by hand and "syntax error" is not a diagnosis.

use std::fmt;

/// One-based line and column, counted in characters rather than bytes so the
/// column matches what an editor shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pos {
    pub line: u32,
    pub column: u32,
}

impl fmt::Display for Pos {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "line {}, column {}", self.line, self.column)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Token {
    /// A bare word or a quoted string. Already unquoted and unescaped: by the
    /// time a token exists, how it was written no longer matters.
    Value(String),
    Open,
    Close,
    /// Comment body, delimiters stripped. `# foo` and `/* foo */` both give
    /// `foo`; the distinction is not carried, because carrying it would mean
    /// the renderer had to reproduce a style choice to round-trip.
    Comment(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Spanned {
    pub token: Token,
    pub pos: Pos,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LexError {
    #[error("{pos}: unterminated quoted value (opened here)")]
    UnterminatedString { pos: Pos },

    #[error("{pos}: unterminated comment -- no closing `*/`")]
    UnterminatedComment { pos: Pos },

    #[error("{pos}: unknown escape `\\{ch}`; valid escapes are \\\\ \\\" \\n \\t \\r \\xNN")]
    UnknownEscape { pos: Pos, ch: char },

    #[error("{pos}: `\\x` must be followed by two hex digits in the range 00-7f")]
    BadHexEscape { pos: Pos },

    #[error("{pos}: unexpected character {ch:?}; quote it if it is part of a value")]
    UnexpectedChar { pos: Pos, ch: char },
}

impl LexError {
    pub fn pos(&self) -> Pos {
        match self {
            LexError::UnterminatedString { pos }
            | LexError::UnterminatedComment { pos }
            | LexError::UnknownEscape { pos, .. }
            | LexError::BadHexEscape { pos }
            | LexError::UnexpectedChar { pos, .. } => *pos,
        }
    }
}

/// Characters a value may be written with no quotes around it.
///
/// Chosen from what actually appears in a Nightshade config: addresses and
/// prefixes (`.` `:` `/`), MACs (`:`), scoped IPv6 (`%`), interface and mode
/// names (`-` `_`), and hash policies (`+`). Deliberately excludes `*`, so the
/// sequence `/*` can only ever be a comment, and `#`, so a comment can only
/// ever be a comment.
///
/// Anything outside this -- including all non-ASCII -- is quoted on the way
/// out. Erring towards quoting costs a pair of quotes; erring the other way
/// costs a config file that no longer parses.
pub fn is_bare_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '.' | ':' | '/' | '_' | '-' | '+' | '%')
}

/// Whether `s` can be written without quotes. Empty never can: it would
/// vanish.
pub fn is_bare(s: &str) -> bool {
    !s.is_empty() && s.chars().all(is_bare_char)
}

/// Append `s` to `out` in a form the lexer reads back as exactly `s`.
pub fn quote_into(out: &mut String, s: &str) {
    if is_bare(s) {
        out.push_str(s);
        return;
    }
    out.push('"');
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            // Anything else non-printable. A value should never contain one,
            // but rendering it unescaped would write a file that does not
            // parse, which is a worse failure than an ugly one.
            c if (c as u32) < 0x20 || c as u32 == 0x7f => {
                out.push_str(&format!("\\x{:02x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

pub fn quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    quote_into(&mut out, s);
    out
}

pub struct Lexer {
    src: Vec<char>,
    i: usize,
    line: u32,
    column: u32,
}

impl Lexer {
    pub fn new(src: &str) -> Self {
        Self {
            src: src.chars().collect(),
            i: 0,
            line: 1,
            column: 1,
        }
    }

    pub fn pos(&self) -> Pos {
        Pos {
            line: self.line,
            column: self.column,
        }
    }

    fn peek(&self) -> Option<char> {
        self.src.get(self.i).copied()
    }

    fn peek_at(&self, ahead: usize) -> Option<char> {
        self.src.get(self.i + ahead).copied()
    }

    fn bump(&mut self) -> Option<char> {
        let c = self.src.get(self.i).copied()?;
        self.i += 1;
        if c == '\n' {
            self.line += 1;
            self.column = 1;
        } else {
            self.column += 1;
        }
        Some(c)
    }

    fn skip_whitespace(&mut self) {
        while matches!(self.peek(), Some(c) if c.is_whitespace()) {
            self.bump();
        }
    }

    /// The next token, or `None` at end of input.
    pub fn next_token(&mut self) -> Result<Option<Spanned>, LexError> {
        self.skip_whitespace();
        let pos = self.pos();
        let Some(c) = self.peek() else {
            return Ok(None);
        };

        // `/*` is checked before words even though `/` is a word character.
        // `*` is not, so a word can never run into a `/*` from the inside;
        // this is only about a token that starts with one.
        if c == '/' && self.peek_at(1) == Some('*') {
            return self.block_comment(pos).map(Some);
        }

        let token = match c {
            '{' => {
                self.bump();
                Token::Open
            }
            '}' => {
                self.bump();
                Token::Close
            }
            '#' => self.line_comment(),
            '"' => self.string(pos)?,
            c if is_bare_char(c) => self.word(),
            c => return Err(LexError::UnexpectedChar { pos, ch: c }),
        };
        Ok(Some(Spanned { token, pos }))
    }

    fn word(&mut self) -> Token {
        let mut s = String::new();
        while matches!(self.peek(), Some(c) if is_bare_char(c)) {
            s.push(self.bump().expect("peeked"));
        }
        Token::Value(s)
    }

    fn string(&mut self, open: Pos) -> Result<Token, LexError> {
        self.bump(); // opening quote
        let mut s = String::new();
        loop {
            let pos = self.pos();
            let Some(c) = self.bump() else {
                return Err(LexError::UnterminatedString { pos: open });
            };
            match c {
                '"' => return Ok(Token::Value(s)),
                '\\' => s.push(self.escape(pos)?),
                c => s.push(c),
            }
        }
    }

    fn escape(&mut self, backslash: Pos) -> Result<char, LexError> {
        let Some(c) = self.bump() else {
            return Err(LexError::UnterminatedString { pos: backslash });
        };
        Ok(match c {
            '\\' => '\\',
            '"' => '"',
            'n' => '\n',
            't' => '\t',
            'r' => '\r',
            'x' => {
                let mut v: u32 = 0;
                for _ in 0..2 {
                    let d = self
                        .peek()
                        .and_then(|c| c.to_digit(16))
                        .ok_or(LexError::BadHexEscape { pos: backslash })?;
                    self.bump();
                    v = v * 16 + d;
                }
                // Only ASCII. `\xff` in a UTF-8 file is a byte that cannot
                // mean what it looks like it means, and guessing Latin-1 is
                // how mojibake gets into a config file.
                if v > 0x7f {
                    return Err(LexError::BadHexEscape { pos: backslash });
                }
                char::from_u32(v).expect("ascii")
            }
            ch => return Err(LexError::UnknownEscape { pos: backslash, ch }),
        })
    }

    fn line_comment(&mut self) -> Token {
        self.bump(); // '#'
        let mut s = String::new();
        while let Some(c) = self.peek() {
            if c == '\n' {
                break;
            }
            s.push(self.bump().expect("peeked"));
        }
        // One optional leading space, so `# foo` and `#foo` both give `foo`
        // and the renderer's `# ` prefix round-trips.
        Token::Comment(s.strip_prefix(' ').unwrap_or(&s).to_string())
    }

    fn block_comment(&mut self, open: Pos) -> Result<Spanned, LexError> {
        self.bump(); // '/'
        self.bump(); // '*'
        let mut raw = String::new();
        loop {
            let Some(c) = self.peek() else {
                return Err(LexError::UnterminatedComment { pos: open });
            };
            if c == '*' && self.peek_at(1) == Some('/') {
                self.bump();
                self.bump();
                break;
            }
            raw.push(self.bump().expect("peeked"));
        }
        // Normalise to the same shape a `#` run produces, so the two forms are
        // interchangeable on input and there is only one form on output.
        let body = raw
            .lines()
            .map(|l| l.trim().trim_start_matches('*').trim_start())
            .collect::<Vec<_>>()
            .join("\n")
            .trim_matches('\n')
            .to_string();
        Ok(Spanned {
            token: Token::Comment(body),
            pos: open,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lex(src: &str) -> Result<Vec<Token>, LexError> {
        let mut l = Lexer::new(src);
        let mut out = Vec::new();
        while let Some(s) = l.next_token()? {
            out.push(s.token);
        }
        Ok(out)
    }

    fn val(s: &str) -> Token {
        Token::Value(s.to_string())
    }

    #[test]
    fn words_and_braces() {
        assert_eq!(
            lex("interfaces { ethernet eth0 }").unwrap(),
            vec![
                val("interfaces"),
                Token::Open,
                val("ethernet"),
                val("eth0"),
                Token::Close
            ]
        );
    }

    #[test]
    fn bare_words_cover_real_config_values() {
        for v in [
            "192.168.1.1/24",
            "2001:db8::1/64",
            "00:11:22:33:44:55",
            "fe80::1%eth0",
            "America/New_York",
            "layer2+3",
            "802.3ad",
            "balance-xor",
            "enp1s0",
            "9216",
        ] {
            assert_eq!(lex(v).unwrap(), vec![val(v)], "{v} should lex as one word");
            assert!(is_bare(v), "{v} should not need quoting");
        }
    }

    #[test]
    fn strings_and_escapes() {
        assert_eq!(lex(r#""hello world""#).unwrap(), vec![val("hello world")]);
        assert_eq!(lex(r#""a\"b""#).unwrap(), vec![val("a\"b")]);
        assert_eq!(lex(r#""a\\b""#).unwrap(), vec![val("a\\b")]);
        assert_eq!(lex(r#""a\nb""#).unwrap(), vec![val("a\nb")]);
        assert_eq!(lex(r#""\x07""#).unwrap(), vec![val("\x07")]);
        assert_eq!(lex(r#""""#).unwrap(), vec![val("")]);
    }

    #[test]
    fn comment_forms_normalise_to_the_same_text() {
        let hash = lex("# the uplink").unwrap();
        let block = lex("/* the uplink */").unwrap();
        assert_eq!(hash, vec![Token::Comment("the uplink".into())]);
        assert_eq!(hash, block);
        // No space after the marker is the same comment.
        assert_eq!(lex("#the uplink").unwrap(), hash);
    }

    #[test]
    fn block_comments_span_lines() {
        assert_eq!(
            lex("/* first\n * second\n */").unwrap(),
            vec![Token::Comment("first\nsecond".into())]
        );
    }

    #[test]
    fn slash_only_starts_a_comment_before_a_star() {
        assert_eq!(lex("/foo").unwrap(), vec![val("/foo")]);
        assert_eq!(lex("a/b").unwrap(), vec![val("a/b")]);
        assert_eq!(lex("/*x*/").unwrap(), vec![Token::Comment("x".into())]);
    }

    #[test]
    fn errors_carry_a_position() {
        let err = lex("ethernet\n  eth0 \"oops").unwrap_err();
        assert_eq!(err.pos(), Pos { line: 2, column: 8 });
        assert!(matches!(err, LexError::UnterminatedString { .. }));

        let err = lex("a\n\n   *").unwrap_err();
        assert_eq!(err.pos(), Pos { line: 3, column: 4 });

        assert!(matches!(
            lex(r#""\q""#).unwrap_err(),
            LexError::UnknownEscape { ch: 'q', .. }
        ));
        assert!(matches!(
            lex(r#""\xzz""#).unwrap_err(),
            LexError::BadHexEscape { .. }
        ));
        assert!(matches!(
            lex(r#""\xff""#).unwrap_err(),
            LexError::BadHexEscape { .. }
        ));
        assert!(matches!(
            lex("/* never closed").unwrap_err(),
            LexError::UnterminatedComment { .. }
        ));
    }

    #[test]
    fn quoting_is_the_inverse_of_lexing() {
        for v in [
            "",
            " ",
            "hello world",
            "a\"b",
            "a\\b",
            "line\nbreak",
            "tab\there",
            "\x07bell",
            "unicode: \u{e9}t\u{e9}",
            "#not-a-comment",
            "/*not-a-comment",
            "{braces}",
            "192.168.1.1/24",
        ] {
            let text = quote(v);
            assert_eq!(
                lex(&text).unwrap(),
                vec![val(v)],
                "{v:?} rendered as {text:?} did not read back"
            );
        }
    }
}
