//! Every exit code of the spec's table, driven through the real binary
//! against a stub daemon on a temporary Unix socket.

use std::io::BufReader;
use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::process::{Command, Output};
use std::thread::JoinHandle;

use overrune_core::Gpu;
use overrune_core::ipc::{ErrorKind, Request, Response, read_msg, write_msg};
use serde_json::Value;

fn sock(tag: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!("overrune-test-{}-{tag}.sock", std::process::id()));
    let _ = std::fs::remove_file(&p);
    p
}

/// Serves exactly one connection: records the request, sends `reply`.
fn stub(path: &PathBuf, reply: Response) -> JoinHandle<Request> {
    let listener = UnixListener::bind(path).unwrap();
    std::thread::spawn(move || {
        let (mut conn, _) = listener.accept().unwrap();
        let req = read_msg::<_, Request>(&mut BufReader::new(&conn))
            .unwrap()
            .unwrap();
        write_msg(&mut conn, &reply).unwrap();
        req
    })
}

fn run(socket: &PathBuf, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_overrune"))
        .args(args)
        .env("OVERRUNE_SOCKET", socket)
        .output()
        .unwrap()
}

fn err(kind: ErrorKind, message: &str) -> Response {
    Response::Error {
        kind,
        message: message.into(),
    }
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

/// `set` and `status` probe real hardware before they do anything else, so
/// those tests only mean something where a device exists. CI runners have none.
fn no_gpu() -> bool {
    Gpu::open().is_err()
}

#[test]
fn success_exits_0_and_sends_one_apply_with_every_flag() {
    if no_gpu() {
        return;
    }
    let path = sock("ok");
    let h = stub(&path, Response::Ok);
    let out = run(&path, &["set", "--core-offset", "0", "--mem-offset", "0"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    match h.join().unwrap() {
        Request::Apply(s) => {
            assert_eq!(s.core_offset_mhz, Some(0));
            assert_eq!(s.mem_offset_mhz, Some(0));
        }
        other => panic!("expected one Apply, got {other:?}"),
    }
}

#[test]
fn rejected_power_limit_exits_6_and_names_the_real_range() {
    if no_gpu() {
        return;
    }
    let caps = Gpu::open().unwrap().capabilities().unwrap();
    let bad = caps.power_limit_max_w + 400.0;
    let expected = caps.validate_power_limit_w(bad).unwrap_err().to_string();
    // No socket at all: exit 6 (not 4) proves validation precedes the connect.
    let out = run(
        &PathBuf::from("/nonexistent/o.sock"),
        &["set", "--power-limit", &bad.to_string()],
    );
    assert_eq!(out.status.code(), Some(6));
    assert!(stderr(&out).contains(&expected), "{}", stderr(&out));
}

#[test]
fn daemon_side_out_of_range_and_driver_rejection_exit_6() {
    if no_gpu() {
        return;
    }
    for kind in [ErrorKind::OutOfRange, ErrorKind::Driver] {
        let path = sock("six");
        let h = stub(&path, err(kind, "nope"));
        let out = run(&path, &["set", "--core-offset", "0"]);
        h.join().unwrap();
        assert_eq!(out.status.code(), Some(6));
        assert!(stderr(&out).contains("nope"));
    }
}

#[test]
fn unauthorized_response_exits_5() {
    let path = sock("auth");
    let h = stub(&path, err(ErrorKind::Unauthorized, "polkit refused"));
    let out = run(&path, &["reset"]);
    h.join().unwrap();
    assert_eq!(out.status.code(), Some(5));
}

#[test]
fn absent_socket_exits_4_for_set_but_0_for_status() {
    if no_gpu() {
        return;
    }
    let missing = PathBuf::from("/nonexistent/o.sock");
    assert_eq!(
        run(&missing, &["set", "--core-offset", "0"]).status.code(),
        Some(4)
    );
    assert_eq!(run(&missing, &["reset"]).status.code(), Some(4));
    assert_eq!(run(&missing, &["daemon", "status"]).status.code(), Some(4));
    assert_eq!(run(&missing, &["status"]).status.code(), Some(0));
}

#[test]
fn bad_arguments_exit_2() {
    let p = PathBuf::from("/nonexistent/o.sock");
    assert_eq!(run(&p, &["frobnicate"]).status.code(), Some(2));
    assert_eq!(run(&p, &["set"]).status.code(), Some(2));
    assert_eq!(
        run(&p, &["set", "--power-limit", "lots"]).status.code(),
        Some(2)
    );
}

#[test]
fn no_such_device_exits_3() {
    let p = PathBuf::from("/nonexistent/o.sock");
    assert_eq!(
        run(&p, &["--device", "ffff:ff:ff.0", "status"])
            .status
            .code(),
        Some(3)
    );
}

#[test]
fn general_failures_exit_1() {
    let path = sock("one");
    let h = stub(&path, err(ErrorKind::NotFound, "no such profile"));
    let out = run(&path, &["profile", "rm", "ghost"]);
    h.join().unwrap();
    assert_eq!(out.status.code(), Some(1));

    // Daemon replies with the wrong shape.
    let path = sock("shape");
    let h = stub(&path, Response::Ok);
    let out = run(&path, &["profile", "list"]);
    h.join().unwrap();
    assert_eq!(out.status.code(), Some(1));
}

#[test]
fn json_mode_errors_emit_one_json_object_on_stdout_with_an_error_field() {
    let path = sock("json");
    let h = stub(&path, err(ErrorKind::Unauthorized, "polkit refused"));
    let out = run(&path, &["--json", "reset"]);
    h.join().unwrap();
    assert_eq!(out.status.code(), Some(5));
    let text = String::from_utf8(out.stdout).unwrap();
    assert_eq!(text.lines().count(), 1);
    let v: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(v["error"], "polkit refused");
    assert_eq!(v["kind"], "unauthorized");

    let out = run(&PathBuf::from("/nonexistent/o.sock"), &["--json", "reset"]);
    assert_eq!(out.status.code(), Some(4));
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["kind"], "unreachable");
    assert!(v["error"].is_string());
}

#[test]
fn profile_commands_reach_the_daemon_and_print_its_answer() {
    let path = sock("list");
    let h = stub(
        &path,
        Response::Profiles {
            names: vec!["quiet".into(), "undervolt".into()],
        },
    );
    let out = run(&path, &["profile", "list"]);
    assert_eq!(h.join().unwrap(), Request::ListProfiles);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "quiet\nundervolt\n");
}
