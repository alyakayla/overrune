//! What the daemon believes it enforces, and the request handlers that change it.

use std::io;
use std::path::PathBuf;

use overrune_core::ipc::{DaemonStatus, ErrorKind, Request, Response};
use overrune_core::profile::{self, ACTIVE_PATH, PROFILE_DIR};
use overrune_core::{Capabilities, CurveState, ProfileError, ProfileSettings, Sample};

use crate::apply::{self, ApplyError, Writes};

/// An `Apply` request is persisted under this profile name so the active
/// state survives a daemon restart and the resume hook can re-apply it.
const ADHOC: &str = "adhoc";

pub struct Daemon<W: Writes> {
    writes: W,
    caps: Capabilities,
    profile_dir: PathBuf,
    active_path: PathBuf,
    active: Option<String>,
    enforcing: ProfileSettings,
    curve: Option<CurveState>,
}

impl<W: Writes> Daemon<W> {
    pub fn new(writes: W, caps: Capabilities) -> Self {
        Self::with_paths(writes, caps, PROFILE_DIR.into(), ACTIVE_PATH.into())
    }

    pub fn with_paths(
        writes: W,
        caps: Capabilities,
        profile_dir: PathBuf,
        active_path: PathBuf,
    ) -> Self {
        Daemon {
            writes,
            caps,
            profile_dir,
            active_path,
            active: None,
            enforcing: ProfileSettings::default(),
            curve: None,
        }
    }

    /// Applies the profile named in the active file, if there is one.
    pub fn load_active(&mut self) -> Result<(), ApplyError> {
        let name = match std::fs::read_to_string(&self.active_path) {
            Ok(n) => n.trim().to_string(),
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(ProfileError::from(e).into()),
        };
        let settings = profile::load(&self.profile_dir, &name)?;
        self.enforce(Some(name), settings)
    }

    fn enforce(&mut self, name: Option<String>, s: ProfileSettings) -> Result<(), ApplyError> {
        apply::apply(&mut self.writes, &self.caps, &s)?;
        self.curve = s.curve.as_ref().map(|c| CurveState::new(c.max_clock_mhz));
        self.active = name;
        self.enforcing = s;
        Ok(())
    }

    fn persist_active(&self) -> Result<(), ProfileError> {
        match &self.active {
            Some(name) => std::fs::write(&self.active_path, name)?,
            None => match std::fs::remove_file(&self.active_path) {
                Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e.into()),
                _ => {}
            },
        }
        Ok(())
    }

    pub fn handle(&mut self, req: Request) -> Response {
        match self.dispatch(req) {
            Ok(r) => r,
            Err(e) => e,
        }
    }

    fn dispatch(&mut self, req: Request) -> Result<Response, Response> {
        match req {
            Request::Apply(s) => {
                apply::validate(&self.caps, &s).map_err(profile_err)?;
                self.enforce(Some(ADHOC.into()), s.clone())
                    .map_err(apply_err)?;
                profile::save(&self.profile_dir, ADHOC, &s).map_err(profile_err)?;
                self.persist_active().map_err(profile_err)?;
            }
            Request::Reset => {
                apply::reset(&mut self.writes, &self.caps).map_err(apply_err)?;
                self.curve = None;
                self.active = None;
                self.enforcing = ProfileSettings::default();
                self.persist_active().map_err(profile_err)?;
            }
            Request::Status => {
                return Ok(Response::Status(DaemonStatus {
                    active_profile: self.active.clone(),
                    enforcing: self.enforcing.clone(),
                    curve_ceiling_mhz: self.curve.as_ref().map(CurveState::ceiling),
                }));
            }
            Request::UseProfile { name } => {
                let s = profile::load(&self.profile_dir, &name).map_err(profile_err)?;
                self.enforce(Some(name), s).map_err(apply_err)?;
                self.persist_active().map_err(profile_err)?;
            }
            Request::SaveProfile { name, settings } => {
                apply::validate(&self.caps, &settings).map_err(profile_err)?;
                profile::save(&self.profile_dir, &name, &settings).map_err(profile_err)?;
            }
            Request::ListProfiles => {
                let names = profile::list(&self.profile_dir).map_err(profile_err)?;
                return Ok(Response::Profiles { names });
            }
            Request::RemoveProfile { name } => {
                profile::remove(&self.profile_dir, &name).map_err(profile_err)?;
                // A dangling active file would fail the next boot's load.
                if self.active.as_deref() == Some(&name) {
                    self.active = None;
                    self.persist_active().map_err(profile_err)?;
                }
            }
        }
        Ok(Response::Ok)
    }

    /// One poll: re-apply if the device lost what we set, otherwise run one
    /// curve step. `sample` is the device's current state.
    pub fn tick(&mut self, sample: &Sample, now_ms: u64) -> Result<(), ApplyError> {
        if apply::drifted(sample, &self.enforcing) {
            let s = self.enforcing.clone();
            return self.enforce(self.active.clone(), s);
        }
        if let (Some(state), Some(curve)) = (&mut self.curve, &self.enforcing.curve)
            && let Some(mhz) = state.step(curve, &self.caps, sample.temp_c, now_ms)
        {
            self.writes.set_locked_max_clock(Some(mhz))?;
        }
        Ok(())
    }
}

fn error(kind: ErrorKind, e: impl std::fmt::Display) -> Response {
    Response::Error {
        kind,
        message: e.to_string(),
    }
}

fn profile_err(e: ProfileError) -> Response {
    let kind = match &e {
        ProfileError::Io(io) if io.kind() == io::ErrorKind::NotFound => ErrorKind::NotFound,
        ProfileError::Io(_) | ProfileError::Toml(_) => ErrorKind::Internal,
        ProfileError::Invalid(_) | ProfileError::OutOfRange(_) | ProfileError::BadName(_) => {
            ErrorKind::OutOfRange
        }
    };
    error(kind, e)
}

fn apply_err(e: ApplyError) -> Response {
    match e {
        ApplyError::Profile(e) => profile_err(e),
        ApplyError::Gpu(e) => error(ErrorKind::Driver, e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::apply::tests::{Recorder, caps, sample};
    use overrune_core::Curve;

    fn daemon(tag: &str) -> Daemon<Recorder> {
        let dir =
            std::env::temp_dir().join(format!("overrunerd-state-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Daemon::with_paths(
            Recorder::default(),
            caps(),
            dir.join("p"),
            dir.join("active"),
        )
    }

    fn enforcing_70w(d: &mut Daemon<Recorder>) {
        let s = ProfileSettings {
            core_offset_mhz: Some(150),
            power_limit_w: Some(70.0),
            ..Default::default()
        };
        assert_eq!(d.handle(Request::Apply(s)), Response::Ok);
        d.writes.log.clear();
    }

    #[test]
    fn tick_reapplies_when_the_device_has_drifted_from_the_enforced_profile() {
        let mut d = daemon("drift");
        enforcing_70w(&mut d);
        d.tick(&sample(0, 0, 80.0), 1000).unwrap();
        assert_eq!(d.writes.log, ["power 70000", "core 150", "lock None"]);
    }

    #[test]
    fn tick_does_not_rewrite_when_the_device_still_matches() {
        let mut d = daemon("match");
        enforcing_70w(&mut d);
        d.tick(&sample(150, 0, 70.0), 1000).unwrap();
        assert!(d.writes.log.is_empty());
    }

    #[test]
    fn tick_steps_the_curve_and_writes_the_new_ceiling() {
        let mut d = daemon("curve");
        let s = ProfileSettings {
            curve: Some(Curve {
                temp_up_c: 80,
                temp_down_c: 70,
                min_clock_mhz: 1905,
                max_clock_mhz: 1980,
                step_mhz: 15,
                dwell_ms: 1000,
            }),
            ..Default::default()
        };
        assert_eq!(d.handle(Request::Apply(s)), Response::Ok);
        d.writes.log.clear();
        let mut hot = sample(0, 0, 80.0);
        hot.temp_c = 85;
        d.tick(&hot, 5000).unwrap();
        assert_eq!(d.writes.log, ["lock Some(1965)"]);
    }

    #[test]
    fn a_rejected_apply_changes_nothing_and_persists_nothing() {
        let mut d = daemon("reject");
        let s = ProfileSettings {
            core_offset_mhz: Some(150),
            power_limit_w: Some(500.0),
            ..Default::default()
        };
        assert!(matches!(
            d.handle(Request::Apply(s)),
            Response::Error {
                kind: ErrorKind::OutOfRange,
                ..
            }
        ));
        assert!(d.writes.log.is_empty());
        assert!(!d.active_path.exists());
    }

    #[test]
    fn the_active_profile_survives_a_restart() {
        let mut d = daemon("restart");
        enforcing_70w(&mut d);
        let mut fresh = Daemon::with_paths(
            Recorder::default(),
            caps(),
            d.profile_dir.clone(),
            d.active_path.clone(),
        );
        fresh.load_active().unwrap();
        assert_eq!(fresh.writes.log, ["power 70000", "core 150", "lock None"]);
    }
}
