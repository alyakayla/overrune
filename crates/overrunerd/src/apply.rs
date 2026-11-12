//! The only code that writes to the GPU. `apply` validates the whole profile
//! before the first write, so a rejected request leaves the device untouched.

use std::fmt;

use nvml_wrapper::Nvml;
use nvml_wrapper::enums::device::GpuLockedClocksSetting;
use overrune_core::{Capabilities, GpuError, ProfileError, ProfileSettings, Sample};

/// The write surface of the device. Production uses [`NvmlWriter`]; tests use
/// a recording stub, since "nothing was written" is otherwise unobservable.
pub trait Writes {
    fn set_power_limit_mw(&mut self, mw: u32) -> Result<(), GpuError>;
    fn set_core_offset(&mut self, mhz: i32) -> Result<(), GpuError>;
    fn set_mem_offset(&mut self, mhz: i32) -> Result<(), GpuError>;
    /// `None` unlocks the clocks.
    fn set_locked_max_clock(&mut self, mhz: Option<u32>) -> Result<(), GpuError>;
}

#[derive(Debug)]
pub enum ApplyError {
    Profile(ProfileError),
    Gpu(GpuError),
}

impl fmt::Display for ApplyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ApplyError::Profile(e) => write!(f, "{e}"),
            ApplyError::Gpu(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for ApplyError {}

impl From<ProfileError> for ApplyError {
    fn from(e: ProfileError) -> Self {
        ApplyError::Profile(e)
    }
}

impl From<GpuError> for ApplyError {
    fn from(e: GpuError) -> Self {
        ApplyError::Gpu(e)
    }
}

/// Everything a profile can get wrong, checked against the driver's bounds.
pub fn validate(caps: &Capabilities, s: &ProfileSettings) -> Result<(), ProfileError> {
    s.validate(caps)?;
    if let Some(mhz) = s.lock_max_clock_mhz
        && caps.snap_clock(mhz) != mhz
    {
        return Err(ProfileError::Invalid(format!(
            "clock ceiling {mhz} MHz is not a supported step (nearest is {})",
            caps.snap_clock(mhz)
        )));
    }
    Ok(())
}

/// The clock ceiling a profile asks for: an explicit lock, else the top of
/// its curve.
pub fn ceiling(s: &ProfileSettings) -> Option<u32> {
    s.lock_max_clock_mhz
        .or(s.curve.as_ref().map(|c| c.max_clock_mhz))
}

/// Validates all of `s`, then writes it. An empty profile writes nothing; any
/// other profile without a ceiling unlocks the clocks.
pub fn apply<W: Writes>(
    w: &mut W,
    caps: &Capabilities,
    s: &ProfileSettings,
) -> Result<(), ApplyError> {
    validate(caps, s)?;
    if *s == ProfileSettings::default() {
        return Ok(());
    }
    if let Some(watts) = s.power_limit_w {
        w.set_power_limit_mw(
            caps.validate_power_limit_w(watts)
                .map_err(ProfileError::from)?,
        )?;
    }
    if let Some(mhz) = s.core_offset_mhz {
        w.set_core_offset(mhz)?;
    }
    if let Some(mhz) = s.mem_offset_mhz {
        w.set_mem_offset(mhz)?;
    }
    w.set_locked_max_clock(ceiling(s))?;
    Ok(())
}

/// Back to driver defaults: zero offsets, default power limit, clocks unlocked.
pub fn reset<W: Writes>(w: &mut W, caps: &Capabilities) -> Result<(), ApplyError> {
    w.set_core_offset(0)?;
    w.set_mem_offset(0)?;
    w.set_power_limit_mw((caps.power_limit_default_w * 1000.0).round() as u32)?;
    w.set_locked_max_clock(None)?;
    Ok(())
}

/// True when the device no longer holds what `s` asked for — after a suspend,
/// a driver reload, or an external `nvidia-smi`. The clock lock is not part of
/// `Sample`, so it is re-established along with the rest on any drift.
pub fn drifted(sample: &Sample, s: &ProfileSettings) -> bool {
    s.core_offset_mhz
        .is_some_and(|v| v != sample.core_offset_mhz)
        || s.mem_offset_mhz.is_some_and(|v| v != sample.mem_offset_mhz)
        || s.power_limit_w
            .is_some_and(|w| (w - sample.power_limit_w).abs() > 0.5)
}

/// Production [`Writes`]: NVML calls on one device. Needs root.
pub struct NvmlWriter {
    nvml: Nvml,
    index: u32,
    min_clock_mhz: u32,
}

impl NvmlWriter {
    pub fn open(caps: &Capabilities) -> Result<NvmlWriter, GpuError> {
        let nvml = Nvml::init().map_err(GpuError::NvmlUnavailable)?;
        let mut found = None;
        for index in 0..nvml.device_count()? {
            let bus = nvml.device_by_index(index)?.pci_info()?.bus_id;
            if bus.eq_ignore_ascii_case(&caps.bus_id) {
                found = Some(index);
                break;
            }
        }
        Ok(NvmlWriter {
            nvml,
            index: found.ok_or(GpuError::NoDevice)?,
            min_clock_mhz: caps.graphics_clocks_mhz.last().copied().unwrap_or(0),
        })
    }
}

impl Writes for NvmlWriter {
    fn set_power_limit_mw(&mut self, mw: u32) -> Result<(), GpuError> {
        Ok(self
            .nvml
            .device_by_index(self.index)?
            .set_power_management_limit(mw)?)
    }

    fn set_core_offset(&mut self, mhz: i32) -> Result<(), GpuError> {
        Ok(self
            .nvml
            .device_by_index(self.index)?
            .set_gpc_clock_vf_offset(mhz)?)
    }

    fn set_mem_offset(&mut self, mhz: i32) -> Result<(), GpuError> {
        Ok(self
            .nvml
            .device_by_index(self.index)?
            .set_mem_clock_vf_offset(mhz)?)
    }

    fn set_locked_max_clock(&mut self, mhz: Option<u32>) -> Result<(), GpuError> {
        let mut dev = self.nvml.device_by_index(self.index)?;
        match mhz {
            Some(max_clock_mhz) => dev.set_gpu_locked_clocks(GpuLockedClocksSetting::Numeric {
                min_clock_mhz: self.min_clock_mhz,
                max_clock_mhz,
            })?,
            None => dev.reset_gpu_locked_clocks()?,
        }
        Ok(())
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use overrune_core::PStateLimits;

    /// Records every write; never touches hardware.
    #[derive(Default)]
    pub struct Recorder {
        pub log: Vec<String>,
    }

    impl Writes for Recorder {
        fn set_power_limit_mw(&mut self, mw: u32) -> Result<(), GpuError> {
            self.log.push(format!("power {mw}"));
            Ok(())
        }
        fn set_core_offset(&mut self, mhz: i32) -> Result<(), GpuError> {
            self.log.push(format!("core {mhz}"));
            Ok(())
        }
        fn set_mem_offset(&mut self, mhz: i32) -> Result<(), GpuError> {
            self.log.push(format!("mem {mhz}"));
            Ok(())
        }
        fn set_locked_max_clock(&mut self, mhz: Option<u32>) -> Result<(), GpuError> {
            self.log.push(format!("lock {mhz:?}"));
            Ok(())
        }
    }

    pub fn caps() -> Capabilities {
        Capabilities {
            bus_id: "0000:01:00.0".into(),
            name: "Test".into(),
            power_limit_min_w: 50.0,
            power_limit_max_w: 80.0,
            power_limit_default_w: 80.0,
            graphics_clocks_mhz: vec![1980, 1965, 1950, 1935, 1920, 1905],
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

    pub fn sample(core: i32, mem: i32, power_limit_w: f32) -> Sample {
        Sample {
            name: "Test".into(),
            bus_id: "0000:01:00.0".into(),
            driver_version: "0".into(),
            core_clock_mhz: 0,
            mem_clock_mhz: 0,
            temp_c: 50,
            power_w: 0.0,
            power_limit_w,
            util_gpu_pct: 0,
            util_mem_pct: 0,
            vram_used_mb: 0,
            vram_total_mb: 0,
            pstate: 0,
            core_offset_mhz: core,
            mem_offset_mhz: mem,
            fan_pct: None,
            throttle_reasons: vec![],
        }
    }

    #[test]
    fn apply_validates_every_field_before_writing_any() {
        let mut w = Recorder::default();
        let s = ProfileSettings {
            core_offset_mhz: Some(150),
            power_limit_w: Some(500.0),
            ..Default::default()
        };
        assert!(apply(&mut w, &caps(), &s).is_err());
        assert!(w.log.is_empty(), "wrote before validating: {:?}", w.log);

        let s = ProfileSettings {
            core_offset_mhz: Some(150),
            lock_max_clock_mhz: Some(1234),
            ..Default::default()
        };
        assert!(apply(&mut w, &caps(), &s).is_err());
        assert!(w.log.is_empty(), "wrote before validating: {:?}", w.log);
    }

    #[test]
    fn an_empty_profile_writes_nothing() {
        let mut w = Recorder::default();
        apply(&mut w, &caps(), &ProfileSettings::default()).unwrap();
        assert!(w.log.is_empty());
    }

    #[test]
    fn a_valid_profile_writes_each_field_and_unlocks_without_a_ceiling() {
        let mut w = Recorder::default();
        let s = ProfileSettings {
            core_offset_mhz: Some(150),
            power_limit_w: Some(70.0),
            ..Default::default()
        };
        apply(&mut w, &caps(), &s).unwrap();
        assert_eq!(w.log, ["power 70000", "core 150", "lock None"]);
    }

    #[test]
    fn reset_restores_defaults() {
        let mut w = Recorder::default();
        reset(&mut w, &caps()).unwrap();
        assert_eq!(w.log, ["core 0", "mem 0", "power 80000", "lock None"]);
    }

    #[test]
    fn drift_is_any_enforced_field_differing_from_the_device() {
        let s = ProfileSettings {
            core_offset_mhz: Some(150),
            power_limit_w: Some(70.0),
            ..Default::default()
        };
        assert!(!drifted(&sample(150, 0, 70.0), &s));
        assert!(drifted(&sample(0, 0, 70.0), &s));
        assert!(drifted(&sample(150, 0, 80.0), &s));
        assert!(!drifted(&sample(7, 7, 80.0), &ProfileSettings::default()));
    }
}
