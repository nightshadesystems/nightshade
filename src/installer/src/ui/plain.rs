//! Line-based frontend.
//!
//! The fallback when the TUI cannot initialise, and the only sane choice on a
//! serial console or a TERM the terminal library does not understand. It
//! assumes nothing beyond an 80-column dumb terminal: no cursor addressing, no
//! colour, no alternate screen. If this frontend cannot drive the install, the
//! machine has bigger problems than the installer.

use std::io::{BufRead, Write};
use std::path::Path;
use std::process::Command;

use super::{Frontend, FinalAction, Progress, text, validate_selection};
use crate::config::{InstallConfig, Layout};
use crate::disk::Disk;
use crate::error::{Error, Result};
use crate::logging;
use crate::secret::SecretString;
use crate::validate;

const WIDTH: usize = 74;

pub struct PlainFrontend {
    /// Set once the destructive phase starts, so step numbering reads sanely.
    in_progress: bool,
}

impl Default for PlainFrontend {
    fn default() -> Self {
        Self::new()
    }
}

impl PlainFrontend {
    pub fn new() -> Self {
        PlainFrontend { in_progress: false }
    }

    fn rule(&self) {
        println!("{}", "-".repeat(WIDTH));
    }

    fn heading(&self, title: &str) {
        println!();
        self.rule();
        println!("  {title}");
        self.rule();
        println!();
    }

    fn para(&self, body: &str) {
        for line in body.lines() {
            println!("  {line}");
        }
        println!();
    }

    /// Read one line. EOF means the console went away; treat it as an abort
    /// rather than looping forever on an empty read.
    fn read_line(&self) -> Result<String> {
        let mut buf = String::new();
        let n = std::io::stdin()
            .lock()
            .read_line(&mut buf)
            .map_err(|e| Error::Io {
                context: "reading from the console".into(),
                source: e,
            })?;
        if n == 0 {
            return Err(Error::Aborted);
        }
        Ok(buf.trim_end_matches(['\n', '\r']).to_string())
    }

    fn prompt(&self, label: &str) -> Result<String> {
        print!("  {label}");
        std::io::stdout().flush().ok();
        self.read_line()
    }

    /// Prompt with echo disabled.
    fn prompt_secret(&self, label: &str) -> Result<SecretString> {
        print!("  {label}");
        std::io::stdout().flush().ok();

        let _guard = EchoGuard::disable();
        let line = self.read_line();
        // The newline the operator typed was not echoed, so supply one.
        println!();
        Ok(SecretString::new(line?))
    }

    /// Yes/no with no default. Anything but an explicit yes is a no.
    fn confirm_yes(&self, label: &str) -> Result<bool> {
        let answer = self.prompt(label)?;
        Ok(matches!(answer.trim().to_ascii_lowercase().as_str(), "yes" | "y"))
    }

    fn show_disk(&self, index: usize, disk: &Disk) {
        println!(
            "  [{}] {:<14} {:>10}  {}",
            index + 1,
            disk.path.display(),
            disk.size_human(),
            disk.model
        );
        println!("      {}  serial {}", disk.media(), disk.serial);
        if disk.signatures.is_empty() {
            println!("      no partition table detected");
        } else {
            println!("      existing contents:");
            for sig in &disk.signatures {
                println!("        {sig}");
            }
        }
        println!();
    }
}

impl Frontend for PlainFrontend {
    fn welcome(&mut self) -> Result<()> {
        self.heading(text::TITLE);
        self.para(text::WELCOME_WARNING);
        let answer = self.prompt("Press Enter to continue, or type 'quit' to abort: ")?;
        if answer.trim().eq_ignore_ascii_case("quit") {
            return Err(Error::Aborted);
        }
        Ok(())
    }

    fn select_disks(&mut self, disks: &[Disk]) -> Result<Vec<usize>> {
        loop {
            self.heading("Select installation target");
            for (i, disk) in disks.iter().enumerate() {
                self.show_disk(i, disk);
            }
            self.para(text::DISK_HELP);

            let answer = self.prompt("Disks: ")?;
            if answer.trim().eq_ignore_ascii_case("quit") {
                return Err(Error::Aborted);
            }

            // Accept "1 2", "1,2" and "12" is deliberately NOT accepted -- an
            // ambiguous parse here erases the wrong disk.
            let parsed: std::result::Result<Vec<usize>, _> = answer
                .split(|c: char| c.is_whitespace() || c == ',')
                .filter(|s| !s.is_empty())
                .map(|s| s.parse::<usize>())
                .collect();

            let selection = match parsed {
                Ok(v) if v.iter().all(|n| *n >= 1) => {
                    v.into_iter().map(|n| n - 1).collect::<Vec<_>>()
                }
                _ => {
                    println!("\n  Enter disk numbers from the list, e.g. 1 or 1 2.\n");
                    continue;
                }
            };

            match validate_selection(&selection, disks.len()) {
                Ok(()) => return Ok(selection),
                Err(e) => println!("\n  {e}\n"),
            }
        }
    }

    fn confirm_warning(&mut self, message: &str) -> Result<()> {
        println!();
        self.para(message);
        if self.confirm_yes("Continue anyway? Type yes: ")? {
            Ok(())
        } else {
            Err(Error::Aborted)
        }
    }

    fn confirm_destruction(&mut self, disks: &[&Disk], layout: Layout) -> Result<()> {
        self.heading("Confirm destruction");

        println!("  These disks will be COMPLETELY ERASED:");
        println!();
        for disk in disks {
            println!(
                "    {:<14} {:>10}  {}  ({})",
                disk.path.display(),
                disk.size_human(),
                disk.model,
                disk.serial
            );
            for sig in &disk.signatures {
                println!("        destroying: {sig}");
            }
        }
        println!();
        println!("  Layout:     {}", layout.label());
        println!("  Boot pool:  {} (2 GiB per disk)", crate::config::BPOOL);
        println!("  Root pool:  {} (remaining space)", crate::config::RPOOL);
        println!();
        println!("  This cannot be undone.");
        println!();

        // No default. The operator types the word or nothing happens.
        let typed = self.prompt(&format!(
            "Type {} to continue, anything else cancels: ",
            validate::ERASE_CONFIRMATION
        ))?;

        match validate::erase_confirmation(&typed) {
            Ok(()) => Ok(()),
            Err(_) => {
                println!("\n  Not confirmed. Nothing has been changed.\n");
                Err(Error::Aborted)
            }
        }
    }

    fn collect_password(&mut self, user: &str) -> Result<SecretString> {
        self.heading("Administrator account");
        println!("  Account: {user}");
        println!();
        self.para(text::PASSWORD_HELP);

        loop {
            let first = self.prompt_secret(&format!(
                "Password (at least {} characters): ",
                validate::MIN_PASSWORD_LEN
            ))?;
            let second = self.prompt_secret("Confirm password: ")?;

            match validate::password(&first, &second) {
                Ok(()) => return Ok(first),
                // Re-prompt rather than abort: there is no path past this
                // screen without a valid password.
                Err(e) => println!("\n  {e}\n"),
            }
        }
    }

    fn collect_hostname(&mut self, default: &str) -> Result<String> {
        self.heading("Hostname");
        loop {
            let answer = self.prompt(&format!("Hostname [{default}]: "))?;
            let candidate = if answer.trim().is_empty() {
                default.to_string()
            } else {
                answer.trim().to_string()
            };

            match validate::hostname(&candidate) {
                Ok(()) => return Ok(candidate),
                Err(e) => println!("\n  {e}\n"),
            }
        }
    }

    fn confirm_summary(&mut self, config: &InstallConfig) -> Result<()> {
        self.heading("Summary");

        println!("  Layout:     {}", config.layout().label());
        for (i, disk) in config.disks.iter().enumerate() {
            println!(
                "  Disk {}:     {} ({}, {})",
                i + 1,
                disk.path.display(),
                disk.size_human(),
                disk.serial
            );
        }
        println!();
        println!("  Boot pool:  {} -> /boot", crate::config::BOOT_DATASET);
        println!("  Root pool:  {} -> /", crate::config::ROOT_DATASET);
        println!("              separate datasets for /var/log and /var/tmp");
        println!("  ESP:        512 MiB FAT32 on each disk");
        println!();
        println!("  Hostname:   {}", config.hostname);
        println!("  Account:    {} (sudo)", validate::DEFAULT_USER);
        println!("  Root login: locked");
        println!();

        if self.confirm_yes("Begin installation? Type yes: ")? {
            Ok(())
        } else {
            println!("\n  Cancelled. Nothing has been changed.\n");
            Err(Error::Aborted)
        }
    }

    fn progress(&mut self, progress: Progress) {
        match progress {
            Progress::Step { index, total, name } => {
                if !self.in_progress {
                    self.heading("Installing");
                    self.in_progress = true;
                }
                println!("  [{index}/{total}] {name}");
            }
            Progress::Detail(detail) => println!("        {detail}"),
            Progress::Warning(warning) => println!("        ! {warning}"),
        }
        std::io::stdout().flush().ok();
    }

    fn failed(&mut self, err: &Error, log_path: &Path) {
        println!();
        self.rule();
        println!("  INSTALLATION FAILED");
        self.rule();
        println!();
        println!("  {err}");
        println!();

        // The full detail: the argv that failed and everything it printed.
        let detail = err.detail();
        if detail.trim() != err.to_string().trim() {
            for line in detail.lines() {
                println!("  {line}");
            }
            println!();
        }

        println!("  Full log: {}", log_path.display());
        println!();
        println!("  The disks are in whatever state the failing step left them.");
        println!("  Re-running the installer will start again from scratch.");
        println!();
        std::io::stdout().flush().ok();
    }

    fn finished(&mut self, log_path: &Path) -> Result<FinalAction> {
        self.heading("Installation complete");
        println!("  Nightshade OS has been installed.");
        println!();
        println!("  Remove the installation medium before rebooting.");
        println!("  Log copied to /var/log/nightshade-install.log on the new system.");
        println!("  Log on this live system: {}", log_path.display());
        println!();

        loop {
            let answer = self.prompt("Reboot now? [r]eboot / [s]hell: ")?;
            match answer.trim().to_ascii_lowercase().as_str() {
                "r" | "reboot" | "" => return Ok(FinalAction::Reboot),
                "s" | "shell" => return Ok(FinalAction::Shell),
                _ => println!("\n  Enter r or s.\n"),
            }
        }
    }
}

/// Turns terminal echo off for as long as it is alive.
///
/// Uses stty rather than raw termios so the installer keeps its zero-dependency
/// build; stty is in coreutils and is therefore in every image we produce.
struct EchoGuard {
    restore: bool,
}

impl EchoGuard {
    fn disable() -> Self {
        // Inherits stdin, unlike crate::cmd::Cmd, which pipes it -- stty has to
        // see the real terminal to change its settings.
        let ok = Command::new("stty")
            .arg("-echo")
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !ok {
            // Better to warn than to silently echo a password to a console that
            // may be a serial line someone else is watching.
            logging::warn("could not disable terminal echo; password will be visible");
            eprintln!("  (warning: terminal echo could not be disabled)");
        }
        EchoGuard { restore: ok }
    }
}

impl Drop for EchoGuard {
    fn drop(&mut self) {
        if self.restore {
            let _ = Command::new("stty").arg("echo").status();
        }
    }
}
