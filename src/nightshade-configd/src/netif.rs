//! What the kernel says about the interfaces.
//!
//! Read from `/sys/class/net`, which needs no privilege, no netlink socket and
//! no crate. The CLI does not read it: `show interfaces` goes through configd
//! like everything else, so there is one place that knows what an interface is
//! and one answer to what it looks like.
//!
//! # What this shows, and what it does not
//!
//! Link state, MAC and MTU come from the kernel. Addresses come from the
//! *running configuration*, and are labelled as such.
//!
//! That is a real limitation and worth being plain about: `/sys` does not
//! carry addresses, and reading them properly means netlink. Until the
//! real-apply path grows that, `show interfaces` answers "what is configured,
//! and is the link up" rather than "what addresses does the kernel hold". The
//! most useful thing it can tell an operator -- that a configured interface is
//! not present at all -- it does tell them.

use std::collections::BTreeMap;

use nightshade_common::paths::Paths;
use nightshade_proto::message::InterfaceStatus;
use nightshade_schema::config::{ConfigTree, Node};
use nightshade_schema::path::Path;

/// Interface types in the schema, in the order they are worth listing.
const KINDS: &[&str] = &["loopback", "ethernet", "bonding", "bridge", "vlan", "vxlan"];

/// Everything configured, plus everything the kernel has that is not.
pub fn interfaces(paths: &Paths, running: &ConfigTree) -> Vec<InterfaceStatus> {
    let mut out: BTreeMap<String, InterfaceStatus> = BTreeMap::new();

    for kind in KINDS {
        let at = Path::from_segments(["interfaces", kind]);
        let Some(instances) = running.get(&at).and_then(Node::children) else {
            continue;
        };
        for (name, node) in instances {
            let mut status = from_sys(paths, name);
            status.kind = (*kind).to_string();
            status.addresses = node
                .children()
                .and_then(|children| children.get("address"))
                .and_then(Node::value_set)
                .map(|values| values.iter().cloned().collect())
                .unwrap_or_default();
            status.description = node
                .children()
                .and_then(|children| children.get("description"))
                .and_then(|node| node.value().map(str::to_string));
            out.insert(name.clone(), status);
        }
    }

    // Devices the kernel has that nothing configures. Worth showing: an
    // operator who has just racked a card wants to know what it came up as
    // before they can configure it.
    for name in present(paths) {
        out.entry(name.clone()).or_insert_with(|| from_sys(paths, &name));
    }

    out.into_values().collect()
}

pub fn interface(paths: &Paths, running: &ConfigTree, name: &str) -> Option<InterfaceStatus> {
    interfaces(paths, running)
        .into_iter()
        .find(|status| status.name == name)
}

/// Interface names the kernel currently has.
fn present(paths: &Paths) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(paths.sys_class_net()) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|entry| entry.file_name().to_str().map(str::to_string))
        .collect()
}

fn from_sys(paths: &Paths, name: &str) -> InterfaceStatus {
    let dir = paths.sys_class_net().join(name);
    let attribute = |file: &str| -> Option<String> {
        std::fs::read_to_string(dir.join(file))
            .ok()
            .map(|text| text.trim().to_string())
            .filter(|text| !text.is_empty())
    };

    InterfaceStatus {
        name: name.to_string(),
        kind: "unconfigured".to_string(),
        // `operstate` is the kernel's own word for it, passed through rather
        // than translated: an operator who reads `lowerlayerdown` here can
        // search for it and find the kernel's meaning.
        state: attribute("operstate").unwrap_or_else(|| "unknown".to_string()),
        mac: attribute("address"),
        mtu: attribute("mtu").and_then(|text| text.parse().ok()),
        addresses: Vec::new(),
        description: None,
        present: dir.exists(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a fake `/sys/class/net` so this is testable without a network.
    fn fake_sys(paths: &Paths, interfaces: &[(&str, &str, &str, &str)]) {
        for (name, state, mac, mtu) in interfaces {
            let dir = paths.sys_class_net().join(name);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("operstate"), format!("{state}\n")).unwrap();
            std::fs::write(dir.join("address"), format!("{mac}\n")).unwrap();
            std::fs::write(dir.join("mtu"), format!("{mtu}\n")).unwrap();
        }
    }

    fn config(pairs: &[(&str, &str)]) -> ConfigTree {
        let schema = nightshade_schema::model::Schema::compiled();
        let mut tree = ConfigTree::new();
        for (path, value) in pairs {
            let path = Path::parse(path).unwrap();
            let value = (!value.is_empty()).then_some(*value);
            schema.apply_set(&mut tree, &path, value).unwrap();
        }
        tree
    }

    #[test]
    fn configured_interfaces_carry_their_kernel_state() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::under(dir.path());
        fake_sys(&paths, &[("eth0", "up", "02:00:5e:10:00:01", "9000")]);

        let running = config(&[
            ("interfaces ethernet eth0 address", "10.0.0.1/24"),
            ("interfaces ethernet eth0 description", "the uplink"),
        ]);
        let listed = interfaces(&paths, &running);
        assert_eq!(listed.len(), 1);

        let eth0 = &listed[0];
        assert_eq!(eth0.name, "eth0");
        assert_eq!(eth0.kind, "ethernet");
        assert_eq!(eth0.state, "up");
        assert_eq!(eth0.mac.as_deref(), Some("02:00:5e:10:00:01"));
        assert_eq!(eth0.mtu, Some(9000));
        assert_eq!(eth0.addresses, ["10.0.0.1/24"]);
        assert_eq!(eth0.description.as_deref(), Some("the uplink"));
        assert!(eth0.present);
    }

    /// The most useful thing this command can say.
    #[test]
    fn a_configured_interface_the_kernel_does_not_have_is_marked_absent() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::under(dir.path());
        fake_sys(&paths, &[("eth0", "up", "02:00:5e:10:00:01", "1500")]);

        let running = config(&[
            ("interfaces ethernet eth0", ""),
            ("interfaces ethernet eth9", ""),
        ]);
        let listed = interfaces(&paths, &running);

        let missing = listed.iter().find(|i| i.name == "eth9").unwrap();
        assert!(!missing.present, "a missing interface was reported as present");
        assert_eq!(missing.state, "unknown");
        assert!(listed.iter().find(|i| i.name == "eth0").unwrap().present);
    }

    #[test]
    fn devices_nothing_configures_are_still_listed() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::under(dir.path());
        fake_sys(
            &paths,
            &[
                ("eth0", "up", "02:00:5e:10:00:01", "1500"),
                ("enp3s0", "down", "02:00:5e:10:00:02", "1500"),
            ],
        );

        let listed = interfaces(&paths, &config(&[("interfaces ethernet eth0", "")]));
        let new = listed.iter().find(|i| i.name == "enp3s0").unwrap();
        assert_eq!(new.kind, "unconfigured");
        assert_eq!(new.state, "down");
        assert!(new.present);
    }

    #[test]
    fn one_interface_can_be_asked_about_by_name() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::under(dir.path());
        fake_sys(&paths, &[("eth0", "up", "02:00:5e:10:00:01", "1500")]);

        let running = config(&[("interfaces ethernet eth0", "")]);
        assert!(interface(&paths, &running, "eth0").is_some());
        assert!(interface(&paths, &running, "eth1").is_none());
    }

    #[test]
    fn a_box_with_no_sysfs_reports_nothing_rather_than_failing() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::under(dir.path());
        assert!(interfaces(&paths, &ConfigTree::new()).is_empty());
    }
}
