//! One-shot rendering of a [`Sample`]: a readable block for people, one JSON
//! line for status bars.

use overrune_core::Sample;

/// The stable machine contract: a single-line object. `fan_pct` is absent
/// (not null) when the device has no fan to read.
pub fn render_json(s: &Sample) -> String {
    let mut v = serde_json::to_value(s).expect("Sample serializes");
    if s.fan_pct.is_none() {
        v.as_object_mut().expect("object").remove("fan_pct");
    }
    v.to_string()
}

pub fn render_human(s: &Sample) -> String {
    let throttle = if s.throttle_reasons.is_empty() {
        "none".to_string()
    } else {
        s.throttle_reasons.join(", ")
    };
    let mut out = format!("{} ({}, driver {})\n", s.name, s.bus_id, s.driver_version);
    let mut row = |label: &str, value: String| out.push_str(&format!("  {label:<9}{value}\n"));
    row("State", format!("P{}", s.pstate));
    row("Temp", format!("{} °C", s.temp_c));
    row(
        "Clocks",
        format!(
            "{} MHz core   {} MHz mem",
            s.core_clock_mhz, s.mem_clock_mhz
        ),
    );
    row(
        "Power",
        format!("{:.1} W / {:.0} W limit", s.power_w, s.power_limit_w),
    );
    row(
        "Load",
        format!("{}% gpu   {}% mem", s.util_gpu_pct, s.util_mem_pct),
    );
    row(
        "VRAM",
        format!("{} / {} MB", s.vram_used_mb, s.vram_total_mb),
    );
    row(
        "Offsets",
        format!(
            "{:+} MHz core   {:+} MHz mem",
            s.core_offset_mhz, s.mem_offset_mhz
        ),
    );
    if let Some(fan) = s.fan_pct {
        row("Fan", format!("{fan}%"));
    }
    row("Throttle", throttle);
    out
}
