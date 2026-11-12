use std::io;
use std::os::fd::FromRawFd;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixListener;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use overrune_core::Gpu;
use overrune_core::ipc::socket_path;
use overrunerd::apply::NvmlWriter;
use overrunerd::session;
use overrunerd::state::Daemon;

/// Curve/drift poll interval; well inside any sane dwell time.
const TICK: Duration = Duration::from_millis(500);

/// Adopts the socket systemd passes as fd 3 (`LISTEN_FDS=1`, `LISTEN_PID`
/// ours); otherwise binds `socket_path()` so the daemon can be run by hand.
fn listener() -> io::Result<UnixListener> {
    let ours = std::env::var("LISTEN_PID").is_ok_and(|p| p == std::process::id().to_string());
    if ours && std::env::var("LISTEN_FDS").is_ok_and(|n| n == "1") {
        // SAFETY: systemd guarantees fd 3 is an open listening socket for us
        // when LISTEN_PID matches and LISTEN_FDS is 1; nothing else owns it.
        return Ok(unsafe { UnixListener::from_raw_fd(3) });
    }
    let path = socket_path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let _ = std::fs::remove_file(&path);
    let l = UnixListener::bind(&path)?;
    // Hand-run only: any local user may connect, polkit is still the gate.
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o666))?;
    Ok(l)
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let gpu = match std::env::var("OVERRUNE_BUS_ID") {
        Ok(id) => Gpu::open_by_bus_id(&id)?,
        Err(_) => Gpu::open()?,
    };
    let caps = gpu.capabilities()?;
    let mut daemon = Daemon::new(NvmlWriter::open(&caps)?, caps);

    // `overrune-reapply` is a symlink to this binary (see dist/install.sh).
    let mut args = std::env::args();
    let as_reapply = args.next().is_some_and(|a| a.ends_with("overrune-reapply"));
    if as_reapply || args.any(|a| a == "--reapply-once") {
        daemon.load_active()?;
        return Ok(());
    }
    if let Err(e) = daemon.load_active() {
        eprintln!("overrunerd: could not apply the active profile: {e}");
    }

    let listener = listener()?;
    let daemon = Arc::new(Mutex::new(daemon));
    let ticking = Arc::clone(&daemon);
    let start = Instant::now();
    std::thread::spawn(move || {
        loop {
            std::thread::sleep(TICK);
            let Ok(sample) = gpu.sample() else { continue };
            let now_ms = start.elapsed().as_millis() as u64;
            if let Err(e) = ticking.lock().unwrap().tick(&sample, now_ms) {
                eprintln!("overrunerd: tick failed: {e}");
            }
        }
    });
    session::serve(&listener, session::authorized, &daemon);
    Ok(())
}

fn main() {
    if let Err(e) = run() {
        eprintln!("overrunerd: {e}");
        std::process::exit(1);
    }
}
