//! The install log.
//!
//! Everything the installer does lands in /tmp/nightshade-install.log: every
//! command, its exit status and both of its output streams. When an install
//! fails the operator is told to read this file, so it is written unbuffered --
//! a crash must not cost us the last few lines, which are the interesting ones.
//!
//! Logging never fails the install. If the log cannot be opened we carry on
//! silently rather than refusing to install because we could not write a file.

use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

pub const DEFAULT_LOG_PATH: &str = "/tmp/nightshade-install.log";

struct Sink {
    file: Option<File>,
    path: PathBuf,
    start: Instant,
}

static SINK: OnceLock<Mutex<Sink>> = OnceLock::new();

/// Open the log. Safe to call once; later calls are ignored.
pub fn init(path: impl Into<PathBuf>) {
    let path = path.into();
    let file = File::create(&path).ok();
    let _ = SINK.set(Mutex::new(Sink {
        file,
        path,
        start: Instant::now(),
    }));
}

pub fn log_path() -> PathBuf {
    SINK.get()
        .and_then(|s| s.lock().ok().map(|s| s.path.clone()))
        .unwrap_or_else(|| PathBuf::from(DEFAULT_LOG_PATH))
}

/// True when the log is actually being written somewhere.
pub fn is_active() -> bool {
    SINK.get()
        .and_then(|s| s.lock().ok().map(|s| s.file.is_some()))
        .unwrap_or(false)
}

fn write_line(level: &str, msg: &str) {
    let Some(sink) = SINK.get() else { return };
    let Ok(mut sink) = sink.lock() else { return };
    let elapsed = sink.start.elapsed().as_secs_f64();
    let Some(file) = sink.file.as_mut() else { return };
    // Written and flushed line by line; see the module note about crashes.
    let _ = writeln!(file, "[{elapsed:9.3}] {level:<5} {msg}");
    let _ = file.flush();
}

pub fn info(msg: impl AsRef<str>) {
    write_line("info", msg.as_ref());
}

pub fn warn(msg: impl AsRef<str>) {
    write_line("warn", msg.as_ref());
}

pub fn error(msg: impl AsRef<str>) {
    write_line("error", msg.as_ref());
}

/// A section heading, so the log reads as the install flow rather than as a
/// flat wall of subprocess output.
pub fn step(msg: impl AsRef<str>) {
    write_line("step", "");
    write_line("step", &format!("=== {} ===", msg.as_ref()));
}

/// Multi-line payloads (captured stdout/stderr) indented under their command.
pub fn block(label: &str, body: &str) {
    let body = body.trim_end();
    if body.is_empty() {
        return;
    }
    const LIMIT: usize = 64 * 1024;
    let (body, truncated) = if body.len() > LIMIT {
        (&body[..LIMIT], true)
    } else {
        (body, false)
    };
    write_line("out", &format!("{label}:"));
    for line in body.lines() {
        write_line("out", &format!("  | {line}"));
    }
    if truncated {
        write_line("out", "  | ... (truncated)");
    }
}

/// Best-effort copy of the log into the installed system, so a machine that
/// booted badly still carries the record of how it was built.
pub fn copy_to(target: &Path) -> std::io::Result<()> {
    let src = log_path();
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::copy(src, target)?;
    Ok(())
}
