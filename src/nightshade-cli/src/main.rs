//! `ns` -- the Nightshade operator CLI.

use std::process::ExitCode;
use std::sync::Arc;

use nightshade_cli::client::Client;
use nightshade_cli::complete::NsCompleter;
use nightshade_cli::output;
use nightshade_cli::repl::{Cli, Mode, NsPrompt, Outcome};
use nightshade_common::paths::Paths;
use nightshade_schema::model::Schema;
use reedline::{
    DefaultHinter, FileBackedHistory, Reedline, Signal, SimpleMatchHighlighter,
};

const USAGE: &str = "\
usage: ns [options]

  -c COMMAND     run one command and exit
  -f FILE        run the commands in FILE, then commit
  --json         render output as JSON
  --version      print the version and exit
  -h, --help     this

With no options, ns is an interactive session. It is also a login shell:
members of nightshade-admin land in operational mode.
";

fn main() -> ExitCode {
    let outcome = run();
    ExitCode::from(outcome.code())
}

/// Was this process started as somebody's login shell?
///
/// The convention every shell relies on: `login` execs the shell with an argv[0]
/// of `-ns` rather than `ns`. It is the only signal available, and it is the one
/// bash, sh and zsh all use.
fn is_login_shell() -> bool {
    std::env::args_os()
        .next()
        .map(|argv0| argv0_is_login_shell(&argv0.to_string_lossy()))
        .unwrap_or(false)
}

fn argv0_is_login_shell(argv0: &str) -> bool {
    // The dash is on the basename, so a full path still carries it: `login`
    // passes `-ns`, but a shell configured with an absolute path arrives as
    // `-/usr/bin/ns` on some systems.
    argv0.starts_with('-')
        || std::path::Path::new(argv0)
            .file_name()
            .map(|name| name.to_string_lossy().starts_with('-'))
            .unwrap_or(false)
}

/// What to do when the session cannot be started at all.
///
/// For a script or an `ns -c`, exiting non-zero is correct and expected.
///
/// As a login shell it is a trapdoor. `ns` is the login shell of the only
/// account on a Nightshade box, root is locked, and there is no second user.
/// A shell that exits the instant it starts returns the operator to `agetty`,
/// which clears the screen -- so the console blinks, a fresh `login:` appears,
/// and the message explaining why is destroyed on its way past. Correct
/// password, correct account, no diagnosis, no way in.
///
/// So in that one case the error is made unmissable and the operator is left in
/// a shell that can fix the box. This grants nothing: `shell` from operational
/// mode already drops to bash as the operator's own uid, so the same door is
/// open on a healthy system. The difference is that here it is the door out of
/// a machine that would otherwise be unreachable.
fn no_session(login_shell: bool, message: &str) -> Outcome {
    if !login_shell {
        eprintln!("{message}");
        return Outcome::CommandError;
    }

    eprintln!("\n{message}\n");
    eprintln!("Nightshade's configuration system is not answering, so there is no");
    eprintln!("operational mode to put you in. Dropping to a shell instead, because");
    eprintln!("the alternative is a console you cannot get past.\n");
    eprintln!("  systemctl status nightshade-configd.service");
    eprintln!("  journalctl -u nightshade-configd.service -b");
    eprintln!("\nFix the daemon and log in again for a normal session.\n");

    match nightshade_cli::shell::run() {
        Ok(()) => Outcome::CommandError,
        Err(e) => {
            eprintln!("ns: the fallback shell could not be started either: {e}");
            Outcome::CommandError
        }
    }
}

fn run() -> Outcome {
    let mut arguments = std::env::args().skip(1);
    let mut command = None;
    let mut file = None;
    let mut json = false;

    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "-c" => match arguments.next() {
                Some(value) => command = Some(value),
                None => return usage_error("-c needs a command"),
            },
            "-f" => match arguments.next() {
                Some(value) => file = Some(value),
                None => return usage_error("-f needs a file"),
            },
            "--json" => json = true,
            "--version" => {
                println!("Nightshade {}", nightshade_common::VERSION);
                return Outcome::Ok;
            }
            "-h" | "--help" => {
                print!("{USAGE}");
                return Outcome::Ok;
            }
            other => return usage_error(&format!("`{other}` is not an option")),
        }
    }

    let login_shell = is_login_shell();

    let paths = Paths::system();
    let mut client = match Client::connect(&paths.socket()) {
        Ok(client) => client,
        Err(e) => return no_session(login_shell, &e.to_string()),
    };
    // Fail before the prompt appears rather than on the first command.
    let _ = &mut client;

    let schema = Schema::compiled();
    let mut cli = Cli::new(client, schema, output::use_colour());

    match (command, file) {
        (Some(_), Some(_)) => usage_error("-c and -f cannot both be given"),
        (Some(command), None) => one_shot(&mut cli, &command, json),
        (None, Some(file)) => batch(&mut cli, &file),
        (None, None) => {
            let outcome = interactive(&mut cli);
            // `exit` returns Ok and should log the operator out; that is the
            // whole point of it. Anything else means the session died on its
            // own -- reedline failing to drive the terminal, most likely -- and
            // for a login shell that is the same trapdoor as never starting.
            if login_shell && outcome != Outcome::Ok {
                return no_session(login_shell, "the operational session ended unexpectedly");
            }
            outcome
        }
    }
}

fn usage_error(message: &str) -> Outcome {
    eprintln!("ns: {message}\n\n{USAGE}");
    Outcome::CommandError
}

/// `ns -c "show interfaces"`.
fn one_shot(cli: &mut Cli, command: &str, json: bool) -> Outcome {
    cli.refresh();
    let line = if json && !command.contains("| display") {
        format!("{command} | display json")
    } else {
        command.to_string()
    };
    cli.run(&line)
}

/// `ns -f batch-file`.
///
/// The whole file, then a commit. Stops at the first line that fails, because
/// the lines after it were written expecting the ones before to have worked --
/// carrying on would apply a configuration nobody wrote.
fn batch(cli: &mut Cli, file: &str) -> Outcome {
    let text = match std::fs::read_to_string(file) {
        Ok(text) => text,
        Err(e) => {
            eprintln!("ns: {file}: {e}");
            return Outcome::CommandError;
        }
    };

    cli.refresh();
    if cli.run("configure") != Outcome::Ok {
        return Outcome::CommandError;
    }

    for (number, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        cli.refresh();
        let outcome = cli.run(line);
        if outcome != Outcome::Ok {
            eprintln!("ns: {file}:{}: `{line}` failed; nothing has been committed", number + 1);
            // The candidate is thrown away, so a half-applied batch cannot be
            // committed later by somebody who did not run it.
            cli.run("exit discard");
            return outcome;
        }
    }

    cli.refresh();
    let outcome = cli.run("commit");
    if outcome != Outcome::Ok {
        cli.run("exit discard");
        return outcome;
    }
    cli.run("exit");
    Outcome::Ok
}

fn interactive(cli: &mut Cli) -> Outcome {
    let history = history_file()
        .and_then(|path| FileBackedHistory::with_file(2000, path).ok())
        .map(Box::new);

    let mut editor = Reedline::create()
        .with_completer(Box::new(NsCompleter::new(
            Schema::compiled(),
            Arc::clone(&cli.context),
        )))
        .with_hinter(Box::new(DefaultHinter::default()))
        // With an empty query this styles nothing: the typed line goes to the
        // terminal in its default colour, with no escape codes around it.
        //
        // Left alone, reedline installs its ExampleHighlighter, which restyles
        // the whole buffer White -- a colour change and its reset wrapped
        // around everything typed, re-emitted on every keystroke. `ns` is read
        // on consoles where every byte is drawn in software (the framebuffer
        // console of an appliance or a VM) or sent down a serial line, and a
        // repaint there is paid for per byte. Config text has no syntax to
        // highlight; the honest style is none.
        .with_highlighter(Box::new(SimpleMatchHighlighter::default()));
    if let Some(history) = history {
        editor = editor.with_history(history);
    }

    println!("Nightshade {}\n", nightshade_common::VERSION);
    warn_if_boot_failed(&Paths::system());

    loop {
        cli.refresh();
        let prompt = NsPrompt::new(cli.prompt(), cli.colour());

        match editor.read_line(&prompt) {
            Ok(Signal::Success(line)) => {
                if cli.run(&line) == Outcome::Leave {
                    break;
                }
            }
            // Ctrl-C abandons the line, as it does in every other shell.
            Ok(Signal::CtrlC) => continue,
            Ok(Signal::CtrlD) => {
                // Ctrl-D in configuration mode goes through `exit`, so it
                // cannot silently discard uncommitted work.
                if cli.mode == Mode::Configuration {
                    if cli.run("exit") == Outcome::ConfigError {
                        continue;
                    }
                } else {
                    break;
                }
            }
            // Nothing binds `ExecuteHostCommand`, so nothing produces this.
            // Treated as a line anyway rather than ignored, so that binding
            // something to it later cannot silently do nothing.
            Ok(Signal::HostCommand(line)) => {
                if cli.run(&line) == Outcome::Leave {
                    break;
                }
            }
            // `Signal` is non-exhaustive. A variant added by a future reedline
            // is not a reason to leave an operator's session, so it redraws
            // the prompt and waits.
            Ok(_) => continue,
            Err(e) => {
                eprintln!("ns: {e}");
                return Outcome::CommandError;
            }
        }
    }
    Outcome::Ok
}

/// Say so, before the first prompt, if the box came up on defaults.
///
/// configd records the reason at startup and logs it. The journal is correct
/// and is not where somebody logging in to fix a firewall looks first -- and
/// somebody logging into a box whose configuration did not load is about to
/// wonder loudly where it went.
fn warn_if_boot_failed(paths: &Paths) {
    let Ok(reason) = std::fs::read_to_string(paths.boot_failure()) else {
        return;
    };
    eprintln!("WARNING: this system started with its DEFAULT configuration.");
    eprintln!("{}", reason.trim_end());
    eprintln!(
        "\nThe saved configuration is still in {}. Fix it and `load`,\n\
         or configure the system and `commit`.\n",
        paths.config_boot().display()
    );
}

fn history_file() -> Option<std::path::PathBuf> {
    std::env::var_os("HOME").map(|home| std::path::PathBuf::from(home).join(".nightshade_history"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Getting this wrong in either direction matters: miss the dash and the
    /// only account on the box loses its way out of a broken configd; see one
    /// that is not there and `ns -c ...` in a script stops failing when it
    /// should.
    #[test]
    fn login_shells_are_recognised_by_the_leading_dash() {
        assert!(argv0_is_login_shell("-ns"));
        assert!(argv0_is_login_shell("-nightshade"));
        assert!(argv0_is_login_shell("-/usr/bin/ns"));
    }

    #[test]
    fn ordinary_invocations_are_not_login_shells() {
        assert!(!argv0_is_login_shell("ns"));
        assert!(!argv0_is_login_shell("/usr/bin/ns"));
        assert!(!argv0_is_login_shell("nightshade"));
        assert!(!argv0_is_login_shell(""));
    }
}
