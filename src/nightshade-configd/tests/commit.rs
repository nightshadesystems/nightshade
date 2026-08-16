//! The commit pipeline, over the socket, against a mock apply layer.
//!
//! The renderers run for real and produce real artifacts; only the last inch
//! that would change a live interface is intercepted. Every decision the
//! pipeline makes -- what to validate, what to render, in what order, what to
//! do when it goes wrong -- is exercised here in milliseconds.

mod harness;

use harness::{Harness, expect_failure, expect_ok};
use nightshade_proto::message::{FailureKind, Request, Response, SessionId};
use nightshade_render::Op;
use nightshade_schema::config::ConfigTree;
use nightshade_schema::path::Path;

fn p(s: &str) -> Path {
    Path::parse(s).unwrap()
}

fn set(session: &SessionId, path: &str, value: &str) -> Request {
    Request::Set {
        session: session.clone(),
        path: p(path),
        value: (!value.is_empty()).then(|| value.to_string()),
    }
}

fn commit(session: &SessionId) -> Request {
    Request::Commit {
        session: session.clone(),
        comment: None,
    }
}

#[track_caller]
fn expect_committed(response: Response) -> (u64, Vec<String>) {
    match response {
        Response::Committed {
            generation,
            changes,
        } => (generation, changes.iter().map(ToString::to_string).collect()),
        other => panic!("expected a commit, got {other:?}"),
    }
}

fn tree(response: Response) -> ConfigTree {
    match response {
        Response::Config { tree } => tree,
        other => panic!("expected a config, got {other:?}"),
    }
}

/// The acceptance case: edit, commit, and find the box configured.
#[test]
fn a_commit_applies_and_promotes() {
    let harness = Harness::start();
    let (mut client, session) = harness.session();

    for (path, value) in [
        ("system host-name", "fw-01"),
        ("system name-server", "1.1.1.1"),
        ("interfaces ethernet eth0 address", "192.168.1.1/24"),
        ("interfaces ethernet eth1", ""),
        ("interfaces ethernet eth2", ""),
        ("interfaces bonding bond0 member", "eth1"),
        ("interfaces bonding bond0 member", "eth2"),
        ("interfaces bonding bond0 address", "10.0.0.1/24"),
        ("interfaces vlan vlan100 parent", "eth0"),
        ("interfaces vlan vlan100 id", "100"),
    ] {
        expect_ok(client.call(set(&session, path, value)));
    }

    let (generation, changes) = expect_committed(client.call(commit(&session)));
    assert_eq!(generation, 1);
    assert!(!changes.is_empty());

    // Running is now the candidate.
    let running = tree(client.call(Request::ShowRunning { path: Path::root() }));
    assert_eq!(
        running.get(&p("system host-name")).unwrap().value(),
        Some("fw-01")
    );
    assert!(running.contains(&p("interfaces bonding bond0 member")));

    // Nothing left to commit, and comparing shows nothing.
    assert_eq!(
        harness_changes(&mut client, &session),
        Vec::<String>::new(),
        "the candidate and running disagree after a commit"
    );

    // The renderers wrote what they should have.
    let files = harness.host().files();
    let networkd = harness.paths().networkd_dir();
    for name in [
        "10-ns-eth0.network",
        "10-ns-eth1.network",
        "20-ns-bond0.netdev",
        "20-ns-vlan100.netdev",
    ] {
        assert!(files.contains_key(&networkd.join(name)), "missing {name}");
    }
    assert!(files.contains_key(&harness.paths().resolv_conf()));

    // And ran the right commands, system before interfaces.
    assert_eq!(
        harness.host().commands(),
        [
            "hostnamectl set-hostname fw-01",
            "networkctl reload"
        ]
    );
}

fn harness_changes(client: &mut harness::Client, session: &SessionId) -> Vec<String> {
    match client.call(Request::Compare {
        session: session.clone(),
    }) {
        Response::Changes { changes } => changes.iter().map(ToString::to_string).collect(),
        other => panic!("expected changes, got {other:?}"),
    }
}

#[test]
fn committing_nothing_is_a_success_and_changes_nothing() {
    let harness = Harness::start();
    let (mut client, session) = harness.session();

    let (generation, changes) = expect_committed(client.call(commit(&session)));
    assert_eq!(generation, 0);
    assert!(changes.is_empty());
    assert!(
        harness.host().ops().is_empty(),
        "an empty commit touched the system: {:?}",
        harness.host().ops()
    );
}

/// The whole point of steps 6 and 7 coming before step 8.
#[test]
fn a_constraint_violation_stops_the_commit_before_anything_is_touched() {
    let harness = Harness::start();
    let (mut client, session) = harness.session();

    // A vlan whose parent was never configured.
    expect_ok(client.call(set(&session, "interfaces vlan vlan100 parent", "eth7")));
    expect_ok(client.call(set(&session, "interfaces vlan vlan100 id", "100")));

    let response = client.call(commit(&session));
    let Response::Failed { kind, message } = &response else {
        panic!("expected a failure, got {response:?}");
    };
    assert_eq!(*kind, FailureKind::Validation);
    assert!(message.contains("eth7"), "{message}");

    assert!(
        harness.host().ops().is_empty(),
        "a refused commit touched the system"
    );
    let running = tree(client.call(Request::ShowRunning { path: Path::root() }));
    assert!(running.is_empty());
}

#[test]
fn a_required_leaf_that_is_missing_stops_the_commit() {
    let harness = Harness::start();
    let (mut client, session) = harness.session();

    // A vxlan with no VNI.
    expect_ok(client.call(set(&session, "interfaces vxlan vxlan1 remote", "10.0.0.2")));

    let message = expect_failure(client.call(commit(&session)));
    assert!(message.contains("vni"), "{message}");
    assert!(message.contains("required"), "{message}");
    assert!(harness.host().ops().is_empty());
}

/// A and B both open, A commits, B is refused -- and told what moved.
#[test]
fn a_session_that_has_fallen_behind_cannot_commit() {
    let harness = Harness::start();
    let (mut a, session_a) = harness.session();
    let (mut b, session_b) = harness.session();

    expect_ok(a.call(set(&session_a, "system host-name", "from-a")));
    expect_ok(b.call(set(&session_b, "system time-zone", "UTC")));

    expect_committed(a.call(commit(&session_a)));

    let response = b.call(commit(&session_b));
    let Response::Failed { kind, message } = &response else {
        panic!("expected a conflict, got {response:?}");
    };
    assert_eq!(*kind, FailureKind::Conflict);

    // Says what moved underneath, and what B's own changes are, so B can
    // decide rather than being told to start again with no information.
    assert!(message.contains("revision 0"), "{message}");
    assert!(message.contains("revision 1"), "{message}");
    assert!(message.contains("+ system host-name from-a"), "{message}");
    assert!(message.contains("+ system time-zone UTC"), "{message}");
    assert!(message.contains("discard"), "{message}");

    // Discarding and redoing the work works.
    expect_ok(b.call(Request::Discard {
        session: session_b.clone(),
    }));
    expect_ok(b.call(set(&session_b, "system time-zone", "UTC")));
    let (generation, _) = expect_committed(b.call(commit(&session_b)));
    assert_eq!(generation, 2);

    // And A's change survived B's.
    let running = tree(b.call(Request::ShowRunning { path: Path::root() }));
    assert_eq!(
        running.get(&p("system host-name")).unwrap().value(),
        Some("from-a")
    );
    assert_eq!(
        running.get(&p("system time-zone")).unwrap().value(),
        Some("UTC")
    );
}

/// The committing session stays level with running, so an operator can carry
/// straight on instead of having to reopen.
#[test]
fn a_session_can_commit_twice_in_a_row() {
    let harness = Harness::start();
    let (mut client, session) = harness.session();

    expect_ok(client.call(set(&session, "system host-name", "first")));
    assert_eq!(expect_committed(client.call(commit(&session))).0, 1);

    expect_ok(client.call(set(&session, "system host-name", "second")));
    assert_eq!(expect_committed(client.call(commit(&session))).0, 2);

    let running = tree(client.call(Request::ShowRunning { path: Path::root() }));
    assert_eq!(
        running.get(&p("system host-name")).unwrap().value(),
        Some("second")
    );
}

/// An apply that fails part way through must leave the previous configuration
/// in place, not half of the new one.
#[test]
fn a_failed_apply_restores_what_was_there() {
    let harness = Harness::start();
    let (mut client, session) = harness.session();

    // A first commit, so there is something to restore to.
    expect_ok(client.call(set(&session, "system host-name", "before")));
    expect_ok(client.call(set(
        &session,
        "interfaces ethernet eth0 address",
        "10.0.0.1/24",
    )));
    expect_committed(client.call(commit(&session)));
    harness.host().take_ops();

    // Now make networkd's reload fail once. The system renderer runs first and
    // succeeds, so the pipeline has to undo it -- and the restore's own reload
    // has to be allowed to work, or this would be testing the unrecovered case
    // instead.
    harness.host().fail_once_matching("networkctl reload");
    expect_ok(client.call(set(&session, "system host-name", "after")));
    expect_ok(client.call(set(
        &session,
        "interfaces ethernet eth1 address",
        "10.0.1.1/24",
    )));

    let message = expect_failure(client.call(commit(&session)));
    assert!(message.contains("networkd"), "{message}");
    assert!(message.contains("restored"), "{message}");

    // Running did not move.
    let running = tree(client.call(Request::ShowRunning { path: Path::root() }));
    assert_eq!(
        running.get(&p("system host-name")).unwrap().value(),
        Some("before")
    );
    assert!(!running.contains(&p("interfaces ethernet eth1")));

    // And the host name really was put back, not just left at the new value.
    let restored = harness
        .host()
        .commands()
        .iter()
        .rev()
        .find(|c| c.starts_with("hostnamectl"))
        .cloned()
        .expect("the system renderer should have been restored");
    assert_eq!(restored, "hostnamectl set-hostname before");

    // The half-written networkd files were rolled back too, not left behind.
    assert!(
        harness
            .host()
            .file(harness.paths().networkd_dir().join("10-ns-eth1.network"))
            .is_none(),
        "the failed commit left its interface files in place"
    );
}

/// When putting it back fails too, the box matches no configuration at all.
/// That is the one outcome an operator has to be told about in as many words.
#[test]
fn a_restore_that_also_fails_says_so_plainly() {
    let harness = Harness::start();
    let (mut client, session) = harness.session();

    expect_ok(client.call(set(&session, "system host-name", "before")));
    expect_ok(client.call(set(
        &session,
        "interfaces ethernet eth0 address",
        "10.0.0.1/24",
    )));
    expect_committed(client.call(commit(&session)));

    // Permanently, so the restore hits it as well.
    harness.host().fail_commands_matching("networkctl reload");
    expect_ok(client.call(set(&session, "system host-name", "after")));

    let message = expect_failure(client.call(commit(&session)));
    assert!(message.contains("RESTORING THE PREVIOUS CONFIGURATION ALSO FAILED"), "{message}");
    assert!(
        message.contains("not in a state that matches any saved configuration"),
        "{message}"
    );

    // Running still did not move: what is recorded as applied is only ever
    // what was actually applied.
    let running = tree(client.call(Request::ShowRunning { path: Path::root() }));
    assert_eq!(
        running.get(&p("system host-name")).unwrap().value(),
        Some("before")
    );
}

/// The last-applied artifacts are what the restore uses, so they have to
/// survive a configd restart.
#[test]
fn the_restore_point_survives_a_restart() {
    let harness = Harness::start();
    let (mut client, session) = harness.session();
    expect_ok(client.call(set(&session, "system host-name", "before")));
    expect_committed(client.call(commit(&session)));
    drop(client);

    let harness = harness.restart();
    let mut client = harness.connect();

    // Running came back from /run, so the generation counter did too.
    let running = tree(client.call(Request::ShowRunning { path: Path::root() }));
    assert_eq!(
        running.get(&p("system host-name")).unwrap().value(),
        Some("before")
    );

    // A session opened after the restart is level with it and can commit.
    let (mut fresh, id) = harness.session();
    expect_ok(fresh.call(set(&id, "system host-name", "after")));
    assert_eq!(expect_committed(fresh.call(commit(&id))).0, 2);
}

/// Removing an interface has to remove its files, not just stop writing them.
#[test]
fn deleting_an_interface_sweeps_its_files_away() {
    let harness = Harness::start();
    let (mut client, session) = harness.session();
    let networkd = harness.paths().networkd_dir();

    expect_ok(client.call(set(&session, "interfaces ethernet eth0", "")));
    expect_ok(client.call(set(&session, "interfaces ethernet eth1", "")));
    expect_committed(client.call(commit(&session)));
    assert!(harness.host().file(networkd.join("10-ns-eth1.network")).is_some());
    harness.host().take_ops();

    expect_ok(client.call(Request::Delete {
        session: session.clone(),
        path: p("interfaces ethernet eth1"),
        value: None,
    }));
    expect_committed(client.call(commit(&session)));

    assert!(
        harness.host().file(networkd.join("10-ns-eth1.network")).is_none(),
        "a deleted interface left its file behind"
    );
    assert!(harness.host().file(networkd.join("10-ns-eth0.network")).is_some());

    // And the sync said so, rather than the file vanishing by accident.
    let swept = harness.host().ops().into_iter().any(|op| {
        matches!(op, Op::Sync { removed, .. } if removed.contains(&"10-ns-eth1.network".to_string()))
    });
    assert!(swept, "{:#?}", harness.host().ops());
}

/// A bond whose mode changed needs the device rebuilt, and the pipeline has to
/// carry that all the way through from a `set`.
#[test]
fn changing_a_bond_mode_rebuilds_the_device() {
    let harness = Harness::start();
    let (mut client, session) = harness.session();

    expect_ok(client.call(set(&session, "interfaces ethernet eth1", "")));
    expect_ok(client.call(set(&session, "interfaces bonding bond0 member", "eth1")));
    expect_ok(client.call(set(&session, "interfaces bonding bond0 mode", "802.3ad")));
    expect_committed(client.call(commit(&session)));
    harness.host().take_ops();

    expect_ok(client.call(set(
        &session,
        "interfaces bonding bond0 mode",
        "active-backup",
    )));
    expect_committed(client.call(commit(&session)));

    assert_eq!(
        harness.host().commands(),
        ["networkctl delete bond0", "networkctl reload"]
    );
}

#[test]
fn an_unchanged_bond_is_not_rebuilt() {
    let harness = Harness::start();
    let (mut client, session) = harness.session();

    expect_ok(client.call(set(&session, "interfaces ethernet eth1", "")));
    expect_ok(client.call(set(&session, "interfaces bonding bond0 member", "eth1")));
    expect_committed(client.call(commit(&session)));
    harness.host().take_ops();

    // Something unrelated changes.
    expect_ok(client.call(set(&session, "system host-name", "fw")));
    expect_committed(client.call(commit(&session)));

    assert!(
        !harness.host().commands().iter().any(|c| c.contains("delete")),
        "a bond was rebuilt for an unrelated change: {:?}",
        harness.host().commands()
    );
}
