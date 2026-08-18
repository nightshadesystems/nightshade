//! Startup: applying `config.boot`, and what happens when it will not load.

mod harness;

use harness::{Harness, expect_ok};
use nightshade_configd::boot::Outcome;
use nightshade_proto::message::{Request, Response, SessionId};
use nightshade_schema::config::ConfigTree;
use nightshade_schema::path::Path;

fn p(s: &str) -> Path {
    Path::parse(s).unwrap()
}

fn running(client: &mut harness::Client) -> ConfigTree {
    match client.call(Request::ShowRunning { path: Path::root() }) {
        Response::Config { tree } => tree,
        other => panic!("expected a config, got {other:?}"),
    }
}

fn save_boot(harness: &Harness, text: &str) {
    std::fs::create_dir_all(harness.paths().etc_dir()).unwrap();
    std::fs::write(harness.paths().config_boot(), text).unwrap();
}

const GOOD: &str = "\
system {
    host-name saved-box
    time-zone UTC
}
interfaces {
    ethernet eth0 {
        address 10.0.0.1/24
    }
}
";

#[test]
fn a_saved_config_is_applied_at_startup() {
    let harness = Harness::start();
    save_boot(&harness, GOOD);

    let harness = harness.reboot();
    let mut client = harness.connect();

    let running = running(&mut client);
    assert_eq!(
        running.get(&p("system host-name")).unwrap().value(),
        Some("saved-box")
    );
    assert!(running.contains(&p("interfaces ethernet eth0 address")));

    // And it was really applied, not just recorded.
    let files = harness.host().files();
    assert!(
        files.contains_key(&harness.paths().networkd_dir().join("10-ns-eth0.network")),
        "{files:#?}"
    );
    assert!(
        harness
            .host()
            .commands()
            .contains(&"hostnamectl set-hostname saved-box".to_string())
    );
    assert!(!harness.paths().boot_failure().exists());
}

/// A box out of the installer has nothing saved. That is not a failure, and it
/// is not a reason to apply anything either.
#[test]
fn nothing_saved_applies_nothing() {
    let harness = Harness::start();
    let harness = harness.reboot();
    let mut client = harness.connect();

    // Empty is the true statement: Nightshade has applied nothing here yet.
    assert!(running(&mut client).is_empty());
    assert!(!harness.paths().boot_failure().exists());

    // In particular, resolv.conf has not been taken over by a managed file
    // with no resolvers in it.
    assert!(
        harness.host().file(harness.paths().resolv_conf()).is_none(),
        "a fresh box had its resolv.conf overwritten before anything was configured"
    );
    assert!(harness.host().commands().is_empty(), "{:?}", harness.host().commands());
}

/// The case the whole fallback exists for.
#[test]
fn a_config_that_will_not_parse_comes_up_on_defaults_and_says_why() {
    let harness = Harness::start();
    save_boot(&harness, "system {\n    hostname saved-box\n");

    let harness = harness.reboot();
    let mut client = harness.connect();

    // The box is up and answering, which is the whole point.
    let running = running(&mut client);
    assert_eq!(
        running.get(&p("system host-name")).unwrap().value(),
        Some("nightshade")
    );

    // And the reason is where an operator will meet it.
    let reason = std::fs::read_to_string(harness.paths().boot_failure()).unwrap();
    assert!(reason.contains("does not parse"), "{reason}");
    assert!(reason.contains("line 1"), "{reason}");

    // Deliberately no network. A firewall whose policy did not load must not
    // bring interfaces up on its own.
    assert!(!running.contains(&p("interfaces")));
    let files = harness.host().files();
    assert!(
        !files.keys().any(|path| path
            .to_string_lossy()
            .contains("10-ns-eth0.network")),
        "{files:#?}"
    );
}

/// The upgrade case: a config written under a schema that has since moved.
#[test]
fn a_config_the_schema_no_longer_describes_comes_up_on_defaults() {
    let harness = Harness::start();
    save_boot(
        &harness,
        "system {\n    hostname saved-box\n    nameserver 1.1.1.1\n}\n",
    );

    let harness = harness.reboot();
    let mut client = harness.connect();

    assert_eq!(
        running(&mut client).get(&p("system host-name")).unwrap().value(),
        Some("nightshade")
    );
    let reason = std::fs::read_to_string(harness.paths().boot_failure()).unwrap();
    assert!(reason.contains("nameserver"), "{reason}");
}

/// The failure marker must not outlive the failure.
#[test]
fn fixing_the_config_clears_the_warning() {
    let harness = Harness::start();
    save_boot(&harness, "system {\n    hostname saved-box\n");
    let harness = harness.reboot();
    assert!(harness.paths().boot_failure().exists());

    save_boot(&harness, GOOD);
    let harness = harness.reboot();
    assert!(
        !harness.paths().boot_failure().exists(),
        "the warning outlived the problem it was about"
    );
}

/// A configd restart is not a reboot. `/run` still holds the running config,
/// so re-applying `config.boot` would undo everything committed since boot and
/// not yet saved.
#[test]
fn a_restart_does_not_re_apply_the_saved_config() {
    let harness = Harness::start();
    save_boot(&harness, GOOD);
    let harness = harness.reboot();

    let mut client = harness.connect();
    let session = match client.call(Request::SessionOpen) {
        Response::Session { id } => id,
        other => panic!("{other:?}"),
    };
    expect_ok(client.call(Request::Set {
        session: session.clone(),
        path: p("system host-name"),
        value: Some("changed-since-boot".into()),
    }));
    match client.call(Request::Commit {
        session,
        comment: None,
        confirm_minutes: None,
    }) {
        Response::Committed { .. } => {}
        other => panic!("{other:?}"),
    }
    drop(client);

    // configd restarts. `config.boot` still says `saved-box`, and the box must
    // stay as it was committed.
    let harness = harness.restart();
    let mut client = harness.connect();
    assert_eq!(
        running(&mut client).get(&p("system host-name")).unwrap().value(),
        Some("changed-since-boot"),
        "a restart re-applied config.boot over what was committed"
    );
}

#[test]
fn the_outcome_says_which_of_the_four_things_happened() {
    // A small type, and worth pinning: `main` decides what to print from it,
    // and an operator's first sight of a broken box is that decision.
    assert_eq!(
        Outcome::Defaults {
            reason: "because".into()
        }
        .failed(),
        Some("because")
    );
    assert_eq!(Outcome::NothingSaved.failed(), None);
    assert_eq!(Outcome::Applied { changes: 3 }.failed(), None);
    assert_eq!(Outcome::AlreadyRunning { generation: 2 }.failed(), None);
}

#[test]
fn an_unreadable_session_id_cannot_be_used_after_a_reboot() {
    // Sessions live in /run, so a reboot takes them with it.
    let harness = Harness::start();
    let (mut client, session) = harness.session();
    expect_ok(client.call(Request::Set {
        session: session.clone(),
        path: p("system host-name"),
        value: Some("gone".into()),
    }));
    drop(client);

    let harness = harness.reboot();
    let mut client = harness.connect();
    let response = client.call(Request::ShowCandidate {
        session: SessionId::parse(session.as_str()).unwrap(),
        path: Path::root(),
    });
    assert!(
        matches!(response, Response::Failed { .. }),
        "a session survived a reboot: {response:?}"
    );
}
