//! `/sys/class/net`, as a fallback and for the few things only it knows.
//!
//! Netlink and ethtool are the primary sources and this is not a third one. It
//! is here for two reasons:
//!
//! - **Bond membership by name.** Netlink gives the master's *index*; turning
//!   that into a name means keeping the whole link table around. `/sys` names
//!   it directly, and the bond driver's own files are the only place the
//!   fallback mode and the active member are written down at all.
//! - **Degrading gracefully.** In a container, under a hypervisor with a
//!   paravirtual NIC, or on a kernel built without `ETHTOOL_GLINKSETTINGS`,
//!   the ioctls fail and `speed`, `duplex` and `carrier` are still readable.
//!   `show interfaces` answering with less is better than it not answering.
//!
//! Nothing here fails. A file that is not there is a `None`.

use std::path::{Path, PathBuf};

/// Where the kernel publishes its interfaces, rooted so tests can point it at
/// a directory they built.
#[derive(Debug, Clone)]
pub struct SysFs {
    root: PathBuf,
}

impl SysFs {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    fn interface(&self, name: &str) -> PathBuf {
        self.root.join(name)
    }

    fn read(&self, name: &str, file: &str) -> Option<String> {
        let text = std::fs::read_to_string(self.interface(name).join(file)).ok()?;
        let text = text.trim().to_string();
        (!text.is_empty()).then_some(text)
    }

    /// Whether the kernel has this device at all.
    pub fn exists(&self, name: &str) -> bool {
        self.interface(name).exists()
    }

    /// Every device the kernel has.
    pub fn names(&self) -> Vec<String> {
        let Ok(entries) = std::fs::read_dir(&self.root) else {
            return Vec::new();
        };
        entries
            .flatten()
            .filter_map(|entry| entry.file_name().to_str().map(str::to_string))
            .collect()
    }

    /// Megabits per second, when the driver will say.
    ///
    /// A port with no link reports `-1` here rather than nothing, which is the
    /// one value that must not be read as a speed.
    pub fn speed(&self, name: &str) -> Option<u64> {
        let raw: i64 = self.read(name, "speed")?.parse().ok()?;
        (raw > 0).then_some(raw as u64)
    }

    pub fn duplex(&self, name: &str) -> Option<bool> {
        match self.read(name, "duplex")?.as_str() {
            "full" => Some(true),
            "half" => Some(false),
            _ => None,
        }
    }

    pub fn operstate(&self, name: &str) -> Option<String> {
        self.read(name, "operstate")
    }

    pub fn carrier(&self, name: &str) -> Option<bool> {
        Some(self.read(name, "carrier")? == "1")
    }

    pub fn mtu(&self, name: &str) -> Option<u32> {
        self.read(name, "mtu")?.parse().ok()
    }

    pub fn address(&self, name: &str) -> Option<String> {
        self.read(name, "address")
    }

    /// The bond this port is enslaved to, by name.
    pub fn master(&self, name: &str) -> Option<String> {
        let target = std::fs::read_link(self.interface(name).join("master")).ok()?;
        Some(file_name(&target))
    }

    /// The members of a bond, in the order the bond driver lists them.
    pub fn bond_members(&self, name: &str) -> Vec<String> {
        self.read(name, "bonding/slaves")
            .map(|text| text.split_whitespace().map(str::to_string).collect())
            .unwrap_or_default()
    }

    /// Whether the bond will fall back to a single member when LACP does not
    /// come up. Only some bonding modes have it, so absent is normal.
    pub fn bond_fallback(&self, name: &str) -> Option<String> {
        // `all_slaves_active` is the closest thing the Linux bonding driver
        // has to EOS's fallback: the file reads `0` or `1`, and `1` means the
        // bond keeps forwarding on members LACP has not agreed on.
        match self.read(name, "bonding/all_slaves_active")?.as_str() {
            "1" => Some("on".to_string()),
            _ => Some("off".to_string()),
        }
    }

    /// The VLANs carried on `name`, found by asking every VLAN which parent it
    /// has. This is what makes the `Vlan` column say `trunk`.
    pub fn vlan_parent(&self, name: &str) -> Option<String> {
        // `/sys/class/net/vlan10/lower_eth1` is a symlink the kernel makes for
        // every device a VLAN sits on; there is exactly one.
        let entries = std::fs::read_dir(self.interface(name)).ok()?;
        entries.flatten().find_map(|entry| {
            entry
                .file_name()
                .to_str()
                .and_then(|file| file.strip_prefix("lower_"))
                .map(str::to_string)
        })
    }

    /// The driver behind a port, for the per-driver counter aliases.
    pub fn driver(&self, name: &str) -> Option<String> {
        let target = std::fs::read_link(self.interface(name).join("device/driver")).ok()?;
        Some(file_name(&target))
    }
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default()
        .to_string()
}

/// The platform's own name for itself, for the `Model` row.
///
/// DMI, which is where a server or an appliance records what it is. A board
/// with none -- a virtual machine, most single-board computers -- has no model
/// to print and gets none.
pub fn dmi_model(root: &Path) -> Option<String> {
    let dmi = root.join("sys/class/dmi/id");
    for file in ["product_name", "board_name"] {
        if let Ok(text) = std::fs::read_to_string(dmi.join(file)) {
            let text = text.trim();
            // The strings motherboard vendors ship when they did not bother.
            if !text.is_empty()
                && !text.eq_ignore_ascii_case("To be filled by O.E.M.")
                && !text.eq_ignore_ascii_case("Default string")
                && !text.eq_ignore_ascii_case("System Product Name")
            {
                return Some(text.to_string());
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake() -> (tempfile::TempDir, SysFs) {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let root = dir.path().join("sys/class/net");
        std::fs::create_dir_all(root.join("eth0")).expect("a directory");
        std::fs::write(root.join("eth0/speed"), "10000\n").expect("a file");
        std::fs::write(root.join("eth0/duplex"), "full\n").expect("a file");
        std::fs::write(root.join("eth0/operstate"), "up\n").expect("a file");
        std::fs::write(root.join("eth0/carrier"), "1\n").expect("a file");
        std::fs::write(root.join("eth0/mtu"), "9214\n").expect("a file");
        std::fs::write(root.join("eth0/address"), "2c:dd:e9:12:00:a1\n").expect("a file");
        let sysfs = SysFs::new(root);
        (dir, sysfs)
    }

    #[test]
    fn what_the_kernel_publishes_is_read_back() {
        let (_dir, sysfs) = fake();
        assert_eq!(sysfs.speed("eth0"), Some(10_000));
        assert_eq!(sysfs.duplex("eth0"), Some(true));
        assert_eq!(sysfs.operstate("eth0").as_deref(), Some("up"));
        assert_eq!(sysfs.carrier("eth0"), Some(true));
        assert_eq!(sysfs.mtu("eth0"), Some(9_214));
        assert_eq!(sysfs.address("eth0").as_deref(), Some("2c:dd:e9:12:00:a1"));
        assert!(sysfs.exists("eth0"));
        assert_eq!(sysfs.names(), ["eth0"]);
    }

    /// The one value that must not be read as a speed.
    #[test]
    fn a_port_with_no_link_has_no_speed_rather_than_a_negative_one() {
        let (dir, sysfs) = fake();
        std::fs::write(dir.path().join("sys/class/net/eth0/speed"), "-1\n").expect("a file");
        assert_eq!(sysfs.speed("eth0"), None);
    }

    #[test]
    fn a_missing_file_is_an_absent_value_and_not_a_failure() {
        let (_dir, sysfs) = fake();
        assert_eq!(sysfs.speed("eth9"), None);
        assert_eq!(sysfs.duplex("eth9"), None);
        assert_eq!(sysfs.master("eth0"), None);
        assert!(sysfs.bond_members("eth0").is_empty());
        assert!(!sysfs.exists("eth9"));
    }

    #[test]
    fn a_bond_lists_what_is_in_it() {
        let (dir, sysfs) = fake();
        let bond = dir.path().join("sys/class/net/bond0/bonding");
        std::fs::create_dir_all(&bond).expect("a directory");
        std::fs::write(bond.join("slaves"), "eth3 eth4\n").expect("a file");
        std::fs::write(bond.join("all_slaves_active"), "0\n").expect("a file");
        assert_eq!(sysfs.bond_members("bond0"), ["eth3", "eth4"]);
        assert_eq!(sysfs.bond_fallback("bond0").as_deref(), Some("off"));
    }

    #[test]
    fn a_vlan_knows_what_it_sits_on() {
        let (dir, sysfs) = fake();
        let vlan = dir.path().join("sys/class/net/vlan10");
        std::fs::create_dir_all(vlan.join("lower_eth1")).expect("a directory");
        assert_eq!(sysfs.vlan_parent("vlan10").as_deref(), Some("eth1"));
        assert_eq!(sysfs.vlan_parent("eth0"), None);
    }

    #[test]
    fn a_board_that_will_not_say_what_it_is_gets_no_model() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let dmi = dir.path().join("sys/class/dmi/id");
        std::fs::create_dir_all(&dmi).expect("a directory");
        assert_eq!(dmi_model(dir.path()), None);

        std::fs::write(dmi.join("product_name"), "To be filled by O.E.M.\n").expect("a file");
        assert_eq!(dmi_model(dir.path()), None);

        std::fs::write(dmi.join("product_name"), "NS-FW-1U-8X10G\n").expect("a file");
        assert_eq!(dmi_model(dir.path()).as_deref(), Some("NS-FW-1U-8X10G"));
    }
}
