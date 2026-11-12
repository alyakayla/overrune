//! The software clock curve: temperature in, a graphics-clock ceiling out.
//!
//! The hardware has no editable voltage/frequency curve on Linux, so the
//! ceiling is enforced by locking the maximum clock. Two thresholds and a
//! dwell time keep it from oscillating around a single temperature.

use crate::caps::Capabilities;
use crate::profile::Curve;

#[derive(Debug, Clone)]
pub struct CurveState {
    ceiling_mhz: u32,
    last_change_ms: u64,
}

impl CurveState {
    pub fn new(initial_ceiling_mhz: u32) -> CurveState {
        CurveState {
            ceiling_mhz: initial_ceiling_mhz,
            last_change_ms: 0,
        }
    }

    pub fn ceiling(&self) -> u32 {
        self.ceiling_mhz
    }

    /// Advances the ceiling by at most one step. Returns the new ceiling only
    /// when it actually changed, so callers write to hardware only on change.
    pub fn step(
        &mut self,
        curve: &Curve,
        caps: &Capabilities,
        temp_c: u32,
        now_ms: u64,
    ) -> Option<u32> {
        let want = if temp_c >= curve.temp_up_c {
            self.ceiling_mhz.saturating_sub(curve.step_mhz)
        } else if temp_c <= curve.temp_down_c {
            self.ceiling_mhz.saturating_add(curve.step_mhz)
        } else {
            return None;
        };

        let want = caps
            .snap_clock(want.clamp(curve.min_clock_mhz, curve.max_clock_mhz))
            .clamp(curve.min_clock_mhz, curve.max_clock_mhz);
        if want == self.ceiling_mhz {
            return None;
        }
        // The dwell gate is checked only once a change is actually due, so a
        // quiet period does not bank credit for a burst of steps.
        if now_ms.saturating_sub(self.last_change_ms) < curve.dwell_ms {
            return None;
        }
        self.ceiling_mhz = want;
        self.last_change_ms = now_ms;
        Some(want)
    }
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

    fn curve() -> Curve {
        Curve {
            temp_up_c: 80,
            temp_down_c: 70,
            min_clock_mhz: 1905,
            max_clock_mhz: 1980,
            step_mhz: 15,
            dwell_ms: 1000,
        }
    }

    #[test]
    fn no_change_inside_the_dead_band() {
        let mut s = CurveState::new(1950);
        assert_eq!(s.step(&curve(), &caps(), 75, 10_000), None);
        assert_eq!(s.ceiling(), 1950);
    }

    #[test]
    fn steps_down_once_above_the_upper_threshold() {
        let mut s = CurveState::new(1950);
        assert_eq!(s.step(&curve(), &caps(), 80, 10_000), Some(1935));
    }

    #[test]
    fn steps_back_up_below_the_lower_threshold() {
        let mut s = CurveState::new(1935);
        assert_eq!(s.step(&curve(), &caps(), 70, 10_000), Some(1950));
    }

    #[test]
    fn honours_the_dwell_time_between_changes() {
        let mut s = CurveState::new(1950);
        assert_eq!(s.step(&curve(), &caps(), 85, 10_000), Some(1935));
        assert_eq!(s.step(&curve(), &caps(), 85, 10_500), None);
        assert_eq!(s.step(&curve(), &caps(), 85, 11_000), Some(1920));
    }

    #[test]
    fn never_moves_more_than_one_step_per_change() {
        let mut s = CurveState::new(1980);
        assert_eq!(s.step(&curve(), &caps(), 99, 10_000), Some(1965));
    }

    #[test]
    fn clamps_to_the_curve_min_and_max() {
        let mut s = CurveState::new(1905);
        assert_eq!(s.step(&curve(), &caps(), 95, 10_000), None);
        let mut s = CurveState::new(1980);
        assert_eq!(s.step(&curve(), &caps(), 20, 10_000), None);
    }

    #[test]
    fn a_sustained_rise_walks_down_in_snapped_increments() {
        let mut s = CurveState::new(1980);
        let (c, caps) = (curve(), caps());
        let mut seen = Vec::new();
        for t in 0..6u64 {
            if let Some(mhz) = s.step(&c, &caps, 90, t * 1000) {
                seen.push(mhz);
            }
        }
        assert_eq!(seen, vec![1965, 1950, 1935, 1920, 1905]);
        assert_eq!(s.ceiling(), 1905);
    }
}
