//! Typed wrapper around the system tools the installer drives.
//!
//! Nightshade does not reimplement partitioning or ZFS in Rust. It drives
//! `sgdisk`, `zpool`, `zfs`, `mkfs.vfat` and `grub-install`, which are the
//! implementations that are actually tested against these on-disk formats.
//! What this module adds is the part those tools do not give you: every
//! invocation logged, both streams captured, and failures that carry the argv
//! and the output instead of just an exit code.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::error::{CommandFailure, Error, IoContext, Result};
use crate::logging;
use crate::secret::SecretString;

pub struct Output {
    pub status: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

impl Output {
    pub fn ok(&self) -> bool {
        self.status == Some(0)
    }

    pub fn trimmed(&self) -> &str {
        self.stdout.trim()
    }

    pub fn lines(&self) -> impl Iterator<Item = &str> {
        self.stdout.lines().map(str::trim).filter(|l| !l.is_empty())
    }
}

enum StdinSource {
    Empty,
    /// Redacted in the log; the character count is recorded, the bytes never are.
    Secret(SecretString),
}

pub struct Cmd {
    program: String,
    args: Vec<String>,
    chroot: Option<PathBuf>,
    stdin: StdinSource,
}

impl Cmd {
    pub fn new(program: impl Into<String>) -> Self {
        Cmd {
            program: program.into(),
            args: Vec::new(),
            chroot: None,
            stdin: StdinSource::Empty,
        }
    }

    pub fn arg(mut self, a: impl Into<String>) -> Self {
        self.args.push(a.into());
        self
    }

    pub fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.args.extend(args.into_iter().map(Into::into));
        self
    }

    /// Run inside the target root. Used for everything that must see the
    /// installed system's own libraries and configuration.
    pub fn in_chroot(mut self, root: impl Into<PathBuf>) -> Self {
        self.chroot = Some(root.into());
        self
    }

    /// Feed a secret on stdin. This is the only channel by which the account
    /// password reaches the system: argv is readable by any process via /proc,
    /// and a temporary file outlives the write.
    pub fn stdin_secret(mut self, secret: SecretString) -> Self {
        self.stdin = StdinSource::Secret(secret);
        self
    }

    /// Log-safe rendering of what is about to run.
    pub fn display(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        if let Some(root) = &self.chroot {
            parts.push("chroot".into());
            parts.push(root.display().to_string());
        }
        parts.push(self.program.clone());
        parts.extend(self.args.iter().map(|a| quote(a)));
        let mut s = parts.join(" ");
        if let StdinSource::Secret(sec) = &self.stdin {
            s.push_str(&format!(" <<< <{} chars redacted>", sec.len()));
        }
        s
    }

    /// Run and require success.
    pub fn run(self) -> Result<Output> {
        let display = self.display();
        let out = self.spawn_and_wait()?;
        if out.ok() {
            Ok(out)
        } else {
            let failure = CommandFailure {
                display,
                status: out.status,
                stdout: out.stdout,
                stderr: out.stderr,
            };
            logging::error(format!("FAILED: {}", failure.display));
            Err(Error::Command(Box::new(failure)))
        }
    }

    /// Run and hand back the result regardless of exit status. For probes where
    /// a non-zero exit is information rather than a fault (`zpool import` on a
    /// disk with no pool, `modinfo` on a missing module).
    pub fn run_lenient(self) -> Result<Output> {
        self.spawn_and_wait()
    }

    /// True when the command exists and exits zero. Swallows spawn errors.
    pub fn succeeds(self) -> bool {
        matches!(self.spawn_and_wait(), Ok(o) if o.ok())
    }

    fn spawn_and_wait(self) -> Result<Output> {
        let display = self.display();
        logging::info(format!("$ {display}"));

        let (program, args) = match &self.chroot {
            Some(root) => {
                let mut a = vec![root.display().to_string(), self.program.clone()];
                a.extend(self.args.iter().cloned());
                ("chroot".to_string(), a)
            }
            None => (self.program.clone(), self.args.clone()),
        };

        let mut command = Command::new(&program);
        command
            .args(&args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        // Stable, parseable output regardless of the operator's locale, and no
        // maintainer script may stop to ask a question on a console we are
        // already drawing on.
        command.env("LC_ALL", "C");
        command.env("LANG", "C");
        command.env("DEBIAN_FRONTEND", "noninteractive");

        let mut child = command
            .spawn()
            .ctx(format!("failed to run `{program}` (is it installed?)"))?;

        {
            let mut pipe = child.stdin.take().expect("stdin was piped");
            let write_result = match &self.stdin {
                StdinSource::Empty => Ok(()),
                StdinSource::Secret(s) => pipe.write_all(s.expose().as_bytes()),
            };
            // Dropping the pipe closes it, which is what lets the child see EOF
            // and exit. Do that before propagating a write error, or a failed
            // write would deadlock in wait_with_output below.
            drop(pipe);
            write_result.ctx(format!("failed writing stdin to `{program}`"))?;
        }

        let out = child
            .wait_with_output()
            .ctx(format!("failed waiting for `{program}`"))?;

        let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
        let status = out.status.code();

        match status {
            Some(0) => logging::info("  -> ok"),
            Some(c) => logging::warn(format!("  -> exit {c}")),
            None => logging::warn("  -> killed by signal"),
        }
        logging::block("  stdout", &stdout);
        logging::block("  stderr", &stderr);

        Ok(Output {
            status,
            stdout,
            stderr,
        })
    }
}

/// Quote an argument for display only. Never used to build a command line --
/// arguments are passed to execve as a vector, so nothing here can inject.
fn quote(s: &str) -> String {
    if !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_=/.,:+@".contains(c))
    {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', r"'\''"))
    }
}

/// Is this tool on PATH? Used by preflight so a missing dependency is reported
/// before any disk is touched rather than halfway through partitioning.
pub fn exists(program: &str) -> bool {
    let Ok(path) = std::env::var("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|dir| {
        let candidate = dir.join(program);
        candidate.is_file() && is_executable(&candidate)
    })
}

fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|m| m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_redacts_secrets() {
        let cmd = Cmd::new("chpasswd")
            .in_chroot("/mnt")
            .stdin_secret(SecretString::new("nightshade:hunter2hunter2".into()));
        let shown = cmd.display();
        assert!(shown.contains("redacted"), "{shown}");
        assert!(!shown.contains("hunter2"), "{shown}");
    }

    #[test]
    fn display_quotes_awkward_arguments() {
        let cmd = Cmd::new("sgdisk").arg("-c1:EFI System").arg("/dev/sda");
        assert_eq!(cmd.display(), "sgdisk '-c1:EFI System' /dev/sda");
    }

    #[test]
    fn display_shows_chroot_prefix() {
        let cmd = Cmd::new("update-grub").in_chroot("/mnt");
        assert_eq!(cmd.display(), "chroot /mnt update-grub");
    }

    #[test]
    fn exists_finds_a_real_tool() {
        assert!(exists("sh"));
        assert!(!exists("nightshade-definitely-not-a-real-tool"));
    }
}
