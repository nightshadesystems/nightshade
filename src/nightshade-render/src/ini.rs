//! Writing systemd unit files.
//!
//! Small on purpose. Keys are written in the order they are added rather than
//! sorted, because a `.netdev` reads best in the order systemd documents it
//! and a diff of two of them should show the change rather than a reshuffle.
//! Determinism comes from the renderer emitting them in a fixed order, which
//! it does, and which the golden tests hold it to.
//!
//! A section header is only written once something goes in it, so a config
//! with no MTU does not produce an empty `[Link]`.

use std::fmt::Write;

const HEADER: &str = "\
# Managed by Nightshade. Do not edit.
#
# This file is generated from /etc/nightshade/config.boot and is rewritten on
# every commit. Changes made here are lost, and are not part of the config the
# next boot will apply.
";

pub struct Ini {
    out: String,
    pending: Option<&'static str>,
    wrote_any: bool,
}

impl Ini {
    pub fn new() -> Self {
        Self {
            out: HEADER.to_string(),
            pending: None,
            wrote_any: false,
        }
    }

    /// Begin a section. Nothing is written until it has a key.
    pub fn section(&mut self, name: &'static str) -> &mut Self {
        self.pending = Some(name);
        self
    }

    pub fn key(&mut self, key: &str, value: impl AsRef<str>) -> &mut Self {
        if let Some(section) = self.pending.take() {
            self.out.push('\n');
            let _ = writeln!(self.out, "[{section}]");
        }
        let _ = writeln!(self.out, "{key}={}", value.as_ref());
        self.wrote_any = true;
        self
    }

    /// Write the key only if there is a value for it.
    pub fn maybe(&mut self, key: &str, value: Option<impl AsRef<str>>) -> &mut Self {
        match value {
            Some(value) => self.key(key, value),
            None => self,
        }
    }

    /// Write the key once per value, which is how systemd expresses lists.
    pub fn each<S: AsRef<str>>(&mut self, key: &str, values: impl IntoIterator<Item = S>) -> &mut Self {
        for value in values {
            self.key(key, value);
        }
        self
    }

    pub fn flag(&mut self, key: &str, set: bool) -> &mut Self {
        self.key(key, if set { "yes" } else { "no" })
    }

    pub fn finish(&self) -> String {
        self.out.clone()
    }
}

impl Default for Ini {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sections_appear_only_when_they_have_content() {
        let mut ini = Ini::new();
        ini.section("Match").key("Name", "eth0");
        ini.section("Link"); // nothing added
        ini.section("Network").key("Address", "10.0.0.1/24");

        let out = ini.finish();
        assert!(!out.contains("[Link]"), "{out}");
        assert!(out.contains("[Match]\nName=eth0\n"), "{out}");
        assert!(out.contains("[Network]\nAddress=10.0.0.1/24\n"), "{out}");
    }

    #[test]
    fn a_managed_header_says_so() {
        assert!(Ini::new().finish().starts_with("# Managed by Nightshade."));
    }

    #[test]
    fn lists_repeat_the_key() {
        let mut ini = Ini::new();
        ini.section("Network")
            .each("Address", ["10.0.0.1/24", "10.0.1.1/24"]);
        let out = ini.finish();
        assert!(out.contains("Address=10.0.0.1/24\nAddress=10.0.1.1/24\n"), "{out}");
    }

    #[test]
    fn absent_values_write_nothing() {
        let mut ini = Ini::new();
        ini.section("Link")
            .maybe("MTUBytes", None::<&str>)
            .maybe("MACAddress", Some("02:00:00:00:00:01"));
        let out = ini.finish();
        assert!(!out.contains("MTUBytes"), "{out}");
        assert!(out.contains("MACAddress=02:00:00:00:00:01"), "{out}");
    }
}
