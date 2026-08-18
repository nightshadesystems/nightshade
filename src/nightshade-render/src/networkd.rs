//! Interfaces, as systemd-networkd sees them.
//!
//! All six interface types map onto `.network`, `.netdev` and `.link` files in
//! `/run/systemd/network`. Under `/run` rather than `/etc` because rendered
//! network config is derived state: it should not outlive a boot that did not
//! apply it, and an operator who edits it by hand should find it gone next
//! boot rather than silently overriding `config.boot`.
//!
//! # What `check` can and cannot do
//!
//! networkd has no dry-run. There is no way to hand it a directory and ask
//! whether it would work, so `check` cannot promise the kernel will accept the
//! result.
//!
//! What it does instead is assert the file set is consistent with itself:
//! every `.network` that enslaves an interface to a bond or bridge has that
//! device's `.netdev`, every device that is referenced exists, and no two
//! files claim the same interface. Individual values were schema-checked long
//! before rendering, so the remaining failure mode is a set that is
//! each-file-valid and collectively wrong -- which is what this catches.
//!
//! Everything past that is caught by the verify step after apply, and undone
//! by restoring the previous artifacts.
//!
//! # Some properties need the device rebuilt
//!
//! A bond's mode is fixed when the bond is created; `networkctl reload` reads
//! the new file and changes nothing. Those settings are listed in
//! [`RECREATE`], and a change to one deletes the device so networkd builds it
//! again. A table rather than a guess, because the failure mode of guessing
//! wrong is reporting success on a change that did not happen.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use nightshade_common::{MANAGED_MARKER, paths::Paths};
use nightshade_schema::config::{ConfigTree, Node};
use nightshade_schema::path::Path;

use crate::artifacts::{Action, ApplyError, Artifacts, LastApplied, Managed, RenderError, Renderer};
use crate::host::Host;
use crate::ini::Ini;

/// `.netdev` settings the kernel fixes when the device is created.
///
/// Keyed by `Kind=`. An empty list means every setting for that kind can be
/// changed on a live device, so it is never rebuilt -- which matters most for
/// bridges, where rebuilding would drop every port on the bridge in order to
/// change a number that is writable in place.
const RECREATE: &[(&str, &[&str])] = &[
    // The kernel refuses to change a bond's mode, hash policy or LACP rate
    // while the bond has members, which it always does by the time we care.
    ("bond", &["Mode", "TransmitHashPolicy", "LACPTransmitRate"]),
    // A VLAN's tag is its identity; there is no retagging in place.
    ("vlan", &["Id"]),
    // Likewise a VXLAN's VNI, and its endpoint and port are set at creation.
    (
        "vxlan",
        &["VNI", "Remote", "Group", "Local", "DestinationPort", "TTL", "Independent"],
    ),
    // STP, priority and ageing time are all writable on a running bridge, and
    // networkd sets them on reconfigure.
    ("bridge", &[]),
];

/// Interface types that get a `.netdev`, and the `Kind=` they map to.
const VIRTUAL: &[(&str, &str)] = &[
    ("vlan", "vlan"),
    ("bonding", "bond"),
    ("bridge", "bridge"),
    ("vxlan", "vxlan"),
];

pub struct NetworkdRenderer {
    paths: Paths,
    host: Arc<dyn Host>,
    last_applied: LastApplied,
}

impl NetworkdRenderer {
    pub fn new(paths: Paths, host: Arc<dyn Host>) -> Self {
        let last_applied =
            LastApplied::new(&paths.last_applied_dir(), "networkd", Arc::clone(&host));
        Self {
            paths,
            host,
            last_applied,
        }
    }
}

// ---------------------------------------------------------------------------
// reading the config
// ---------------------------------------------------------------------------

fn instances<'a>(config: &'a ConfigTree, kind: &str) -> BTreeMap<&'a String, &'a Node> {
    config
        .get(&Path::from_segments(["interfaces", kind]))
        .and_then(Node::children)
        .map(|children| children.iter().collect())
        .unwrap_or_default()
}

fn leaf<'a>(node: &'a Node, name: &str) -> Option<&'a str> {
    node.children()?.get(name)?.value()
}

fn list<'a>(node: &'a Node, name: &str) -> Vec<&'a str> {
    node.children()
        .and_then(|children| children.get(name))
        .and_then(Node::value_set)
        .map(|values| values.iter().map(String::as_str).collect())
        .unwrap_or_default()
}

fn flag(node: &Node, name: &str) -> bool {
    node.children().is_some_and(|children| {
        children
            .get(name)
            .is_some_and(|node| node.value_set().is_some_and(BTreeSet::is_empty))
    })
}

/// Which interface is enslaved to what, and what rides on each interface.
///
/// Built once before anything is emitted, because a member's `.network` needs
/// to name its master and a parent's `.network` needs to name every VLAN on
/// it -- neither of which is visible from the interface's own subtree.
#[derive(Default)]
struct Attachments {
    master: BTreeMap<String, String>,
    vlans: BTreeMap<String, Vec<String>>,
    vxlans: BTreeMap<String, Vec<String>>,
}

impl Attachments {
    fn of(config: &ConfigTree) -> Self {
        let mut out = Self::default();

        for (master_kind, key) in [("bonding", "Bond"), ("bridge", "Bridge")] {
            for (name, node) in instances(config, master_kind) {
                for member in list(node, "member") {
                    out.master
                        .insert(member.to_string(), format!("{key}={name}"));
                }
            }
        }

        for (name, node) in instances(config, "vlan") {
            if let Some(parent) = leaf(node, "parent") {
                out.vlans
                    .entry(parent.to_string())
                    .or_default()
                    .push(name.clone());
            }
        }

        for (name, node) in instances(config, "vxlan") {
            if let Some(source) = leaf(node, "source-interface") {
                out.vxlans
                    .entry(source.to_string())
                    .or_default()
                    .push(name.clone());
            }
        }

        out
    }
}

// ---------------------------------------------------------------------------
// emitting
// ---------------------------------------------------------------------------

fn physical_file(name: &str, extension: &str) -> String {
    format!("10{MANAGED_MARKER}{name}.{extension}")
}

fn virtual_file(name: &str, extension: &str) -> String {
    format!("20{MANAGED_MARKER}{name}.{extension}")
}

/// The `.network` every interface gets, whatever type it is.
fn network_file(name: &str, node: &Node, attachments: &Attachments) -> String {
    let mut ini = Ini::new();
    ini.section("Match").key("Name", name);

    ini.section("Link");
    ini.maybe("MTUBytes", leaf(node, "mtu"));
    if flag(node, "disable") {
        // Not `Unmanaged=`: that would make networkd ignore the interface and
        // leave it however it was found, which is not the same as down.
        ini.key("ActivationPolicy", "always-down");
    }

    ini.section("Network");
    ini.maybe("Description", leaf(node, "description"));

    let addresses = list(node, "address");
    let dhcp = addresses.contains(&"dhcp");
    if dhcp {
        ini.key("DHCP", "yes");
    }
    ini.each("Address", addresses.iter().filter(|a| **a != "dhcp"));

    // Enslavement, and what rides on top.
    if let Some(master) = attachments.master.get(name) {
        let (key, value) = master.split_once('=').expect("built with an =");
        ini.key(key, value);
    }
    if let Some(vlans) = attachments.vlans.get(name) {
        ini.each("VLAN", vlans);
    }
    if let Some(vxlans) = attachments.vxlans.get(name) {
        ini.each("VXLAN", vxlans);
    }

    ini.finish()
}

/// The `.link` file: what udev applies, including the port's name.
///
/// # One file per port, and why it cannot be two
///
/// udev does not merge `.link` files. The first one whose `[Match]` succeeds,
/// in lexical order across every directory it reads, is applied *in full* and
/// the rest are never looked at. So the name and the port's properties have to
/// be in the same file -- a separate `00-name.link` would win outright and
/// silently discard the speed and duplex somebody configured here.
///
/// # Matching on the permanent MAC
///
/// `hw-id` is the port's burned-in address, and matching on it is what makes
/// `eth0` mean the same socket on the chassis after a reboot, a kernel
/// upgrade, or another card being added in front of it. `OriginalName=` cannot
/// do that job: it matches the name the kernel invented (`ens33`, `enp1s0`),
/// which is the very thing being renamed, and it changes under exactly the
/// hardware moves the pin exists to survive.
///
/// Without `hw-id` there is nothing to pin to, so the file falls back to
/// matching the name as given and does not rename anything. That is the case
/// for a port an operator named by hand, and for `lo`.
///
/// Returns `None` when there would be nothing to say: an empty `.link` still
/// matches, and would override the defaults with nothing.
fn link_file(name: &str, node: &Node) -> Option<String> {
    let hw_id = leaf(node, "hw-id");
    let mac = leaf(node, "mac");
    let speed = leaf(node, "speed").filter(|s| *s != "auto");
    let duplex = leaf(node, "duplex").filter(|d| *d != "auto");
    if hw_id.is_none() && mac.is_none() && speed.is_none() && duplex.is_none() {
        return None;
    }

    let mut ini = Ini::new();
    match hw_id {
        Some(hw_id) => ini.section("Match").key("PermanentMACAddress", hw_id),
        None => ini.section("Match").key("OriginalName", name),
    };
    ini.section("Link");
    // Only when the port was pinned: `Name=` on a file matched by name is a
    // rename to what it is already called, which udev warns about.
    if hw_id.is_some() {
        ini.key("Name", name);
    }
    ini.maybe("MACAddress", mac);
    // The schema's speeds are megabits, which is what the `M` suffix means.
    ini.maybe("BitsPerSecond", speed.map(|s| format!("{s}M")));
    ini.maybe("Duplex", duplex);
    Some(ini.finish())
}

fn netdev_file(kind: &str, name: &str, node: &Node) -> String {
    let mut ini = Ini::new();
    ini.section("NetDev").key("Name", name).key("Kind", kind);
    ini.maybe("Description", leaf(node, "description"));
    ini.maybe("MTUBytes", leaf(node, "mtu"));
    ini.maybe("MACAddress", leaf(node, "mac"));

    match kind {
        "vlan" => {
            ini.section("VLAN");
            ini.maybe("Id", leaf(node, "id"));
        }
        "bond" => {
            ini.section("Bond");
            ini.maybe("Mode", leaf(node, "mode"));
            ini.maybe("TransmitHashPolicy", leaf(node, "hash-policy"));
            ini.maybe("LACPTransmitRate", leaf(node, "lacp-rate"));
            ini.maybe("MinLinks", leaf(node, "min-links"));
            ini.maybe("PrimarySlave", leaf(node, "primary"));
        }
        "bridge" => {
            ini.section("Bridge");
            ini.flag("STP", flag(node, "stp"));
            ini.maybe("Priority", leaf(node, "priority"));
            ini.maybe("AgeingTimeSec", leaf(node, "aging-time"));
        }
        "vxlan" => {
            ini.section("VXLAN");
            ini.maybe("VNI", leaf(node, "vni"));
            ini.maybe("Remote", leaf(node, "remote"));
            ini.maybe("Group", leaf(node, "group"));
            ini.maybe("Local", leaf(node, "source-address"));
            ini.maybe("DestinationPort", leaf(node, "port"));
            ini.maybe("TTL", leaf(node, "ttl"));
            // With no source interface there is nothing to attach the tunnel
            // to, so it has to be told to stand alone. Without this networkd
            // creates the device and never brings it up.
            if leaf(node, "source-interface").is_none() {
                ini.key("Independent", "true");
            }
        }
        _ => {}
    }
    ini.finish()
}

// ---------------------------------------------------------------------------
// a very small INI reader, for comparing what we wrote last time
// ---------------------------------------------------------------------------

/// `(section, key)` to values, for the recreation comparison.
///
/// Reads only what this renderer writes, which is a strict subset of the INI
/// syntax systemd accepts. It is not a general parser and does not need to be:
/// its input is the previous output of the function above.
fn settings(text: &str) -> BTreeMap<(String, String), Vec<String>> {
    let mut out: BTreeMap<(String, String), Vec<String>> = BTreeMap::new();
    let mut section = String::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            section = name.to_string();
        } else if let Some((key, value)) = line.split_once('=') {
            out.entry((section.clone(), key.trim().to_string()))
                .or_default()
                .push(value.trim().to_string());
        }
    }
    out
}

fn kind_of(netdev: &str) -> Option<String> {
    settings(netdev)
        .get(&("NetDev".to_string(), "Kind".to_string()))
        .and_then(|values| values.first().cloned())
}

/// Whether the device described by `before` has to be destroyed for `after` to
/// take effect.
fn needs_recreating(before: &str, after: &str) -> bool {
    let (Some(old_kind), Some(new_kind)) = (kind_of(before), kind_of(after)) else {
        return true;
    };
    if old_kind != new_kind {
        return true;
    }

    let Some((_, watched)) = RECREATE.iter().find(|(kind, _)| *kind == old_kind) else {
        // A kind with no entry in the table is one nobody has decided about.
        // Rebuilding is the answer that cannot silently do nothing.
        return true;
    };

    let before = settings(before);
    let after = settings(after);
    watched.iter().any(|key| {
        let at = |map: &BTreeMap<(String, String), Vec<String>>| {
            map.iter()
                .find(|((_, k), _)| k == key)
                .map(|(_, v)| v.clone())
        };
        at(&before) != at(&after)
    })
}

/// Netdevs to destroy before the new files take effect.
fn recreations(previous: Option<&Artifacts>, next: &Artifacts) -> Vec<String> {
    let files = |artifacts: Option<&Artifacts>| -> BTreeMap<String, String> {
        artifacts
            .and_then(|a| a.managed.first())
            .map(|m| m.files.clone())
            .unwrap_or_default()
    };
    let before = files(previous);
    let after = files(Some(next));

    let device = |name: &str| -> Option<String> {
        name.strip_suffix(".netdev")?
            .split_once(MANAGED_MARKER)
            .map(|(_, device)| device.to_string())
    };

    let mut out = BTreeSet::new();
    for (name, old) in &before {
        let Some(device) = device(name) else { continue };
        match after.get(name) {
            // Gone from the config. Removing the file does not remove the
            // device -- networkd creates netdevs but will not delete one it no
            // longer has a definition for -- so a deleted bond would otherwise
            // stay up carrying traffic nothing describes.
            None => {
                out.insert(device);
            }
            Some(new) if needs_recreating(old, new) => {
                out.insert(device);
            }
            Some(_) => {}
        }
    }
    out.into_iter().collect()
}

// ---------------------------------------------------------------------------
// the renderer
// ---------------------------------------------------------------------------

impl Renderer for NetworkdRenderer {
    fn name(&self) -> &'static str {
        "networkd"
    }

    fn owns(&self) -> Path {
        Path::from_segments(["interfaces"])
    }

    fn render(&self, config: &ConfigTree) -> Result<Artifacts, RenderError> {
        let attachments = Attachments::of(config);
        let mut files: BTreeMap<String, String> = BTreeMap::new();
        // `.link` files live in a different directory from everything else --
        // a persistent one, because udev reads them before configd exists.
        // See `Paths::link_dir`.
        let mut links: BTreeMap<String, String> = BTreeMap::new();

        // Physical and loopback: a .network, and a .link when there is
        // something for udev to do.
        for kind in ["ethernet", "loopback"] {
            for (name, node) in instances(config, kind) {
                files.insert(
                    physical_file(name, "network"),
                    network_file(name, node, &attachments),
                );
                if let Some(link) = link_file(name, node) {
                    links.insert(physical_file(name, "link"), link);
                }
            }
        }

        // Virtual: a .netdev to create the device and a .network to configure
        // it.
        for (config_kind, netdev_kind) in VIRTUAL {
            for (name, node) in instances(config, config_kind) {
                files.insert(
                    virtual_file(name, "netdev"),
                    netdev_file(netdev_kind, name, node),
                );
                files.insert(
                    virtual_file(name, "network"),
                    network_file(name, node, &attachments),
                );
            }
        }

        Ok(Artifacts {
            managed: vec![
                Managed {
                    dir: self.paths.networkd_dir(),
                    marker: MANAGED_MARKER.to_string(),
                    files,
                },
                Managed {
                    dir: self.paths.link_dir(),
                    marker: MANAGED_MARKER.to_string(),
                    files: links,
                },
            ],
            files: BTreeMap::new(),
            actions: vec![Action::ReloadNetworkd],
        })
    }

    fn check(&self, artifacts: &Artifacts) -> Result<(), RenderError> {
        let inconsistent = |message: String| RenderError::Inconsistent {
            subsystem: "networkd",
            message,
        };

        let files = artifacts
            .managed
            .first()
            .map(|m| m.files.clone())
            .unwrap_or_default();

        // Every device that has a .netdev, by name.
        let devices: BTreeSet<String> = files
            .keys()
            .filter_map(|name| {
                name.strip_suffix(".netdev")?
                    .split_once(MANAGED_MARKER)
                    .map(|(_, device)| device.to_string())
            })
            .collect();

        let mut claimed: BTreeMap<String, String> = BTreeMap::new();
        for (name, contents) in &files {
            if !name.ends_with(".network") {
                continue;
            }
            let parsed = settings(contents);

            // Two .network files matching the same interface means one of them
            // silently loses, and which one depends on lexical order.
            if let Some(matched) = parsed
                .get(&("Match".to_string(), "Name".to_string()))
                .and_then(|values| values.first())
                && let Some(other) = claimed.insert(matched.clone(), name.clone())
            {
                return Err(inconsistent(format!(
                    "{name} and {other} both configure {matched}"
                )));
            }

            // Everything this file points at has to exist as a device.
            for key in ["Bond", "Bridge", "VLAN", "VXLAN"] {
                let referenced = parsed
                    .get(&("Network".to_string(), key.to_string()))
                    .cloned()
                    .unwrap_or_default();
                for device in referenced {
                    if !devices.contains(&device) {
                        return Err(inconsistent(format!(
                            "{name} refers to {device} as {key}, but nothing creates it"
                        )));
                    }
                }
            }
        }

        // Every device we create must also be configured, or it comes up with
        // no addresses and no explanation.
        for device in &devices {
            let expected = virtual_file(device, "network");
            if !files.contains_key(&expected) {
                return Err(inconsistent(format!(
                    "{device} is created but has no .network file"
                )));
            }
        }

        Ok(())
    }

    fn apply(&self, artifacts: &Artifacts) -> Result<(), ApplyError> {
        // Files first, then destroy what has to be rebuilt, then reload.
        //
        // Deleting first would open a window where the device is gone and its
        // replacement is not described yet. This way networkd already has the
        // new definition when the old device disappears.
        for managed in &artifacts.managed {
            self.host.sync(&managed.dir, &managed.marker, &managed.files)?;
        }
        for (path, contents) in &artifacts.files {
            self.host.write(path, contents)?;
        }

        for device in recreations(self.previous().as_ref(), artifacts) {
            self.host.run(&Action::RecreateNetdev(device).argv().expect("a command"))?;
        }

        for action in &artifacts.actions {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::MockHost;
    use nightshade_schema::model::Schema;

    pub(crate) fn config(pairs: &[(&str, &str)]) -> ConfigTree {
        let schema = Schema::compiled();
        let mut tree = ConfigTree::new();
        for (path, value) in pairs {
            let path = Path::parse(path).unwrap();
            let value = (!value.is_empty()).then_some(*value);
            schema
                .apply_set(&mut tree, &path, value)
                .unwrap_or_else(|e| panic!("{path}: {e}"));
        }
        assert_eq!(schema.check_constraints(&tree), [], "fixture breaks a constraint");
        tree
    }

    fn renderer() -> (NetworkdRenderer, Arc<MockHost>) {
        let host = Arc::new(MockHost::new());
        (
            NetworkdRenderer::new(Paths::under("/test"), Arc::clone(&host) as Arc<dyn Host>),
            host,
        )
    }

    /// The networkd directory: `.network` and `.netdev`.
    fn rendered(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        let (renderer, _) = renderer();
        let artifacts = renderer.render(&config(pairs)).unwrap();
        renderer.check(&artifacts).expect("check must pass");
        artifacts.managed[0].files.clone()
    }

    /// The persistent directory: `.link` only.
    fn rendered_links(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        let (renderer, _) = renderer();
        let artifacts = renderer.render(&config(pairs)).unwrap();
        renderer.check(&artifacts).expect("check must pass");
        assert_eq!(artifacts.managed[1].dir, Paths::under("/test").link_dir());
        artifacts.managed[1].files.clone()
    }

    #[test]
    fn an_ethernet_becomes_a_network_file() {
        let files = rendered(&[
            ("interfaces ethernet eth0 address", "192.168.1.1/24"),
            ("interfaces ethernet eth0 description", "the uplink"),
            ("interfaces ethernet eth0 mtu", "9000"),
        ]);
        let network = &files["10-ns-eth0.network"];
        assert!(network.contains("[Match]\nName=eth0\n"), "{network}");
        assert!(network.contains("MTUBytes=9000"), "{network}");
        assert!(network.contains("Description=the uplink"), "{network}");
        assert!(network.contains("Address=192.168.1.1/24"), "{network}");
        // The .link files are not in this directory at all.
        assert!(!files.contains_key("10-ns-eth0.link"), "{files:#?}");
    }

    #[test]
    fn mac_speed_and_duplex_go_in_a_link_file() {
        let links = rendered_links(&[
            ("interfaces ethernet eth0 mac", "02:00:5e:10:00:01"),
            ("interfaces ethernet eth0 speed", "1000"),
            ("interfaces ethernet eth0 duplex", "full"),
        ]);
        let link = &links["10-ns-eth0.link"];
        assert!(link.contains("OriginalName=eth0"), "{link}");
        assert!(link.contains("MACAddress=02:00:5e:10:00:01"), "{link}");
        assert!(link.contains("BitsPerSecond=1000M"), "{link}");
        assert!(link.contains("Duplex=full"), "{link}");
    }

    #[test]
    fn auto_speed_and_duplex_say_nothing_at_all() {
        let links = rendered_links(&[
            ("interfaces ethernet eth0 speed", "auto"),
            ("interfaces ethernet eth0 duplex", "auto"),
        ]);
        assert!(
            !links.contains_key("10-ns-eth0.link"),
            "auto produced a .link file: {links:#?}"
        );
    }

    /// The rename. `hw-id` pins the name to the port's burned-in address, so
    /// the kernel's own guess (`ens33`) never reaches the operator.
    #[test]
    fn hw_id_pins_the_name_to_the_port() {
        let links = rendered_links(&[("interfaces ethernet eth0 hw-id", "00:0c:29:1a:2b:3c")]);
        let link = &links["10-ns-eth0.link"];
        assert!(
            link.contains("PermanentMACAddress=00:0c:29:1a:2b:3c"),
            "{link}"
        );
        assert!(link.contains("Name=eth0"), "{link}");
        // Matching the kernel's name would defeat the point: that name is the
        // thing being replaced.
        assert!(!link.contains("OriginalName"), "{link}");
    }

    /// One file per port, because udev applies the first match and no other.
    /// Name and properties therefore have to arrive together.
    #[test]
    fn a_pinned_port_carries_its_properties_in_the_same_file() {
        let links = rendered_links(&[
            ("interfaces ethernet eth0 hw-id", "00:0c:29:1a:2b:3c"),
            ("interfaces ethernet eth0 mac", "02:00:5e:10:00:01"),
            ("interfaces ethernet eth0 speed", "10000"),
            ("interfaces ethernet eth0 duplex", "full"),
        ]);
        assert_eq!(links.len(), 1, "{links:#?}");
        let link = &links["10-ns-eth0.link"];
        assert!(link.contains("PermanentMACAddress=00:0c:29:1a:2b:3c"), "{link}");
        assert!(link.contains("Name=eth0"), "{link}");
        // The administrative MAC is a different thing from the one matched on.
        assert!(link.contains("MACAddress=02:00:5e:10:00:01"), "{link}");
        assert!(link.contains("BitsPerSecond=10000M"), "{link}");
        assert!(link.contains("Duplex=full"), "{link}");
    }

    /// Renaming survives a reboot only if the file does, so it must be the
    /// persistent directory and never the tmpfs one.
    #[test]
    fn link_files_are_written_where_udev_reads_them_before_configd_runs() {
        let (renderer, _) = renderer();
        let artifacts = renderer
            .render(&config(&[("interfaces ethernet eth0 hw-id", "00:0c:29:1a:2b:3c")]))
            .unwrap();
        let paths = Paths::under("/test");
        let links = artifacts
            .managed
            .iter()
            .find(|m| m.files.keys().any(|f| f.ends_with(".link")))
            .expect("a directory holding the .link files");
        assert_eq!(links.dir, paths.link_dir());
        assert_ne!(links.dir, paths.networkd_dir());
    }

    #[test]
    fn dhcp_and_static_addresses_coexist() {
        let files = rendered(&[
            ("interfaces ethernet eth0 address", "dhcp"),
            ("interfaces ethernet eth0 address", "10.0.0.1/24"),
        ]);
        let network = &files["10-ns-eth0.network"];
        assert!(network.contains("DHCP=yes"), "{network}");
        assert!(network.contains("Address=10.0.0.1/24"), "{network}");
        assert!(!network.contains("Address=dhcp"), "{network}");
    }

    #[test]
    fn disable_holds_the_interface_down() {
        let files = rendered(&[("interfaces ethernet eth0 disable", "")]);
        assert!(
            files["10-ns-eth0.network"].contains("ActivationPolicy=always-down"),
            "{files:#?}"
        );
    }

    #[test]
    fn a_vlan_gets_a_netdev_and_its_parent_gains_a_vlan_line() {
        let files = rendered(&[
            ("interfaces ethernet eth0", ""),
            ("interfaces vlan vlan100 parent", "eth0"),
            ("interfaces vlan vlan100 id", "100"),
            ("interfaces vlan vlan100 address", "172.16.0.1/24"),
        ]);
        let netdev = &files["20-ns-vlan100.netdev"];
        assert!(netdev.contains("Kind=vlan"), "{netdev}");
        assert!(netdev.contains("[VLAN]\nId=100"), "{netdev}");
        assert!(files["20-ns-vlan100.network"].contains("Address=172.16.0.1/24"));
        assert!(files["10-ns-eth0.network"].contains("VLAN=vlan100"));
    }

    #[test]
    fn a_bond_carries_its_settings_and_its_members_carry_none() {
        let files = rendered(&[
            ("interfaces ethernet eth1", ""),
            ("interfaces ethernet eth2", ""),
            ("interfaces bonding bond0 member", "eth1"),
            ("interfaces bonding bond0 member", "eth2"),
            ("interfaces bonding bond0 mode", "802.3ad"),
            ("interfaces bonding bond0 hash-policy", "layer3+4"),
            ("interfaces bonding bond0 address", "10.0.0.1/24"),
        ]);
        let netdev = &files["20-ns-bond0.netdev"];
        assert!(netdev.contains("Kind=bond"), "{netdev}");
        assert!(netdev.contains("Mode=802.3ad"), "{netdev}");
        assert!(netdev.contains("TransmitHashPolicy=layer3+4"), "{netdev}");

        for member in ["eth1", "eth2"] {
            let network = &files[&format!("10-ns-{member}.network")];
            assert!(network.contains("Bond=bond0"), "{network}");
            assert!(!network.contains("Address="), "{network}");
        }
        assert!(files["20-ns-bond0.network"].contains("Address=10.0.0.1/24"));
    }

    #[test]
    fn a_bridge_carries_its_settings_and_its_members_name_it() {
        let files = rendered(&[
            ("interfaces ethernet eth1", ""),
            ("interfaces bridge br0 member", "eth1"),
            ("interfaces bridge br0 stp", ""),
            ("interfaces bridge br0 priority", "4096"),
            ("interfaces bridge br0 aging-time", "600"),
        ]);
        let netdev = &files["20-ns-br0.netdev"];
        assert!(netdev.contains("Kind=bridge"), "{netdev}");
        assert!(netdev.contains("STP=yes"), "{netdev}");
        assert!(netdev.contains("Priority=4096"), "{netdev}");
        assert!(netdev.contains("AgeingTimeSec=600"), "{netdev}");
        assert!(files["10-ns-eth1.network"].contains("Bridge=br0"));
    }

    #[test]
    fn a_vxlan_attaches_to_its_source_or_stands_alone() {
        let attached = rendered(&[
            ("interfaces ethernet eth0", ""),
            ("interfaces vxlan vxlan1 vni", "4242"),
            ("interfaces vxlan vxlan1 remote", "10.0.0.2"),
            ("interfaces vxlan vxlan1 source-interface", "eth0"),
        ]);
        let netdev = &attached["20-ns-vxlan1.netdev"];
        assert!(netdev.contains("VNI=4242"), "{netdev}");
        assert!(netdev.contains("Remote=10.0.0.2"), "{netdev}");
        assert!(!netdev.contains("Independent"), "{netdev}");
        assert!(attached["10-ns-eth0.network"].contains("VXLAN=vxlan1"));

        let alone = rendered(&[
            ("interfaces vxlan vxlan1 vni", "4242"),
            ("interfaces vxlan vxlan1 remote", "10.0.0.2"),
        ]);
        assert!(
            alone["20-ns-vxlan1.netdev"].contains("Independent=true"),
            "a vxlan with no source interface must stand alone"
        );
    }

    #[test]
    fn rendering_is_a_function_of_the_config() {
        let config = config(&[
            ("interfaces ethernet eth1", ""),
            ("interfaces bonding bond0 member", "eth1"),
            ("interfaces bonding bond0 address", "10.0.0.1/24"),
        ]);
        let (renderer, _) = renderer();
        assert_eq!(renderer.render(&config).unwrap(), renderer.render(&config).unwrap());
    }

    // -- check --------------------------------------------------------------

    #[test]
    fn check_catches_a_reference_to_a_device_nothing_creates() {
        let (renderer, _) = renderer();
        let mut artifacts = renderer
            .render(&config(&[
                ("interfaces ethernet eth1", ""),
                ("interfaces bonding bond0 member", "eth1"),
            ]))
            .unwrap();
        assert!(renderer.check(&artifacts).is_ok());

        // Lose the bond's netdev, as a partial render would.
        artifacts.managed[0].files.remove("20-ns-bond0.netdev");
        let err = renderer.check(&artifacts).unwrap_err();
        assert!(err.to_string().contains("nothing creates it"), "{err}");
    }

    #[test]
    fn check_catches_two_files_claiming_one_interface() {
        let (renderer, _) = renderer();
        let mut artifacts = renderer
            .render(&config(&[("interfaces ethernet eth0", "")]))
            .unwrap();
        let duplicate = artifacts.managed[0].files["10-ns-eth0.network"].clone();
        artifacts.managed[0]
            .files
            .insert("20-ns-imposter.network".into(), duplicate);

        let err = renderer.check(&artifacts).unwrap_err();
        assert!(err.to_string().contains("both configure eth0"), "{err}");
    }

    #[test]
    fn check_catches_a_device_with_no_configuration() {
        let (renderer, _) = renderer();
        let mut artifacts = renderer
            .render(&config(&[
                ("interfaces vxlan vxlan1 vni", "1"),
                ("interfaces vxlan vxlan1 remote", "10.0.0.2"),
            ]))
            .unwrap();
        artifacts.managed[0].files.remove("20-ns-vxlan1.network");
        let err = renderer.check(&artifacts).unwrap_err();
        assert!(err.to_string().contains("no .network file"), "{err}");
    }

    // -- recreation ---------------------------------------------------------

    fn netdev(pairs: &[(&str, &str)], device: &str) -> String {
        let (renderer, _) = renderer();
        renderer.render(&config(pairs)).unwrap().managed[0].files
            [&virtual_file(device, "netdev")]
            .clone()
    }

    #[test]
    fn a_bond_mode_change_needs_the_device_rebuilt() {
        let before = netdev(
            &[
                ("interfaces ethernet eth1", ""),
                ("interfaces bonding bond0 member", "eth1"),
                ("interfaces bonding bond0 mode", "802.3ad"),
            ],
            "bond0",
        );
        let after = netdev(
            &[
                ("interfaces ethernet eth1", ""),
                ("interfaces bonding bond0 member", "eth1"),
                ("interfaces bonding bond0 mode", "active-backup"),
            ],
            "bond0",
        );
        assert!(needs_recreating(&before, &after));
        assert!(!needs_recreating(&before, &before));
    }

    #[test]
    fn a_bridge_setting_change_does_not_tear_the_bridge_down() {
        let with = |priority: &str, aging: &str| {
            netdev(
                &[
                    ("interfaces bridge br0 priority", priority),
                    ("interfaces bridge br0 aging-time", aging),
                ],
                "br0",
            )
        };
        // Every bridge setting is writable on a live bridge; rebuilding one
        // would drop every port for nothing.
        assert!(!needs_recreating(&with("4096", "300"), &with("32768", "600")));
    }

    #[test]
    fn a_vlan_id_change_needs_the_device_rebuilt() {
        let with = |id: &str| {
            netdev(
                &[
                    ("interfaces ethernet eth0", ""),
                    ("interfaces vlan vlan100 parent", "eth0"),
                    ("interfaces vlan vlan100 id", id),
                ],
                "vlan100",
            )
        };
        assert!(needs_recreating(&with("100"), &with("200")));
    }

    #[test]
    fn a_device_removed_from_the_config_is_destroyed() {
        let (renderer, host) = renderer();

        let before = renderer
            .render(&config(&[
                ("interfaces ethernet eth1", ""),
                ("interfaces bonding bond0 member", "eth1"),
            ]))
            .unwrap();
        renderer.apply(&before).unwrap();
        renderer.remember(&before).unwrap();
        host.take_ops();

        // The bond is gone from the config. Removing its file does not remove
        // the device, so it has to be deleted explicitly or it stays up.
        let after = renderer
            .render(&config(&[("interfaces ethernet eth1", "")]))
            .unwrap();
        renderer.apply(&after).unwrap();

        assert_eq!(
            host.commands(),
            ["networkctl delete bond0", "networkctl reload"]
        );
    }

    #[test]
    fn files_are_written_before_the_device_is_destroyed() {
        let (renderer, host) = renderer();
        let before = netdev_artifacts(&renderer, "802.3ad");
        renderer.apply(&before).unwrap();
        renderer.remember(&before).unwrap();
        host.take_ops();

        let after = netdev_artifacts(&renderer, "active-backup");
        renderer.apply(&after).unwrap();

        // Sync, then delete, then reload. Deleting first would leave a window
        // where the device is gone and nothing describes its replacement.
        let ops = host.ops();
        let sync_at = ops
            .iter()
            .position(|op| matches!(op, crate::host::Op::Sync { .. }))
            .expect("a sync");
        let delete_at = ops
            .iter()
            .position(|op| matches!(op, crate::host::Op::Run { argv } if argv.contains(&"delete".to_string())))
            .expect("a delete");
        let reload_at = ops
            .iter()
            .position(|op| matches!(op, crate::host::Op::Run { argv } if argv.contains(&"reload".to_string())))
            .expect("a reload");
        assert!(sync_at < delete_at && delete_at < reload_at, "{ops:#?}");
    }

    fn netdev_artifacts(renderer: &NetworkdRenderer, mode: &str) -> Artifacts {
        renderer
            .render(&config(&[
                ("interfaces ethernet eth1", ""),
                ("interfaces bonding bond0 member", "eth1"),
                ("interfaces bonding bond0 mode", mode),
            ]))
            .unwrap()
    }

    #[test]
    fn nothing_is_recreated_when_nothing_changed() {
        let (renderer, host) = renderer();
        let artifacts = netdev_artifacts(&renderer, "802.3ad");
        renderer.apply(&artifacts).unwrap();
        renderer.remember(&artifacts).unwrap();
        host.take_ops();

        renderer.apply(&artifacts).unwrap();
        assert_eq!(host.commands(), ["networkctl reload"]);
    }
}
