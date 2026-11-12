//! Named profiles on disk. The daemon owns the directory; the CLI and GUI
//! reach it through the daemon, so everything here is plain file I/O.

use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

use crate::caps::{Capabilities, OutOfRange};

pub const PROFILE_DIR: &str = "/etc/overrune/profiles";
pub const ACTIVE_PATH: &str = "/etc/overrune/active";

#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ProfileSettings {
    #[serde(default)]
    pub core_offset_mhz: Option<i32>,
    #[serde(default)]
    pub mem_offset_mhz: Option<i32>,
    #[serde(default)]
    pub power_limit_w: Option<f32>,
    #[serde(default)]
    pub lock_max_clock_mhz: Option<u32>,
    #[serde(default)]
    pub curve: Option<Curve>,
}

/// The software clock curve: a temperature-driven ceiling on the graphics
/// clock, with separate up/down thresholds so it cannot oscillate.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Curve {
    pub temp_up_c: u32,
    pub temp_down_c: u32,
    pub min_clock_mhz: u32,
    pub max_clock_mhz: u32,
    pub step_mhz: u32,
    pub dwell_ms: u64,
}

#[derive(Debug)]
pub enum ProfileError {
    Io(io::Error),
    Toml(String),
    Invalid(String),
    OutOfRange(OutOfRange),
    BadName(String),
}

impl fmt::Display for ProfileError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ProfileError::Io(e) => write!(f, "{e}"),
            ProfileError::Toml(e) => write!(f, "malformed profile: {e}"),
            ProfileError::Invalid(e) => write!(f, "{e}"),
            ProfileError::OutOfRange(e) => write!(f, "{e}"),
            ProfileError::BadName(n) => write!(
                f,
                "profile name {n:?} is not allowed: use letters, digits, '-' and '_'"
            ),
        }
    }
}

impl std::error::Error for ProfileError {}

impl From<io::Error> for ProfileError {
    fn from(e: io::Error) -> Self {
        ProfileError::Io(e)
    }
}

impl From<OutOfRange> for ProfileError {
    fn from(e: OutOfRange) -> Self {
        ProfileError::OutOfRange(e)
    }
}

impl ProfileSettings {
    /// Every bound comes from the driver; a profile that fails this is never
    /// written to hardware and never saved.
    pub fn validate(&self, caps: &Capabilities) -> Result<(), ProfileError> {
        let pstate = caps.offset_pstate();
        if let Some(v) = self.core_offset_mhz {
            caps.validate_core_offset(pstate, v)?;
        }
        if let Some(v) = self.mem_offset_mhz {
            caps.validate_mem_offset(pstate, v)?;
        }
        if let Some(w) = self.power_limit_w {
            caps.validate_power_limit_w(w)?;
        }
        if let Some(c) = &self.curve {
            c.validate(caps)?;
        }
        Ok(())
    }
}

impl Curve {
    pub fn validate(&self, caps: &Capabilities) -> Result<(), ProfileError> {
        if self.temp_down_c >= self.temp_up_c {
            return Err(ProfileError::Invalid(format!(
                "curve temp_down_c ({}) must be below temp_up_c ({}) so the ceiling cannot oscillate",
                self.temp_down_c, self.temp_up_c
            )));
        }
        if self.min_clock_mhz > self.max_clock_mhz {
            return Err(ProfileError::Invalid(format!(
                "curve min_clock_mhz ({}) is above max_clock_mhz ({})",
                self.min_clock_mhz, self.max_clock_mhz
            )));
        }
        if self.step_mhz == 0 {
            return Err(ProfileError::Invalid(
                "curve step_mhz must be greater than zero".into(),
            ));
        }
        for mhz in [self.min_clock_mhz, self.max_clock_mhz] {
            if caps.snap_clock(mhz) != mhz {
                return Err(ProfileError::Invalid(format!(
                    "curve clock {mhz} MHz is not a supported step (nearest is {})",
                    caps.snap_clock(mhz)
                )));
            }
        }
        Ok(())
    }
}

fn check_name(name: &str) -> Result<(), ProfileError> {
    let ok = !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    if ok {
        Ok(())
    } else {
        Err(ProfileError::BadName(name.to_string()))
    }
}

fn path_of(dir: &Path, name: &str) -> Result<PathBuf, ProfileError> {
    check_name(name)?;
    Ok(dir.join(format!("{name}.toml")))
}

pub fn load(dir: &Path, name: &str) -> Result<ProfileSettings, ProfileError> {
    let text = std::fs::read_to_string(path_of(dir, name)?)?;
    toml::from_str(&text).map_err(|e| ProfileError::Toml(e.to_string()))
}

pub fn save(dir: &Path, name: &str, s: &ProfileSettings) -> Result<(), ProfileError> {
    let path = path_of(dir, name)?;
    std::fs::create_dir_all(dir)?;
    let text = toml::to_string_pretty(s).map_err(|e| ProfileError::Toml(e.to_string()))?;
    std::fs::write(path, text)?;
    Ok(())
}

pub fn list(dir: &Path) -> Result<Vec<String>, ProfileError> {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };
    let mut names: Vec<String> = entries
        .filter_map(Result::ok)
        .filter_map(|e| {
            let path = e.path();
            if path.extension()? != "toml" {
                return None;
            }
            Some(path.file_stem()?.to_str()?.to_string())
        })
        .collect();
    names.sort();
    Ok(names)
}

pub fn remove(dir: &Path, name: &str) -> Result<(), ProfileError> {
    Ok(std::fs::remove_file(path_of(dir, name)?)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::caps::PStateLimits;

    fn caps() -> Capabilities {
        Capabilities {
            bus_id: "0000:01:00.0".into(),
            name: "Test".into(),
            power_limit_min_w: 50.0,
            power_limit_max_w: 80.0,
            power_limit_default_w: 80.0,
            graphics_clocks_mhz: vec![1980, 1965, 1950, 300],
            mem_clock_mhz: 6001,
            num_fans: 0,
            pstates: vec![PStateLimits {
                pstate: 0,
                core_offset_min_mhz: -200,
                core_offset_max_mhz: 1000,
                mem_offset_min_mhz: -500,
                mem_offset_max_mhz: 2000,
            }],
        }
    }

    fn tmpdir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "overrune-test-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn a_profile_survives_a_save_and_load_round_trip() {
        let dir = tmpdir();
        let s = ProfileSettings {
            core_offset_mhz: Some(150),
            mem_offset_mhz: Some(-100),
            power_limit_w: Some(75.0),
            lock_max_clock_mhz: Some(1950),
            curve: Some(Curve {
                temp_up_c: 80,
                temp_down_c: 70,
                min_clock_mhz: 300,
                max_clock_mhz: 1980,
                step_mhz: 15,
                dwell_ms: 2000,
            }),
        };
        save(&dir, "gaming", &s).unwrap();
        assert_eq!(load(&dir, "gaming").unwrap(), s);
        assert_eq!(list(&dir).unwrap(), vec!["gaming".to_string()]);
        remove(&dir, "gaming").unwrap();
        assert!(list(&dir).unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_absent_profile_directory_lists_nothing_rather_than_failing() {
        assert!(list(&tmpdir().join("nope")).unwrap().is_empty());
    }

    #[test]
    fn missing_fields_default_to_none() {
        let s: ProfileSettings = toml::from_str("core_offset_mhz = 10").unwrap();
        assert_eq!(s.core_offset_mhz, Some(10));
        assert_eq!(s.power_limit_w, None);
        assert_eq!(s.curve, None);
    }

    #[test]
    fn names_that_could_escape_the_profile_directory_are_rejected() {
        for bad in ["../../etc/passwd", "a/b", "", ".", "x.toml"] {
            assert!(matches!(
                save(&tmpdir(), bad, &ProfileSettings::default()),
                Err(ProfileError::BadName(_))
            ));
            assert!(matches!(
                load(&tmpdir(), bad),
                Err(ProfileError::BadName(_))
            ));
            assert!(matches!(
                remove(&tmpdir(), bad),
                Err(ProfileError::BadName(_))
            ));
        }
    }

    #[test]
    fn validation_rejects_values_the_driver_would_refuse() {
        let caps = caps();
        assert!(
            ProfileSettings {
                core_offset_mhz: Some(5000),
                ..Default::default()
            }
            .validate(&caps)
            .is_err()
        );
        assert!(
            ProfileSettings {
                power_limit_w: Some(10.0),
                ..Default::default()
            }
            .validate(&caps)
            .is_err()
        );
        assert!(
            ProfileSettings {
                core_offset_mhz: Some(150),
                power_limit_w: Some(75.0),
                ..Default::default()
            }
            .validate(&caps)
            .is_ok()
        );
    }

    #[test]
    fn a_curve_that_would_oscillate_or_misstep_is_rejected() {
        let caps = caps();
        let base = Curve {
            temp_up_c: 80,
            temp_down_c: 70,
            min_clock_mhz: 300,
            max_clock_mhz: 1980,
            step_mhz: 15,
            dwell_ms: 2000,
        };
        assert!(base.validate(&caps).is_ok());
        assert!(
            Curve {
                temp_down_c: 80,
                ..base.clone()
            }
            .validate(&caps)
            .is_err()
        );
        assert!(
            Curve {
                step_mhz: 0,
                ..base.clone()
            }
            .validate(&caps)
            .is_err()
        );
        assert!(
            Curve {
                min_clock_mhz: 1907,
                ..base
            }
            .validate(&caps)
            .is_err()
        );
    }
}
