//! NVML read layer. No X11, no `nvidia-settings`: every value here comes from
//! `libnvidia-ml.so` directly.

use std::fmt;

use nvml_wrapper::Nvml;
use nvml_wrapper::bitmasks::device::ThrottleReasons;
use nvml_wrapper::enum_wrappers::device::{Clock, PerformanceState, TemperatureSensor};
use nvml_wrapper::error::NvmlError;

/// Performance state offsets are read and written at, matching what the
/// driver exposes to Afterburner-style tools: the boost state.
pub const OFFSET_PSTATE: PerformanceState = PerformanceState::Zero;

/// Every performance state, in order, for capability probing.
pub const PSTATES: [PerformanceState; 16] = [
    PerformanceState::Zero,
    PerformanceState::One,
    PerformanceState::Two,
    PerformanceState::Three,
    PerformanceState::Four,
    PerformanceState::Five,
    PerformanceState::Six,
    PerformanceState::Seven,
    PerformanceState::Eight,
    PerformanceState::Nine,
    PerformanceState::Ten,
    PerformanceState::Eleven,
    PerformanceState::Twelve,
    PerformanceState::Thirteen,
    PerformanceState::Fourteen,
    PerformanceState::Fifteen,
];

#[derive(Debug)]
pub enum GpuError {
    NvmlUnavailable(NvmlError),
    NoDevice,
    Driver(NvmlError),
}

impl fmt::Display for GpuError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            GpuError::NvmlUnavailable(e) => {
                write!(f, "NVML is unavailable (is the NVIDIA driver loaded?): {e}")
            }
            GpuError::NoDevice => write!(f, "no NVIDIA device found"),
            GpuError::Driver(e) => write!(f, "driver call failed: {e}"),
        }
    }
}

impl std::error::Error for GpuError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            GpuError::NvmlUnavailable(e) | GpuError::Driver(e) => Some(e),
            GpuError::NoDevice => None,
        }
    }
}

impl From<NvmlError> for GpuError {
    fn from(e: NvmlError) -> Self {
        GpuError::Driver(e)
    }
}

/// One NVIDIA device, identified by index and PCI bus id.
///
/// `nvml_wrapper::Device` borrows its `Nvml`, so the handle is re-derived per
/// call rather than stored; it is a table lookup in the driver, not a probe.
pub struct Gpu {
    nvml: Nvml,
    index: u32,
    pub bus_id: String,
    pub name: String,
    pub driver_version: String,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Sample {
    pub name: String,
    pub bus_id: String,
    pub driver_version: String,
    pub core_clock_mhz: u32,
    pub mem_clock_mhz: u32,
    pub temp_c: u32,
    pub power_w: f32,
    pub power_limit_w: f32,
    pub util_gpu_pct: u32,
    pub util_mem_pct: u32,
    pub vram_used_mb: u64,
    pub vram_total_mb: u64,
    pub pstate: u8,
    pub core_offset_mhz: i32,
    pub mem_offset_mhz: i32,
    pub fan_pct: Option<u32>,
    pub throttle_reasons: Vec<String>,
}

impl Gpu {
    /// Opens the first NVIDIA device.
    pub fn open() -> Result<Gpu, GpuError> {
        Self::open_with(|nvml| {
            if nvml.device_count()? == 0 {
                return Err(GpuError::NoDevice);
            }
            Ok(0)
        })
    }

    /// Opens the device whose PCI bus id matches, e.g. `0000:01:00.0`.
    pub fn open_by_bus_id(bus_id: &str) -> Result<Gpu, GpuError> {
        Self::open_with(|nvml| {
            for index in 0..nvml.device_count()? {
                let dev = nvml.device_by_index(index)?;
                if dev.pci_info()?.bus_id.eq_ignore_ascii_case(bus_id) {
                    return Ok(index);
                }
            }
            Err(GpuError::NoDevice)
        })
    }

    fn open_with(pick: impl FnOnce(&Nvml) -> Result<u32, GpuError>) -> Result<Gpu, GpuError> {
        let nvml = Nvml::init().map_err(GpuError::NvmlUnavailable)?;
        let index = pick(&nvml)?;
        let driver_version = nvml.sys_driver_version()?;
        let dev = nvml.device_by_index(index)?;
        let bus_id = dev.pci_info()?.bus_id;
        let name = dev.name()?;
        Ok(Gpu {
            nvml,
            index,
            bus_id,
            name,
            driver_version,
        })
    }

    pub(crate) fn device(&self) -> Result<nvml_wrapper::Device<'_>, GpuError> {
        Ok(self.nvml.device_by_index(self.index)?)
    }

    pub fn sample(&self) -> Result<Sample, GpuError> {
        let dev = self.device()?;
        let util = dev.utilization_rates()?;
        let mem = dev.memory_info()?;
        let fan_pct = match dev.num_fans()? {
            0 => None,
            _ => Some(dev.fan_speed(0)?),
        };
        Ok(Sample {
            name: self.name.clone(),
            bus_id: self.bus_id.clone(),
            driver_version: self.driver_version.clone(),
            core_clock_mhz: dev.clock_info(Clock::Graphics)?,
            mem_clock_mhz: dev.clock_info(Clock::Memory)?,
            temp_c: dev.temperature(TemperatureSensor::Gpu)?,
            power_w: dev.power_usage()? as f32 / 1000.0,
            // `power_management_limit` is NotSupported on laptop GPUs; the
            // enforced limit is the value actually in effect either way.
            power_limit_w: dev.enforced_power_limit()? as f32 / 1000.0,
            util_gpu_pct: util.gpu,
            util_mem_pct: util.memory,
            vram_used_mb: mem.used / 1_048_576,
            vram_total_mb: mem.total / 1_048_576,
            pstate: pstate_number(dev.performance_state()?),
            core_offset_mhz: dev
                .clock_offset(Clock::Graphics, OFFSET_PSTATE)?
                .clock_offset_mhz,
            mem_offset_mhz: dev
                .clock_offset(Clock::Memory, OFFSET_PSTATE)?
                .clock_offset_mhz,
            fan_pct,
            throttle_reasons: throttle_names(dev.current_throttle_reasons()?),
        })
    }
}

pub fn pstate_number(p: PerformanceState) -> u8 {
    use PerformanceState::*;
    match p {
        Zero => 0,
        One => 1,
        Two => 2,
        Three => 3,
        Four => 4,
        Five => 5,
        Six => 6,
        Seven => 7,
        Eight => 8,
        Nine => 9,
        Ten => 10,
        Eleven => 11,
        Twelve => 12,
        Thirteen => 13,
        Fourteen => 14,
        Fifteen => 15,
        Unknown => 32,
    }
}

/// Reasons the GPU is currently below its maximum clock, in the wording the
/// CLI and GUI show. `GPU_IDLE` is dropped: it is the normal desktop state,
/// not a throttle worth alarming anyone about.
fn throttle_names(r: ThrottleReasons) -> Vec<String> {
    const NAMED: &[(ThrottleReasons, &str)] = &[
        (ThrottleReasons::SW_POWER_CAP, "power limit"),
        (ThrottleReasons::SW_THERMAL_SLOWDOWN, "thermal"),
        (ThrottleReasons::HW_THERMAL_SLOWDOWN, "thermal (hardware)"),
        (ThrottleReasons::HW_POWER_BRAKE_SLOWDOWN, "power brake"),
        (ThrottleReasons::HW_SLOWDOWN, "hardware slowdown"),
        (ThrottleReasons::SYNC_BOOST, "sync boost"),
        (
            ThrottleReasons::APPLICATIONS_CLOCKS_SETTING,
            "applications clocks",
        ),
        (ThrottleReasons::DISPLAY_CLOCK_SETTING, "display clocks"),
    ];
    NAMED
        .iter()
        .filter(|(bit, _)| r.contains(*bit))
        .map(|(_, name)| (*name).to_string())
        .collect()
}
