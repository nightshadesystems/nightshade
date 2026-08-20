//! `show interfaces phy` and `show interfaces phy detail`.
//!
//! The two-column layout is fixed: labels from their indent, values from
//! column [`VALUE`]. Everything in it is driver-dependent, so a row the driver
//! cannot answer is left out rather than printed empty -- a PHY reset count of
//! nothing and a PHY reset count of zero are different, and only one of them
//! means the port has not been bounced.

use crate::block::Block;
use crate::model::{Interface, Phy, Snapshot};
use crate::units;

use super::stanzas;

/// Absolute column every value starts at, including the nested rows.
const VALUE: usize = 45;
const SECTION: usize = 2;
const ROW: usize = 4;
const NESTED: usize = 6;

/// `detail` adds the system clock at the top.
///
/// It is there so that the `... ago` durations below have something to be
/// relative to: a support bundle read three days later otherwise says a port
/// flapped "four minutes ago" with no way to know four minutes before what.
pub fn render(snapshot: &Snapshot, detail: bool) -> String {
    let blocks: Vec<String> = super::physical(snapshot)
        .into_iter()
        .filter_map(one)
        .collect();

    let body = stanzas(blocks);
    // A daemon that could not format the time sends an empty string rather
    // than failing the whole command; a header with nothing after it is a
    // trailing space in a support bundle, so it is dropped instead.
    let clock = snapshot
        .system
        .time
        .as_deref()
        .filter(|time| !time.trim().is_empty());
    match (detail, clock) {
        (true, Some(time)) => format!("Current System Time: {time}\n{body}"),
        _ => body,
    }
}

fn one(interface: &Interface) -> Option<String> {
    let phy = interface.phy.as_ref()?;
    let mut block = Block::new();
    block.heading(&interface.name);

    block.raw(SECTION, "Current State");
    block.maybe_aligned(ROW, "PHY state", phy.state.clone(), VALUE);
    block.maybe_aligned(ROW, "Interface state", phy.interface_state.clone(), VALUE);
    block.maybe_aligned(ROW, "HW resets", number(phy.hw_resets), VALUE);
    block.maybe_aligned(ROW, "Transceiver", phy.transceiver.clone(), VALUE);
    block.maybe_aligned(
        ROW,
        "Transceiver SN",
        phy.transceiver_serial.clone(),
        VALUE,
    );
    block.maybe_aligned(ROW, "Oper speed", phy.oper_speed.clone(), VALUE);
    block.maybe_aligned(ROW, "Interrupt count", number(phy.interrupt_count), VALUE);
    block.maybe_aligned(ROW, "Diags mode", phy.diags_mode.clone(), VALUE);
    block.maybe_aligned(ROW, "Model", phy.model.clone(), VALUE);
    block.maybe_aligned(ROW, "Reset count", number(phy.reset_count), VALUE);
    block.maybe_aligned(ROW, "PHY state changes", number(phy.state_changes), VALUE);
    if let Some(last) = phy.last_change {
        // Nested under the change count, and its value still lines up with
        // every other value in the section rather than with its own indent.
        block.aligned(NESTED, "Last change", &units::ago(Some(last)), VALUE);
    }

    if has_speed_configuration(phy) {
        block.raw(SECTION, "Speed Configuration");
        block.maybe_aligned(ROW, "Configured speed", phy.configured_speed.clone(), VALUE);
        block.maybe_aligned(ROW, "Auto-negotiation", phy.autoneg.clone(), VALUE);
    }

    Some(block.take())
}

fn has_speed_configuration(phy: &Phy) -> bool {
    phy.configured_speed.is_some() || phy.autoneg.is_some()
}

fn number(value: Option<u64>) -> Option<String> {
    value.map(|value| value.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Kind, System};

    fn with_phy(phy: Phy) -> Snapshot {
        let mut interface = Interface::new("eth0", Kind::Ethernet);
        interface.phy = Some(phy);
        Snapshot {
            interfaces: vec![interface],
            system: System {
                time: Some("Thu Aug 20 14:02:11 2026".into()),
                ..System::default()
            },
        }
    }

    #[test]
    fn every_value_starts_at_the_same_column_however_deep_its_label_is() {
        let text = render(
            &with_phy(Phy {
                state: Some("linkUp".into()),
                state_changes: Some(2),
                last_change: Some(4),
                ..Phy::default()
            }),
            true,
        );
        for line in text.lines().filter(|line| line.starts_with("    ") || line.starts_with("      ")) {
            let value = line.len() - line.trim_start_matches(' ').len();
            assert!(value >= 4, "{line}");
            let start = line.rfind("  ").map(|at| at + 2);
            assert_eq!(start, Some(VALUE), "{line}");
        }
    }

    #[test]
    fn a_driver_that_says_nothing_produces_no_rows_rather_than_empty_ones() {
        let text = render(&with_phy(Phy::default()), false);
        assert_eq!(text, "eth0\n  Current State\n");
    }

    #[test]
    fn the_system_clock_belongs_to_the_detail_form_only() {
        let snapshot = with_phy(Phy {
            state: Some("linkUp".into()),
            ..Phy::default()
        });
        assert!(render(&snapshot, true).starts_with("Current System Time: "));
        assert!(render(&snapshot, false).starts_with("eth0\n"));
    }

    #[test]
    fn a_port_with_no_phy_is_not_a_block() {
        let snapshot = Snapshot {
            interfaces: vec![Interface::new("eth0", Kind::Ethernet)],
            ..Snapshot::default()
        };
        assert_eq!(render(&snapshot, true), "");
    }
}
