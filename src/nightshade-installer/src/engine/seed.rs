//! The configuration the installed system boots with.
//!
//! Until this ran, a fresh box had no `/etc/nightshade/config.boot` at all, so
//! configd started with nothing saved and `show configuration` printed a blank
//! screen on a machine that plainly had a host name and a set of ports. The
//! configuration should describe the box from the first login.
//!
//! # Naming the ports
//!
//! Physical ports are renamed to `eth0`, `eth1`, ... in slot order, and the
//! binding is written into the configuration as `hw-id` -- the port's permanent
//! MAC. That is what makes the name survive: a kernel upgrade, an added card,
//! or a BIOS that enumerates the bus differently can all change `enp1s0` into
//! `enp2s0`, and every one of them would silently move a firewall rule to a
//! different piece of copper. A MAC does not move.
//!
//! # Why the installer writes the `.link` files too
//!
//! configd renders exactly these files on every commit, so it owns them from
//! the first commit onwards. But the first *boot* happens before configd has
//! ever run, and udev reads `.link` files when the device appears -- far
//! earlier. Without them on disk already, the ports come up with kernel names,
//! the rendered `.network` files match nothing, and the box comes up with no
//! addresses on the very interfaces that were configured.
//!
//! So they are written here, and configd overwrites them with its own
//! rendering at the first commit. They only have to be right once. That is
//! also why this file formats them by hand rather than depending on
//! nightshade-render: see the note at the top of this crate's Cargo.toml about
//! the installer having no dependencies.

use std::fs;
use std::path::Path;

use crate::config::InstallConfig;
use crate::error::{IoContext, Result};
use crate::logging;
use crate::validate::DEFAULT_USER;

/// A physical port, as the kernel currently presents it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Port {
    /// What the kernel called it: `enp1s0`, `ens33`.
    pub kernel_name: String,
    /// Permanent MAC, lower case.
    pub mac: String,
    /// Bus address of the slot, used only to order the ports.
    pub slot: String,
}

const MANAGED_HEADER: &str = "\
# Managed by Nightshade. Do not edit.
#
# This file is generated from /etc/nightshade/config.boot and is rewritten on
# every commit. Changes made here are lost, and are not part of the config the
# next boot will apply.
";

/// Every physical ethernet port the box has, in the order they will be named.
///
/// Ordered by bus address rather than by the kernel's enumeration order, which
/// is a race between driver probes and is not the same twice. Sorting by slot
/// means `eth0` is the lowest-numbered port on the lowest-numbered bus -- which
/// is the one silkscreened `1` on almost every chassis.
pub fn enumerate_ports(sys_class_net: &Path) -> Vec<Port> {
    let Ok(entries) = fs::read_dir(sys_class_net) else {
        return Vec::new();
    };

    let mut ports: Vec<Port> = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let dir = entry.path();

        // A `device` symlink is what separates a real port from a bridge, a
        // bond, a tunnel, or `lo`. Virtual devices have no bus address, and
        // renaming one would collide with the name configd gives it anyway.
        let Ok(slot) = fs::read_link(dir.join("device")) else {
            continue;
        };
        let slot = slot
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();

        // Only permanently assigned addresses. `addr_assign_type` is 0 for a
        // burned-in MAC; anything else is random or generated and would change
        // under us, which is the one thing a name must not be pinned to.
        match read_trimmed(&dir.join("addr_assign_type")).as_deref() {
            Some("0") => {}
            _ => continue,
        }

        let Some(mac) = read_trimmed(&dir.join("address")) else {
            continue;
        };
        if mac.is_empty() || mac == "00:00:00:00:00:00" {
            continue;
        }

        ports.push(Port {
            kernel_name: name,
            mac: mac.to_ascii_lowercase(),
            slot,
        });
    }

    // Slot first, then name, so two ports behind one multifunction address
    // still land in a fixed order rather than whichever readdir returned first.
    ports.sort_by(|a, b| a.slot.cmp(&b.slot).then_with(|| a.kernel_name.cmp(&b.kernel_name)));
    ports
}

fn read_trimmed(path: &Path) -> Option<String> {
    fs::read_to_string(path).ok().map(|s| s.trim().to_string())
}

/// `eth0`, `eth1`, ... paired with the port each one names.
pub fn assign_names(ports: &[Port]) -> Vec<(String, &Port)> {
    ports
        .iter()
        .enumerate()
        .map(|(i, port)| (format!("eth{i}"), port))
        .collect()
}

/// The `config.boot` text for a freshly installed box.
///
/// Curly format, rendered in the same shape `nightshade-schema` emits: keys
/// sorted, four-space indent, quoted only where a value contains a space.
pub fn config_boot(config: &InstallConfig, named: &[(String, &Port)], hash: Option<&str>) -> String {
    let mut out = String::new();
    out.push_str(
        "/* Written by the Nightshade installer. This is the configuration the\n \
         * system boots with; edit it with `configure` rather than by hand. */\n",
    );

    // Always an interfaces block, because `lo` is always there. Ethernet
    // before loopback, which is the order the schema renders them in, so a
    // `save` straight after boot does not reshuffle the file.
    out.push_str("interfaces {\n");
    for (name, port) in named {
        out.push_str(&format!("    ethernet {name} {{\n"));
        out.push_str(&format!("        hw-id {}\n", port.mac));
        out.push_str("    }\n");
    }
    // The loopback, listed rather than assumed. Every box has one and nothing
    // needs to configure it, but a configuration that silently omits an
    // interface the box has is not a picture an operator can trust as
    // complete -- and `lo` is where a service binds when it is meant not to be
    // reachable, which is worth being able to see and to set an address on.
    out.push_str("    loopback lo {\n");
    out.push_str("    }\n");
    out.push_str("}\n");

    out.push_str("system {\n");
    out.push_str(&format!("    host-name {}\n", config.hostname));
    if let Some(hash) = hash {
        out.push_str("    login {\n");
        out.push_str(&format!("        user {DEFAULT_USER} {{\n"));
        out.push_str("            authentication {\n");
        // Quoted, via the Debug formatter. A crypt hash is full of `$`,
        // which the curly lexer does not accept in a bare word -- an
        // unquoted hash is a config.boot that does not parse, and a first
        // boot that silently falls back to defaults.
        //
        // `{:?}` on a `&str` emits the value in double quotes with `\` and
        // `"` escaped, which is exactly what `lex::quote_into` does when
        // configd re-renders this file on `save`. Safe here because the
        // schema already refuses a hash containing whitespace, a control
        // character or a colon, so there is nothing left for the two
        // escaping rules to disagree about.
        out.push_str(&format!("                encrypted-password {hash:?}\n"));
        out.push_str("            }\n");
        out.push_str("            full-name \"Nightshade administrator\"\n");
        out.push_str("        }\n");
        out.push_str("    }\n");
    }
    out.push_str("}\n");
    out
}

/// The `.link` file that renames one port, byte-identical to what
/// `nightshade-render` produces for the same configuration.
pub fn link_file(name: &str, port: &Port) -> String {
    format!(
        "{MANAGED_HEADER}\n[Match]\nPermanentMACAddress={}\n\n[Link]\nName={name}\n",
        port.mac
    )
}

/// The account's hash, read back out of the target's `/etc/shadow`.
///
/// Read rather than kept from the installer's own memory on purpose: what
/// belongs in the configuration is what the system actually ended up with. If
/// `chpasswd` applied a different crypt scheme than expected, the config should
/// say what is true, not what was intended.
pub fn hash_from_shadow(shadow: &str, user: &str) -> Option<String> {
    for line in shadow.lines() {
        let mut fields = line.split(':');
        if fields.next() == Some(user) {
            let hash = fields.next()?.trim();
            // An empty field is no password at all, and is not something to
            // copy into a config as though it were a credential.
            if hash.is_empty() {
                return None;
            }
            return Some(hash.to_string());
        }
    }
    None
}

/// Write `config.boot` and the first-boot `.link` files into the target.
pub fn write_configuration(config: &InstallConfig, target: &Path) -> Result<()> {
    let ports = enumerate_ports(Path::new("/sys/class/net"));
    let named = assign_names(&ports);

    if named.is_empty() {
        logging::info("no physical ports found; config.boot will list no interfaces");
    }
    for (name, port) in &named {
        logging::info(format!(
            "{name} is {} ({}, slot {})",
            port.mac, port.kernel_name, port.slot
        ));
    }

    let shadow = fs::read_to_string(target.join("etc/shadow")).unwrap_or_default();
    let hash = hash_from_shadow(&shadow, DEFAULT_USER);
    if hash.is_none() {
        // Not fatal: the box still boots and the account still works. What is
        // lost is the configuration describing it, which is worth a line in
        // the log rather than a failed install at the last step.
        logging::info(format!(
            "no password hash for {DEFAULT_USER} in the target's /etc/shadow; \
             config.boot will not describe the account"
        ));
    }

    // 0700/0600: config.boot carries the same crypt hash /etc/shadow does, and
    // /etc/shadow is not world-readable for a reason. systemd-tmpfiles keeps
    // the directory at this mode afterwards; this is what it is created with.
    let dir = target.join("etc/nightshade");
    fs::create_dir_all(&dir).ctx(format!("creating {}", dir.display()))?;
    fs::set_permissions(&dir, perms(0o700)).ctx("securing /etc/nightshade")?;

    let boot = dir.join("config.boot");
    fs::write(&boot, config_boot(config, &named, hash.as_deref()))
        .ctx(format!("writing {}", boot.display()))?;
    fs::set_permissions(&boot, perms(0o600)).ctx("securing config.boot")?;
    logging::info(format!("wrote {}", boot.display()));

    let links = target.join("etc/systemd/network");
    fs::create_dir_all(&links).ctx(format!("creating {}", links.display()))?;
    for (name, port) in &named {
        let path = links.join(format!("10-ns-{name}.link"));
        fs::write(&path, link_file(name, port)).ctx(format!("writing {}", path.display()))?;
        fs::set_permissions(&path, perms(0o644)).ctx("setting mode on a .link file")?;
    }
    logging::info(format!("wrote {} interface .link files", named.len()));

    Ok(())
}

fn perms(mode: u32) -> fs::Permissions {
    use std::os::unix::fs::PermissionsExt;
    fs::Permissions::from_mode(mode)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn install_config(hostname: &str) -> InstallConfig {
        InstallConfig {
            disks: Vec::new(),
            hostname: hostname.to_string(),
            password: crate::secret::SecretString::new(String::new()),
        }
    }

    fn port(kernel_name: &str, mac: &str, slot: &str) -> Port {
        Port {
            kernel_name: kernel_name.into(),
            mac: mac.into(),
            slot: slot.into(),
        }
    }

    /// Slot order, not the order readdir happened to return.
    #[test]
    fn ports_are_named_in_slot_order() {
        let ports = vec![
            port("ens192", "00:0c:29:00:00:03", "0000:0b:00.0"),
            port("ens33", "00:0c:29:00:00:01", "0000:02:01.0"),
            port("ens36", "00:0c:29:00:00:02", "0000:02:02.0"),
        ];
        let mut sorted = ports.clone();
        sorted.sort_by(|a, b| a.slot.cmp(&b.slot).then_with(|| a.kernel_name.cmp(&b.kernel_name)));
        let named = assign_names(&sorted);
        let pairs: Vec<(&str, &str)> = named
            .iter()
            .map(|(n, p)| (n.as_str(), p.kernel_name.as_str()))
            .collect();
        assert_eq!(
            pairs,
            [("eth0", "ens33"), ("eth1", "ens36"), ("eth2", "ens192")]
        );
    }

    #[test]
    fn a_link_file_pins_the_name_to_the_permanent_mac() {
        let p = port("ens33", "00:0c:29:00:00:01", "0000:02:01.0");
        let link = link_file("eth0", &p);
        assert!(link.starts_with("# Managed by Nightshade."), "{link}");
        assert!(
            link.contains("PermanentMACAddress=00:0c:29:00:00:01"),
            "{link}"
        );
        assert!(link.contains("Name=eth0"), "{link}");
        // Matching the kernel's name would defeat the point: that name is the
        // thing being replaced.
        assert!(!link.contains("OriginalName"), "{link}");
        assert!(!link.contains("ens33"), "{link}");
    }

    /// Byte-for-byte what `nightshade-render` produces for the same port.
    ///
    /// The other half of this assertion lives in
    /// `networkd::tests::a_pinned_link_file_is_exactly_this`. The installer
    /// cannot call the renderer -- this crate has no dependencies, on purpose
    /// -- so the two formats are kept honest by both being pinned to the same
    /// literal. If they drift, the first commit on a new box silently rewrites
    /// every `.link` file, which works but means the installed system and the
    /// committed system disagreed and nobody was told.
    #[test]
    fn a_link_file_matches_what_the_renderer_would_write() {
        let p = port("ens33", "00:0c:29:1a:2b:3c", "0000:02:01.0");
        assert_eq!(
            link_file("eth0", &p),
            concat!(
                "# Managed by Nightshade. Do not edit.
",
                "#
",
                "# This file is generated from /etc/nightshade/config.boot and is rewritten on
",
                "# every commit. Changes made here are lost, and are not part of the config the
",
                "# next boot will apply.
",
                "
",
                "[Match]
",
                "PermanentMACAddress=00:0c:29:1a:2b:3c
",
                "
",
                "[Link]
",
                "Name=eth0
",
            )
        );
    }

    #[test]
    fn the_hash_comes_out_of_the_right_shadow_field() {
        let shadow = "root:!:20000:0:99999:7:::\n\
                      nightshade:$6$salt$checksum:20000:0:99999:7:::\n\
                      sshd:*:20000:0:99999:7:::\n";
        assert_eq!(
            hash_from_shadow(shadow, "nightshade").as_deref(),
            Some("$6$salt$checksum")
        );
        assert_eq!(hash_from_shadow(shadow, "root").as_deref(), Some("!"));
        assert_eq!(hash_from_shadow(shadow, "nobody"), None);
        // An account with an empty password field is not a credential.
        assert_eq!(hash_from_shadow("empty::20000:0:::\n", "empty"), None);
    }

    /// What `show configuration` prints on a freshly installed box.
    #[test]
    fn a_fresh_config_describes_the_host_the_ports_and_the_account() {
        let config = install_config("fw-01");
        let ports = vec![
            port("ens33", "00:0c:29:00:00:01", "0000:02:01.0"),
            port("ens36", "00:0c:29:00:00:02", "0000:02:02.0"),
        ];
        let named = assign_names(&ports);
        let text = config_boot(&config, &named, Some("$6$salt$checksum"));

        assert!(text.contains("ethernet eth0 {"), "{text}");
        assert!(text.contains("hw-id 00:0c:29:00:00:01"), "{text}");
        assert!(text.contains("ethernet eth1 {"), "{text}");
        // The loopback is part of the picture even though nothing configures it.
        assert!(text.contains("loopback lo {"), "{text}");
        assert!(text.contains("host-name fw-01"), "{text}");
        assert!(text.contains("user nightshade {"), "{text}");
        // Quoted, or the file does not parse: `$` is not a bare-word char.
        assert!(
            text.contains(r#"encrypted-password "$6$salt$checksum""#),
            "{text}"
        );
        // The kernel's names are gone from the configuration entirely.
        assert!(!text.contains("ens33"), "{text}");
    }

    /// The generated text, as bytes, for the parser test in nightshade-schema
    /// to consume. Printed rather than asserted so a drift shows the actual
    /// output instead of only that it differed.
    #[test]
    fn the_generated_config_is_shaped_like_curly_format() {
        let config = install_config("fw-01");
        let ports = vec![port("ens33", "00:0c:29:1a:2b:3c", "0000:02:01.0")];
        let named = assign_names(&ports);
        let text = config_boot(&config, &named, Some("$6$rounds=656000$salt$checksum0123456789"));

        // Braces balance, which is the failure the parser would report as a
        // syntax error at the very end of the file.
        let opens = text.matches('{').count();
        let closes = text.matches('}').count();
        assert_eq!(opens, closes, "unbalanced braces:
{text}");

        // Every value line is `key value`, and the one value that needs quotes
        // has them.
        for line in text.lines() {
            let trimmed = line.trim();
            if trimmed.starts_with("encrypted-password") {
                assert!(
                    trimmed.ends_with('"'),
                    "the hash must be quoted: {trimmed}"
                );
            }
        }
        assert!(text.ends_with("}
"), "{text}");
    }

    /// A box with no password hash still gets a usable configuration.
    #[test]
    fn a_missing_hash_leaves_the_account_out_rather_than_writing_a_broken_one() {
        let config = install_config("nightshade");
        let text = config_boot(&config, &[], None);
        // Still an interfaces block: a box with no ports still has `lo`.
        assert!(text.contains("loopback lo {"), "{text}");
        assert!(!text.contains("login"), "{text}");
        assert!(!text.contains("encrypted-password"), "{text}");
        assert!(text.contains("host-name"), "{text}");
    }
}
