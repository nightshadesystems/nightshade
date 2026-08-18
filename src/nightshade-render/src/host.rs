//! The boundary between rendering and the machine.
//!
//! Everything that touches the running system goes through [`Host`]. There are
//! two implementations: [`RealHost`], which does it, and [`MockHost`], which
//! writes down what it was asked to do.
//!
//! That split is what makes the commit pipeline testable at speed. A test that
//! has to create bridges to find out whether the pipeline ordered its steps
//! correctly is a test that needs a privileged VM and thirty seconds; a test
//! against `MockHost` needs neither, and checks the same decisions. The real
//! path is exercised separately, on a box where breaking the network is
//! allowed.
//!
//! `MockHost` is not a stub. It keeps a filesystem in memory with the same
//! semantics -- sync really does remove files that are no longer wanted -- so
//! a bug in the sync logic fails a fast test rather than surviving until the
//! slow one.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

#[derive(Debug, thiserror::Error)]
pub enum HostError {
    #[error("{context}: {source}")]
    Io {
        context: String,
        #[source]
        source: std::io::Error,
    },

    #[error("`{command}` failed ({status}){detail}")]
    Command {
        command: String,
        status: String,
        detail: String,
    },
}

impl HostError {
    pub fn io(context: impl Into<String>, source: std::io::Error) -> Self {
        Self::Io {
            context: context.into(),
            source,
        }
    }
}

/// One thing done to the machine, in the order it was done.
///
/// What `MockHost` records and what the real host logs. Comparing these
/// against an expectation is how the pipeline's ordering is tested.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Op {
    /// A managed directory was made to hold exactly these files.
    Sync {
        dir: PathBuf,
        marker: String,
        wrote: Vec<String>,
        removed: Vec<String>,
    },
    Write {
        path: PathBuf,
        contents: String,
    },
    Run {
        argv: Vec<String>,
    },
}

pub trait Host: Send + Sync {
    /// Make `dir` contain exactly `files` among those whose name contains
    /// `marker`, leaving everything else in it alone.
    ///
    /// The marker is the whole of the safety story for
    /// `/run/systemd/network`: that directory is not ours, and a file we did
    /// not write is not ours to remove even when it conflicts.
    fn sync(
        &self,
        dir: &Path,
        marker: &str,
        files: &BTreeMap<String, String>,
    ) -> Result<(), HostError>;

    /// Replace a single file at an absolute path.
    fn write(&self, path: &Path, contents: &str) -> Result<(), HostError>;

    fn read(&self, path: &Path) -> Result<Option<String>, HostError>;

    /// Run a program. Never a shell: the argv is passed through as given.
    fn run(&self, argv: &[String]) -> Result<(), HostError>;

    /// Run a program, feeding it something on stdin that must not be seen.
    ///
    /// The separate method exists for one reason: `/proc/<pid>/cmdline` is
    /// world-readable, so an argument is visible to every process on the box
    /// for as long as the program runs. A password hash handed to
    /// `chpasswd -e` as an argument is a password hash published to anyone
    /// with a shell. On stdin it is a pipe between two processes and nothing
    /// else can see it.
    ///
    /// `secret` is never logged, never recorded in an [`Op`], and never
    /// included in an error message.
    fn run_with_secret(&self, argv: &[String], secret: &str) -> Result<(), HostError>;
}

// ---------------------------------------------------------------------------
// the real one
// ---------------------------------------------------------------------------

pub struct RealHost;

impl Host for RealHost {
    fn sync(
        &self,
        dir: &Path,
        marker: &str,
        files: &BTreeMap<String, String>,
    ) -> Result<(), HostError> {
        std::fs::create_dir_all(dir)
            .map_err(|e| HostError::io(format!("creating {}", dir.display()), e))?;

        for (name, contents) in files {
            let path = dir.join(name);
            write_atomically(&path, contents)?;
        }

        // Remove ours that are no longer wanted. Read after writing so a
        // failure part way through leaves too many files rather than too few:
        // an extra interface definition is visible and harmless next to a
        // missing one.
        let entries = std::fs::read_dir(dir)
            .map_err(|e| HostError::io(format!("reading {}", dir.display()), e))?;
        for entry in entries {
            let entry =
                entry.map_err(|e| HostError::io(format!("reading {}", dir.display()), e))?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.contains(marker) && !files.contains_key(&name) {
                let path = entry.path();
                std::fs::remove_file(&path)
                    .map_err(|e| HostError::io(format!("removing {}", path.display()), e))?;
            }
        }
        Ok(())
    }

    fn write(&self, path: &Path, contents: &str) -> Result<(), HostError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| HostError::io(format!("creating {}", parent.display()), e))?;
        }
        write_atomically(path, contents)
    }

    fn read(&self, path: &Path) -> Result<Option<String>, HostError> {
        match std::fs::read_to_string(path) {
            Ok(text) => Ok(Some(text)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(HostError::io(format!("reading {}", path.display()), e)),
        }
    }

    fn run(&self, argv: &[String]) -> Result<(), HostError> {
        let Some((program, arguments)) = argv.split_first() else {
            return Err(HostError::Command {
                command: String::new(),
                status: "no program".into(),
                detail: String::new(),
            });
        };

        // No shell, ever. Not `sh -c`, not a string that gets split later.
        // Values reaching here have been through the schema, but an interface
        // name is still operator input and this is a process running as root.
        let output = std::process::Command::new(program)
            .args(arguments)
            .output()
            .map_err(|e| HostError::io(format!("running {program}"), e))?;

        if output.status.success() {
            return Ok(());
        }

        let stderr = String::from_utf8_lossy(&output.stderr);
        let reason = stderr.trim();
        Err(HostError::Command {
            command: argv.join(" "),
            status: match output.status.code() {
                Some(code) => format!("exit {code}"),
                None => "killed by signal".into(),
            },
            detail: if reason.is_empty() {
                String::new()
            } else {
                format!(": {}", reason.lines().next().unwrap_or(reason))
            },
        })
    }

    fn run_with_secret(&self, argv: &[String], secret: &str) -> Result<(), HostError> {
        use std::io::Write;
        use std::process::Stdio;

        let Some((program, arguments)) = argv.split_first() else {
            return Err(HostError::Command {
                command: String::new(),
                status: "no program".into(),
                detail: String::new(),
            });
        };

        let mut child = std::process::Command::new(program)
            .args(arguments)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| HostError::io(format!("running {program}"), e))?;

        // Dropped at the end of this block, which closes the pipe and is what
        // tells the child there is no more input coming.
        {
            let mut stdin = child.stdin.take().ok_or_else(|| HostError::Command {
                command: program.clone(),
                status: "no stdin".into(),
                detail: String::new(),
            })?;
            stdin
                .write_all(secret.as_bytes())
                .map_err(|e| HostError::io(format!("writing to {program}"), e))?;
        }

        let output = child
            .wait_with_output()
            .map_err(|e| HostError::io(format!("waiting for {program}"), e))?;
        if output.status.success() {
            return Ok(());
        }

        // The argv is safe to report -- the secret was never in it -- but the
        // child's stderr is not: `chpasswd` echoes the line it could not parse.
        Err(HostError::Command {
            command: argv.join(" "),
            status: match output.status.code() {
                Some(code) => format!("exit {code}"),
                None => "killed by signal".into(),
            },
            detail: String::new(),
        })
    }
}

/// Write via a temporary file and a rename.
///
/// networkd may be reading the directory while we write into it. A partially
/// written `.netdev` is a parse error at exactly the moment the config is
/// being changed, which is the worst possible time to introduce one.
fn write_atomically(path: &Path, contents: &str) -> Result<(), HostError> {
    let temporary = path.with_extension("ns-new");
    std::fs::write(&temporary, contents)
        .map_err(|e| HostError::io(format!("writing {}", temporary.display()), e))?;
    std::fs::rename(&temporary, path)
        .map_err(|e| HostError::io(format!("renaming to {}", path.display()), e))
}

// ---------------------------------------------------------------------------
// the recording one
// ---------------------------------------------------------------------------

#[derive(Default)]
pub struct MockHost {
    inner: Mutex<Recorded>,
}

#[derive(Default)]
struct Recorded {
    ops: Vec<Op>,
    files: BTreeMap<PathBuf, String>,
    /// argv substring that makes `run` fail, for testing what happens when an
    /// apply goes wrong half way through.
    fail_matching: Option<String>,
    /// Whether that failure clears itself after firing once.
    ///
    /// The difference matters: a transient failure exercises the restore, and
    /// a permanent one exercises the restore *failing*, which is a different
    /// and much louder outcome.
    fail_once: bool,
}

impl MockHost {
    pub fn new() -> Self {
        Self::default()
    }

    /// Every command whose argv contains `needle` fails, from now on.
    pub fn fail_commands_matching(&self, needle: impl Into<String>) {
        let mut inner = self.inner.lock().expect("not poisoned");
        inner.fail_matching = Some(needle.into());
        inner.fail_once = false;
    }

    /// The next command whose argv contains `needle` fails, and the one after
    /// it succeeds.
    pub fn fail_once_matching(&self, needle: impl Into<String>) {
        let mut inner = self.inner.lock().expect("not poisoned");
        inner.fail_matching = Some(needle.into());
        inner.fail_once = true;
    }

    pub fn stop_failing(&self) {
        self.inner.lock().expect("not poisoned").fail_matching = None;
    }

    pub fn ops(&self) -> Vec<Op> {
        self.inner.lock().expect("not poisoned").ops.clone()
    }

    pub fn take_ops(&self) -> Vec<Op> {
        std::mem::take(&mut self.inner.lock().expect("not poisoned").ops)
    }

    /// Every command line run, joined, in order.
    pub fn commands(&self) -> Vec<String> {
        self.ops()
            .into_iter()
            .filter_map(|op| match op {
                Op::Run { argv } => Some(argv.join(" ")),
                _ => None,
            })
            .collect()
    }

    pub fn files(&self) -> BTreeMap<PathBuf, String> {
        self.inner.lock().expect("not poisoned").files.clone()
    }

    pub fn file(&self, path: impl AsRef<Path>) -> Option<String> {
        self.inner
            .lock()
            .expect("not poisoned")
            .files
            .get(path.as_ref())
            .cloned()
    }
}

impl Host for MockHost {
    fn sync(
        &self,
        dir: &Path,
        marker: &str,
        files: &BTreeMap<String, String>,
    ) -> Result<(), HostError> {
        let mut inner = self.inner.lock().expect("not poisoned");

        // Same rule as the real host, including only touching our own marker,
        // so a bug in that rule fails here rather than on a firewall.
        let stale: Vec<PathBuf> = inner
            .files
            .keys()
            .filter(|path| {
                path.parent() == Some(dir)
                    && path
                        .file_name()
                        .and_then(|name| name.to_str())
                        .is_some_and(|name| {
                            name.contains(marker) && !files.contains_key(name)
                        })
            })
            .cloned()
            .collect();

        let removed: Vec<String> = stale
            .iter()
            .filter_map(|p| p.file_name()?.to_str().map(str::to_string))
            .collect();
        for path in stale {
            inner.files.remove(&path);
        }

        for (name, contents) in files {
            inner.files.insert(dir.join(name), contents.clone());
        }

        inner.ops.push(Op::Sync {
            dir: dir.to_path_buf(),
            marker: marker.to_string(),
            wrote: files.keys().cloned().collect(),
            removed,
        });
        Ok(())
    }

    fn write(&self, path: &Path, contents: &str) -> Result<(), HostError> {
        let mut inner = self.inner.lock().expect("not poisoned");
        inner.files.insert(path.to_path_buf(), contents.to_string());
        inner.ops.push(Op::Write {
            path: path.to_path_buf(),
            contents: contents.to_string(),
        });
        Ok(())
    }

    fn read(&self, path: &Path) -> Result<Option<String>, HostError> {
        Ok(self
            .inner
            .lock()
            .expect("not poisoned")
            .files
            .get(path)
            .cloned())
    }

    fn run(&self, argv: &[String]) -> Result<(), HostError> {
        let mut inner = self.inner.lock().expect("not poisoned");
        inner.ops.push(Op::Run {
            argv: argv.to_vec(),
        });

        let line = argv.join(" ");
        let matched = inner
            .fail_matching
            .as_deref()
            .is_some_and(|needle| line.contains(needle));
        if matched {
            if inner.fail_once {
                inner.fail_matching = None;
            }
            return Err(HostError::Command {
                command: line,
                status: "exit 1".into(),
                detail: ": refused by the test".into(),
            });
        }
        Ok(())
    }

    /// Records the argv exactly as [`MockHost::run`] does, and deliberately
    /// does not record the secret.
    ///
    /// A test asserting on recorded ops therefore cannot accidentally start
    /// depending on a password hash being in them -- which is the property the
    /// real host is trying to have.
    fn run_with_secret(&self, argv: &[String], _secret: &str) -> Result<(), HostError> {
        self.run(argv)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn files(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(name, body)| (name.to_string(), body.to_string()))
            .collect()
    }

    const MARKER: &str = nightshade_common::MANAGED_MARKER;

    /// The rule the whole of `/run/systemd/network` safety rests on.
    #[test]
    fn sync_removes_only_our_own_stale_files() {
        let host = MockHost::new();
        let dir = Path::new("/run/systemd/network");

        // Two files that are not ours, placed by hand or by a package. The
        // second is the interesting one: it sorts and reads like ours without
        // carrying the marker.
        host.write(&dir.join("50-someone-else.network"), "not ours")
            .unwrap();
        host.write(&dir.join("10-nsswitch.network"), "also not ours")
            .unwrap();

        host.sync(
            dir,
            MARKER,
            &files(&[("10-ns-eth0.network", "a"), ("10-ns-eth1.network", "b")]),
        )
        .unwrap();
        assert_eq!(host.file(dir.join("10-ns-eth0.network")).as_deref(), Some("a"));

        // A later render no longer has eth1 in it.
        host.sync(dir, MARKER, &files(&[("10-ns-eth0.network", "a")]))
            .unwrap();
        assert!(
            host.file(dir.join("10-ns-eth1.network")).is_none(),
            "a stale Nightshade file was kept"
        );
        for foreign in ["50-someone-else.network", "10-nsswitch.network"] {
            assert!(
                host.file(dir.join(foreign)).is_some(),
                "{foreign} was removed, and it is not ours to remove"
            );
        }
    }

    #[test]
    fn sync_reports_what_it_wrote_and_removed() {
        let host = MockHost::new();
        let dir = Path::new("/run/systemd/network");
        host.sync(
            dir,
            MARKER,
            &files(&[("10-ns-eth0.network", "a"), ("10-ns-eth1.network", "b")]),
        )
        .unwrap();
        host.take_ops();

        host.sync(dir, MARKER, &files(&[("10-ns-eth0.network", "changed")]))
            .unwrap();
        assert_eq!(
            host.ops(),
            [Op::Sync {
                dir: dir.to_path_buf(),
                marker: MARKER.into(),
                wrote: vec!["10-ns-eth0.network".into()],
                removed: vec!["10-ns-eth1.network".into()],
            }]
        );
    }

    #[test]
    fn commands_are_recorded_in_order_and_can_be_made_to_fail() {
        let host = MockHost::new();
        host.run(&["networkctl".into(), "reload".into()]).unwrap();

        host.fail_commands_matching("hostnamectl");
        let err = host
            .run(&["hostnamectl".into(), "set-hostname".into(), "fw".into()])
            .unwrap_err();
        assert!(matches!(err, HostError::Command { .. }));

        assert_eq!(
            host.commands(),
            ["networkctl reload", "hostnamectl set-hostname fw"]
        );
    }
}
