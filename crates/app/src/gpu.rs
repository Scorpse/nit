//! Graphics adapter choice (audit step 9). eframe's default picks the first adapter that matches
//! `HighPerformance`, which on this project's Windows machines was Vulkan: measured 1 to 3 % of a
//! core while idle against 0 with DX12, and it wakes the discrete GPU of a laptop to draw text.
//!
//! This picks from the adapters wgpu already enumerated (Vulkan, DX12, GL), so a missing DX12
//! adapter falls back to the others and a machine with no usable GPU (an RDP session, an old
//! driver) ends on the software adapter (WARP, llvmpipe). A second start with
//! `force_fallback_adapter` is not possible, because winit allows one event loop per process;
//! ranking the software adapter last gives the same result without it.
//!
//! The environment overrides of egui-wgpu keep working: `WGPU_BACKEND` restricts the
//! enumeration (and turns the DX12 preference off), `WGPU_POWER_PREF` replaces the AC / battery
//! rule.

use std::sync::OnceLock;

use eframe::egui_wgpu::{self, wgpu};
use wgpu::{Backend, DeviceType, PowerPreference};

/// "<adapter> (<backend>)" of the adapter in use, set once at start-up.
static ADAPTER: OnceLock<String> = OnceLock::new();

/// What the Settings screen shows after "Graphics: ".
pub fn adapter_line() -> &'static str {
    ADAPTER.get().map_or("not started", String::as_str)
}

/// One enumerated adapter, reduced to what the ranking needs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cand {
    pub backend: Backend,
    pub device_type: DeviceType,
    /// Can present to the window's surface.
    pub supported: bool,
}

/// `None`: unusable. Higher is better. Hardware always beats software; then DX12 (when
/// preferred); then the device type that suits the power preference.
fn score(c: &Cand, power: PowerPreference, prefer_dx12: bool) -> Option<u32> {
    if !c.supported {
        return None;
    }
    let type_rank = match (power, c.device_type) {
        (PowerPreference::LowPower, DeviceType::IntegratedGpu) => 4,
        (PowerPreference::LowPower, DeviceType::DiscreteGpu) => 3,
        (_, DeviceType::DiscreteGpu) => 4,
        (_, DeviceType::IntegratedGpu) => 3,
        (_, DeviceType::VirtualGpu) => 2,
        (_, DeviceType::Other) => 1,
        (_, DeviceType::Cpu) => 0,
    };
    let hardware = u32::from(c.device_type != DeviceType::Cpu);
    let dx12 = u32::from(prefer_dx12 && c.backend == Backend::Dx12);
    // the first adapter of an equal rank wins (enumeration order), so no tie-break term
    Some(hardware * 100 + dx12 * 10 + type_rank)
}

/// Index of the best adapter, or None when none can present.
pub fn pick(cands: &[Cand], power: PowerPreference, prefer_dx12: bool) -> Option<usize> {
    let mut best: Option<(usize, u32)> = None;
    for (i, c) in cands.iter().enumerate() {
        if let Some(s) = score(c, power, prefer_dx12) {
            if best.is_none_or(|(_, b)| s > b) {
                best = Some((i, s));
            }
        }
    }
    best.map(|(i, _)| i)
}

/// AC power or no battery: the speed the owner asked for. On battery: the integrated GPU.
/// `ac_line`: 0 battery, 1 AC, 255 unknown (Win32 `ACLineStatus`); `battery_flag` bit 128: no
/// battery (`BatteryFlag`). Windows only reads these (see `system_power`).
#[cfg_attr(not(windows), allow(dead_code))]
pub fn power_for_status(ac_line: u8, battery_flag: u8) -> PowerPreference {
    if ac_line == 0 && battery_flag & 128 == 0 {
        PowerPreference::LowPower
    } else {
        PowerPreference::HighPerformance
    }
}

#[cfg(windows)]
fn system_power() -> PowerPreference {
    use windows_sys::Win32::System::Power::{GetSystemPowerStatus, SYSTEM_POWER_STATUS};
    // SAFETY: a plain out-parameter struct, fully written on success.
    let mut s: SYSTEM_POWER_STATUS = unsafe { std::mem::zeroed() };
    if unsafe { GetSystemPowerStatus(&mut s) } == 0 {
        return PowerPreference::HighPerformance;
    }
    power_for_status(s.ACLineStatus, s.BatteryFlag)
}

/// Off Windows the battery state is not queried (unknown): low power, the audit's default (no
/// discrete-GPU wake-up; a frame is under 4 ms even on an integrated GPU). `WGPU_POWER_PREF`
/// still overrides it.
#[cfg(not(windows))]
fn system_power() -> PowerPreference {
    PowerPreference::LowPower
}

/// Power preference after the `WGPU_POWER_PREF` override (`none` counts as high).
fn effective_power() -> PowerPreference {
    match PowerPreference::from_env() {
        Some(PowerPreference::None) => PowerPreference::HighPerformance,
        Some(p) => p,
        None => system_power(),
    }
}

/// The wgpu configuration for `NativeOptions`: the defaults plus the adapter selector above.
pub fn configuration() -> egui_wgpu::WgpuConfiguration {
    let mut setup = egui_wgpu::WgpuSetupCreateNew::without_display_handle();
    // an explicit backend choice is the owner's: do not second-guess it
    let prefer_dx12 = cfg!(windows) && wgpu::Backends::from_env().is_none();
    let power = effective_power();
    setup.power_preference = power;
    setup.native_adapter_selector = Some(std::sync::Arc::new(move |adapters, surface| {
        let cands: Vec<Cand> = adapters
            .iter()
            .map(|a| {
                let info = a.get_info();
                Cand {
                    backend: info.backend,
                    device_type: info.device_type,
                    supported: surface.is_none_or(|s| a.is_surface_supported(s)),
                }
            })
            .collect();
        pick(&cands, power, prefer_dx12)
            .map(|i| adapters[i].clone())
            .ok_or_else(|| {
                format!(
                    "none of {} graphics adapters can draw the window",
                    adapters.len()
                )
            })
    }));
    egui_wgpu::WgpuConfiguration {
        wgpu_setup: egui_wgpu::WgpuSetup::CreateNew(setup),
        ..Default::default()
    }
}

/// Records the adapter eframe ended up with (stderr once, and for the Settings screen); true
/// when it is a software one, so the caller can say so in the window title.
pub fn record(state: Option<&egui_wgpu::RenderState>) -> bool {
    let Some(state) = state else {
        return false;
    };
    let info = state.adapter.get_info();
    let line = format!("{} ({:?})", info.name, info.backend);
    if ADAPTER.set(line.clone()).is_ok() {
        eprintln!("graphics: {line}, {:?}", info.device_type);
    }
    info.device_type == DeviceType::Cpu
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(backend: Backend, device_type: DeviceType) -> Cand {
        Cand {
            backend,
            device_type,
            supported: true,
        }
    }

    const HIGH: PowerPreference = PowerPreference::HighPerformance;
    const LOW: PowerPreference = PowerPreference::LowPower;

    #[test]
    fn dx12_beats_vulkan_on_the_same_gpu() {
        let l = [
            c(Backend::Vulkan, DeviceType::DiscreteGpu),
            c(Backend::Dx12, DeviceType::DiscreteGpu),
            c(Backend::Gl, DeviceType::DiscreteGpu),
        ];
        assert_eq!(pick(&l, HIGH, true), Some(1));
        assert_eq!(pick(&l, HIGH, false), Some(0), "enumeration order");
    }

    #[test]
    fn dx12_missing_falls_back_to_the_other_backends() {
        let l = [
            c(Backend::Gl, DeviceType::DiscreteGpu),
            c(Backend::Vulkan, DeviceType::DiscreteGpu),
        ];
        assert_eq!(pick(&l, HIGH, true), Some(0));
    }

    #[test]
    fn power_preference_picks_the_device_type() {
        let l = [
            c(Backend::Dx12, DeviceType::DiscreteGpu),
            c(Backend::Dx12, DeviceType::IntegratedGpu),
        ];
        assert_eq!(pick(&l, HIGH, true), Some(0));
        assert_eq!(pick(&l, LOW, true), Some(1));
    }

    #[test]
    fn software_is_the_last_resort_not_a_dx12_win() {
        let warp = c(Backend::Dx12, DeviceType::Cpu);
        let vk = c(Backend::Vulkan, DeviceType::IntegratedGpu);
        assert_eq!(pick(&[warp, vk], HIGH, true), Some(1), "hardware first");
        assert_eq!(pick(&[warp], HIGH, true), Some(0), "software when alone");
    }

    #[test]
    fn unsupported_adapters_are_skipped_and_none_means_none() {
        let mut bad = c(Backend::Dx12, DeviceType::DiscreteGpu);
        bad.supported = false;
        let ok = c(Backend::Gl, DeviceType::Other);
        assert_eq!(pick(&[bad, ok], HIGH, true), Some(1));
        assert_eq!(pick(&[bad], HIGH, true), None);
        assert_eq!(pick(&[], HIGH, true), None);
    }

    #[test]
    fn battery_means_low_power_ac_or_no_battery_means_high() {
        assert_eq!(power_for_status(0, 1), LOW, "on battery");
        assert_eq!(power_for_status(1, 8), HIGH, "plugged in");
        assert_eq!(power_for_status(255, 255), HIGH, "unknown");
        assert_eq!(power_for_status(0, 128), HIGH, "no battery");
    }
}
