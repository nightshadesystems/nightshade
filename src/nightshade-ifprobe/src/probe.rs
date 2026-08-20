//! Putting one interface together from four places.
//!
//! The kernel says what the device is and what has gone through it. The driver
//! says what the hardware can do. The module says what light is coming out of
//! it. The configuration says what any of it was meant to be, and supplies the
//! two things the kernel has no concept of: a description, and a load interval
//! to average the rates over.
//!
//! # Configured and absent
//!
//! An interface the configuration names and the kernel does not have is not
//! left out. It is reported with `line protocol is notpresent`, which is the
//! single most useful line `show interfaces` can produce -- a port that was
//! renamed, a card that did not come up, a bond whose members all went away.

use std::collections::{BTreeMap, BTreeSet};

use nightshade_common::paths::Paths;
use nightshade_ifstate::model::*;
use nightshade_ifstate::query::Query;
use nightshade_ifstate::render::kind_from_name;
use nightshade_ifstate::units;
use nightshade_schema::config::{ConfigTree, Node};
use nightshade_schema::path::Path;

use crate::driverstats::{self, Statistics};
use crate::ethtool::{self, Ethtool, LinkSettings};
use crate::netlink::{self, Link as KernelLink, Stats};
use crate::sysfs::{self, SysFs};
use crate::tracker::Tracker;

/// The interface types the schema has, and what each becomes in the model.
const CONFIGURED_KINDS: &[(&str, Kind)] = &[
    ("loopback", Kind::Loopback),
    ("ethernet", Kind::Ethernet),
    ("bonding", Kind::PortChannel),
    ("bridge", Kind::Bridge),
    ("vlan", Kind::Vlan),
    ("vxlan", Kind::Tunnel),
];

/// The load interval when the configuration does not set one.
pub const DEFAULT_LOAD_INTERVAL: u32 = 300;

/// `PORT_TP` and `PORT_FIBRE` from `ethtool.h`.
const PORT_TWISTED_PAIR: u8 = 0x00;
const PORT_DIRECT_ATTACH: u8 = 0x05;

/// Everything read from the kernel in one pass, before the configuration is
/// laid over it.
struct Kernel {
    links: Vec<KernelLink>,
    /// Addresses by interface index.
    addresses: BTreeMap<u32, Vec<netlink::Address>>,
}

/// What `show interfaces` is answered from.
pub struct Probe {
    paths: Paths,
    sysfs: SysFs,
    /// Absent on a kernel with no `AF_INET` socket to hang ioctls off, which
    /// is a situation that exists (a build container) and is not fatal: the
    /// sysfs fallbacks still answer.
    ethtool: Option<Ethtool>,
}

impl Probe {
    pub fn new(paths: Paths) -> Self {
        let sysfs = SysFs::new(paths.sys_class_net());
        Self {
            paths,
            sysfs,
            ethtool: Ethtool::open().ok(),
        }
    }

    /// One round of counters, for the tracker.
    ///
    /// Cheap on purpose: a netlink dump and nothing else. Anything that costs
    /// an ioctl per port is done when somebody asks a question, not every five
    /// seconds forever.
    pub fn links(&self) -> std::io::Result<Vec<KernelLink>> {
        let mut socket = netlink::Socket::open()?;
        socket.set_timeout(5)?;
        socket.links()
    }

    /// Build the answer to one `show interfaces ...`.
    pub fn snapshot(&self, query: &Query, running: &ConfigTree, tracker: &Tracker, now: u64) -> Snapshot {
        let kernel = self.read_kernel();
        let configured = configured_interfaces(running);
        let vlan_parents = self.vlan_parents(&kernel);

        let wanted: Option<BTreeSet<&str>> = (!query.names.is_empty())
            .then(|| query.names.iter().map(String::as_str).collect());

        let mut interfaces = Vec::new();
        let mut seen = BTreeSet::new();

        for link in &kernel.links {
            if wanted.as_ref().is_some_and(|names| !names.contains(link.name.as_str())) {
                continue;
            }
            seen.insert(link.name.clone());
            interfaces.push(self.build(link, &kernel, &configured, &vlan_parents, query, tracker, now));
        }

        // Configured, and the kernel does not have it.
        for (name, settings) in &configured {
            if seen.contains(name) {
                continue;
            }
            if wanted.as_ref().is_some_and(|names| !names.contains(name.as_str())) {
                continue;
            }
            interfaces.push(absent(name, settings));
        }

        Snapshot {
            interfaces,
            system: System {
                model: self.model(running),
                time: None,
            },
        }
    }

    fn read_kernel(&self) -> Kernel {
        let mut links = Vec::new();
        let mut addresses: BTreeMap<u32, Vec<netlink::Address>> = BTreeMap::new();

        if let Ok(mut socket) = netlink::Socket::open() {
            let _ = socket.set_timeout(5);
            links = socket.links().unwrap_or_default();
            for address in socket.addresses().unwrap_or_default() {
                addresses.entry(address.index).or_default().push(address);
            }
        }

        // Netlink not answering is not a reason to say the box has no
        // interfaces. sysfs knows their names, and everything read from it
        // below is per-name rather than per-index.
        if links.is_empty() {
            links = self
                .sysfs
                .names()
                .into_iter()
                .map(|name| self.link_from_sysfs(&name))
                .collect();
        }

        Kernel { links, addresses }
    }

    /// A link built from `/sys` alone, for when netlink is unavailable.
    fn link_from_sysfs(&self, name: &str) -> KernelLink {
        let operstate = match self.sysfs.operstate(name).as_deref() {
            Some("up") => netlink::IF_OPER_UP,
            Some("down") => netlink::IF_OPER_DOWN,
            Some("lowerlayerdown") => netlink::IF_OPER_LOWERLAYERDOWN,
            Some("notpresent") => netlink::IF_OPER_NOTPRESENT,
            Some("dormant") => netlink::IF_OPER_DORMANT,
            _ => 0,
        };
        KernelLink {
            name: name.to_string(),
            // sysfs does not publish IFF_UP, so an interface with a carrier is
            // taken to be administratively up. Wrong only for a port that is
            // shut with something still plugged into it, which cannot have a
            // carrier.
            flags: if operstate == netlink::IF_OPER_UP { 1 } else { 0 },
            mtu: self.sysfs.mtu(name),
            address: self
                .sysfs
                .address(name)
                .and_then(|text| parse_mac(&text)),
            operstate,
            ..KernelLink::default()
        }
    }

    /// Which interfaces are the parent of a VLAN, for the `trunk` cell.
    fn vlan_parents(&self, kernel: &Kernel) -> BTreeSet<String> {
        kernel
            .links
            .iter()
            .filter(|link| kind_from_name(&link.name, link.kind.as_deref()) == Kind::Vlan)
            .filter_map(|link| self.sysfs.vlan_parent(&link.name))
            .collect()
    }

    /// The platform's name for itself: configured if set, DMI if not.
    fn model(&self, running: &ConfigTree) -> Option<String> {
        leaf(running, &["system", "platform-model"])
            .or_else(|| sysfs::dmi_model(self.paths.root()))
    }

    #[allow(clippy::too_many_arguments)]
    fn build(
        &self,
        link: &KernelLink,
        kernel: &Kernel,
        configured: &BTreeMap<String, Settings>,
        vlan_parents: &BTreeSet<String>,
        query: &Query,
        tracker: &Tracker,
        now: u64,
    ) -> Interface {
        let name = link.name.as_str();
        let settings = configured.get(name).cloned().unwrap_or_default();
        let kind = settings
            .kind
            .unwrap_or_else(|| kind_from_name(name, link.kind.as_deref()));

        let statistics = (kind.is_physical() && query.view.needs_driver_stats())
            .then(|| self.ethtool.as_ref().and_then(|e| e.statistics(name)))
            .flatten()
            .unwrap_or_default();

        let link_settings = kind
            .is_physical()
            .then(|| self.ethtool.as_ref().and_then(|e| e.link_settings(name)))
            .flatten();

        let module = kind
            .is_physical()
            .then(|| self.module(name, query.view.needs_eeprom()))
            .flatten();

        // The baseline `clear counters` recorded, subtracted from everything
        // the kernel reports from here on.
        let stats = link.stats.map(|stats| match tracker.baseline(name) {
            Some(baseline) => baseline.applied(&stats),
            None => stats,
        });

        let speed_mbps = link_settings
            .as_ref()
            .and_then(|settings| settings.speed_mbps)
            .or_else(|| self.sysfs.speed(name))
            .or_else(|| self.bond_speed(name, kernel));

        let duplex = link_settings
            .as_ref()
            .and_then(|settings| settings.duplex)
            .or_else(|| self.sysfs.duplex(name))
            .map(|full| if full { Duplex::Full } else { Duplex::Half });

        let autoneg = link_settings.as_ref().map(|settings| settings.autoneg);
        let member_of = self.sysfs.master(name);
        let addresses = kernel
            .addresses
            .get(&link.index)
            .map(|found| {
                found
                    .iter()
                    .map(|address| Address {
                        prefix: address.prefix.clone(),
                        broadcast: address.broadcast.clone(),
                    })
                    .collect()
            })
            .unwrap_or_default();

        let mac = link
            .address
            .as_ref()
            .map(|bytes| units::mac(bytes))
            .or_else(|| self.sysfs.address(name));
        let bia = link
            .permanent_address
            .as_ref()
            .map(|bytes| units::mac(bytes))
            .or_else(|| {
                self.ethtool
                    .as_ref()
                    .and_then(|e| e.permanent_address(name))
                    .map(|bytes| units::mac(&bytes))
            })
            .or_else(|| mac.clone());

        let admin_up = link.admin_up() && !settings.disabled;
        let oper = operstate(link.operstate);
        let media_type = module
            .as_ref()
            .and_then(|module| module.media_type.clone())
            .or_else(|| copper_media_type(link_settings.as_ref(), speed_mbps));

        let load_interval = settings.load_interval.unwrap_or(DEFAULT_LOAD_INTERVAL);

        Interface {
            name: name.to_string(),
            kind,
            present: true,
            admin_up,
            oper,
            link: link_state(admin_up, oper, member_of.is_some()),
            errdisable_reason: None,
            description: settings.description.clone(),
            mac: settings.mac.clone().or_else(|| mac.clone()),
            bia,
            addresses,
            mtu: link.mtu.or_else(|| self.sysfs.mtu(name)),
            bandwidth_kbit: speed_mbps.map(|speed| speed * 1_000),
            duplex,
            speed_mbps,
            speed_source: if autoneg == Some(true) && settings.speed.is_none() {
                SpeedSource::Negotiated
            } else {
                SpeedSource::Forced
            },
            admin_speed_mbps: settings.speed,
            admin_duplex: settings.duplex,
            autoneg,
            uni_link: None,
            // Linux exposes no MAC-level loopback control, so a port is never
            // in one. The line is kept because its absence would read as "this
            // box cannot tell you", which is not the case.
            loopback_mode: kind.is_physical().then(|| "None".to_string()),
            since: tracker.since(name, now),
            link_changes: tracker.history(name).map(|h| h.changes).unwrap_or(0),
            last_clear: tracker.last_clear(name, now),
            rates: tracker.rates(name, load_interval, speed_mbps),
            counters: stats.map(|stats| counters(&stats, &statistics)),
            bond: (kind == Kind::PortChannel).then(|| self.bond(name, kernel)),
            member_of: member_of.clone(),
            membership: membership(
                kind,
                member_of.as_deref(),
                &kernel.addresses,
                link.index,
                vlan_parents.contains(name),
            ),
            media_type: media_type.clone(),
            transceiver: module,
            capabilities: link_settings.as_ref().map(capabilities),
            flow_control: self.flow_control(name, &statistics),
            negotiation: link_settings.as_ref().map(|settings| negotiation(settings, oper)),
            phy: kind.is_physical().then(|| {
                self.phy(name, link, tracker, now, speed_mbps, &media_type, &settings)
            }),
            mac_layer: kind.is_physical().then(|| self.mac_layer(name, admin_up, oper, &statistics)),
            queues: driverstats::queues(&statistics),
            bins: driverstats::bins(&statistics),
            flags: None,
            encapsulation: None,
        }
    }

    /// The module, read only as deeply as the command needs.
    fn module(&self, name: &str, with_eeprom: bool) -> Option<Transceiver> {
        let module = self.ethtool.as_ref()?.module(name)?;
        let mut decoded = crate::sff::decode(module.kind, &module.bytes);
        // The DOM values were read a moment ago, which is what `Last Update`
        // means -- Linux has no cached copy with an age of its own.
        decoded.age = Some(0);
        if !with_eeprom {
            decoded.pages.clear();
        }
        Some(decoded)
    }

    fn bond(&self, name: &str, kernel: &Kernel) -> Bond {
        let members = self
            .sysfs
            .bond_members(name)
            .into_iter()
            .map(|member| {
                let link = kernel.links.iter().find(|link| link.name == member);
                BondMember {
                    duplex: self
                        .sysfs
                        .duplex(&member)
                        .map(|full| if full { Duplex::Full } else { Duplex::Half }),
                    speed_mbps: self.sysfs.speed(&member).or_else(|| {
                        link.and_then(|_| self.ethtool.as_ref()?.link_settings(&member)?.speed_mbps)
                    }),
                    name: member,
                }
            })
            .collect();
        Bond {
            members,
            fallback: self.sysfs.bond_fallback(name),
        }
    }

    /// A bond has no speed of its own; it has the sum of what is up in it.
    fn bond_speed(&self, name: &str, _kernel: &Kernel) -> Option<u64> {
        let members = self.sysfs.bond_members(name);
        if members.is_empty() {
            return None;
        }
        let total: u64 = members
            .iter()
            .filter_map(|member| self.sysfs.speed(member))
            .sum();
        (total > 0).then_some(total)
    }

    fn flow_control(&self, name: &str, statistics: &Statistics) -> Option<FlowControl> {
        let pause = self.ethtool.as_ref()?.pause(name)?;
        // `desired` is EOS's word for "negotiate it", which is what the
        // autoneg bit on the pause parameters means.
        let admin = |on: bool| {
            if pause.autoneg {
                "desired".to_string()
            } else if on {
                "on".to_string()
            } else {
                "off".to_string()
            }
        };
        let oper = |on: bool| if on { "on" } else { "off" }.to_string();
        Some(FlowControl {
            send_admin: admin(pause.tx),
            send_oper: oper(pause.tx),
            receive_admin: admin(pause.rx),
            receive_oper: oper(pause.rx),
            rx_pause: driverstats::total(statistics, driverstats::PAUSE_IN).unwrap_or(0),
            tx_pause: driverstats::total(statistics, driverstats::PAUSE_OUT).unwrap_or(0),
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn phy(
        &self,
        name: &str,
        link: &KernelLink,
        tracker: &Tracker,
        now: u64,
        speed_mbps: Option<u64>,
        media_type: &Option<String>,
        settings: &Settings,
    ) -> Phy {
        let history = tracker.history(name);
        Phy {
            state: Some(
                if Tracker::is_up(link.operstate) {
                    "linkUp"
                } else if link.admin_up() {
                    "linkDown"
                } else {
                    "phyOff"
                }
                .to_string(),
            ),
            interface_state: Some(operstate(link.operstate).label().to_string()),
            transceiver: media_type.clone(),
            transceiver_serial: self
                .ethtool
                .as_ref()
                .and_then(|e| e.module(name))
                .and_then(|module| crate::sff::decode(module.kind, &module.bytes).serial_number),
            oper_speed: speed_mbps.map(units::speed_phy),
            state_changes: history.map(|history| history.changes),
            last_change: tracker.since(name, now),
            configured_speed: settings
                .speed
                .map(|speed| units::speed_duplex_word(speed, settings.duplex)),
            autoneg: self
                .ethtool
                .as_ref()
                .and_then(|e| e.link_settings(name))
                .map(|settings| if settings.autoneg { "on" } else { "off" }.to_string()),
            // Reset counts, interrupt counts, diagnostic mode and the PHY's
            // own part number are read over MDIO by a vendor SDK. Linux
            // exposes none of them, so the rows are left out rather than
            // filled with a plausible zero.
            hw_resets: None,
            interrupt_count: None,
            diags_mode: None,
            model: None,
            reset_count: None,
        }
    }

    fn mac_layer(&self, name: &str, admin_up: bool, oper: Oper, statistics: &Statistics) -> MacLayer {
        let fec = self.ethtool.as_ref().and_then(|e| e.fec(name));
        MacLayer {
            state: if !admin_up {
                "phyOff".to_string()
            } else if oper == Oper::Up {
                "linkUp".to_string()
            } else {
                "linkDown".to_string()
            },
            local_fault: driverstats::lookup(statistics, driverstats::LOCAL_FAULT)
                .map(|count| count != 0),
            remote_fault: driverstats::lookup(statistics, driverstats::REMOTE_FAULT)
                .map(|count| count != 0),
            fec_mode: fec.map(|fec| ethtool::fec_name(fec).to_string()),
            fec_corrected: driverstats::lookup(statistics, driverstats::FEC_CORRECTED),
            fec_uncorrected: driverstats::lookup(statistics, driverstats::FEC_UNCORRECTED),
        }
    }
}

// -- the pieces -------------------------------------------------------------

fn operstate(raw: u8) -> Oper {
    match raw {
        netlink::IF_OPER_UP => Oper::Up,
        netlink::IF_OPER_LOWERLAYERDOWN => Oper::LowerLayerDown,
        netlink::IF_OPER_NOTPRESENT => Oper::NotPresent,
        _ => Oper::Down,
    }
}

/// The word in brackets on the state line.
fn link_state(admin_up: bool, oper: Oper, enslaved: bool) -> Link {
    if !admin_up {
        Link::Disabled
    } else if oper == Oper::Up {
        Link::Connected
    } else if enslaved {
        // A member the bond has not taken into service is present and not
        // carrying anything, which is what `inactive` means.
        Link::Inactive
    } else {
        Link::NotConnect
    }
}

/// What the `Vlan` column says.
fn membership(
    kind: Kind,
    member_of: Option<&str>,
    addresses: &BTreeMap<u32, Vec<netlink::Address>>,
    index: u32,
    carries_vlans: bool,
) -> Membership {
    if let Some(bond) = member_of {
        return Membership::InBond(bond.to_string());
    }
    if addresses.get(&index).is_some_and(|found| !found.is_empty()) {
        return Membership::Routed;
    }
    if carries_vlans || matches!(kind, Kind::PortChannel | Kind::Bridge) {
        return Membership::Trunk;
    }
    if kind.is_physical() {
        // An untagged port nothing has claimed is in the default VLAN, which
        // is what a switch would say about it.
        return Membership::Access(1);
    }
    Membership::Unknown
}

/// The kernel's counters, plus whatever the driver adds to them.
fn counters(stats: &Stats, statistics: &Statistics) -> Counters {
    let multicast = driverstats::lookup(statistics, driverstats::RX_MULTICAST)
        .unwrap_or(stats.multicast);
    let broadcast = driverstats::lookup(statistics, driverstats::RX_BROADCAST).unwrap_or(0);

    Counters {
        // `rx_packets` counts everything. The `packets input` line and the
        // `InUcastPkts` column are both the unicast total, so the two kinds
        // the kernel counts separately come off here rather than in two
        // different renderers.
        in_unicast: stats
            .rx_packets
            .saturating_sub(multicast)
            .saturating_sub(broadcast),
        in_multicast: multicast,
        in_broadcast: broadcast,
        in_octets: stats.rx_bytes,
        out_unicast: stats
            .tx_packets
            .saturating_sub(driverstats::lookup(statistics, driverstats::TX_MULTICAST).unwrap_or(0))
            .saturating_sub(driverstats::lookup(statistics, driverstats::TX_BROADCAST).unwrap_or(0)),
        out_multicast: driverstats::lookup(statistics, driverstats::TX_MULTICAST).unwrap_or(0),
        out_broadcast: driverstats::lookup(statistics, driverstats::TX_BROADCAST).unwrap_or(0),
        out_octets: stats.tx_bytes,
        in_errors: stats.rx_errors,
        in_discards: stats.rx_dropped,
        out_errors: stats.tx_errors,
        out_discards: stats.tx_dropped,
        // The kernel keeps three of these itself; the rest are the driver's,
        // and are absent when it does not keep them.
        fcs_errors: driverstats::lookup(statistics, driverstats::FCS_ERRORS)
            .or(Some(stats.rx_crc_errors)),
        alignment_errors: driverstats::lookup(statistics, driverstats::ALIGNMENT_ERRORS)
            .or(Some(stats.rx_frame_errors)),
        symbol_errors: driverstats::lookup(statistics, driverstats::SYMBOL_ERRORS),
        runts: driverstats::lookup(statistics, driverstats::RUNTS)
            .or(Some(stats.rx_length_errors)),
        giants: driverstats::lookup(statistics, driverstats::GIANTS)
            .or(Some(stats.rx_over_errors)),
        collisions: driverstats::lookup(statistics, driverstats::COLLISIONS)
            .or(Some(stats.collisions)),
        late_collisions: driverstats::lookup(statistics, driverstats::LATE_COLLISIONS),
        deferred: driverstats::lookup(statistics, driverstats::DEFERRED),
        pause_in: driverstats::total(statistics, driverstats::PAUSE_IN),
        pause_out: driverstats::total(statistics, driverstats::PAUSE_OUT),
    }
}

/// The speed and duplex list, from the driver's supported-modes bitmap.
fn capabilities(settings: &LinkSettings) -> Capabilities {
    let mut speed_duplex: Vec<String> = ethtool::speeds(&settings.supported)
        .into_iter()
        .map(|(speed, full)| {
            units::speed_duplex_slashed(speed, if full { Duplex::Full } else { Duplex::Half })
        })
        .collect();
    if ethtool::mode_is_set(&settings.supported, ethtool::LINK_MODE_AUTONEG) {
        speed_duplex.push("auto".to_string());
    }

    // What the hardware can be asked for, rather than what it is set to. Every
    // Linux driver that implements the pause parameters accepts all three.
    let flowcontrol = if ethtool::mode_is_set(&settings.supported, ethtool::LINK_MODE_PAUSE)
        || ethtool::mode_is_set(&settings.supported, ethtool::LINK_MODE_ASYM_PAUSE)
    {
        vec![
            "rx-(off,on,desired)".to_string(),
            "tx-(off,on,desired)".to_string(),
        ]
    } else {
        Vec::new()
    };

    Capabilities {
        speed_duplex,
        flowcontrol,
    }
}

fn advertisement(mask: &[u32]) -> Advertisement {
    Advertisement {
        speed_duplex: ethtool::speeds(mask)
            .into_iter()
            .map(|(speed, full)| {
                units::speed_duplex_slashed(speed, if full { Duplex::Full } else { Duplex::Half })
            })
            .collect(),
        pause: Some(ethtool::pause_advertisement(mask).to_string()),
    }
}

fn negotiation(settings: &LinkSettings, oper: Oper) -> Negotiation {
    let partner = (oper == Oper::Up && settings.link_partner.iter().any(|word| *word != 0))
        .then(|| advertisement(&settings.link_partner));

    Negotiation {
        mode: if settings.autoneg { "802.3" } else { "off" }.to_string(),
        status: match (settings.autoneg, oper) {
            (true, Oper::Up) => "success",
            (true, _) => "in progress",
            (false, _) => "n/a",
        }
        .to_string(),
        local: advertisement(&settings.advertising),
        partner,
        resolution: settings.speed_mbps.map(|speed| Advertisement {
            speed_duplex: vec![units::speed_duplex_slashed(
                speed,
                match settings.duplex {
                    Some(false) => Duplex::Half,
                    _ => Duplex::Full,
                },
            )],
            pause: None,
        }),
        resolved_pause: None,
    }
}

/// A copper or direct-attach port has no module to ask, so its media type is
/// what its speed and its connector say it is.
fn copper_media_type(settings: Option<&LinkSettings>, speed_mbps: Option<u64>) -> Option<String> {
    let settings = settings?;
    let speed = speed_mbps?;
    match settings.port {
        PORT_TWISTED_PAIR => Some(format!("{}BASE-T", units::speed_short(speed))),
        PORT_DIRECT_ATTACH => Some(format!("{}BASE-CR", units::speed_short(speed))),
        _ => None,
    }
}

// -- the configuration ------------------------------------------------------

/// What the configuration says about one interface.
#[derive(Debug, Clone, Default)]
struct Settings {
    kind: Option<Kind>,
    description: Option<String>,
    mac: Option<String>,
    speed: Option<u64>,
    duplex: Option<Duplex>,
    disabled: bool,
    load_interval: Option<u32>,
}

fn configured_interfaces(running: &ConfigTree) -> BTreeMap<String, Settings> {
    let mut out = BTreeMap::new();
    for (node, kind) in CONFIGURED_KINDS {
        let at = Path::from_segments(["interfaces", node]);
        let Some(instances) = running.get(&at).and_then(Node::children) else {
            continue;
        };
        for (name, instance) in instances {
            let value = |child: &str| {
                instance
                    .children()
                    .and_then(|children| children.get(child))
                    .and_then(|node| node.value().map(str::to_string))
            };
            let present = |child: &str| {
                instance
                    .children()
                    .is_some_and(|children| children.contains_key(child))
            };
            out.insert(
                name.clone(),
                Settings {
                    kind: Some(*kind),
                    description: value("description"),
                    mac: value("mac"),
                    // `auto` is the schema's word for "do not force one", and
                    // is not a speed.
                    speed: value("speed").and_then(|text| text.parse().ok()),
                    duplex: match value("duplex").as_deref() {
                        Some("full") => Some(Duplex::Full),
                        Some("half") => Some(Duplex::Half),
                        _ => None,
                    },
                    disabled: present("disable"),
                    load_interval: value("load-interval").and_then(|text| text.parse().ok()),
                },
            );
        }
    }
    out
}

/// An interface the configuration names and the kernel does not have.
fn absent(name: &str, settings: &Settings) -> Interface {
    Interface {
        name: name.to_string(),
        kind: settings.kind.unwrap_or_else(|| kind_from_name(name, None)),
        present: false,
        admin_up: !settings.disabled,
        oper: Oper::NotPresent,
        link: Link::NotConnect,
        description: settings.description.clone(),
        mac: settings.mac.clone(),
        admin_speed_mbps: settings.speed,
        admin_duplex: settings.duplex,
        ..Interface::default()
    }
}

fn leaf(tree: &ConfigTree, path: &[&str]) -> Option<String> {
    tree.get(&Path::from_segments(path.iter().copied()))
        .and_then(|node| node.value().map(str::to_string))
}

fn parse_mac(text: &str) -> Option<Vec<u8>> {
    let bytes: Option<Vec<u8>> = text
        .split(':')
        .map(|byte| u8::from_str_radix(byte, 16).ok())
        .collect();
    bytes.filter(|bytes| !bytes.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_operstate_is_translated_and_not_guessed_at() {
        assert_eq!(operstate(netlink::IF_OPER_UP), Oper::Up);
        assert_eq!(operstate(netlink::IF_OPER_DOWN), Oper::Down);
        assert_eq!(
            operstate(netlink::IF_OPER_LOWERLAYERDOWN),
            Oper::LowerLayerDown
        );
        assert_eq!(operstate(netlink::IF_OPER_NOTPRESENT), Oper::NotPresent);
        // Unknown, dormant and testing are all "not carrying traffic".
        assert_eq!(operstate(0), Oper::Down);
        assert_eq!(operstate(netlink::IF_OPER_DORMANT), Oper::Down);
    }

    #[test]
    fn the_bracketed_word_says_why_a_port_is_not_carrying_anything() {
        assert_eq!(link_state(true, Oper::Up, false), Link::Connected);
        assert_eq!(link_state(false, Oper::Down, false), Link::Disabled);
        assert_eq!(link_state(true, Oper::Down, false), Link::NotConnect);
        assert_eq!(link_state(true, Oper::Down, true), Link::Inactive);
    }

    #[test]
    fn membership_prefers_the_most_specific_thing_true_of_the_port() {
        let mut addresses = BTreeMap::new();
        addresses.insert(
            3u32,
            vec![netlink::Address {
                index: 3,
                prefix: "10.0.0.1/24".into(),
                broadcast: None,
            }],
        );

        // Enslaved beats everything: a member with an address still reads as
        // a member, because the address on it is the misconfiguration.
        assert_eq!(
            membership(Kind::Ethernet, Some("bond0"), &addresses, 3, false),
            Membership::InBond("bond0".into())
        );
        assert_eq!(
            membership(Kind::Ethernet, None, &addresses, 3, false),
            Membership::Routed
        );
        assert_eq!(
            membership(Kind::Ethernet, None, &addresses, 4, true),
            Membership::Trunk
        );
        assert_eq!(
            membership(Kind::PortChannel, None, &addresses, 4, false),
            Membership::Trunk
        );
        assert_eq!(
            membership(Kind::Ethernet, None, &addresses, 4, false),
            Membership::Access(1)
        );
        assert_eq!(
            membership(Kind::Wireguard, None, &addresses, 4, false),
            Membership::Unknown
        );
    }

    #[test]
    fn the_unicast_count_is_the_total_with_the_other_two_taken_off() {
        let stats = Stats {
            rx_packets: 1_000,
            tx_packets: 500,
            multicast: 100,
            ..Stats::default()
        };
        let statistics: Statistics = [
            ("rx_broadcast".to_string(), 50u64),
            ("tx_multicast".to_string(), 20u64),
            ("tx_broadcast".to_string(), 5u64),
        ]
        .into_iter()
        .collect();

        let counters = counters(&stats, &statistics);
        assert_eq!(counters.in_unicast, 850);
        assert_eq!(counters.in_multicast, 100);
        assert_eq!(counters.in_broadcast, 50);
        assert_eq!(counters.out_unicast, 475);
    }

    /// A driver reporting more multicast than total packets would otherwise
    /// produce a unicast count near `u64::MAX`.
    #[test]
    fn a_broken_driver_cannot_produce_a_negative_unicast_count() {
        let stats = Stats {
            rx_packets: 10,
            multicast: 1_000,
            ..Stats::default()
        };
        assert_eq!(counters(&stats, &Statistics::new()).in_unicast, 0);
    }

    #[test]
    fn a_counter_the_kernel_keeps_is_used_when_the_driver_has_no_name_for_it() {
        let stats = Stats {
            rx_crc_errors: 12,
            ..Stats::default()
        };
        let counters = counters(&stats, &Statistics::new());
        assert_eq!(counters.fcs_errors, Some(12));
        // And one only a driver can answer stays absent.
        assert_eq!(counters.symbol_errors, None);
        assert_eq!(counters.pause_in, None);
    }

    #[test]
    fn a_configured_interface_the_kernel_lacks_is_reported_as_not_present() {
        let settings = Settings {
            kind: Some(Kind::Ethernet),
            description: Some("the uplink".into()),
            ..Settings::default()
        };
        let interface = absent("eth9", &settings);
        assert!(!interface.present);
        assert_eq!(interface.oper, Oper::NotPresent);
        assert_eq!(interface.description.as_deref(), Some("the uplink"));
    }

    #[test]
    fn a_copper_port_is_named_for_its_speed_and_its_connector() {
        let twisted = LinkSettings {
            port: PORT_TWISTED_PAIR,
            ..LinkSettings::default()
        };
        assert_eq!(
            copper_media_type(Some(&twisted), Some(1_000)).as_deref(),
            Some("1GBASE-T")
        );
        let attached = LinkSettings {
            port: PORT_DIRECT_ATTACH,
            ..LinkSettings::default()
        };
        assert_eq!(
            copper_media_type(Some(&attached), Some(10_000)).as_deref(),
            Some("10GBASE-CR")
        );
        // A fibre port with no module read has nothing to be named for.
        let fibre = LinkSettings {
            port: 0x03,
            ..LinkSettings::default()
        };
        assert_eq!(copper_media_type(Some(&fibre), Some(10_000)), None);
        assert_eq!(copper_media_type(None, Some(10_000)), None);
    }

    #[test]
    fn capabilities_come_from_the_supported_bitmap() {
        let settings = LinkSettings {
            // 10/100/1000 half and full, plus autoneg and pause.
            supported: vec![0b0011_1111 | (1 << 6) | (1 << 13)],
            ..LinkSettings::default()
        };
        let capabilities = capabilities(&settings);
        assert_eq!(
            capabilities.speed_duplex,
            [
                "10M/half", "10M/full", "100M/half", "100M/full", "1G/half", "1G/full", "auto"
            ]
        );
        assert_eq!(capabilities.flowcontrol.len(), 2);
    }

    #[test]
    fn a_port_that_does_not_negotiate_says_so_and_has_no_partner() {
        let settings = LinkSettings {
            autoneg: false,
            speed_mbps: Some(10_000),
            duplex: Some(true),
            ..LinkSettings::default()
        };
        let negotiation = negotiation(&settings, Oper::Up);
        assert_eq!(negotiation.mode, "off");
        assert_eq!(negotiation.status, "n/a");
        assert!(negotiation.partner.is_none());
        assert_eq!(
            negotiation.resolution.expect("a resolution").speed_duplex,
            ["10G/full"]
        );
    }

    #[test]
    fn a_link_partner_is_only_reported_when_there_is_a_link_to_have_one() {
        let settings = LinkSettings {
            autoneg: true,
            link_partner: vec![0b0011_1111],
            ..LinkSettings::default()
        };
        assert!(negotiation(&settings, Oper::Up).partner.is_some());
        assert!(negotiation(&settings, Oper::Down).partner.is_none());
        assert_eq!(negotiation(&settings, Oper::Down).status, "in progress");
    }

    #[test]
    fn a_mac_from_sysfs_is_read_back_into_bytes() {
        assert_eq!(
            parse_mac("2c:dd:e9:12:00:a1"),
            Some(vec![0x2c, 0xdd, 0xe9, 0x12, 0x00, 0xa1])
        );
        assert_eq!(parse_mac("not a mac"), None);
        assert_eq!(parse_mac(""), None);
    }

    #[test]
    fn the_configuration_supplies_what_the_kernel_has_no_concept_of() {
        let schema = nightshade_schema::model::Schema::compiled();
        let mut tree = ConfigTree::new();
        for (path, value) in [
            ("interfaces ethernet eth0 description", Some("the uplink")),
            ("interfaces ethernet eth0 speed", Some("10000")),
            ("interfaces ethernet eth0 duplex", Some("full")),
            ("interfaces ethernet eth0 disable", None),
            ("interfaces ethernet eth1", None),
        ] {
            let path = Path::parse(path).expect("a path");
            schema.apply_set(&mut tree, &path, value).expect("a set");
        }

        let configured = configured_interfaces(&tree);
        let eth0 = configured.get("eth0").expect("eth0");
        assert_eq!(eth0.description.as_deref(), Some("the uplink"));
        assert_eq!(eth0.speed, Some(10_000));
        assert_eq!(eth0.duplex, Some(Duplex::Full));
        assert!(eth0.disabled);
        assert_eq!(eth0.kind, Some(Kind::Ethernet));

        let eth1 = configured.get("eth1").expect("eth1");
        assert!(!eth1.disabled);
        assert_eq!(eth1.description, None);
    }
}
