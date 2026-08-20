//! Turning a [`Snapshot`] into the text an operator reads.
//!
//! One module per command family, one entry point. Every renderer takes the
//! whole snapshot and picks its own rows out of it, because which interfaces a
//! command lists is part of what the command means: `show interfaces status`
//! is about ports, `show interfaces counters` is about the ports that carry
//! counters, and neither of them is "everything the daemon sent".
//!
//! # Nothing here can fail
//!
//! Every function returns a `String`. A renderer that could return an error
//! would be a renderer that prints nothing at all because one optic did not
//! answer, and `show interfaces` is what an operator runs when something is
//! already wrong. Missing data is missing lines, never a missing command.

use crate::layout::natural_cmp;
use crate::model::{Interface, Kind, Snapshot};
use crate::query::View;

pub mod capabilities;
pub mod counters;
pub mod description;
pub mod detail;
pub mod flowcontrol;
pub mod mac;
pub mod negotiation;
pub mod phy;
pub mod status;
pub mod transceiver;

/// Render a snapshot the way the view asks for.
pub fn render(snapshot: &Snapshot, view: &View) -> String {
    match view {
        View::Detail => detail::render(snapshot),
        View::Description => description::render(snapshot),
        View::Status(filter) => status::render(snapshot, *filter),
        View::Counters => counters::totals(snapshot),
        View::CountersErrors => counters::errors(snapshot),
        View::CountersDiscards => counters::discards(snapshot),
        View::CountersRates => counters::rates(snapshot),
        View::CountersQueue => counters::queue(snapshot),
        View::CountersBins => counters::bins(snapshot),
        View::Transceiver => transceiver::summary(snapshot),
        View::TransceiverDetail => transceiver::detail(snapshot),
        View::TransceiverProperties => transceiver::properties(snapshot),
        View::TransceiverEeprom => transceiver::eeprom(snapshot),
        View::Capabilities => capabilities::render(snapshot),
        View::FlowControl => flowcontrol::render(snapshot),
        View::Negotiation => negotiation::summary(snapshot),
        View::NegotiationDetail => negotiation::detail(snapshot),
        View::Phy => phy::render(snapshot, false),
        View::PhyDetail => phy::render(snapshot, true),
        View::Mac => mac::summary(snapshot),
        View::MacDetail => mac::detail(snapshot),
    }
}

/// The interfaces a command lists, in the order it lists them.
///
/// Physical ports first, then everything built on top of them, each group in
/// natural order: `eth0, eth1, ... eth9, eth10, bond0, lo, vlan10`.
///
/// The ports come first because they are the layer a fault is at. An operator
/// running `show interfaces` on a box that has stopped forwarding is looking
/// for a link that is down, and putting `bond0` between `eth1` and `eth2`
/// because `b` sorts before `e` buries the row they came for.
///
/// Sorted here rather than trusted from the wire: a second frontend that built
/// a snapshot in a different order must not produce a differently ordered
/// table.
pub(crate) fn rows(snapshot: &Snapshot, keep: impl Fn(&Interface) -> bool) -> Vec<&Interface> {
    let mut chosen: Vec<&Interface> = snapshot.interfaces.iter().filter(|i| keep(i)).collect();
    chosen.sort_by(|a, b| {
        a.kind
            .is_physical()
            .cmp(&b.kind.is_physical())
            .reverse()
            .then_with(|| natural_cmp(&a.name, &b.name))
    });
    chosen
}

/// The physical ports, which is what the hardware tables are about.
pub(crate) fn physical(snapshot: &Snapshot) -> Vec<&Interface> {
    rows(snapshot, |interface| {
        interface.kind.is_physical() && interface.present
    })
}

/// The names of a row set, for sizing the name column.
pub(crate) fn names(interfaces: &[&Interface]) -> Vec<String> {
    interfaces.iter().map(|i| i.name.clone()).collect()
}

/// A bond has no hardware of its own, so it has no counters of its own worth
/// tabulating -- the members carry them, and adding them up would double every
/// packet that crossed the box.
pub(crate) fn tabulates_counters(interface: &Interface) -> bool {
    interface.kind.is_physical() && interface.present
}

/// Join blocks with a blank line between them, and none after the last.
pub(crate) fn stanzas(blocks: Vec<String>) -> String {
    let blocks: Vec<String> = blocks.into_iter().filter(|b| !b.is_empty()).collect();
    if blocks.is_empty() {
        return String::new();
    }
    let mut out = blocks.join("\n");
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out
}

/// Whether an interface is one the transceiver commands have anything to say
/// about. Copper ports have no module and are left out entirely rather than
/// filling the table with `N/A`.
pub(crate) fn has_module(interface: &Interface) -> bool {
    interface.transceiver.is_some()
}

/// The `Kind` a name suggests, for a device the configuration does not
/// describe.
///
/// Used by the collector; here because the rule -- which prefix means what --
/// belongs with the type it produces rather than with the netlink code that
/// happens to call it first.
pub fn kind_from_name(name: &str, netlink_kind: Option<&str>) -> Kind {
    match netlink_kind {
        Some("vlan") => return Kind::Vlan,
        Some("bond") => return Kind::PortChannel,
        Some("bridge") => return Kind::Bridge,
        Some("wireguard") => return Kind::Wireguard,
        Some("gre" | "gretap" | "ipip" | "sit" | "vti" | "tun") => return Kind::Tunnel,
        _ => {}
    }
    if name == "lo" {
        Kind::Loopback
    } else if name.starts_with("vlan") {
        Kind::Vlan
    } else if name.starts_with("bond") {
        Kind::PortChannel
    } else if name.starts_with("br") {
        Kind::Bridge
    } else if name.starts_with("tun") {
        Kind::Tunnel
    } else if name.starts_with("wg") {
        Kind::Wireguard
    } else {
        Kind::Ethernet
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Interface;

    fn snapshot(names: &[(&str, Kind)]) -> Snapshot {
        Snapshot {
            interfaces: names
                .iter()
                .map(|(name, kind)| Interface::new(*name, *kind))
                .collect(),
            ..Snapshot::default()
        }
    }

    #[test]
    fn rows_come_back_ports_first_whatever_order_they_arrived_in() {
        let snapshot = snapshot(&[
            ("vlan10", Kind::Vlan),
            ("eth10", Kind::Ethernet),
            ("bond0", Kind::PortChannel),
            ("eth2", Kind::Ethernet),
            ("lo", Kind::Loopback),
        ]);
        let names: Vec<&str> = rows(&snapshot, |_| true)
            .iter()
            .map(|i| i.name.as_str())
            .collect();
        assert_eq!(names, ["eth2", "eth10", "bond0", "lo", "vlan10"]);
    }

    #[test]
    fn a_kind_is_read_from_the_kernel_first_and_the_name_second() {
        assert_eq!(kind_from_name("eth0", None), Kind::Ethernet);
        assert_eq!(kind_from_name("lo", None), Kind::Loopback);
        assert_eq!(kind_from_name("vlan10", None), Kind::Vlan);
        assert_eq!(kind_from_name("bond0", None), Kind::PortChannel);
        assert_eq!(kind_from_name("tun0", None), Kind::Tunnel);
        assert_eq!(kind_from_name("wg0", None), Kind::Wireguard);
        // A VLAN that is not called one, which is what `ip link add link eth0
        // name uplink.10 type vlan` produces.
        assert_eq!(kind_from_name("uplink.10", Some("vlan")), Kind::Vlan);
        assert_eq!(kind_from_name("enp3s0", None), Kind::Ethernet);
    }

    #[test]
    fn stanzas_have_one_blank_line_between_them_and_none_at_the_end() {
        assert_eq!(
            stanzas(vec!["a\n".into(), "b\n".into()]),
            "a\n\nb\n"
        );
        assert_eq!(stanzas(vec!["a\n".into()]), "a\n");
        assert_eq!(stanzas(vec![]), "");
    }
}
