//! `show interfaces mac` and its detail form.
//!
//! The MAC layer rather than the address book: the address the port answers
//! on, whether the MAC is up, whether either end is signalling a fault, and
//! what FEC is doing. On a 25G or 100G link the FEC codeword counters are the
//! first place a marginal cable shows, long before a packet is lost.

use crate::block::Block;
use crate::layout::{Align, Layout, name_width};
use crate::model::{Interface, Oper, Snapshot};

use super::{names, stanzas};

const PORT: usize = 8;
const ADDRESS: usize = 21;

/// Value column of the first group of detail rows.
const VALUE: usize = 26;
/// The FEC codeword rows line up with each other rather than with the group
/// above, because their labels are longer than that column. EOS's, preserved.
const FEC_VALUE: usize = 29;

pub fn summary(snapshot: &Snapshot) -> String {
    let interfaces = super::physical(snapshot);
    let layout = Layout::new(&[(PORT, Align::Left), (ADDRESS, Align::Left)])
        .widen(0, name_width(&names(&interfaces), PORT));

    let mut out = String::new();
    out.push_str(&layout.row(&["Port", "MAC Address", "State"]));
    out.push('\n');
    for interface in interfaces {
        out.push_str(&layout.row(&[
            interface.name.clone(),
            interface.mac.clone().unwrap_or_else(|| "N/A".to_string()),
            mac_state(interface),
        ]));
        out.push('\n');
    }
    out
}

/// What the MAC is doing, in the driver's words where it has any.
///
/// The fallback is not a guess about the PHY: a port that is administratively
/// down has had its PHY powered off, and that is what `phyOff` says.
pub fn mac_state(interface: &Interface) -> String {
    if let Some(layer) = &interface.mac_layer
        && !layer.state.is_empty()
    {
        return layer.state.clone();
    }
    if !interface.admin_up {
        "phyOff".to_string()
    } else if interface.oper == Oper::Up {
        "linkUp".to_string()
    } else {
        "linkDown".to_string()
    }
}

pub fn detail(snapshot: &Snapshot) -> String {
    let blocks: Vec<String> = super::rows(snapshot, |interface: &Interface| {
        interface.kind.is_physical() && interface.mac_layer.is_some()
    })
    .into_iter()
    .map(|interface| {
        let layer = interface.mac_layer.as_ref().expect("filtered on it");
        let mut block = Block::new();
        block.heading(&interface.name);
        block.maybe_aligned(2, "MAC address:", interface.mac.clone(), VALUE);
        block.aligned(2, "MAC state:", &mac_state(interface), VALUE);
        block.maybe_aligned(2, "Local fault:", truth(layer.local_fault), VALUE);
        block.maybe_aligned(2, "Remote fault:", truth(layer.remote_fault), VALUE);
        block.maybe_aligned(2, "FEC mode:", layer.fec_mode.clone(), VALUE);
        block.maybe_aligned(
            2,
            "FEC corrected codewords:",
            layer.fec_corrected.map(|n| n.to_string()),
            FEC_VALUE,
        );
        block.maybe_aligned(
            2,
            "FEC uncorrected codewords:",
            layer.fec_uncorrected.map(|n| n.to_string()),
            FEC_VALUE,
        );
        block.take()
    })
    .collect();
    stanzas(blocks)
}

fn truth(value: Option<bool>) -> Option<String> {
    value.map(|value| if value { "True" } else { "False" }.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Kind, MacLayer};

    fn port(name: &str, admin_up: bool, oper: Oper) -> Interface {
        let mut interface = Interface::new(name, Kind::Ethernet);
        interface.admin_up = admin_up;
        interface.oper = oper;
        interface.mac = Some("2c:dd:e9:12:00:a1".into());
        interface
    }

    #[test]
    fn a_port_that_is_shut_has_its_phy_powered_off() {
        assert_eq!(mac_state(&port("eth2", false, Oper::Down)), "phyOff");
        assert_eq!(mac_state(&port("eth0", true, Oper::Up)), "linkUp");
        assert_eq!(mac_state(&port("eth1", true, Oper::Down)), "linkDown");
    }

    #[test]
    fn what_the_driver_says_beats_what_the_flags_imply() {
        let mut interface = port("eth0", true, Oper::Up);
        interface.mac_layer = Some(MacLayer {
            state: "linkDown".into(),
            ..MacLayer::default()
        });
        assert_eq!(mac_state(&interface), "linkDown");
    }

    #[test]
    fn the_codeword_rows_line_up_with_each_other() {
        let mut interface = port("eth0", true, Oper::Up);
        interface.mac_layer = Some(MacLayer {
            state: "linkUp".into(),
            fec_mode: Some("Disabled".into()),
            fec_corrected: Some(0),
            fec_uncorrected: Some(0),
            ..MacLayer::default()
        });
        let snapshot = Snapshot {
            interfaces: vec![interface],
            ..Snapshot::default()
        };
        let text = detail(&snapshot);
        let corrected = text
            .lines()
            .find(|line| line.contains("FEC corrected"))
            .unwrap();
        let uncorrected = text
            .lines()
            .find(|line| line.contains("FEC uncorrected"))
            .unwrap();
        assert_eq!(corrected.rfind('0'), uncorrected.rfind('0'));
        assert_eq!(corrected.rfind('0'), Some(FEC_VALUE));
        // And not with the group above them, whose labels are shorter.
        let mode = text.lines().find(|line| line.contains("FEC mode")).unwrap();
        assert_eq!(mode.find("Disabled"), Some(VALUE));
    }

    #[test]
    fn a_port_with_no_mac_layer_reported_is_still_a_summary_row() {
        let snapshot = Snapshot {
            interfaces: vec![port("eth0", true, Oper::Up)],
            ..Snapshot::default()
        };
        assert!(summary(&snapshot).contains("eth0    2c:dd:e9:12:00:a1    linkUp"));
        assert_eq!(detail(&snapshot), "");
    }
}
