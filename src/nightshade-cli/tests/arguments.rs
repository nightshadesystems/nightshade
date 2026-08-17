//! The built `ns` binary, run the way the image build runs it.
//!
//! `--version` has to answer without a configuration daemon to talk to: it is
//! what the image build uses to prove the binary works inside the chroot,
//! where there is no configd and never will be. Connecting first would make
//! the gate fail on a perfectly good binary.

use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const LIMIT: Duration = Duration::from_secs(20);

fn run(arguments: &[&str]) -> (Option<i32>, String) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_ns"))
        .args(arguments)
        // No socket in the environment these run in, which is the point.
        .env("NIGHTSHADE_LOG", "off")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("the binary was built");

    let deadline = Instant::now() + LIMIT;
    loop {
        match child.try_wait().expect("waiting on the child") {
            Some(status) => {
                let output = child.wait_with_output().expect("collecting output");
                return (
                    status.code(),
                    String::from_utf8_lossy(&output.stdout).into_owned(),
                );
            }
            None if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                panic!("ns {} did not exit within {LIMIT:?}", arguments.join(" "));
            }
            None => std::thread::sleep(Duration::from_millis(20)),
        }
    }
}

/// The one the image build depends on, and it must not need a daemon.
#[test]
fn version_prints_and_exits_without_a_daemon() {
    let (code, stdout) = run(&["--version"]);
    assert_eq!(code, Some(0), "{stdout:?}");
    assert!(stdout.contains("Nightshade"), "{stdout:?}");
    assert!(stdout.contains(nightshade_common::VERSION), "{stdout:?}");
}

#[test]
fn help_prints_and_exits_without_a_daemon() {
    for flag in ["-h", "--help"] {
        let (code, stdout) = run(&[flag]);
        assert_eq!(code, Some(0), "{flag}");
        assert!(stdout.contains("usage:"), "{flag}: {stdout:?}");
    }
}

/// Anything that needs configd exits 1 when there is none, rather than
/// hanging or panicking.
#[test]
fn commands_that_need_the_daemon_fail_cleanly_without_one() {
    let (code, _) = run(&["-c", "show version"]);
    assert_eq!(code, Some(1));
}

#[test]
fn an_unknown_option_is_refused() {
    let (code, _) = run(&["--not-an-option"]);
    assert_eq!(code, Some(1));
}
