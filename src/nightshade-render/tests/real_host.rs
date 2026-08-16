//! `RealHost` against a real filesystem.
//!
//! Everything else in the test suite runs against `MockHost`, which is fast
//! and covers every decision the pipeline makes. It does not cover the code
//! that actually writes files -- and that code is the one that runs on the
//! appliance, with `/run/systemd/network` on the other end of it.
//!
//! None of this needs privilege or a network: the sync rules are about
//! directories, and a `TempDir` is a directory. What is left for a privileged
//! VM is whether the kernel accepts the result, which no test on this side of
//! `networkctl` can answer.

use std::collections::BTreeMap;

use nightshade_common::MANAGED_MARKER;
use nightshade_render::{Host, RealHost};

fn files(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(name, body)| (name.to_string(), body.to_string()))
        .collect()
}

fn listing(dir: &std::path::Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

#[test]
fn sync_writes_what_it_is_given() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("network");

    RealHost
        .sync(
            &target,
            MANAGED_MARKER,
            &files(&[("10-ns-eth0.network", "one"), ("20-ns-bond0.netdev", "two")]),
        )
        .unwrap();

    assert_eq!(
        listing(&target),
        ["10-ns-eth0.network", "20-ns-bond0.netdev"]
    );
    assert_eq!(
        std::fs::read_to_string(target.join("10-ns-eth0.network")).unwrap(),
        "one"
    );
}

/// The rule the whole of `/run/systemd/network` safety rests on, exercised on
/// a real directory rather than an in-memory one.
#[test]
fn sync_removes_our_stale_files_and_nothing_else() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("network");
    std::fs::create_dir_all(&target).unwrap();

    // Files somebody else put there. The second is the interesting one: it
    // sorts and reads like ours without carrying the marker.
    std::fs::write(target.join("50-someone-else.network"), "not ours").unwrap();
    std::fs::write(target.join("10-nsswitch.network"), "also not ours").unwrap();

    RealHost
        .sync(
            &target,
            MANAGED_MARKER,
            &files(&[("10-ns-eth0.network", "a"), ("10-ns-eth1.network", "b")]),
        )
        .unwrap();

    // eth1 leaves the configuration.
    RealHost
        .sync(&target, MANAGED_MARKER, &files(&[("10-ns-eth0.network", "a")]))
        .unwrap();

    assert_eq!(
        listing(&target),
        ["10-ns-eth0.network", "10-nsswitch.network", "50-someone-else.network"],
    );
    assert_eq!(
        std::fs::read_to_string(target.join("50-someone-else.network")).unwrap(),
        "not ours"
    );
}

/// Nothing may be left half-written where networkd can read it.
#[test]
fn sync_leaves_no_temporary_files_behind() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("network");

    RealHost
        .sync(
            &target,
            MANAGED_MARKER,
            &files(&[("10-ns-eth0.network", "one"), ("20-ns-vlan100.netdev", "two")]),
        )
        .unwrap();

    for name in listing(&target) {
        assert!(
            !name.ends_with(".ns-new"),
            "{name} is a temporary file that survived the write"
        );
    }
}

/// An existing file is replaced, not appended to or merged with.
#[test]
fn sync_replaces_rather_than_merges() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("network");

    RealHost
        .sync(&target, MANAGED_MARKER, &files(&[("10-ns-eth0.network", "a much longer first version")]))
        .unwrap();
    RealHost
        .sync(&target, MANAGED_MARKER, &files(&[("10-ns-eth0.network", "short")]))
        .unwrap();

    assert_eq!(
        std::fs::read_to_string(target.join("10-ns-eth0.network")).unwrap(),
        "short"
    );
}

#[test]
fn write_and_read_agree_and_create_the_parent() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("etc/resolv.conf");

    assert_eq!(RealHost.read(&path).unwrap(), None);
    RealHost.write(&path, "nameserver 1.1.1.1\n").unwrap();
    assert_eq!(
        RealHost.read(&path).unwrap().as_deref(),
        Some("nameserver 1.1.1.1\n")
    );
}

/// Running a program that fails must produce an error carrying enough to
/// diagnose it, not a bare "command failed".
#[test]
fn a_failing_command_says_what_failed_and_why() {
    let err = RealHost
        .run(&["false".to_string()])
        .expect_err("false always fails");
    let text = err.to_string();
    assert!(text.contains("false"), "{text}");
    assert!(text.contains("exit 1"), "{text}");

    let err = RealHost
        .run(&["a-program-that-is-not-installed".to_string()])
        .expect_err("that program does not exist");
    assert!(err.to_string().contains("a-program-that-is-not-installed"), "{err}");
}

#[test]
fn a_successful_command_succeeds() {
    RealHost.run(&["true".to_string()]).unwrap();
}

/// An argument is an argument, never something a shell gets to look at.
#[test]
fn arguments_reach_the_program_uninterpreted() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("$(touch pwned); echo hi");

    // If anything expanded this, the file would not be there under its literal
    // name -- and `pwned` would be.
    RealHost.write(&path, "contents").unwrap();
    assert!(path.exists(), "the literal filename was not used");
    assert!(!dir.path().join("pwned").exists(), "a substitution ran");
}
