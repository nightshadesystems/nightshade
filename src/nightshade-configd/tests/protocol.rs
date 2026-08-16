//! What the socket does with input that is not a well-behaved client.
//!
//! configd runs as root and this socket is the whole of its attack surface, so
//! these are written from the outside: raw bytes onto the stream, and forged
//! messages the typed `Request` cannot express.

mod harness;

use harness::Harness;
use nightshade_proto::frame;
use nightshade_proto::message::{FailureKind, Request, Response};
use nightshade_schema::path::Path;
use serde::Serialize;

/// A message shaped like a `Request` but with fields the real type would not
/// allow. ciborium tags variants by name, so this decodes against
/// `Request::ShowCandidate` -- and has to be refused by `SessionId`.
#[derive(Serialize)]
enum Forged {
    ShowCandidate { session: String, path: Vec<String> },
}

#[test]
fn an_oversized_frame_is_refused_from_its_header() {
    let harness = Harness::start();
    let mut client = harness.connect();

    // A length one byte past the cap, and nothing behind it. If the limit were
    // checked after reading, configd would now be waiting on four megabytes it
    // was told to expect.
    let claimed = frame::MAX_BODY as u32 + 1;
    client.send_raw(&claimed.to_be_bytes());

    let response = client.read().expect("a refusal, not a hang");
    let Response::Failed { kind, message } = response else {
        panic!("expected a refusal");
    };
    assert_eq!(kind, FailureKind::Request);
    assert!(message.contains("exceeds"), "{message}");

    // The connection is dropped afterwards: there is no way to know where the
    // next message would have started.
    assert!(client.read().is_err());
}

#[test]
fn a_zero_length_frame_is_refused() {
    let harness = Harness::start();
    let mut client = harness.connect();
    client.send_raw(&0u32.to_be_bytes());
    assert!(matches!(
        client.read().expect("a refusal"),
        Response::Failed { .. }
    ));
}

#[test]
fn a_body_that_is_not_cbor_is_refused() {
    let harness = Harness::start();
    let mut client = harness.connect();

    let body = b"this is not a CBOR message";
    client.send_raw(&(body.len() as u32).to_be_bytes());
    client.send_raw(body);

    let Response::Failed { kind, .. } = client.read().expect("a refusal") else {
        panic!("expected a refusal");
    };
    assert_eq!(kind, FailureKind::Request);
}

/// The one that matters. A session id becomes a filename under `/run`, in a
/// process running as root; a traversal here would be a write primitive into
/// `/etc`.
#[test]
fn a_session_id_that_is_a_path_never_reaches_the_filesystem() {
    let harness = Harness::start();

    for forged in [
        "../../etc/nightshade/config",
        "../../../../tmp/owned",
        "..",
        "/etc/passwd",
        "",
        "0123456789abcdefX",
    ] {
        let mut client = harness.connect();
        let frame = frame::encode(&Forged::ShowCandidate {
            session: forged.to_string(),
            path: Vec::new(),
        })
        .unwrap();
        client.send_raw(&frame);

        let response = client.read().expect("a refusal");
        let Response::Failed { kind, .. } = response else {
            panic!("{forged:?} was accepted: {response:?}");
        };
        assert_eq!(kind, FailureKind::Request, "{forged:?}");
    }

    // And nothing was created outside the sessions directory.
    let sessions = harness.paths().sessions_dir();
    assert!(std::fs::read_dir(&sessions).unwrap().next().is_none());
    assert!(!harness.paths().config_boot().exists());
}

#[test]
fn one_connection_carries_many_requests() {
    let harness = Harness::start();
    let (mut client, session) = harness.session();

    for value in ["a-name", "b-name", "c-name"] {
        let response = client.call(Request::Set {
            session: session.clone(),
            path: Path::parse("system host-name").unwrap(),
            value: Some(value.into()),
        });
        assert_eq!(response, Response::Ok);
    }

    let Response::Config { tree } = client.call(Request::ShowCandidate {
        session,
        path: Path::root(),
    }) else {
        panic!("expected a config");
    };
    assert_eq!(
        tree.get(&Path::parse("system host-name").unwrap())
            .unwrap()
            .value(),
        Some("c-name")
    );
}

/// One misbehaving client must not take the daemon down with it.
#[test]
fn a_broken_connection_does_not_disturb_the_others() {
    let harness = Harness::start();
    let (mut good, session) = harness.session();

    // A client that sends rubbish and one that vanishes mid-frame.
    let mut rude = harness.connect();
    rude.send_raw(b"\x00\x00\x00\x08garbage!");
    let _ = rude.read();
    drop(rude);

    let mut truncated = harness.connect();
    truncated.send_raw(&[0x00, 0x00, 0x01]); // half a header
    drop(truncated);

    let response = good.call(Request::Set {
        session,
        path: Path::parse("system host-name").unwrap(),
        value: Some("still-here".into()),
    });
    assert_eq!(response, Response::Ok);
}

#[test]
fn the_socket_is_not_world_accessible() {
    use std::os::unix::fs::PermissionsExt;

    let harness = Harness::start();
    let mode = std::fs::metadata(harness.socket())
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o660, "socket mode is {mode:o}");
}
