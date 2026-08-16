//! Value types and their validators.
//!
//! Every leaf in the schema names one of these, and it is the only thing
//! standing between a typed string and something that reaches `ip link`. They
//! run in configd, never in a client -- a check a caller can skip is not a
//! check.
//!
//! Errors say what was expected rather than that something was wrong.
//! `"1.2.3.400/24" is not an IPv4 address and prefix length` tells an operator
//! what to type next; "invalid value" does not.

use std::fmt;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use regex::Regex;

/// Longest interface name the kernel accepts, `IFNAMSIZ - 1`.
const IFNAMSIZ_MAX: usize = 15;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{value:?} is not {expected}")]
pub struct ValueError {
    pub value: String,
    pub expected: String,
}

impl ValueError {
    fn new(value: &str, expected: impl Into<String>) -> Self {
        Self {
            value: value.to_string(),
            expected: expected.into(),
        }
    }
}

type Check = Result<(), ValueError>;

/// A bounded integer range, optionally stepped.
///
/// The step is not decoration: a bridge priority the kernel will accept is a
/// multiple of 4096, and a bridge that silently rounds 5000 down to 4096 is a
/// spanning tree that does not elect the root the operator drew on the
/// whiteboard.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Range {
    pub min: i64,
    pub max: i64,
    pub step: Option<u64>,
}

impl Range {
    pub fn new(min: i64, max: i64) -> Self {
        Self { min, max, step: None }
    }

    pub fn stepped(min: i64, max: i64, step: u64) -> Self {
        Self {
            min,
            max,
            step: Some(step),
        }
    }

    fn describe(&self) -> String {
        match self.step {
            Some(step) => format!(
                "a whole number between {} and {} in steps of {step}",
                self.min, self.max
            ),
            None => format!("a whole number between {} and {}", self.min, self.max),
        }
    }

    fn check(&self, value: &str) -> Check {
        let n: i64 = value
            .parse()
            .map_err(|_| ValueError::new(value, self.describe()))?;
        if n < self.min || n > self.max {
            return Err(ValueError::new(value, self.describe()));
        }
        if let Some(step) = self.step
            && !(n - self.min).unsigned_abs().is_multiple_of(step)
        {
            return Err(ValueError::new(value, self.describe()));
        }
        Ok(())
    }
}

/// What a leaf, or a tag node's key, is allowed to hold.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ValueType {
    Text,
    Bool,
    Number(Range),
    Ipv4Address,
    Ipv6Address,
    IpAddress,
    Ipv4Prefix,
    Ipv6Prefix,
    IpPrefix,
    /// A bare address or an address with a prefix length. What
    /// `source-address` takes.
    IpOrPrefix,
    MulticastAddress,
    MacAddress,
    Port,
    PortRange,
    InterfaceName,
    Hostname,
    TimeZone,
    Enum(Vec<String>),
}

impl ValueType {
    /// What completion and `?` help show in place of a value.
    pub fn placeholder(&self) -> String {
        match self {
            ValueType::Text => "<text>".into(),
            ValueType::Bool => "<true|false>".into(),
            ValueType::Number(r) => format!("<{}-{}>", r.min, r.max),
            ValueType::Ipv4Address => "<x.x.x.x>".into(),
            ValueType::Ipv6Address => "<h:h:h:h:h:h:h:h>".into(),
            ValueType::IpAddress => "<ip-address>".into(),
            ValueType::Ipv4Prefix => "<x.x.x.x/x>".into(),
            ValueType::Ipv6Prefix => "<h:h:h:h:h:h:h:h/x>".into(),
            ValueType::IpPrefix => "<ip-address/prefix>".into(),
            ValueType::IpOrPrefix => "<ip-address[/prefix]>".into(),
            ValueType::MulticastAddress => "<multicast-address>".into(),
            ValueType::MacAddress => "<xx:xx:xx:xx:xx:xx>".into(),
            ValueType::Port => "<1-65535>".into(),
            ValueType::PortRange => "<port[-port]>".into(),
            ValueType::InterfaceName => "<interface>".into(),
            ValueType::Hostname => "<hostname>".into(),
            ValueType::TimeZone => "<Area/Location>".into(),
            ValueType::Enum(values) => format!("<{}>", values.join("|")),
        }
    }

    /// The phrase that follows "is not" in an error.
    fn expected(&self) -> String {
        match self {
            ValueType::Text => "text".into(),
            ValueType::Bool => "`true` or `false`".into(),
            ValueType::Number(r) => r.describe(),
            ValueType::Ipv4Address => "an IPv4 address".into(),
            ValueType::Ipv6Address => "an IPv6 address".into(),
            ValueType::IpAddress => "an IP address".into(),
            ValueType::Ipv4Prefix => "an IPv4 address and prefix length, e.g. 192.168.1.1/24".into(),
            ValueType::Ipv6Prefix => "an IPv6 address and prefix length, e.g. 2001:db8::1/64".into(),
            ValueType::IpPrefix => "an IP address and prefix length".into(),
            ValueType::IpOrPrefix => "an IP address, with or without a prefix length".into(),
            ValueType::MulticastAddress => "a multicast IP address".into(),
            ValueType::MacAddress => "a unicast MAC address, e.g. 02:00:5e:10:00:01".into(),
            ValueType::Port => "a port number between 1 and 65535".into(),
            ValueType::PortRange => "a port or a port range, e.g. 1024-2048".into(),
            ValueType::InterfaceName => {
                format!("a valid interface name (up to {IFNAMSIZ_MAX} characters)")
            }
            ValueType::Hostname => "a valid host name".into(),
            ValueType::TimeZone => "a known time zone, e.g. Europe/London".into(),
            ValueType::Enum(values) => format!("one of: {}", values.join(", ")),
        }
    }

    pub fn check(&self, value: &str) -> Check {
        let ok = match self {
            ValueType::Text => return check_text(value),
            ValueType::Bool => matches!(value, "true" | "false"),
            ValueType::Number(range) => return range.check(value),
            ValueType::Ipv4Address => value.parse::<Ipv4Addr>().is_ok(),
            ValueType::Ipv6Address => value.parse::<Ipv6Addr>().is_ok(),
            ValueType::IpAddress => value.parse::<IpAddr>().is_ok(),
            ValueType::Ipv4Prefix => is_prefix(value, Family::V4),
            ValueType::Ipv6Prefix => is_prefix(value, Family::V6),
            ValueType::IpPrefix => is_prefix(value, Family::Any),
            ValueType::IpOrPrefix => {
                value.parse::<IpAddr>().is_ok() || is_prefix(value, Family::Any)
            }
            ValueType::MulticastAddress => value.parse::<IpAddr>().is_ok_and(|a| a.is_multicast()),
            ValueType::MacAddress => is_unicast_mac(value),
            ValueType::Port => Range::new(1, 65535).check(value).is_ok(),
            ValueType::PortRange => is_port_range(value),
            ValueType::InterfaceName => is_interface_name(value),
            ValueType::Hostname => is_hostname(value),
            ValueType::TimeZone => return check_time_zone(value),
            ValueType::Enum(values) => values.iter().any(|v| v == value),
        };
        if ok {
            Ok(())
        } else {
            Err(ValueError::new(value, self.expected()))
        }
    }
}

/// A type, plus the extra rules the schema layered on it.
#[derive(Debug, Clone)]
pub struct ValueSpec {
    pub ty: ValueType,
    /// Literal keywords accepted instead of a value of `ty`.
    ///
    /// This is how `address` takes `dhcp`. A general union type would cover it
    /// too, and would also let a schema author write something nobody can
    /// complete or explain; a list of extra keywords is the whole of what is
    /// actually needed.
    pub accepts: Vec<String>,
    pub pattern: Option<Regex>,
}

/// `Regex` has no `PartialEq`, and the schema needs one so the generated and
/// the runtime-loaded tree can be compared. Two patterns are the same rule if
/// they are the same source text.
impl PartialEq for ValueSpec {
    fn eq(&self, other: &Self) -> bool {
        self.ty == other.ty
            && self.accepts == other.accepts
            && self.pattern.as_ref().map(Regex::as_str) == other.pattern.as_ref().map(Regex::as_str)
    }
}

impl Eq for ValueSpec {}

impl ValueSpec {
    pub fn new(ty: ValueType) -> Self {
        Self {
            ty,
            accepts: Vec::new(),
            pattern: None,
        }
    }

    pub fn accepting(mut self, keywords: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.accepts = keywords.into_iter().map(Into::into).collect();
        self
    }

    pub fn check(&self, value: &str) -> Check {
        if self.accepts.iter().any(|k| k == value) {
            return Ok(());
        }
        self.ty.check(value).map_err(|mut e| {
            if !self.accepts.is_empty() {
                e.expected = format!("{}, or one of: {}", e.expected, self.accepts.join(", "));
            }
            e
        })?;
        if let Some(pattern) = &self.pattern
            && !pattern.is_match(value)
        {
            return Err(ValueError::new(
                value,
                format!("{} matching {}", self.ty.expected(), pattern.as_str()),
            ));
        }
        Ok(())
    }

    pub fn placeholder(&self) -> String {
        if self.accepts.is_empty() {
            self.ty.placeholder()
        } else {
            format!("<{}|{}>", self.ty.placeholder().trim_matches(['<', '>']), self.accepts.join("|"))
        }
    }
}

impl fmt::Display for ValueSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.placeholder())
    }
}

// ---------------------------------------------------------------------------
// the individual rules
// ---------------------------------------------------------------------------

/// Free text still has limits. A config value that contains a newline or a
/// control character round-trips through the file format -- it is escaped --
/// but it will end up in a `Description=` line in a networkd unit, in a log
/// message, and on a terminal, and none of those want it.
fn check_text(value: &str) -> Check {
    match value.chars().find(|c| c.is_control()) {
        Some(_) => Err(ValueError::new(
            value,
            "text without control characters or line breaks",
        )),
        None => Ok(()),
    }
}

enum Family {
    V4,
    V6,
    Any,
}

fn is_prefix(value: &str, family: Family) -> bool {
    let Some((addr, len)) = value.split_once('/') else {
        return false;
    };
    // Rejected rather than accepted-and-normalised: `/024` and `/24` are the
    // same number and two different strings, and a config that holds both is a
    // config where two rules that look different are the same rule.
    if len.is_empty() || len.len() > 3 || !len.bytes().all(|b| b.is_ascii_digit()) {
        return false;
    }
    if len.len() > 1 && len.starts_with('0') {
        return false;
    }
    let Ok(len) = len.parse::<u8>() else {
        return false;
    };
    match (family, addr.parse::<IpAddr>()) {
        (Family::V4 | Family::Any, Ok(IpAddr::V4(_))) => len <= 32,
        (Family::V6 | Family::Any, Ok(IpAddr::V6(_))) => len <= 128,
        _ => false,
    }
}

/// Six colon-separated hex octets, unicast, not all zero.
///
/// The unicast check is a real rule and not pedantry: the low bit of the first
/// octet is the group bit, and an interface configured with it set does not
/// receive its own unicast traffic.
fn is_unicast_mac(value: &str) -> bool {
    let mut octets = [0u8; 6];
    let mut count = 0;
    for part in value.split(':') {
        if count == 6 || part.len() != 2 || !part.bytes().all(|b| b.is_ascii_hexdigit()) {
            return false;
        }
        octets[count] = u8::from_str_radix(part, 16).expect("two hex digits");
        count += 1;
    }
    count == 6 && octets != [0; 6] && octets[0] & 1 == 0
}

fn is_port_range(value: &str) -> bool {
    let port = |s: &str| -> Option<u16> {
        if s.is_empty() || (s.len() > 1 && s.starts_with('0')) {
            return None;
        }
        s.parse::<u16>().ok().filter(|p| *p > 0)
    };
    match value.split_once('-') {
        Some((low, high)) => match (port(low), port(high)) {
            (Some(low), Some(high)) => low <= high,
            _ => false,
        },
        None => port(value).is_some(),
    }
}

/// What the kernel will accept as an interface name.
///
/// `IFNAMSIZ - 1` characters, no `/` (it would break `/sys/class/net`
/// lookups), no whitespace or control characters, and neither `.` nor `..`
/// (they are directory entries).
fn is_interface_name(value: &str) -> bool {
    if value.is_empty() || value.len() > IFNAMSIZ_MAX || value == "." || value == ".." {
        return false;
    }
    value
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

/// RFC 1123.
///
/// Stricter than the kernel, which accepts nearly anything: an invalid name
/// breaks sudo's reverse lookup and every certificate the appliance later
/// presents.
fn is_hostname(value: &str) -> bool {
    if value.is_empty() || value.len() > 253 || value.starts_with('.') || value.ends_with('.') {
        return false;
    }
    value.split('.').all(|label| {
        !label.is_empty()
            && label.len() <= 63
            && !label.starts_with('-')
            && !label.ends_with('-')
            && label.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
    })
}

/// A zone name the box actually has.
///
/// Two checks, and the split matters. The shape check is unconditional: a
/// value that reaches `timedatectl set-timezone` should look like a zone name
/// whatever else is true. The existence check consults the system tzdb, and is
/// skipped when there is not one, so that an image built without `tzdata` --
/// or a test container -- fails at apply with a clear message from
/// `timedatectl` rather than rejecting every zone on earth at validation time.
fn check_time_zone(value: &str) -> Check {
    let expected = ValueType::TimeZone.expected();
    let shaped = !value.is_empty()
        && value.len() <= 64
        && !value.starts_with('/')
        && !value.ends_with('/')
        && !value.contains("..")
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '_' | '-' | '+'));
    if !shaped {
        return Err(ValueError::new(value, expected));
    }
    if tzdb_available() && jiff::tz::db().get(value).is_err() {
        return Err(ValueError::new(value, format!("{expected} (not in this system's time zone database)")));
    }
    Ok(())
}

fn tzdb_available() -> bool {
    jiff::tz::db().available().next().is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn accepts(ty: ValueType, values: &[&str]) {
        for v in values {
            assert!(ty.check(v).is_ok(), "{ty:?} should accept {v:?}");
        }
    }

    fn rejects(ty: ValueType, values: &[&str]) {
        for v in values {
            assert!(ty.check(v).is_err(), "{ty:?} should reject {v:?}");
        }
    }

    #[test]
    fn addresses_and_prefixes() {
        accepts(ValueType::Ipv4Address, &["192.168.1.1", "0.0.0.0", "255.255.255.255"]);
        rejects(
            ValueType::Ipv4Address,
            &["192.168.1.256", "192.168.1", "192.168.001.1", "1.2.3.4/24", ""],
        );

        accepts(ValueType::Ipv4Prefix, &["192.168.1.1/24", "10.0.0.0/8", "0.0.0.0/0"]);
        rejects(
            ValueType::Ipv4Prefix,
            &["192.168.1.1/33", "192.168.1.1", "192.168.1.1/", "192.168.1.1/024", "2001:db8::1/64"],
        );

        accepts(ValueType::Ipv6Prefix, &["2001:db8::1/64", "::/0", "fe80::1/128"]);
        rejects(ValueType::Ipv6Prefix, &["2001:db8::1/129", "192.168.1.1/24"]);

        accepts(ValueType::IpPrefix, &["192.168.1.1/24", "2001:db8::1/64"]);
        accepts(ValueType::IpOrPrefix, &["192.168.1.1", "2001:db8::1/64"]);
        rejects(ValueType::IpOrPrefix, &["192.168.1.1/33", "not-an-address"]);
    }

    #[test]
    fn multicast_is_checked_for_being_multicast() {
        accepts(ValueType::MulticastAddress, &["239.1.1.1", "224.0.0.1", "ff02::1"]);
        rejects(ValueType::MulticastAddress, &["192.168.1.1", "2001:db8::1"]);
    }

    #[test]
    fn mac_addresses_must_be_unicast() {
        accepts(ValueType::MacAddress, &["02:00:5e:10:00:01", "AA:BB:CC:DD:EE:00"]);
        rejects(
            ValueType::MacAddress,
            &[
                "01:00:5e:10:00:01",     // group bit set
                "00:00:00:00:00:00",     // all zero
                "02:00:5e:10:00",        // too short
                "02:00:5e:10:00:01:02",  // too long
                "02-00-5e-10-00-01",     // wrong separator
                "2:0:5e:10:0:1",         // unpadded
                "zz:00:5e:10:00:01",
            ],
        );
    }

    #[test]
    fn numbers_respect_range_and_step() {
        let mtu = ValueType::Number(Range::new(68, 9216));
        accepts(mtu.clone(), &["68", "1500", "9216"]);
        rejects(mtu, &["67", "9217", "", "1500.0", "-1", "abc"]);

        let priority = ValueType::Number(Range::stepped(0, 61440, 4096));
        accepts(priority.clone(), &["0", "4096", "32768", "61440"]);
        rejects(priority, &["5000", "1", "65536"]);
    }

    #[test]
    fn ports_and_ranges() {
        accepts(ValueType::Port, &["1", "4789", "65535"]);
        rejects(ValueType::Port, &["0", "65536", "-1", ""]);

        accepts(ValueType::PortRange, &["80", "1024-2048", "1-65535"]);
        rejects(ValueType::PortRange, &["2048-1024", "0-100", "80-", "-80", "80-70000"]);
    }

    #[test]
    fn interface_names_follow_the_kernel() {
        accepts(
            ValueType::InterfaceName,
            &["eth0", "enp1s0", "bond0", "br0", "vlan100", "eth0.100", "a"],
        );
        rejects(
            ValueType::InterfaceName,
            &[
                "",
                ".",
                "..",
                "eth 0",
                "eth/0",
                "abcdefghijklmnop", // 16 characters, IFNAMSIZ is 16 including the NUL
            ],
        );
        assert!(is_interface_name(&"a".repeat(IFNAMSIZ_MAX)));
    }

    #[test]
    fn hostnames_follow_rfc_1123() {
        accepts(ValueType::Hostname, &["nightshade", "fw-01", "edge1.example.com", "a", "0fw"]);
        rejects(
            ValueType::Hostname,
            &["", "-leading", "trailing-", "has space", "under_score", "double..dot", ".dot", "dot."],
        );
    }

    #[test]
    fn enums_list_what_was_expected() {
        let duplex = ValueType::Enum(vec!["auto".into(), "half".into(), "full".into()]);
        accepts(duplex.clone(), &["auto", "half", "full"]);
        let err = duplex.check("Full").unwrap_err();
        assert_eq!(err.to_string(), r#""Full" is not one of: auto, half, full"#);
    }

    #[test]
    fn text_rejects_control_characters() {
        accepts(ValueType::Text, &["the uplink", "", "\u{e9}t\u{e9}"]);
        rejects(ValueType::Text, &["two\nlines", "bell\x07"]);
    }

    #[test]
    fn time_zone_shape_is_checked_regardless_of_tzdata() {
        rejects(
            ValueType::TimeZone,
            &["", "/etc/passwd", "../../etc/passwd", "Europe/", "Europe/Lon don"],
        );
        // Only assert on real zones where the box has a database to check
        // against; a container without tzdata is not a failing test.
        if tzdb_available() {
            accepts(ValueType::TimeZone, &["UTC", "Europe/London", "America/New_York"]);
            rejects(ValueType::TimeZone, &["Mars/Olympus_Mons"]);
        }
    }

    #[test]
    fn accepted_keywords_bypass_the_type() {
        let address = ValueSpec::new(ValueType::IpPrefix).accepting(["dhcp"]);
        assert!(address.check("192.168.1.1/24").is_ok());
        assert!(address.check("dhcp").is_ok());

        let err = address.check("nonsense").unwrap_err();
        assert!(err.to_string().contains("or one of: dhcp"), "{err}");
        assert_eq!(address.placeholder(), "<ip-address/prefix|dhcp>");
    }

    #[test]
    fn a_pattern_narrows_a_type_without_replacing_it() {
        let vlan = ValueSpec {
            ty: ValueType::InterfaceName,
            accepts: Vec::new(),
            pattern: Some(Regex::new("^vlan[0-9]+$").unwrap()),
        };
        assert!(vlan.check("vlan100").is_ok());
        // Fails the pattern, passes the type.
        assert!(vlan.check("eth0").is_err());
        // Fails the type, so the type's message is what comes back.
        let err = vlan.check("eth 0").unwrap_err();
        assert!(err.to_string().contains("interface name"), "{err}");
    }

    #[test]
    fn specs_with_the_same_pattern_compare_equal() {
        let one = ValueSpec {
            ty: ValueType::InterfaceName,
            accepts: vec![],
            pattern: Some(Regex::new("^vlan[0-9]+$").unwrap()),
        };
        let two = ValueSpec {
            ty: ValueType::InterfaceName,
            accepts: vec![],
            pattern: Some(Regex::new("^vlan[0-9]+$").unwrap()),
        };
        assert_eq!(one, two);
    }
}
