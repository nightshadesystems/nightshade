//! What a renderer produces, and the trait it produces it through.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::host::{Host, HostError};

/// A directory whose marked files this renderer owns.
///
/// Files under `dir` whose name contains `marker` and that are absent from the
/// artifacts are removed on apply. Anything else in the directory is left
/// alone, forever.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Managed {
    pub dir: PathBuf,
    pub marker: String,
    /// File name to contents, relative to `dir`.
    pub files: BTreeMap<String, String>,
}

/// Something applied by running a tool rather than by writing a file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Action {
    SetHostName(String),
    SetTimeZone(String),
    /// Ask networkd to re-read what was just written.
    ReloadNetworkd,
    /// Destroy a device so networkd rebuilds it.
    ///
    /// Never produced by `render`: which devices need this depends on what is
    /// already running, and `render` is a function of the config alone. `apply`
    /// works it out by comparing against [`Renderer::previous`].
    RecreateNetdev(String),

    /// An account that should exist, with this shell, groups and GECOS.
    ///
    /// The password is deliberately not here. It is a separate action so that
    /// the argv this turns into -- which is what gets logged, recorded and
    /// compared in the golden tests -- can never contain a hash.
    EnsureAccount {
        name: String,
        full_name: Option<String>,
    },

    /// Set an account's password from its crypt hash.
    ///
    /// The hash rides in the action rather than the argv: [`Action::argv`]
    /// returns the `chpasswd -e` command with nothing sensitive in it, and the
    /// apply path feeds the hash to it on stdin via
    /// [`Host::run_with_secret`](crate::host::Host::run_with_secret).
    ///
    /// This is serialised into the last-applied state, which is why that
    /// directory is 0700 root -- see `dist/systemd/nightshade.conf`.
    SetPassword { name: String, hash: String },

    /// Remove an account this system used to manage and no longer does.
    ///
    /// Never produced by `render`, for the same reason as `RecreateNetdev`:
    /// which accounts are surplus depends on what the last apply created, not
    /// on the config alone.
    RemoveAccount(String),
}

impl Action {
    /// The argv this runs as, or `None` if it is not a command.
    ///
    /// A list, never a string. Nothing here is ever handed to a shell.
    pub fn argv(&self) -> Option<Vec<String>> {
        let argv = match self {
            Action::SetHostName(name) => {
                vec!["hostnamectl".into(), "set-hostname".into(), name.clone()]
            }
            Action::SetTimeZone(zone) => {
                vec!["timedatectl".into(), "set-timezone".into(), zone.clone()]
            }
            Action::ReloadNetworkd => vec!["networkctl".into(), "reload".into()],
            Action::RecreateNetdev(name) => {
                vec!["networkctl".into(), "delete".into(), name.clone()]
            }
            // usermod rather than useradd: whether the account already exists
            // is not knowable at render time, so the apply path picks. What is
            // pinned here is the shape both share.
            Action::EnsureAccount { name, full_name } => {
                let mut argv = vec!["usermod".to_string()];
                if let Some(full_name) = full_name {
                    argv.push("--comment".into());
                    argv.push(full_name.clone());
                }
                argv.push(name.clone());
                argv
            }
            // `-e` means "what follows is already hashed". Without it chpasswd
            // would hash the hash.
            Action::SetPassword { .. } => vec!["chpasswd".into(), "-e".into()],
            // No `--remove`: the home directory is not ours to delete. An
            // operator who removed an account by mistake can put it back;
            // one whose files went with it cannot.
            Action::RemoveAccount(name) => vec!["userdel".into(), name.clone()],
        };
        Some(argv)
    }
}

/// The complete output of one renderer for one config.
///
/// A function of the config and nothing else: no timestamps, no ordering that
/// depends on how the tree was built, no reading of the current system. That
/// is what makes the golden tests byte-exact and what makes it safe to render
/// twice and compare.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Artifacts {
    pub managed: Vec<Managed>,
    /// Files at absolute paths that this renderer owns outright.
    pub files: BTreeMap<PathBuf, String>,
    pub actions: Vec<Action>,
}

impl Artifacts {
    pub fn is_empty(&self) -> bool {
        self.actions.is_empty()
            && self.files.is_empty()
            && self.managed.iter().all(|m| m.files.is_empty())
    }

    /// Every managed file, keyed by its full path. For tests and diagnostics.
    pub fn all_files(&self) -> BTreeMap<PathBuf, String> {
        let mut out = self.files.clone();
        for managed in &self.managed {
            for (name, contents) in &managed.files {
                out.insert(managed.dir.join(name), contents.clone());
            }
        }
        out
    }
}

#[derive(Debug, thiserror::Error)]
pub enum RenderError {
    #[error("{subsystem}: {message}")]
    Inconsistent {
        subsystem: &'static str,
        message: String,
    },
}

#[derive(Debug, thiserror::Error)]
pub enum ApplyError {
    #[error("{0}")]
    Host(#[from] HostError),

    #[error("{path} was applied but {reason}")]
    Unverified { path: String, reason: String },
}

/// A subsystem the config maps onto.
pub trait Renderer: Send + Sync {
    /// Short name, used in logs and to key the last-applied state.
    fn name(&self) -> &'static str;

    /// The config subtree this renderer owns.
    ///
    /// Its schema priority is what orders renderers against each other, so
    /// there is one place that decides what is applied before what -- the
    /// schema -- rather than a second ordering here that can disagree with it.
    fn owns(&self) -> nightshade_schema::path::Path;

    /// Config in, complete target state out. Pure.
    fn render(&self, config: &nightshade_schema::config::ConfigTree)
    -> Result<Artifacts, RenderError>;

    /// Everything that can be checked without touching the machine.
    fn check(&self, artifacts: &Artifacts) -> Result<(), RenderError>;

    /// Make it so.
    fn apply(&self, artifacts: &Artifacts) -> Result<(), ApplyError>;

    /// Read back what `apply` wrote and confirm it is there.
    ///
    /// Deliberately modest about what it proves. It cannot ask networkd
    /// whether the kernel accepted a bond mode -- there is no interface for
    /// that -- so it checks the thing it can check: that every file the render
    /// produced is on disk with the contents it was given. That catches a sync
    /// that silently did nothing, a directory that turned out to be read-only,
    /// and a file another process overwrote between writing and reloading.
    ///
    /// Confirming the kernel's opinion is the real-apply path's job, on a box
    /// where there is a kernel to ask.
    fn verify(&self, artifacts: &Artifacts) -> Result<(), ApplyError>;

    /// What the last successful apply used, if there was one.
    fn previous(&self) -> Option<Artifacts>;

    /// Record `artifacts` as the last successful apply.
    fn remember(&self, artifacts: &Artifacts) -> Result<(), ApplyError>;
}

/// Read every file in `artifacts` back and confirm it matches.
///
/// Shared by both renderers, so "applied" means the same thing for a
/// `.netdev` as it does for `resolv.conf`.
pub fn verify_files(host: &dyn Host, artifacts: &Artifacts) -> Result<(), ApplyError> {
    for (path, expected) in artifacts.all_files() {
        match host.read(&path)? {
            Some(found) if found == expected => {}
            Some(_) => {
                return Err(ApplyError::Unverified {
                    path: path.display().to_string(),
                    reason: "the contents are not what was written".into(),
                });
            }
            None => {
                return Err(ApplyError::Unverified {
                    path: path.display().to_string(),
                    reason: "the file is not there".into(),
                });
            }
        }
    }
    Ok(())
}

/// Where a renderer keeps its last-applied artifacts.
///
/// One JSON file per renderer under `last-applied/`. JSON rather than a
/// directory of the files themselves because the artifacts are more than
/// files -- an apply that has to be undone needs the actions back too, and
/// half the state in one shape and half in another is how a restore comes to
/// restore half of it.
pub struct LastApplied {
    path: PathBuf,
    host: std::sync::Arc<dyn Host>,
}

impl LastApplied {
    pub fn new(dir: &std::path::Path, name: &str, host: std::sync::Arc<dyn Host>) -> Self {
        Self {
            path: dir.join(format!("{name}.json")),
            host,
        }
    }

    pub fn load(&self) -> Option<Artifacts> {
        let text = self.host.read(&self.path).ok()??;
        serde_json::from_str(&text).ok()
    }

    pub fn save(&self, artifacts: &Artifacts) -> Result<(), ApplyError> {
        let text = serde_json::to_string_pretty(artifacts).expect("artifacts always serialise");
        self.host.write(&self.path, &text)?;
        Ok(())
    }
}
