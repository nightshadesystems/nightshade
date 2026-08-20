//! The `show interfaces transceiver` family.
//!
//! Copper ports have no module and are left out of every one of these rather
//! than filling four columns with `N/A`. A field the module does not report is
//! `N/A`; a module that is not there is not a row.
//!
//! # A note on the header alignment
//!
//! The value columns of the summary table sit one character to the right of
//! the rule above them, and the two power columns are the ones it shows on.
//! That is EOS's, reproduced deliberately: an operator diffing this output
//! against a real EOS box should get an empty diff, and "we straightened it"
//! is a change to the thing being compared.

use crate::block::Block;
use crate::layout::{Align, Layout, name_width};
use crate::model::{Interface, Measure, Snapshot, Transceiver};
use crate::units;

use super::{has_module, names, stanzas};

/// The advisory EOS prints above the table, verbatim.
const PREAMBLE: &str = "If system temperature is too high, transceiver temperature will rise 5 C\n\
                        per 1 C rise in system temperature\n";

/// Header and rule columns.
const HEAD: [(usize, Align); 6] = [
    (11, Align::Left),
    (10, Align::Left),
    (13, Align::Left),
    (18, Align::Left),
    (11, Align::Left),
    (10, Align::Left),
];

/// Value columns. Deliberately not the same as [`HEAD`]; see the module note.
const BODY: [(usize, Align); 7] = [
    (10, Align::Left),
    (10, Align::Right),
    (13, Align::Right),
    (18, Align::Right),
    (11, Align::Right),
    (11, Align::Right),
    (1, Align::Left),
];

/// Label field, value field and unit field of a threshold line.
const THRESHOLD_LABEL: usize = 22;
const THRESHOLD_VALUE: usize = 6;
const THRESHOLD_UNIT: usize = 4;
/// The second half's label is one narrower than the first's. EOS's, preserved.
const THRESHOLD_LABEL_2: usize = 21;

fn modules(snapshot: &Snapshot) -> Vec<&Interface> {
    super::rows(snapshot, |interface| {
        interface.kind.is_physical() && has_module(interface)
    })
}

/// `show interfaces transceiver` -- the diagnostic monitoring summary.
pub fn summary(snapshot: &Snapshot) -> String {
    let interfaces = modules(snapshot);
    let extra = name_width(&names(&interfaces), HEAD[0].0);

    let head = Layout::new(&HEAD).widen(0, extra);
    let body = Layout::new(&BODY).widen(0, extra.saturating_sub(1));

    let mut out = String::from(PREAMBLE);
    out.push_str(&head.row(&["", "", "", "", "Rx Power", "Tx Power"]));
    out.push('\n');
    out.push_str(&head.row(&[
        "Port",
        "Temp (C)",
        "Voltage (V)",
        "Bias (mA)",
        "(dBm)",
        "(dBm)",
        "Last Update",
    ]));
    out.push('\n');
    out.push_str(&head.rule(&[extra.saturating_sub(1), 9, 12, 17, 10, 9, 19]));
    out.push('\n');

    for interface in interfaces {
        let module = interface.transceiver.as_ref().expect("filtered on it");
        out.push_str(&body.row(&[
            interface.name.clone(),
            measured(module.temperature.value),
            measured(module.voltage.value),
            measured(module.tx_bias.value),
            measured(module.rx_power.value),
            measured(module.tx_power.value),
            String::new(),
            match module.age {
                Some(age) => units::ago(Some(age)),
                None => "N/A".to_string(),
            },
        ]));
        out.push('\n');
    }
    out
}

/// Two decimals, or `N/A` for a value the module does not report.
fn measured(value: Option<f64>) -> String {
    match value {
        Some(value) => format!("{value:.2}"),
        None => "N/A".to_string(),
    }
}

/// `show interfaces transceiver detail` -- identity and thresholds.
pub fn detail(snapshot: &Snapshot) -> String {
    let blocks: Vec<String> = modules(snapshot)
        .into_iter()
        .map(|interface| {
            let module = interface.transceiver.as_ref().expect("filtered on it");
            let mut block = Block::new();
            block.heading(&interface.name);
            block.maybe(2, "Transceiver Type", module.media_type.as_deref());
            block.maybe(2, "Vendor Name", module.vendor.as_deref());
            block.maybe(2, "Vendor Part Number", module.part_number.as_deref());
            block.maybe(2, "Vendor Serial Number", module.serial_number.as_deref());
            block.maybe(2, "Vendor Date Code", module.date_code.as_deref());
            measure(&mut block, "Temperature", &module.temperature, "C");
            measure(&mut block, "Voltage", &module.voltage, "V");
            measure(&mut block, "Tx Bias", &module.tx_bias, "mA");
            measure(&mut block, "Tx Power", &module.tx_power, "dBm");
            measure(&mut block, "Rx Power", &module.rx_power, "dBm");
            block.take()
        })
        .collect();
    stanzas(blocks)
}

/// One measured value and its four thresholds.
///
/// The thresholds are omitted rather than printed as `N/A`: a module that does
/// not carry an alarm threshold is not a module that will alarm, and a line of
/// `N/A`s reads as one that would.
fn measure(block: &mut Block, label: &str, measure: &Measure, unit: &str) {
    let Some(value) = measure.value else {
        return;
    };
    block.field(2, label, &format!("{value:.2} {unit}"));
    threshold(
        block,
        "High alarm threshold:",
        measure.high_alarm,
        "High warn threshold:",
        measure.high_warn,
        unit,
    );
    threshold(
        block,
        "Low alarm threshold:",
        measure.low_alarm,
        "Low warn threshold:",
        measure.low_warn,
        unit,
    );
}

fn threshold(
    block: &mut Block,
    alarm_label: &str,
    alarm: Option<f64>,
    warn_label: &str,
    warn: Option<f64>,
    unit: &str,
) {
    let (Some(alarm), Some(warn)) = (alarm, warn) else {
        return;
    };
    let mut line = String::from("    ");
    pad(&mut line, alarm_label, THRESHOLD_LABEL);
    right(&mut line, &format!("{alarm:.2}"), THRESHOLD_VALUE);
    line.push(' ');
    pad(&mut line, unit, THRESHOLD_UNIT);
    pad(&mut line, warn_label, THRESHOLD_LABEL_2);
    right(&mut line, &format!("{warn:.2}"), THRESHOLD_VALUE);
    line.push(' ');
    line.push_str(unit);
    block.raw(0, &line);
}

fn pad(out: &mut String, text: &str, width: usize) {
    out.push_str(text);
    out.extend(std::iter::repeat_n(
        ' ',
        width.saturating_sub(text.chars().count()),
    ));
}

fn right(out: &mut String, text: &str, width: usize) {
    out.extend(std::iter::repeat_n(
        ' ',
        width.saturating_sub(text.chars().count()),
    ));
    out.push_str(text);
}

/// `show interfaces transceiver properties`.
///
/// Administrative against operational, which is the pair that answers "why is
/// this port at a gigabit when I asked for ten".
pub fn properties(snapshot: &Snapshot) -> String {
    let blocks: Vec<String> = modules(snapshot)
        .into_iter()
        .map(|interface| {
            let mut block = Block::new();
            // The space before the colon is EOS's, on this line only.
            block.raw(0, &format!("Name : {}", interface.name));
            block.field(0, "Administrative Speed", &speed(interface.admin_speed_mbps));
            block.field(0, "Administrative Duplex", &duplex(interface.admin_duplex));
            block.field(0, "Operational Speed", &speed(interface.speed_mbps));
            block.field(0, "Operational Duplex", &duplex(interface.duplex));
            block.field(
                0,
                "Media Type",
                interface.media_type.as_deref().unwrap_or("N/A"),
            );
            block.take()
        })
        .collect();
    stanzas(blocks)
}

fn speed(mbps: Option<u64>) -> String {
    mbps.map(units::speed_short)
        .unwrap_or_else(|| "auto".to_string())
}

fn duplex(duplex: Option<crate::model::Duplex>) -> String {
    duplex
        .map(|duplex| duplex.short().to_string())
        .unwrap_or_else(|| "auto".to_string())
}

/// `show interfaces transceiver eeprom` -- the raw SFF pages.
///
/// Sixteen bytes a line with a gap in the middle, which is where a person
/// counting to the offset in an SFF-8472 table stops and starts again.
pub fn eeprom(snapshot: &Snapshot) -> String {
    let blocks: Vec<String> = modules(snapshot)
        .into_iter()
        .filter_map(|interface| {
            let module = interface.transceiver.as_ref().expect("filtered on it");
            if module.pages.is_empty() {
                return None;
            }
            let mut block = Block::new();
            block.raw(0, &format!("{}:", interface.name));
            for page in &module.pages {
                block.raw(2, &format!("{} page:", page.name));
                for (index, chunk) in page.bytes.chunks(16).enumerate() {
                    block.raw(4, &hex_line(index * 16, chunk));
                }
            }
            Some(block.take())
        })
        .collect();
    stanzas(blocks)
}

fn hex_line(offset: usize, bytes: &[u8]) -> String {
    let mut line = format!("{offset:04x}:");
    for (index, byte) in bytes.iter().enumerate() {
        // The wider gap at the halfway mark, so an eye can find byte 8
        // without counting from the start of the line.
        line.push(' ');
        if index == 8 {
            line.push(' ');
        }
        line.push_str(&format!("{byte:02x}"));
    }
    line
}

/// Whether a module carries diagnostics, which is what decides if there is an
/// `A2` page to dump at all.
pub fn has_diagnostics(module: &Transceiver) -> bool {
    module.pages.iter().any(|page| page.name == "A2")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{EepromPage, Kind};

    fn optic(name: &str) -> Interface {
        let mut interface = Interface::new(name, Kind::Ethernet);
        interface.transceiver = Some(Transceiver::default());
        interface
    }

    #[test]
    fn a_copper_port_is_not_a_row() {
        let snapshot = Snapshot {
            interfaces: vec![optic("eth0"), Interface::new("eth1", Kind::Ethernet)],
            ..Snapshot::default()
        };
        let text = summary(&snapshot);
        assert!(text.contains("eth0"), "{text}");
        assert!(!text.contains("eth1"), "{text}");
    }

    #[test]
    fn a_field_the_module_does_not_report_is_not_a_zero() {
        let snapshot = Snapshot {
            interfaces: vec![optic("eth0")],
            ..Snapshot::default()
        };
        let row = summary(&snapshot).lines().last().unwrap().to_string();
        assert!(row.contains("N/A"), "{row}");
        assert!(!row.contains("0.00"), "{row}");
    }

    #[test]
    fn a_hex_line_has_its_gap_in_the_middle() {
        let bytes: Vec<u8> = (0..16).collect();
        assert_eq!(
            hex_line(0, &bytes),
            "0000: 00 01 02 03 04 05 06 07  08 09 0a 0b 0c 0d 0e 0f"
        );
        assert_eq!(hex_line(0x10, &bytes[..3]), "0010: 00 01 02");
    }

    #[test]
    fn a_module_with_no_pages_read_is_not_dumped() {
        let snapshot = Snapshot {
            interfaces: vec![optic("eth0")],
            ..Snapshot::default()
        };
        assert_eq!(eeprom(&snapshot), "");
    }

    #[test]
    fn diagnostics_are_the_second_page() {
        let mut module = Transceiver::default();
        assert!(!has_diagnostics(&module));
        module.pages.push(EepromPage {
            name: "A0".into(),
            bytes: vec![0; 256],
        });
        assert!(!has_diagnostics(&module));
        module.pages.push(EepromPage {
            name: "A2".into(),
            bytes: vec![0; 256],
        });
        assert!(has_diagnostics(&module));
    }
}
