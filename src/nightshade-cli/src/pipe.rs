//! Pipe modifiers.
//!
//! `show interfaces | match eth0 | count` reads like a shell pipeline and is
//! not one. Nothing is spawned, nothing is forked, and the text never reaches
//! a program. Each modifier is a function from the output to the output,
//! applied in order, inside this process.
//!
//! That is a security property and not a stylistic one. `ns` is somebody's
//! login shell; a `|` that reached `sh -c` would be a shell escape with a
//! friendly name.
//!
//! # Splitting on `|`
//!
//! Done with quote awareness rather than `str::split`, so
//! `set ... description "a | b"` keeps its pipe. The config lexer is not used
//! here because a modifier list is not config syntax and a half-typed command
//! should not produce a lexer error about it.

use regex::Regex;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Modifier {
    /// Keep lines matching a regular expression.
    Match(String),
    /// Replace the output with the number of lines.
    Count,
    /// Do not page.
    NoMore,
    /// Render as JSON rather than as text.
    DisplayJson,
    /// Show values the schema marks secret.
    DisplaySecrets,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PipeError {
    #[error("`| {0}` is not a pipe modifier; try match, count, no-more or display")]
    Unknown(String),

    #[error("`| match` needs something to match")]
    MatchNeedsPattern,

    #[error("`| match {pattern}` is not a valid regular expression: {reason}")]
    BadPattern { pattern: String, reason: String },

    #[error("`| display` needs `json` or `secrets`")]
    DisplayNeedsFormat,

    #[error("`| display {0}` is not something that can be displayed; try json or secrets")]
    UnknownDisplay(String),

    #[error("a pipe with nothing after it")]
    Empty,
}

/// Split a typed line into the command and its modifiers.
pub fn split(line: &str) -> Result<(String, Vec<Modifier>), PipeError> {
    let parts = split_unquoted(line);
    let (command, rest) = parts.split_first().expect("split always yields one part");

    let modifiers = rest
        .iter()
        .map(|part| parse(part.trim()))
        .collect::<Result<_, _>>()?;
    Ok((command.trim().to_string(), modifiers))
}

fn split_unquoted(line: &str) -> Vec<String> {
    let mut parts = vec![String::new()];
    let mut quoted = false;
    let mut escaped = false;

    for c in line.chars() {
        let current = parts.last_mut().expect("never empty");
        match c {
            _ if escaped => {
                current.push(c);
                escaped = false;
            }
            '\\' => {
                current.push(c);
                escaped = true;
            }
            '"' => {
                current.push(c);
                quoted = !quoted;
            }
            '|' if !quoted => parts.push(String::new()),
            _ => current.push(c),
        }
    }
    parts
}

fn parse(text: &str) -> Result<Modifier, PipeError> {
    let mut words = text.split_whitespace();
    let Some(name) = words.next() else {
        return Err(PipeError::Empty);
    };
    let rest: Vec<&str> = words.collect();

    match name {
        "match" => {
            if rest.is_empty() {
                return Err(PipeError::MatchNeedsPattern);
            }
            let pattern = rest.join(" ");
            // Compiled now so a bad pattern is reported before any output is
            // produced, rather than after half of it has scrolled past.
            Regex::new(&pattern).map_err(|e| PipeError::BadPattern {
                pattern: pattern.clone(),
                reason: e.to_string().lines().last().unwrap_or("").trim().to_string(),
            })?;
            Ok(Modifier::Match(pattern))
        }
        "count" => Ok(Modifier::Count),
        "no-more" => Ok(Modifier::NoMore),
        "display" => match rest.first() {
            None => Err(PipeError::DisplayNeedsFormat),
            Some(&"json") => Ok(Modifier::DisplayJson),
            Some(&"secrets") => Ok(Modifier::DisplaySecrets),
            Some(other) => Err(PipeError::UnknownDisplay((*other).to_string())),
        },
        other => Err(PipeError::Unknown(other.to_string())),
    }
}

/// Apply the text-level modifiers. `display` is handled before rendering.
pub fn apply(text: String, modifiers: &[Modifier]) -> String {
    let mut text = text;
    for modifier in modifiers {
        text = match modifier {
            Modifier::Match(pattern) => {
                // Already compiled once in `parse`; a failure here is
                // impossible, and falling back to the unfiltered text is
                // better than a panic in a login shell.
                match Regex::new(pattern) {
                    Ok(regex) => text
                        .lines()
                        .filter(|line| regex.is_match(line))
                        .map(|line| format!("{line}\n"))
                        .collect(),
                    Err(_) => text,
                }
            }
            Modifier::Count => {
                let lines = text.lines().filter(|line| !line.trim().is_empty()).count();
                format!("{lines}\n")
            }
            Modifier::NoMore | Modifier::DisplayJson | Modifier::DisplaySecrets => text,
        };
    }
    text
}

pub fn wants(modifiers: &[Modifier], wanted: &Modifier) -> bool {
    modifiers.contains(wanted)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_command_with_no_pipes_is_itself() {
        let (command, modifiers) = split("show interfaces").unwrap();
        assert_eq!(command, "show interfaces");
        assert!(modifiers.is_empty());
    }

    #[test]
    fn modifiers_are_parsed_in_order() {
        let (command, modifiers) = split("show configuration | match eth0 | count").unwrap();
        assert_eq!(command, "show configuration");
        assert_eq!(
            modifiers,
            [Modifier::Match("eth0".into()), Modifier::Count]
        );
    }

    /// The reason this does not use `str::split`.
    #[test]
    fn a_pipe_inside_a_quoted_value_is_part_of_the_value() {
        let (command, modifiers) =
            split(r#"set interfaces ethernet eth0 description "left | right""#).unwrap();
        assert_eq!(
            command,
            r#"set interfaces ethernet eth0 description "left | right""#
        );
        assert!(modifiers.is_empty());

        // And a real modifier after a quoted pipe still works.
        let (command, modifiers) = split(r#"show configuration | match "a | b""#).unwrap();
        assert_eq!(command, "show configuration");
        assert_eq!(modifiers, [Modifier::Match(r#""a | b""#.into())]);
    }

    #[test]
    fn an_escaped_quote_does_not_open_a_quoted_run() {
        let (command, modifiers) = split(r#"set a b "c\"d" | count"#).unwrap();
        assert_eq!(command, r#"set a b "c\"d""#);
        assert_eq!(modifiers, [Modifier::Count]);
    }

    #[test]
    fn bad_modifiers_are_refused_with_something_to_do_about_it() {
        assert!(matches!(
            split("show configuration | grep eth0"),
            Err(PipeError::Unknown(_))
        ));
        assert!(matches!(
            split("show configuration | match"),
            Err(PipeError::MatchNeedsPattern)
        ));
        assert!(matches!(
            split("show configuration | match ["),
            Err(PipeError::BadPattern { .. })
        ));
        assert!(matches!(
            split("show configuration | display"),
            Err(PipeError::DisplayNeedsFormat)
        ));
        assert!(matches!(
            split("show configuration | display xml"),
            Err(PipeError::UnknownDisplay(_))
        ));
        assert!(matches!(
            split("show configuration |"),
            Err(PipeError::Empty)
        ));
    }

    #[test]
    fn match_keeps_matching_lines() {
        let text = "eth0 up\neth1 down\nbond0 up\n".to_string();
        let filtered = apply(text, &[Modifier::Match("^eth".into())]);
        assert_eq!(filtered, "eth0 up\neth1 down\n");
    }

    #[test]
    fn count_counts_lines_that_have_something_on_them() {
        let text = "one\n\ntwo\nthree\n".to_string();
        assert_eq!(apply(text, &[Modifier::Count]), "3\n");
    }

    #[test]
    fn modifiers_compose_left_to_right() {
        let text = "eth0 up\neth1 down\nbond0 up\n".to_string();
        let out = apply(
            text,
            &[Modifier::Match("up".into()), Modifier::Count],
        );
        assert_eq!(out, "2\n");
    }

    /// Nothing here may ever produce something that gets executed.
    #[test]
    fn modifiers_are_data_and_never_a_command() {
        let (command, modifiers) = split("show configuration | match $(rm -rf /)").unwrap();
        assert_eq!(command, "show configuration");
        // Parsed as a pattern, held as a string, and used only by the regex
        // engine.
        assert_eq!(modifiers.len(), 1);
        let out = apply("nothing here\n".into(), &modifiers);
        assert_eq!(out, "");
    }
}
