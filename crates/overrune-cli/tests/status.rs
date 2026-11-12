use std::process::Command;

use overrune_cli::status::{render_human, render_json};
use overrune_core::Sample;
use serde_json::Value;

fn sample(fan_pct: Option<u32>) -> Sample {
    Sample {
        name: "NVIDIA GeForce RTX 2070 with Max-Q Design".into(),
        bus_id: "00000000:01:00.0".into(),
        driver_version: "620.70.06".into(),
        core_clock_mhz: 1350,
        mem_clock_mhz: 5001,
        temp_c: 61,
        power_w: 42.5,
        power_limit_w: 80.0,
        util_gpu_pct: 37,
        util_mem_pct: 12,
        vram_used_mb: 812,
        vram_total_mb: 7982,
        pstate: 2,
        core_offset_mhz: -50,
        mem_offset_mhz: 100,
        fan_pct,
        throttle_reasons: vec!["power limit".into()],
    }
}

#[test]
fn json_omits_fan_speed_when_the_device_reports_none() {
    let v: Value = serde_json::from_str(&render_json(&sample(None))).unwrap();
    assert!(v.as_object().unwrap().get("fan_pct").is_none());
}

#[test]
fn json_includes_fan_speed_when_present() {
    let v: Value = serde_json::from_str(&render_json(&sample(Some(55)))).unwrap();
    assert_eq!(v["fan_pct"], 55);
}

#[test]
fn json_is_one_object_on_one_line() {
    let s = render_json(&sample(None));
    assert!(!s.contains('\n'));
    assert!(serde_json::from_str::<Value>(&s).unwrap().is_object());
}

#[test]
fn human_output_names_the_device_and_temperature() {
    let out = render_human(&sample(None));
    assert!(out.contains("RTX 2070"));
    assert!(out.contains("61 °C"));
    assert!(out.contains("42.5 W / 80 W"));
    assert!(out.contains("-50 MHz core"));
    assert!(!out.contains("Fan"));
    assert!(render_human(&sample(Some(55))).contains("55%"));
}

#[test]
fn status_succeeds_with_no_daemon_socket() {
    if overrune_core::Gpu::open().is_err() {
        return; // `status` reads real hardware. CI runners have no device.
    }
    let bin = env!("CARGO_BIN_EXE_overrune");
    for args in [&["--json"][..], &[][..], &["json"][..]] {
        let out = Command::new(bin)
            .args(args)
            .env("OVERRUNE_SOCKET", "/nonexistent/overrune.sock")
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(0), "args {args:?}");
    }
    let out = Command::new(bin)
        .arg("--json")
        .env("OVERRUNE_SOCKET", "/nonexistent/overrune.sock")
        .output()
        .unwrap();
    assert!(serde_json::from_slice::<Value>(&out.stdout).is_ok());
}
