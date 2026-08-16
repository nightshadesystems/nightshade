//! Talking to configd.
//!
//! One connection, held open for the life of the session. Requests go out and
//! responses come back in order; there is no pipelining and no state on this
//! side beyond the session id.
//!
//! Errors from configd are passed through untouched. The CLI does not
//! paraphrase them, does not add a prefix, and does not decide that some of
//! them mean something else -- configd is the only side that knows the schema,
//! the running config and who asked, and two wordings of one error is one
//! wording too many.

use std::os::unix::net::UnixStream;
use std::path::Path;

use nightshade_proto::frame;
use nightshade_proto::message::{Request, Response};

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error(
        "cannot reach the configuration daemon at {path}: {source}\n\
         is nightshade-configd running?"
    )]
    Connect {
        path: String,
        #[source]
        source: std::io::Error,
    },

    #[error("the connection to the configuration daemon failed: {0}")]
    Transport(#[from] frame::FrameError),
}

pub struct Client {
    stream: UnixStream,
}

impl Client {
    pub fn connect(socket: &Path) -> Result<Self, ClientError> {
        let stream = UnixStream::connect(socket).map_err(|source| ClientError::Connect {
            path: socket.display().to_string(),
            source,
        })?;
        Ok(Self { stream })
    }

    pub fn call(&mut self, request: Request) -> Result<Response, ClientError> {
        frame::write_blocking(&mut self.stream, &request)?;
        Ok(frame::read_blocking(&mut self.stream)?)
    }
}
