//! Every number that reaches a person, formatted in one place.
//!
//! These are the rules an operator learns by reading the output, so they are
//! written down once and unit-tested rather than spelled out again at each
//! call site. A rate that is three significant figures here and two somewhere
//! else is a rate an operator cannot compare between two commands.

use crate::model::Duplex;

/// Bytes of preamble, start-of-frame delimiter and inter-frame gap that the
/// wire carries and the counters do not: 7 + 1 + 12.
///
/// This is the whole of what `with framing overhead` means. A port at exactly
/// line rate with 64-byte frames shows 100% only when these 20 bytes per frame
/// are counted, and a utilisation figure that quietly leaves them out is one
/// that never reaches 100% and cannot be used to answer "is this port full".
pub const FRAMING_OVERHEAD_BYTES: f64 = 20.0;

// ---------------------------------------------------------------------------
// MAC addresses
// ---------------------------------------------------------------------------

/// `2c:dd:e9:12:00:a1`.
///
/// Colon separated and lower case, which is what `ip link` prints and what an
/// operator will paste into a filter, rather than the dotted quads EOS uses.
pub fn mac(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 3);
    for (i, byte) in bytes.iter().enumerate() {
        if i > 0 {
            out.push(':');
        }
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// Whether an address is the all-zero one the kernel gives interfaces that
/// have none. `lo` has one, and printing it would be inventing hardware.
pub fn mac_is_unset(text: &str) -> bool {
    !text.is_empty() && text.chars().all(|c| c == '0' || c == ':')
}

// ---------------------------------------------------------------------------
// durations
// ---------------------------------------------------------------------------

/// `12 days, 4 hours, 33 minutes, 12 seconds`.
///
/// Leading zero units are dropped and the rest are kept, zero or not: an
/// uptime of `12 days, 0 hours, 5 minutes, 0 seconds` reads as a duration,
/// where `12 days, 5 minutes` reads as an omission.
pub fn duration_words(seconds: u64) -> String {
    let days = seconds / 86_400;
    let hours = (seconds % 86_400) / 3_600;
    let minutes = (seconds % 3_600) / 60;
    let secs = seconds % 60;

    let mut parts: Vec<String> = Vec::with_capacity(4);
    let mut started = false;
    for (value, unit) in [(days, "day"), (hours, "hour"), (minutes, "minute")] {
        if value != 0 {
            started = true;
        }
        if started {
            parts.push(plural(value, unit));
        }
    }
    parts.push(plural(secs, "second"));
    parts.join(", ")
}

fn plural(value: u64, unit: &str) -> String {
    if value == 1 {
        format!("{value} {unit}")
    } else {
        format!("{value} {unit}s")
    }
}

/// `12 days, 4:33:12`, or `0:00:04` when it is under a day.
///
/// Hours are not padded and minutes and seconds are, which is how a clock
/// reads and how EOS prints it.
pub fn duration_clock(seconds: u64) -> String {
    let days = seconds / 86_400;
    let hours = (seconds % 86_400) / 3_600;
    let minutes = (seconds % 3_600) / 60;
    let secs = seconds % 60;
    let clock = format!("{hours}:{minutes:02}:{secs:02}");
    if days == 0 {
        clock
    } else {
        format!("{}, {clock}", plural(days, "day"))
    }
}

/// `12 days, 4:33:12 ago`, or `never`.
pub fn ago(seconds: Option<u64>) -> String {
    match seconds {
        Some(seconds) => format!("{} ago", duration_clock(seconds)),
        None => "never".to_string(),
    }
}

/// What the rate lines call the load interval: `5 minutes`, `30 seconds`.
///
/// Minutes when it divides evenly, because that is how it is configured and
/// how it is talked about; seconds otherwise, rather than `0.5 minutes`.
pub fn interval_label(seconds: u32) -> String {
    if seconds != 0 && seconds.is_multiple_of(60) {
        plural((seconds / 60) as u64, "minute")
    } else {
        plural(seconds as u64, "second")
    }
}

/// The `Intvl` column: a 300-second interval prints as `0:05`.
///
/// That is not a typo and it is not seconds. EOS renders the interval in
/// minutes through an `h:mm` clock, so five minutes comes out as five past
/// midnight. It is preserved because an operator who has learnt to read `0:05`
/// as "five minutes" on one box should not have to unlearn it on this one.
pub fn interval_clock(seconds: u32) -> String {
    let minutes = seconds / 60;
    format!("{}:{:02}", minutes / 60, minutes % 60)
}

// ---------------------------------------------------------------------------
// rates and utilisation
// ---------------------------------------------------------------------------

/// `24.7 Mbps`, `3.11 Mbps`, `210 Mbps`, `0 bps`.
///
/// Three significant figures, in the largest unit that leaves the value at or
/// above one. Zero is `0 bps` rather than `0.00 bps`, because a port with
/// nothing on it should say so in as few characters as possible.
pub fn rate(bits_per_second: f64) -> String {
    if !bits_per_second.is_finite() || bits_per_second <= 0.0 {
        return "0 bps".to_string();
    }

    const UNITS: [(&str, f64); 4] = [
        ("bps", 1.0),
        ("kbps", 1e3),
        ("Mbps", 1e6),
        ("Gbps", 1e9),
    ];

    let mut chosen = 0;
    for (index, (_, scale)) in UNITS.iter().enumerate() {
        if bits_per_second >= *scale {
            chosen = index;
        }
    }
    let mut value = bits_per_second / UNITS[chosen].1;
    let mut decimals = significant_decimals(value);

    // 999.6 kbps would round to `1000 kbps`, which is three digits too many
    // and one unit too few. Carry it rather than print it.
    if round_to(value, decimals) >= 1000.0 && chosen + 1 < UNITS.len() {
        chosen += 1;
        value = bits_per_second / UNITS[chosen].1;
        decimals = significant_decimals(value);
    }

    format!("{value:.decimals$} {}", UNITS[chosen].0)
}

fn significant_decimals(value: f64) -> usize {
    if value < 10.0 {
        2
    } else if value < 100.0 {
        1
    } else {
        0
    }
}

fn round_to(value: f64, decimals: usize) -> f64 {
    let scale = 10f64.powi(decimals as i32);
    (value * scale).round() / scale
}

/// `0.2%`. One decimal, everywhere a percentage of line rate is printed.
pub fn percent(value: f64) -> String {
    let value = if value.is_finite() { value } else { 0.0 };
    format!("{value:.1}%")
}

/// Utilisation of a line, framing overhead included.
///
/// The counters measure frames from destination address to FCS. The wire also
/// carries [`FRAMING_OVERHEAD_BYTES`] per frame that no counter ever sees, so
/// they are added back from the packet rate before the division -- which is
/// why this needs the packet rate as well as the bit rate, and why a port
/// running small frames shows a higher utilisation than its byte counters
/// alone would suggest.
///
/// A port with no known speed has no line to be a fraction of, and reports 0
/// rather than an infinity.
pub fn utilisation(bits_per_second: f64, packets_per_second: f64, speed_mbps: Option<u64>) -> f64 {
    let Some(speed_mbps) = speed_mbps.filter(|speed| *speed > 0) else {
        return 0.0;
    };
    let line = speed_mbps as f64 * 1e6;
    let framing = packets_per_second * FRAMING_OVERHEAD_BYTES * 8.0;
    let used = (bits_per_second + framing).max(0.0);
    (used / line) * 100.0
}

// ---------------------------------------------------------------------------
// speeds
// ---------------------------------------------------------------------------

/// `10G`, `2.5G`, `100M` -- the `Speed` column.
pub fn speed_short(mbps: u64) -> String {
    if mbps >= 1000 && mbps.is_multiple_of(1000) {
        format!("{}G", mbps / 1000)
    } else if mbps >= 1000 {
        format!("{:.1}G", mbps as f64 / 1000.0)
    } else {
        format!("{mbps}M")
    }
}

/// `10Gb/s` -- the detail line.
pub fn speed_long(mbps: u64) -> String {
    format!("{}b/s", speed_short(mbps))
}

/// `10Gbps` -- what the PHY section calls the same number.
pub fn speed_phy(mbps: u64) -> String {
    format!("{}bps", speed_short(mbps))
}

/// `10Gfull`, `1Ghalf` -- the configured speed and duplex, as one word.
pub fn speed_duplex_word(mbps: u64, duplex: Option<Duplex>) -> String {
    match duplex {
        Some(duplex) => format!("{}{}", speed_short(mbps), duplex.short()),
        None => speed_short(mbps),
    }
}

/// `1G/full` -- one entry of an advertisement or a capability list.
pub fn speed_duplex_slashed(mbps: u64, duplex: Duplex) -> String {
    format!("{}/{}", speed_short(mbps), duplex.short())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn macs_are_colon_separated_and_lower_case() {
        assert_eq!(mac(&[0x2c, 0xdd, 0xe9, 0x12, 0x00, 0xa1]), "2c:dd:e9:12:00:a1");
        assert_eq!(mac(&[0xff, 0x00, 0x0a, 0xb0, 0xcd, 0x01]), "ff:00:0a:b0:cd:01");
        // Not every interface has six bytes. Infiniband has twenty, and a
        // format that assumes six would silently print a third of one.
        assert_eq!(mac(&[0x01, 0x02, 0x03]), "01:02:03");
        assert_eq!(mac(&[]), "");
    }

    #[test]
    fn an_all_zero_address_is_no_address() {
        assert!(mac_is_unset("00:00:00:00:00:00"));
        assert!(!mac_is_unset("2c:dd:e9:12:00:a1"));
        assert!(!mac_is_unset(""));
    }

    #[test]
    fn durations_read_as_english() {
        assert_eq!(
            duration_words(12 * 86_400 + 4 * 3_600 + 33 * 60 + 12),
            "12 days, 4 hours, 33 minutes, 12 seconds"
        );
        // vlan10 in the reference output: a trailing zero is kept.
        assert_eq!(
            duration_words(12 * 86_400 + 4 * 3_600 + 31 * 60),
            "12 days, 4 hours, 31 minutes, 0 seconds"
        );
        // Leading zeroes are not.
        assert_eq!(duration_words(90), "1 minute, 30 seconds");
        assert_eq!(duration_words(1), "1 second");
        assert_eq!(duration_words(0), "0 seconds");
        assert_eq!(duration_words(86_400), "1 day, 0 hours, 0 minutes, 0 seconds");
    }

    #[test]
    fn durations_also_read_as_a_clock() {
        assert_eq!(
            duration_clock(12 * 86_400 + 4 * 3_600 + 33 * 60 + 12),
            "12 days, 4:33:12"
        );
        assert_eq!(duration_clock(4), "0:00:04");
        assert_eq!(duration_clock(86_400), "1 day, 0:00:00");
        assert_eq!(duration_clock(0), "0:00:00");
    }

    #[test]
    fn a_counter_that_was_never_cleared_says_so() {
        assert_eq!(ago(None), "never");
        assert_eq!(
            ago(Some(12 * 86_400 + 4 * 3_600 + 33 * 60 + 12)),
            "12 days, 4:33:12 ago"
        );
        assert_eq!(ago(Some(4)), "0:00:04 ago");
    }

    #[test]
    fn the_load_interval_is_named_in_the_unit_it_was_set_in() {
        assert_eq!(interval_label(300), "5 minutes");
        assert_eq!(interval_label(60), "1 minute");
        assert_eq!(interval_label(30), "30 seconds");
        assert_eq!(interval_label(1), "1 second");
        assert_eq!(interval_label(0), "0 seconds");
    }

    /// The quirk. Five minutes is `0:05`, not `5:00` and not `0:05:00`.
    #[test]
    fn the_interval_column_is_minutes_on_an_hour_clock() {
        assert_eq!(interval_clock(300), "0:05");
        assert_eq!(interval_clock(30), "0:00");
        assert_eq!(interval_clock(3_600), "1:00");
        assert_eq!(interval_clock(5_400), "1:30");
    }

    #[test]
    fn rates_carry_three_significant_figures() {
        assert_eq!(rate(24_700_000.0), "24.7 Mbps");
        assert_eq!(rate(3_110_000.0), "3.11 Mbps");
        assert_eq!(rate(1_020_000.0), "1.02 Mbps");
        assert_eq!(rate(96_300_000.0), "96.3 Mbps");
        assert_eq!(rate(210_000_000.0), "210 Mbps");
        assert_eq!(rate(189_000_000.0), "189 Mbps");
        assert_eq!(rate(0.0), "0 bps");
        assert_eq!(rate(512.0), "512 bps");
        assert_eq!(rate(9_400.0), "9.40 kbps");
        assert_eq!(rate(4_000_000_000.0), "4.00 Gbps");
    }

    /// The carry: 999_600 bits is not `1000 kbps`.
    #[test]
    fn a_rate_that_rounds_past_its_unit_moves_up_one() {
        assert_eq!(rate(999_600.0), "1.00 Mbps");
        assert_eq!(rate(999_999_999.0), "1.00 Gbps");
    }

    #[test]
    fn a_rate_cannot_be_negative_or_nan() {
        assert_eq!(rate(-1.0), "0 bps");
        assert_eq!(rate(f64::NAN), "0 bps");
        assert_eq!(rate(f64::INFINITY), "0 bps");
    }

    #[test]
    fn utilisation_counts_the_bytes_the_counters_cannot_see() {
        // 4123 frames a second of preamble and gap is 659_680 bits nobody
        // counted; on a 10G port that is a quarter of a tenth of a percent,
        // and it is the difference between 0.2 and 0.3 on the display.
        let without = utilisation(24_700_000.0, 0.0, Some(10_000));
        let with = utilisation(24_700_000.0, 4_123.0, Some(10_000));
        assert!((without - 0.247).abs() < 1e-9, "{without}");
        assert!((with - 0.2536).abs() < 1e-4, "{with}");

        // A full 10G port of 64-byte frames: 14_880_952 frames a second,
        // 7_619_047_424 bits of payload, and the rest is framing.
        let full = utilisation(7_619_047_424.0, 14_880_952.0, Some(10_000));
        assert!((full - 100.0).abs() < 0.01, "{full}");
    }

    #[test]
    fn utilisation_of_a_port_with_no_known_speed_is_zero_and_not_an_infinity() {
        assert_eq!(utilisation(1_000.0, 1.0, None), 0.0);
        assert_eq!(utilisation(1_000.0, 1.0, Some(0)), 0.0);
    }

    #[test]
    fn percentages_carry_one_decimal() {
        assert_eq!(percent(0.247), "0.2%");
        assert_eq!(percent(0.963), "1.0%");
        assert_eq!(percent(1.05), "1.1%");
        assert_eq!(percent(0.0), "0.0%");
        assert_eq!(percent(f64::NAN), "0.0%");
    }

    #[test]
    fn speeds_are_named_the_way_a_port_is_sold() {
        assert_eq!(speed_short(10_000), "10G");
        assert_eq!(speed_short(1_000), "1G");
        assert_eq!(speed_short(2_500), "2.5G");
        assert_eq!(speed_short(100), "100M");
        assert_eq!(speed_short(10), "10M");
        assert_eq!(speed_long(10_000), "10Gb/s");
        assert_eq!(speed_phy(10_000), "10Gbps");
        assert_eq!(speed_duplex_word(10_000, Some(Duplex::Full)), "10Gfull");
        assert_eq!(speed_duplex_word(10_000, None), "10G");
        assert_eq!(speed_duplex_slashed(1_000, Duplex::Full), "1G/full");
        assert_eq!(speed_duplex_slashed(10, Duplex::Half), "10M/half");
    }
}
