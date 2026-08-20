//! Against a real kernel, on a real interface.
//!
//! Everything else in this crate is tested against messages and buffers built
//! by hand, which proves the parsing and proves nothing about whether the
//! kernel sends what the parsing expects. This does the other half: it creates
//! a veth pair, pushes packets through it, and checks that the numbers come
//! back.
//!
//! Behind a feature because it needs `CAP_NET_ADMIN` and a network namespace,
//! and `cargo test --workspace` runs on laptops and on CI runners that have
//! neither:
//!
//! ```sh
//! sudo -E cargo test -p nightshade-ifprobe --features kernel-tests -- --test-threads=1
//! ```
//!
//! Single-threaded on purpose: these share one network namespace, and two of
//! them creating interfaces at once is two tests watching each other's links.

#![cfg(feature = "kernel-tests")]

use std::process::Command;

use nightshade_common::paths::Paths;
use nightshade_ifprobe::netlink;
use nightshade_ifprobe::probe::Probe;
use nightshade_ifprobe::tracker::Tracker;
use nightshade_ifstate::model::{Kind, Oper};
use nightshade_ifstate::query::{Query, View};
use nightshade_schema::config::ConfigTree;

/// A veth pair that removes itself.
struct Pair {
    name: String,
    peer: String,
}

impl Pair {
    fn create(name: &str) -> Option<Self> {
        let peer = format!("{name}p");
        // `ip` rather than netlink: this crate does not create interfaces and
        // should not grow the ability to in order to be tested.
        let created = Command::new("ip")
            .args(["link", "add", name, "type", "veth", "peer", "name", &peer])
            .status()
            .ok()?;
        if !created.success() {
            return None;
        }
        for interface in [name, &peer] {
            let _ = Command::new("ip")
                .args(["link", "set", interface, "up"])
                .status();
        }
        Some(Self {
            name: name.to_string(),
            peer,
        })
    }
}

impl Drop for Pair {
    fn drop(&mut self) {
        // Removing one end removes both.
        let _ = Command::new("ip")
            .args(["link", "del", &self.name])
            .status();
    }
}

fn probe() -> Probe {
    Probe::new(Paths::system())
}

fn query(view: View) -> Query {
    Query {
        view,
        names: Vec::new(),
    }
}

#[test]
fn a_netlink_dump_finds_an_interface_that_was_just_created() {
    let Some(pair) = Pair::create("nstest0") else {
        panic!("could not create a veth pair; run this as root");
    };

    let mut socket = netlink::Socket::open().expect("a netlink socket");
    let links = socket.links().expect("a link dump");

    let link = links
        .iter()
        .find(|link| link.name == pair.name)
        .expect("the interface just created");
    assert!(link.admin_up());
    assert!(link.index > 0);
    assert_eq!(link.kind.as_deref(), Some("veth"));
    assert!(link.address.as_ref().is_some_and(|mac| mac.len() == 6));
    assert!(link.stats.is_some(), "no counters on a real interface");
}

#[test]
fn the_addresses_the_kernel_holds_come_back_with_it() {
    let Some(pair) = Pair::create("nstest1") else {
        panic!("could not create a veth pair; run this as root");
    };
    let added = Command::new("ip")
        .args(["addr", "add", "203.0.113.2/30", "dev", &pair.name])
        .status()
        .expect("ip addr add");
    assert!(added.success());

    let snapshot = probe().snapshot(
        &query(View::Detail),
        &ConfigTree::new(),
        &Tracker::new(),
        1_000,
    );
    let interface = snapshot.get(&pair.name).expect("the interface");
    assert_eq!(interface.kind, Kind::Ethernet);
    assert!(interface.present);
    assert_eq!(interface.oper, Oper::Up);
    assert!(
        interface
            .addresses
            .iter()
            .any(|address| address.prefix == "203.0.113.2/30"),
        "{:?}",
        interface.addresses
    );
    assert_eq!(interface.mtu, Some(1500));
}

#[test]
fn counters_move_when_packets_do_and_the_rate_follows() {
    let Some(pair) = Pair::create("nstest2") else {
        panic!("could not create a veth pair; run this as root");
    };
    for (interface, address) in [(&pair.name, "203.0.113.2/30"), (&pair.peer, "203.0.113.1/30")] {
        let _ = Command::new("ip")
            .args(["addr", "add", address, "dev", interface])
            .status();
    }

    let probe = probe();
    let mut tracker = Tracker::new();

    tracker.sample(&probe.links().expect("a dump"), 1_000);
    let _ = Command::new("ping")
        .args(["-c", "20", "-i", "0.05", "-I", &pair.name, "203.0.113.1"])
        .output();
    tracker.sample(&probe.links().expect("a dump"), 1_010);

    let snapshot = probe.snapshot(&query(View::Detail), &ConfigTree::new(), &tracker, 1_010);
    let interface = snapshot.get(&pair.name).expect("the interface");

    let counters = interface.counters.as_ref().expect("counters");
    assert!(
        counters.out_octets > 0 && counters.out_unicast > 0,
        "{counters:?}"
    );

    let rates = interface.rates.expect("a rate");
    assert_eq!(rates.interval, 300);
    assert!(rates.out_bps > 0.0, "{rates:?}");
    // A veth carries no line rate, so utilisation is not a fraction of
    // anything and must be zero rather than an infinity.
    assert!(rates.out_percent.is_finite(), "{rates:?}");
}

/// The whole point of the graceful-degradation rule: a veth has no PHY, no
/// module and no driver statistics, and every command still answers.
#[test]
fn every_command_renders_against_a_device_that_answers_almost_nothing() {
    let Some(pair) = Pair::create("nstest3") else {
        panic!("could not create a veth pair; run this as root");
    };

    let views = [
        View::Detail,
        View::Description,
        View::Status(None),
        View::Counters,
        View::CountersErrors,
        View::CountersDiscards,
        View::CountersRates,
        View::CountersQueue,
        View::CountersBins,
        View::Transceiver,
        View::TransceiverDetail,
        View::TransceiverProperties,
        View::TransceiverEeprom,
        View::Capabilities,
        View::FlowControl,
        View::Negotiation,
        View::NegotiationDetail,
        View::Phy,
        View::PhyDetail,
        View::Mac,
        View::MacDetail,
    ];

    let probe = probe();
    for view in views {
        let snapshot = probe.snapshot(
            &Query {
                view: view.clone(),
                names: vec![pair.name.clone()],
            },
            &ConfigTree::new(),
            &Tracker::new(),
            1_000,
        );
        let text = nightshade_ifstate::render(&snapshot, &view);
        assert!(
            text.is_empty() || text.ends_with('\n'),
            "{view:?} produced {text:?}"
        );
        for line in text.lines() {
            assert!(!line.ends_with(' '), "{view:?} left a trailing space");
        }
    }
}

/// Asking for one interface must answer about that interface and no other.
#[test]
fn a_named_query_is_answered_about_that_name() {
    let Some(pair) = Pair::create("nstest4") else {
        panic!("could not create a veth pair; run this as root");
    };

    let snapshot = probe().snapshot(
        &Query {
            view: View::Detail,
            names: vec![pair.name.clone()],
        },
        &ConfigTree::new(),
        &Tracker::new(),
        1_000,
    );
    assert_eq!(snapshot.interfaces.len(), 1);
    assert_eq!(snapshot.interfaces[0].name, pair.name);
}
