//! `ethtool -S`, and the fact that no two drivers name anything the same way.
//!
//! The kernel's own counters (`rtnl_link_stats64`) are the same everywhere and
//! are the ones the totals come from. Everything below them -- CRC errors,
//! runts, pause frames, per-queue counters, the RMON frame-size bins -- exists
//! only in the driver's private statistics, whose names are the driver
//! author's and are not an ABI.
//!
//! So this is an alias table. `rx_crc_errors` on `ixgbe`, `rx_fcs_errors` on
//! `bnxt_en`, `rx_crc_errors_phy` on `mlx5_core`, `port.crc_errors` on `i40e`
//! -- one counter, four spellings. The first alias that is present wins, and a
//! counter with no alias present stays absent rather than becoming a zero,
//! because "this driver does not count CRC errors" and "there have been no CRC
//! errors" are answers an operator must be able to tell apart.
//!
//! Adding a driver is adding names to a list here. Nothing else changes.

use std::collections::BTreeMap;

use nightshade_ifstate::model::{Bins, Queue};

/// The driver statistics for one interface, by the driver's own names.
pub type Statistics = BTreeMap<String, u64>;

/// The first of `aliases` that the driver reports.
pub fn lookup(statistics: &Statistics, aliases: &[&str]) -> Option<u64> {
    aliases
        .iter()
        .find_map(|alias| statistics.get(*alias).copied())
}

/// The sum of every alias present, for counters a driver splits per port or
/// per lane.
fn sum(statistics: &Statistics, aliases: &[&str]) -> Option<u64> {
    let mut total = None;
    for alias in aliases {
        if let Some(value) = statistics.get(*alias) {
            total = Some(total.unwrap_or(0u64).saturating_add(*value));
        }
    }
    total
}

pub const FCS_ERRORS: &[&str] = &[
    "rx_crc_errors",
    "rx_fcs_errors",
    "rx_crc_errors_phy",
    "port.crc_errors",
    "rx_frame_check_sequence_errors",
];

pub const ALIGNMENT_ERRORS: &[&str] = &[
    "rx_align_errors",
    "rx_alignment_errors",
    "alignment_errors",
    "port.rx_alignment_errors",
];

pub const SYMBOL_ERRORS: &[&str] = &[
    "rx_symbol_errors",
    "rx_symbol_error",
    "rx_symbol_err_phy",
    "symbol_errors",
    "port.rx_symbol_errors",
];

pub const RUNTS: &[&str] = &[
    "rx_undersize_errors",
    "rx_undersize_packets",
    "rx_undersize_pkts",
    "rx_runt_errors",
    "rx_short_length_errors",
    "port.rx_undersize",
];

pub const GIANTS: &[&str] = &[
    "rx_oversize_errors",
    "rx_oversize_packets",
    "rx_oversize_pkts",
    "rx_jabber_errors",
    "rx_too_long_errors",
    "port.rx_oversize",
];

pub const COLLISIONS: &[&str] = &["collisions", "tx_collisions", "tx_total_collisions"];

pub const LATE_COLLISIONS: &[&str] = &["tx_late_collisions", "late_collisions"];

pub const DEFERRED: &[&str] = &["tx_deferred", "tx_deferred_ok", "deferred_transmissions"];

pub const PAUSE_IN: &[&str] = &[
    "rx_pause_frames",
    "rx_pause",
    "rx_pause_ctrl_phy",
    "port.rx_pause",
    "rx_flow_control_xon",
];

pub const PAUSE_OUT: &[&str] = &[
    "tx_pause_frames",
    "tx_pause",
    "tx_pause_ctrl_phy",
    "port.tx_pause",
    "tx_flow_control_xon",
];

pub const RX_BROADCAST: &[&str] = &[
    "rx_broadcast",
    "rx_broadcast_packets",
    "rx_bcast_packets",
    "rx_broadcast_phy",
    "port.rx_broadcast",
];

pub const RX_MULTICAST: &[&str] = &[
    "rx_multicast",
    "rx_multicast_packets",
    "rx_mcast_packets",
    "rx_multicast_phy",
    "port.rx_multicast",
];

pub const TX_BROADCAST: &[&str] = &[
    "tx_broadcast",
    "tx_broadcast_packets",
    "tx_bcast_packets",
    "tx_broadcast_phy",
    "port.tx_broadcast",
];

pub const TX_MULTICAST: &[&str] = &[
    "tx_multicast",
    "tx_multicast_packets",
    "tx_mcast_packets",
    "tx_multicast_phy",
    "port.tx_multicast",
];

pub const FEC_CORRECTED: &[&str] = &[
    "fec_corrected_blocks",
    "rx_corrected_bits_phy",
    "fec_corrected_symbols_total",
    "port.fec_corrected_blocks",
];

pub const FEC_UNCORRECTED: &[&str] = &[
    "fec_uncorrectable_blocks",
    "rx_err_lane_0_phy",
    "fec_uncorrected_symbols_total",
    "port.fec_uncorrectable_blocks",
];

pub const LOCAL_FAULT: &[&str] = &["rx_local_fault", "link_down_events_phy", "local_fault"];
pub const REMOTE_FAULT: &[&str] = &["rx_remote_fault", "remote_fault"];

/// The seven frame-size bins, in the order they are printed, with the aliases
/// each driver family uses for them.
///
/// The last bin is "everything above a tagged maximum frame", which drivers
/// name for its lower bound (`1523`), for the jumbo range (`big`,
/// `jumbo`), or not at all.
const RX_BINS: [&[&str]; 7] = [
    &["rx_size_64", "rx_64_byte_packets", "rx_frames_64", "rx_64b_frames"],
    &[
        "rx_size_127",
        "rx_65_to_127_byte_packets",
        "rx_frames_65_127",
        "rx_65b_127b_frames",
    ],
    &[
        "rx_size_255",
        "rx_128_to_255_byte_packets",
        "rx_frames_128_255",
        "rx_128b_255b_frames",
    ],
    &[
        "rx_size_511",
        "rx_256_to_511_byte_packets",
        "rx_frames_256_511",
        "rx_256b_511b_frames",
    ],
    &[
        "rx_size_1023",
        "rx_512_to_1023_byte_packets",
        "rx_frames_512_1023",
        "rx_512b_1023b_frames",
    ],
    &[
        "rx_size_1522",
        "rx_1024_to_1518_byte_packets",
        "rx_frames_1024_1518",
        "rx_1024b_1518b_frames",
    ],
    &[
        "rx_size_big",
        "rx_1519_to_max_byte_packets",
        "rx_frames_1519_plus",
        "rx_jumbo_frames",
    ],
];

const TX_BINS: [&[&str]; 7] = [
    &["tx_size_64", "tx_64_byte_packets", "tx_frames_64", "tx_64b_frames"],
    &[
        "tx_size_127",
        "tx_65_to_127_byte_packets",
        "tx_frames_65_127",
        "tx_65b_127b_frames",
    ],
    &[
        "tx_size_255",
        "tx_128_to_255_byte_packets",
        "tx_frames_128_255",
        "tx_128b_255b_frames",
    ],
    &[
        "tx_size_511",
        "tx_256_to_511_byte_packets",
        "tx_frames_256_511",
        "tx_256b_511b_frames",
    ],
    &[
        "tx_size_1023",
        "tx_512_to_1023_byte_packets",
        "tx_frames_512_1023",
        "tx_512b_1023b_frames",
    ],
    &[
        "tx_size_1522",
        "tx_1024_to_1518_byte_packets",
        "tx_frames_1024_1518",
        "tx_1024b_1518b_frames",
    ],
    &[
        "tx_size_big",
        "tx_1519_to_max_byte_packets",
        "tx_frames_1519_plus",
        "tx_jumbo_frames",
    ],
];

/// The frame-size distribution, or `None` if the driver keeps none of it.
///
/// A driver that keeps some bins and not others reports zero for the ones it
/// lacks: the frames went somewhere, and the bins that are present account for
/// them. That is different from a driver with no bins at all, which gets no
/// section rather than seven zeroes.
pub fn bins(statistics: &Statistics) -> Option<Bins> {
    let mut received = [0u64; 7];
    let mut transmitted = [0u64; 7];
    let mut any = false;

    for (index, aliases) in RX_BINS.iter().enumerate() {
        if let Some(value) = lookup(statistics, aliases) {
            received[index] = value;
            any = true;
        }
    }
    for (index, aliases) in TX_BINS.iter().enumerate() {
        if let Some(value) = lookup(statistics, aliases) {
            transmitted[index] = value;
            any = true;
        }
    }

    any.then_some(Bins {
        received,
        transmitted,
    })
}

/// The per-queue transmit counters, as `UC0..UC(N-1)`.
///
/// Every driver names these differently and all of them embed the queue number
/// in the name: `tx_queue_0_packets` (Intel), `tx0_packets` (mlx5),
/// `tx-0.packets` (virtio, bnxt). Rather than a list of formats, this pulls
/// the number out of any name that has one and matches a known suffix, which
/// means a driver nobody has seen yet works if it follows any of the three
/// conventions.
pub fn queues(statistics: &Statistics) -> Vec<Queue> {
    let mut found: BTreeMap<u32, Queue> = BTreeMap::new();

    for (name, value) in statistics {
        let Some((index, field)) = transmit_queue(name) else {
            continue;
        };
        let queue = found.entry(index).or_insert_with(|| Queue {
            name: format!("UC{index}"),
            ..Queue::default()
        });
        match field {
            Field::Packets => queue.packets = *value,
            Field::Bytes => queue.bytes = *value,
            Field::DroppedPackets => queue.dropped_packets = *value,
            Field::DroppedBytes => queue.dropped_bytes = *value,
        }
    }

    found.into_values().collect()
}

enum Field {
    Packets,
    Bytes,
    DroppedPackets,
    DroppedBytes,
}

/// `("tx_queue_3_bytes")` becomes `(3, Bytes)`.
fn transmit_queue(name: &str) -> Option<(u32, Field)> {
    let rest = name
        .strip_prefix("tx_queue_")
        .or_else(|| name.strip_prefix("tx_"))
        .or_else(|| name.strip_prefix("tx-"))
        .or_else(|| name.strip_prefix("tx"))?;

    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        return None;
    }
    let index = digits.parse().ok()?;
    let suffix = rest[digits.len()..].trim_start_matches(['_', '.', '-']);

    let field = match suffix {
        "packets" | "pkts" | "xdp_packets" => Field::Packets,
        "bytes" => Field::Bytes,
        "drops" | "dropped" | "drop" => Field::DroppedPackets,
        "drop_bytes" | "dropped_bytes" => Field::DroppedBytes,
        _ => return None,
    };
    Some((index, field))
}

/// A counter that some drivers split into several named parts.
pub fn total(statistics: &Statistics, aliases: &[&str]) -> Option<u64> {
    sum(statistics, aliases)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn statistics(pairs: &[(&str, u64)]) -> Statistics {
        pairs
            .iter()
            .map(|(name, value)| ((*name).to_string(), *value))
            .collect()
    }

    #[test]
    fn the_first_alias_present_is_the_one_used() {
        let ixgbe = statistics(&[("rx_crc_errors", 12)]);
        let bnxt = statistics(&[("rx_fcs_errors", 7)]);
        assert_eq!(lookup(&ixgbe, FCS_ERRORS), Some(12));
        assert_eq!(lookup(&bnxt, FCS_ERRORS), Some(7));
    }

    /// The whole reason the counters are `Option`.
    #[test]
    fn a_counter_no_driver_alias_matches_stays_absent() {
        let sparse = statistics(&[("rx_packets", 1)]);
        assert_eq!(lookup(&sparse, FCS_ERRORS), None);
        assert_eq!(lookup(&sparse, SYMBOL_ERRORS), None);
    }

    #[test]
    fn queues_are_found_however_the_driver_spells_them() {
        for names in [
            ["tx_queue_0_packets", "tx_queue_0_bytes"],
            ["tx0_packets", "tx0_bytes"],
            ["tx-0.packets", "tx-0.bytes"],
        ] {
            let stats = statistics(&[(names[0], 5), (names[1], 640)]);
            let queues = queues(&stats);
            assert_eq!(queues.len(), 1, "{names:?}");
            assert_eq!(queues[0].name, "UC0");
            assert_eq!(queues[0].packets, 5);
            assert_eq!(queues[0].bytes, 640);
        }
    }

    #[test]
    fn queues_come_back_numbered_in_order_and_carry_their_drops() {
        let stats = statistics(&[
            ("tx_queue_10_packets", 10),
            ("tx_queue_2_packets", 2),
            ("tx_queue_2_drops", 1),
            ("tx_queue_0_packets", 0),
        ]);
        let queues = queues(&stats);
        let names: Vec<&str> = queues.iter().map(|q| q.name.as_str()).collect();
        assert_eq!(names, ["UC0", "UC2", "UC10"]);
        assert_eq!(queues[1].dropped_packets, 1);
    }

    /// A receive-queue counter is not a transmit queue, and a driver-global
    /// counter is not a queue at all.
    #[test]
    fn only_transmit_queues_become_rows() {
        let stats = statistics(&[
            ("rx_queue_0_packets", 5),
            ("tx_packets", 9),
            ("tx_timeout_count", 2),
        ]);
        assert!(queues(&stats).is_empty());
    }

    #[test]
    fn a_driver_with_no_frame_size_bins_reports_none_of_them() {
        assert_eq!(bins(&statistics(&[("rx_packets", 1)])), None);
    }

    #[test]
    fn a_driver_with_some_bins_reports_zero_for_the_rest() {
        let stats = statistics(&[("rx_size_64", 100), ("tx_size_64", 50)]);
        let bins = bins(&stats).expect("some bins");
        assert_eq!(bins.received[0], 100);
        assert_eq!(bins.received[6], 0);
        assert_eq!(bins.transmitted[0], 50);
    }

    #[test]
    fn a_split_counter_is_added_up_rather_than_read_once() {
        let stats = statistics(&[("port.rx_pause", 3), ("rx_pause_frames", 4)]);
        assert_eq!(total(&stats, PAUSE_IN), Some(7));
        assert_eq!(total(&statistics(&[]), PAUSE_IN), None);
    }
}
