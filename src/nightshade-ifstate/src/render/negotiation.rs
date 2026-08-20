//! `show interfaces negotiation` and its detail form.
//!
//! Both sides of the autonegotiation, which is the only way to tell a port
//! that is not advertising a gigabit from one whose neighbour is not. The
//! advertisement is a list that does not fit a column, so it wraps under
//! itself rather than being truncated -- the entry that got cut would be the
//! one that mattered.

use crate::block::Block;
use crate::layout::{Align, Layout, name_width, wrap};
use crate::model::{Advertisement, Interface, Negotiation, Snapshot};

use super::{names, stanzas};

const BODY: [(usize, Align); 4] = [
    (8, Align::Left),
    (13, Align::Left),
    (33, Align::Left),
    (20, Align::Left),
];

/// The top header line, whose two cells span the columns below them.
const SPANNED: [(usize, Align); 2] = [(8, Align::Left), (46, Align::Left)];

/// Nothing to report, in the one spelling used everywhere in this table.
const NONE: &str = "n/a";

pub fn summary(snapshot: &Snapshot) -> String {
    let interfaces = super::physical(snapshot);
    let extra = name_width(&names(&interfaces), BODY[0].0);

    let body = Layout::new(&BODY).widen(0, extra);
    let spanned = Layout::new(&SPANNED).widen(0, extra);
    let advertisement_width = body.width(3);

    let mut out = String::new();
    out.push_str(&spanned.row(&["Port", "Auto-Negotiation", "Local Advertisement"]));
    out.push('\n');
    out.push_str(&body.row(&["", "Mode", "Status", "Speed/Duplex", "Pause"]));
    out.push('\n');

    for interface in interfaces {
        let negotiation = interface.negotiation.as_ref();
        let mode = negotiation.map(|n| n.mode.as_str()).unwrap_or("off");
        let status = negotiation.map(|n| n.status.as_str()).unwrap_or(NONE);
        let advertised = negotiation
            .map(|n| n.local.speed_duplex.join(" "))
            .filter(|list| !list.is_empty())
            .unwrap_or_else(|| NONE.to_string());
        let pause = negotiation
            .and_then(|n| n.local.pause.clone())
            .unwrap_or_else(|| NONE.to_string());

        let mut lines = wrap(&advertised, advertisement_width).into_iter();
        let first = lines.next().unwrap_or_default();
        out.push_str(&body.row(&[
            interface.name.clone(),
            mode.to_string(),
            status.to_string(),
            first,
            pause,
        ]));
        out.push('\n');
        // Continuation lines carry nothing but the rest of the list, indented
        // to sit under the column it belongs to.
        for line in lines {
            out.push_str(&body.row(&["", "", "", &line]));
            out.push('\n');
        }
    }
    out
}

pub fn detail(snapshot: &Snapshot) -> String {
    let blocks: Vec<String> = super::rows(snapshot, |interface: &Interface| {
        interface.kind.is_physical() && interface.negotiation.is_some()
    })
    .into_iter()
    .map(|interface| {
        let negotiation = interface.negotiation.as_ref().expect("filtered on it");
        let mut block = Block::new();
        block.heading(&interface.name);
        block.field(2, "Auto-Negotiation Mode", &long_mode(negotiation));
        block.field(2, "Auto-Negotiation Status", &capitalise(&negotiation.status));
        advertisement(&mut block, "Local Advertisement", &negotiation.local);
        if let Some(partner) = &negotiation.partner {
            advertisement(&mut block, "Link Partner Advertisement", partner);
        }
        if let Some(resolution) = &negotiation.resolution {
            block.raw(2, "Resolution");
            if !resolution.speed_duplex.is_empty() {
                block.field(4, "Speed/Duplex", &resolution.speed_duplex.join(" "));
            }
            if let Some(pause) = negotiation
                .resolved_pause
                .as_deref()
                .or(resolution.pause.as_deref())
            {
                block.field(4, "Pause", pause);
            }
        }
        block.take()
    })
    .collect();
    stanzas(blocks)
}

fn advertisement(block: &mut Block, title: &str, advertisement: &Advertisement) {
    block.raw(2, title);
    if !advertisement.speed_duplex.is_empty() {
        block.field(4, "Speed/Duplex", &advertisement.speed_duplex.join(" "));
    }
    block.maybe(4, "Pause", advertisement.pause.as_deref());
}

/// `802.3` in the table, `IEEE 802.3` in the detail. The standard's name is
/// worth the width once.
fn long_mode(negotiation: &Negotiation) -> String {
    if negotiation.mode == "802.3" {
        "IEEE 802.3".to_string()
    } else {
        capitalise(&negotiation.mode)
    }
}

fn capitalise(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Kind;

    #[test]
    fn a_port_that_does_not_negotiate_is_still_a_row() {
        let snapshot = Snapshot {
            interfaces: vec![Interface::new("eth0", Kind::Ethernet)],
            ..Snapshot::default()
        };
        let text = summary(&snapshot);
        let row = text.lines().nth(2).unwrap();
        assert!(row.starts_with("eth0    off          n/a"), "{row}");
        assert_eq!(row.matches("n/a").count(), 3, "{row}");
    }

    #[test]
    fn a_long_advertisement_wraps_under_its_own_column() {
        let mut interface = Interface::new("eth1", Kind::Ethernet);
        interface.negotiation = Some(Negotiation {
            mode: "802.3".into(),
            status: "success".into(),
            local: Advertisement {
                speed_duplex: vec![
                    "10M/half".into(),
                    "10M/full".into(),
                    "100M/half".into(),
                    "100M/full".into(),
                    "1G/full".into(),
                ],
                pause: Some("None".into()),
            },
            ..Negotiation::default()
        });
        let snapshot = Snapshot {
            interfaces: vec![interface],
            ..Snapshot::default()
        };
        let text = summary(&snapshot);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 5);
        assert_eq!(lines[2].find("10M/half"), lines[3].find("100M/half"));
        assert_eq!(lines[3].find("100M/half"), lines[4].find("1G/full"));
        // The continuation lines carry nothing else.
        assert_eq!(lines[4].trim(), "1G/full");
    }

    #[test]
    fn the_detail_form_spells_the_standard_out() {
        let mut interface = Interface::new("eth1", Kind::Ethernet);
        interface.negotiation = Some(Negotiation {
            mode: "802.3".into(),
            status: "success".into(),
            ..Negotiation::default()
        });
        let snapshot = Snapshot {
            interfaces: vec![interface],
            ..Snapshot::default()
        };
        let text = detail(&snapshot);
        assert!(text.contains("Auto-Negotiation Mode: IEEE 802.3\n"), "{text}");
        assert!(text.contains("Auto-Negotiation Status: Success\n"), "{text}");
    }
}
