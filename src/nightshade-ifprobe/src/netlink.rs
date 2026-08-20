//! rtnetlink, by hand.
//!
//! Two dumps and one subscription: `RTM_GETLINK` for what the interfaces are
//! and what has gone through them, `RTM_GETADDR` for the addresses the kernel
//! actually holds, and `RTNLGRP_LINK` for a link going up or down while we are
//! not looking.
//!
//! # Why not a crate
//!
//! Because this is a firewall appliance and the code below is the whole of
//! what it needs: a message header, an attribute walk and a counter struct.
//! A netlink crate is a great deal more surface than that, in a daemon running
//! as root, to save about three hundred lines that will not change again --
//! the layout of `struct ifinfomsg` is kernel ABI and cannot.
//!
//! # Parsing is separate from the socket
//!
//! Everything that reads bytes is a pure function over a slice, so it can be
//! tested against a message built by hand. The syscalls are in one place and
//! are the part that cannot be tested without a kernel.

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

use nix::libc;

// -- kernel constants -------------------------------------------------------

const NETLINK_ROUTE: libc::c_int = 0;

const NLM_F_REQUEST: u16 = 0x001;
const NLM_F_DUMP: u16 = 0x300;

const NLMSG_ERROR: u16 = 0x2;
const NLMSG_DONE: u16 = 0x3;

const RTM_NEWLINK: u16 = 16;
const RTM_DELLINK: u16 = 17;
const RTM_GETLINK: u16 = 18;
const RTM_GETADDR: u16 = 22;

const IFLA_ADDRESS: u16 = 1;
const IFLA_IFNAME: u16 = 3;
const IFLA_MTU: u16 = 4;
const IFLA_MASTER: u16 = 10;
const IFLA_OPERSTATE: u16 = 16;
const IFLA_LINKINFO: u16 = 18;
const IFLA_STATS64: u16 = 23;
const IFLA_PERM_ADDRESS: u16 = 54;

const IFLA_INFO_KIND: u16 = 1;

const IFA_ADDRESS: u16 = 1;
const IFA_LOCAL: u16 = 2;
const IFA_BROADCAST: u16 = 4;

const IFF_UP: u32 = 0x1;

/// `RTNLGRP_LINK` is group 1, and the bind mask is a bit position.
const RTMGRP_LINK: u32 = 1;

/// `if_link.h`: the values `IFLA_OPERSTATE` carries.
pub const IF_OPER_NOTPRESENT: u8 = 1;
pub const IF_OPER_DOWN: u8 = 2;
pub const IF_OPER_LOWERLAYERDOWN: u8 = 3;
pub const IF_OPER_DORMANT: u8 = 5;
pub const IF_OPER_UP: u8 = 6;

const NLMSG_HEADER: usize = 16;
const IFINFOMSG: usize = 16;
const IFADDRMSG: usize = 8;
const RTATTR_HEADER: usize = 4;

/// Netlink and its attributes are both four-byte aligned.
fn align(length: usize) -> usize {
    length.div_ceil(4) * 4
}

// -- what a dump comes back as ---------------------------------------------

/// One interface, as `RTM_GETLINK` describes it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Link {
    pub index: u32,
    pub name: String,
    pub flags: u32,
    pub mtu: Option<u32>,
    /// The address the interface answers on now.
    pub address: Option<Vec<u8>>,
    /// The one burned into the hardware. Absent on drivers that do not report
    /// it, in which case the two are the same as far as anyone can tell.
    pub permanent_address: Option<Vec<u8>>,
    pub operstate: u8,
    /// The bond or bridge this is enslaved to, by index.
    pub master: Option<u32>,
    /// `vlan`, `bond`, `bridge`, `wireguard` -- absent for a real device.
    pub kind: Option<String>,
    pub stats: Option<Stats>,
}

impl Link {
    pub fn admin_up(&self) -> bool {
        self.flags & IFF_UP != 0
    }
}

/// `rtnl_link_stats64`, in the order the kernel lays it out.
///
/// Only the fields that reach the output are named. The struct has grown over
/// the years and is read by offset with a length check, so a kernel with more
/// fields than this build knows about is read correctly rather than refused.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Stats {
    pub rx_packets: u64,
    pub tx_packets: u64,
    pub rx_bytes: u64,
    pub tx_bytes: u64,
    pub rx_errors: u64,
    pub tx_errors: u64,
    pub rx_dropped: u64,
    pub tx_dropped: u64,
    pub multicast: u64,
    pub collisions: u64,
    pub rx_length_errors: u64,
    pub rx_over_errors: u64,
    pub rx_crc_errors: u64,
    pub rx_frame_errors: u64,
}

/// One address on one interface.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Address {
    pub index: u32,
    /// `203.0.113.2/30`.
    pub prefix: String,
    pub broadcast: Option<String>,
}

// -- the socket -------------------------------------------------------------

/// A netlink socket, for a dump or for events.
pub struct Socket {
    fd: OwnedFd,
    sequence: u32,
}

impl Socket {
    /// A socket for request/response dumps.
    pub fn open() -> io::Result<Self> {
        Self::bind(0)
    }

    /// A socket subscribed to link up/down events.
    pub fn open_monitor() -> io::Result<Self> {
        Self::bind(RTMGRP_LINK)
    }

    fn bind(groups: u32) -> io::Result<Self> {
        // SAFETY: a socket(2) with constant arguments. The descriptor is
        // adopted by OwnedFd immediately, so it cannot be leaked on an error
        // path below.
        let raw = unsafe {
            libc::socket(
                libc::AF_NETLINK,
                libc::SOCK_RAW | libc::SOCK_CLOEXEC,
                NETLINK_ROUTE,
            )
        };
        if raw < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `raw` is a fresh descriptor this call owns.
        let fd = unsafe { OwnedFd::from_raw_fd(raw) };

        let mut address: libc::sockaddr_nl = unsafe { std::mem::zeroed() };
        address.nl_family = libc::AF_NETLINK as u16;
        address.nl_groups = groups;

        // SAFETY: `address` is a correctly initialised sockaddr_nl of the
        // length given, and `fd` is open for the duration of the call.
        let bound = unsafe {
            libc::bind(
                fd.as_raw_fd(),
                &address as *const libc::sockaddr_nl as *const libc::sockaddr,
                std::mem::size_of::<libc::sockaddr_nl>() as libc::socklen_t,
            )
        };
        if bound < 0 {
            return Err(io::Error::last_os_error());
        }

        Ok(Self { fd, sequence: 0 })
    }

    /// How long a read waits before giving up.
    ///
    /// A daemon blocked forever in `recv` on a netlink socket is a daemon that
    /// has stopped answering the CLI, and the situation that produces it -- a
    /// dump interrupted by a device disappearing mid-walk -- is one an
    /// appliance really does hit.
    pub fn set_timeout(&self, seconds: i64) -> io::Result<()> {
        let timeout = libc::timeval {
            tv_sec: seconds,
            tv_usec: 0,
        };
        // SAFETY: a setsockopt with a correctly sized timeval.
        let set = unsafe {
            libc::setsockopt(
                self.fd.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_RCVTIMEO,
                &timeout as *const libc::timeval as *const libc::c_void,
                std::mem::size_of::<libc::timeval>() as libc::socklen_t,
            )
        };
        if set < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// Ask for every message of `kind` and return each one's payload.
    fn dump(&mut self, kind: u16, family: u8) -> io::Result<Vec<Vec<u8>>> {
        self.sequence = self.sequence.wrapping_add(1);
        let sequence = self.sequence;
        self.send(&dump_request(kind, family, sequence))?;

        let mut payloads = Vec::new();
        let mut buffer = vec![0u8; 64 * 1024];
        loop {
            let read = self.receive(&mut buffer)?;
            let mut rest = &buffer[..read];
            while let Some((header, payload, remainder)) = split_message(rest) {
                rest = remainder;
                match header.kind {
                    NLMSG_DONE => return Ok(payloads),
                    NLMSG_ERROR => {
                        // The first four bytes of the payload are a negative
                        // errno; zero is an acknowledgement rather than a
                        // failure.
                        let code = payload
                            .get(..4)
                            .map(|bytes| i32::from_ne_bytes(bytes.try_into().expect("four bytes")))
                            .unwrap_or(0);
                        if code == 0 {
                            continue;
                        }
                        return Err(io::Error::from_raw_os_error(-code));
                    }
                    _ => payloads.push(payload.to_vec()),
                }
            }
        }
    }

    /// Every interface the kernel has.
    pub fn links(&mut self) -> io::Result<Vec<Link>> {
        Ok(self
            .dump(RTM_GETLINK, libc::AF_UNSPEC as u8)?
            .iter()
            .filter_map(|payload| parse_link(payload))
            .collect())
    }

    /// Every address the kernel holds, v4 and v6.
    pub fn addresses(&mut self) -> io::Result<Vec<Address>> {
        Ok(self
            .dump(RTM_GETADDR, libc::AF_UNSPEC as u8)?
            .iter()
            .filter_map(|payload| parse_address(payload))
            .collect())
    }

    /// Wait for a link to change state.
    ///
    /// Returns the interfaces named in whatever arrived, which the caller
    /// treats as "look at these again" rather than as a complete description
    /// -- an event and a dump can disagree, and the dump is the one that is
    /// right. `Ok(None)` is the read timing out, which is not an event and is
    /// not a failure.
    pub fn wait_for_link_event(&self) -> io::Result<Option<Vec<Link>>> {
        let mut buffer = vec![0u8; 32 * 1024];
        let read = match self.receive(&mut buffer) {
            Ok(read) => read,
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut | io::ErrorKind::Interrupted
                ) =>
            {
                return Ok(None);
            }
            Err(e) => return Err(e),
        };

        let mut links = Vec::new();
        let mut rest = &buffer[..read];
        while let Some((header, payload, remainder)) = split_message(rest) {
            rest = remainder;
            if (header.kind == RTM_NEWLINK || header.kind == RTM_DELLINK)
                && let Some(mut link) = parse_link(payload)
            {
                // A deletion carries the last state the device had, which is
                // not the state it is in now.
                if header.kind == RTM_DELLINK {
                    link.operstate = IF_OPER_NOTPRESENT;
                }
                links.push(link);
            }
        }
        Ok(Some(links))
    }

    fn send(&self, bytes: &[u8]) -> io::Result<()> {
        // SAFETY: `bytes` is a valid slice for the length given, and the
        // descriptor is open.
        let sent = unsafe {
            libc::send(
                self.fd.as_raw_fd(),
                bytes.as_ptr() as *const libc::c_void,
                bytes.len(),
                0,
            )
        };
        if sent < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    fn receive(&self, buffer: &mut [u8]) -> io::Result<usize> {
        // SAFETY: `buffer` is a valid mutable slice for the length given.
        let read = unsafe {
            libc::recv(
                self.fd.as_raw_fd(),
                buffer.as_mut_ptr() as *mut libc::c_void,
                buffer.len(),
                0,
            )
        };
        if read < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(read as usize)
    }
}

// -- message building -------------------------------------------------------

/// A `NLM_F_DUMP` request: a header and a one-byte family.
fn dump_request(kind: u16, family: u8, sequence: u32) -> Vec<u8> {
    // Both ifinfomsg and ifaddrmsg begin with the family byte, and a dump
    // needs nothing else in them, so one zeroed body serves for either.
    let body = align(IFINFOMSG);
    let mut message = vec![0u8; NLMSG_HEADER + body];
    message[0..4].copy_from_slice(&((NLMSG_HEADER + body) as u32).to_ne_bytes());
    message[4..6].copy_from_slice(&kind.to_ne_bytes());
    message[6..8].copy_from_slice(&(NLM_F_REQUEST | NLM_F_DUMP).to_ne_bytes());
    message[8..12].copy_from_slice(&sequence.to_ne_bytes());
    message[NLMSG_HEADER] = family;
    message
}

// -- parsing ----------------------------------------------------------------

#[derive(Debug, Clone, Copy)]
struct Header {
    kind: u16,
}

/// Split the first message off a buffer: its header, its payload, and the rest.
fn split_message(bytes: &[u8]) -> Option<(Header, &[u8], &[u8])> {
    if bytes.len() < NLMSG_HEADER {
        return None;
    }
    let length = u32::from_ne_bytes(bytes[0..4].try_into().expect("four bytes")) as usize;
    let kind = u16::from_ne_bytes(bytes[4..6].try_into().expect("two bytes"));
    // A length shorter than the header, or longer than what was read, is a
    // malformed message; walking on from it would be walking into whatever
    // happens to be next in the buffer.
    if length < NLMSG_HEADER || length > bytes.len() {
        return None;
    }
    let payload = &bytes[NLMSG_HEADER..length];
    let next = align(length).min(bytes.len());
    Some((Header { kind }, payload, &bytes[next..]))
}

/// Walk a netlink attribute list.
///
/// Every bound is checked and a malformed entry stops the walk rather than
/// being skipped, because an attribute whose length is wrong means the offsets
/// of everything after it are wrong too.
pub fn attributes(bytes: &[u8]) -> Attributes<'_> {
    Attributes { rest: bytes }
}

pub struct Attributes<'a> {
    rest: &'a [u8],
}

impl<'a> Iterator for Attributes<'a> {
    type Item = (u16, &'a [u8]);

    fn next(&mut self) -> Option<Self::Item> {
        if self.rest.len() < RTATTR_HEADER {
            return None;
        }
        let length = u16::from_ne_bytes(self.rest[0..2].try_into().expect("two bytes")) as usize;
        let kind = u16::from_ne_bytes(self.rest[2..4].try_into().expect("two bytes"));
        if length < RTATTR_HEADER || length > self.rest.len() {
            self.rest = &[];
            return None;
        }
        let value = &self.rest[RTATTR_HEADER..length];
        self.rest = &self.rest[align(length).min(self.rest.len())..];
        Some((kind, value))
    }
}

fn as_u32(bytes: &[u8]) -> Option<u32> {
    bytes
        .get(..4)
        .map(|bytes| u32::from_ne_bytes(bytes.try_into().expect("four bytes")))
}

/// A NUL-terminated kernel string.
fn as_string(bytes: &[u8]) -> String {
    let end = bytes.iter().position(|byte| *byte == 0).unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..end]).into_owned()
}

pub fn parse_link(payload: &[u8]) -> Option<Link> {
    if payload.len() < IFINFOMSG {
        return None;
    }
    let mut link = Link {
        index: i32::from_ne_bytes(payload[4..8].try_into().expect("four bytes")) as u32,
        flags: u32::from_ne_bytes(payload[8..12].try_into().expect("four bytes")),
        ..Link::default()
    };

    for (kind, value) in attributes(&payload[IFINFOMSG..]) {
        match kind {
            IFLA_IFNAME => link.name = as_string(value),
            IFLA_MTU => link.mtu = as_u32(value),
            IFLA_ADDRESS => link.address = Some(value.to_vec()),
            IFLA_PERM_ADDRESS => link.permanent_address = Some(value.to_vec()),
            IFLA_OPERSTATE => link.operstate = value.first().copied().unwrap_or(0),
            IFLA_MASTER => link.master = as_u32(value),
            IFLA_STATS64 => link.stats = parse_stats(value),
            IFLA_LINKINFO => {
                for (nested, nested_value) in attributes(value) {
                    if nested == IFLA_INFO_KIND {
                        link.kind = Some(as_string(nested_value));
                    }
                }
            }
            _ => {}
        }
    }

    // A link with no name is a link nothing can be said about.
    (!link.name.is_empty()).then_some(link)
}

fn parse_stats(bytes: &[u8]) -> Option<Stats> {
    let field = |index: usize| -> u64 {
        let at = index * 8;
        bytes
            .get(at..at + 8)
            .map(|bytes| u64::from_ne_bytes(bytes.try_into().expect("eight bytes")))
            .unwrap_or(0)
    };
    // Anything shorter than the first four counters is not a stats struct.
    if bytes.len() < 32 {
        return None;
    }
    Some(Stats {
        rx_packets: field(0),
        tx_packets: field(1),
        rx_bytes: field(2),
        tx_bytes: field(3),
        rx_errors: field(4),
        tx_errors: field(5),
        rx_dropped: field(6),
        tx_dropped: field(7),
        multicast: field(8),
        collisions: field(9),
        rx_length_errors: field(10),
        rx_over_errors: field(11),
        rx_crc_errors: field(12),
        rx_frame_errors: field(13),
    })
}

pub fn parse_address(payload: &[u8]) -> Option<Address> {
    if payload.len() < IFADDRMSG {
        return None;
    }
    let family = payload[0];
    let prefix_length = payload[1];
    let index = u32::from_ne_bytes(payload[4..8].try_into().expect("four bytes"));

    let mut local = None;
    let mut address = None;
    let mut broadcast = None;
    for (kind, value) in attributes(&payload[IFADDRMSG..]) {
        match kind {
            // On a point-to-point link IFA_ADDRESS is the peer's and
            // IFA_LOCAL is ours. Taking IFA_ADDRESS unconditionally is how a
            // tunnel ends up reported as holding the far end's address.
            IFA_LOCAL => local = format_address(family, value),
            IFA_ADDRESS => address = format_address(family, value),
            IFA_BROADCAST => broadcast = format_address(family, value),
            _ => {}
        }
    }

    let text = local.or(address)?;
    Some(Address {
        index,
        prefix: format!("{text}/{prefix_length}"),
        broadcast,
    })
}

fn format_address(family: u8, bytes: &[u8]) -> Option<String> {
    match family as libc::c_int {
        libc::AF_INET if bytes.len() >= 4 => Some(format!(
            "{}.{}.{}.{}",
            bytes[0], bytes[1], bytes[2], bytes[3]
        )),
        libc::AF_INET6 if bytes.len() >= 16 => {
            let mut groups = [0u16; 8];
            for (index, group) in groups.iter_mut().enumerate() {
                *group = u16::from_be_bytes([bytes[index * 2], bytes[index * 2 + 1]]);
            }
            Some(std::net::Ipv6Addr::from(groups).to_string())
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build an attribute, header and padding included.
    fn attribute(kind: u16, value: &[u8]) -> Vec<u8> {
        let length = RTATTR_HEADER + value.len();
        let mut out = Vec::with_capacity(align(length));
        out.extend_from_slice(&(length as u16).to_ne_bytes());
        out.extend_from_slice(&kind.to_ne_bytes());
        out.extend_from_slice(value);
        out.resize(align(length), 0);
        out
    }

    fn link_payload(attributes: &[Vec<u8>]) -> Vec<u8> {
        let mut payload = vec![0u8; IFINFOMSG];
        payload[4..8].copy_from_slice(&3i32.to_ne_bytes());
        payload[8..12].copy_from_slice(&IFF_UP.to_ne_bytes());
        for attribute in attributes {
            payload.extend_from_slice(attribute);
        }
        payload
    }

    #[test]
    fn a_link_carries_its_name_state_and_counters() {
        let mut stats = vec![0u8; 24 * 8];
        stats[0..8].copy_from_slice(&1_000u64.to_ne_bytes()); // rx_packets
        stats[16..24].copy_from_slice(&64_000u64.to_ne_bytes()); // rx_bytes
        stats[64..72].copy_from_slice(&7u64.to_ne_bytes()); // multicast

        let payload = link_payload(&[
            attribute(IFLA_IFNAME, b"eth0\0"),
            attribute(IFLA_MTU, &9000u32.to_ne_bytes()),
            attribute(IFLA_ADDRESS, &[0x2c, 0xdd, 0xe9, 0x12, 0x00, 0xa1]),
            attribute(IFLA_OPERSTATE, &[IF_OPER_UP]),
            attribute(IFLA_STATS64, &stats),
        ]);

        let link = parse_link(&payload).expect("a link");
        assert_eq!(link.name, "eth0");
        assert_eq!(link.index, 3);
        assert!(link.admin_up());
        assert_eq!(link.mtu, Some(9000));
        assert_eq!(link.operstate, IF_OPER_UP);
        assert_eq!(
            link.address.as_deref(),
            Some(&[0x2c, 0xdd, 0xe9, 0x12, 0x00, 0xa1][..])
        );
        let stats = link.stats.expect("counters");
        assert_eq!(stats.rx_packets, 1_000);
        assert_eq!(stats.rx_bytes, 64_000);
        assert_eq!(stats.multicast, 7);
    }

    #[test]
    fn a_nested_attribute_says_what_kind_of_device_it_is() {
        let nested = attribute(IFLA_INFO_KIND, b"vlan\0");
        let payload = link_payload(&[
            attribute(IFLA_IFNAME, b"vlan10\0"),
            attribute(IFLA_LINKINFO, &nested),
        ]);
        let link = parse_link(&payload).expect("a link");
        assert_eq!(link.kind.as_deref(), Some("vlan"));
    }

    /// The kernel grows this struct. A build that knows about fourteen fields
    /// must read a message carrying twenty-four, and one carrying ten.
    #[test]
    fn counters_are_read_by_offset_and_tolerate_a_different_length() {
        let mut short = vec![0u8; 10 * 8];
        short[0..8].copy_from_slice(&5u64.to_ne_bytes());
        let stats = parse_stats(&short).expect("counters");
        assert_eq!(stats.rx_packets, 5);
        assert_eq!(stats.rx_crc_errors, 0);

        let long = vec![0u8; 40 * 8];
        assert!(parse_stats(&long).is_some());
        assert!(parse_stats(&[0u8; 8]).is_none());
    }

    #[test]
    fn an_attribute_whose_length_is_a_lie_stops_the_walk() {
        // A length claiming more than the buffer holds.
        let mut bytes = attribute(IFLA_IFNAME, b"eth0\0");
        bytes[0..2].copy_from_slice(&999u16.to_ne_bytes());
        assert_eq!(attributes(&bytes).count(), 0);

        // A length shorter than its own header.
        let mut bytes = attribute(IFLA_MTU, &1500u32.to_ne_bytes());
        bytes[0..2].copy_from_slice(&2u16.to_ne_bytes());
        assert_eq!(attributes(&bytes).count(), 0);
    }

    #[test]
    fn a_truncated_message_is_dropped_rather_than_read_past() {
        assert!(split_message(&[0u8; 4]).is_none());
        let mut message = vec![0u8; 32];
        message[0..4].copy_from_slice(&4u32.to_ne_bytes());
        assert!(split_message(&message).is_none());
        message[0..4].copy_from_slice(&999u32.to_ne_bytes());
        assert!(split_message(&message).is_none());
    }

    #[test]
    fn an_address_prefers_the_local_end_of_a_point_to_point_link() {
        let mut payload = vec![0u8; IFADDRMSG];
        payload[0] = libc::AF_INET as u8;
        payload[1] = 30;
        payload[4..8].copy_from_slice(&3u32.to_ne_bytes());
        payload.extend_from_slice(&attribute(IFA_ADDRESS, &[203, 0, 113, 1]));
        payload.extend_from_slice(&attribute(IFA_LOCAL, &[203, 0, 113, 2]));
        payload.extend_from_slice(&attribute(IFA_BROADCAST, &[203, 0, 113, 3]));

        let address = parse_address(&payload).expect("an address");
        assert_eq!(address.index, 3);
        assert_eq!(address.prefix, "203.0.113.2/30");
        assert_eq!(address.broadcast.as_deref(), Some("203.0.113.3"));
    }

    #[test]
    fn a_v6_address_is_written_the_way_v6_addresses_are_written() {
        let mut payload = vec![0u8; IFADDRMSG];
        payload[0] = libc::AF_INET6 as u8;
        payload[1] = 64;
        payload[4..8].copy_from_slice(&3u32.to_ne_bytes());
        let mut bytes = [0u8; 16];
        bytes[0] = 0x20;
        bytes[1] = 0x01;
        bytes[2] = 0x0d;
        bytes[3] = 0xb8;
        bytes[15] = 1;
        payload.extend_from_slice(&attribute(IFA_ADDRESS, &bytes));

        let address = parse_address(&payload).expect("an address");
        assert_eq!(address.prefix, "2001:db8::1/64");
        assert_eq!(address.broadcast, None);
    }

    #[test]
    fn a_dump_request_asks_for_everything_once() {
        let request = dump_request(RTM_GETLINK, libc::AF_UNSPEC as u8, 7);
        assert_eq!(request.len(), NLMSG_HEADER + IFINFOMSG);
        assert_eq!(
            u32::from_ne_bytes(request[0..4].try_into().unwrap()),
            request.len() as u32
        );
        assert_eq!(
            u16::from_ne_bytes(request[4..6].try_into().unwrap()),
            RTM_GETLINK
        );
        assert_eq!(
            u16::from_ne_bytes(request[6..8].try_into().unwrap()),
            NLM_F_REQUEST | NLM_F_DUMP
        );
        assert_eq!(u32::from_ne_bytes(request[8..12].try_into().unwrap()), 7);
    }
}
