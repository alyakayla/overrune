//! Socket client for the write commands. Read commands never come here.

use std::fmt;
use std::io::{self, BufReader};
use std::os::unix::net::UnixStream;

use overrune_core::ipc::{Request, Response, read_msg, socket_path, write_msg};

#[derive(Debug)]
pub enum ClientError {
    /// No daemon on the socket (absent, or nobody listening).
    Unreachable(io::Error),
    /// Connected, but the conversation failed.
    Protocol(String),
}

impl fmt::Display for ClientError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ClientError::Unreachable(e) => write!(
                f,
                "cannot reach overrunerd at {}: {e} (is the daemon running? try `systemctl start overrunerd.socket`)",
                socket_path().display()
            ),
            ClientError::Protocol(e) => write!(f, "daemon conversation failed: {e}"),
        }
    }
}

impl std::error::Error for ClientError {}

pub fn send(req: Request) -> Result<Response, ClientError> {
    let stream = UnixStream::connect(socket_path()).map_err(ClientError::Unreachable)?;
    let mut writer = &stream;
    // A daemon may refuse on connect and hang up before reading; its reply is
    // still waiting for us, so a failed write is only reported if no reply is.
    let wrote = write_msg(&mut writer, &req);
    match read_msg(&mut BufReader::new(&stream)) {
        Ok(Some(resp)) => Ok(resp),
        Ok(None) => Err(ClientError::Protocol(match wrote {
            Err(e) => e.to_string(),
            Ok(()) => "daemon closed the connection without replying".into(),
        })),
        Err(e) => Err(ClientError::Protocol(e.to_string())),
    }
}
