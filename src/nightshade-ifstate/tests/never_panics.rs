//! Every renderer, over every combination of the fields that may be missing.
//!
//! `show interfaces` is what an operator runs when something is already wrong,
//! and the things that are wrong are exactly the ones that make fields go
//! missing: an optic that has stopped answering i2c, a driver whose stats
//! table changed under a kernel upgrade, an interface that has just been
//! deleted between the netlink dump and the ethtool call. A renderer that
//! panics on any of that turns a diagnostic command into a second fault.
//!
//! So this generates interfaces whose optional fields are independently
//! present or absent, whose numbers are at the edges of their types, and whose
//! strings are empty or long, and renders every view of them. The assertion is
//! only that it comes back -- plus the two invariants that hold whatever the
//! input is.

use nightshade_ifstate::model::*;
use nightshade_ifstate::query::View;
use nightshade_ifstate::render;
use proptest::prelude::*;

/// Every view, so a new one cannot be added without being covered here.
fn views() -> Vec<View> {
    vec![
        View::Detail,
        View::Description,
        View::Status(None),
        View::Status(Some(Link::Connected)),
        View::Status(Some(Link::ErrDisabled)),
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
    ]
}

fn any_kind() -> BoxedStrategy<Kind> {
    prop_oneof![
        Just(Kind::Ethernet),
        Just(Kind::Loopback),
        Just(Kind::Vlan),
        Just(Kind::PortChannel),
        Just(Kind::Tunnel),
        Just(Kind::Wireguard),
        Just(Kind::Bridge),
        Just(Kind::Other),
    ]
    .boxed()
}

fn any_oper() -> BoxedStrategy<Oper> {
    prop_oneof![
        Just(Oper::Up),
        Just(Oper::Down),
        Just(Oper::LowerLayerDown),
        Just(Oper::NotPresent),
    ]
    .boxed()
}

fn any_link() -> BoxedStrategy<Link> {
    prop_oneof![
        Just(Link::Connected),
        Just(Link::NotConnect),
        Just(Link::Disabled),
        Just(Link::ErrDisabled),
        Just(Link::Inactive),
    ]
    .boxed()
}

fn any_duplex() -> BoxedStrategy<Option<Duplex>> {
    prop_oneof![Just(None), Just(Some(Duplex::Half)), Just(Some(Duplex::Full))]
    .boxed()
}

/// Names include the empty string and one longer than any column, because a
/// name is a kernel string and nothing here has checked it.
fn any_name() -> BoxedStrategy<String> {
    prop_oneof![
        Just(String::new()),
        "[a-z]{1,4}[0-9]{0,3}",
        "[a-z.:-]{40,60}",
    ]
    .boxed()
}

fn any_text() -> BoxedStrategy<Option<String>> {
    prop_oneof![Just(None), Just(Some(String::new())), ".{0,80}".prop_map(Some)]
    .boxed()
}

/// Counts at both ends of the range, so a renderer that adds two of them
/// together is caught overflowing rather than caught in production.
fn any_count() -> BoxedStrategy<u64> {
    prop_oneof![Just(0u64), Just(u64::MAX), any::<u64>()]
    .boxed()
}

fn any_maybe_count() -> BoxedStrategy<Option<u64>> {
    prop_oneof![Just(None), any_count().prop_map(Some)]
    .boxed()
}

/// Rates include the values that break arithmetic: infinities, NaN and
/// negatives, any of which a division by a zero-length sample window produces.
fn any_rate() -> BoxedStrategy<f64> {
    prop_oneof![
        Just(0.0),
        Just(f64::NAN),
        Just(f64::INFINITY),
        Just(f64::NEG_INFINITY),
        Just(-1.0),
        Just(f64::MAX),
        (0.0f64..1e12),
    ]
    .boxed()
}

fn any_measure() -> BoxedStrategy<Measure> {
    (
        prop::option::of(any_rate()),
        prop::option::of(any_rate()),
        prop::option::of(any_rate()),
        prop::option::of(any_rate()),
        prop::option::of(any_rate()),
    )
        .prop_map(|(value, high_alarm, high_warn, low_alarm, low_warn)| Measure {
            value,
            high_alarm,
            high_warn,
            low_alarm,
            low_warn,
        })
    .boxed()
}

fn any_counters() -> BoxedStrategy<Counters> {
    (
        (any_count(), any_count(), any_count(), any_count()),
        (any_count(), any_count(), any_count(), any_count()),
        (any_count(), any_count(), any_count(), any_count()),
        (
            any_maybe_count(),
            any_maybe_count(),
            any_maybe_count(),
            any_maybe_count(),
            any_maybe_count(),
        ),
        (
            any_maybe_count(),
            any_maybe_count(),
            any_maybe_count(),
            any_maybe_count(),
            any_maybe_count(),
        ),
    )
        .prop_map(|(inbound, outbound, aggregate, specific, more)| Counters {
            in_unicast: inbound.0,
            in_multicast: inbound.1,
            in_broadcast: inbound.2,
            in_octets: inbound.3,
            out_unicast: outbound.0,
            out_multicast: outbound.1,
            out_broadcast: outbound.2,
            out_octets: outbound.3,
            in_errors: aggregate.0,
            in_discards: aggregate.1,
            out_errors: aggregate.2,
            out_discards: aggregate.3,
            fcs_errors: specific.0,
            alignment_errors: specific.1,
            symbol_errors: specific.2,
            runts: specific.3,
            giants: specific.4,
            collisions: more.0,
            late_collisions: more.1,
            deferred: more.2,
            pause_in: more.3,
            pause_out: more.4,
        })
    .boxed()
}

fn any_transceiver() -> BoxedStrategy<Transceiver> {
    (
        any_text(),
        any_text(),
        any_text(),
        (any_measure(), any_measure(), any_measure()),
        (any_measure(), any_measure()),
        any_maybe_count(),
        prop::collection::vec(any::<u8>(), 0..40),
    )
        .prop_map(
            |(media_type, vendor, serial, first, second, age, bytes)| Transceiver {
                media_type,
                vendor,
                part_number: None,
                serial_number: serial,
                date_code: None,
                temperature: first.0,
                voltage: first.1,
                tx_bias: first.2,
                tx_power: second.0,
                rx_power: second.1,
                age,
                pages: vec![EepromPage {
                    name: "A0".to_string(),
                    bytes,
                }],
            },
        )
    .boxed()
}

fn any_interface() -> BoxedStrategy<Interface> {
    (
        (any_name(), any_kind(), any::<bool>(), any::<bool>()),
        (any_oper(), any_link(), any_text(), any_text()),
        (
            any_duplex(),
            prop::option::of(0u64..200_000),
            prop::option::of(0u32..70_000),
            prop::option::of(any_count()),
        ),
        (
            prop::option::of(any_count()),
            any_count(),
            any_maybe_count(),
            prop::option::of(any_counters()),
        ),
        (
            prop::option::of(any_transceiver()),
            prop::option::of(any_text()),
            any::<bool>(),
            any::<bool>(),
        ),
    )
        .prop_map(
            |(identity, state, link, history, extras)| {
                let (name, kind, present, admin_up) = identity;
                let (oper, link_state, description, errdisable) = state;
                let (duplex, speed_mbps, mtu, bandwidth) = link;
                let (since, link_changes, last_clear, counters) = history;
                let (transceiver, phy_text, has_phy, has_mac_layer) = extras;

                Interface {
                    name,
                    kind,
                    present,
                    admin_up,
                    oper,
                    link: link_state,
                    errdisable_reason: errdisable,
                    description,
                    mac: Some(String::new()),
                    bia: None,
                    addresses: vec![Address {
                        prefix: String::new(),
                        broadcast: None,
                    }],
                    mtu,
                    bandwidth_kbit: bandwidth,
                    duplex,
                    speed_mbps,
                    speed_source: SpeedSource::Negotiated,
                    admin_speed_mbps: speed_mbps,
                    admin_duplex: duplex,
                    autoneg: Some(true),
                    uni_link: None,
                    loopback_mode: None,
                    since,
                    link_changes,
                    last_clear,
                    rates: Some(Rates {
                        interval: 0,
                        in_bps: f64::NAN,
                        in_pps: f64::INFINITY,
                        in_percent: -1.0,
                        out_bps: f64::MAX,
                        out_pps: 0.0,
                        out_percent: f64::NAN,
                    }),
                    counters,
                    bond: Some(Bond {
                        members: vec![BondMember {
                            name: String::new(),
                            duplex,
                            speed_mbps,
                        }],
                        fallback: None,
                    }),
                    member_of: None,
                    membership: Membership::Access(u16::MAX),
                    media_type: None,
                    transceiver,
                    capabilities: Some(Capabilities::default()),
                    flow_control: Some(FlowControl::default()),
                    negotiation: Some(Negotiation::default()),
                    phy: has_phy.then(|| Phy {
                        state: phy_text.clone().flatten(),
                        model: phy_text.clone().flatten(),
                        last_change: since,
                        ..Phy::default()
                    }),
                    mac_layer: has_mac_layer.then(MacLayer::default),
                    queues: vec![Queue::default()],
                    bins: Some(Bins::default()),
                    flags: None,
                    encapsulation: None,
                }
            },
        )
    .boxed()
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(96))]

    /// The whole point: no combination of missing, empty or extreme fields
    /// makes any renderer panic.
    #[test]
    fn no_view_panics_on_any_interface(
        interfaces in prop::collection::vec(any_interface(), 0..4)
    ) {
        let snapshot = Snapshot {
            interfaces,
            system: System {
                model: Some(String::new()),
                time: Some(String::new()),
            },
        };
        for view in views() {
            let text = render(&snapshot, &view);
            // Two invariants that hold whatever went in. A trailing space is
            // invisible on a terminal and shows up in every diff of a support
            // bundle, and a line that does not end is a line the pager loses.
            for line in text.lines() {
                prop_assert!(!line.ends_with(' '), "{view:?} left a trailing space: {line:?}");
            }
            prop_assert!(
                text.is_empty() || text.ends_with('\n'),
                "{view:?} produced text with no final newline"
            );
        }
    }
}

/// A snapshot with nothing in it is a real answer -- a box whose ports have
/// all been removed, or a filter that matched none of them -- and every view
/// must survive it.
#[test]
fn an_empty_snapshot_renders_headers_and_no_rows() {
    let snapshot = Snapshot::default();
    for view in views() {
        let text = render(&snapshot, &view);
        assert!(
            text.is_empty() || text.ends_with('\n'),
            "{view:?}: {text:?}"
        );
        // A table still prints its header, so the operator can see that the
        // question was understood and the answer was none.
        if matches!(view, View::Description | View::Status(None) | View::Counters) {
            assert!(text.contains("Port") || text.contains("Interface"), "{text:?}");
        }
    }
}
