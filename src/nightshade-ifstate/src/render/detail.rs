//! `show interfaces` and `show interfaces <name>`.
//!
//! The long form: one indented block per interface, in the order EOS prints
//! them. Which lines appear depends on what the interface is -- a loopback has
//! no duplex, a bond has members instead of a PHY, and an interface with no
//! traffic through it has no counter block -- so the block is assembled from
//! sections rather than printed from one template with holes in it.

use crate::block::Block;
use crate::model::{Counters, Interface, Kind, Rates};
use crate::units;

use super::stanzas;

/// Indent of everything under the state line.
const FIELD: usize = 2;
/// Indent of the counter block, which EOS sets in further than the fields.
const COUNTER: usize = 5;

pub fn render(snapshot: &super::Snapshot) -> String {
    stanzas(
        super::rows(snapshot, |_| true)
            .into_iter()
            .map(one)
            .collect(),
    )
}

/// One interface's block, ending in a newline.
pub fn one(interface: &Interface) -> String {
    let mut block = Block::new();

    state_line(&mut block, interface);
    hardware(&mut block, interface);
    block.maybe(FIELD, "Description", interface.description.as_deref());
    addresses(&mut block, interface);
    mtu(&mut block, interface);
    if interface.kind.is_physical() {
        link_line(&mut block, interface);
    }
    uptime(&mut block, interface);
    if interface.kind.is_physical() {
        block.maybe(FIELD, "Loopback Mode ", interface.loopback_mode.as_deref());
        block.raw(
            FIELD,
            &format!(
                "{} link status changes since last clear",
                interface.link_changes
            ),
        );
        block.raw(
            FIELD,
            &format!(
                "Last clearing of \"show interface\" counters {}",
                units::ago(interface.last_clear)
            ),
        );
    }
    if let Some(bond) = &interface.bond {
        block.raw(
            FIELD,
            &format!("Active members in this channel: {}", bond.members.len()),
        );
        for member in &bond.members {
            // The three dots are EOS's, not an elision: it is how a
            // port-channel lists what is in it.
            let mut line = format!("... {} ", member.name);
            if let Some(duplex) = member.duplex {
                line.push_str(&format!(", {}", duplex.long()));
            }
            if let Some(speed) = member.speed_mbps {
                line.push_str(&format!(", {}", units::speed_long(speed)));
            }
            block.raw(FIELD, &line);
        }
        block.maybe(FIELD, "Fallback mode is", bond.fallback.as_deref());
    }
    if let Some(rates) = &interface.rates {
        rate_lines(&mut block, rates);
    }
    if interface.shows_counters()
        && let Some(counters) = &interface.counters
    {
        counter_block(&mut block, interface.kind, counters);
    }

    block.take()
}

/// `eth0 is up, line protocol is up (connected)`.
fn state_line(block: &mut Block, interface: &Interface) {
    block.heading(&format!(
        "{} is {}, line protocol is {} ({})",
        interface.name,
        interface.admin_words(),
        interface.oper.label(),
        interface.link.label(),
    ));
}

/// `Hardware is Ethernet, address is <mac> (bia <bia>)`.
///
/// The address half is dropped when there is none to print. A loopback's
/// all-zero address is not an address, and printing it would be inventing
/// hardware that is not there.
fn hardware(block: &mut Block, interface: &Interface) {
    let mut line = format!("Hardware is {}", interface.kind.hardware());
    if let Some(mac) = interface.mac.as_deref().filter(|m| !units::mac_is_unset(m)) {
        let bia = interface.bia.as_deref().unwrap_or(mac);
        line.push_str(&format!(", address is {mac} (bia {bia})"));
    }
    block.raw(FIELD, &line);
}

fn addresses(block: &mut Block, interface: &Interface) {
    for address in &interface.addresses {
        if address.prefix.is_empty() {
            continue;
        }
        block.raw(FIELD, &format!("Internet address is {}", address.prefix));
        if let Some(broadcast) = address.broadcast.as_deref().filter(|b| !b.is_empty()) {
            block.raw(FIELD, &format!("Broadcast address is {broadcast}"));
        }
    }
}

/// `IP MTU 1500 bytes, BW 10000000 kbit`.
///
/// `IP MTU` on an interface that carries addresses and `Ethernet MTU` on one
/// that does not, which is EOS's way of saying whether the number is a layer 3
/// or a layer 2 limit.
fn mtu(block: &mut Block, interface: &Interface) {
    let Some(mtu) = interface.mtu else {
        return;
    };
    let label = if interface.addresses.is_empty() {
        "Ethernet MTU"
    } else {
        "IP MTU"
    };
    let mut line = format!("{label} {mtu} bytes");
    if let Some(bandwidth) = interface.bandwidth_kbit {
        line.push_str(&format!(", BW {bandwidth} kbit"));
    }
    block.raw(FIELD, &line);
}

/// `Full-duplex, 10Gb/s, auto negotiation: off, uni-link: n/a`.
fn link_line(block: &mut Block, interface: &Interface) {
    let duplex = match interface.duplex {
        Some(duplex) => duplex.long().to_string(),
        None => "Unconfigured".to_string(),
    };
    // A port with no link and no forced speed has no speed to report, and
    // saying `0Mb/s` would be a measurement rather than an absence.
    let speed = match interface.speed_mbps {
        Some(speed) => units::speed_long(speed),
        None => "Unconfigured".to_string(),
    };
    let autoneg = match interface.autoneg {
        Some(true) => "on",
        Some(false) => "off",
        None => "n/a",
    };
    let uni_link = interface.uni_link.as_deref().unwrap_or("n/a");
    block.raw(
        FIELD,
        &format!("{duplex}, {speed}, auto negotiation: {autoneg}, uni-link: {uni_link}"),
    );
}

/// `Up 12 days, 4 hours, 33 minutes, 12 seconds`.
fn uptime(block: &mut Block, interface: &Interface) {
    let Some(since) = interface.since else {
        return;
    };
    let word = if interface.oper == crate::model::Oper::Up {
        "Up"
    } else {
        "Down"
    };
    block.raw(FIELD, &format!("{word} {}", units::duration_words(since)));
}

/// The two rate lines.
fn rate_lines(block: &mut Block, rates: &Rates) {
    let window = units::interval_label(rates.interval);
    for (direction, bps, pps, percent) in [
        ("input", rates.in_bps, rates.in_pps, rates.in_percent),
        ("output", rates.out_bps, rates.out_pps, rates.out_percent),
    ] {
        block.raw(
            FIELD,
            &format!(
                "{window} {direction} rate {} ({} with framing overhead), {} packets/sec",
                units::rate(bps),
                units::percent(percent),
                pps.max(0.0).round() as u64,
            ),
        );
    }
}

/// The counter block.
///
/// Physical ports get the full one. Everything else gets the reduced one:
/// runts, giants, CRC, symbol errors, collisions and PAUSE frames are
/// properties of a wire, and a bond, a VLAN or a tunnel has none. Printing
/// them as zero would be answering a question the interface cannot be asked.
fn counter_block(block: &mut Block, kind: Kind, counters: &Counters) {
    block.raw(
        COUNTER,
        &format!(
            "{} packets input, {} bytes",
            counters.in_unicast, counters.in_octets
        ),
    );
    block.raw(
        COUNTER,
        &format!(
            "Received {} broadcasts, {} multicast",
            counters.in_broadcast, counters.in_multicast
        ),
    );

    if kind.is_physical() {
        block.raw(
            COUNTER,
            &format!(
                "{} runts, {} giants",
                counters.runts.unwrap_or(0),
                counters.giants.unwrap_or(0)
            ),
        );
        block.raw(
            COUNTER,
            &format!(
                "{} input errors, {} CRC, {} alignment, {} symbol, {} input discards",
                counters.in_errors,
                counters.fcs_errors.unwrap_or(0),
                counters.alignment_errors.unwrap_or(0),
                counters.symbol_errors.unwrap_or(0),
                counters.in_discards,
            ),
        );
        block.raw(
            COUNTER,
            &format!("{} PAUSE input", counters.pause_in.unwrap_or(0)),
        );
    } else {
        block.raw(
            COUNTER,
            &format!(
                "{} input errors, {} input discards",
                counters.in_errors, counters.in_discards
            ),
        );
    }

    block.raw(
        COUNTER,
        &format!(
            "{} packets output, {} bytes",
            counters.out_unicast, counters.out_octets
        ),
    );
    block.raw(
        COUNTER,
        &format!(
            "Sent {} broadcasts, {} multicast",
            counters.out_broadcast, counters.out_multicast
        ),
    );

    if kind.is_physical() {
        block.raw(
            COUNTER,
            &format!(
                "{} output errors, {} collisions",
                counters.out_errors,
                counters.collisions.unwrap_or(0)
            ),
        );
        block.raw(
            COUNTER,
            &format!(
                "{} late collision, {} deferred, {} output discards",
                counters.late_collisions.unwrap_or(0),
                counters.deferred.unwrap_or(0),
                counters.out_discards,
            ),
        );
        block.raw(
            COUNTER,
            &format!("{} PAUSE output", counters.pause_out.unwrap_or(0)),
        );
    } else {
        block.raw(
            COUNTER,
            &format!(
                "{} output errors, {} output discards",
                counters.out_errors, counters.out_discards
            ),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Kind, Link, Oper};

    #[test]
    fn an_interface_the_kernel_does_not_have_says_so_on_its_first_line() {
        let mut interface = Interface::new("eth9", Kind::Ethernet);
        interface.present = false;
        interface.oper = Oper::NotPresent;
        interface.link = Link::NotConnect;
        let text = one(&interface);
        assert!(
            text.starts_with("eth9 is administratively down, line protocol is notpresent (notconnect)\n"),
            "{text}"
        );
    }

    #[test]
    fn a_loopback_has_no_hardware_address_to_print() {
        let mut interface = Interface::new("lo", Kind::Loopback);
        interface.mac = Some("00:00:00:00:00:00".into());
        let text = one(&interface);
        assert!(text.contains("Hardware is Loopback\n"), "{text}");
        assert!(!text.contains("address is 00"), "{text}");
    }

    #[test]
    fn an_interface_with_nothing_known_about_it_still_renders() {
        let text = one(&Interface::new("wg0", Kind::Wireguard));
        assert_eq!(
            text,
            "wg0 is administratively down, line protocol is down (notconnect)\n  Hardware is Wireguard\n"
        );
    }
}
