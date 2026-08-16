//! A string that does not leak.
//!
//! The account password passes through this type and nothing else. It is never
//! placed in a command line (argv is world-readable via /proc), never written
//! to a temporary file, and never rendered by Debug. It reaches the system
//! exactly once, as stdin to `chpasswd` inside the target chroot.

use std::fmt;

pub struct SecretString(String);

impl SecretString {
    pub fn new(s: String) -> Self {
        SecretString(s)
    }

    /// The only way to read the secret. Named to make review easy: every call
    /// site is a place the password becomes visible.
    pub fn expose(&self) -> &str {
        &self.0
    }

    pub fn len(&self) -> usize {
        self.0.chars().count()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn matches(&self, other: &SecretString) -> bool {
        // Not constant-time, and deliberately so: both sides are operator input
        // typed at a local console during installation. There is no remote
        // attacker to time and no stored secret to compare against.
        self.0 == other.0
    }
}

impl Drop for SecretString {
    fn drop(&mut self) {
        // Overwrite the heap buffer before it is freed. write_volatile is what
        // stops the optimiser deleting a store to memory that is about to
        // become unreachable.
        let bytes = unsafe { self.0.as_bytes_mut() };
        for b in bytes.iter_mut() {
            unsafe { std::ptr::write_volatile(b, 0) };
        }
        std::sync::atomic::compiler_fence(std::sync::atomic::Ordering::SeqCst);
    }
}

impl fmt::Debug for SecretString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "SecretString(<redacted>)")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_is_redacted() {
        let s = SecretString::new("hunter2hunter2".into());
        assert_eq!(format!("{s:?}"), "SecretString(<redacted>)");
        assert!(!format!("{s:?}").contains("hunter"));
    }

    #[test]
    fn len_counts_characters_not_bytes() {
        // A password of eight non-ASCII characters must satisfy an
        // eight-character minimum, not an eight-byte one.
        let s = SecretString::new("pässwörd".into());
        assert_eq!(s.len(), 8);
    }

    #[test]
    fn matching() {
        let a = SecretString::new("correct horse".into());
        let b = SecretString::new("correct horse".into());
        let c = SecretString::new("correct hors".into());
        assert!(a.matches(&b));
        assert!(!a.matches(&c));
    }
}
