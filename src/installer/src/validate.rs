//! Input rules.
//!
//! Kept apart from both frontends so the TUI and the plain prompt flow enforce
//! exactly the same thing. There is no "skip" path through any of these: the
//! installer will not produce a machine with no password on it.

use crate::error::{Error, Result};
use crate::secret::SecretString;

pub const MIN_PASSWORD_LEN: usize = 8;
pub const DEFAULT_HOSTNAME: &str = "nightshade";
pub const DEFAULT_USER: &str = "nightshade";

/// The word the operator has to type before anything is erased. Deliberately
/// not "yes": it has to be a thing you cannot produce by holding down enter.
pub const ERASE_CONFIRMATION: &str = "ERASE";

pub fn password(first: &SecretString, second: &SecretString) -> Result<()> {
    if first.is_empty() {
        return Err(Error::invalid("Password must not be empty."));
    }
    if first.len() < MIN_PASSWORD_LEN {
        return Err(Error::invalid(format!(
            "Password must be at least {MIN_PASSWORD_LEN} characters ({} entered).",
            first.len()
        )));
    }
    if !first.matches(second) {
        return Err(Error::invalid("Passwords do not match."));
    }
    Ok(())
}

/// RFC 1123 host name.
///
/// Stricter than the kernel, which will accept almost anything: an invalid
/// name here silently breaks sudo's reverse lookup and every certificate the
/// appliance later presents.
pub fn hostname(name: &str) -> Result<()> {
    if name.is_empty() {
        return Err(Error::invalid("Hostname must not be empty."));
    }
    if name.len() > 253 {
        return Err(Error::invalid(
            "Hostname must be at most 253 characters.",
        ));
    }
    if name.starts_with('.') || name.ends_with('.') {
        return Err(Error::invalid("Hostname must not start or end with a dot."));
    }

    for label in name.split('.') {
        if label.is_empty() {
            return Err(Error::invalid("Hostname must not contain an empty label."));
        }
        if label.len() > 63 {
            return Err(Error::invalid(format!(
                "Hostname label '{label}' is longer than 63 characters."
            )));
        }
        if label.starts_with('-') || label.ends_with('-') {
            return Err(Error::invalid(format!(
                "Hostname label '{label}' must not start or end with a hyphen."
            )));
        }
        if let Some(bad) = label
            .chars()
            .find(|c| !c.is_ascii_alphanumeric() && *c != '-')
        {
            return Err(Error::invalid(format!(
                "Hostname contains an invalid character: '{bad}'. \
                 Use letters, digits and hyphens only."
            )));
        }
    }

    Ok(())
}

/// The typed confirmation before destruction. Case-sensitive on purpose.
pub fn erase_confirmation(typed: &str) -> Result<()> {
    if typed.trim() == ERASE_CONFIRMATION {
        Ok(())
    } else {
        Err(Error::invalid(format!(
            "Type {ERASE_CONFIRMATION} exactly to confirm, or cancel."
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secret(s: &str) -> SecretString {
        SecretString::new(s.to_string())
    }

    #[test]
    fn password_rejects_empty() {
        assert!(password(&secret(""), &secret("")).is_err());
    }

    #[test]
    fn password_rejects_short() {
        assert!(password(&secret("short12"), &secret("short12")).is_err());
    }

    #[test]
    fn password_rejects_mismatch() {
        assert!(password(&secret("correcthorse"), &secret("correcthors")).is_err());
    }

    #[test]
    fn password_accepts_eight_characters() {
        assert!(password(&secret("12345678"), &secret("12345678")).is_ok());
    }

    #[test]
    fn hostname_accepts_reasonable_names() {
        for name in ["nightshade", "fw-01", "edge1.example.com", "a", "0fw"] {
            assert!(hostname(name).is_ok(), "{name} should be valid");
        }
    }

    #[test]
    fn hostname_rejects_bad_names() {
        for name in [
            "",
            "-leading",
            "trailing-",
            "has space",
            "under_score",
            "double..dot",
            ".leadingdot",
            "trailingdot.",
        ] {
            assert!(hostname(name).is_err(), "{name} should be invalid");
        }
    }

    #[test]
    fn hostname_rejects_overlong_label() {
        let long = "a".repeat(64);
        assert!(hostname(&long).is_err());
        let ok = "a".repeat(63);
        assert!(hostname(&ok).is_ok());
    }

    #[test]
    fn erase_is_case_sensitive_and_exact() {
        assert!(erase_confirmation("ERASE").is_ok());
        assert!(erase_confirmation("  ERASE  ").is_ok());
        assert!(erase_confirmation("erase").is_err());
        assert!(erase_confirmation("yes").is_err());
        assert!(erase_confirmation("").is_err());
    }
}
