//! The socket, and what happens on it.
//!
//! # Where the socket comes from
//!
//! Normally from systemd: `nightshade-configd.socket` creates and owns it, and
//! hands it over as file descriptor 3. That is what makes the mode and the
//! ownership a packaging decision rather than a race in this code, and it
//! means a client connecting before the daemon is up waits rather than fails.
//!
//! When there is no systemd -- the tests, a foreground run while debugging --
//! the same socket is created here, with the same mode and group. Both paths
//! produce a listener; nothing above this cares which.
//!
//! # Timeouts
//!
//! A connection that has said nothing for [`IDLE_TIMEOUT`] is closed. A client
//! holding a descriptor open forever is not malicious, it is a CLI whose
//! terminal was closed, and there is no reason for configd to keep the
//! bookkeeping. The candidate is in `/run` regardless, so the operator loses
//! nothing by reconnecting.

use std::os::fd::{FromRawFd, RawFd};
use std::os::unix::net::UnixListener as StdUnixListener;
use std::path::{Path as FsPath, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use nightshade_common::SOCKET_MODE;
use nightshade_proto::frame;
use nightshade_proto::message::{Request, Response};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::watch;
use tokio::task::JoinSet;
use tracing::{debug, error, info, warn};

use crate::peer::{Access, authenticate};
use crate::state::Configd;

/// Silence after which a connection is closed.
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(300);

/// How long a shutting-down daemon waits for connections to finish.
const DRAIN_TIMEOUT: Duration = Duration::from_secs(5);

/// The first descriptor systemd passes for socket activation.
const SD_LISTEN_FDS_START: RawFd = 3;

#[derive(Debug, thiserror::Error)]
pub enum ServerError {
    #[error("{context}: {source}")]
    Io {
        context: String,
        #[source]
        source: std::io::Error,
    },

    #[error("looking up group {group}: {source}")]
    Group {
        group: String,
        #[source]
        source: nix::Error,
    },
}

fn io(context: impl Into<String>) -> impl FnOnce(std::io::Error) -> ServerError {
    let context = context.into();
    move |source| ServerError::Io { context, source }
}

/// A listener, and whether the socket file is ours to remove.
pub struct Bound {
    listener: UnixListener,
    /// Set only when this process created the socket. A systemd-activated
    /// socket belongs to systemd, and unlinking it on exit would break the
    /// next activation.
    owned_path: Option<PathBuf>,
}

impl Bound {
    /// Take the socket systemd passed, if it passed one.
    ///
    /// Checks `LISTEN_PID` against our own: the variables are inherited, so a
    /// child process would otherwise believe it had been handed a socket that
    /// belongs to its parent.
    pub fn from_systemd() -> Result<Option<Self>, ServerError> {
        let pid_matches = std::env::var("LISTEN_PID")
            .ok()
            .and_then(|pid| pid.parse::<u32>().ok())
            .is_some_and(|pid| pid == std::process::id());
        let count = std::env::var("LISTEN_FDS")
            .ok()
            .and_then(|count| count.parse::<i32>().ok())
            .unwrap_or(0);

        if !pid_matches || count < 1 {
            return Ok(None);
        }

        // Cleared so anything configd execs does not inherit the claim.
        unsafe {
            std::env::remove_var("LISTEN_PID");
            std::env::remove_var("LISTEN_FDS");
            std::env::remove_var("LISTEN_FDNAMES");
        }

        // SAFETY: systemd guarantees descriptor 3 is an open listening socket
        // when LISTEN_FDS says so and LISTEN_PID names this process, and the
        // variables have just been cleared so nothing else will claim it.
        let listener = unsafe { StdUnixListener::from_raw_fd(SD_LISTEN_FDS_START) };
        listener
            .set_nonblocking(true)
            .map_err(io("preparing the activated socket"))?;

        info!("using the socket passed by systemd");
        Ok(Some(Self {
            listener: UnixListener::from_std(listener).map_err(io("adopting the socket"))?,
            owned_path: None,
        }))
    }

    /// Create the socket ourselves.
    pub fn create(path: &FsPath, access: &Access) -> Result<Self, ServerError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(io(format!("creating {}", parent.display())))?;
        }

        // A socket file left by a previous run stops us binding. Removing one
        // that something is still listening on would steal its clients, so it
        // is only removed once a connection to it is refused.
        if path.exists() {
            match StdUnixListener::bind(path) {
                Ok(_) => {}
                Err(_) => match std::os::unix::net::UnixStream::connect(path) {
                    Ok(_) => {
                        return Err(ServerError::Io {
                            context: format!("{} is already being served", path.display()),
                            source: std::io::Error::from(std::io::ErrorKind::AddrInUse),
                        });
                    }
                    Err(_) => {
                        debug!(path = %path.display(), "removing a stale socket");
                        std::fs::remove_file(path)
                            .map_err(io(format!("removing {}", path.display())))?;
                    }
                },
            }
            let _ = std::fs::remove_file(path);
        }

        let listener =
            StdUnixListener::bind(path).map_err(io(format!("binding {}", path.display())))?;
        listener
            .set_nonblocking(true)
            .map_err(io("preparing the socket"))?;

        restrict(path, access)?;

        Ok(Self {
            listener: UnixListener::from_std(listener).map_err(io("adopting the socket"))?,
            owned_path: Some(path.to_path_buf()),
        })
    }
}

/// Mode 0660, owned by the admin group.
///
/// The peer check in `peer.rs` would catch an unauthorised connection anyway.
/// This stops one being made: a connection refused by the kernel never reaches
/// the CBOR decoder, and the decoder is the part written by us.
fn restrict(path: &FsPath, access: &Access) -> Result<(), ServerError> {
    use std::os::unix::fs::PermissionsExt;

    std::fs::set_permissions(path, std::fs::Permissions::from_mode(SOCKET_MODE))
        .map_err(io(format!("setting the mode on {}", path.display())))?;

    let Access::Group(group) = access else {
        return Ok(());
    };
    let found = nix::unistd::Group::from_name(group).map_err(|source| ServerError::Group {
        group: group.clone(),
        source,
    })?;
    match found {
        Some(admin) => {
            nix::unistd::chown(path, None, Some(admin.gid))
                .map_err(|e| ServerError::Group {
                    group: group.clone(),
                    source: e,
                })?;
        }
        None => {
            // Not fatal: with mode 0660 and no group, only root can connect,
            // which is a usable if unhelpful appliance. Saying so is what turns
            // "the CLI does not work" into a five-second fix.
            warn!(
                group = %group,
                "group does not exist; only root will be able to configure this system"
            );
        }
    }
    Ok(())
}

pub struct Server {
    configd: Arc<Configd>,
    access: Access,
}

impl Server {
    pub fn new(configd: Arc<Configd>, access: Access) -> Self {
        Self { configd, access }
    }

    /// Accept until `shutdown` fires, then let in-flight requests finish.
    pub async fn run(self, bound: Bound, mut shutdown: watch::Receiver<bool>) {
        let Bound {
            listener,
            owned_path,
        } = bound;
        let mut connections = JoinSet::new();

        info!("ready");
        loop {
            tokio::select! {
                accepted = listener.accept() => match accepted {
                    Ok((stream, _)) => {
                        let configd = Arc::clone(&self.configd);
                        let access = self.access.clone();
                        connections.spawn(async move {
                            if let Err(e) = serve(stream, configd, access).await {
                                debug!(error = %e, "connection ended");
                            }
                        });
                    }
                    Err(e) => {
                        // Per-connection failures are the client's problem, not
                        // a reason to stop serving everyone else.
                        warn!(error = %e, "accept failed");
                    }
                },
                _ = shutdown.changed() => break,
            }

            // Reap finished connections so the set does not grow without
            // bound over the life of the daemon.
            while connections.try_join_next().is_some() {}
        }

        info!(in_flight = connections.len(), "shutting down");
        if tokio::time::timeout(DRAIN_TIMEOUT, async {
            while connections.join_next().await.is_some() {}
        })
        .await
        .is_err()
        {
            warn!("gave up waiting for connections to finish");
        }

        if let Some(path) = owned_path {
            let _ = std::fs::remove_file(path);
        }
    }
}

async fn serve(
    mut stream: UnixStream,
    configd: Arc<Configd>,
    access: Access,
) -> Result<(), std::io::Error> {
    let credentials = stream.peer_cred()?;
    let actor = match authenticate(
        credentials.uid(),
        credentials.gid(),
        credentials.pid(),
        &access,
    ) {
        Ok(actor) => actor,
        Err(e) => {
            // Refused connections are logged at warn: on an appliance this is
            // either a misconfiguration or somebody trying, and both are worth
            // a line.
            warn!(uid = credentials.uid(), error = %e, "refused a connection");
            // Answered rather than dropped, so the operator gets the reason
            // instead of a closed socket.
            let response = Response::failed(
                nightshade_proto::message::FailureKind::Request,
                e.to_string(),
            );
            let _ = write(&mut stream, &response).await;
            return Ok(());
        }
    };

    debug!(actor = %actor.describe(), "connection accepted");

    loop {
        let request = match tokio::time::timeout(IDLE_TIMEOUT, read(&mut stream)).await {
            Err(_) => {
                debug!(actor = %actor.describe(), "idle timeout");
                return Ok(());
            }
            // Clean end of stream.
            Ok(Ok(None)) => return Ok(()),
            Ok(Ok(Some(request))) => request,
            Ok(Err(ReadError::Frame(e))) => {
                // A malformed frame is where an attack would show up, so it is
                // logged and the connection is dropped rather than resynced --
                // there is no way to know where the next message starts.
                warn!(actor = %actor.describe(), error = %e, "malformed request");
                let response = Response::bad_request(e.to_string());
                let _ = write(&mut stream, &response).await;
                return Ok(());
            }
            Ok(Err(ReadError::Io(e))) => return Err(e),
        };

        let response = configd.handle(request, &actor).await;
        write(&mut stream, &response).await?;
    }
}

enum ReadError {
    Frame(frame::FrameError),
    Io(std::io::Error),
}

/// One request, or `None` at a clean end of stream.
async fn read(stream: &mut UnixStream) -> Result<Option<Request>, ReadError> {
    let mut header = [0u8; frame::HEADER];
    match stream.read_exact(&mut header).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(ReadError::Io(e)),
    }

    // The length is checked before the buffer is allocated, so an oversized
    // claim costs four bytes rather than what it asked for.
    let len = frame::body_len(header).map_err(ReadError::Frame)?;
    let mut body = vec![0u8; len];
    stream
        .read_exact(&mut body)
        .await
        .map_err(ReadError::Io)?;

    frame::decode(&body).map(Some).map_err(ReadError::Frame)
}

async fn write(stream: &mut UnixStream, response: &Response) -> Result<(), std::io::Error> {
    let frame = frame::encode(response).map_err(|e| {
        error!(error = %e, "could not encode a response");
        std::io::Error::other(e.to_string())
    })?;
    stream.write_all(&frame).await?;
    stream.flush().await
}
