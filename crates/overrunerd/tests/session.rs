use std::io::{BufRead, BufReader};
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use overrune_core::ipc::{ErrorKind, Request, Response, read_msg, write_msg};
use overrune_core::{Capabilities, GpuError};
use overrunerd::apply::Writes;
use overrunerd::session::serve;
use overrunerd::state::Daemon;

struct NoWrites;
impl Writes for NoWrites {
    fn set_power_limit_mw(&mut self, _: u32) -> Result<(), GpuError> {
        panic!("test must not write")
    }
    fn set_core_offset(&mut self, _: i32) -> Result<(), GpuError> {
        panic!("test must not write")
    }
    fn set_mem_offset(&mut self, _: i32) -> Result<(), GpuError> {
        panic!("test must not write")
    }
    fn set_locked_max_clock(&mut self, _: Option<u32>) -> Result<(), GpuError> {
        panic!("test must not write")
    }
}

fn caps() -> Capabilities {
    Capabilities {
        bus_id: "0000:01:00.0".into(),
        name: "Test".into(),
        power_limit_min_w: 50.0,
        power_limit_max_w: 80.0,
        power_limit_default_w: 80.0,
        graphics_clocks_mhz: vec![1980],
        mem_clock_mhz: 6001,
        num_fans: 0,
        pstates: vec![],
    }
}

/// Serves on a temp socket; the n-th connection (0-based) is authorized iff
/// `allow(n)`.
fn start(tag: &str, allow: fn(usize) -> bool) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("overrunerd-sess-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let sock = dir.join("s.sock");
    let listener = UnixListener::bind(&sock).unwrap();
    let daemon = Mutex::new(Daemon::with_paths(
        NoWrites,
        caps(),
        dir.join("p"),
        dir.join("active"),
    ));
    let n = Arc::new(AtomicUsize::new(0));
    std::thread::spawn(move || {
        serve(
            &listener,
            |_| allow(n.fetch_add(1, Ordering::SeqCst)),
            &daemon,
        )
    });
    sock
}

#[test]
fn an_unauthorized_connection_is_refused_on_connect_and_the_socket_stays_usable() {
    let sock = start("unauth", |n| n != 0);

    let mut denied = BufReader::new(UnixStream::connect(&sock).unwrap());
    // The request is never read; the refusal arrives on connect regardless.
    let _ = write_msg(denied.get_mut(), &Request::Reset);
    match read_msg::<_, Response>(&mut denied).unwrap() {
        Some(Response::Error { kind, .. }) => assert_eq!(kind, ErrorKind::Unauthorized),
        other => panic!("expected a refusal, got {other:?}"),
    }
    // Disconnected: clean EOF, or a reset because we left a request unread.
    assert!(!matches!(read_msg::<_, Response>(&mut denied), Ok(Some(_))));

    let mut ok = BufReader::new(UnixStream::connect(&sock).unwrap());
    write_msg(ok.get_mut(), &Request::ListProfiles).unwrap();
    assert!(matches!(
        read_msg::<_, Response>(&mut ok).unwrap(),
        Some(Response::Profiles { .. })
    ));
}

#[test]
fn a_request_on_an_authorized_connection_gets_exactly_one_response_line() {
    let sock = start("auth", |_| true);
    let mut c = BufReader::new(UnixStream::connect(&sock).unwrap());
    write_msg(c.get_mut(), &Request::Status).unwrap();
    let mut line = String::new();
    c.read_line(&mut line).unwrap();
    assert!(line.ends_with('\n') && line.matches('\n').count() == 1);
    assert!(matches!(
        serde_json::from_str::<Response>(&line).unwrap(),
        Response::Status(_)
    ));
    // A second request on the same connection gets its own single line.
    write_msg(c.get_mut(), &Request::ListProfiles).unwrap();
    assert!(matches!(
        read_msg::<_, Response>(&mut c).unwrap(),
        Some(Response::Profiles { .. })
    ));
}
