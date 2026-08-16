//! Logging.
//!
//! journald when there is one, stderr otherwise. Under systemd both end up in
//! the journal, but only the journald layer carries structured fields, and the
//! commit log is the reason to want them: `journalctl NIGHTSHADE_ACTOR=1000`
//! answers "what did this person change" in a way that grepping a sentence
//! does not.
//!
//! `NIGHTSHADE_LOG` takes an `RUST_LOG`-style filter. Named for the product
//! rather than the language, because an operator turning up daemon logging on
//! an appliance should not have to know it is written in Rust.

use tracing_subscriber::EnvFilter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

pub fn init() {
    let filter = EnvFilter::try_from_env("NIGHTSHADE_LOG").unwrap_or_else(|_| EnvFilter::new("info"));
    let registry = tracing_subscriber::registry().with(filter);

    match tracing_journald::layer() {
        Ok(journald) => registry.with(journald).init(),
        Err(e) => {
            registry
                .with(tracing_subscriber::fmt::layer().with_target(false))
                .init();
            // After init, so it goes somewhere.
            tracing::debug!(error = %e, "no journald; logging to stderr");
        }
    }
}
