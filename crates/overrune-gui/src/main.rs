//! Rune window for overrune: live NVML metrics read directly, and clock/power
//! controls that exist only while the daemon is reachable.

use std::io::{self, BufReader};
use std::os::unix::net::UnixStream;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use overrune_core::caps::Capabilities;
use overrune_core::gpu::{Gpu, Sample};
use overrune_core::ipc::{DaemonStatus, Request, Response, read_msg, socket_path, write_msg};
use overrune_core::profile::ProfileSettings;
use rune::tok::{self, space, type_};
use rune::{
    AppSpec, Color, Cx, Drag, El, LineCap, Phase, Shape, Tx, Waker, canvas, col, el, row, text,
};

const HISTORY: usize = 120;
const DEBOUNCE_S: f64 = 0.4;
const MAX_BACKOFF_S: u64 = 15;

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
}

impl State {
    fn new() -> State {
        let gpu = Gpu::open();
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
                Err(e) => s.error = Some(e.to_string()),
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
    } else if let (false, Some(e)) = (s.synced, enforcing) {
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
        Knob::Power => edits.power_limit_w = Some(v.round()),
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
                    set_knob(&mut s.edits, &caps_for_step, k, cur + steps as f32 * step);
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

fn stat(t: tok::Theme, key: &'static str, label: &'static str, value: String) -> El {
    col()
        .key(key)
        .gap(2.0)
        .child(text(label).font(type_::SECTION).color(t.text_secondary))
        .child(text(value).font(type_::TITLE.mono()).color(t.text))
}

fn graph(t: tok::Theme, accent: Color, s: &State, limit_w: f32) -> El {
    let (w, h) = (HISTORY as f32 * 3.0, 100.0);
    let pts = |v: &[f32], max: f32| -> Vec<[f32; 2]> {
        v.iter()
            .enumerate()
            .map(|(i, x)| [i as f32 * 3.0, h - (x / max).clamp(0.0, 1.0) * h])
            .collect()
    };
    let mut shape = Shape::new();
    if s.temps.len() > 1 {
        shape = shape
            .stroke_with(pts(&s.temps, 100.0), false, 1.5, LineCap::Round, accent)
            .stroke_with(
                pts(&s.powers, limit_w.max(1.0)),
                false,
                1.5,
                LineCap::Round,
                Color {
                    r: 1.0,
                    g: 0.55,
                    b: 0.3,
                    a: 1.0,
                },
            );
    }
    col()
        .gap(space::S2)
        .child(
            row()
                .gap(space::S3)
                .child(
                    text("Temperature 0–100 °C")
                        .font(type_::LABEL)
                        .color(accent),
                )
                .child(
                    text(format!("Power 0–{limit_w:.0} W"))
                        .font(type_::LABEL)
                        .color(Color {
                            r: 1.0,
                            g: 0.55,
                            b: 0.3,
                            a: 1.0,
                        }),
                ),
        )
        .child(
            canvas(shape.view_box(w, h))
                .key("graph")
                .size(w, h)
                .fill(t.card)
                .radius(tok::radius::MD),
        )
}

fn view(s: &State, cx: &mut Cx<State>) -> El {
    let t = tok::theme(cx.system_dark());
    let accent = tok::accent(0, cx.system_dark());
    let mut root = col()
        .key("root")
        .padding(space::S6)
        .gap(space::S3)
        .fill(t.bg);

    let Some(sm) = &s.sample else {
        let msg = s.error.clone().unwrap_or_else(|| "Reading the GPU…".into());
        return root
            .child(text("overrune").font(type_::TITLE).color(t.text))
            .child(text(msg).key("error").color(t.text_secondary));
    };
    root = root
        .child(
            text(sm.name.clone())
                .key("name")
                .font(type_::TITLE)
                .color(t.text),
        )
        .child(
            text(format!("driver {} · {}", sm.driver_version, sm.bus_id))
                .font(type_::LABEL)
                .color(t.text_secondary),
        )
        .child(
            row()
                .gap(space::S6)
                .child(stat(t, "temp", "Temperature", format!("{} °C", sm.temp_c)))
                .child(stat(
                    t,
                    "core",
                    "Core clock",
                    format!("{} MHz", sm.core_clock_mhz),
                ))
                .child(stat(
                    t,
                    "mem",
                    "Memory clock",
                    format!("{} MHz", sm.mem_clock_mhz),
                ))
                .child(stat(
                    t,
                    "power",
                    "Power",
                    format!("{:.0} / {:.0} W", sm.power_w, sm.power_limit_w),
                )),
        )
        .child(
            row()
                .gap(space::S6)
                .child(stat(
                    t,
                    "util",
                    "Utilisation",
                    format!("{} %", sm.util_gpu_pct),
                ))
                .child(stat(
                    t,
                    "vram",
                    "VRAM",
                    format!("{} / {} MiB", sm.vram_used_mb, sm.vram_total_mb),
                ))
                .child(stat(
                    t,
                    "pstate",
                    "Performance state",
                    format!("P{}", sm.pstate),
                ))
                .child(
                    match (s.caps.as_ref().map_or(0, |c| c.num_fans), sm.fan_pct) {
                        (0, _) | (_, None) => el(),
                        (_, Some(f)) => stat(t, "fan", "Fan", format!("{f} %")),
                    },
                ),
        );
    if !sm.throttle_reasons.is_empty() {
        root = root.child(
            text(format!("Throttling: {}", sm.throttle_reasons.join(", ")))
                .key("throttle")
                .font(type_::LABEL)
                .color(t.text_secondary),
        );
    }
    root = root.child(graph(t, accent, s, sm.power_limit_w));

    // Controls exist only while the daemon answers, never as inert widgets.
    if s.caps.is_some() && s.connected() {
        let link = s.link.lock().unwrap();
        root = root
            .child(
                text("Controls")
                    .font(type_::SECTION)
                    .color(t.text_secondary),
            )
            .child(slider(
                t,
                accent,
                "core-offset",
                "Core offset",
                "MHz",
                Knob::Core,
                s,
            ))
            .child(slider(
                t,
                accent,
                "mem-offset",
                "Memory offset",
                "MHz",
                Knob::Mem,
                s,
            ))
            .child(slider(
                t,
                accent,
                "power-limit",
                "Power limit",
                "W",
                Knob::Power,
                s,
            ))
            .child(slider(
                t,
                accent,
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
                        text(link.message.clone())
                            .key("message")
                            .font(type_::LABEL)
                            .color(t.text_secondary),
                    ),
            );
    } else {
        root = root.child(
            text("overrunerd is not reachable: showing read-only metrics. Reconnecting…")
                .key("read-only")
                .font(type_::LABEL)
                .color(t.text_secondary),
        );
    }
    root
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
        assert_eq!((v.len(), v[0]), (HISTORY, 5.0));
    }

    #[test]
    fn without_a_daemon_the_window_is_read_only() {
        let mut s = State::new();
        s.sample = Some(Sample {
            name: "x".into(),
            bus_id: "0".into(),
            driver_version: "1".into(),
            core_clock_mhz: 1,
            mem_clock_mhz: 1,
            temp_c: 1,
            power_w: 1.0,
            power_limit_w: 80.0,
            util_gpu_pct: 0,
            util_mem_pct: 0,
            vram_used_mb: 0,
            vram_total_mb: 1,
            pstate: 0,
            core_offset_mhz: 0,
            mem_offset_mhz: 0,
            fan_pct: None,
            throttle_reasons: vec![],
        });
        s.caps = Some(caps());
        let mut rt = rune::Runtime::new(s, view, (620.0, 640.0), rune::Env::new(true, 1.0));
        rt.frame(0.0);
        assert!(rt.find("read-only").is_some());
        assert!(rt.find("core-offset").is_none());
        assert!(rt.find("fan").is_none());
    }
}
