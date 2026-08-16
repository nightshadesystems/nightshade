//! Ground floor of the Nightshade workspace.
//!
//! Whatever more than one component has to agree on, and that is not a wire
//! type (`nightshade-proto`) or a config type (`nightshade-schema`), lives
//! here: where files are, what the admin group is called, what version this
//! is. It depends on nothing, so everything can depend on it.

pub mod paths;

/// The version reported by `show version`, `ns --version` and configd's
/// startup log line.
///
/// Every crate in the workspace inherits `version` from `[workspace.package]`,
/// so this is the whole product's version, not this crate's. Reading it in one
/// place keeps the CLI and the daemon from ever disagreeing about it.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Unix group that may talk to configd.
///
/// The socket is owned `root:nightshade-admin` mode 0660, and configd checks
/// the peer's credentials against this group on top of the file mode. The
/// mode alone would be enough if the group were the only way in, but configd
/// also has to record *who* acted for the commit log, so it is reading peer
/// credentials regardless -- and having read them, refusing a uid that should
/// not be there costs nothing.
pub const ADMIN_GROUP: &str = "nightshade-admin";

/// Mode of `configd.sock`. Group-writable, no world access.
pub const SOCKET_MODE: u32 = 0o660;

/// Prefix on every file Nightshade writes into a shared systemd directory.
///
/// `/run/systemd/network` is not ours: a networkd unit dropped there by hand,
/// or by a future package, has to survive us. Renderers may only create,
/// overwrite or delete files starting with this, and the sync step that
/// removes stale files filters on it. Nothing else in the tree is safe to
/// delete on the strength of "we did not render it this time".
pub const MANAGED_PREFIX: &str = "ns-";

/// Commit revisions retained in the archive before pruning.
pub const ARCHIVE_KEEP: usize = 50;
