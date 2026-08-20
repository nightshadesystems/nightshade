//! `show interfaces description`.
//!
//! Every interface the box has, of every kind, in one natural-sorted list.
//! Deliberately not grouped by type: the question this answers is "which one
//! is the uplink", and an operator looking for a name should not first have to
//! know what sort of thing it is.

use crate::layout::{Align, Layout, name_width};
use crate::model::Snapshot;

use super::names;

/// EOS's widths. The first grows for longer Linux names; the rest do not move
/// relative to each other.
const INTERFACE: usize = 31;
const STATUS: usize = 15;
const PROTOCOL: usize = 19;

pub fn render(snapshot: &Snapshot) -> String {
    let interfaces = super::rows(snapshot, |_| true);
    let layout = Layout::new(&[
        (INTERFACE, Align::Left),
        (STATUS, Align::Left),
        (PROTOCOL, Align::Left),
    ])
    .widen(0, name_width(&names(&interfaces), INTERFACE));

    let mut out = String::new();
    out.push_str(&layout.row(&["Interface", "Status", "Protocol", "Description"]));
    out.push('\n');

    for interface in interfaces {
        // Admin state and operational state are different questions and this
        // table asks both: a port that is up and unplugged reads `up down`,
        // which is the row an operator is looking for.
        let status = if interface.admin_up { "up" } else { "admin down" };
        out.push_str(&layout.row(&[
            &interface.name,
            status,
            interface.oper.label(),
            interface.description.as_deref().unwrap_or(""),
        ]));
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Interface, Kind, Oper};

    #[test]
    fn a_row_with_no_description_ends_after_the_protocol() {
        let mut eth2 = Interface::new("eth2", Kind::Ethernet);
        eth2.oper = Oper::Down;
        let snapshot = Snapshot {
            interfaces: vec![eth2],
            ..Snapshot::default()
        };
        let text = render(&snapshot);
        let row = text.lines().nth(1).unwrap();
        assert_eq!(row, "eth2                           admin down     down");
        assert!(!row.ends_with(' '));
    }

    #[test]
    fn a_long_name_moves_every_column_right_together() {
        let mut long = Interface::new("enp3s0f1np1-something-long-indeed", Kind::Ethernet);
        long.admin_up = true;
        long.oper = Oper::Up;
        long.description = Some("uplink".into());
        let snapshot = Snapshot {
            interfaces: vec![long],
            ..Snapshot::default()
        };
        let text = render(&snapshot);
        let header = text.lines().next().unwrap();
        let row = text.lines().nth(1).unwrap();
        assert_eq!(header.find("Status"), row.find("up"));
        assert_eq!(header.find("Description"), row.find("uplink"));
    }
}
