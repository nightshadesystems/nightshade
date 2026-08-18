//! A session over the socket, end to end.

mod harness;

use harness::{Harness, expect_failure, expect_ok};
use nightshade_proto::message::{FailureKind, Request, Response, SessionId};
use nightshade_schema::config::ConfigTree;
use nightshade_schema::path::Path;
use nightshade_schema::{curly, diff};

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

fn tree(response: Response) -> ConfigTree {
    match response {
        Response::Config { tree } => tree,
        other => panic!("expected a config, got {other:?}"),
    }
}

fn changes(response: Response) -> Vec<String> {
    match response {
        Response::Changes { changes } => changes.iter().map(ToString::to_string).collect(),
        other => panic!("expected changes, got {other:?}"),
    }
}

/// The acceptance case: a whole edit, over the socket, from open to compare.
#[test]
fn a_full_session_over_the_socket() {
    let harness = Harness::start();
    let (mut client, session) = harness.session();

    for (path, value) in [
        ("system host-name", "fw-01"),
        ("system name-server", "1.1.1.1"),
        ("interfaces ethernet eth0 address", "192.168.1.1/24"),
        ("interfaces ethernet eth1", ""),
        ("interfaces ethernet eth2", ""),
        ("interfaces vlan vlan100 parent", "eth0"),
        ("interfaces vlan vlan100 id", "100"),
        ("interfaces bonding bond0 member", "eth1"),
        ("interfaces bonding bond0 member", "eth2"),
        ("interfaces bonding bond0 address", "10.0.0.1/24"),
    ] {
        expect_ok(client.call(set(&session, path, value)));
    }

    // The candidate holds everything that was set.
    let candidate = tree(client.call(Request::ShowCandidate {
        session: session.clone(),
        path: Path::root(),
    }));
    assert_eq!(
        candidate.get(&p("system host-name")).unwrap().value(),
        Some("fw-01")
    );
    assert_eq!(
        candidate
            .values_at(&p("interfaces bonding bond0 member"))
            .unwrap()
            .len(),
        2
    );

    // Running is still empty -- nothing has been committed, and nothing can be
    // until the commit pipeline exists.
    let running = tree(client.call(Request::ShowRunning { path: Path::root() }));
    assert!(running.is_empty());

    // So the comparison is the whole candidate, as additions.
    let compared = changes(client.call(Request::Compare {
        session: session.clone(),
    }));
    assert!(compared.iter().all(|line| line.starts_with('+')), "{compared:#?}");
    assert!(compared.contains(&"+ system host-name fw-01".to_string()));
    assert!(compared.contains(&"+ interfaces vlan vlan100 id 100".to_string()));
    assert!(compared.contains(&"+ interfaces bonding bond0 member eth1".to_string()));
    assert!(compared.contains(&"+ interfaces bonding bond0 member eth2".to_string()));

    // And it agrees with what the diff of the two trees says, so the wire is
    // carrying the comparison rather than recomputing a different one.
    assert_eq!(
        compared,
        diff::diff(&ConfigTree::new(), &candidate)
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
    );

    expect_ok(client.call(Request::SessionClose { session }));
}

#[test]
fn a_bad_value_is_refused_with_the_schema_s_own_words() {
    let harness = Harness::start();
    let (mut client, session) = harness.session();

    let response = client.call(set(&session, "interfaces ethernet eth0 mtu", "100000"));
    let Response::Failed { kind, message } = &response else {
        panic!("expected a failure, got {response:?}");
    };
    assert_eq!(*kind, FailureKind::Validation);
    assert!(message.contains("between 68 and 9216"), "{message}");

    // A refused edit changes nothing.
    let candidate = tree(client.call(Request::ShowCandidate {
        session,
        path: Path::root(),
    }));
    assert!(candidate.is_empty(), "a rejected set modified the candidate");
}

#[test]
fn unknown_paths_and_bad_interface_names_are_refused() {
    let harness = Harness::start();
    let (mut client, session) = harness.session();

    // `host-name` is the node; `hostname` is not a path the schema has.
    let message = expect_failure(client.call(set(&session, "system hostname", "fw")));
    assert!(message.contains("not a configuration path"), "{message}");

    let message = expect_failure(client.call(Request::Set {
        session: session.clone(),
        path: Path::from_segments(["interfaces", "ethernet", "eth 0", "mtu"]),
        value: Some("1500".into()),
    }));
    assert!(message.contains("interface name"), "{message}");

    // A leaf that needs a value, and a flag that must not have one.
    let message = expect_failure(client.call(set(&session, "system host-name", "")));
    assert!(message.contains("takes a value"), "{message}");
    let message = expect_failure(client.call(set(
        &session,
        "interfaces ethernet eth0 disable",
        "yes",
    )));
    assert!(message.contains("takes no value"), "{message}");
}

#[test]
fn delete_removes_a_node_or_one_value_of_a_multi_leaf() {
    let harness = Harness::start();
    let (mut client, session) = harness.session();

    expect_ok(client.call(set(&session, "system name-server", "1.1.1.1")));
    expect_ok(client.call(set(&session, "system name-server", "9.9.9.9")));
    expect_ok(client.call(set(&session, "system host-name", "fw")));

    // One value of the pair.
    expect_ok(client.call(Request::Delete {
        session: session.clone(),
        path: p("system name-server"),
        value: Some("1.1.1.1".into()),
    }));
    let candidate = tree(client.call(Request::ShowCandidate {
        session: session.clone(),
        path: Path::root(),
    }));
    assert_eq!(
        candidate.values_at(&p("system name-server")).unwrap().len(),
        1
    );

    // The whole node.
    expect_ok(client.call(Request::Delete {
        session: session.clone(),
        path: p("system name-server"),
        value: None,
    }));
    let candidate = tree(client.call(Request::ShowCandidate {
        session: session.clone(),
        path: Path::root(),
    }));
    assert!(!candidate.contains(&p("system name-server")));
    assert!(candidate.contains(&p("system host-name")));

    // Deleting what is not there is an error, not a quiet success -- that is
    // how somebody comes to believe they removed an address they did not.
    let message = expect_failure(client.call(Request::Delete {
        session: session.clone(),
        path: p("system name-server"),
        value: None,
    }));
    assert!(message.contains("not configured"), "{message}");

    let message = expect_failure(client.call(Request::Delete {
        session,
        path: p("system host-name"),
        value: Some("not-the-value".into()),
    }));
    assert!(message.contains("does not have the value"), "{message}");
}

#[test]
fn show_can_ask_for_part_of_a_config() {
    let harness = Harness::start();
    let (mut client, session) = harness.session();

    expect_ok(client.call(set(&session, "system host-name", "fw")));
    expect_ok(client.call(set(
        &session,
        "interfaces ethernet eth0 address",
        "10.0.0.1/24",
    )));

    let part = tree(client.call(Request::ShowCandidate {
        session: session.clone(),
        path: p("interfaces ethernet"),
    }));
    // Full paths are kept, so the fragment renders as part of the config it
    // came from rather than as a different config that looks similar.
    assert!(part.contains(&p("interfaces ethernet eth0 address")));
    assert!(!part.contains(&p("system host-name")));

    // A path that is not configured is an empty config, not an error.
    let nothing = tree(client.call(Request::ShowCandidate {
        session,
        path: p("interfaces vxlan"),
    }));
    assert!(nothing.is_empty());
}

#[test]
fn discard_puts_the_candidate_back_to_running() {
    let harness = Harness::start();
    let (mut client, session) = harness.session();

    expect_ok(client.call(set(&session, "system host-name", "fw")));
    expect_ok(client.call(Request::Discard {
        session: session.clone(),
    }));

    let candidate = tree(client.call(Request::ShowCandidate {
        session: session.clone(),
        path: Path::root(),
    }));
    assert!(candidate.is_empty());
    assert_eq!(changes(client.call(Request::Compare { session })), Vec::<String>::new());
}

#[test]
fn sessions_do_not_see_each_other_s_edits() {
    let harness = Harness::start();
    let (mut a, session_a) = harness.session();
    let (mut b, session_b) = harness.session();
    assert_ne!(session_a, session_b, "two sessions got the same id");

    expect_ok(a.call(set(&session_a, "system host-name", "from-a")));

    let seen_by_b = tree(b.call(Request::ShowCandidate {
        session: session_b.clone(),
        path: Path::root(),
    }));
    assert!(seen_by_b.is_empty(), "session B saw session A's edit");

    // Closing one leaves the other alone.
    expect_ok(a.call(Request::SessionClose { session: session_a }));
    let still_there = tree(b.call(Request::ShowCandidate {
        session: session_b,
        path: Path::root(),
    }));
    assert!(still_there.is_empty());
}

#[test]
fn an_unknown_session_is_refused() {
    let harness = Harness::start();
    let mut client = harness.connect();

    let ghost = SessionId::parse("0123456789abcdef").unwrap();
    let response = client.call(Request::ShowCandidate {
        session: ghost,
        path: Path::root(),
    });
    let Response::Failed { kind, message } = &response else {
        panic!("expected a failure, got {response:?}");
    };
    assert_eq!(*kind, FailureKind::Request);
    assert!(message.contains("expired or was never opened"), "{message}");
}

/// An operator's unsaved work must outlive a `systemctl restart`.
#[test]
fn candidates_survive_a_restart() {
    let harness = Harness::start();
    let (mut client, session) = harness.session();
    expect_ok(client.call(set(&session, "system host-name", "survivor")));
    expect_ok(client.call(set(
        &session,
        "interfaces ethernet eth0 address",
        "10.0.0.1/24",
    )));
    drop(client);

    let harness = harness.restart();
    let mut client = harness.connect();

    let candidate = tree(client.call(Request::ShowCandidate {
        session: session.clone(),
        path: Path::root(),
    }));
    assert_eq!(
        candidate.get(&p("system host-name")).unwrap().value(),
        Some("survivor")
    );
    assert!(candidate.contains(&p("interfaces ethernet eth0 address")));

    // And the recovered session is still usable, not just readable.
    expect_ok(client.call(set(&session, "system time-zone", "UTC")));
}

#[test]
fn show_saved_reads_config_boot_from_disk() {
    let harness = Harness::start();
    let mut client = harness.connect();

    // Nothing saved yet is an empty config rather than an error.
    let empty = tree(client.call(Request::ShowSaved { path: Path::root() }));
    assert!(empty.is_empty());

    std::fs::create_dir_all(harness.paths().etc_dir()).unwrap();
    std::fs::write(
        harness.paths().config_boot(),
        "system {\n    host-name saved-name\n}\n",
    )
    .unwrap();

    let saved = tree(client.call(Request::ShowSaved { path: Path::root() }));
    assert_eq!(
        saved.get(&p("system host-name")).unwrap().value(),
        Some("saved-name")
    );

    // A file edited into something that does not parse is diagnosed with the
    // parser's line and column, not swallowed.
    std::fs::write(harness.paths().config_boot(), "system {\n    hostname\n").unwrap();
    let message = expect_failure(client.call(Request::ShowSaved { path: Path::root() }));
    assert!(message.contains("line 1"), "{message}");
}

#[test]
fn a_saved_config_round_trips_through_the_socket() {
    let harness = Harness::start();
    let (mut client, session) = harness.session();

    for (path, value) in [
        ("system host-name", "fw-01"),
        ("interfaces ethernet eth0 address", "192.168.1.1/24"),
        ("interfaces ethernet eth0 description", "the uplink"),
    ] {
        expect_ok(client.call(set(&session, path, value)));
    }
    let candidate = tree(client.call(Request::ShowCandidate {
        session,
        path: Path::root(),
    }));

    let text = curly::render(&candidate, nightshade_schema::model::Schema::compiled());
    assert!(text.contains("ethernet eth0 {"), "{text}");
    assert_eq!(curly::parse(&text).unwrap(), candidate);
}
