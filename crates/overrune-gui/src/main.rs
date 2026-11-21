//! Rune window for overrune: live NVML metrics read directly, and clock/power
//! controls that exist only while the daemon is reachable.

use std::io::{self, BufReader};
use std::os::unix::net::UnixStream;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;

use overrune_core::caps::Capabilities;
use overrune_core::gpu::{Gpu, Sample};
use overrune_core::ipc::{DaemonStatus, Request, Response, read_msg, socket_path, write_msg};
use overrune_core::profile::ProfileSettings;
use rune::tok::{self, space, type_};
use rune::{
    AppSpec, Color, Cx, Drag, El, LineCap, Phase, Shape, TextStyle, Tx, Waker, canvas, col, el,
    row, text,
};

mod icons;

const HISTORY: usize = 120;
const DEBOUNCE_S: f64 = 0.4;
const MAX_BACKOFF_S: u64 = 15;
const IO_TIMEOUT: Duration = Duration::from_secs(5);

/// What the connection thread tells the window.
#[derive(Default)]
struct Link {
    connected: bool,
    status: Option<DaemonStatus>,
    message: String,
    /// Bumped on every change so the poll can tell cheaply.
    generation: u64,
}

struct State {
    gpu: Option<Gpu>,
    caps: Option<Capabilities>,
    sample: Option<Sample>,
    error: Option<String>,
    temps: Vec<f32>,
    powers: Vec<f32>,
    next_sample: f64,
    /// The values the user wants; a `None` field is left to the daemon.
    edits: ProfileSettings,
    pending_since: Option<f64>,
    dragging: bool,
    synced: bool,
    to_daemon: Sender<Request>,
    worker_rx: Option<Receiver<Request>>,
    link: Arc<Mutex<Link>>,
    seen_generation: u64,
    /// The tile currently expanded, if any.
    open: Option<usize>,
}

impl State {
    fn new() -> State {
        // Same device choice as the daemon, so readings and tuning hit one GPU.
        let gpu = match std::env::var("OVERRUNE_BUS_ID") {
            Ok(id) => Gpu::open_by_bus_id(&id),
            Err(_) => Gpu::open(),
        };
        let (caps, error) = match gpu.as_ref().map(Gpu::capabilities) {
            Ok(Ok(c)) => (Some(c), None),
            Ok(Err(e)) => (None, Some(e.to_string())),
            Err(e) => (None, Some(e.to_string())),
        };
        let (to_daemon, rx) = channel();
        State {
            gpu: gpu.ok(),
            caps,
            sample: None,
            error,
            temps: Vec::new(),
            powers: Vec::new(),
            next_sample: 0.0,
            edits: ProfileSettings::default(),
            pending_since: None,
            dragging: false,
            synced: false,
            to_daemon,
            worker_rx: Some(rx),
            link: Arc::default(),
            seen_generation: 0,
            open: None,
        }
    }

    fn connected(&self) -> bool {
        self.link.lock().unwrap().connected
    }
}

fn push_capped(v: &mut Vec<f32>, x: f32) {
    if v.len() == HISTORY {
        v.remove(0);
    }
    v.push(x);
}

// ---- daemon connection (worker thread) ----

fn exchange(
    w: &mut UnixStream,
    r: &mut BufReader<UnixStream>,
    req: &Request,
    link: &Mutex<Link>,
) -> io::Result<()> {
    write_msg(w, req)?;
    let reply: Response =
        read_msg(r)?.ok_or_else(|| io::Error::from(io::ErrorKind::UnexpectedEof))?;
    let mut l = link.lock().unwrap();
    match reply {
        Response::Ok => l.message = "Applied".into(),
        Response::Status(s) => l.status = Some(s),
        Response::Profiles { .. } => {}
        Response::Error { message, .. } => l.message = message,
    }
    l.generation += 1;
    Ok(())
}

/// One connected session; returns on any I/O error so the caller reconnects.
fn session(
    sock: UnixStream,
    rx: &Receiver<Request>,
    link: &Mutex<Link>,
    wake: &Waker,
    intended: &mut Option<Request>,
) -> io::Result<()> {
    let mut r = BufReader::new(sock.try_clone()?);
    let mut w = sock;
    {
        let mut l = link.lock().unwrap();
        l.connected = true;
        l.generation += 1;
    }
    wake();
    // Re-send only what the user currently wants, then learn what is enforced.
    if let Some(req) = intended.clone() {
        exchange(&mut w, &mut r, &req, link)?;
    }
    exchange(&mut w, &mut r, &Request::Status, link)?;
    wake();
    while let Ok(req) = rx.recv() {
        remember(req.clone(), intended);
        exchange(&mut w, &mut r, &req, link)?;
        exchange(&mut w, &mut r, &Request::Status, link)?;
        wake();
    }
    Ok(())
}

fn remember(req: Request, intended: &mut Option<Request>) {
    match req {
        Request::Apply(_) => *intended = Some(req),
        Request::Reset => *intended = None,
        _ => {}
    }
}

fn worker(rx: Receiver<Request>, link: Arc<Mutex<Link>>, wake: Waker) {
    let mut intended = None;
    let mut backoff = 1;
    loop {
        if let Ok(sock) = UnixStream::connect(socket_path()) {
            let _ = sock.set_read_timeout(Some(IO_TIMEOUT));
            let _ = sock.set_write_timeout(Some(IO_TIMEOUT));
            backoff = 1;
            let _ = session(sock, &rx, &link, &wake, &mut intended);
        }
        {
            let mut l = link.lock().unwrap();
            l.connected = false;
            l.status = None;
            l.generation += 1;
        }
        wake();
        match rx.recv_timeout(Duration::from_secs(backoff)) {
            Ok(req) => remember(req, &mut intended),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return,
        }
        backoff = (backoff * 2).min(MAX_BACKOFF_S);
    }
}

// ---- app hooks ----

fn setup(s: &mut State, wake: Waker) {
    let rx = s.worker_rx.take().expect("setup runs once");
    let link = s.link.clone();
    std::thread::spawn(move || worker(rx, link, wake));
}

fn poll(s: &mut State, now: f64, tx: &mut Tx) -> bool {
    let mut changed = false;
    if now >= s.next_sample {
        s.next_sample = now + 1.0;
        tx.wake_after(Duration::from_secs(1));
        if let Some(gpu) = &s.gpu {
            match gpu.sample() {
                Ok(sm) => {
                    push_capped(&mut s.temps, sm.temp_c as f32);
                    push_capped(&mut s.powers, sm.power_w);
                    s.sample = Some(sm);
                    s.error = None;
                }
                Err(e) => {
                    // Don't keep presenting the last reading as live.
                    s.sample = None;
                    s.error = Some(e.to_string());
                }
            }
            changed = true;
        }
    }
    let (generation, connected, enforcing) = {
        let l = s.link.lock().unwrap();
        (
            l.generation,
            l.connected,
            l.status.as_ref().map(|st| st.enforcing.clone()),
        )
    };
    if generation != s.seen_generation {
        s.seen_generation = generation;
        changed = true;
    }
    if !connected {
        s.synced = false;
    } else if s.pending_since.is_none()
        && let (false, Some(e)) = (s.synced, enforcing)
    {
        s.synced = true;
        s.edits = e;
    }
    if let Some(t) = s.pending_since {
        if s.dragging {
            // Still dragging: the release schedules the wake.
        } else if now - t >= DEBOUNCE_S {
            s.pending_since = None;
            let _ = s.to_daemon.send(Request::Apply(s.edits.clone()));
        } else {
            tx.wake_after(Duration::from_secs_f64(DEBOUNCE_S - (now - t) + 0.01));
        }
    }
    changed
}

// ---- controls ----

#[derive(Clone, Copy, PartialEq)]
enum Knob {
    Core,
    Mem,
    Power,
    Ceiling,
}

/// The bounds a slider moves within, from the probed capabilities.
fn range(caps: &Capabilities, k: Knob) -> (f32, f32) {
    let p = caps
        .pstates
        .iter()
        .find(|p| p.pstate == caps.offset_pstate());
    match k {
        Knob::Core => p.map_or((0.0, 0.0), |p| {
            (p.core_offset_min_mhz as f32, p.core_offset_max_mhz as f32)
        }),
        Knob::Mem => p.map_or((0.0, 0.0), |p| {
            (p.mem_offset_min_mhz as f32, p.mem_offset_max_mhz as f32)
        }),
        Knob::Power => (caps.power_limit_min_w, caps.power_limit_max_w),
        Knob::Ceiling => (
            caps.graphics_clocks_mhz.iter().copied().min().unwrap_or(0) as f32,
            caps.graphics_clocks_mhz.iter().copied().max().unwrap_or(0) as f32,
        ),
    }
}

fn current(s: &State, k: Knob) -> f32 {
    let (e, sm) = (&s.edits, s.sample.as_ref());
    match k {
        Knob::Core => e
            .core_offset_mhz
            .map(|v| v as f32)
            .or(sm.map(|m| m.core_offset_mhz as f32)),
        Knob::Mem => e
            .mem_offset_mhz
            .map(|v| v as f32)
            .or(sm.map(|m| m.mem_offset_mhz as f32)),
        Knob::Power => e.power_limit_w.or(sm.map(|m| m.power_limit_w)),
        Knob::Ceiling => e.lock_max_clock_mhz.map(|v| v as f32),
    }
    .unwrap_or_else(|| range(s.caps.as_ref().expect("controls need caps"), k).1)
}

/// Writes a slider position into the edits, bounded and (for the ceiling) snapped.
fn set_knob(edits: &mut ProfileSettings, caps: &Capabilities, k: Knob, v: f32) {
    let (lo, hi) = range(caps, k);
    let v = v.clamp(lo, hi);
    match k {
        Knob::Core => edits.core_offset_mhz = Some(v.round() as i32),
        Knob::Mem => edits.mem_offset_mhz = Some(v.round() as i32),
        Knob::Power => edits.power_limit_w = Some(v.round().clamp(lo, hi)),
        Knob::Ceiling => edits.lock_max_clock_mhz = Some(caps.snap_clock(v.round() as u32)),
    }
}

fn slider(
    t: tok::Theme,
    accent: Color,
    key: &'static str,
    label: &'static str,
    unit: &'static str,
    k: Knob,
    s: &State,
) -> El {
    let caps = s.caps.as_ref().expect("controls need caps");
    let (lo, hi) = range(caps, k);
    let v = current(s, k);
    let frac = if hi > lo {
        ((v - lo) / (hi - lo)).clamp(0.0, 1.0)
    } else {
        0.0
    };
    let (w, h, kn, bar) = (240.0, 22.0, 18.0, 4.0);
    let travel = w - kn;
    let step = (hi - lo) / 20.0;
    let caps_for_drag = caps.clone();
    let caps_for_step = caps.clone();
    let to_value = move |x: f32| lo + ((x - kn / 2.0) / travel).clamp(0.0, 1.0) * (hi - lo);
    row()
        .gap(space::S3)
        .center_y()
        .child(
            text(label)
                .font(type_::LABEL)
                .color(t.text_secondary)
                .w(110.0),
        )
        .child(
            el().key(key)
                .role("slider")
                .label(label)
                .range_value(f64::from(v), f64::from(lo), f64::from(hi))
                .range_step(f64::from(step))
                .a11y_value(format!("{v:.0} {unit}"))
                .on_step(move |s: &mut State, steps, tx| {
                    let cur = current(s, k);
                    let target = if k == Knob::Ceiling {
                        // Move by supported clocks, not a fixed MHz step that snaps back.
                        let mut c = caps_for_step.graphics_clocks_mhz.clone();
                        c.sort_unstable();
                        let at = c
                            .iter()
                            .position(|&m| m == caps_for_step.snap_clock(cur as u32));
                        at.map_or(cur, |i| {
                            c[(i as i64 + i64::from(steps)).clamp(0, c.len() as i64 - 1) as usize]
                                as f32
                        })
                    } else {
                        cur + steps as f32 * step
                    };
                    set_knob(&mut s.edits, &caps_for_step, k, target);
                    s.pending_since = Some(tx.clock());
                    tx.wake_after(Duration::from_secs_f64(DEBOUNCE_S + 0.05));
                })
                .size(w, h)
                .on_drag(move |s: &mut State, d: &mut Drag, tx| {
                    if d.phase != Phase::Up {
                        s.dragging = true;
                        set_knob(
                            &mut s.edits,
                            &caps_for_drag,
                            k,
                            to_value(d.x * w / d.w.max(1.0)),
                        );
                        s.pending_since = Some(tx.clock());
                        tx.instant();
                    } else {
                        s.dragging = false;
                        tx.wake_after(Duration::from_secs_f64(DEBOUNCE_S + 0.05));
                    }
                })
                .child(
                    el().size(w, bar)
                        .absolute(0.0, (h - bar) / 2.0)
                        .radius(bar / 2.0)
                        .fill(t.control_off)
                        .a11y_hidden(),
                )
                .child(
                    el().size(kn / 2.0 + frac * travel, bar)
                        .absolute(0.0, (h - bar) / 2.0)
                        .radius(bar / 2.0)
                        .fill(accent)
                        .a11y_hidden(),
                )
                .child(
                    el().size(kn, kn)
                        .absolute(frac * travel, (h - kn) / 2.0)
                        .radius(kn / 2.0)
                        .fill(t.knob)
                        .shadow(1.0, 3.0, t.knob_shadow)
                        .a11y_hidden(),
                ),
        )
        .child(
            text(format!("{v:.0} {unit}"))
                .key(format!("{key}-value"))
                .font(type_::MONO_CAPTION)
                .color(t.text)
                .w(80.0)
                .a11y_hidden(),
        )
}

fn reset_button(t: tok::Theme) -> El {
    el().key("reset")
        .role("button")
        .label("Reset to defaults")
        .focusable(true)
        .px(space::S3)
        .py(space::S2)
        .radius(tok::radius::SM)
        .fill(t.control_off)
        .on_press(|s: &mut State| {
            s.edits = ProfileSettings::default();
            s.pending_since = None;
            let _ = s.to_daemon.send(Request::Reset);
        })
        .child(text("Reset to defaults").font(type_::LABEL).color(t.text))
}

// ---- view ----

/// The Paper design's night-and-sky-blue paletdte.
struct Pal {
    ground: Color,
    glass: Color,
    edge: Color,
    grid: Color,
    ink: Color,
    muted: Color,
    accent: Color,
    focus: Color,
    profiles: Color,
    black: Color,
    filled: Color,
}

fn pal() -> Pal {
    let c = |h| rune::color::hex(h).expect("valid palette hex");
    Pal {
        ground: c("#0a0d14"),
        glass: c("#ffffff14"),
        edge: c("#ffffff1a"),
        grid: c("#5ab9fc0d"),
        ink: c("#e9edf3"),
        muted: c("#8891a1"),
        accent: c("#5ab9fc"),
        focus: c("#131824EE"),
        profiles: c("#232c40"),
        black: c("#000000"),
        filled: c("#ffffff0a"),
    }
}

const TILES: usize = 8;
const BIG: TextStyle = TextStyle::new(40.0, 500.0);

/// Icon pixel runs parsed once: each `M x y h w v h h -w z` becomes `[x, y, w, h]`.
fn icon_rects(i: usize) -> &'static [[f32; 4]] {
    static CACHE: LazyLock<Vec<Vec<[f32; 4]>>> = LazyLock::new(|| {
        icons::ALL
            .iter()
            .map(|d| {
                d.split('M')
                    .filter_map(|run| {
                        let n: Vec<f32> = run
                            .split(|c: char| !(c.is_ascii_digit() || c == '-' || c == '.'))
                            .filter_map(|t| t.parse().ok())
                            .collect();
                        // x y w h (the `h`/`v` numbers are width and height)
                        (n.len() >= 4).then(|| [n[0], n[1], n[2], n[3]])
                    })
                    .collect()
            })
            .collect()
    });
    &CACHE[i]
}

fn icon(i: usize, color: Color, size: f32) -> El {
    let mut shape = Shape::new();
    for [x, y, w, h] in icon_rects(i) {
        shape = shape.fill(
            vec![[*x, *y], [x + w, *y], [x + w, y + h], [*x, y + h]],
            color,
        );
    }
    canvas(shape.view_box(96.0, 96.0))
        .size(size, size)
        .a11y_hidden()
}

/// Circular gauge: a faint track and an accent arc for `pct` percent.
fn ring(p: &Pal, pct: f32) -> El {
    let (c, r) = (48.0_f32, 40.0_f32);
    let arc = |frac: f32| -> Vec<[f32; 2]> {
        let n = ((frac * 64.0).ceil() as usize).max(1);
        (0..=n)
            .map(|k| {
                let a = -std::f32::consts::FRAC_PI_2
                    + std::f32::consts::TAU * frac * k as f32 / n as f32;
                [c + r * a.cos(), c + r * a.sin()]
            })
            .collect()
    };
    let frac = (pct / 100.0).clamp(0.0, 1.0);
    let mut shape = Shape::new().stroke_with(arc(1.0), true, 6.0, LineCap::Butt, p.edge);
    if frac > 0.0 {
        shape = shape.stroke_with(arc(frac), false, 6.0, LineCap::Round, p.accent);
    }
    canvas(shape.view_box(96.0, 96.0))
        .size(96.0, 96.0)
        .a11y_hidden()
}

struct Face {
    label: &'static str,
    big: String,
    sub: String,
}

fn face(i: usize, s: &State, sm: &Sample) -> Face {
    let link = s.link.lock().unwrap();
    let enforcing = link.status.as_ref().map(|st| &st.enforcing);
    let ceiling = link
        .status
        .as_ref()
        .and_then(|st| st.curve_ceiling_mhz)
        .or(enforcing.and_then(|e| e.lock_max_clock_mhz));
    let (label, big, sub) = match i {
        0 => ("GPU TEMP", format!("{}°C", sm.temp_c), "now".to_string()),
        1 => (
            "POWER",
            format!("{:.0} W", sm.power_w),
            format!("/ {:.0} W", sm.power_limit_w),
        ),
        2 => (
            "CLOCKS",
            format!("{}", sm.core_clock_mhz),
            format!("MHz core · {} mem", sm.mem_clock_mhz),
        ),
        3 => (
            "UTILIZATION",
            format!("{}%", sm.util_gpu_pct),
            format!("GPU · {}% MEM", sm.util_mem_pct),
        ),
        4 => (
            "TEMP → CEILING",
            ceiling.map_or("—".into(), |c| c.to_string()),
            "MHz ceiling".to_string(),
        ),
        5 => (
            "TUNING",
            format!("{:+}", sm.core_offset_mhz),
            "core MHz".to_string(),
        ),
        6 => (
            "ENFORCED NOW",
            link.status
                .as_ref()
                .and_then(|st| st.active_profile.clone())
                .unwrap_or_else(|| "default".into()),
            format!(
                "P{} · {}",
                sm.pstate,
                if link.connected {
                    "daemon ok"
                } else {
                    "no daemon"
                }
            ),
        ),
        _ => (
            "PROFILES",
            link.status
                .as_ref()
                .and_then(|st| st.active_profile.clone())
                .unwrap_or_else(|| "none".into()),
            "active".to_string(),
        ),
    };
    Face { label, big, sub }
}

fn kv(p: &Pal, key: &'static str, label: &'static str, value: String) -> El {
    row()
        .key(key)
        .justify_between()
        .py(space::S2)
        .border_sides([0.0, 0.0, 1.0, 0.0], p.edge)
        .child(text(label).font(type_::SECTION).color(p.muted))
        .child(text(value).font(type_::BODY.mono()).color(p.ink))
}

fn line_graph(p: &Pal, key: &'static str, data: &[f32], max: f32) -> El {
    let (w, h) = (HISTORY as f32 * 4.0, 120.0);
    let pts: Vec<[f32; 2]> = data
        .iter()
        .enumerate()
        .map(|(i, x)| [i as f32 * 4.0, h - (x / max.max(1.0)).clamp(0.0, 1.0) * h])
        .collect();
    let mut shape = Shape::new();
    for k in 0..=4 {
        let y = h * k as f32 / 4.0;
        shape = shape.line([0.0, y], [w, y], 1.0, LineCap::Butt, p.edge);
    }
    if pts.len() > 1 {
        // Faded area: stacked bands, each clipped to the line, brighter near it.
        const BANDS: usize = 6;
        for b in 0..BANDS {
            let (top, bot) = (b as f32 / BANDS as f32, (b + 1) as f32 / BANDS as f32);
            let mut poly: Vec<[f32; 2]> = pts
                .iter()
                .map(|q| [q[0], q[1] + top * (h - q[1])])
                .collect();
            poly.extend(pts.iter().rev().map(|q| [q[0], q[1] + bot * (h - q[1])]));
            shape = shape.fill(poly, p.accent.alpha(0.22 * (1.0 - top)));
        }
        shape = shape.stroke_with(pts, false, 1.5, LineCap::Round, p.accent);
    }
    canvas(shape.view_box(w, h))
        .key(key)
        .size(w, h)
        .radius(tok::radius::SM)
}

/// The extra rows and widgets an expanded tile shows.
fn body(i: usize, p: &Pal, t: tok::Theme, s: &State, sm: &Sample) -> El {
    let peak = |v: &[f32]| v.iter().copied().fold(0.0, f32::max);
    let throttle = if sm.throttle_reasons.is_empty() {
        "none".to_string()
    } else {
        sm.throttle_reasons.join(", ")
    };
    let mut c = col().gap(space::S2);
    match i {
        0 => {
            c = c
                .child(line_graph(p, "graph-temp", &s.temps, 100.0))
                .child(kv(p, "x-now", "NOW", format!("{} °C", sm.temp_c)))
                .child(kv(p, "x-peak", "PEAK", format!("{:.0} °C", peak(&s.temps))))
                .child(kv(p, "x-throttle", "THROTTLE", throttle));
        }
        1 => {
            c = c
                .child(line_graph(p, "graph-power", &s.powers, sm.power_limit_w))
                .child(kv(p, "x-draw", "DRAW", format!("{:.0} W", sm.power_w)))
                .child(kv(
                    p,
                    "x-limit",
                    "ENFORCED LIMIT",
                    format!("{:.0} W", sm.power_limit_w),
                ))
                .child(kv(p, "x-peak", "PEAK", format!("{:.0} W", peak(&s.powers))))
                .child(kv(p, "x-throttle", "THROTTLE", throttle));
        }
        2 => {
            c = c
                .child(kv(
                    p,
                    "x-core",
                    "CORE",
                    format!("{} MHz", sm.core_clock_mhz),
                ))
                .child(kv(
                    p,
                    "x-mem",
                    "MEMORY",
                    format!("{} MHz", sm.mem_clock_mhz),
                ))
                .child(kv(
                    p,
                    "x-coff",
                    "CORE OFFSET",
                    format!("{:+} MHz", sm.core_offset_mhz),
                ))
                .child(kv(
                    p,
                    "x-moff",
                    "MEMORY OFFSET",
                    format!("{:+} MHz", sm.mem_offset_mhz),
                ))
                .child(kv(p, "x-pstate", "P-STATE", format!("P{}", sm.pstate)));
        }
        3 => {
            c = c
                .child(kv(p, "x-gpu", "GPU", format!("{}%", sm.util_gpu_pct)))
                .child(kv(
                    p,
                    "x-memu",
                    "MEMORY BUS",
                    format!("{}%", sm.util_mem_pct),
                ))
                .child(kv(
                    p,
                    "x-vram",
                    "VRAM",
                    format!("{} / {} MiB", sm.vram_used_mb, sm.vram_total_mb),
                ));
        }
        4 => {
            let link = s.link.lock().unwrap();
            let ceil = link.status.as_ref().and_then(|st| st.curve_ceiling_mhz);
            c = c
                .child(kv(p, "x-temp", "TEMPERATURE", format!("{} °C", sm.temp_c)))
                .child(kv(
                    p,
                    "x-ceil",
                    "CURVE CEILING",
                    ceil.map_or("no curve active".into(), |m| format!("{m} MHz")),
                ))
                .child(kv(
                    p,
                    "x-core",
                    "CORE CLOCK",
                    format!("{} MHz", sm.core_clock_mhz),
                ));
        }
        5 => {
            if s.caps.is_some() && s.connected() {
                let link_msg = s.link.lock().unwrap().message.clone();
                c = c
                    .child(slider(
                        t,
                        p.accent,
                        "core-offset",
                        "Core offset",
                        "MHz",
                        Knob::Core,
                        s,
                    ))
                    .child(slider(
                        t,
                        p.accent,
                        "mem-offset",
                        "Memory offset",
                        "MHz",
                        Knob::Mem,
                        s,
                    ))
                    .child(slider(
                        t,
                        p.accent,
                        "power-limit",
                        "Power limit",
                        "W",
                        Knob::Power,
                        s,
                    ))
                    .child(slider(
                        t,
                        p.accent,
                        "lock-ceiling",
                        "Clock ceiling",
                        "MHz",
                        Knob::Ceiling,
                        s,
                    ))
                    .child(
                        row()
                            .gap(space::S3)
                            .center_y()
                            .child(reset_button(t))
                            .child(
                                text(link_msg)
                                    .key("message")
                                    .font(type_::LABEL)
                                    .color(p.muted),
                            ),
                    );
            } else {
                c = c.child(
                    text("overrunerd is not reachable: showing read-only metrics. Reconnecting…")
                        .key("tuning-unavailable")
                        .font(type_::LABEL)
                        .color(p.muted),
                );
            }
        }
        _ => {
            let link = s.link.lock().unwrap();
            if let Some(st) = &link.status {
                let e = &st.enforcing;
                let show = |v: Option<String>| v.unwrap_or_else(|| "default".into());
                c = c
                    .child(kv(p, "x-prof", "PROFILE", show(st.active_profile.clone())))
                    .child(kv(
                        p,
                        "x-ecore",
                        "CORE OFFSET",
                        show(e.core_offset_mhz.map(|v| format!("{v:+} MHz"))),
                    ))
                    .child(kv(
                        p,
                        "x-emem",
                        "MEMORY OFFSET",
                        show(e.mem_offset_mhz.map(|v| format!("{v:+} MHz"))),
                    ))
                    .child(kv(
                        p,
                        "x-epow",
                        "POWER LIMIT",
                        show(e.power_limit_w.map(|v| format!("{v:.0} W"))),
                    ))
                    .child(kv(
                        p,
                        "x-elock",
                        "CLOCK CEILING",
                        show(e.lock_max_clock_mhz.map(|v| format!("{v} MHz"))),
                    ));
            } else {
                c = c.child(
                    text("No daemon: nothing is being enforced.")
                        .key("enforced-none")
                        .font(type_::LABEL)
                        .color(p.muted),
                );
            }
        }
    }
    c
}

fn tile(i: usize, mode: Mode, p: &Pal, t: tok::Theme, s: &State, sm: &Sample) -> El {
    let f = face(i, s, sm);
    let on_accent = mode != Mode::Open && i == 6;
    let (ink, sub_ink) = if on_accent {
        (p.black, p.black)
    } else {
        (p.ink, p.muted)
    };
    let icon_ink = if mode == Mode::Open { p.accent } else { ink };
    let id = format!("tile-{i}");
    let mut el_ = col()
        .key(id.clone())
        .matched(id)
        .role("button")
        .label(f.label)
        .focusable(true)
        .w(10.0)
        .grow(1.0)
        .padding(space::S3)
        .gap(space::S2)
        .radius(tok::radius::SM)
        .border(1.0, if mode == Mode::Open { p.accent } else { p.edge })
        .fill(match mode {
            Mode::Open => p.focus,
            _ if i == 6 => p.accent,
            _ if i == 7 => p.profiles,
            Mode::Rest => p.glass,
            Mode::Compact => p.filled,
        })
        .backdrop_blur(4.0)
        .on_press(move |s: &mut State| {
            s.open = if s.open == Some(i) { None } else { Some(i) };
        })
        .child(text(f.label).font(type_::SECTION).color(sub_ink));
    match mode {
        Mode::Rest | Mode::Compact => {
            let big = if mode == Mode::Rest {
                BIG
            } else {
                type_::TITLE
            };
            el_ = el_
                .child(el().grow(1.0))
                .child(icon(
                    i,
                    icon_ink,
                    if mode == Mode::Rest { 128.0 } else { 96.0 },
                ))
                .child(text(f.big).font(big.mono()).color(ink))
                .child(text(f.sub).font(type_::LABEL.mono()).color(sub_ink));
        }
        Mode::Open => {
            el_ = el_
                .grow(2.0)
                .padding(space::S6)
                .child(
                    row()
                        .gap(space::S3)
                        .center_y()
                        .child(if i == 3 {
                            ring(p, sm.util_gpu_pct as f32)
                        } else {
                            icon(i, icon_ink, 128.0)
                        })
                        .child(text(f.big).font(BIG.mono()).color(ink))
                        .child(text(f.sub).font(type_::LABEL.mono()).color(sub_ink)),
                )
                .child(body(i, p, t, s, sm));
        }
    }
    el_
}

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Rest,
    Compact,
    Open,
}

fn ticker(p: &Pal, s: &State, sm: &Sample) -> El {
    let item = |key: &'static str, label: &'static str, v: String| {
        col()
            .key(key)
            .child(text(label).font(type_::SECTION).color(p.muted))
            .child(text(v).font(type_::LABEL.mono()).color(p.ink))
    };
    let mut r = row()
        .gap(space::S6)
        .center_y()
        .padding(space::S3)
        .radius(tok::radius::SM)
        .border(1.0, p.edge)
        .fill(p.glass)
        .child(text("OVERRUNE").font(type_::TITLE.mono()).color(p.ink))
        .child(item("temp", "TEMP", format!("{}°C", sm.temp_c)))
        .child(item("core", "CORE", format!("{} MHz", sm.core_clock_mhz)))
        .child(item("mem", "MEM", format!("{} MHz", sm.mem_clock_mhz)))
        .child(item(
            "power",
            "POWER",
            format!("{:.0} / {:.0} W", sm.power_w, sm.power_limit_w),
        ))
        .child(item("util", "UTIL", format!("{}%", sm.util_gpu_pct)))
        .child(item("pstate", "PSTATE", format!("P{}", sm.pstate)))
        .child(item(
            "vram",
            "VRAM",
            format!("{} / {} MB", sm.vram_used_mb, sm.vram_total_mb),
        ));
    if let (Some(f), true) = (sm.fan_pct, s.caps.as_ref().is_some_and(|c| c.num_fans > 0)) {
        r = r.child(item("fan", "FAN", format!("{f}%")));
    }
    let status = if s.connected() {
        text("● daemon")
            .key("daemon")
            .font(type_::LABEL.mono())
            .color(p.accent)
    } else {
        text("● read-only")
            .key("read-only")
            .font(type_::LABEL.mono())
            .color(p.muted)
    };
    r.child(el().grow(1.0)).child(status)
}

fn grid_backdrop(p: &Pal) -> El {
    // ~190 strokes: built once per thread, not on every view run.
    thread_local! {
        static GRID: std::cell::OnceCell<El> = const { std::cell::OnceCell::new() };
    }
    GRID.with(|g| {
        g.get_or_init(|| {
            // Larger than any window we expect; the root clips what overflows.
            let (w, h) = (3840.0, 2160.0);
            let mut shape = Shape::new();
            let mut x = 0.0;
            while x <= w {
                shape = shape.stroke(vec![[x, 0.0], [x, h]], 1.0, p.grid);
                x += 32.0;
            }
            let mut y = 0.0;
            while y <= h {
                shape = shape.stroke(vec![[0.0, y], [w, y]], 1.0, p.grid);
                y += 32.0;
            }
            canvas(shape.view_box(w, h))
                .size(w, h)
                .absolute(0.0, 0.0)
                .a11y_hidden()
        })
        .clone()
    })
}

fn view(s: &State, cx: &mut Cx<State>) -> El {
    let t = tok::theme(true);
    let p = pal();
    let _ = cx;
    let root = col()
        .key("root")
        .padding(space::S3)
        .gap(space::S3)
        .fill(p.ground)
        .clip_rounded(0.0);

    let Some(sm) = &s.sample else {
        let msg = s.error.clone().unwrap_or_else(|| "Reading the GPU…".into());
        return root
            .child(text("overrune").font(type_::TITLE).color(p.ink))
            .child(text(msg).key("error").color(p.muted));
    };

    let stage = match s.open {
        None => {
            let rows = (0..2).map(|r| {
                (0..4).fold(row().gap(space::S2).grow(1.0), |row_, c| {
                    row_.child(tile(r * 4 + c, Mode::Rest, &p, t, s, sm))
                })
            });
            rows.fold(col().gap(space::S2).grow(1.0), |c, r| c.child(r))
        }
        Some(o) => {
            let others: Vec<usize> = (0..TILES).filter(|i| *i != o).collect();
            let side = others
                .chunks(2)
                .fold(col().gap(space::S2).w(10.0).grow(1.0), |c, pair| {
                    c.child(pair.iter().fold(row().gap(space::S2).grow(1.0), |r, i| {
                        r.child(tile(*i, Mode::Compact, &p, t, s, sm))
                    }))
                });
            col().gap(space::S2).grow(1.0).child(
                row()
                    .gap(space::S2)
                    .grow(1.0)
                    .child(tile(o, Mode::Open, &p, t, s, sm))
                    .child(side),
            )
        }
    };
    root.child(grid_backdrop(&p))
        .child(ticker(&p, s, sm))
        .child(stage)
}

fn main() {
    rune::run(AppSpec::<State> {
        setup: Some(setup),
        poll: Some(poll),
        ..AppSpec::new("overrune", (620.0, 640.0), State::new, view)
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use overrune_core::caps::PStateLimits;

    fn caps() -> Capabilities {
        Capabilities {
            bus_id: "0".into(),
            name: "t".into(),
            power_limit_min_w: 1.0,
            power_limit_max_w: 100.0,
            power_limit_default_w: 80.0,
            graphics_clocks_mhz: vec![300, 600, 900, 1200],
            mem_clock_mhz: 7000,
            num_fans: 0,
            pstates: vec![PStateLimits {
                pstate: 0,
                core_offset_min_mhz: -200,
                core_offset_max_mhz: 200,
                mem_offset_min_mhz: -500,
                mem_offset_max_mhz: 500,
            }],
        }
    }

    #[test]
    fn slider_values_are_bounded_and_the_ceiling_snaps() {
        let c = caps();
        let mut e = ProfileSettings::default();
        set_knob(&mut e, &c, Knob::Core, 900.0);
        set_knob(&mut e, &c, Knob::Power, 0.2);
        set_knob(&mut e, &c, Knob::Ceiling, 1000.0);
        assert_eq!(e.core_offset_mhz, Some(200));
        assert_eq!(e.power_limit_w, Some(1.0));
        assert_eq!(e.lock_max_clock_mhz, Some(900));
    }

    #[test]
    fn history_is_capped() {
        let mut v = Vec::new();
        for i in 0..HISTORY + 5 {
            push_capped(&mut v, i as f32);
        }
        assert_eq!(v, (5..HISTORY + 5).map(|i| i as f32).collect::<Vec<_>>());
    }

    fn sample() -> Sample {
        Sample {
            name: "Test GPU".into(),
            bus_id: "0".into(),
            driver_version: "1".into(),
            core_clock_mhz: 915,
            mem_clock_mhz: 7000,
            temp_c: 63,
            power_w: 42.8,
            power_limit_w: 80.0,
            util_gpu_pct: 75,
            util_mem_pct: 25,
            vram_used_mb: 256,
            vram_total_mb: 1024,
            pstate: 2,
            core_offset_mhz: 30,
            mem_offset_mhz: -45,
            fan_pct: None,
            throttle_reasons: vec![],
        }
    }

    fn test_state() -> (State, Receiver<Request>) {
        let (to_daemon, rx) = channel();
        let state = State {
            gpu: None,
            caps: Some(caps()),
            sample: Some(sample()),
            error: None,
            temps: vec![],
            powers: vec![],
            next_sample: 0.0,
            edits: ProfileSettings::default(),
            pending_since: None,
            dragging: false,
            synced: false,
            to_daemon,
            worker_rx: None,
            link: Arc::default(),
            seen_generation: 0,
            open: None,
        };
        (state, rx)
    }

    fn runtime(s: State) -> rune::Runtime<State> {
        let mut rt = rune::Runtime::new(s, view, (620.0, 640.0), rune::Env::new(true, 1.0));
        rt.set_system_reduced_motion(true);
        rt
    }

    fn click(rt: &mut rune::Runtime<State>, key: &str) {
        let (x, y) = rt.find(key).unwrap_or_else(|| panic!("missing {key}"));
        rt.pointer_down(x, y);
        rt.pointer_up(x, y);
        rt.frame(0.016);
    }

    fn daemon_status(core_offset_mhz: i32) -> DaemonStatus {
        DaemonStatus {
            active_profile: None,
            enforcing: ProfileSettings {
                core_offset_mhz: Some(core_offset_mhz),
                ..Default::default()
            },
            curve_ceiling_mhz: None,
        }
    }

    #[test]
    fn without_a_daemon_the_window_is_read_only() {
        let (mut s, _) = test_state();
        s.open = Some(5);
        let mut rt = runtime(s);
        rt.frame(0.0);
        assert!(rt.find("read-only").is_some());
        assert!(rt.find("tuning-unavailable").is_some());
        for key in [
            "core-offset",
            "mem-offset",
            "power-limit",
            "lock-ceiling",
            "reset",
        ] {
            assert!(rt.find(key).is_none(), "{key} should be hidden");
        }
    }

    #[test]
    fn connected_dashboard_reveals_controls_only_in_the_expanded_tuning_tile() {
        let (s, _) = test_state();
        s.link.lock().unwrap().connected = true;
        let mut rt = runtime(s);
        rt.frame(0.0);
        assert!(rt.find("daemon").is_some());
        assert!(rt.find("core-offset").is_none());

        click(&mut rt, "tile-5");
        for key in [
            "core-offset",
            "mem-offset",
            "power-limit",
            "lock-ceiling",
            "reset",
        ] {
            assert!(rt.find(key).is_some(), "{key} should be visible");
        }
        assert!(rt.find("tuning-unavailable").is_none());

        click(&mut rt, "tile-5");
        assert!(rt.find("core-offset").is_none());
        assert!(rt.find("reset").is_none());
    }

    #[test]
    fn reset_clears_edits_and_cancels_a_pending_apply() {
        let (mut s, rx) = test_state();
        s.link.lock().unwrap().connected = true;
        s.open = Some(5);
        s.edits = daemon_status(120).enforcing;
        s.pending_since = Some(0.0);
        let mut rt = runtime(s);
        rt.frame(0.0);

        click(&mut rt, "reset");
        assert_eq!(rt.state.edits, ProfileSettings::default());
        assert_eq!(rx.try_recv(), Ok(Request::Reset));
        poll(&mut rt.state, 1.0, &mut Tx::default());
        assert_eq!(rx.try_recv(), Err(std::sync::mpsc::TryRecvError::Empty));
    }

    #[test]
    fn metrics_wait_for_a_sample_and_history_is_shown_only_for_the_expanded_tile() {
        let (mut s, _) = test_state();
        s.sample = None;
        let mut rt = runtime(s);
        rt.frame(0.0);
        assert!(rt.find("error").is_some());
        assert!(rt.find("tile-0").is_none());

        let (mut s, _) = test_state();
        s.open = Some(0);
        s.temps = vec![60.0, 63.0];
        s.powers = vec![40.0, 42.8];
        let mut rt = runtime(s);
        rt.frame(0.0);
        assert!(rt.find("error").is_none());
        assert!(rt.find("graph-temp").is_some());
        assert!(rt.find("graph-power").is_none());
    }

    #[test]
    fn tile_icons_decode_to_visible_rectangles_within_the_design_bounds() {
        assert_eq!(icon_rects(0).first(), Some(&[36.0, 8.0, 24.0, 4.0]));
        for i in 0..TILES {
            let rects = icon_rects(i);
            assert!(!rects.is_empty(), "tile {i} has no icon");
            for &[x, y, w, h] in rects {
                assert!(
                    x >= 0.0 && y >= 0.0 && w > 0.0 && h > 0.0 && x + w <= 96.0 && y + h <= 96.0,
                    "tile {i} has an invalid rectangle: {:?}",
                    [x, y, w, h]
                );
            }
        }
    }

    #[test]
    fn dashboard_shows_fan_telemetry_only_when_supported_and_sampled() {
        for (num_fans, fan_pct, visible) in
            [(0, Some(45), false), (1, None, false), (1, Some(45), true)]
        {
            let (mut s, _) = test_state();
            s.caps.as_mut().unwrap().num_fans = num_fans;
            s.sample.as_mut().unwrap().fan_pct = fan_pct;
            let mut rt = runtime(s);
            rt.frame(0.0);
            assert_eq!(rt.find("fan").is_some(), visible, "{num_fans}, {fan_pct:?}");
        }
    }

    #[test]
    fn ceiling_tile_prefers_the_curve_then_falls_back_to_the_enforced_lock() {
        let (s, _) = test_state();
        let sm = s.sample.as_ref().unwrap();
        assert_eq!(face(4, &s, sm).big, "—");

        let mut status = daemon_status(0);
        status.enforcing.lock_max_clock_mhz = Some(1200);
        status.curve_ceiling_mhz = Some(900);
        s.link.lock().unwrap().status = Some(status);
        assert_eq!(face(4, &s, sm).big, "900");

        s.link
            .lock()
            .unwrap()
            .status
            .as_mut()
            .unwrap()
            .curve_ceiling_mhz = None;
        assert_eq!(face(4, &s, sm).big, "1200");
    }

    #[test]
    fn slider_uses_edits_then_telemetry_then_capability_bounds() {
        let (mut s, _) = test_state();
        assert_eq!(current(&s, Knob::Core), 30.0);
        assert_eq!(current(&s, Knob::Mem), -45.0);
        assert_eq!(current(&s, Knob::Power), 80.0);
        assert_eq!(current(&s, Knob::Ceiling), 1200.0);

        // An explicit zero is still a local edit.
        s.edits.core_offset_mhz = Some(0);
        assert_eq!(current(&s, Knob::Core), 0.0);
        s.edits = ProfileSettings::default();
        s.sample = None;
        assert_eq!(current(&s, Knob::Core), 200.0);
        assert_eq!(current(&s, Knob::Power), 100.0);
    }

    #[test]
    fn memory_edits_round_and_clamp_without_changing_other_knobs() {
        let c = caps();
        let mut e = daemon_status(30).enforcing;
        set_knob(&mut e, &c, Knob::Mem, -42.5);
        assert_eq!(e.mem_offset_mhz, Some(-43));
        set_knob(&mut e, &c, Knob::Mem, -900.0);
        assert_eq!(
            e,
            ProfileSettings {
                core_offset_mhz: Some(30),
                mem_offset_mhz: Some(-500),
                ..Default::default()
            }
        );
    }

    #[test]
    fn poll_applies_the_latest_edit_once_after_drag_and_debounce() {
        let (mut s, rx) = test_state();
        let mut tx = Tx::default();
        s.edits = daemon_status(15).enforcing;
        s.pending_since = Some(0.0);
        s.dragging = true;
        poll(&mut s, 1.0, &mut tx);
        assert_eq!(rx.try_recv(), Err(std::sync::mpsc::TryRecvError::Empty));

        s.edits = daemon_status(35).enforcing;
        s.pending_since = Some(1.0);
        s.dragging = false;
        poll(&mut s, 1.39, &mut tx);
        assert_eq!(rx.try_recv(), Err(std::sync::mpsc::TryRecvError::Empty));

        poll(&mut s, 1.41, &mut tx);
        assert_eq!(
            rx.try_recv(),
            Ok(Request::Apply(daemon_status(35).enforcing))
        );
        poll(&mut s, 2.0, &mut tx);
        assert_eq!(rx.try_recv(), Err(std::sync::mpsc::TryRecvError::Empty));
    }

    #[test]
    fn poll_syncs_once_per_connection_and_preserves_local_edits_between_status_updates() {
        let (mut s, _) = test_state();
        let mut tx = Tx::default();
        let link = s.link.clone();
        {
            let mut l = link.lock().unwrap();
            l.connected = true;
            l.generation += 1;
        }
        assert!(poll(&mut s, 0.0, &mut tx));
        assert!(!s.synced, "a connection alone does not supply settings");

        let publish = |connected, status| {
            let mut l = link.lock().unwrap();
            l.connected = connected;
            l.status = status;
            l.generation += 1;
        };
        publish(true, Some(daemon_status(20)));
        assert!(poll(&mut s, 0.1, &mut tx));
        assert_eq!(s.edits, daemon_status(20).enforcing);
        assert!(!poll(&mut s, 0.2, &mut tx));

        s.edits = daemon_status(45).enforcing;
        publish(true, Some(daemon_status(25)));
        assert!(poll(&mut s, 0.3, &mut tx));
        assert_eq!(s.edits, daemon_status(45).enforcing);

        publish(false, None);
        assert!(poll(&mut s, 0.4, &mut tx));
        publish(true, Some(daemon_status(60)));
        assert!(poll(&mut s, 0.5, &mut tx));
        assert_eq!(s.edits, daemon_status(60).enforcing);
    }

    #[test]
    fn session_replays_only_the_latest_intent_then_refreshes_status_after_reset() {
        let old = Request::Apply(daemon_status(10).enforcing);
        let latest = Request::Apply(daemon_status(20).enforcing);
        let mut intended = None;
        remember(old, &mut intended);
        remember(latest.clone(), &mut intended);
        remember(Request::Status, &mut intended);

        let (client, mut peer) = socket_pair();
        let status = daemon_status(0);
        // Queue the fake daemon replies in advance; no server thread or daemon is needed.
        for reply in [
            Response::Ok,
            Response::Status(daemon_status(20)),
            Response::Ok,
            Response::Status(status.clone()),
        ] {
            write_msg(&mut peer, &reply).unwrap();
        }
        let (tx, rx) = channel();
        tx.send(Request::Reset).unwrap();
        drop(tx);
        let link = Mutex::new(Link::default());
        let wake: Waker = Arc::new(|| {});

        session(client, &rx, &link, &wake, &mut intended).unwrap();

        let mut reader = BufReader::new(peer);
        let mut requests = Vec::new();
        while let Some(req) = read_msg::<_, Request>(&mut reader).unwrap() {
            requests.push(req);
        }
        assert_eq!(
            requests,
            [latest, Request::Status, Request::Reset, Request::Status]
        );
        assert_eq!(intended, None);
        let l = link.into_inner().unwrap();
        assert!(l.connected);
        assert_eq!(l.status, Some(status));
        assert_eq!(l.message, "Applied");
    }

    #[test]
    fn session_preserves_an_apply_when_the_daemon_disconnects_before_replying() {
        let (client, mut peer) = socket_pair();
        write_msg(&mut peer, &Response::Status(daemon_status(0))).unwrap();
        peer.shutdown(std::net::Shutdown::Write).unwrap();
        let (tx, rx) = channel();
        let apply = Request::Apply(daemon_status(70).enforcing);
        tx.send(apply.clone()).unwrap();
        drop(tx);
        let link = Mutex::new(Link::default());
        let wake: Waker = Arc::new(|| {});
        let mut intended = None;

        let err = session(client, &rx, &link, &wake, &mut intended).unwrap_err();

        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
        assert_eq!(intended, Some(apply));
        assert_eq!(link.into_inner().unwrap().status, Some(daemon_status(0)));
    }

    fn socket_pair() -> (UnixStream, UnixStream) {
        let (client, peer) = UnixStream::pair().unwrap();
        for stream in [&client, &peer] {
            stream
                .set_read_timeout(Some(Duration::from_secs(1)))
                .unwrap();
            stream
                .set_write_timeout(Some(Duration::from_secs(1)))
                .unwrap();
        }
        (client, peer)
    }
}
