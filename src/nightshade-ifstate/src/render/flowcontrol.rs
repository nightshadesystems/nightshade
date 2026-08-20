//! `show interfaces flowcontrol`.
//!
//! Admin against oper on both directions, plus the pause frames that actually
//! crossed the wire. The counters are the point: a port configured `desired`
//! whose neighbour never agreed shows `desired on` and zero frames, and that
//! is a different fault from one that is pausing constantly.
//!
//! A port whose driver has no flow control to report is left out rather than
//! given a row of dashes -- `ethtool -a` failing and flow control being off
//! are not the same answer.

use crate::layout::{Align, Layout, name_width};
use crate::model::{Interface, Snapshot};

use super::names;

/// The value columns.
const BODY: [(usize, Align); 8] = [
    (11, Align::Left),
    (8, Align::Left),
    (11, Align::Left),
    (8, Align::Left),
    (15, Align::Left),
    (7, Align::Right),
    (3, Align::Left),
    (7, Align::Right),
];

/// The top header line, whose cells span the pairs below them.
const SPANNED: [(usize, Align); 4] = [
    (11, Align::Left),
    (19, Align::Left),
    (23, Align::Left),
    (10, Align::Left),
];

pub fn render(snapshot: &Snapshot) -> String {
    let interfaces: Vec<&Interface> = super::rows(snapshot, |interface| {
        interface.kind.is_physical() && interface.flow_control.is_some()
    });
    let extra = name_width(&names(&interfaces), BODY[0].0);

    let body = Layout::new(&BODY).widen(0, extra);
    let spanned = Layout::new(&SPANNED).widen(0, extra);

    let mut out = String::new();
    out.push_str(&spanned.row(&[
        "Port",
        "Send FlowControl",
        "Receive FlowControl",
        "RxPause",
        "TxPause",
    ]));
    out.push('\n');
    out.push_str(&body.row(&["", "admin", "oper", "admin", "oper"]));
    out.push('\n');
    out.push_str(&body.rule(&[extra.saturating_sub(2), 5, 5, 5, 5, 7, 0, 7]));
    out.push('\n');

    for interface in interfaces {
        let flow = interface.flow_control.as_ref().expect("filtered on it");
        out.push_str(&body.row(&[
            interface.name.clone(),
            flow.send_admin.clone(),
            flow.send_oper.clone(),
            flow.receive_admin.clone(),
            flow.receive_oper.clone(),
            flow.rx_pause.to_string(),
            String::new(),
            flow.tx_pause.to_string(),
        ]));
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{FlowControl, Kind};

    fn port(name: &str, admin: &str, oper: &str, rx: u64, tx: u64) -> Interface {
        let mut interface = Interface::new(name, Kind::Ethernet);
        interface.flow_control = Some(FlowControl {
            send_admin: admin.into(),
            send_oper: oper.into(),
            receive_admin: admin.into(),
            receive_oper: oper.into(),
            rx_pause: rx,
            tx_pause: tx,
        });
        interface
    }

    #[test]
    fn a_port_with_no_flow_control_to_report_is_not_a_row() {
        let snapshot = Snapshot {
            interfaces: vec![
                port("eth0", "off", "off", 0, 0),
                Interface::new("eth2", Kind::Ethernet),
            ],
            ..Snapshot::default()
        };
        let text = render(&snapshot);
        assert!(text.contains("eth0"), "{text}");
        assert!(!text.contains("eth2"), "{text}");
    }

    #[test]
    fn the_two_header_lines_and_the_rule_agree_with_the_rows() {
        let snapshot = Snapshot {
            interfaces: vec![port("eth3", "desired", "on", 12, 0)],
            ..Snapshot::default()
        };
        let text = render(&snapshot);
        let lines: Vec<&str> = text.lines().collect();
        // `admin` and `oper` sit over the values under them.
        assert_eq!(lines[1].find("admin"), lines[3].find("desired"));
        assert_eq!(lines[1].rfind("oper"), lines[3].rfind("on"));
        // The rule is under the header rather than under the name.
        assert!(lines[2].starts_with("---------  -----"), "{}", lines[2]);
    }
}
