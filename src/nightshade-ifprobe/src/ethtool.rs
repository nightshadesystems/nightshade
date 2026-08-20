//! `SIOCETHTOOL`, by hand.
//!
//! Everything the kernel will say about a port that rtnetlink will not: speed,
//! duplex, what both ends advertised, flow control, the driver's own counters,
//! FEC, and the module's EEPROM.
//!
//! # The shape of every call
//!
//! One ioctl on any socket, with an `ifreq` naming the interface and pointing
//! at a command buffer whose first four bytes are the command number. The
//! kernel writes its answer back into that buffer. So the whole of this module
//! is: build a buffer, call [`Ethtool::call`], read fields back out at fixed
//! offsets.
//!
//! The buffers are built as byte vectors with explicit offsets rather than as
//! `#[repr(C)]` structs. That is deliberate: several of these structures end
//! in a variable-length array, one of them is negotiated over two calls, and
//! writing them as bytes means the offsets are visible next to the field names
//! from `ethtool.h` instead of being implied by a layout the compiler chose.
//!
//! # Every call may fail, and that is normal
//!
//! A virtual interface has no driver to ask; a driver may implement four of
//! these nine commands. Every method returns an `Option` and `None` means "not
//! answered", which the renderers turn into an absent line rather than a zero.

use std::collections::BTreeMap;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

use nix::libc;

const SIOCETHTOOL: libc::c_ulong = 0x8946;

const ETHTOOL_GDRVINFO: u32 = 0x00000003;
const ETHTOOL_GPAUSEPARAM: u32 = 0x00000012;
const ETHTOOL_GSTRINGS: u32 = 0x0000001b;
const ETHTOOL_GSTATS: u32 = 0x0000001d;
const ETHTOOL_GPERMADDR: u32 = 0x00000020;
const ETHTOOL_GMODULEINFO: u32 = 0x00000042;
const ETHTOOL_GMODULEEEPROM: u32 = 0x00000043;
const ETHTOOL_GLINKSETTINGS: u32 = 0x0000004c;
const ETHTOOL_GFECPARAM: u32 = 0x00000050;

const ETH_SS_STATS: u32 = 1;
const ETH_GSTRING_LEN: usize = 32;

/// `struct ifreq` is 40 bytes on the only architecture this ships on, and the
/// kernel copies exactly that many in. The buffer is larger so that a
/// mistaken size cannot be a read past the end of an allocation.
const IFREQ: usize = 64;
const IFNAMSIZ: usize = 16;

/// Caps on what a driver may claim, checked before anything is allocated.
///
/// `n_stats` is a `u32` the driver fills in. A driver that returns nonsense --
/// or a struct read from a kernel whose layout moved -- would otherwise be a
/// multi-gigabyte allocation inside a daemon running as root.
const MAX_STATS: u32 = 4_096;
const MAX_EEPROM: u32 = 8 * 1024;
const MAX_LINK_MODE_WORDS: i8 = 32;

/// Offsets into the command structures, named for the fields in `ethtool.h`.
///
/// Written out rather than derived from a `#[repr(C)]` struct so that a
/// reviewer can check them against the header without compiling anything.
/// `struct ethtool_link_settings` is 48 bytes of fixed fields followed by the
/// three bitmaps.
const LINK_SETTINGS: usize = 48;
/// `__s8 link_mode_masks_nwords`, the tenth byte-sized field.
const LINK_SETTINGS_NWORDS: usize = 15;
/// `struct ethtool_drvinfo`: `cmd` plus five 32-byte strings, twelve reserved
/// bytes and `n_priv_flags` come before `n_stats`.
const DRVINFO: usize = 196;
const DRVINFO_DRIVER: usize = 4;
const DRVINFO_BUS_INFO: usize = 100;
const DRVINFO_N_STATS: usize = 180;
/// `struct ethtool_modinfo`: `cmd`, `type`, `eeprom_len`, `reserved[8]`.
const MODINFO: usize = 44;

/// What `ETHTOOL_GLINKSETTINGS` came back with.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LinkSettings {
    /// Megabits per second. `u32::MAX` from the kernel means "unknown" and is
    /// turned into `None` here.
    pub speed_mbps: Option<u64>,
    /// 0 half, 1 full; anything else is unknown.
    pub duplex: Option<bool>,
    pub autoneg: bool,
    pub port: u8,
    /// Link mode bitmaps, in the kernel's bit order.
    pub supported: Vec<u32>,
    pub advertising: Vec<u32>,
    pub link_partner: Vec<u32>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DriverInfo {
    pub driver: String,
    pub bus_info: String,
    pub n_stats: u32,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Pause {
    pub autoneg: bool,
    pub rx: bool,
    pub tx: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Module {
    /// `ETH_MODULE_SFF_8079` and friends, as the kernel numbers them.
    pub kind: u32,
    pub bytes: Vec<u8>,
}

/// A socket to hang ioctls off.
pub struct Ethtool {
    fd: OwnedFd,
}

impl Ethtool {
    pub fn open() -> io::Result<Self> {
        // Any socket will do; ethtool ioctls are not routed. AF_INET because
        // it exists on every kernel this will ever run on.
        // SAFETY: socket(2) with constant arguments; the descriptor is adopted
        // immediately.
        let raw = unsafe {
            libc::socket(libc::AF_INET, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, 0)
        };
        if raw < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `raw` is a fresh descriptor this call owns.
        Ok(Self {
            fd: unsafe { OwnedFd::from_raw_fd(raw) },
        })
    }

    /// One ioctl. `command` is both the input and the output buffer.
    fn call(&self, name: &str, command: &mut [u8]) -> io::Result<()> {
        // A name that does not fit is not an interface name, and truncating it
        // would ask the kernel about a different interface.
        if name.is_empty() || name.len() >= IFNAMSIZ {
            return Err(io::Error::from(io::ErrorKind::InvalidInput));
        }

        let mut request = [0u8; IFREQ];
        request[..name.len()].copy_from_slice(name.as_bytes());
        let pointer = command.as_mut_ptr() as usize;
        request[IFNAMSIZ..IFNAMSIZ + std::mem::size_of::<usize>()]
            .copy_from_slice(&pointer.to_ne_bytes());

        // SAFETY: `request` is a correctly laid out `struct ifreq` whose data
        // pointer refers to `command`, which outlives the call and is
        // writable for its whole length.
        let result = unsafe {
            libc::ioctl(
                self.fd.as_raw_fd(),
                SIOCETHTOOL as _,
                request.as_mut_ptr() as *mut libc::c_void,
            )
        };
        if result < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// Speed, duplex, and the three advertisement bitmaps.
    ///
    /// Two calls. The first passes a word count of zero and the kernel answers
    /// with the negative of the count it wants; the second passes that count
    /// and gets the bitmaps. That handshake is the ABI, not a retry.
    pub fn link_settings(&self, name: &str) -> Option<LinkSettings> {
        let mut probe = vec![0u8; LINK_SETTINGS];
        put_u32(&mut probe, 0, ETHTOOL_GLINKSETTINGS);
        self.call(name, &mut probe).ok()?;

        // `link_mode_masks_nwords` comes back as the negative of the count the
        // kernel wants. A non-negative answer means the handshake did not
        // happen and the buffer holds nothing to read.
        let words = (probe[LINK_SETTINGS_NWORDS] as i8).checked_neg()?;
        if words <= 0 || words > MAX_LINK_MODE_WORDS {
            return None;
        }
        let words = words as usize;

        let mut buffer = vec![0u8; LINK_SETTINGS + words * 3 * 4];
        put_u32(&mut buffer, 0, ETHTOOL_GLINKSETTINGS);
        buffer[LINK_SETTINGS_NWORDS] = words as u8;
        self.call(name, &mut buffer).ok()?;

        let speed = get_u32(&buffer, 4)?;
        let masks = |which: usize| -> Vec<u32> {
            (0..words)
                .filter_map(|word| {
                    get_u32(&buffer, LINK_SETTINGS + (which * words + word) * 4)
                })
                .collect()
        };

        Some(LinkSettings {
            // The kernel writes 0 or `u32::MAX` for a port with no link.
            speed_mbps: (speed != 0 && speed != u32::MAX).then_some(speed as u64),
            duplex: match buffer[8] {
                0 => Some(false),
                1 => Some(true),
                _ => None,
            },
            port: buffer[9],
            autoneg: buffer[11] == 1,
            supported: masks(0),
            advertising: masks(1),
            link_partner: masks(2),
        })
    }

    /// The driver's name and how many statistics it keeps.
    pub fn driver(&self, name: &str) -> Option<DriverInfo> {
        let mut buffer = vec![0u8; DRVINFO];
        put_u32(&mut buffer, 0, ETHTOOL_GDRVINFO);
        self.call(name, &mut buffer).ok()?;
        Some(DriverInfo {
            driver: fixed_string(&buffer, DRVINFO_DRIVER, 32),
            bus_info: fixed_string(&buffer, DRVINFO_BUS_INFO, 32),
            n_stats: get_u32(&buffer, DRVINFO_N_STATS)?,
        })
    }

    /// The driver's own counters, by the driver's own names.
    ///
    /// Two calls again: the names, then the values, matched by position. A
    /// driver whose two answers disagree about the count is not read at all
    /// -- pairing them off anyway would attribute one counter's value to
    /// another counter's name, which is worse than having neither.
    pub fn statistics(&self, name: &str) -> Option<BTreeMap<String, u64>> {
        let count = self.driver(name)?.n_stats;
        if count == 0 || count > MAX_STATS {
            return None;
        }
        let count = count as usize;

        let mut names = vec![0u8; 12 + count * ETH_GSTRING_LEN];
        put_u32(&mut names, 0, ETHTOOL_GSTRINGS);
        put_u32(&mut names, 4, ETH_SS_STATS);
        put_u32(&mut names, 8, count as u32);
        self.call(name, &mut names).ok()?;

        let mut values = vec![0u8; 8 + count * 8];
        put_u32(&mut values, 0, ETHTOOL_GSTATS);
        put_u32(&mut values, 4, count as u32);
        self.call(name, &mut values).ok()?;

        let mut statistics = BTreeMap::new();
        for index in 0..count {
            let label = fixed_string(&names, 12 + index * ETH_GSTRING_LEN, ETH_GSTRING_LEN);
            if label.is_empty() {
                continue;
            }
            if let Some(value) = get_u64(&values, 8 + index * 8) {
                statistics.insert(label, value);
            }
        }
        Some(statistics)
    }

    /// `ethtool -a`.
    pub fn pause(&self, name: &str) -> Option<Pause> {
        let mut buffer = vec![0u8; 16];
        put_u32(&mut buffer, 0, ETHTOOL_GPAUSEPARAM);
        self.call(name, &mut buffer).ok()?;
        Some(Pause {
            autoneg: get_u32(&buffer, 4)? != 0,
            rx: get_u32(&buffer, 8)? != 0,
            tx: get_u32(&buffer, 12)? != 0,
        })
    }

    /// The module's EEPROM, all of it.
    pub fn module(&self, name: &str) -> Option<Module> {
        let mut info = vec![0u8; MODINFO];
        put_u32(&mut info, 0, ETHTOOL_GMODULEINFO);
        self.call(name, &mut info).ok()?;

        let kind = get_u32(&info, 4)?;
        let length = get_u32(&info, 8)?;
        if length == 0 || length > MAX_EEPROM {
            return None;
        }

        let mut buffer = vec![0u8; 16 + length as usize];
        put_u32(&mut buffer, 0, ETHTOOL_GMODULEEEPROM);
        put_u32(&mut buffer, 8, 0); // offset
        put_u32(&mut buffer, 12, length); // len
        self.call(name, &mut buffer).ok()?;

        Some(Module {
            kind,
            bytes: buffer[16..].to_vec(),
        })
    }

    /// The FEC mode in force, as a bitmask of `ETHTOOL_FEC_*` bits.
    pub fn fec(&self, name: &str) -> Option<u32> {
        let mut buffer = vec![0u8; 16];
        put_u32(&mut buffer, 0, ETHTOOL_GFECPARAM);
        self.call(name, &mut buffer).ok()?;
        get_u32(&buffer, 4)
    }

    /// The address burned into the hardware, for the `bia` field.
    pub fn permanent_address(&self, name: &str) -> Option<Vec<u8>> {
        const MAX: usize = 32;
        let mut buffer = vec![0u8; 8 + MAX];
        put_u32(&mut buffer, 0, ETHTOOL_GPERMADDR);
        put_u32(&mut buffer, 4, MAX as u32);
        self.call(name, &mut buffer).ok()?;
        let size = get_u32(&buffer, 4)? as usize;
        if size == 0 || size > MAX {
            return None;
        }
        Some(buffer[8..8 + size].to_vec())
    }
}

// -- reading and writing the command buffers --------------------------------

fn put_u32(buffer: &mut [u8], at: usize, value: u32) {
    if let Some(slot) = buffer.get_mut(at..at + 4) {
        slot.copy_from_slice(&value.to_ne_bytes());
    }
}

fn get_u32(buffer: &[u8], at: usize) -> Option<u32> {
    buffer
        .get(at..at + 4)
        .map(|bytes| u32::from_ne_bytes(bytes.try_into().expect("four bytes")))
}

fn get_u64(buffer: &[u8], at: usize) -> Option<u64> {
    buffer
        .get(at..at + 8)
        .map(|bytes| u64::from_ne_bytes(bytes.try_into().expect("eight bytes")))
}

/// A fixed-width, possibly NUL-padded field.
fn fixed_string(buffer: &[u8], at: usize, width: usize) -> String {
    let Some(bytes) = buffer.get(at..at + width) else {
        return String::new();
    };
    let end = bytes.iter().position(|byte| *byte == 0).unwrap_or(width);
    String::from_utf8_lossy(&bytes[..end]).trim().to_string()
}

// -- link mode bits ---------------------------------------------------------

/// The link modes this maps to a speed and a duplex.
///
/// From `ETHTOOL_LINK_MODE_*_BIT` in `ethtool.h`. Not the whole list -- the
/// backplane and multi-lane variants of a speed report the same speed as the
/// pluggable one, so the table carries one entry per (speed, duplex, medium)
/// that a port on this class of box can come up at, and anything else falls
/// through and is left out of the capability list rather than guessed at.
pub const LINK_MODES: &[(u32, u64, bool)] = &[
    (0, 10, false),
    (1, 10, true),
    (2, 100, false),
    (3, 100, true),
    (4, 1_000, false),
    (5, 1_000, true),
    (12, 10_000, true),
    (15, 2_500, true),
    (17, 1_000, true),
    (18, 10_000, true),
    (19, 10_000, true),
    (23, 40_000, true),
    (24, 40_000, true),
    (25, 40_000, true),
    (26, 40_000, true),
    (31, 25_000, true),
    (32, 25_000, true),
    (33, 25_000, true),
    (34, 50_000, true),
    (35, 50_000, true),
    (36, 100_000, true),
    (37, 100_000, true),
    (38, 100_000, true),
    (39, 100_000, true),
    (40, 50_000, true),
    (41, 1_000, true),
    (42, 10_000, true),
    (43, 10_000, true),
    (44, 10_000, true),
    (45, 10_000, true),
    (46, 10_000, true),
    (47, 2_500, true),
    (48, 5_000, true),
];

/// `ETHTOOL_LINK_MODE_Autoneg_BIT`.
pub const LINK_MODE_AUTONEG: u32 = 6;
/// `ETHTOOL_LINK_MODE_Pause_BIT`.
pub const LINK_MODE_PAUSE: u32 = 13;
/// `ETHTOOL_LINK_MODE_Asym_Pause_BIT`.
pub const LINK_MODE_ASYM_PAUSE: u32 = 14;

/// Whether `bit` is set in a link-mode bitmap.
pub fn mode_is_set(mask: &[u32], bit: u32) -> bool {
    let word = (bit / 32) as usize;
    mask.get(word)
        .is_some_and(|word| word & (1 << (bit % 32)) != 0)
}

/// Every (speed, duplex) pair a bitmap carries, ascending and deduplicated.
///
/// Deduplicated because a copper port advertises `1000baseT_Full` and
/// `1000baseKX_Full` and an operator does not want to read `1G/full` twice.
pub fn speeds(mask: &[u32]) -> Vec<(u64, bool)> {
    let mut found: Vec<(u64, bool)> = LINK_MODES
        .iter()
        .filter(|(bit, _, _)| mode_is_set(mask, *bit))
        .map(|(_, speed, full)| (*speed, *full))
        .collect();
    found.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
    found.dedup();
    found
}

/// `None`, `Symmetric` or `Asymmetric`, as an advertisement describes it.
pub fn pause_advertisement(mask: &[u32]) -> &'static str {
    match (
        mode_is_set(mask, LINK_MODE_PAUSE),
        mode_is_set(mask, LINK_MODE_ASYM_PAUSE),
    ) {
        (true, _) => "Symmetric",
        (false, true) => "Asymmetric",
        (false, false) => "None",
    }
}

/// The FEC bits, as `ethtool.h` numbers them.
///
/// `ETHTOOL_FEC_NONE_BIT` (0), `_AUTO_BIT` (1) and `_OFF_BIT` (2) all mean
/// nothing is correcting anything, and so are not named here -- they are the
/// fall-through in [`fec_name`].
const FEC_RS: u32 = 1 << 3;
const FEC_BASER: u32 = 1 << 4;
const FEC_LLRS: u32 = 1 << 5;

/// What to print in the `FEC mode` row.
pub fn fec_name(active: u32) -> &'static str {
    if active & FEC_RS != 0 {
        "Reed-Solomon"
    } else if active & FEC_BASER != 0 {
        "Fire code"
    } else if active & FEC_LLRS != 0 {
        "Low latency Reed-Solomon"
    } else {
        // `ETHTOOL_FEC_OFF_BIT` and a driver that answered with no bits at all
        // mean the same thing to a reader: nothing is correcting anything.
        "Disabled"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bit_is_looked_up_in_the_word_it_lives_in() {
        let mask = [0b0000_0010u32, 0b0000_0001u32];
        assert!(mode_is_set(&mask, 1));
        assert!(!mode_is_set(&mask, 0));
        assert!(mode_is_set(&mask, 32));
        assert!(!mode_is_set(&mask, 33));
        // Past the end of the bitmap is not set, rather than a panic.
        assert!(!mode_is_set(&mask, 1_000));
        assert!(!mode_is_set(&[], 0));
    }

    #[test]
    fn a_copper_ports_modes_come_back_in_order_and_once_each() {
        // 10/100/1000 half and full, plus 1000baseKX_Full which is the same
        // speed and duplex by another name.
        let mask = [0b0011_1111u32 | (1 << 17)];
        assert_eq!(
            speeds(&mask),
            [
                (10, false),
                (10, true),
                (100, false),
                (100, true),
                (1_000, false),
                (1_000, true)
            ]
        );
    }

    #[test]
    fn pause_is_named_by_which_bits_are_set() {
        assert_eq!(pause_advertisement(&[0]), "None");
        assert_eq!(pause_advertisement(&[1 << LINK_MODE_PAUSE]), "Symmetric");
        assert_eq!(
            pause_advertisement(&[1 << LINK_MODE_ASYM_PAUSE]),
            "Asymmetric"
        );
    }

    #[test]
    fn fec_is_named_by_the_strongest_mode_in_force() {
        assert_eq!(fec_name(FEC_RS), "Reed-Solomon");
        assert_eq!(fec_name(FEC_BASER), "Fire code");
        // `ETHTOOL_FEC_OFF_BIT`, which is a real answer and not an absent one.
        assert_eq!(fec_name(1 << 2), "Disabled");
        assert_eq!(fec_name(0), "Disabled");
        assert_eq!(fec_name(FEC_RS | (1 << 2)), "Reed-Solomon");
    }

    #[test]
    fn a_fixed_width_field_stops_at_its_nul() {
        let mut buffer = vec![0u8; 40];
        buffer[4..8].copy_from_slice(b"ixgb");
        assert_eq!(fixed_string(&buffer, 4, 32), "ixgb");
        assert_eq!(fixed_string(&buffer, 4, 4), "ixgb");
        // Past the end is empty rather than a panic.
        assert_eq!(fixed_string(&buffer, 4, 100), "");
    }

    #[test]
    fn writing_and_reading_a_command_buffer_stays_inside_it() {
        let mut buffer = vec![0u8; 8];
        put_u32(&mut buffer, 0, ETHTOOL_GDRVINFO);
        put_u32(&mut buffer, 100, 7); // out of range: ignored
        assert_eq!(get_u32(&buffer, 0), Some(ETHTOOL_GDRVINFO));
        assert_eq!(get_u32(&buffer, 100), None);
        assert_eq!(get_u64(&buffer, 4), None);
    }

    /// An interface name is a fixed-size kernel field, and a name that does
    /// not fit is not a name to truncate.
    #[test]
    fn an_oversized_interface_name_is_refused_rather_than_cut() {
        let Ok(ethtool) = Ethtool::open() else {
            // No AF_INET socket: nothing to test against on this box.
            return;
        };
        let mut buffer = vec![0u8; 16];
        assert!(ethtool.call(&"e".repeat(64), &mut buffer).is_err());
        assert!(ethtool.call("", &mut buffer).is_err());
    }
}
