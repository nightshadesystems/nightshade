//! nightshade-installer -- the Nightshade disk installer.
//!
//! Structure: `engine` owns the flow and every destructive action, `ui` owns
//! presentation. The engine talks to a `Frontend` trait, so the plain
//! line-based prompt flow and the TUI are interchangeable and identical in
//! behaviour -- including the fallback path where the TUI cannot start on a
//! serial console and the same install runs against the plain frontend.

mod cmd;
mod config;
mod disk;
mod engine;
mod error;
mod logging;
mod secret;
mod ui;
mod validate;

use std::process::ExitCode;

use error::Error;
use ui::{FinalAction, Frontend};

fn main() -> ExitCode {
    let args = Args::parse();

    if args.help {
        print_help();
        return ExitCode::SUCCESS;
    }
    if args.version {
        println!("nightshade-installer {}", env!("CARGO_PKG_VERSION"));
        return ExitCode::SUCCESS;
    }

    logging::init(&args.log_path);
    logging::info(format!(
        "nightshade-installer {} starting",
        env!("CARGO_PKG_VERSION")
    ));
    if !logging::is_active() {
        eprintln!(
            "warning: could not open {} for writing; continuing without a log",
            args.log_path
        );
    }

    let mut frontend = select_frontend(&args);

    match engine::run(frontend.as_mut()) {
        Ok(FinalAction::Reboot) => {
            logging::info("rebooting at operator request");
            println!("\n  Rebooting...\n");
            // If reboot fails we still exit cleanly: the install succeeded, and
            // the live session drops the operator to a shell where they can
            // reboot by hand.
            let _ = cmd::Cmd::new("systemctl").arg("reboot").run_lenient();
            ExitCode::SUCCESS
        }
        Ok(FinalAction::Shell) => {
            println!("\n  Exiting to a shell. Reboot when ready.\n");
            ExitCode::SUCCESS
        }
        Err(Error::Aborted) => {
            logging::info("cancelled by the operator");
            println!("\n  Cancelled. No changes were made.\n");
            ExitCode::SUCCESS
        }
        Err(e) => {
            logging::error(format!("exiting with failure: {e}"));
            // The frontend has already rendered the detail; this is for
            // anything scraping stderr.
            eprintln!("nightshade-installer: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Choose a frontend.
///
/// Only the plain frontend exists today. When the TUI lands this is where it
/// is attempted first and where a failure to initialise falls back, which is
/// why the decision is isolated in one function rather than spread through
/// main.
fn select_frontend(args: &Args) -> Box<dyn Frontend> {
    if args.plain {
        logging::info("frontend: plain (forced by --plain)");
        return Box::new(ui::plain::PlainFrontend::new());
    }

    if !terminal_supports_tui() {
        logging::info(format!(
            "frontend: plain (TERM={} cannot support a full-screen interface)",
            std::env::var("TERM").unwrap_or_else(|_| "<unset>".into())
        ));
        return Box::new(ui::plain::PlainFrontend::new());
    }

    #[cfg(feature = "tui")]
    match ui::tui::TuiFrontend::new() {
        Ok(tui) => {
            logging::info("frontend: tui");
            return Box::new(tui);
        }
        Err(e) => {
            // Not fatal, and the reason matters: this is exactly the serial
            // console case the plain frontend exists for.
            logging::warn(format!("TUI could not start ({e}); falling back to plain"));
            eprintln!("  (full-screen interface unavailable: {e})");
        }
    }

    logging::info("frontend: plain");
    Box::new(ui::plain::PlainFrontend::new())
}

/// Is this terminal worth trying a full-screen interface on?
///
/// A serial console reporting `vt100`, or anything reporting `dumb`, gets the
/// plain flow: a TUI on a 9600-baud line is unusable even when it technically
/// renders.
fn terminal_supports_tui() -> bool {
    match std::env::var("TERM") {
        Ok(term) => !matches!(term.as_str(), "" | "dumb" | "vt100" | "vt102" | "unknown"),
        Err(_) => false,
    }
}

struct Args {
    help: bool,
    version: bool,
    plain: bool,
    log_path: String,
}

impl Args {
    fn parse() -> Self {
        let mut args = Args {
            help: false,
            version: false,
            plain: false,
            log_path: logging::DEFAULT_LOG_PATH.to_string(),
        };

        let mut argv = std::env::args().skip(1);
        while let Some(arg) = argv.next() {
            match arg.as_str() {
                "-h" | "--help" => args.help = true,
                "-V" | "--version" => args.version = true,
                "--plain" => args.plain = true,
                "--log" => {
                    if let Some(path) = argv.next() {
                        args.log_path = path;
                    }
                }
                other => {
                    eprintln!("nightshade-installer: unknown argument: {other}");
                    args.help = true;
                }
            }
        }

        args
    }
}

fn print_help() {
    println!(
        "\
nightshade-installer {version}

Installs Nightshade to disk with ZFS, optionally as a two-disk RAID1 mirror.
Must be run as root from the Nightshade live medium.

usage: nightshade-installer [options]

options:
  --plain         force the line-based frontend (the default on a serial
                  console or a terminal that cannot support a full-screen UI)
  --log PATH      write the install log here (default: {log})
  -V, --version   print the version
  -h, --help      this help
",
        version = env!("CARGO_PKG_VERSION"),
        log = logging::DEFAULT_LOG_PATH
    );
}
