//! Installer error type.
//!
//! Errors carry enough to put a real diagnosis on a bare TTY. When `sgdisk` or
//! `zpool` fails at 2am on a machine with no network, "command failed" is
//! useless; the operator needs the exact argv and both output streams.

use std::fmt;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug)]
pub enum Error {
    /// The operator declined at a confirmation prompt, or pressed the abort
    /// key. Not a failure: callers unwind quietly and exit 0.
    Aborted,

    /// A system tool exited non-zero.
    Command(Box<CommandFailure>),

    /// The installer cannot run here at all -- not root, no live medium, no
    /// ZFS module. Distinct from a command failure because it is detected
    /// before anything destructive happens.
    Environment(String),

    Io {
        context: String,
        source: std::io::Error,
    },

    /// Operator input that failed a rule. Frontends re-prompt on these rather
    /// than aborting.
    Validation(String),
}

#[derive(Debug)]
pub struct CommandFailure {
    /// Command line with any secret stdin redacted; safe to print and log.
    pub display: String,
    pub status: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

impl CommandFailure {
    /// Multi-line rendering for the failure screen and the log.
    pub fn detail(&self) -> String {
        let mut s = String::new();
        s.push_str(&format!("command: {}\n", self.display));
        match self.status {
            Some(c) => s.push_str(&format!("exit:    {c}\n")),
            None => s.push_str("exit:    killed by signal\n"),
        }
        for (name, body) in [("stdout", &self.stdout), ("stderr", &self.stderr)] {
            let body = body.trim_end();
            if !body.is_empty() {
                s.push_str(&format!("{name}:\n"));
                for line in body.lines() {
                    s.push_str("    ");
                    s.push_str(line);
                    s.push('\n');
                }
            }
        }
        s
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Aborted => write!(f, "cancelled"),
            Error::Command(c) => {
                let status = match c.status {
                    Some(code) => format!("exit {code}"),
                    None => "killed by signal".to_string(),
                };
                // Prefer stderr for the one-line form; most tools put the
                // actual reason there and stdout is often just progress noise.
                let reason = if !c.stderr.trim().is_empty() {
                    c.stderr.trim()
                } else {
                    c.stdout.trim()
                };
                if reason.is_empty() {
                    write!(f, "`{}` failed ({status})", c.display)
                } else {
                    let first = reason.lines().next().unwrap_or(reason);
                    write!(f, "`{}` failed ({status}): {first}", c.display)
                }
            }
            Error::Environment(m) => write!(f, "{m}"),
            Error::Io { context, source } => write!(f, "{context}: {source}"),
            Error::Validation(m) => write!(f, "{m}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

impl Error {
    pub fn env(msg: impl Into<String>) -> Self {
        Error::Environment(msg.into())
    }

    pub fn invalid(msg: impl Into<String>) -> Self {
        Error::Validation(msg.into())
    }

    /// Full multi-line detail where there is one, otherwise the Display form.
    pub fn detail(&self) -> String {
        match self {
            Error::Command(c) => c.detail(),
            other => other.to_string(),
        }
    }
}

/// Attach a human description to an io::Error at the call site.
pub trait IoContext<T> {
    fn ctx(self, context: impl Into<String>) -> Result<T>;
}

impl<T> IoContext<T> for std::result::Result<T, std::io::Error> {
    fn ctx(self, context: impl Into<String>) -> Result<T> {
        self.map_err(|source| Error::Io {
            context: context.into(),
            source,
        })
    }
}
