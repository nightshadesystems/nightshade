//! The built binary, run the way the image build runs it.
//!
//! This exists because of a specific failure. The image build gates on
//! `nightshade-configd --version` to prove the binary works inside the image,
//! and the daemon did not parse arguments -- so `--version` started it, it
//! bound a socket in the build chroot and waited for connections, and the
//! build hung rather than failing.
//!
//! A test that a daemon *exits* is an odd-looking test. It is the right one:
//! everything that runs this binary non-interactively depends on it, and the
//! failure mode is a job that never finishes rather than one that goes red.

use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Long enough for a cold start on a loaded runner, short enough that a hang
/// fails a test instead of a pipeline.
const LIMIT: Duration = Duration::from_secs(20);

struct Finished {
    code: Option<i32>,
    stdout: String,
}

/// Run the binary and insist it finishes.
///
/// Polls rather than blocking, because the whole point is to tell "exited
/// non-zero" apart from "never exited" -- and `Child::wait` cannot.
fn run(arguments: &[&str]) -> Result<Finished, String> {
    let mut child = Command::new(env!("CARGO_BIN_EXE_nightshade-configd"))
        .args(arguments)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("the binary was built");

    let deadline = Instant::now() + LIMIT;
    loop {
        match child.try_wait().expect("waiting on the child") {
            Some(status) => {
                let output = child.wait_with_output().expect("collecting output");
                return Ok(Finished {
                    code: status.code(),
                    stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
                });
            }
            None if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!(
                    "nightshade-configd {} did not exit within {LIMIT:?}",
                    arguments.join(" ")
                ));
            }
            None => std::thread::sleep(Duration::from_millis(20)),
        }
    }
}

/// The one the image build depends on.
#[test]
fn version_prints_and_exits() {
    let finished = run(&["--version"]).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(finished.code, Some(0));
    assert!(
        finished.stdout.contains("nightshade-configd"),
        "{:?}",
        finished.stdout
    );
    assert!(
        finished.stdout.contains(nightshade_common::VERSION),
        "{:?}",
        finished.stdout
    );
}

#[test]
fn help_prints_and_exits() {
    for flag in ["-h", "--help"] {
        let finished = run(&[flag]).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(finished.code, Some(0), "{flag}");
        assert!(finished.stdout.contains("usage:"), "{flag}: {:?}", finished.stdout);
    }
}

/// An option that does not exist must be refused rather than ignored. Ignoring
/// it is what turned `--help` into "start the daemon".
#[test]
fn an_unknown_option_is_refused_rather_than_started() {
    let finished = run(&["--not-an-option"]).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(finished.code, Some(1));
}
