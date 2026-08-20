//! The box the golden files describe.
//!
//! One eight-port appliance, built as data rather than as text: two 10G optics
//! on the uplink and the bond, two copper gigabit ports, a shut port, a
//! loopback, a VLAN and a bond. Every golden file in `tests/golden/` is this
//! device rendered through one view, so a change to a renderer shows up as a
//! diff in an output rather than as a change to a fixture.
//!
//! Where the reference outputs in `docs/specs/show-interfaces.md` disagree
//! with each other about a value -- the same MAC address is `bond0` in one
//! block and `eth3` in another -- the fixture per view carries what that
//! block shows. The point of these tests is the shape of the output, and
//! bending the shape to make one set of numbers serve every block would test
//! the fixture instead.

#![allow(dead_code)]

use nightshade_ifstate::model::*;

/// 12 days, 4 hours, 33 minutes, 12 seconds -- the uptime of the box.
pub const UPTIME: u64 = 12 * 86_400 + 4 * 3_600 + 33 * 60 + 12;

/// The load interval every rate in the goldens was measured over.
pub const INTERVAL: u32 = 300;

fn port(name: &str, mac: &str) -> Interface {
    let mut interface = Interface::new(name, Kind::Ethernet);
    interface.admin_up = true;
    interface.oper = Oper::Up;
    interface.link = Link::Connected;
    interface.mac = Some(mac.to_string());
    interface.bia = Some(mac.to_string());
    interface
}

fn snapshot(interfaces: Vec<Interface>) -> Snapshot {
    Snapshot {
        interfaces,
        system: System::default(),
    }
}

fn address(prefix: &str) -> Address {
    Address {
        prefix: prefix.to_string(),
        broadcast: Some("255.255.255.255".to_string()),
    }
}

fn rates(in_mbps: f64, in_pps: f64, in_pct: f64, out_mbps: f64, out_pps: f64, out_pct: f64) -> Rates {
    Rates {
        interval: INTERVAL,
        in_bps: in_mbps * 1e6,
        in_pps,
        in_percent: in_pct,
        out_bps: out_mbps * 1e6,
        out_pps,
        out_percent: out_pct,
    }
}

/// Every specific error counter present and zero, which is what a driver that
/// implements the whole of `ethtool -S` reports on a healthy port.
fn clean(mut counters: Counters) -> Counters {
    counters.fcs_errors.get_or_insert(0);
    counters.alignment_errors.get_or_insert(0);
    counters.symbol_errors.get_or_insert(0);
    counters.runts.get_or_insert(0);
    counters.giants.get_or_insert(0);
    counters.collisions.get_or_insert(0);
    counters.late_collisions.get_or_insert(0);
    counters.deferred.get_or_insert(0);
    counters.pause_in.get_or_insert(0);
    counters.pause_out.get_or_insert(0);
    counters
}

/// Packets, octets, multicast and broadcast -- in one direction.
type Direction = (u64, u64, u64, u64);

fn traffic(inbound: Direction, outbound: Direction) -> Counters {
    clean(Counters {
        in_unicast: inbound.0,
        in_octets: inbound.1,
        in_multicast: inbound.2,
        in_broadcast: inbound.3,
        out_unicast: outbound.0,
        out_octets: outbound.1,
        out_multicast: outbound.2,
        out_broadcast: outbound.3,
        ..Counters::default()
    })
}

// ---------------------------------------------------------------------------
// show interfaces
// ---------------------------------------------------------------------------

/// The long form: an uplink, a trunk, a shut port, a bond, the loopback and a
/// VLAN.
pub fn interfaces() -> Snapshot {
    let mut eth0 = port("eth0", "2c:dd:e9:12:00:a1");
    eth0.description = Some("WAN uplink to ISP - Circuit ID 4471-A".into());
    eth0.addresses = vec![address("203.0.113.2/30")];
    eth0.mtu = Some(1500);
    eth0.bandwidth_kbit = Some(10_000_000);
    eth0.duplex = Some(Duplex::Full);
    eth0.speed_mbps = Some(10_000);
    eth0.autoneg = Some(false);
    eth0.loopback_mode = Some("None".into());
    eth0.since = Some(UPTIME);
    eth0.link_changes = 2;
    eth0.last_clear = Some(UPTIME);
    eth0.rates = Some(rates(24.7, 4_123.0, 0.2, 96.3, 9_877.0, 1.0));
    eth0.counters = Some(clean(Counters {
        out_discards: 2,
        ..traffic(
            (4_294_811_034, 4_816_030_792_344, 89_127, 15_234),
            (8_812_734_120, 11_278_449_021_837, 44_506, 1_287),
        )
    }));

    let mut eth1 = port("eth1", "2c:dd:e9:12:00:a2");
    eth1.description = Some("LAN trunk to qs-hq-access1".into());
    eth1.mtu = Some(9214);
    eth1.bandwidth_kbit = Some(1_000_000);
    eth1.duplex = Some(Duplex::Full);
    eth1.speed_mbps = Some(1_000);
    eth1.autoneg = Some(true);
    eth1.loopback_mode = Some("None".into());
    eth1.since = Some(12 * 86_400 + 4 * 3_600 + 31 * 60 + 2);
    eth1.link_changes = 1;
    eth1.rates = Some(rates(3.11, 1_204.0, 0.3, 1.02, 655.0, 0.1));
    eth1.counters = Some(traffic(
        (102_981_234, 90_238_471_234, 412_987, 88_123),
        (88_123_911, 71_234_098_123, 128_730, 9_812),
    ));

    let mut eth2 = port("eth2", "2c:dd:e9:12:00:a3");
    eth2.admin_up = false;
    eth2.oper = Oper::Down;
    eth2.link = Link::Disabled;
    eth2.mtu = Some(9214);
    eth2.bandwidth_kbit = Some(1_000_000);
    eth2.duplex = Some(Duplex::Full);
    // No link and no forced speed, so there is no speed to report.
    eth2.speed_mbps = None;
    eth2.autoneg = Some(false);
    eth2.loopback_mode = Some("None".into());
    eth2.since = Some(12 * 86_400 + 4 * 3_600 + 40 * 60 + 51);
    eth2.rates = Some(rates(0.0, 0.0, 0.0, 0.0, 0.0, 0.0));
    eth2.counters = Some(clean(Counters::default()));

    let mut lo = Interface::new("lo", Kind::Loopback);
    lo.admin_up = true;
    lo.oper = Oper::Up;
    lo.link = Link::Connected;
    lo.mac = Some("00:00:00:00:00:00".into());
    lo.description = Some("Router-ID".into());
    lo.addresses = vec![address("10.255.0.1/32")];
    lo.mtu = Some(65535);
    lo.since = Some(12 * 86_400 + 4 * 3_600 + 41 * 60 + 10);

    let mut vlan10 = Interface::new("vlan10", Kind::Vlan);
    vlan10.admin_up = true;
    vlan10.oper = Oper::Up;
    vlan10.link = Link::Connected;
    // A VLAN wears its parent's address, which is why this is eth1's.
    vlan10.mac = Some("2c:dd:e9:12:00:a2".into());
    vlan10.bia = Some("2c:dd:e9:12:00:a2".into());
    vlan10.description = Some("LAN-USERS".into());
    vlan10.addresses = vec![address("10.20.10.1/24")];
    vlan10.mtu = Some(1500);
    vlan10.bandwidth_kbit = Some(1_000_000);
    vlan10.since = Some(12 * 86_400 + 4 * 3_600 + 31 * 60);

    let mut bond0 = Interface::new("bond0", Kind::PortChannel);
    bond0.admin_up = true;
    bond0.oper = Oper::Up;
    bond0.link = Link::Connected;
    bond0.mac = Some("2c:dd:e9:12:00:a4".into());
    bond0.bia = Some("2c:dd:e9:12:00:a4".into());
    bond0.description = Some("LAG to qs-hq-core".into());
    bond0.mtu = Some(9214);
    bond0.bandwidth_kbit = Some(20_000_000);
    bond0.since = Some(12 * 86_400 + 3 * 3_600 + 58 * 60 + 44);
    bond0.bond = Some(Bond {
        members: vec![
            BondMember {
                name: "eth3".into(),
                duplex: Some(Duplex::Full),
                speed_mbps: Some(10_000),
            },
            BondMember {
                name: "eth4".into(),
                duplex: Some(Duplex::Full),
                speed_mbps: Some(10_000),
            },
        ],
        fallback: Some("off".into()),
    });
    bond0.rates = Some(rates(210.0, 24_123.0, 1.1, 189.0, 21_877.0, 1.0));
    bond0.counters = Some(Counters {
        in_unicast: 84_812_734_120,
        in_octets: 101_278_449_021_837,
        in_multicast: 991_234,
        in_broadcast: 812,
        out_unicast: 78_123_911_223,
        out_octets: 91_234_098_123_441,
        out_multicast: 812_734,
        out_broadcast: 44,
        ..Counters::default()
    });

    snapshot(vec![eth0, eth1, eth2, lo, vlan10, bond0])
}

// ---------------------------------------------------------------------------
// show interfaces description
// ---------------------------------------------------------------------------

pub fn description() -> Snapshot {
    let described = |name: &str, kind: Kind, up: bool, text: Option<&str>| {
        let mut interface = Interface::new(name, kind);
        interface.admin_up = up;
        interface.oper = if up { Oper::Up } else { Oper::Down };
        interface.description = text.map(str::to_string);
        interface
    };

    snapshot(vec![
        described(
            "eth0",
            Kind::Ethernet,
            true,
            Some("WAN uplink to ISP - Circuit ID 4471-A"),
        ),
        described(
            "eth1",
            Kind::Ethernet,
            true,
            Some("LAN trunk to qs-hq-access1"),
        ),
        described("eth2", Kind::Ethernet, false, None),
        described("eth3", Kind::Ethernet, true, Some("LAG member bond0")),
        described("eth4", Kind::Ethernet, true, Some("LAG member bond0")),
        described("bond0", Kind::PortChannel, true, Some("LAG to qs-hq-core")),
        described("lo", Kind::Loopback, true, Some("Router-ID")),
        described("vlan10", Kind::Vlan, true, Some("LAN-USERS")),
    ])
}

// ---------------------------------------------------------------------------
// show interfaces status
// ---------------------------------------------------------------------------

pub fn status() -> Snapshot {
    let mut eth0 = port("eth0", "2c:dd:e9:12:00:a1");
    eth0.description = Some("WAN uplink to ISP - Circuit ID 4471-A".into());
    eth0.membership = Membership::Routed;
    eth0.duplex = Some(Duplex::Full);
    eth0.speed_mbps = Some(10_000);
    eth0.media_type = Some("10GBASE-SR".into());

    let mut eth1 = port("eth1", "2c:dd:e9:12:00:a2");
    eth1.description = Some("LAN trunk to qs-hq-access1".into());
    eth1.membership = Membership::Trunk;
    eth1.duplex = Some(Duplex::Full);
    eth1.speed_mbps = Some(1_000);
    eth1.media_type = Some("1000BASE-T".into());

    let mut eth2 = port("eth2", "2c:dd:e9:12:00:a3");
    eth2.admin_up = false;
    eth2.oper = Oper::Down;
    eth2.link = Link::Disabled;
    eth2.membership = Membership::Access(1);
    eth2.duplex = Some(Duplex::Full);
    eth2.speed_mbps = None;
    eth2.media_type = Some("1000BASE-T".into());

    let member = |name: &str, mac: &str| {
        let mut interface = port(name, mac);
        interface.description = Some("LAG member bond0".into());
        interface.membership = Membership::InBond("bond0".into());
        interface.member_of = Some("bond0".into());
        interface.duplex = Some(Duplex::Full);
        interface.speed_mbps = Some(10_000);
        interface.media_type = Some("10GBASE-CR".into());
        interface
    };

    let mut bond0 = Interface::new("bond0", Kind::PortChannel);
    bond0.admin_up = true;
    bond0.oper = Oper::Up;
    bond0.link = Link::Connected;
    bond0.description = Some("LAG to qs-hq-core".into());
    bond0.membership = Membership::Trunk;
    bond0.duplex = Some(Duplex::Full);
    bond0.speed_mbps = Some(20_000);

    snapshot(vec![
        eth0,
        eth1,
        eth2,
        member("eth3", "2c:dd:e9:12:00:a4"),
        member("eth4", "2c:dd:e9:12:00:a5"),
        bond0,
    ])
}

/// A port the box shut by itself.
pub fn errdisabled() -> Snapshot {
    let mut eth6 = port("eth6", "2c:dd:e9:12:00:a7");
    eth6.link = Link::ErrDisabled;
    eth6.oper = Oper::Down;
    eth6.description = None;
    eth6.errdisable_reason = Some("link-flap".into());
    snapshot(vec![eth6])
}

// ---------------------------------------------------------------------------
// show interfaces counters
// ---------------------------------------------------------------------------

/// The five physical ports, with counters and rates. Serves the totals, error,
/// discard and rate tables, which are four views of one set of numbers.
pub fn ports() -> Snapshot {
    let mut eth0 = port("eth0", "2c:dd:e9:12:00:a1");
    eth0.counters = Some(clean(Counters {
        out_discards: 2,
        ..traffic(
            (4_294_811_034, 4_816_030_792_344, 89_127, 15_234),
            (8_812_734_120, 11_278_449_021_837, 44_506, 1_287),
        )
    }));
    eth0.rates = Some(rates(24.7, 4_123.0, 0.2, 96.3, 9_877.0, 1.0));

    let mut eth1 = port("eth1", "2c:dd:e9:12:00:a2");
    eth1.counters = Some(traffic(
        (102_981_234, 90_238_471_234, 412_987, 88_123),
        (88_123_911, 71_234_098_123, 128_730, 9_812),
    ));
    eth1.rates = Some(rates(3.11, 1_204.0, 0.3, 1.02, 655.0, 0.1));

    let mut eth2 = port("eth2", "2c:dd:e9:12:00:a3");
    eth2.admin_up = false;
    eth2.oper = Oper::Down;
    eth2.link = Link::Disabled;
    eth2.counters = Some(clean(Counters::default()));
    eth2.rates = Some(rates(0.0, 0.0, 0.0, 0.0, 0.0, 0.0));

    // The one port with something wrong with it: a marginal 10G DAC. The
    // aggregate is the sum of the two specific counters plus nothing else.
    let mut eth3 = port("eth3", "2c:dd:e9:12:00:a4");
    eth3.counters = Some(clean(Counters {
        fcs_errors: Some(12),
        symbol_errors: Some(3),
        in_errors: 15,
        out_discards: 41_234,
        ..traffic(
            (48_123_911_223, 5_123_098_123_441, 412_334, 12),
            (39_123_911_223, 45_012_098_123_441, 406_877, 22),
        )
    }));
    eth3.rates = Some(rates(105.2, 12_100.0, 1.1, 94.4, 10_900.0, 0.9));

    let mut eth4 = port("eth4", "2c:dd:e9:12:00:a5");
    eth4.counters = Some(clean(Counters {
        out_discards: 40_997,
        ..traffic(
            (47_123_911_223, 5_012_098_123_441, 409_877, 10),
            (39_000_000_997, 46_222_049_021_837, 405_877, 22),
        )
    }));
    eth4.rates = Some(rates(104.8, 12_000.0, 1.0, 94.6, 11_000.0, 0.9));

    snapshot(vec![eth0, eth1, eth2, eth3, eth4])
}

/// One port with eight transmit queues.
pub fn queues() -> Snapshot {
    let counts: [(u64, u64, u64, u64); 8] = [
        (84_123_441, 81_234_981_234_412, 0, 0),
        (0, 0, 0, 0),
        (0, 0, 0, 0),
        (12_341_123, 9_812_734_412_334, 0, 0),
        (0, 0, 0, 0),
        (0, 0, 0, 0),
        (441_233, 4_412_334_981, 0, 0),
        (8_812_344, 88_123_449_812, 2, 3_028),
    ];
    let mut eth0 = port("eth0", "2c:dd:e9:12:00:a1");
    eth0.queues = counts
        .iter()
        .enumerate()
        .map(|(index, (packets, bytes, dropped, dropped_bytes))| Queue {
            name: format!("UC{index}"),
            packets: *packets,
            bytes: *bytes,
            dropped_packets: *dropped,
            dropped_bytes: *dropped_bytes,
        })
        .collect();
    snapshot(vec![eth0])
}

/// One port's RMON frame-size distribution.
pub fn bins() -> Snapshot {
    let mut eth0 = port("eth0", "2c:dd:e9:12:00:a1");
    eth0.bins = Some(Bins {
        received: [
            412_334_981,
            1_123_441_233,
            441_233_441,
            123_441_233,
            88_123_441,
            2_105_745_912,
            0,
        ],
        transmitted: [
            212_334_981,
            923_441_233,
            341_233_441,
            223_441_233,
            188_123_441,
            6_923_158_791,
            0,
        ],
    });
    snapshot(vec![eth0])
}

// ---------------------------------------------------------------------------
// show interfaces transceiver
// ---------------------------------------------------------------------------

fn measure(value: f64) -> Measure {
    Measure {
        value: Some(value),
        ..Measure::default()
    }
}

/// Three optics: the uplink and the two bond members. The copper ports have no
/// module and so are not in the snapshot at all.
pub fn transceivers() -> Snapshot {
    let optic = |name: &str, mac: &str, temp, volts, bias, rx, tx| {
        let mut interface = port(name, mac);
        interface.media_type = Some("10GBASE-SR".into());
        interface.transceiver = Some(Transceiver {
            media_type: Some("10GBASE-SR".into()),
            temperature: measure(temp),
            voltage: measure(volts),
            tx_bias: measure(bias),
            rx_power: measure(rx),
            tx_power: measure(tx),
            age: Some(4),
            ..Transceiver::default()
        });
        interface
    };

    snapshot(vec![
        optic("eth0", "2c:dd:e9:12:00:a1", 33.45, 3.28, 6.42, -2.35, -1.87),
        optic("eth3", "2c:dd:e9:12:00:a4", 29.11, 3.30, 7.01, -1.02, -0.95),
        optic("eth4", "2c:dd:e9:12:00:a5", 29.87, 3.29, 6.88, -1.11, -0.98),
    ])
}

fn bounded(value: f64, high_alarm: f64, high_warn: f64, low_alarm: f64, low_warn: f64) -> Measure {
    Measure {
        value: Some(value),
        high_alarm: Some(high_alarm),
        high_warn: Some(high_warn),
        low_alarm: Some(low_alarm),
        low_warn: Some(low_warn),
    }
}

/// One optic with its identity and every alarm and warning threshold.
pub fn transceiver_detail() -> Snapshot {
    let mut eth0 = port("eth0", "2c:dd:e9:12:00:a1");
    eth0.media_type = Some("10GBASE-SR".into());
    eth0.transceiver = Some(Transceiver {
        media_type: Some("10GBASE-SR".into()),
        vendor: Some("FINISAR CORP.".into()),
        part_number: Some("FTLX8574D3BCL".into()),
        serial_number: Some("UWM01B7".into()),
        date_code: Some("210412".into()),
        temperature: bounded(33.45, 75.0, 70.0, -5.0, 0.0),
        voltage: bounded(3.28, 3.63, 3.46, 2.97, 3.13),
        tx_bias: bounded(6.42, 11.80, 10.80, 4.0, 5.0),
        tx_power: bounded(-1.87, 1.70, -1.30, -9.50, -8.30),
        rx_power: bounded(-2.35, 2.00, -1.00, -13.10, -12.10),
        age: Some(4),
        ..Transceiver::default()
    });
    snapshot(vec![eth0])
}

/// The administrative and operational speed and duplex of one port.
pub fn transceiver_properties() -> Snapshot {
    let mut eth0 = port("eth0", "2c:dd:e9:12:00:a1");
    eth0.media_type = Some("10GBASE-SR".into());
    eth0.admin_speed_mbps = Some(10_000);
    eth0.admin_duplex = Some(Duplex::Full);
    eth0.speed_mbps = Some(10_000);
    eth0.duplex = Some(Duplex::Full);
    eth0.transceiver = Some(Transceiver {
        media_type: Some("10GBASE-SR".into()),
        ..Transceiver::default()
    });
    snapshot(vec![eth0])
}

/// The first two rows of an SFF-8472 A0 page: the identifier bytes and the
/// beginning of the vendor name.
pub fn eeprom() -> Snapshot {
    let mut eth0 = port("eth0", "2c:dd:e9:12:00:a1");
    eth0.transceiver = Some(Transceiver {
        pages: vec![EepromPage {
            name: "A0".into(),
            bytes: vec![
                0x03, 0x04, 0x07, 0x10, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x06, 0x67,
                0x00, 0x0a, 0x64, 0x00, 0x00, 0x00, 0x00, 0x46, 0x49, 0x4e, 0x49, 0x53, 0x41,
                0x52, 0x20, 0x43, 0x4f, 0x52, 0x50,
            ],
        }],
        ..Transceiver::default()
    });
    snapshot(vec![eth0])
}

// ---------------------------------------------------------------------------
// show interfaces capabilities
// ---------------------------------------------------------------------------

pub fn capabilities() -> Snapshot {
    let flowcontrol = || {
        vec![
            "rx-(off,on,desired)".to_string(),
            "tx-(off,on,desired)".to_string(),
        ]
    };

    let mut eth0 = port("eth0", "2c:dd:e9:12:00:a1");
    eth0.media_type = Some("10GBASE-SR".into());
    eth0.capabilities = Some(Capabilities {
        speed_duplex: vec!["1G/full".into(), "10G/full".into(), "auto".into()],
        flowcontrol: flowcontrol(),
    });

    let mut eth1 = port("eth1", "2c:dd:e9:12:00:a2");
    eth1.media_type = Some("1000BASE-T".into());
    eth1.capabilities = Some(Capabilities {
        speed_duplex: vec![
            "10M/half".into(),
            "10M/full".into(),
            "100M/half".into(),
            "100M/full".into(),
            "1G/full".into(),
            "auto".into(),
        ],
        flowcontrol: flowcontrol(),
    });

    Snapshot {
        interfaces: vec![eth0, eth1],
        system: System {
            model: Some("NS-FW-1U-8X10G".into()),
            ..System::default()
        },
    }
}

// ---------------------------------------------------------------------------
// show interfaces flowcontrol
// ---------------------------------------------------------------------------

pub fn flowcontrol() -> Snapshot {
    let with = |name: &str, mac: &str, admin: &str, oper: &str, rx: u64, tx: u64| {
        let mut interface = port(name, mac);
        interface.flow_control = Some(FlowControl {
            send_admin: admin.into(),
            send_oper: oper.into(),
            receive_admin: admin.into(),
            receive_oper: oper.into(),
            rx_pause: rx,
            tx_pause: tx,
        });
        interface
    };

    snapshot(vec![
        with("eth0", "2c:dd:e9:12:00:a1", "off", "off", 0, 0),
        with("eth1", "2c:dd:e9:12:00:a2", "off", "off", 0, 0),
        with("eth3", "2c:dd:e9:12:00:a4", "desired", "on", 12, 0),
        with("eth4", "2c:dd:e9:12:00:a5", "desired", "on", 9, 0),
    ])
}

// ---------------------------------------------------------------------------
// show interfaces negotiation
// ---------------------------------------------------------------------------

fn copper_advertisement() -> Advertisement {
    Advertisement {
        speed_duplex: vec![
            "10M/half".into(),
            "10M/full".into(),
            "100M/half".into(),
            "100M/full".into(),
            "1G/full".into(),
        ],
        pause: Some("None".into()),
    }
}

/// A fibre port that does not negotiate, and a copper one that does.
pub fn negotiation() -> Snapshot {
    let eth0 = port("eth0", "2c:dd:e9:12:00:a1");

    let mut eth1 = port("eth1", "2c:dd:e9:12:00:a2");
    eth1.negotiation = Some(Negotiation {
        mode: "802.3".into(),
        status: "success".into(),
        local: copper_advertisement(),
        ..Negotiation::default()
    });

    snapshot(vec![eth0, eth1])
}

/// Both advertisements and what they resolved to.
pub fn negotiation_detail() -> Snapshot {
    let mut eth1 = port("eth1", "2c:dd:e9:12:00:a2");
    eth1.negotiation = Some(Negotiation {
        mode: "802.3".into(),
        status: "success".into(),
        local: copper_advertisement(),
        partner: Some(Advertisement {
            pause: Some("Symmetric".into()),
            ..copper_advertisement()
        }),
        resolution: Some(Advertisement {
            speed_duplex: vec!["1G/full".into()],
            pause: None,
        }),
        resolved_pause: Some("rx off, tx off".into()),
    });
    snapshot(vec![eth1])
}

// ---------------------------------------------------------------------------
// show interfaces phy
// ---------------------------------------------------------------------------

pub fn phy() -> Snapshot {
    let mut eth0 = port("eth0", "2c:dd:e9:12:00:a1");
    eth0.phy = Some(Phy {
        state: Some("linkUp".into()),
        interface_state: Some("up".into()),
        hw_resets: Some(1),
        transceiver: Some("10GBASE-SR".into()),
        transceiver_serial: Some("UWM01B7".into()),
        oper_speed: Some("10Gbps".into()),
        interrupt_count: Some(4),
        diags_mode: Some("normalOperation".into()),
        model: Some("NS-PHY-BCM84891".into()),
        reset_count: Some(1),
        state_changes: Some(2),
        last_change: Some(UPTIME),
        configured_speed: Some("10Gfull".into()),
        autoneg: Some("off".into()),
    });

    Snapshot {
        interfaces: vec![eth0],
        system: System {
            time: Some("Thu Aug 20 14:02:11 2026".into()),
            ..System::default()
        },
    }
}

// ---------------------------------------------------------------------------
// show interfaces mac
// ---------------------------------------------------------------------------

pub fn macs() -> Snapshot {
    let addresses = [
        "2c:dd:e9:12:00:a1",
        "2c:dd:e9:12:00:a2",
        "2c:dd:e9:12:00:a3",
        "2c:dd:e9:12:00:a4",
        "2c:dd:e9:12:00:a5",
    ];
    let interfaces = addresses
        .iter()
        .enumerate()
        .map(|(index, mac)| {
            let mut interface = port(&format!("eth{index}"), mac);
            if index == 2 {
                interface.admin_up = false;
                interface.oper = Oper::Down;
                interface.link = Link::Disabled;
            }
            interface
        })
        .collect();
    snapshot(interfaces)
}

/// One port's MAC layer, down to the FEC codewords.
pub fn mac_detail() -> Snapshot {
    let mut eth0 = port("eth0", "2c:dd:e9:12:00:a1");
    eth0.mac_layer = Some(MacLayer {
        state: "linkUp".into(),
        local_fault: Some(false),
        remote_fault: Some(false),
        fec_mode: Some("Disabled".into()),
        fec_corrected: Some(0),
        fec_uncorrected: Some(0),
    });
    snapshot(vec![eth0])
}
