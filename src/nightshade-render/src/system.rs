//! Host name, resolvers, time zone.
//!
//! Small, and that is the point of it. It exists to prove the renderer trait
//! describes a *renderer* rather than describing networkd: two subsystems with
//! nothing in common -- one writes unit files into a directory, one runs
//! `hostnamectl` -- go through the same `render`/`check`/`apply`/`previous`
//! and the commit pipeline treats them identically.
//!
//! # /etc/resolv.conf is ours
//!
//! This phase runs no `systemd-resolved`, so there is no stub file and no
//! symlink to respect. The file is written outright, with a header saying so.
//! Owning it is the honest arrangement: the alternative is merging into
//! something nobody else is maintaining, which reads as cooperation and
//! behaves as a race.
//!
//! # /etc/hosts is not
//!
//! Only the one line mapping the configured host name is maintained. The rest
//! of the file belongs to whoever put it there.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use nightshade_common::paths::Paths;
use nightshade_schema::config::ConfigTree;
use nightshade_schema::path::Path;

use crate::artifacts::{Action, ApplyError, Artifacts, LastApplied, RenderError, Renderer};
use crate::host::Host;

const RESOLV_HEADER: &str = "\
# Managed by Nightshade. Do not edit.
#
# Generated from `set system name-server` in /etc/nightshade/config.boot.
# Changes made here are lost on the next commit.
";

/// The line in `/etc/hosts` this renderer maintains, and nothing else in that
/// file.
const HOSTS_MARKER: &str = "# nightshade hostname";

pub struct SystemRenderer {
    paths: Paths,
    host: Arc<dyn Host>,
    last_applied: LastApplied,
}

impl SystemRenderer {
    pub fn new(paths: Paths, host: Arc<dyn Host>) -> Self {
        let last_applied = LastApplied::new(&paths.last_applied_dir(), "system", Arc::clone(&host));
        Self {
            paths,
            host,
            last_applied,
        }
    }

    fn value<'a>(config: &'a ConfigTree, path: &str) -> Option<&'a str> {
        config.get(&Path::parse(path).ok()?)?.value()
    }
}

impl Renderer for SystemRenderer {
    fn name(&self) -> &'static str {
        "system"
    }

    fn owns(&self) -> Path {
        Path::from_segments(["system"])
    }

    fn render(&self, config: &ConfigTree) -> Result<Artifacts, RenderError> {
        let mut files: BTreeMap<PathBuf, String> = BTreeMap::new();
        let mut actions = Vec::new();

        let host_name = Self::value(config, "system host-name");

        // Resolvers. Written even when there are none, because an empty
        // managed file and no file at all mean different things: the first is
        // "no resolvers are configured", the second is "something else owns
        // this".
        let servers: Vec<String> = config
            .values_at(&Path::from_segments(["system", "name-server"]))
            .map(|values| values.iter().cloned().collect())
            .unwrap_or_default();
        let domain = Self::value(config, "system domain-name");
        let mut resolv = String::from(RESOLV_HEADER);
        // `search` before the resolvers, which is the order resolv.conf(5)
        // uses throughout. One `search`, never `domain`: the two directives
        // are mutually exclusive and the last one read wins, so writing both
        // would leave which applies depending on line order.
        if let Some(domain) = domain {
            resolv.push_str("search ");
            resolv.push_str(domain);
            resolv.push('\n');
        }
        for server in &servers {
            resolv.push_str("nameserver ");
            resolv.push_str(server);
            resolv.push('\n');
        }
        files.insert(self.paths.resolv_conf(), resolv);

        if let Some(name) = host_name {
            actions.push(Action::SetHostName {
                name: name.to_string(),
                domain: domain.map(str::to_string),
            });
        }
        if let Some(zone) = Self::value(config, "system time-zone") {
            actions.push(Action::SetTimeZone(zone.to_string()));
        }

        Ok(Artifacts {
            managed: Vec::new(),
            files,
            actions,
        })
    }

    fn check(&self, artifacts: &Artifacts) -> Result<(), RenderError> {
        // The values were schema-checked before rendering. What is left to
        // assert is that the set makes sense together -- here, that nothing
        // was asked for twice, which would mean the renderer emitted two
        // answers and the last one silently won.
        let mut seen = std::collections::BTreeSet::new();
        for action in &artifacts.actions {
            let key = match action {
                Action::SetHostName { .. } => "host-name",
                Action::SetTimeZone(_) => "time-zone",
                _ => continue,
            };
            if !seen.insert(key) {
                return Err(RenderError::Inconsistent {
                    subsystem: "system",
                    message: format!("{key} was rendered more than once"),
                });
            }
        }
        Ok(())
    }

    fn apply(&self, artifacts: &Artifacts) -> Result<(), ApplyError> {
        for (path, contents) in &artifacts.files {
            self.host.write(path, contents)?;
        }

        for action in &artifacts.actions {
            if let Action::SetHostName { name, domain } = action {
                self.update_hosts(name, domain.as_deref())?;
            }
            if let Some(argv) = action.argv() {
                self.host.run(&argv)?;
            }
        }
        Ok(())
    }

    fn verify(&self, artifacts: &Artifacts) -> Result<(), ApplyError> {
        crate::artifacts::verify_files(self.host.as_ref(), artifacts)
    }

    fn previous(&self) -> Option<Artifacts> {
        self.last_applied.load()
    }

    fn remember(&self, artifacts: &Artifacts) -> Result<(), ApplyError> {
        self.last_applied.save(artifacts)
    }
}

impl SystemRenderer {
    /// Maintain exactly one marked line in `/etc/hosts`.
    ///
    /// Without it, `sudo` and anything else that resolves the local host name
    /// waits for a DNS timeout on a box whose whole job may be to not have
    /// working DNS yet.
    fn update_hosts(&self, name: &str, domain: Option<&str>) -> Result<(), ApplyError> {
        let path = self.paths.hosts();
        let existing = self.host.read(&path)?.unwrap_or_default();

        let mut lines: Vec<String> = existing
            .lines()
            .filter(|line| !line.trim_end().ends_with(HOSTS_MARKER))
            .map(str::to_string)
            .collect();
        // Fully qualified name first, short name second. That order is what
        // makes `hostname -f` answer correctly: the resolver takes the first
        // name on a matching line as canonical and the rest as aliases, so
        // reversing them gives a box that cannot state its own FQDN.
        let names = match domain {
            Some(domain) => format!("{name}.{domain}\t{name}"),
            None => name.to_string(),
        };
        lines.push(format!("127.0.1.1\t{names}\t{HOSTS_MARKER}"));

        let mut out = lines.join("\n");
        out.push('\n');
        self.host.write(&path, &out)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::MockHost;

    fn config(pairs: &[(&str, &str)]) -> ConfigTree {
        let schema = nightshade_schema::model::Schema::compiled();
        let mut tree = ConfigTree::new();
        for (path, value) in pairs {
            let path = Path::parse(path).unwrap();
            schema
                .apply_set(&mut tree, &path, Some(value))
                .unwrap_or_else(|e| panic!("{path}: {e}"));
        }
        tree
    }

    fn renderer() -> (SystemRenderer, Arc<MockHost>, Paths) {
        let host = Arc::new(MockHost::new());
        let paths = Paths::under("/test");
        (
            SystemRenderer::new(paths.clone(), Arc::clone(&host) as Arc<dyn Host>),
            host,
            paths,
        )
    }

    /// The domain suffix: a `search` line, and a fully qualified `/etc/hosts`
    /// entry so the box can state its own name.
    #[test]
    fn a_domain_name_becomes_a_search_line_and_an_fqdn() {
        let (renderer, host, paths) = renderer();
        let artifacts = renderer
            .render(&config(&[
                ("system host-name", "fw-01"),
                ("system domain-name", "example.com"),
                ("system name-server", "1.1.1.1"),
            ]))
            .unwrap();
        renderer.check(&artifacts).expect("check");

        let resolv = &artifacts.files[&paths.resolv_conf()];
        // `search` ahead of the resolvers, and exactly one of it.
        assert!(resolv.contains("search example.com
"), "{resolv}");
        assert_eq!(resolv.matches("search ").count(), 1, "{resolv}");
        assert!(
            resolv.find("search ") < resolv.find("nameserver "),
            "{resolv}"
        );
        // `domain` and `search` are mutually exclusive; only one is written.
        assert!(!resolv.contains("
domain "), "{resolv}");

        renderer.apply(&artifacts).expect("apply");
        let hosts = host.file(paths.hosts()).expect("an /etc/hosts");
        // FQDN first, short name second, or `hostname -f` cannot answer.
        assert!(
            hosts.contains("127.0.1.1	fw-01.example.com	fw-01	"),
            "{hosts}"
        );
    }

    /// With no domain configured, nothing is invented.
    #[test]
    fn no_domain_name_leaves_the_short_name_alone() {
        let (renderer, host, paths) = renderer();
        let artifacts = renderer
            .render(&config(&[("system host-name", "fw-01")]))
            .unwrap();
        assert!(
            !artifacts.files[&paths.resolv_conf()].contains("search"),
            "a search line appeared without a domain"
        );
        renderer.apply(&artifacts).expect("apply");
        let hosts = host.file(paths.hosts()).expect("an /etc/hosts");
        assert!(hosts.contains("127.0.1.1	fw-01	"), "{hosts}");
        assert!(!hosts.contains("fw-01."), "{hosts}");
    }

    #[test]
    fn resolvers_become_resolv_conf_in_config_order() {
        let (renderer, _, paths) = renderer();
        let artifacts = renderer
            .render(&config(&[
                ("system name-server", "9.9.9.9"),
                ("system name-server", "1.1.1.1"),
            ]))
            .unwrap();

        let resolv = &artifacts.files[&paths.resolv_conf()];
        assert!(resolv.starts_with("# Managed by Nightshade."), "{resolv}");
        // Sorted, because the config tree is, so two ways of arriving at the
        // same resolvers produce the same file.
        assert!(resolv.ends_with("nameserver 1.1.1.1\nnameserver 9.9.9.9\n"), "{resolv}");
    }

    #[test]
    fn no_resolvers_still_writes_the_file() {
        let (renderer, _, paths) = renderer();
        let artifacts = renderer.render(&config(&[("system host-name", "fw")])).unwrap();
        let resolv = &artifacts.files[&paths.resolv_conf()];
        assert!(!resolv.contains("nameserver"), "{resolv}");
    }

    #[test]
    fn host_name_and_time_zone_become_commands_never_a_shell() {
        let (renderer, host, _) = renderer();
        let artifacts = renderer
            .render(&config(&[
                ("system host-name", "fw-01"),
                ("system time-zone", "UTC"),
            ]))
            .unwrap();
        renderer.apply(&artifacts).unwrap();

        assert_eq!(
            host.commands(),
            [
                "hostnamectl set-hostname fw-01",
                "timedatectl set-timezone UTC"
            ]
        );
        for command in host.commands() {
            assert!(!command.contains("sh -c"), "{command}");
        }
    }

    #[test]
    fn the_hosts_line_is_maintained_and_the_rest_of_the_file_is_not_touched() {
        let (renderer, host, paths) = renderer();
        host.write(
            &paths.hosts(),
            "127.0.0.1\tlocalhost\n::1\tlocalhost ip6-localhost\n10.0.0.5\tsomething-else\n",
        )
        .unwrap();

        let artifacts = renderer.render(&config(&[("system host-name", "fw-01")])).unwrap();
        renderer.apply(&artifacts).unwrap();

        let hosts = host.file(paths.hosts()).unwrap();
        assert!(hosts.contains("127.0.0.1\tlocalhost\n"), "{hosts}");
        assert!(hosts.contains("10.0.0.5\tsomething-else\n"), "{hosts}");
        assert!(hosts.contains("127.0.1.1\tfw-01\t# nightshade hostname\n"), "{hosts}");

        // Renaming replaces our line rather than adding a second.
        let artifacts = renderer.render(&config(&[("system host-name", "fw-02")])).unwrap();
        renderer.apply(&artifacts).unwrap();
        let hosts = host.file(paths.hosts()).unwrap();
        assert_eq!(hosts.matches("# nightshade hostname").count(), 1, "{hosts}");
        assert!(hosts.contains("fw-02"), "{hosts}");
        assert!(!hosts.contains("fw-01"), "{hosts}");
    }

    #[test]
    fn rendering_is_a_function_of_the_config() {
        let (renderer, _, _) = renderer();
        let config = config(&[
            ("system host-name", "fw"),
            ("system name-server", "1.1.1.1"),
            ("system time-zone", "Europe/London"),
        ]);
        assert_eq!(renderer.render(&config).unwrap(), renderer.render(&config).unwrap());
    }

    #[test]
    fn the_last_applied_artifacts_come_back() {
        let (renderer, _, _) = renderer();
        assert!(renderer.previous().is_none());

        let artifacts = renderer.render(&config(&[("system host-name", "fw")])).unwrap();
        renderer.remember(&artifacts).unwrap();
        assert_eq!(renderer.previous().unwrap(), artifacts);
    }
}
