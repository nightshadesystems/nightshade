//! `show interfaces status`, and its filters.
//!
//! Ports and bonds only. A VLAN or a tunnel has no `Duplex`, no `Speed` and no
//! `Type`, and listing them here would be five columns of `N/A` in front of
//! the rows somebody is actually reading.
//!
//! `show interfaces status errdisabled` is a different table rather than the
//! same one filtered, because when a port has been shut by the box the only
//! thing worth the width is why.

use crate::layout::{Align, Layout, name_width, truncate};
use crate::model::{Interface, Link, Membership, Snapshot, SpeedSource};
use crate::units;

use super::names;

const PORT: usize = 11;
const NAME: usize = 30;
/// The description is cut here, hard. See [`crate::layout::truncate`].
const NAME_TEXT: usize = 26;
const STATUS: usize = 13;
const VLAN: usize = 9;
const DUPLEX: usize = 7;
const SPEED: usize = 7;
const TYPE: usize = 16;
const FLAGS: usize = 6;

pub fn render(snapshot: &Snapshot, filter: Option<Link>) -> String {
    let interfaces = super::rows(snapshot, |interface| {
        interface.kind.is_port() && filter.is_none_or(|wanted| interface.link == wanted)
    });

    if filter == Some(Link::ErrDisabled) {
        return errdisabled(&interfaces);
    }

    let layout = Layout::new(&[
        (PORT, Align::Left),
        (NAME, Align::Left),
        (STATUS, Align::Left),
        (VLAN, Align::Left),
        (DUPLEX, Align::Left),
        (SPEED, Align::Left),
        (TYPE, Align::Left),
        (FLAGS, Align::Left),
    ])
    .widen(0, name_width(&names(&interfaces), PORT));

    let mut out = String::new();
    out.push_str(&layout.row(&[
        "Port",
        "Name",
        "Status",
        "Vlan",
        "Duplex",
        "Speed",
        "Type",
        "Flags",
        "Encapsulation",
    ]));
    out.push('\n');

    for interface in interfaces {
        out.push_str(&layout.row(&[
            interface.name.clone(),
            truncate(interface.description.as_deref().unwrap_or(""), NAME_TEXT),
            interface.link.label().to_string(),
            vlan_cell(interface),
            duplex_cell(interface),
            speed_cell(interface),
            interface
                .media_type
                .clone()
                .unwrap_or_else(|| "N/A".to_string()),
            interface.flags.clone().unwrap_or_default(),
            interface.encapsulation.clone().unwrap_or_default(),
        ]));
        out.push('\n');
    }
    out
}

/// The errdisabled table: the same first three columns, and the reason.
fn errdisabled(interfaces: &[&Interface]) -> String {
    let layout = Layout::new(&[
        (PORT, Align::Left),
        (NAME, Align::Left),
        (STATUS, Align::Left),
    ])
    .widen(0, name_width(&names(interfaces), PORT));

    let mut out = String::new();
    out.push_str(&layout.row(&["Port", "Name", "Status", "Reason"]));
    out.push('\n');
    for interface in interfaces {
        out.push_str(&layout.row(&[
            interface.name.clone(),
            truncate(interface.description.as_deref().unwrap_or(""), NAME_TEXT),
            interface.link.label().to_string(),
            interface
                .errdisable_reason
                .clone()
                .unwrap_or_else(|| "unknown".to_string()),
        ]));
        out.push('\n');
    }
    out
}

fn vlan_cell(interface: &Interface) -> String {
    match &interface.membership {
        Membership::Routed => "routed".to_string(),
        Membership::Trunk => "trunk".to_string(),
        Membership::Access(id) => id.to_string(),
        // Linux naming, so the cell reads `in bond0`. EOS would say `in Po1`;
        // renaming the bond to say it would be renaming it everywhere.
        Membership::InBond(bond) => format!("in {bond}"),
        Membership::Unknown => String::new(),
    }
}

/// `full`, or `a-full` when autonegotiation is what settled it.
fn duplex_cell(interface: &Interface) -> String {
    match interface.duplex {
        Some(duplex) => auto_prefixed(interface, duplex.short()),
        None => "unconf".to_string(),
    }
}

/// `10G`, `a-1G`, or `unconf` for a port with no link and no forced speed.
fn speed_cell(interface: &Interface) -> String {
    match interface.speed_mbps {
        Some(speed) => auto_prefixed(interface, &units::speed_short(speed)),
        None => "unconf".to_string(),
    }
}

/// The `a-` that marks a value autonegotiation produced rather than one
/// configuration pinned. See [`SpeedSource`].
fn auto_prefixed(interface: &Interface, value: &str) -> String {
    match interface.speed_source {
        SpeedSource::Negotiated => format!("a-{value}"),
        SpeedSource::Forced => value.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Duplex, Kind};

    fn port(name: &str) -> Interface {
        let mut interface = Interface::new(name, Kind::Ethernet);
        interface.admin_up = true;
        interface.link = Link::Connected;
        interface.duplex = Some(Duplex::Full);
        interface.speed_mbps = Some(1_000);
        interface
    }

    #[test]
    fn a_negotiated_speed_and_duplex_are_marked_and_a_forced_one_is_not() {
        let mut forced = port("eth0");
        forced.speed_source = SpeedSource::Forced;
        assert_eq!(duplex_cell(&forced), "full");
        assert_eq!(speed_cell(&forced), "1G");

        let mut negotiated = port("eth1");
        negotiated.speed_source = SpeedSource::Negotiated;
        assert_eq!(duplex_cell(&negotiated), "a-full");
        assert_eq!(speed_cell(&negotiated), "a-1G");
    }

    /// A port with no link and no forced speed has no speed. `unconf` is not
    /// marked `a-`, because nothing was negotiated.
    #[test]
    fn an_unconfigured_speed_is_never_marked_negotiated() {
        let mut down = port("eth2");
        down.speed_mbps = None;
        down.speed_source = SpeedSource::Negotiated;
        assert_eq!(speed_cell(&down), "unconf");
    }

    #[test]
    fn the_vlan_column_says_what_the_port_is_for() {
        let mut interface = port("eth0");
        interface.membership = Membership::Routed;
        assert_eq!(vlan_cell(&interface), "routed");
        interface.membership = Membership::Trunk;
        assert_eq!(vlan_cell(&interface), "trunk");
        interface.membership = Membership::Access(1);
        assert_eq!(vlan_cell(&interface), "1");
        interface.membership = Membership::InBond("bond0".into());
        assert_eq!(vlan_cell(&interface), "in bond0");
        interface.membership = Membership::Unknown;
        assert_eq!(vlan_cell(&interface), "");
    }

    #[test]
    fn a_filter_restricts_the_rows_and_leaves_the_header_alone() {
        let mut up = port("eth0");
        up.link = Link::Connected;
        let mut down = port("eth1");
        down.link = Link::NotConnect;
        let snapshot = Snapshot {
            interfaces: vec![up, down],
            ..Snapshot::default()
        };

        let all = render(&snapshot, None);
        let connected = render(&snapshot, Some(Link::Connected));
        assert_eq!(all.lines().count(), 3);
        assert_eq!(connected.lines().count(), 2);
        assert_eq!(all.lines().next(), connected.lines().next());
    }

    /// Not the same table with fewer rows: a different table.
    #[test]
    fn errdisabled_is_asked_a_different_question() {
        let mut broken = port("eth6");
        broken.link = Link::ErrDisabled;
        broken.errdisable_reason = Some("link-flap".into());
        let snapshot = Snapshot {
            interfaces: vec![broken],
            ..Snapshot::default()
        };
        let text = render(&snapshot, Some(Link::ErrDisabled));
        assert!(text.starts_with("Port       Name                          Status       Reason\n"));
        assert!(text.contains("errdisabled  link-flap"), "{text}");
    }

    #[test]
    fn a_description_is_cut_at_twenty_six_characters() {
        let mut interface = port("eth0");
        interface.description = Some("WAN uplink to ISP - Circuit ID 4471-A".into());
        let snapshot = Snapshot {
            interfaces: vec![interface],
            ..Snapshot::default()
        };
        let text = render(&snapshot, None);
        assert!(text.contains("WAN uplink to ISP - Circui    "), "{text}");
        assert!(!text.contains("Circuit"), "{text}");
    }
}
