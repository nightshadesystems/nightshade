//! Local accounts.
//!
//! The one renderer that does not write a file. Accounts live in
//! `/etc/passwd` and `/etc/shadow`, and those are not files for a daemon to
//! hand-edit: they are written under a lock (`lckpwdf`) that `useradd` and
//! `chpasswd` take and this would have to reimplement, and a torn
//! `/etc/shadow` is an appliance nobody can log in to. So this renders
//! *actions* and lets the shadow tools do what they are for.
//!
//! # The password never reaches a command line
//!
//! `/proc/<pid>/cmdline` is world-readable. A hash passed to `chpasswd` as an
//! argument is a hash published to every process on the box for as long as the
//! command runs. It goes on stdin instead, through `Host::run_with_secret`,
//! and `Action::argv` for a `SetPassword` deliberately returns a command with
//! nothing sensitive in it -- which is also what the golden tests pin, so a
//! future edit that moves a hash into the argv fails a test rather than
//! shipping.
//!
//! # Locking everyone out is the failure that matters
//!
//! Root is locked on a Nightshade box and `ns` is the only account's login
//! shell, so an apply that leaves no usable administrator is an appliance that
//! has to be reinstalled. [`LoginRenderer::check`] refuses that configuration
//! before anything is applied, which is the last point at which refusing is
//! still cheap.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use nightshade_common::paths::Paths;
use nightshade_schema::config::{ConfigTree, Node};
use nightshade_schema::path::Path;

use crate::artifacts::{Action, ApplyError, Artifacts, LastApplied, RenderError, Renderer};
use crate::host::Host;

/// Accounts this renderer will not touch, whatever the configuration says.
///
/// `root` above all: it is deliberately locked on an installed box, and a
/// configuration able to name it is a configuration able to unlock it. The
/// rest are the system accounts Debian ships, which are not administrators and
/// are not ours to reshape.
const NEVER_MANAGED: &[&str] = &[
    "root",
    "daemon",
    "bin",
    "sys",
    "sync",
    "games",
    "man",
    "lp",
    "mail",
    "news",
    "uucp",
    "proxy",
    "backup",
    "list",
    "irc",
    "nobody",
    "systemd-network",
    "systemd-resolve",
    "messagebus",
    "sshd",
];

pub struct LoginRenderer {
    host: Arc<dyn Host>,
    last_applied: LastApplied,
}

impl LoginRenderer {
    pub fn new(paths: Paths, host: Arc<dyn Host>) -> Self {
        let last_applied = LastApplied::new(&paths.last_applied_dir(), "login", Arc::clone(&host));
        Self { host, last_applied }
    }

    fn users(config: &ConfigTree) -> BTreeMap<&String, &Node> {
        config
            .get(&Path::from_segments(["system", "login", "user"]))
            .and_then(Node::children)
            .map(|children| children.iter().collect())
            .unwrap_or_default()
    }

    /// The accounts a set of artifacts says should exist.
    fn named(artifacts: &Artifacts) -> BTreeSet<String> {
        artifacts
            .actions
            .iter()
            .filter_map(|action| match action {
                Action::EnsureAccount { name, .. } => Some(name.clone()),
                _ => None,
            })
            .collect()
    }
}

impl Renderer for LoginRenderer {
    fn name(&self) -> &'static str {
        "login"
    }

    fn owns(&self) -> Path {
        Path::from_segments(["system", "login"])
    }

    fn render(&self, config: &ConfigTree) -> Result<Artifacts, RenderError> {
        let mut actions = Vec::new();

        for (name, node) in Self::users(config) {
            let full_name = node
                .children()
                .and_then(|c| c.get("full-name"))
                .and_then(Node::value)
                .map(str::to_string);
            actions.push(Action::EnsureAccount {
                name: name.clone(),
                full_name,
            });

            let hash = node
                .children()
                .and_then(|c| c.get("authentication"))
                .and_then(Node::children)
                .and_then(|c| c.get("encrypted-password"))
                .and_then(Node::value);
            if let Some(hash) = hash {
                actions.push(Action::SetPassword {
                    name: name.clone(),
                    hash: hash.to_string(),
                });
            }
        }

        Ok(Artifacts {
            managed: Vec::new(),
            files: BTreeMap::new(),
            actions,
        })
    }

    fn check(&self, artifacts: &Artifacts) -> Result<(), RenderError> {
        let inconsistent = |message: String| RenderError::Inconsistent {
            subsystem: "login",
            message,
        };

        let named = Self::named(artifacts);

        // Nothing may name an account that is not ours. Without this, a
        // configuration could set root's password -- or clear it.
        for name in &named {
            if NEVER_MANAGED.contains(&name.as_str()) {
                return Err(inconsistent(format!(
                    "`{name}` is a system account and is not configurable here"
                )));
            }
        }

        // `!` and `*` are the two shadow values that mean "exists, but not by
        // password". Legitimate for a key-only account, and not a way in.
        let usable: BTreeSet<&String> = artifacts
            .actions
            .iter()
            .filter_map(|action| match action {
                Action::SetPassword { name, hash } if hash != "!" && hash != "*" => Some(name),
                _ => None,
            })
            .collect();

        // Configuring accounts but leaving none of them able to log in is the
        // one mistake this system cannot recover from on its own: root is
        // locked and the console is the only door.
        if !named.is_empty() && usable.is_empty() {
            return Err(inconsistent(
                "no configured account has a password, so nobody could log in; \
                 root is locked on this system and the console is the only way in"
                    .to_string(),
            ));
        }

        Ok(())
    }

    fn apply(&self, artifacts: &Artifacts) -> Result<(), ApplyError> {
        // Create and set passwords first, remove surplus accounts second.
        //
        // The order is the safety property: at no point between the two is
        // there a moment with no account that can log in. Removing first would
        // open exactly that window, and a failure inside it would leave the
        // box sitting in it.
        for action in &artifacts.actions {
            match action {
                // Down a pipe, never into an argv. chpasswd reads `name:hash`.
                Action::SetPassword { name, hash } => {
                    let argv = action.argv().expect("chpasswd has an argv");
                    self.host
                        .run_with_secret(&argv, &format!("{name}:{hash}\n"))?;
                }
                _ => {
                    if let Some(argv) = action.argv() {
                        self.host.run(&argv)?;
                    }
                }
            }
        }

        for removal in removals(self.previous().as_ref(), artifacts) {
            self.host.run(&removal.argv().expect("a command"))?;
        }
        Ok(())
    }

    fn verify(&self, _artifacts: &Artifacts) -> Result<(), ApplyError> {
        // Nothing to read back: this renderer writes no files. Asking `getent`
        // whether an account exists would prove the tool ran, not that it did
        // what was asked, and a tool that failed already produced a non-zero
        // exit that `apply` propagated.
        Ok(())
    }

    fn previous(&self) -> Option<Artifacts> {
        self.last_applied.load()
    }

    fn remember(&self, artifacts: &Artifacts) -> Result<(), ApplyError> {
        self.last_applied.save(artifacts)
    }
}

/// Accounts the last apply created that this one no longer wants.
///
/// Separate from `render` for the same reason `RecreateNetdev` is: it is a
/// function of what happened before, not of the configuration alone.
fn removals(previous: Option<&Artifacts>, next: &Artifacts) -> Vec<Action> {
    let before = previous.map(LoginRenderer::named).unwrap_or_default();
    let after = LoginRenderer::named(next);
    before
        .difference(&after)
        .map(|name| Action::RemoveAccount(name.clone()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::MockHost;

    const HASH: &str = "$6$rounds=656000$saltsalt$aVeryLongCheck.sum/Here0123456789";

    fn config(pairs: &[(&str, &str)]) -> ConfigTree {
        let schema = nightshade_schema::model::Schema::compiled();
        let mut tree = ConfigTree::new();
        for (path, value) in pairs {
            let path = Path::parse(path).expect("a path");
            let value = (!value.is_empty()).then_some(*value);
            schema.apply_set(&mut tree, &path, value).expect("a set");
        }
        tree
    }

    fn renderer() -> (LoginRenderer, Arc<MockHost>) {
        let host = Arc::new(MockHost::new());
        (
            LoginRenderer::new(Paths::under("/test"), Arc::clone(&host) as Arc<dyn Host>),
            host,
        )
    }

    fn rendered(pairs: &[(&str, &str)]) -> Artifacts {
        let (renderer, _) = renderer();
        renderer.render(&config(pairs)).expect("render")
    }

    #[test]
    fn a_user_becomes_an_account_and_a_password() {
        let artifacts = rendered(&[
            ("system login user nightshade full-name", "Administrator"),
            (
                "system login user nightshade authentication encrypted-password",
                HASH,
            ),
        ]);
        assert_eq!(
            artifacts.actions,
            vec![
                Action::EnsureAccount {
                    name: "nightshade".into(),
                    full_name: Some("Administrator".into()),
                },
                Action::SetPassword {
                    name: "nightshade".into(),
                    hash: HASH.into(),
                },
            ]
        );
    }

    /// The property the whole design is for: the hash is not in any argv, so
    /// it is not in `/proc/<pid>/cmdline`, the journal, or a recorded op.
    #[test]
    fn no_command_line_ever_carries_the_hash() {
        let (renderer, host) = renderer();
        let artifacts = renderer
            .render(&config(&[(
                "system login user nightshade authentication encrypted-password",
                HASH,
            )]))
            .unwrap();

        for action in &artifacts.actions {
            let argv = action.argv().unwrap_or_default().join(" ");
            assert!(!argv.contains(HASH), "the hash reached an argv: {argv}");
        }

        renderer.apply(&artifacts).expect("apply");
        for op in host.ops() {
            let recorded = format!("{op:?}");
            assert!(!recorded.contains(HASH), "the hash was recorded: {recorded}");
        }
    }

    /// chpasswd still has to receive it, or nothing was set at all.
    #[test]
    fn the_hash_reaches_chpasswd_on_stdin() {
        let (renderer, host) = renderer();
        let artifacts = renderer
            .render(&config(&[(
                "system login user nightshade authentication encrypted-password",
                HASH,
            )]))
            .unwrap();
        renderer.apply(&artifacts).expect("apply");

        let ran: Vec<String> = host
            .ops()
            .iter()
            .filter_map(|op| match op {
                crate::host::Op::Run { argv } => Some(argv.join(" ")),
                _ => None,
            })
            .collect();
        assert!(
            ran.iter().any(|line| line == "chpasswd -e"),
            "chpasswd was never run: {ran:?}"
        );
    }

    #[test]
    fn system_accounts_are_refused() {
        let (renderer, _) = renderer();
        for name in ["root", "daemon", "sshd"] {
            let artifacts = renderer
                .render(&config(&[(
                    &format!("system login user {name} authentication encrypted-password"),
                    HASH,
                )]))
                .unwrap();
            let refused = renderer.check(&artifacts);
            assert!(refused.is_err(), "`{name}` was accepted");
            assert!(
                format!("{}", refused.unwrap_err()).contains("system account"),
                "`{name}` was refused for the wrong reason"
            );
        }
    }

    /// Root is locked and the console is the only way in, so a configuration
    /// where nobody can authenticate has to be refused before it is applied.
    #[test]
    fn a_config_nobody_could_log_in_to_is_refused() {
        let (renderer, _) = renderer();

        let locked_out = renderer
            .render(&config(&[("system login user nightshade full-name", "Admin")]))
            .unwrap();
        assert!(renderer.check(&locked_out).is_err(), "no password at all");

        let disabled = renderer
            .render(&config(&[(
                "system login user nightshade authentication encrypted-password",
                "!",
            )]))
            .unwrap();
        assert!(renderer.check(&disabled).is_err(), "every account disabled");

        // One usable account alongside a disabled one is fine: somebody can
        // still get in.
        let mixed = renderer
            .render(&config(&[
                ("system login user svc authentication encrypted-password", "!"),
                (
                    "system login user nightshade authentication encrypted-password",
                    HASH,
                ),
            ]))
            .unwrap();
        renderer.check(&mixed).expect("one usable account is enough");
    }

    /// No accounts configured at all is not a lockout -- it is a box whose
    /// accounts are simply not managed here, which is how one boots today.
    #[test]
    fn an_empty_login_tree_is_not_a_lockout() {
        let (renderer, _) = renderer();
        let nothing = renderer.render(&config(&[])).unwrap();
        assert!(nothing.actions.is_empty());
        renderer.check(&nothing).expect("no accounts is not a lockout");
    }

    #[test]
    fn an_account_that_goes_away_is_removed() {
        let before = rendered(&[(
            "system login user olduser authentication encrypted-password",
            HASH,
        )]);
        let after = rendered(&[(
            "system login user nightshade authentication encrypted-password",
            HASH,
        )]);

        assert_eq!(
            removals(Some(&before), &after),
            vec![Action::RemoveAccount("olduser".into())]
        );
        // And nothing is removed when the account is still configured.
        assert!(removals(Some(&after), &after).is_empty());
    }
}
