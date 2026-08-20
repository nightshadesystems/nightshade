//! What an interface is, as far as `show interfaces` is concerned.
//!
//! One set of structs, serialised over the socket, rendered as text by
//! [`crate::render`] and as JSON by `serde`. That is what makes
//! `| display json` a modifier rather than a second code path: the text form
//! is never parsed back to produce the JSON one.
//!
//! # Optional everywhere
//!
//! Almost every field below is an `Option`, and that is deliberate rather than
//! lazy. A `wg0` has no duplex, a copper port has no transceiver, a driver
//! that does not implement `ETHTOOL_GSTATS` has no per-queue counters, and an
//! interface the kernel does not have at all has nothing but a name. The
//! renderers omit what is `None` and never substitute a zero for it -- a zero
//! error counter and an error counter the driver does not expose are different
//! facts, and only one of them is good news.

use serde::{Deserialize, Serialize};

/// Everything one `show interfaces ...` was answered with.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Snapshot {
    pub interfaces: Vec<Interface>,
    pub system: System,
}

/// Facts about the box rather than about any one interface.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct System {
    /// Platform identity, as `show interfaces capabilities` prints it. From
    /// the configuration if set, from DMI if not.
    pub model: Option<String>,
    /// Preformatted local time, for the header `show interfaces phy detail`
    /// prints. Formatted daemon-side because the daemon is the side that knows
    /// the configured time zone.
    pub time: Option<String>,
}

impl Snapshot {
    pub fn get(&self, name: &str) -> Option<&Interface> {
        self.interfaces.iter().find(|i| i.name == name)
    }
}

/// What sort of thing this is. Decides the `Hardware is ...` word, which
/// sections are printed, and which tables the interface appears in at all.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Kind {
    /// A physical port. The only kind with a PHY, a transceiver or a queue.
    #[default]
    Ethernet,
    Loopback,
    Vlan,
    /// A bond. EOS calls it a port-channel and so do the section headings.
    PortChannel,
    Tunnel,
    Wireguard,
    Bridge,
    /// Something the kernel has that is none of the above.
    Other,
}

impl Kind {
    /// The word after `Hardware is`.
    pub fn hardware(self) -> &'static str {
        match self {
            Kind::Ethernet => "Ethernet",
            Kind::Loopback => "Loopback",
            Kind::Vlan => "Vlan",
            Kind::PortChannel => "Port-Channel",
            Kind::Tunnel => "Tunnel",
            Kind::Wireguard => "Wireguard",
            Kind::Bridge => "Bridge",
            Kind::Other => "Unknown",
        }
    }

    /// Physical ports are the only ones with hardware to ask about, so they
    /// are the only rows in the counters, transceiver, phy and mac tables.
    pub fn is_physical(self) -> bool {
        matches!(self, Kind::Ethernet)
    }

    /// Ports and bonds, which is what `show interfaces status` lists.
    pub fn is_port(self) -> bool {
        matches!(self, Kind::Ethernet | Kind::PortChannel)
    }
}

/// The kernel's operational state, in the kernel's own words.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Oper {
    Up,
    #[default]
    Down,
    /// The device is up but what it sits on is not: a VLAN whose parent is
    /// down, a bond with no live member.
    LowerLayerDown,
    /// Configured, and the kernel has no such device.
    NotPresent,
}

impl Oper {
    pub fn label(self) -> &'static str {
        match self {
            Oper::Up => "up",
            Oper::Down => "down",
            Oper::LowerLayerDown => "lowerlayerdown",
            Oper::NotPresent => "notpresent",
        }
    }
}

/// The parenthesised word on the state line, and the `Status` column.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Link {
    Connected,
    #[default]
    NotConnect,
    /// Administratively down.
    Disabled,
    /// Shut by the box itself. The reason is in
    /// [`Interface::errdisable_reason`].
    ErrDisabled,
    /// Present and not participating -- a bond member the bond has rejected.
    Inactive,
}

impl Link {
    pub fn label(self) -> &'static str {
        match self {
            Link::Connected => "connected",
            Link::NotConnect => "notconnect",
            Link::Disabled => "disabled",
            Link::ErrDisabled => "errdisabled",
            Link::Inactive => "inactive",
        }
    }

    /// The words `show interfaces status <filter>` accepts.
    pub fn parse(word: &str) -> Option<Self> {
        match word {
            "connected" => Some(Link::Connected),
            "notconnect" => Some(Link::NotConnect),
            "disabled" => Some(Link::Disabled),
            "errdisabled" => Some(Link::ErrDisabled),
            "inactive" => Some(Link::Inactive),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Duplex {
    Half,
    Full,
}

impl Duplex {
    /// `full` -- the `show interfaces status` column.
    pub fn short(self) -> &'static str {
        match self {
            Duplex::Half => "half",
            Duplex::Full => "full",
        }
    }

    /// `Full-duplex` -- the detail line.
    pub fn long(self) -> &'static str {
        match self {
            Duplex::Half => "Half-duplex",
            Duplex::Full => "Full-duplex",
        }
    }
}

/// Where the running speed and duplex came from.
///
/// This is what puts the `a-` in front of `a-1G`, and it is deliberately not
/// the same question as "is the autoneg bit set". A port can advertise and
/// still be pinned to one speed by configuration; EOS marks the value that was
/// *resolved by* negotiation, not the port that was willing to negotiate.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SpeedSource {
    /// Set by configuration, or by a driver that reports a fixed speed.
    #[default]
    Forced,
    /// Resolved by autonegotiation. Prints `a-full`, `a-1G`.
    Negotiated,
}

/// One address on an interface.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Address {
    /// `203.0.113.2/30`.
    pub prefix: String,
    /// `255.255.255.255`, when the kernel has one. IPv6 has none.
    pub broadcast: Option<String>,
}

/// What the `Vlan` column of `show interfaces status` says.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Membership {
    /// Layer 3. The port has addresses of its own.
    Routed,
    /// Carrying tagged VLANs, or an L2 master with nothing else to say.
    Trunk,
    /// An untagged port, in this VLAN.
    Access(u16),
    /// Enslaved. The string is the bond's name, so the column reads
    /// `in bond0` with Linux naming rather than EOS's.
    InBond(String),
    /// Nothing is known -- a port the kernel has and nothing configures.
    #[default]
    Unknown,
}

/// The counters the kernel keeps, after the last-clear baseline is subtracted.
///
/// `rtnl_link_stats64` gives the first block; the rest come from
/// `ethtool -S`, whose names vary by driver, so any of them may be absent on
/// a given box. Absent is not zero and is rendered differently.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Counters {
    /// Unicast only. `rtnl_link_stats64.rx_packets` counts everything, so the
    /// collector subtracts multicast and broadcast to get here -- which is
    /// what makes the `packets input` line and the `InUcastPkts` column the
    /// same number rather than two answers to one question.
    pub in_unicast: u64,
    pub in_multicast: u64,
    pub in_broadcast: u64,
    pub in_octets: u64,
    pub out_unicast: u64,
    pub out_multicast: u64,
    pub out_broadcast: u64,
    pub out_octets: u64,

    /// Aggregate. At least the sum of the specific error counters below.
    pub in_errors: u64,
    pub in_discards: u64,
    pub out_errors: u64,
    pub out_discards: u64,

    pub fcs_errors: Option<u64>,
    pub alignment_errors: Option<u64>,
    pub symbol_errors: Option<u64>,
    pub runts: Option<u64>,
    pub giants: Option<u64>,
    pub collisions: Option<u64>,
    pub late_collisions: Option<u64>,
    pub deferred: Option<u64>,
    pub pause_in: Option<u64>,
    pub pause_out: Option<u64>,
}

/// Rates over the configured load interval.
///
/// The percentages are carried rather than recomputed at render time. The
/// daemon is the side that knows the line speed the sample was taken at, and
/// recomputing here from a rounded `bps` would print a utilisation that does
/// not match the one the daemon measured.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct Rates {
    /// Seconds. 300 prints as `5 minutes`.
    pub interval: u32,
    pub in_bps: f64,
    pub in_pps: f64,
    /// Percent of line rate, framing overhead included.
    pub in_percent: f64,
    pub out_bps: f64,
    pub out_pps: f64,
    pub out_percent: f64,
}

/// A bond, from the master's side.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Bond {
    pub members: Vec<BondMember>,
    /// `Fallback mode is: off`.
    pub fallback: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BondMember {
    pub name: String,
    pub duplex: Option<Duplex>,
    pub speed_mbps: Option<u64>,
}

/// One hardware transmit queue.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Queue {
    /// `UC0`.
    pub name: String,
    pub packets: u64,
    pub bytes: u64,
    pub dropped_packets: u64,
    pub dropped_bytes: u64,
}

/// RMON frame-size distribution. One value per bin, in the order printed.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Bins {
    pub received: [u64; 7],
    pub transmitted: [u64; 7],
}

/// The labels of the seven bins, in order.
pub const BIN_LABELS: [&str; 7] = [
    "64 bytes:",
    "65-127 bytes:",
    "128-255 bytes:",
    "256-511 bytes:",
    "512-1023 bytes:",
    "1024-1522 bytes:",
    "1523-max bytes:",
];

/// A measured value with its alarm and warning thresholds.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct Measure {
    pub value: Option<f64>,
    pub high_alarm: Option<f64>,
    pub high_warn: Option<f64>,
    pub low_alarm: Option<f64>,
    pub low_warn: Option<f64>,
}

/// An optic, as SFF-8472/SFF-8636 describes it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Transceiver {
    /// `10GBASE-SR`.
    pub media_type: Option<String>,
    pub vendor: Option<String>,
    pub part_number: Option<String>,
    pub serial_number: Option<String>,
    pub date_code: Option<String>,
    pub temperature: Measure,
    pub voltage: Measure,
    pub tx_bias: Measure,
    pub tx_power: Measure,
    pub rx_power: Measure,
    /// Seconds since the DOM page was last read.
    pub age: Option<u64>,
    /// Raw pages, for `show interfaces transceiver eeprom`. `A0` is always
    /// there when the module is; `A2` only on a module with diagnostics.
    pub pages: Vec<EepromPage>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EepromPage {
    /// `A0`.
    pub name: String,
    pub bytes: Vec<u8>,
}

/// What the port could do, rather than what it is doing.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Capabilities {
    /// `1G/full`, `10G/full`, ... in ascending order, `auto` last when the
    /// port can negotiate.
    pub speed_duplex: Vec<String>,
    /// `rx-(off,on,desired)`.
    pub flowcontrol: Vec<String>,
}

/// `ethtool -a`, plus the pause-frame counters that go with it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FlowControl {
    pub send_admin: String,
    pub send_oper: String,
    pub receive_admin: String,
    pub receive_oper: String,
    pub rx_pause: u64,
    pub tx_pause: u64,
}

/// One side's advertisement.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Advertisement {
    /// `10M/half`, `1G/full`, ...
    pub speed_duplex: Vec<String>,
    /// `None`, `Symmetric`, `Asymmetric`.
    pub pause: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Negotiation {
    /// `802.3`, or `off` when the port does not negotiate.
    pub mode: String,
    /// `success`, `n/a`.
    pub status: String,
    pub local: Advertisement,
    /// Absent when there is no link, or when the driver does not report it.
    pub partner: Option<Advertisement>,
    /// What the two sides settled on.
    pub resolution: Option<Advertisement>,
    /// `rx off, tx off`.
    pub resolved_pause: Option<String>,
}

/// The PHY, as far as the driver will say.
///
/// Every field is optional and every one of them is driver-dependent: the rows
/// a driver cannot answer are left out of the output rather than printed as
/// zero.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Phy {
    pub state: Option<String>,
    pub interface_state: Option<String>,
    pub hw_resets: Option<u64>,
    pub transceiver: Option<String>,
    pub transceiver_serial: Option<String>,
    /// `10Gbps`.
    pub oper_speed: Option<String>,
    pub interrupt_count: Option<u64>,
    pub diags_mode: Option<String>,
    pub model: Option<String>,
    pub reset_count: Option<u64>,
    pub state_changes: Option<u64>,
    /// Seconds since the last PHY state change.
    pub last_change: Option<u64>,
    /// `10Gfull`.
    pub configured_speed: Option<String>,
    /// `off`, `on`.
    pub autoneg: Option<String>,
}

/// The MAC layer: fault signalling and FEC.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MacLayer {
    /// `linkUp`, `phyOff`.
    pub state: String,
    pub local_fault: Option<bool>,
    pub remote_fault: Option<bool>,
    /// `Disabled`, `Reed-Solomon`, `Fire code`.
    pub fec_mode: Option<String>,
    pub fec_corrected: Option<u64>,
    pub fec_uncorrected: Option<u64>,
}

/// One interface.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Interface {
    pub name: String,
    pub kind: Kind,
    /// Whether the kernel has this device at all. A configured interface that
    /// is missing is the single most useful thing this command can say, and it
    /// says it as `line protocol is notpresent`.
    pub present: bool,
    /// `IFF_UP`. False prints `administratively down`.
    pub admin_up: bool,
    pub oper: Oper,
    pub link: Link,
    pub errdisable_reason: Option<String>,
    pub description: Option<String>,
    /// The address the interface answers on -- the configured override where
    /// there is one.
    pub mac: Option<String>,
    /// Burned in. Equal to `mac` unless configuration overrode it.
    pub bia: Option<String>,
    pub addresses: Vec<Address>,
    pub mtu: Option<u32>,
    pub bandwidth_kbit: Option<u64>,
    pub duplex: Option<Duplex>,
    pub speed_mbps: Option<u64>,
    pub speed_source: SpeedSource,
    /// What the configuration asked for, as against what the link settled on.
    /// `show interfaces transceiver properties` prints both, and the pair is
    /// the fastest way to see a port that came up at a speed nobody asked for.
    pub admin_speed_mbps: Option<u64>,
    pub admin_duplex: Option<Duplex>,
    /// The autoneg bit, for the `auto negotiation: on` half of the detail
    /// line. Distinct from [`SpeedSource`]; see there.
    pub autoneg: Option<bool>,
    /// `n/a` on every Linux driver so far; carried so a driver that reports
    /// unidirectional link detection can fill it in.
    pub uni_link: Option<String>,
    pub loopback_mode: Option<String>,
    /// Seconds in the current link state.
    pub since: Option<u64>,
    pub link_changes: u64,
    /// Seconds since `clear counters`, or `None` for never.
    pub last_clear: Option<u64>,
    pub rates: Option<Rates>,
    pub counters: Option<Counters>,
    pub bond: Option<Bond>,
    /// The bond this port is enslaved to.
    pub member_of: Option<String>,
    pub membership: Membership,
    /// `10GBASE-SR`, `1000BASE-T`. The `Type` column.
    pub media_type: Option<String>,
    pub transceiver: Option<Transceiver>,
    pub capabilities: Option<Capabilities>,
    pub flow_control: Option<FlowControl>,
    pub negotiation: Option<Negotiation>,
    pub phy: Option<Phy>,
    pub mac_layer: Option<MacLayer>,
    pub queues: Vec<Queue>,
    pub bins: Option<Bins>,
    /// The two columns EOS reserves and almost never fills.
    pub flags: Option<String>,
    pub encapsulation: Option<String>,
}

impl Interface {
    pub fn new(name: impl Into<String>, kind: Kind) -> Self {
        Self {
            name: name.into(),
            kind,
            present: true,
            ..Self::default()
        }
    }

    /// `eth0 is up`, `eth2 is administratively down`.
    pub fn admin_words(&self) -> &'static str {
        if self.admin_up {
            "up"
        } else {
            "administratively down"
        }
    }

    /// Whether the counter block is worth printing.
    ///
    /// Physical ports always get one: a port with nothing on it and a port
    /// whose counters were just cleared look the same, and both are worth
    /// seeing. Virtual interfaces get one only once something has gone through
    /// them -- rtnetlink keeps stats for `lo` and every VLAN, and eight lines
    /// of zeroes under every one of them is noise that hides the port that
    /// matters.
    pub fn shows_counters(&self) -> bool {
        let Some(counters) = &self.counters else {
            return false;
        };
        if self.kind.is_physical() {
            return true;
        }
        counters.in_unicast != 0
            || counters.out_unicast != 0
            || counters.in_octets != 0
            || counters.out_octets != 0
    }
}
