//! Decoding a pluggable module's EEPROM.
//!
//! What `ethtool -m` hands back is the raw i2c image: page A0 at offset 0, and
//! for a module with diagnostics, page A2 at offset 256. SFF-8472 says what is
//! in them. This turns that into the [`Transceiver`] the renderers print.
//!
//! # Only what can be trusted
//!
//! Every field is bounds-checked against the length actually read and every
//! one of them is optional. A module that has stopped answering i2c returns
//! `0xff` for everything, a module that was pulled between the length probe
//! and the read returns a short buffer, and neither of those is a reason for
//! `show interfaces transceiver` to fail -- they are reasons for a row of
//! `N/A`.

use nightshade_ifstate::model::{EepromPage, Measure, Transceiver};

/// `ETH_MODULE_SFF_*`, as the kernel numbers them.
pub const SFF_8079: u32 = 0x1;
pub const SFF_8472: u32 = 0x2;
pub const SFF_8436: u32 = 0x3;
pub const SFF_8636: u32 = 0x4;

/// Where page A2 starts in what `ethtool -m` returns.
const A2: usize = 256;
const PAGE: usize = 256;

/// The floor put under a received power of zero.
///
/// A raw reading of zero means "below the module's resolution", which is
/// minus infinity in dBm and is not a number anyone can put in a column. Every
/// optic this will meet alarms well above this, so an operator reading -40.00
/// reads "dark", which is what it means.
const DARK_DBM: f64 = -40.0;

/// Turn a raw EEPROM image into what the transceiver commands print.
pub fn decode(kind: u32, bytes: &[u8]) -> Transceiver {
    let mut module = Transceiver {
        media_type: media_type(kind, bytes),
        vendor: ascii(bytes, 20, 16),
        part_number: ascii(bytes, 40, 16),
        serial_number: ascii(bytes, 68, 16),
        date_code: ascii(bytes, 84, 6),
        pages: pages(bytes),
        ..Transceiver::default()
    };

    // Diagnostics live on the second page, and a module without one reports
    // nothing rather than zero.
    if bytes.len() > A2 + 106 {
        module.temperature = temperature(bytes);
        module.voltage = voltage(bytes);
        module.tx_bias = bias(bytes);
        module.tx_power = power(bytes, 102, 24);
        module.rx_power = power(bytes, 104, 32);
    }

    module
}

/// The raw pages, for the hex dump.
fn pages(bytes: &[u8]) -> Vec<EepromPage> {
    let mut pages = Vec::new();
    if let Some(page) = bytes.get(..PAGE.min(bytes.len())) {
        pages.push(EepromPage {
            name: "A0".to_string(),
            bytes: page.to_vec(),
        });
    }
    if bytes.len() > A2 {
        pages.push(EepromPage {
            name: "A2".to_string(),
            bytes: bytes[A2..].to_vec(),
        });
    }
    pages
}

/// A fixed-width ASCII field, space-padded by the standard.
fn ascii(bytes: &[u8], at: usize, width: usize) -> Option<String> {
    let field = bytes.get(at..at + width)?;
    // A module that has stopped answering reads back all ones; a field of
    // 0xff is not a vendor name.
    if field.iter().all(|byte| *byte == 0xff || *byte == 0x00) {
        return None;
    }
    let text: String = field
        .iter()
        .map(|byte| {
            if byte.is_ascii_graphic() || *byte == b' ' {
                *byte as char
            } else {
                ' '
            }
        })
        .collect();
    let text = text.trim().to_string();
    (!text.is_empty()).then_some(text)
}

/// `10GBASE-SR`, from the compliance-code bytes.
///
/// The codes are a bitmap of what the module claims it can do, most specific
/// first here, because a module that sets both the 10G and the 1G bit is a 10G
/// module with a backwards-compatible mode.
fn media_type(kind: u32, bytes: &[u8]) -> Option<String> {
    if matches!(kind, SFF_8436 | SFF_8636) {
        return qsfp_media_type(bytes);
    }

    let ten_gig = *bytes.get(3)?;
    let ethernet = *bytes.get(6)?;
    let cable = bytes.get(8).copied().unwrap_or(0);

    let name = if ten_gig & 0x10 != 0 {
        "10GBASE-SR"
    } else if ten_gig & 0x20 != 0 {
        "10GBASE-LR"
    } else if ten_gig & 0x40 != 0 {
        "10GBASE-LRM"
    } else if ten_gig & 0x80 != 0 {
        "10GBASE-ER"
    // A direct-attach copper cable sets no optical compliance bit at all and
    // is identified by its cable technology instead.
    } else if cable & 0x04 != 0 {
        "10GBASE-CR"
    } else if cable & 0x08 != 0 {
        "10GBASE-ACC"
    } else if ethernet & 0x01 != 0 {
        "1000BASE-SX"
    } else if ethernet & 0x02 != 0 {
        "1000BASE-LX"
    } else if ethernet & 0x04 != 0 {
        "1000BASE-CX"
    } else if ethernet & 0x08 != 0 {
        "1000BASE-T"
    } else {
        return None;
    };
    Some(name.to_string())
}

/// SFF-8636 puts its Ethernet compliance codes in byte 131 instead.
fn qsfp_media_type(bytes: &[u8]) -> Option<String> {
    let compliance = *bytes.get(131)?;
    let name = if compliance & 0x01 != 0 {
        "40GBASE-CR4"
    } else if compliance & 0x02 != 0 {
        "40GBASE-SR4"
    } else if compliance & 0x04 != 0 {
        "40GBASE-LR4"
    } else if compliance & 0x08 != 0 {
        "40GBASE-CR4"
    } else if compliance & 0x80 != 0 {
        "100GBASE-SR4"
    } else {
        return None;
    };
    Some(name.to_string())
}

fn signed(bytes: &[u8], at: usize) -> Option<i16> {
    Some(i16::from_be_bytes([
        *bytes.get(at)?,
        *bytes.get(at + 1)?,
    ]))
}

fn unsigned(bytes: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_be_bytes([
        *bytes.get(at)?,
        *bytes.get(at + 1)?,
    ]))
}

/// Degrees, as a signed sixteenth-of-a-degree fixed point value.
fn as_celsius(raw: Option<i16>) -> Option<f64> {
    raw.map(|raw| raw as f64 / 256.0)
}

/// Volts, in units of 100 microvolts.
fn as_volts(raw: Option<u16>) -> Option<f64> {
    raw.map(|raw| raw as f64 / 10_000.0)
}

/// Milliamps, in units of two microamps.
fn as_milliamps(raw: Option<u16>) -> Option<f64> {
    raw.map(|raw| raw as f64 / 500.0)
}

/// dBm, from a reading in tenths of a microwatt.
fn as_dbm(raw: Option<u16>) -> Option<f64> {
    let raw = raw?;
    if raw == 0 {
        return Some(DARK_DBM);
    }
    let milliwatts = raw as f64 / 10_000.0;
    Some((10.0 * milliwatts.log10()).max(DARK_DBM))
}

/// The four thresholds are laid out high alarm, low alarm, high warning, low
/// warning -- not in the order they are printed in.
fn temperature(bytes: &[u8]) -> Measure {
    Measure {
        value: as_celsius(signed(bytes, A2 + 96)),
        high_alarm: as_celsius(signed(bytes, A2)),
        low_alarm: as_celsius(signed(bytes, A2 + 2)),
        high_warn: as_celsius(signed(bytes, A2 + 4)),
        low_warn: as_celsius(signed(bytes, A2 + 6)),
    }
}

fn voltage(bytes: &[u8]) -> Measure {
    Measure {
        value: as_volts(unsigned(bytes, A2 + 98)),
        high_alarm: as_volts(unsigned(bytes, A2 + 8)),
        low_alarm: as_volts(unsigned(bytes, A2 + 10)),
        high_warn: as_volts(unsigned(bytes, A2 + 12)),
        low_warn: as_volts(unsigned(bytes, A2 + 14)),
    }
}

fn bias(bytes: &[u8]) -> Measure {
    Measure {
        value: as_milliamps(unsigned(bytes, A2 + 100)),
        high_alarm: as_milliamps(unsigned(bytes, A2 + 16)),
        low_alarm: as_milliamps(unsigned(bytes, A2 + 18)),
        high_warn: as_milliamps(unsigned(bytes, A2 + 20)),
        low_warn: as_milliamps(unsigned(bytes, A2 + 22)),
    }
}

/// `value_at` is the reading; `thresholds_at` is the first of the four.
fn power(bytes: &[u8], value_at: usize, thresholds_at: usize) -> Measure {
    Measure {
        value: as_dbm(unsigned(bytes, A2 + value_at)),
        high_alarm: as_dbm(unsigned(bytes, A2 + thresholds_at)),
        low_alarm: as_dbm(unsigned(bytes, A2 + thresholds_at + 2)),
        high_warn: as_dbm(unsigned(bytes, A2 + thresholds_at + 4)),
        low_warn: as_dbm(unsigned(bytes, A2 + thresholds_at + 6)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 10GBASE-SR SFP+ with diagnostics, built the way a real one reads.
    fn finisar() -> Vec<u8> {
        let mut bytes = vec![0u8; 512];
        bytes[0] = 0x03; // SFP
        bytes[3] = 0x10; // 10GBASE-SR
        bytes[20..33].copy_from_slice(b"FINISAR CORP.");
        bytes[33..36].fill(b' ');
        bytes[40..53].copy_from_slice(b"FTLX8574D3BCL");
        bytes[53..56].fill(b' ');
        bytes[68..75].copy_from_slice(b"UWM01B7");
        bytes[75..84].fill(b' ');
        bytes[84..90].copy_from_slice(b"210412");

        // 33.45 C is 0x2173 in sixteenths of a degree.
        let temperature = (33.45f64 * 256.0).round() as i16;
        bytes[A2 + 96..A2 + 98].copy_from_slice(&temperature.to_be_bytes());
        bytes[A2..A2 + 2].copy_from_slice(&(75i16 * 256).to_be_bytes());
        bytes[A2 + 2..A2 + 4].copy_from_slice(&(-5i16 * 256).to_be_bytes());

        // 3.28 V in hundred-microvolt units.
        bytes[A2 + 98..A2 + 100].copy_from_slice(&32_800u16.to_be_bytes());
        // 6.42 mA in two-microamp units.
        bytes[A2 + 100..A2 + 102].copy_from_slice(&3_210u16.to_be_bytes());
        // -1.87 dBm is 0.65 mW, in tenths of a microwatt.
        bytes[A2 + 102..A2 + 104].copy_from_slice(&6_501u16.to_be_bytes());
        bytes
    }

    #[test]
    fn an_optic_reads_back_as_the_part_it_is() {
        let module = decode(SFF_8472, &finisar());
        assert_eq!(module.media_type.as_deref(), Some("10GBASE-SR"));
        assert_eq!(module.vendor.as_deref(), Some("FINISAR CORP."));
        assert_eq!(module.part_number.as_deref(), Some("FTLX8574D3BCL"));
        assert_eq!(module.serial_number.as_deref(), Some("UWM01B7"));
        assert_eq!(module.date_code.as_deref(), Some("210412"));
    }

    #[test]
    fn the_diagnostics_come_back_in_the_units_they_are_printed_in() {
        let module = decode(SFF_8472, &finisar());
        let temperature = module.temperature.value.expect("a temperature");
        assert!((temperature - 33.45).abs() < 0.01, "{temperature}");
        assert_eq!(module.temperature.high_alarm, Some(75.0));
        assert_eq!(module.temperature.low_alarm, Some(-5.0));

        let voltage = module.voltage.value.expect("a voltage");
        assert!((voltage - 3.28).abs() < 0.001, "{voltage}");

        let bias = module.tx_bias.value.expect("a bias");
        assert!((bias - 6.42).abs() < 0.01, "{bias}");

        let power = module.tx_power.value.expect("a power");
        assert!((power - -1.87).abs() < 0.01, "{power}");
    }

    #[test]
    fn a_dark_receiver_reads_as_dark_rather_than_as_minus_infinity() {
        assert_eq!(as_dbm(Some(0)), Some(DARK_DBM));
        // 1 mW is 0 dBm.
        let odbm = as_dbm(Some(10_000)).expect("a power");
        assert!(odbm.abs() < 1e-9, "{odbm}");
    }

    /// The failure that actually happens: a module stops answering i2c and
    /// every byte reads back as one.
    #[test]
    fn a_module_that_has_stopped_answering_reports_nothing_rather_than_nonsense() {
        let module = decode(SFF_8472, &[0xff; 512]);
        assert_eq!(module.vendor, None);
        assert_eq!(module.serial_number, None);
    }

    #[test]
    fn a_short_read_is_decoded_as_far_as_it_goes() {
        let mut bytes = finisar();
        bytes.truncate(256);
        let module = decode(SFF_8079, &bytes);
        assert_eq!(module.vendor.as_deref(), Some("FINISAR CORP."));
        // No second page, so no diagnostics and no A2 in the dump.
        assert_eq!(module.temperature.value, None);
        assert_eq!(module.pages.len(), 1);
        assert_eq!(module.pages[0].name, "A0");

        let module = decode(SFF_8472, &finisar());
        assert_eq!(module.pages.len(), 2);
        assert_eq!(module.pages[1].name, "A2");
    }

    #[test]
    fn a_direct_attach_cable_is_identified_by_its_cable_technology() {
        let mut bytes = vec![0u8; 256];
        bytes[0] = 0x03;
        bytes[8] = 0x04; // passive cable
        assert_eq!(
            media_type(SFF_8079, &bytes).as_deref(),
            Some("10GBASE-CR")
        );
    }

    #[test]
    fn a_module_with_no_compliance_bits_set_has_no_media_type() {
        assert_eq!(media_type(SFF_8079, &vec![0u8; 256]), None);
        assert_eq!(media_type(SFF_8079, &[]), None);
    }
}
