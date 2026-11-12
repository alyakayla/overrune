//! Connection handling. Authorization happens once, on connect, before a
//! single byte of the peer's input is read.

use std::io::{self, BufReader, Read};
use std::os::fd::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::Duration;

use overrune_core::ipc::{ErrorKind, Request, Response, read_msg, write_msg};

use crate::apply::Writes;
use crate::state::Daemon;

pub const ACTION_ID: &str = "com.github.overrune.manage";

/// A silent or stalled peer must not wedge the serial accept loop.
const IO_TIMEOUT: Duration = Duration::from_secs(10);
/// Upper bound on bytes read from one connection; requests are tiny.
const MAX_CONNECTION_BYTES: u64 = 1 << 20;

/// root is authorized outright; anyone else needs polkit to say yes for the
/// peer's process (pid + start time, so a recycled pid cannot inherit it).
pub fn authorized(stream: &UnixStream) -> bool {
    let Some((pid, uid)) = peer_cred(stream) else {
        return false;
    };
    if uid == 0 {
        return true;
    }
    let Some(start) = start_time(pid) else {
        return false;
    };
    Command::new("pkcheck")
        .args([
            "--action-id",
            ACTION_ID,
            "--allow-user-interaction",
            "--process",
        ])
        .arg(format!("{pid},{start},{uid}"))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// `SO_PEERCRED`: the pid and uid the kernel recorded at connect time.
/// (`UnixStream::peer_cred` is still unstable in std.)
fn peer_cred(stream: &UnixStream) -> Option<(i32, u32)> {
    let mut cred = libc::ucred {
        pid: 0,
        uid: 0,
        gid: 0,
    };
    let mut len = size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: `cred` and `len` are valid for writes and `len` is its size.
    let rc = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&raw mut cred).cast(),
            &mut len,
        )
    };
    (rc == 0 && cred.pid > 0).then_some((cred.pid, cred.uid))
}

/// Field 22 of `/proc/<pid>/stat`. The command name (field 2) may contain
/// spaces and parentheses, so parse from the last `)`.
fn start_time(pid: i32) -> Option<u64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let rest = &stat[stat.rfind(')')? + 1..];
    rest.split_whitespace().nth(19)?.parse().ok()
}

/// Serves connections one at a time, forever.
pub fn serve<W: Writes>(
    listener: &UnixListener,
    authorize: impl Fn(&UnixStream) -> bool,
    daemon: &Mutex<Daemon<W>>,
) {
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        let ok = authorize(&stream);
        if let Err(e) = serve_connection(stream, ok, daemon) {
            eprintln!("overrunerd: connection ended: {e}");
        }
    }
}

/// An unauthorized peer gets one refusal line and is disconnected. An
/// authorized one gets exactly one response line per request line.
pub fn serve_connection<W: Writes>(
    stream: UnixStream,
    authorized: bool,
    daemon: &Mutex<Daemon<W>>,
) -> io::Result<()> {
    stream.set_read_timeout(Some(IO_TIMEOUT))?;
    stream.set_write_timeout(Some(IO_TIMEOUT))?;
    let mut out = &stream;
    if !authorized {
        return write_msg(
            &mut out,
            &Response::Error {
                kind: ErrorKind::Unauthorized,
                message: format!("not authorized for {ACTION_ID}"),
            },
        );
    }
    let mut reader = BufReader::new((&stream).take(MAX_CONNECTION_BYTES));
    loop {
        let resp = match read_msg::<_, Request>(&mut reader) {
            Ok(Some(req)) => daemon.lock().unwrap().handle(req),
            Ok(None) => return Ok(()),
            Err(e) if e.kind() == io::ErrorKind::InvalidData => {
                let bad = Response::Error {
                    kind: ErrorKind::Internal,
                    message: format!("malformed request: {e}"),
                };
                return write_msg(&mut out, &bad);
            }
            Err(e) => return Err(e),
        };
        write_msg(&mut out, &resp)?;
    }
}
