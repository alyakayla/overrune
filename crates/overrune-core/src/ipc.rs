//! The CLI/GUI ↔ daemon protocol: JSON objects, one per line, over a Unix
//! stream socket. Line framing is safe because `serde_json` escapes every
//! newline inside a string.

use std::io::{self, BufRead, Write};
use std::path::PathBuf;

use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::profile::ProfileSettings;

pub const DEFAULT_SOCKET: &str = "/run/overrune/overrunerd.sock";

/// Variants carrying a bare `String` or `Vec` must be struct variants:
/// serde's internal tagging can only tag a map.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Request {
    Apply(ProfileSettings),
    Reset,
    Status,
    UseProfile {
        name: String,
    },
    SaveProfile {
        name: String,
        settings: ProfileSettings,
    },
    ListProfiles,
    RemoveProfile {
        name: String,
    },
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "reply", rename_all = "snake_case")]
pub enum Response {
    Ok,
    Status(DaemonStatus),
    Profiles { names: Vec<String> },
    Error { kind: ErrorKind, message: String },
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct DaemonStatus {
    pub active_profile: Option<String>,
    pub enforcing: ProfileSettings,
    pub curve_ceiling_mhz: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorKind {
    Unauthorized,
    OutOfRange,
    NotFound,
    Driver,
    Internal,
}

pub fn socket_path() -> PathBuf {
    std::env::var_os("OVERRUNE_SOCKET")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(DEFAULT_SOCKET))
}

pub fn write_msg<W: Write, T: Serialize>(w: &mut W, msg: &T) -> io::Result<()> {
    let mut line = serde_json::to_vec(msg)?;
    line.push(b'\n');
    w.write_all(&line)?;
    w.flush()
}

/// Reads one message. `Ok(None)` means the peer closed cleanly.
pub fn read_msg<R: BufRead, T: DeserializeOwned>(r: &mut R) -> io::Result<Option<T>> {
    let mut line = String::new();
    if r.read_line(&mut line)? == 0 {
        return Ok(None);
    }
    Ok(Some(serde_json::from_str(&line)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_request_round_trips_over_a_pipe() {
        let req = Request::Apply(ProfileSettings {
            core_offset_mhz: Some(150),
            power_limit_w: Some(75.0),
            ..Default::default()
        });
        let mut buf = Vec::new();
        write_msg(&mut buf, &req).unwrap();
        let mut r = io::BufReader::new(&buf[..]);
        assert_eq!(read_msg::<_, Request>(&mut r).unwrap(), Some(req));
    }

    #[test]
    fn two_messages_in_one_buffer_are_read_as_two() {
        let mut buf = Vec::new();
        write_msg(&mut buf, &Request::Status).unwrap();
        write_msg(&mut buf, &Request::Reset).unwrap();
        let mut r = io::BufReader::new(&buf[..]);
        assert_eq!(
            read_msg::<_, Request>(&mut r).unwrap(),
            Some(Request::Status)
        );
        assert_eq!(
            read_msg::<_, Request>(&mut r).unwrap(),
            Some(Request::Reset)
        );
        assert_eq!(read_msg::<_, Request>(&mut r).unwrap(), None);
    }

    #[test]
    fn a_message_containing_a_newline_in_a_string_stays_one_line() {
        let req = Request::UseProfile {
            name: "evil\nname".into(),
        };
        let mut buf = Vec::new();
        write_msg(&mut buf, &req).unwrap();
        assert_eq!(buf.iter().filter(|b| **b == b'\n').count(), 1);
        let mut r = io::BufReader::new(&buf[..]);
        assert_eq!(read_msg::<_, Request>(&mut r).unwrap(), Some(req));
    }

    #[test]
    fn clean_eof_yields_none_not_an_error() {
        let mut r = io::BufReader::new(&b""[..]);
        assert_eq!(read_msg::<_, Request>(&mut r).unwrap(), None);
    }

    #[test]
    fn a_malformed_line_is_an_error_not_a_silent_skip() {
        let mut r = io::BufReader::new(&b"{not json}\n"[..]);
        assert!(read_msg::<_, Request>(&mut r).is_err());
    }

    #[test]
    fn socket_path_honours_the_environment_override() {
        // SAFETY: single-threaded assertion on process-wide state; no other
        // test in this module reads the environment.
        unsafe { std::env::set_var("OVERRUNE_SOCKET", "/tmp/x.sock") };
        assert_eq!(socket_path(), PathBuf::from("/tmp/x.sock"));
        unsafe { std::env::remove_var("OVERRUNE_SOCKET") };
        assert_eq!(socket_path(), PathBuf::from(DEFAULT_SOCKET));
    }
}
