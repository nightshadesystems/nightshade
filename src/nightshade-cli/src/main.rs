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
    DefaultHinter, FileBackedHistory, Reedline, Signal,
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

    let paths = Paths::system();
    let mut client = match Client::connect(&paths.socket()) {
        Ok(client) => client,
        Err(e) => {
            eprintln!("{e}");
            return Outcome::CommandError;
        }
    };
    // Fail before the prompt appears rather than on the first command.
    let _ = &mut client;

    let schema = Schema::compiled();
    let mut cli = Cli::new(client, schema, output::use_colour());

    match (command, file) {
        (Some(_), Some(_)) => usage_error("-c and -f cannot both be given"),
        (Some(command), None) => one_shot(&mut cli, &command, json),
        (None, Some(file)) => batch(&mut cli, &file),
        (None, None) => interactive(&mut cli),
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
        .with_hinter(Box::new(DefaultHinter::default()));
    if let Some(history) = history {
        editor = editor.with_history(history);
    }

    println!("Nightshade {}", nightshade_common::VERSION);
    println!("`?` lists what can go here, <Tab> completes it, `exit` leaves.\n");

    loop {
        cli.refresh();
        let prompt = NsPrompt { text: cli.prompt() };

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

fn history_file() -> Option<std::path::PathBuf> {
    std::env::var_os("HOME").map(|home| std::path::PathBuf::from(home).join(".nightshade_history"))
}
