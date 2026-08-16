//! A configd on a temporary socket, and a blocking client for it.
//!
//! The server runs on its own thread with its own runtime so the tests
//! themselves stay synchronous -- they are scripts of what an operator does,
//! and reading them should not require unpicking an async control flow.
//!
//! The client is the same framing the CLI will use, over a plain
//! `std::os::unix::net::UnixStream`. Testing against the real socket rather
//! than calling `Configd::handle` directly is the point: it covers the
//! framing, the peer check and the connection loop, which is where the
//! interesting failures are.

// This module is compiled separately into each test binary, and neither uses
// all of it. The alternative is splitting the harness by which test needs
// which half, which would be a worse harness for a tidier warning.
#![allow(dead_code)]

use std::io::Write;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use nightshade_common::paths::Paths;
use nightshade_configd::{Access, Bound, Configd, Server};
use nightshade_proto::frame;
use nightshade_proto::message::{Request, Response, SessionId};
use nightshade_render::MockHost;
use nightshade_schema::model::Schema;
use tempfile::TempDir;
use tokio::sync::watch;

pub struct Harness {
    dir: TempDir,
    paths: Paths,
    host: Arc<MockHost>,
    shutdown: watch::Sender<bool>,
    server: Option<std::thread::JoinHandle<()>>,
}

impl Harness {
    pub fn start() -> Self {
        let dir = tempfile::tempdir().expect("a temporary directory");
        Self::start_in(dir, Arc::new(MockHost::new()))
    }

    /// Start a fresh configd over an existing state directory, as a restart
    /// would.
    ///
    /// The host carries over, because a restart does not un-apply anything: the
    /// files a previous configd wrote into `/run/systemd/network` are still
    /// there, and so is the last-applied state it would restore from.
    pub fn restart(self) -> Self {
        let host = Arc::clone(&self.host);
        let dir = self.stop();
        Self::start_in(dir, host)
    }

    /// What the renderers did, so a test can assert on the ordering and the
    /// files without a network.
    pub fn host(&self) -> &MockHost {
        &self.host
    }

    fn start_in(dir: TempDir, host: Arc<MockHost>) -> Self {
        let paths = Paths::under(dir.path());
        let socket = paths.socket();
        let (shutdown, rx) = watch::channel(false);

        let serving = paths.clone();
        let host_for_server: Arc<dyn nightshade_render::Host> = Arc::clone(&host) as _;
        let host = Arc::clone(&host);
        let server = std::thread::spawn(move || {
            let host = host_for_server;
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("a runtime");
            runtime.block_on(async move {
                let access = Access::current_user();
                // Renderers write their artifacts for real, into memory, and
                // apply nothing. Every decision the pipeline makes is
                // exercised; only the last inch that would change a live
                // interface is not.
                let configd = Arc::new(
                    Configd::start(Schema::compiled(), serving, host).expect("configd starts"),
                );
                configd.resume().await;
                let bound = Bound::create(&socket, &access).expect("the socket binds");
                Server::new(configd, access).run(bound, rx).await;
            });
        });

        let harness = Self {
            dir,
            paths,
            host,
            shutdown,
            server: Some(server),
        };
        harness.wait_until_ready();
        harness
    }

    /// Stop the server and hand back the state directory.
    fn stop(mut self) -> TempDir {
        let _ = self.shutdown.send(true);
        if let Some(server) = self.server.take() {
            let _ = server.join();
        }
        // Replaced with a throwaway so `Drop` has something to work with.
        std::mem::replace(&mut self.dir, tempfile::tempdir().expect("a temporary directory"))
    }

    fn wait_until_ready(&self) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if UnixStream::connect(self.paths.socket()).is_ok() {
                return;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        panic!("configd never started listening on {}", self.socket().display());
    }

    pub fn socket(&self) -> PathBuf {
        self.paths.socket()
    }

    pub fn paths(&self) -> &Paths {
        &self.paths
    }

    /// Move the pending-confirm deadline, so the rollback can be tested
    /// without waiting out a window measured in minutes.
    ///
    /// Reaches into the marker file rather than into configd, which is the
    /// honest way round: the marker is the contract between one run of the
    /// daemon and the next, and a test that can only set the deadline through
    /// an internal API would not be testing that contract.
    pub fn move_confirm_deadline(&self, seconds_from_now: i64) {
        let path = self.paths.pending_confirm();
        let text = std::fs::read_to_string(&path).expect("a pending-confirm marker");
        let mut marker: serde_json::Value =
            serde_json::from_str(&text).expect("the marker is JSON");
        let now = jiff::Timestamp::now().as_second();
        marker["deadline"] = serde_json::json!(now + seconds_from_now);
        std::fs::write(&path, serde_json::to_vec(&marker).unwrap()).expect("writing the marker");
    }

    pub fn confirm_pending(&self) -> bool {
        self.paths.pending_confirm().exists()
    }

    pub fn connect(&self) -> Client {
        Client {
            stream: UnixStream::connect(self.socket()).expect("connecting to configd"),
        }
    }

    /// A connected client with a session already open.
    pub fn session(&self) -> (Client, SessionId) {
        let mut client = self.connect();
        let id = match client.call(Request::SessionOpen) {
            Response::Session { id } => id,
            other => panic!("SessionOpen answered {other:?}"),
        };
        (client, id)
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        let _ = self.shutdown.send(true);
        if let Some(server) = self.server.take() {
            let _ = server.join();
        }
    }
}

pub struct Client {
    stream: UnixStream,
}

impl Client {
    pub fn call(&mut self, request: Request) -> Response {
        frame::write_blocking(&mut self.stream, &request).expect("sending a request");
        frame::read_blocking(&mut self.stream).expect("reading a response")
    }

    /// Send bytes that are not a well-formed message.
    pub fn send_raw(&mut self, bytes: &[u8]) {
        self.stream.write_all(bytes).expect("writing raw bytes");
        self.stream.flush().expect("flushing");
    }

    pub fn read(&mut self) -> Result<Response, frame::FrameError> {
        frame::read_blocking(&mut self.stream)
    }
}

/// `Response::Ok`, or a panic naming what came back instead.
#[track_caller]
pub fn expect_ok(response: Response) {
    match response {
        Response::Ok => {}
        other => panic!("expected Ok, got {other:?}"),
    }
}

#[track_caller]
pub fn expect_failure(response: Response) -> String {
    match response {
        Response::Failed { message, .. } => message,
        other => panic!("expected a failure, got {other:?}"),
    }
}
