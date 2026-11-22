//! What this card actually allows, read from the driver once, and the
//! validation every write path goes through. Nothing here invents a limit:
//! every bound comes from NVML.

use std::fmt;

use nvml_wrapper::enum_wrappers::device::Clock;

use crate::gpu::{Gpu, GpuError, OFFSET_PSTATE, pstate_number};

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Capabilities {
    pub bus_id: String,
    pub name: String,
    pub power_limit_min_w: f32,
    pub power_limit_max_w: f32,
    pub power_limit_default_w: f32,
    /// Supported graphics clocks, descending.
    pub graphics_clocks_mhz: Vec<u32>,
    pub mem_clock_mhz: u32,
    pub num_fans: u32,
    pub pstates: Vec<PStateLimits>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PStateLimits {
    pub pstate: u8,
    pub core_offset_min_mhz: i32,
    pub core_offset_max_mhz: i32,
    pub mem_offset_min_mhz: i32,
    pub mem_offset_max_mhz: i32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct OutOfRange {
    pub what: &'static str,
    pub requested: f32,
    pub min: f32,
    pub max: f32,
}

impl fmt::Display for OutOfRange {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} {} is outside the driver-reported range {}–{}",
            self.what,
            trim(self.requested),
            trim(self.min),
            trim(self.max)
        )
    }
}

impl std::error::Error for OutOfRange {}

fn trim(v: f32) -> String {
    if v.fract() == 0.0 {
        format!("{}", v as i64)
    } else {
        format!("{v:.1}")
    }
}

impl Gpu {
    pub fn capabilities(&self) -> Result<Capabilities, GpuError> {
        let dev = self.device()?;
        let constraints = dev.power_management_limit_constraints()?;
        // `max_clock_info` can report a value the driver's own list lacks, and
        // `supported_graphics_clocks` rejects that; ask with a listed clock.
        let mem_clock_mhz = dev
            .supported_memory_clocks()?
            .into_iter()
            .max()
            .ok_or(nvml_wrapper::error::NvmlError::NotFound)?;
        let mut graphics_clocks_mhz = dev.supported_graphics_clocks(mem_clock_mhz)?;
        graphics_clocks_mhz.sort_unstable_by(|a, b| b.cmp(a));
        graphics_clocks_mhz.dedup();

        // Offsets are per performance state; keep only the states the driver
        // reports a range for (on most consumer cards that is P0 alone).
        let mut pstates = Vec::new();
        for state in crate::gpu::PSTATES {
            let Ok(core) = dev.clock_offset(Clock::Graphics, state) else {
                continue;
            };
            let mem = dev.clock_offset(Clock::Memory, state).ok();
            pstates.push(PStateLimits {
                pstate: pstate_number(state),
                core_offset_min_mhz: core.min_clock_offset_mhz,
                core_offset_max_mhz: core.max_clock_offset_mhz,
                mem_offset_min_mhz: mem.as_ref().map_or(0, |m| m.min_clock_offset_mhz),
                mem_offset_max_mhz: mem.as_ref().map_or(0, |m| m.max_clock_offset_mhz),
            });
        }

        Ok(Capabilities {
            bus_id: self.bus_id.clone(),
            name: self.name.clone(),
            power_limit_min_w: constraints.min_limit as f32 / 1000.0,
            power_limit_max_w: constraints.max_limit as f32 / 1000.0,
            power_limit_default_w: dev.power_management_limit_default()? as f32 / 1000.0,
            graphics_clocks_mhz,
            mem_clock_mhz,
            num_fans: dev.num_fans()?,
            pstates,
        })
    }
}

impl Capabilities {
    /// The performance state offsets are applied at.
    pub fn offset_pstate(&self) -> u8 {
        pstate_number(OFFSET_PSTATE)
    }

    /// Nearest supported graphics clock, clamped to the supported range.
    /// Ties go to the higher clock.
    pub fn snap_clock(&self, mhz: u32) -> u32 {
        self.graphics_clocks_mhz
            .iter()
            .copied()
            .min_by_key(|c| (c.abs_diff(mhz), u32::MAX - *c))
            .unwrap_or(mhz)
    }

    /// Validates a power limit in watts, returning milliwatts for NVML.
    pub fn validate_power_limit_w(&self, w: f32) -> Result<u32, OutOfRange> {
        if !w.is_finite() || w < self.power_limit_min_w || w > self.power_limit_max_w {
            return Err(OutOfRange {
                what: "power limit",
                requested: w,
                min: self.power_limit_min_w,
                max: self.power_limit_max_w,
            });
        }
        Ok((w * 1000.0).round() as u32)
    }

    pub fn validate_core_offset(&self, pstate: u8, mhz: i32) -> Result<i32, OutOfRange> {
        self.validate_offset("core clock offset", pstate, mhz, |p| {
            (p.core_offset_min_mhz, p.core_offset_max_mhz)
        })
    }

    pub fn validate_mem_offset(&self, pstate: u8, mhz: i32) -> Result<i32, OutOfRange> {
        self.validate_offset("memory clock offset", pstate, mhz, |p| {
            (p.mem_offset_min_mhz, p.mem_offset_max_mhz)
        })
    }

    fn validate_offset(
        &self,
        what: &'static str,
        pstate: u8,
        mhz: i32,
        range: impl Fn(&PStateLimits) -> (i32, i32),
    ) -> Result<i32, OutOfRange> {
        // An unknown performance state has no reported range, so the only
        // value that can be honoured there is "no offset".
        let (min, max) = self
            .pstates
            .iter()
            .find(|p| p.pstate == pstate)
            .map_or((0, 0), range);
        if mhz < min || mhz > max {
            return Err(OutOfRange {
                what,
                requested: mhz as f32,
                min: min as f32,
                max: max as f32,
            });
        }
        Ok(mhz)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn snap_clock_picks_the_nearest_supported_step() {
        assert_eq!(caps().snap_clock(1958), 1965);
        assert_eq!(caps().snap_clock(1951), 1950);
    }

    #[test]
    fn snap_clock_clamps_outside_the_supported_range() {
        assert_eq!(caps().snap_clock(9000), 1980);
        assert_eq!(caps().snap_clock(10), 300);
    }

    #[test]
    fn snap_clock_breaks_ties_upward() {
        // Exactly between 300 and 1950.
        assert_eq!(caps().snap_clock(1125), 1950);
    }

    #[test]
    fn power_limit_converts_to_milliwatts_inside_the_range() {
        assert_eq!(caps().validate_power_limit_w(62.5), Ok(62_500));
    }

    #[test]
    fn power_limit_outside_the_range_is_rejected_with_both_bounds() {
        let err = caps().validate_power_limit_w(500.0).unwrap_err();
        assert_eq!(
            err.to_string(),
            "power limit 500 is outside the driver-reported range 50–80"
        );
        assert!(caps().validate_power_limit_w(49.9).is_err());
        assert!(caps().validate_power_limit_w(f32::NAN).is_err());
    }

    #[test]
    fn offsets_are_checked_against_the_reported_pstate_range() {
        assert_eq!(caps().validate_core_offset(0, 1000), Ok(1000));
        assert!(caps().validate_core_offset(0, 1001).is_err());
        assert_eq!(caps().validate_mem_offset(0, -500), Ok(-500));
        assert!(caps().validate_mem_offset(0, -501).is_err());
    }

    #[test]
    fn an_unreported_pstate_accepts_only_a_zero_offset() {
        assert_eq!(caps().validate_core_offset(8, 0), Ok(0));
        assert!(caps().validate_core_offset(8, 15).is_err());
    }
}
