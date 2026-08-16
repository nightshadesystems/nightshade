//! Commit-confirm, the archive, rollback, save and load.
//!
//! The commit-confirm tests are written from the outside, against the marker
//! file, because the marker is the contract between one run of configd and the
//! next. A test that could only reach the deadline through an internal API
//! would not be testing the thing that has to work when the daemon dies.

mod harness;

use std::time::Duration;

use harness::{Harness, expect_failure, expect_ok};
use nightshade_proto::message::{FailureKind, LoadSource, Request, Response, SessionId};
use nightshade_schema::config::ConfigTree;
use nightshade_schema::path::Path;

fn p(s: &str) -> Path {
    Path::parse(s).unwrap()
}

fn set(session: &SessionId, path: &str, value: &str) -> Request {
    Request::Set {
        session: session.clone(),
        path: p(path),
        value: Some(value.to_string()),
    }
}

fn commit(session: &SessionId, confirm_minutes: Option<u16>) -> Request {
    Request::Commit {
        session: session.clone(),
        comment: Some("a change".into()),
        confirm_minutes,
    }
}

#[track_caller]
fn committed(response: Response) -> (u64, Option<u64>) {
    match response {
        Response::Committed {
            generation,
            confirm_within,
            ..
        } => (generation, confirm_within),
        other => panic!("expected a commit, got {other:?}"),
    }
}

fn running(client: &mut harness::Client) -> ConfigTree {
    match client.call(Request::ShowRunning { path: Path::root() }) {
        Response::Config { tree } => tree,
        other => panic!("expected a config, got {other:?}"),
    }
}

fn host_name(client: &mut harness::Client) -> Option<String> {
    running(client)
        .get(&p("system host-name"))
        .and_then(|node| node.value().map(str::to_string))
}

fn revisions(client: &mut harness::Client) -> Vec<nightshade_proto::RevisionInfo> {
    match client.call(Request::CommitLog) {
        Response::Revisions { revisions } => revisions,
        other => panic!("expected revisions, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// commit-confirm
// ---------------------------------------------------------------------------

#[test]
fn a_confirmed_commit_is_kept() {
    let harness = Harness::start();
    let (mut client, session) = harness.session();

    expect_ok(client.call(set(&session, "system host-name", "after")));
    let (generation, within) = committed(client.call(commit(&session, Some(5))));
    assert_eq!(generation, 1);
    assert!(within.unwrap() > 290, "{within:?}");
    assert!(harness.confirm_pending(), "no marker was written");

    // The change is applied during the window. What is pending is whether it
    // stays, not whether it took effect.
    assert_eq!(host_name(&mut client).as_deref(), Some("after"));

    committed(client.call(Request::Confirm {
        session: session.clone(),
    }));
    assert!(!harness.confirm_pending(), "the marker outlived the confirmation");
    assert_eq!(host_name(&mut client).as_deref(), Some("after"));

    // Only now is it a revision.
    let log = revisions(&mut client);
    assert_eq!(log.len(), 1);
    assert_eq!(log[0].revision, 1);
    assert_eq!(log[0].comment.as_deref(), Some("a change"));
}

/// The case the whole mechanism exists for: the operator loses their session
/// and the box puts itself back.
#[test]
fn an_unconfirmed_commit_rolls_back_when_the_deadline_passes() {
    let harness = Harness::start();
    let (mut client, session) = harness.session();

    expect_ok(client.call(set(&session, "system host-name", "before")));
    committed(client.call(commit(&session, None)));

    expect_ok(client.call(set(&session, "system host-name", "after")));
    committed(client.call(commit(&session, Some(5))));
    assert_eq!(host_name(&mut client).as_deref(), Some("after"));

    // The operator's session goes away, exactly as it would if their route to
    // the box had just been cut by the change they made.
    drop(client);

    // Bring the deadline forward and restart, which arms the timer for what is
    // left of the window -- a second.
    harness.move_confirm_deadline(1);
    let harness = harness.restart();

    let mut client = harness.connect();
    wait_until(Duration::from_secs(15), || !harness.confirm_pending());

    assert_eq!(
        host_name(&mut client).as_deref(),
        Some("before"),
        "the unconfirmed change was not rolled back"
    );
    // A rolled-back change was never a revision.
    let log = revisions(&mut client);
    assert_eq!(log.len(), 1, "{log:#?}");
    assert_eq!(log[0].revision, 1);
}

/// configd restarting inside the window must resume the timer, not forget it.
#[test]
fn a_restart_inside_the_window_keeps_the_commit_pending() {
    let harness = Harness::start();
    let (mut client, session) = harness.session();

    expect_ok(client.call(set(&session, "system host-name", "before")));
    committed(client.call(commit(&session, None)));
    expect_ok(client.call(set(&session, "system host-name", "after")));
    committed(client.call(commit(&session, Some(30))));
    drop(client);

    let harness = harness.restart();
    let mut client = harness.connect();

    assert!(harness.confirm_pending(), "the marker was lost across a restart");
    assert_eq!(host_name(&mut client).as_deref(), Some("after"));

    // Still pending, so a fresh commit is refused -- editing a candidate is
    // not, because that changes nothing on the box.
    let (mut other, other_session) = harness.session();
    expect_ok(other.call(set(&other_session, "system time-zone", "UTC")));
    let message = expect_failure(other.call(commit(&other_session, None)));
    assert!(message.contains("waiting to be confirmed"), "{message}");

    // And confirming after the restart works.
    committed(client.call(Request::Confirm {
        session: session.clone(),
    }));
    assert!(!harness.confirm_pending());
    assert_eq!(host_name(&mut client).as_deref(), Some("after"));
}

/// configd down when the deadline passes: the rollback happens on startup.
#[test]
fn a_deadline_that_passed_while_configd_was_down_rolls_back_at_once() {
    let harness = Harness::start();
    let (mut client, session) = harness.session();

    expect_ok(client.call(set(&session, "system host-name", "before")));
    committed(client.call(commit(&session, None)));
    expect_ok(client.call(set(&session, "system host-name", "after")));
    committed(client.call(commit(&session, Some(5))));
    drop(client);

    // The deadline goes by while nothing is running.
    harness.move_confirm_deadline(-120);
    let harness = harness.restart();
    let mut client = harness.connect();

    assert!(
        !harness.confirm_pending(),
        "a deadline that had already passed was not acted on"
    );
    assert_eq!(host_name(&mut client).as_deref(), Some("before"));
}

#[test]
fn only_one_commit_can_be_awaiting_confirmation() {
    let harness = Harness::start();
    let (mut client, session) = harness.session();

    expect_ok(client.call(set(&session, "system host-name", "first")));
    committed(client.call(commit(&session, Some(30))));

    expect_ok(client.call(set(&session, "system host-name", "second")));
    let response = client.call(commit(&session, Some(30)));
    let Response::Failed { kind, message } = &response else {
        panic!("expected a refusal, got {response:?}");
    };
    assert_eq!(*kind, FailureKind::Conflict);
    assert!(message.contains("waiting to be confirmed"), "{message}");
    assert!(message.contains("seconds left"), "{message}");
}

#[test]
fn confirming_nothing_is_an_error() {
    let harness = Harness::start();
    let (mut client, session) = harness.session();
    let message = expect_failure(client.call(Request::Confirm { session }));
    assert!(message.contains("nothing is waiting"), "{message}");
}

/// A colleague must be able to confirm: the operator who committed may be
/// precisely the one who has just lost their session.
#[test]
fn another_session_can_confirm() {
    let harness = Harness::start();
    let (mut client, session) = harness.session();
    expect_ok(client.call(set(&session, "system host-name", "after")));
    committed(client.call(commit(&session, Some(30))));
    drop(client);

    let (mut other, other_session) = harness.session();
    committed(other.call(Request::Confirm {
        session: other_session,
    }));
    assert!(!harness.confirm_pending());
    assert_eq!(host_name(&mut other).as_deref(), Some("after"));
}

// ---------------------------------------------------------------------------
// the archive and rollback
// ---------------------------------------------------------------------------

#[test]
fn the_commit_log_records_who_did_what() {
    let harness = Harness::start();
    let (mut client, session) = harness.session();

    for name in ["one", "two", "three"] {
        expect_ok(client.call(set(&session, "system host-name", name)));
        committed(client.call(Request::Commit {
            session: session.clone(),
            comment: Some(format!("set the name to {name}")),
            confirm_minutes: None,
        }));
    }

    let log = revisions(&mut client);
    assert_eq!(log.len(), 3);
    // Newest first.
    assert_eq!(log[0].revision, 3);
    assert_eq!(log[2].revision, 1);
    assert_eq!(log[0].comment.as_deref(), Some("set the name to three"));
    assert!(!log[0].actor.is_empty());
    assert_eq!(log[0].timestamp.len(), 16, "{}", log[0].timestamp);
    assert!(
        log[0].changes.iter().any(|c| c.to_string().contains("three")),
        "{:#?}",
        log[0].changes
    );
}

/// Three commits, roll back to the first, commit, and the box is as it was.
#[test]
fn rolling_back_loads_a_revision_and_leaves_committing_to_the_operator() {
    let harness = Harness::start();
    let (mut client, session) = harness.session();

    for name in ["one", "two", "three"] {
        expect_ok(client.call(set(&session, "system host-name", name)));
        committed(client.call(commit(&session, None)));
    }
    assert_eq!(host_name(&mut client).as_deref(), Some("three"));

    expect_ok(client.call(Request::Load {
        session: session.clone(),
        source: LoadSource::Archive { revision: 1 },
    }));

    // Loaded into the candidate, applied to nothing. Rollback never changes
    // the box on its own.
    assert_eq!(host_name(&mut client).as_deref(), Some("three"));

    committed(client.call(commit(&session, None)));
    assert_eq!(host_name(&mut client).as_deref(), Some("one"));

    // The rollback is itself a revision, so the history shows what happened
    // rather than quietly rewinding.
    let log = revisions(&mut client);
    assert_eq!(log.len(), 4);
    assert_eq!(log[0].revision, 4);
}

#[test]
fn rolling_back_to_a_revision_that_is_not_there_says_so() {
    let harness = Harness::start();
    let (mut client, session) = harness.session();
    let message = expect_failure(client.call(Request::Load {
        session,
        source: LoadSource::Archive { revision: 99 },
    }));
    assert!(message.contains("not in the archive"), "{message}");
}

// ---------------------------------------------------------------------------
// save and load
// ---------------------------------------------------------------------------

/// The acceptance case: save, edit the file by hand, load it back, commit.
#[test]
fn a_saved_config_can_be_edited_by_hand_and_loaded_back() {
    let harness = Harness::start();
    let (mut client, session) = harness.session();

    expect_ok(client.call(set(&session, "system host-name", "fw-01")));
    expect_ok(client.call(set(
        &session,
        "interfaces ethernet eth0 address",
        "10.0.0.1/24",
    )));
    committed(client.call(commit(&session, None)));
    expect_ok(client.call(Request::Save));

    // What landed on disk is the format an operator edits.
    let path = harness.paths().config_boot();
    let saved = std::fs::read_to_string(&path).unwrap();
    assert!(saved.contains("host-name fw-01"), "{saved}");
    assert!(saved.contains("ethernet eth0 {"), "{saved}");

    // Hand-edit it, the way somebody would in vi.
    let edited = saved.replace("host-name fw-01", "host-name fw-02\n    time-zone UTC");
    std::fs::write(&path, edited).unwrap();

    expect_ok(client.call(Request::Load {
        session: session.clone(),
        source: LoadSource::Saved,
    }));
    committed(client.call(commit(&session, None)));

    assert_eq!(host_name(&mut client).as_deref(), Some("fw-02"));
    let running = running(&mut client);
    assert_eq!(
        running.get(&p("system time-zone")).unwrap().value(),
        Some("UTC")
    );
    assert!(running.contains(&p("interfaces ethernet eth0 address")));
}

#[test]
fn a_hand_edited_file_with_a_mistake_in_it_is_refused_and_changes_nothing() {
    let harness = Harness::start();
    let (mut client, session) = harness.session();

    expect_ok(client.call(set(&session, "system host-name", "fw-01")));
    committed(client.call(commit(&session, None)));
    expect_ok(client.call(Request::Save));

    // A typo an operator would plausibly make.
    let path = harness.paths().config_boot();
    std::fs::write(
        &path,
        "system {\n    host-name fw-02\n    nameserver 1.1.1.1\n}\n",
    )
    .unwrap();

    let message = expect_failure(client.call(Request::Load {
        session: session.clone(),
        source: LoadSource::Saved,
    }));
    assert!(message.contains("nameserver"), "{message}");
    assert!(message.contains("has not been changed"), "{message}");

    // The candidate is untouched, so nothing was half-loaded.
    let Response::Config { tree } = client.call(Request::ShowCandidate {
        session,
        path: Path::root(),
    }) else {
        panic!("expected a config");
    };
    assert_eq!(
        tree.get(&p("system host-name")).unwrap().value(),
        Some("fw-01")
    );
}

#[test]
fn a_file_that_does_not_parse_is_reported_with_a_position() {
    let harness = Harness::start();
    let (mut client, session) = harness.session();

    std::fs::create_dir_all(harness.paths().etc_dir()).unwrap();
    std::fs::write(harness.paths().config_boot(), "system {\n    host-name fw\n").unwrap();

    let message = expect_failure(client.call(Request::Load {
        session,
        source: LoadSource::Saved,
    }));
    assert!(message.contains("line 1"), "{message}");
}

#[test]
fn loading_when_nothing_has_been_saved_says_so() {
    let harness = Harness::start();
    let (mut client, session) = harness.session();
    let message = expect_failure(client.call(Request::Load {
        session,
        source: LoadSource::Saved,
    }));
    assert!(message.contains("nothing has been saved"), "{message}");
}

fn wait_until(limit: Duration, mut done: impl FnMut() -> bool) {
    let deadline = std::time::Instant::now() + limit;
    while std::time::Instant::now() < deadline {
        if done() {
            return;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    panic!("condition never became true within {limit:?}");
}
