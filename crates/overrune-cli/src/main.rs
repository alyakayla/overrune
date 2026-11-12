use std::process::ExitCode;

use clap::{Args, Parser, Subcommand};
use overrune_cli::{client, status, watch};
use overrune_core::ipc::{DaemonStatus, ErrorKind, Request, Response};
use overrune_core::{Gpu, GpuError, ProfileSettings};

/// Monitor and tune an NVIDIA GPU without X.
///
/// Exit codes: 0 ok, 1 failure, 2 usage, 3 no NVIDIA device, 4 daemon
/// unreachable, 5 not authorized, 6 value out of range or rejected by the driver.
#[derive(Parser)]
#[command(version)]
struct Cli {
    /// Print machine-readable JSON (one object per line) instead of text.
    #[arg(long, global = true)]
    json: bool,
    /// Select the GPU by PCI bus id, e.g. 0000:01:00.0.
    #[arg(long, global = true, value_name = "BUS_ID")]
    device: Option<String>,
    #[command(subcommand)]
    command: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Show the GPU's current state (the default).
    Status,
    /// Print one JSON object and exit; same as `status --json`.
    Json,
    /// Live view, refreshed every second.
    Watch,
    /// Apply settings (needs the daemon and authorization).
    Set(Settings),
    /// Clear offsets, restore the default power limit, unlock clocks.
    Reset,
    /// Manage saved profiles.
    #[command(subcommand)]
    Profile(ProfileCmd),
    /// Talk to the daemon.
    #[command(subcommand)]
    Daemon(DaemonCmd),
}

#[derive(Args, Clone)]
#[group(required = true, multiple = true)]
struct Settings {
    /// Core clock offset in MHz (may be negative).
    #[arg(long, allow_hyphen_values = true, value_name = "MHZ")]
    core_offset: Option<i32>,
    /// Memory clock offset in MHz (may be negative).
    #[arg(long, allow_hyphen_values = true, value_name = "MHZ")]
    mem_offset: Option<i32>,
    /// Power limit in watts.
    #[arg(long, value_name = "WATTS")]
    power_limit: Option<f32>,
    /// Locked maximum graphics clock in MHz, snapped to a supported step.
    #[arg(long, value_name = "MHZ")]
    lock_clock: Option<u32>,
}

#[derive(Subcommand)]
enum ProfileCmd {
    /// List saved profiles.
    List,
    /// Save the given settings as a named profile.
    Save {
        name: String,
        #[command(flatten)]
        settings: Settings,
    },
    /// Make a saved profile the active one.
    Use { name: String },
    /// Delete a saved profile.
    Rm { name: String },
}

#[derive(Subcommand)]
enum DaemonCmd {
    /// What the daemon is currently enforcing.
    Status,
}

enum CliError {
    Gpu(GpuError),
    /// A value outside the driver's range, caught before any socket is opened.
    Range(String),
    Client(client::ClientError),
    Daemon {
        kind: ErrorKind,
        message: String,
    },
}

impl CliError {
    fn kind(&self) -> &'static str {
        match self {
            CliError::Gpu(GpuError::Driver(_)) => "driver",
            CliError::Gpu(_) => "no_device",
            CliError::Range(_) => "out_of_range",
            CliError::Client(client::ClientError::Unreachable(_)) => "unreachable",
            CliError::Client(_) => "protocol",
            CliError::Daemon { kind, .. } => match kind {
                ErrorKind::Unauthorized => "unauthorized",
                ErrorKind::OutOfRange => "out_of_range",
                ErrorKind::NotFound => "not_found",
                ErrorKind::Driver => "driver",
                ErrorKind::Internal => "internal",
            },
        }
    }

    fn message(&self) -> String {
        match self {
            CliError::Gpu(e) => e.to_string(),
            CliError::Range(m) => m.clone(),
            CliError::Client(e) => e.to_string(),
            CliError::Daemon { message, .. } => message.clone(),
        }
    }
}

/// The exit-code table of the spec (§5.5); usage errors (2) come from clap.
fn exit_code(e: &CliError) -> i32 {
    match e {
        CliError::Gpu(GpuError::NvmlUnavailable(_) | GpuError::NoDevice) => 3,
        CliError::Gpu(GpuError::Driver(_)) => 1,
        CliError::Client(client::ClientError::Unreachable(_)) => 4,
        CliError::Client(client::ClientError::Protocol(_)) => 1,
        CliError::Range(_) => 6,
        CliError::Daemon { kind, .. } => match kind {
            ErrorKind::Unauthorized => 5,
            ErrorKind::OutOfRange | ErrorKind::Driver => 6,
            ErrorKind::NotFound | ErrorKind::Internal => 1,
        },
    }
}

fn open(device: &Option<String>) -> Result<Gpu, CliError> {
    match device {
        Some(id) => Gpu::open_by_bus_id(id),
        None => Gpu::open(),
    }
    .map_err(CliError::Gpu)
}

/// Validates against the probed capabilities, so an out-of-range value never
/// reaches the daemon. The daemon re-validates regardless.
fn checked(s: Settings, device: &Option<String>) -> Result<ProfileSettings, CliError> {
    let gpu = open(device)?;
    let caps = gpu.capabilities().map_err(CliError::Gpu)?;
    let settings = ProfileSettings {
        core_offset_mhz: s.core_offset,
        mem_offset_mhz: s.mem_offset,
        power_limit_w: s.power_limit,
        lock_max_clock_mhz: s.lock_clock.map(|m| caps.snap_clock(m)),
        curve: None,
    };
    settings
        .validate(&caps)
        .map_err(|e| CliError::Range(e.to_string()))?;
    Ok(settings)
}

fn ask(req: Request) -> Result<Response, CliError> {
    match client::send(req).map_err(CliError::Client)? {
        Response::Error { kind, message } => Err(CliError::Daemon { kind, message }),
        ok => Ok(ok),
    }
}

fn unexpected(r: Response) -> CliError {
    CliError::Client(client::ClientError::Protocol(format!(
        "unexpected reply from daemon: {r:?}"
    )))
}

/// Success output: `{"ok":true}` under `--json`, a short line otherwise.
fn done(json: bool, msg: &str) {
    if json {
        println!("{}", serde_json::json!({ "ok": true }));
    } else {
        println!("{msg}");
    }
}

fn run(cli: Cli) -> Result<(), CliError> {
    let dev = &cli.device;
    let json = cli.json || matches!(cli.command, Some(Cmd::Json));
    match cli.command.unwrap_or(Cmd::Status) {
        Cmd::Status | Cmd::Json => {
            let s = open(dev)?.sample().map_err(CliError::Gpu)?;
            println!(
                "{}",
                if json {
                    status::render_json(&s)
                } else {
                    status::render_human(&s)
                }
                .trim_end()
            );
        }
        Cmd::Watch => watch::run(&open(dev)?, json).map_err(CliError::Gpu)?,
        Cmd::Set(s) => {
            let settings = checked(s, dev)?;
            ask(Request::Apply(settings))?;
            done(json, "applied");
        }
        Cmd::Reset => {
            ask(Request::Reset)?;
            done(json, "reset to defaults");
        }
        Cmd::Profile(p) => match p {
            ProfileCmd::List => match ask(Request::ListProfiles)? {
                Response::Profiles { names } if json => {
                    println!("{}", serde_json::json!({ "profiles": names }))
                }
                Response::Profiles { names } => names.iter().for_each(|n| println!("{n}")),
                other => return Err(unexpected(other)),
            },
            ProfileCmd::Save { name, settings } => {
                let settings = checked(settings, dev)?;
                ask(Request::SaveProfile {
                    name: name.clone(),
                    settings,
                })?;
                done(json, &format!("saved profile {name}"));
            }
            ProfileCmd::Use { name } => {
                ask(Request::UseProfile { name: name.clone() })?;
                done(json, &format!("active profile: {name}"));
            }
            ProfileCmd::Rm { name } => {
                ask(Request::RemoveProfile { name: name.clone() })?;
                done(json, &format!("removed profile {name}"));
            }
        },
        Cmd::Daemon(DaemonCmd::Status) => match ask(Request::Status)? {
            Response::Status(st) if json => {
                println!("{}", serde_json::to_string(&st).expect("serializes"))
            }
            Response::Status(st) => print_daemon(&st),
            other => return Err(unexpected(other)),
        },
    }
    Ok(())
}

fn print_daemon(st: &DaemonStatus) {
    let e = &st.enforcing;
    let show = |v: Option<String>| v.unwrap_or_else(|| "default".into());
    println!(
        "  Profile  {}",
        st.active_profile.as_deref().unwrap_or("none")
    );
    println!(
        "  Core     {}",
        show(e.core_offset_mhz.map(|v| format!("{v:+} MHz")))
    );
    println!(
        "  Memory   {}",
        show(e.mem_offset_mhz.map(|v| format!("{v:+} MHz")))
    );
    println!(
        "  Power    {}",
        show(e.power_limit_w.map(|v| format!("{v:.0} W")))
    );
    println!(
        "  Ceiling  {}",
        show(
            st.curve_ceiling_mhz
                .or(e.lock_max_clock_mhz)
                .map(|v| format!("{v} MHz"))
        )
    );
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let json = cli.json || matches!(cli.command, Some(Cmd::Json));
    match run(cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            if json {
                println!(
                    "{}",
                    serde_json::json!({ "error": e.message(), "kind": e.kind() })
                );
            } else {
                eprintln!("error: {}", e.message());
            }
            ExitCode::from(exit_code(&e) as u8)
        }
    }
}
