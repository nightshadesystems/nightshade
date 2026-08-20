//! Byte-exact output fixtures for every `show interfaces` command.
//!
//! Each file under `tests/golden/` is the reference output from
//! `docs/specs/show-interfaces.md`, copied in. Byte-exact is the point: it is
//! the only way a change to a shared column module shows up in the diff of the
//! commit that caused it rather than in a support call six months later.
//!
//! Regenerate after a deliberate change:
//!
//! ```sh
//! UPDATE_GOLDEN=1 cargo test -p nightshade-ifstate --test golden
//! ```
//!
//! and read the diff before committing it. A golden file updated without being
//! read is a golden file that tests nothing.
//!
//! # Two places these differ from the specification, and why
//!
//! **Row order in `show interfaces`.** The reference blocks do not agree with
//! each other. `show interfaces description` and `show interfaces status` both
//! list the physical ports first and then `bond0, lo, vlan10`; `show
//! interfaces` lists `eth0, eth1, eth2, lo, vlan10, bond0`. No single ordering
//! rule produces all three, so the rule the other two agree on is the one
//! implemented -- ports first, then everything built on them, each in natural
//! order -- and the `show interfaces` golden carries `bond0` in the position
//! that rule puts it. Every byte inside each stanza is the reference's.
//!
//! **The `...` line in the EEPROM dump.** The reference block ends with a
//! literal `    ...`, which marks an elision rather than a line of output. The
//! fixture's page is exactly the thirty-two bytes the two shown rows decode,
//! so the golden is the reference with that marker removed.

use std::path::{Path, PathBuf};

use nightshade_ifstate::model::Link;
use nightshade_ifstate::query::View;
use nightshade_ifstate::{Snapshot, render};

mod fixtures;

fn golden_path(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/golden")
        .join(format!("{name}.txt"))
}

fn updating() -> bool {
    std::env::var_os("UPDATE_GOLDEN").is_some()
}

/// Render `snapshot` through `view` and hold it against the golden file.
fn check(name: &str, snapshot: &Snapshot, view: View) {
    let produced = render(snapshot, &view);
    let path = golden_path(name);

    if updating() {
        std::fs::write(&path, &produced).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        return;
    }

    let expected = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("reading {}: {e}", path.display()));

    if produced != expected {
        panic!(
            "{} does not match {}\n\
             --- expected ---\n{}\n--- produced ---\n{}\n--- first difference ---\n{}",
            name,
            path.display(),
            expected,
            produced,
            first_difference(&expected, &produced),
        );
    }
}

/// The line and column that differ, because a whitespace change in a table is
/// invisible in a diff of two blocks of text.
fn first_difference(expected: &str, produced: &str) -> String {
    for (number, (left, right)) in expected.lines().zip(produced.lines()).enumerate() {
        if left != right {
            let column = left
                .chars()
                .zip(right.chars())
                .position(|(a, b)| a != b)
                .unwrap_or_else(|| left.chars().count().min(right.chars().count()));
            return format!(
                "line {}, column {column}\n  expected: {left:?}\n  produced: {right:?}",
                number + 1
            );
        }
    }
    format!(
        "line count: expected {}, produced {}",
        expected.lines().count(),
        produced.lines().count()
    )
}

#[test]
fn show_interfaces() {
    check("interfaces", &fixtures::interfaces(), View::Detail);
}

#[test]
fn show_interfaces_description() {
    check("description", &fixtures::description(), View::Description);
}

#[test]
fn show_interfaces_status() {
    check("status", &fixtures::status(), View::Status(None));
}

#[test]
fn show_interfaces_status_errdisabled() {
    check(
        "status-errdisabled",
        &fixtures::errdisabled(),
        View::Status(Some(Link::ErrDisabled)),
    );
}

#[test]
fn show_interfaces_counters() {
    check("counters", &fixtures::ports(), View::Counters);
}

#[test]
fn show_interfaces_counters_errors() {
    check("counters-errors", &fixtures::ports(), View::CountersErrors);
}

#[test]
fn show_interfaces_counters_discards() {
    check(
        "counters-discards",
        &fixtures::ports(),
        View::CountersDiscards,
    );
}

#[test]
fn show_interfaces_counters_rates() {
    check("counters-rates", &fixtures::ports(), View::CountersRates);
}

#[test]
fn show_interfaces_counters_queue() {
    check("counters-queue", &fixtures::queues(), View::CountersQueue);
}

#[test]
fn show_interfaces_counters_bins() {
    check("counters-bins", &fixtures::bins(), View::CountersBins);
}

#[test]
fn show_interfaces_transceiver() {
    check("transceiver", &fixtures::transceivers(), View::Transceiver);
}

#[test]
fn show_interfaces_transceiver_detail() {
    check(
        "transceiver-detail",
        &fixtures::transceiver_detail(),
        View::TransceiverDetail,
    );
}

#[test]
fn show_interfaces_transceiver_properties() {
    check(
        "transceiver-properties",
        &fixtures::transceiver_properties(),
        View::TransceiverProperties,
    );
}

#[test]
fn show_interfaces_transceiver_eeprom() {
    check(
        "transceiver-eeprom",
        &fixtures::eeprom(),
        View::TransceiverEeprom,
    );
}

#[test]
fn show_interfaces_capabilities() {
    check("capabilities", &fixtures::capabilities(), View::Capabilities);
}

#[test]
fn show_interfaces_flowcontrol() {
    check("flowcontrol", &fixtures::flowcontrol(), View::FlowControl);
}

#[test]
fn show_interfaces_negotiation() {
    check("negotiation", &fixtures::negotiation(), View::Negotiation);
}

#[test]
fn show_interfaces_negotiation_detail() {
    check(
        "negotiation-detail",
        &fixtures::negotiation_detail(),
        View::NegotiationDetail,
    );
}

#[test]
fn show_interfaces_phy_detail() {
    check("phy-detail", &fixtures::phy(), View::PhyDetail);
}

#[test]
fn show_interfaces_mac() {
    check("mac", &fixtures::macs(), View::Mac);
}

#[test]
fn show_interfaces_mac_detail() {
    check("mac-detail", &fixtures::mac_detail(), View::MacDetail);
}

/// The same data, structured. `| display json` and the text form come from one
/// snapshot, so this is a check that the model carries everything the text
/// prints rather than a second rendering of it.
#[test]
fn the_json_form_carries_what_the_text_form_prints() {
    let snapshot = fixtures::interfaces();
    let json = serde_json::to_value(&snapshot).expect("a snapshot serialises");

    let eth0 = &json["interfaces"][0];
    assert_eq!(eth0["name"], "eth0");
    assert_eq!(eth0["mac"], "2c:dd:e9:12:00:a1");
    assert_eq!(eth0["counters"]["in_octets"], 4_816_030_792_344u64);
    assert_eq!(eth0["rates"]["interval"], 300);
    assert_eq!(eth0["oper"], "up");
    assert_eq!(eth0["link"], "connected");

    // And it survives the round trip, which is what the socket does to it.
    let back: Snapshot = serde_json::from_value(json).expect("a snapshot deserialises");
    assert_eq!(back, snapshot);
}

/// Asking for one interface is the same command with a filter, so it must
/// produce exactly that interface's stanza out of the full output.
#[test]
fn one_interface_is_a_slice_of_the_whole() {
    let snapshot = fixtures::interfaces();
    let whole = render(&snapshot, &View::Detail);

    let only = Snapshot {
        interfaces: vec![snapshot.get("eth1").expect("eth1 is in the fixture").clone()],
        system: snapshot.system.clone(),
    };
    let one = render(&only, &View::Detail);

    assert!(whole.contains(&one), "{one}");
    assert!(one.starts_with("eth1 is up,"), "{one}");
}
