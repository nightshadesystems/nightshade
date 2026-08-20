//! The `show interfaces counters` family.
//!
//! Six tables over the same set of rows: the physical ports. Bonds are left
//! out on purpose -- the members carry the counters, and a bond row would be
//! every packet counted twice on the same screen.
//!
//! All six are right-aligned columns of raw `u64`s with no thousands
//! separators. That is not an aesthetic choice: these numbers get subtracted
//! from each other by hand and pasted into tickets, and a separator is one
//! more thing to strip.

use crate::block::Block;
use crate::layout::{Align, Layout, name_width};
use crate::model::{BIN_LABELS, Counters, Interface, Snapshot};
use crate::units;

use super::{names, stanzas, tabulates_counters};

/// Widths for the octet/packet tables.
const TOTALS_NAME: usize = 13;
const TOTALS: [usize; 4] = [16, 16, 18, 16];

/// Widths for the error table.
const ERRORS_NAME: usize = 8;
const ERRORS: [usize; 7] = [12, 12, 12, 13, 12, 12, 12];

/// Widths for the discard table.
const DISCARDS_NAME: usize = 8;
const DISCARDS: [usize; 2] = [15, 18];

/// Widths for the rate table.
const RATES_NAME: usize = 10;
const RATES: [usize; 7] = [5, 9, 12, 7, 10, 12, 8];

/// Widths for the queue table.
const QUEUE_NAME: usize = 10;
const QUEUE_TXQ: usize = 7;
const QUEUE: [usize; 4] = [12, 21, 18, 18];

/// Label field and value column of a frame-size bin row.
const BIN_LABEL: usize = 22;
const BIN_VALUE_END: usize = 38;

fn ports(snapshot: &Snapshot) -> Vec<&Interface> {
    super::rows(snapshot, tabulates_counters)
}

fn right(name_width: usize, widths: &[usize]) -> Layout {
    let mut columns = vec![(name_width, Align::Left)];
    columns.extend(widths.iter().map(|width| (*width, Align::Right)));
    Layout::new(&columns)
}

/// `show interfaces counters` -- two stacked tables, in and out.
pub fn totals(snapshot: &Snapshot) -> String {
    let interfaces = ports(snapshot);
    let layout = right(
        name_width(&names(&interfaces), TOTALS_NAME),
        &TOTALS,
    );

    let mut inbound = String::new();
    inbound.push_str(&layout.row(&[
        "Port",
        "InOctets",
        "InUcastPkts",
        "InMcastPkts",
        "InBcastPkts",
    ]));
    inbound.push('\n');

    let mut outbound = String::new();
    outbound.push_str(&layout.row(&[
        "Port",
        "OutOctets",
        "OutUcastPkts",
        "OutMcastPkts",
        "OutBcastPkts",
    ]));
    outbound.push('\n');

    for interface in interfaces {
        let counters = interface.counters.clone().unwrap_or_default();
        inbound.push_str(&layout.row(&[
            interface.name.clone(),
            counters.in_octets.to_string(),
            counters.in_unicast.to_string(),
            counters.in_multicast.to_string(),
            counters.in_broadcast.to_string(),
        ]));
        inbound.push('\n');
        outbound.push_str(&layout.row(&[
            interface.name.clone(),
            counters.out_octets.to_string(),
            counters.out_unicast.to_string(),
            counters.out_multicast.to_string(),
            counters.out_broadcast.to_string(),
        ]));
        outbound.push('\n');
    }

    stanzas(vec![inbound, outbound])
}

/// `show interfaces counters errors`.
///
/// `RxErr` is the aggregate the kernel keeps and the others are the driver's
/// breakdown of it, so `RxErr` is at least their sum and is usually more: a
/// driver counts things there that it has no specific counter for.
pub fn errors(snapshot: &Snapshot) -> String {
    let interfaces = ports(snapshot);
    let layout = right(name_width(&names(&interfaces), ERRORS_NAME), &ERRORS);

    let mut out = String::new();
    out.push_str(&layout.row(&[
        "Port", "FCSErr", "AlignErr", "SymbolErr", "RxErr", "Runts", "Giants", "TxErr",
    ]));
    out.push('\n');

    for interface in interfaces {
        let counters = interface.counters.clone().unwrap_or_default();
        out.push_str(&layout.row(&[
            interface.name.clone(),
            optional(counters.fcs_errors),
            optional(counters.alignment_errors),
            optional(counters.symbol_errors),
            counters.in_errors.to_string(),
            optional(counters.runts),
            optional(counters.giants),
            counters.out_errors.to_string(),
        ]));
        out.push('\n');
    }
    out
}

/// `show interfaces counters discards`.
pub fn discards(snapshot: &Snapshot) -> String {
    let interfaces = ports(snapshot);
    let layout = right(name_width(&names(&interfaces), DISCARDS_NAME), &DISCARDS);

    let mut out = String::new();
    out.push_str(&layout.row(&["Port", "InDiscards", "OutDiscards"]));
    out.push('\n');
    for interface in interfaces {
        let counters = interface.counters.clone().unwrap_or_default();
        out.push_str(&layout.row(&[
            interface.name.clone(),
            counters.in_discards.to_string(),
            counters.out_discards.to_string(),
        ]));
        out.push('\n');
    }
    out
}

/// `show interfaces counters rates`.
pub fn rates(snapshot: &Snapshot) -> String {
    let interfaces = ports(snapshot);
    let layout = right(name_width(&names(&interfaces), RATES_NAME), &RATES);

    let mut out = String::new();
    out.push_str(&layout.row(&[
        "Port", "Intvl", "InMbps", "InKpps", "InPct", "OutMbps", "OutKpps", "OutPct",
    ]));
    out.push('\n');

    for interface in interfaces {
        let rates = interface.rates.unwrap_or_default();
        out.push_str(&layout.row(&[
            interface.name.clone(),
            units::interval_clock(rates.interval),
            per_million(rates.in_bps),
            per_thousand(rates.in_pps),
            units::percent(rates.in_percent),
            per_million(rates.out_bps),
            per_thousand(rates.out_pps),
            units::percent(rates.out_percent),
        ]));
        out.push('\n');
    }
    out
}

/// This table is a column of like-for-like numbers, so it fixes one decimal
/// rather than using the significant-figure rule the detail lines use -- the
/// point of a column is that the digits line up.
fn per_million(value: f64) -> String {
    format!("{:.1}", clamp(value) / 1e6)
}

fn per_thousand(value: f64) -> String {
    format!("{:.1}", clamp(value) / 1e3)
}

fn clamp(value: f64) -> f64 {
    if value.is_finite() { value.max(0.0) } else { 0.0 }
}

/// `show interfaces counters queue`.
///
/// One row per transmit queue per port. A driver that reports no per-queue
/// statistics contributes no rows rather than one row of zeroes, because
/// "this NIC has one queue with nothing in it" and "this driver does not count
/// queues" are different things to know.
pub fn queue(snapshot: &Snapshot) -> String {
    let interfaces = ports(snapshot);
    let mut columns = vec![
        (name_width(&names(&interfaces), QUEUE_NAME), Align::Left),
        (QUEUE_TXQ, Align::Left),
    ];
    columns.extend(QUEUE.iter().map(|width| (*width, Align::Right)));
    let layout = Layout::new(&columns);

    let mut out = String::new();
    out.push_str(&layout.row(&[
        "Port",
        "TxQ",
        "Counter/pkts",
        "Counter/bytes",
        "Dropped/pkts",
        "Dropped/bytes",
    ]));
    out.push('\n');

    for interface in interfaces {
        for queue in &interface.queues {
            out.push_str(&layout.row(&[
                interface.name.clone(),
                queue.name.clone(),
                queue.packets.to_string(),
                queue.bytes.to_string(),
                queue.dropped_packets.to_string(),
                queue.dropped_bytes.to_string(),
            ]));
            out.push('\n');
        }
    }
    out
}

/// `show interfaces counters bins` -- the RMON frame-size distribution.
///
/// The bins come from `ethtool -S`, whose names are the driver's own
/// (`rx_64_byte_packets` on one, `rx_size_64` on another); the collector maps
/// them. A driver with no bin for a size reports zero, because the frames did
/// go somewhere and the other bins account for them.
pub fn bins(snapshot: &Snapshot) -> String {
    let blocks: Vec<String> = ports(snapshot)
        .into_iter()
        .filter_map(|interface| {
            let bins = interface.bins.as_ref()?;
            let mut block = Block::new();
            block.heading(&interface.name);
            for (title, values) in [
                ("Received frame size distribution:", &bins.received),
                ("Transmitted frame size distribution:", &bins.transmitted),
            ] {
                block.raw(2, title);
                for (label, value) in BIN_LABELS.iter().zip(values.iter()) {
                    block.ragged(4, label, BIN_LABEL, &value.to_string(), BIN_VALUE_END);
                }
            }
            Some(block.take())
        })
        .collect();
    stanzas(blocks)
}

/// A counter the driver does not keep prints as zero here.
///
/// The aggregate `RxErr` next to it is the kernel's and is always present, so
/// a zero in a specific column under a non-zero aggregate reads correctly:
/// there were errors, and this driver cannot say they were these.
fn optional(value: Option<u64>) -> String {
    value.unwrap_or(0).to_string()
}

/// The sum of the specific error counters, for the invariant that `RxErr` is
/// at least as large.
pub fn specific_error_total(counters: &Counters) -> u64 {
    [
        counters.fcs_errors,
        counters.alignment_errors,
        counters.symbol_errors,
        counters.runts,
        counters.giants,
    ]
    .into_iter()
    .flatten()
    .sum()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Kind, Rates};

    fn port(name: &str) -> Interface {
        let mut interface = Interface::new(name, Kind::Ethernet);
        interface.counters = Some(Counters::default());
        interface
    }

    #[test]
    fn a_bond_is_not_a_row_in_a_counter_table() {
        let mut bond = Interface::new("bond0", Kind::PortChannel);
        bond.counters = Some(Counters::default());
        let snapshot = Snapshot {
            interfaces: vec![port("eth0"), bond],
            ..Snapshot::default()
        };
        let text = totals(&snapshot);
        assert!(text.contains("eth0"), "{text}");
        assert!(!text.contains("bond0"), "{text}");
    }

    #[test]
    fn the_rate_table_fixes_one_decimal_so_the_digits_line_up() {
        let mut interface = port("eth0");
        interface.rates = Some(Rates {
            interval: 300,
            in_bps: 24_700_000.0,
            in_pps: 4_123.0,
            in_percent: 0.2536,
            out_bps: 96_300_000.0,
            out_pps: 9_877.0,
            out_percent: 0.979,
        });
        let snapshot = Snapshot {
            interfaces: vec![interface],
            ..Snapshot::default()
        };
        let row = rates(&snapshot).lines().nth(1).unwrap().to_string();
        assert!(row.contains("24.7"), "{row}");
        assert!(row.contains("4.1"), "{row}");
        assert!(row.contains("0.3%"), "{row}");
        assert!(row.contains("0:05"), "{row}");
    }

    #[test]
    fn a_port_with_no_queue_statistics_contributes_no_rows() {
        let snapshot = Snapshot {
            interfaces: vec![port("eth0")],
            ..Snapshot::default()
        };
        assert_eq!(queue(&snapshot).lines().count(), 1);
    }

    #[test]
    fn the_aggregate_error_counter_is_never_smaller_than_its_parts() {
        let counters = Counters {
            fcs_errors: Some(12),
            symbol_errors: Some(3),
            in_errors: 15,
            ..Counters::default()
        };
        assert_eq!(specific_error_total(&counters), 15);
        assert!(counters.in_errors >= specific_error_total(&counters));
    }
}
