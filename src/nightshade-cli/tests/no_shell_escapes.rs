//! An audit of the source, not of the behaviour.
//!
//! [`session::nothing_typed_reaches_a_shell`] checks that the things somebody
//! would try do not work. This checks the property that makes that true and
//! keeps it true: there is exactly one place in the CLI that names a shell,
//! and every program it starts is named by a literal.
//!
//! A behaviour test can only cover the escapes somebody thought of. This
//! covers the ones they did not, by failing when a future edit introduces the
//! shape.

use std::path::{Path, PathBuf};

fn sources(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            out.extend(sources(&path));
        } else if path.extension().and_then(|e| e.to_str()) == Some("rs") {
            out.push(path);
        }
    }
    out.sort();
    out
}

fn crate_sources() -> Vec<(PathBuf, String)> {
    sources(&Path::new(env!("CARGO_MANIFEST_DIR")).join("src"))
        .into_iter()
        .filter_map(|path| {
            let text = std::fs::read_to_string(&path).ok()?;
            Some((path, text))
        })
        .collect()
}

/// Line number, the code with any comment stripped, and the whole line.
///
/// The needles are matched against the code, so prose about shells does not
/// trip an audit that is about shells. The whole line comes back too, because
/// a waiver is written in the comment the stripping removes.
fn code_lines(text: &str) -> impl Iterator<Item = (usize, &str, &str)> {
    text.lines().enumerate().filter_map(|(i, line)| {
        let trimmed = line.trim_start();
        if trimmed.starts_with("//") {
            return None;
        }
        let code = match line.find("//") {
            Some(at) => &line[..at],
            None => line,
        };
        Some((i + 1, code, line))
    })
}

/// The one file allowed to know what bash is.
const SHELL_MODULE: &str = "shell.rs";

/// What a line says about itself to be allowed a bare `"sh"`.
///
/// `sh` is the only reserved word here that is also an ordinary English
/// abbreviation -- the CLI has `sh` as an alias for `show` -- so it is the only
/// one that can be waived. `/bin/sh`, `"bash"` and `/bin/bash` name a program
/// and nothing else, and no comment excuses them.
///
/// A waiver is not a way around the audit. It is a marker that survives grep,
/// shows up in a diff, and has to be argued for in review; the audit's job is
/// to make naming a shell a decision somebody made on purpose, not to make it
/// impossible to write the two letters down.
const WAIVER: &str = "not-a-shell:";

#[test]
fn only_one_file_names_a_shell() {
    // Not `-c`: the CLI has an option by that name, and a `-c` next to no
    // shell is a flag. The check that matters is below.
    let forbidden = ["/bin/sh", "\"sh\"", "\"bash\"", "/bin/bash"];

    for (path, text) in crate_sources() {
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        for (number, code, whole) in code_lines(&text) {
            for needle in forbidden {
                if !code.contains(needle) {
                    continue;
                }
                if needle == "\"sh\"" && whole.contains(WAIVER) {
                    continue;
                }
                assert_eq!(
                    name, SHELL_MODULE,
                    "{}:{number} names a shell ({needle}); \
                     the only file allowed to is {SHELL_MODULE}\n  {}",
                    path.display(),
                    whole.trim()
                );
            }
        }
    }
}

/// A waiver has to say something. An empty marker is a comment somebody added
/// to make a test stop failing, which is the one use of it that is not allowed.
#[test]
fn every_waiver_gives_a_reason() {
    for (path, text) in crate_sources() {
        for (number, _, whole) in code_lines(&text) {
            let Some(at) = whole.find(WAIVER) else {
                continue;
            };
            let reason = whole[at + WAIVER.len()..].trim();
            assert!(
                reason.len() > 10,
                "{}:{number} waives the shell audit without saying why\n  {}",
                path.display(),
                whole.trim()
            );
        }
    }
}

/// The shell is started interactively, never handed a string to run.
///
/// `bash --rcfile X -i` gives an operator a session. `bash -c "..."` executes
/// something, and there is no version of that which is not an escape.
#[test]
fn the_shell_is_never_given_a_command_to_run() {
    let shell = std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("src").join(SHELL_MODULE),
    )
    .expect("the shell module");

    for (number, line, _) in code_lines(&shell) {
        assert!(
            !line.contains("\"-c\""),
            "{SHELL_MODULE}:{number} passes -c to a shell\n  {}",
            line.trim()
        );
    }
    assert!(
        shell.contains(r#".arg("-i")"#),
        "the shell is started without -i, so it is not an interactive session"
    );
}

/// Every program started is named by a literal.
///
/// This is the check that matters. `Command::new(something)` where `something`
/// came from a typed line is a shell escape whatever the program turns out to
/// be -- and it would pass every test that only tries `!id` and backticks.
#[test]
fn every_program_started_is_named_by_a_literal() {
    for (path, text) in crate_sources() {
        for (number, line, _) in code_lines(&text) {
            let Some(at) = line.find("Command::new(") else {
                continue;
            };
            let rest = &line[at + "Command::new(".len()..];
            let literal = rest.starts_with('"')
                // `Command::new(program)` where `program` is one of a fixed
                // set is the shape `ping`/`traceroute` use; the audit below
                // pins that set.
                || rest.starts_with("program)");
            assert!(
                literal,
                "{}:{number} starts a program that is not named by a literal\n  {}",
                path.display(),
                line.trim()
            );
        }
    }
}

/// The one indirect program name, pinned to the two it can be.
#[test]
fn the_only_indirect_program_comes_from_a_fixed_set() {
    let repl = std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("src/repl.rs"),
    )
    .expect("repl.rs");

    // `trace_or_ping` is called with a literal at every call site, and there
    // are exactly two of them.
    let calls: Vec<&str> = repl
        .lines()
        .filter(|line| line.contains("self.trace_or_ping("))
        .collect();
    assert_eq!(calls.len(), 2, "{calls:#?}");
    assert!(calls.iter().any(|line| line.contains(r#""ping""#)), "{calls:#?}");
    assert!(
        calls.iter().any(|line| line.contains(r#""traceroute""#)),
        "{calls:#?}"
    );
}

/// Nothing builds a command line as a string and hands it over.
#[test]
fn nothing_reaches_a_process_through_a_string() {
    for (path, text) in crate_sources() {
        for (number, line, _) in code_lines(&text) {
            for needle in ["libc::system", "process::exit(", "exec(", "execvp"] {
                assert!(
                    !line.contains(needle),
                    "{}:{number} uses {needle}\n  {}",
                    path.display(),
                    line.trim()
                );
            }
        }
    }
}

/// The daemon is worth auditing too: it runs as root and spawns tools.
#[test]
fn the_daemon_and_the_renderers_start_nothing_through_a_shell() {
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("src/")
        .to_path_buf();

    for crate_name in ["nightshade-configd", "nightshade-render"] {
        for path in sources(&workspace.join(crate_name).join("src")) {
            let text = std::fs::read_to_string(&path).expect("a source file");
            // No waiver here. configd runs as root and has no operator-facing
            // vocabulary to collide with, so it has no reason to write any of
            // these and no way to excuse doing it.
            for (number, line, _) in code_lines(&text) {
                for needle in ["/bin/sh", "\"sh\"", "\"bash\"", "libc::system"] {
                    assert!(
                        !line.contains(needle),
                        "{}:{number} names a shell ({needle})\n  {}",
                        path.display(),
                        line.trim()
                    );
                }
                if let Some(at) = line.find("Command::new(") {
                    let rest = &line[at + "Command::new(".len()..];
                    assert!(
                        rest.starts_with('"') || rest.starts_with("program)"),
                        "{}:{number} starts a program that is not named by a literal\n  {}",
                        path.display(),
                        line.trim()
                    );
                }
            }
        }
    }
}
