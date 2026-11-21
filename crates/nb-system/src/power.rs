//! Battery and power mode (overlay scheme) mapping and reads.

use nb_core::{PowerMode, PowerState};
use windows::Win32::Foundation::{HLOCAL, LocalFree};
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
use windows::Win32::System::Power::{GetSystemPowerStatus, SYSTEM_POWER_STATUS};
use windows::core::{GUID, w, s};

type GetOverlayFn = unsafe extern "system" fn(*mut GUID) -> u32;
type SetOverlayFn = unsafe extern "system" fn(GUID) -> u32;
type GetActiveFn = unsafe extern "system" fn(isize, *mut *mut GUID) -> u32;
type SetActiveFn = unsafe extern "system" fn(isize, *const GUID) -> u32;

/// The overlay-scheme exports of powrprof.dll are not in the `windows` crate,
/// so they are resolved at runtime.
fn powrprof_fn(name: windows::core::PCSTR) -> Option<unsafe extern "system" fn() -> isize> {
    // SAFETY: powrprof.dll stays loaded for the process lifetime.
    unsafe {
        let m = LoadLibraryW(w!("powrprof.dll")).ok()?;
        GetProcAddress(m, name)
    }
}

pub const SCHEME_BALANCED: GUID = GUID::from_u128(0x381b4222_f694_41f0_9685_ff5bb260df2e);
pub const SCHEME_POWER_SAVER: GUID = GUID::from_u128(0xa1841308_3541_4fab_bc81_f71556f20b4a);
pub const SCHEME_HIGH_PERFORMANCE: GUID = GUID::from_u128(0x8c5e7fda_e8bf_4a96_9a85_a6e23a8c635c);
pub const SCHEME_ULTIMATE: GUID = GUID::from_u128(0xe9a42b02_d5df_448d_aa00_03f14749eb61);
/// Power setting notified when the active plan changes.
pub const GUID_ACTIVE_PLAN: GUID = GUID::from_u128(0x31f9f286_5084_42fe_b720_2b0264993763);

pub const OVERLAY_BETTER_BATTERY: GUID = GUID::from_u128(0x961cc777_2547_4f9d_8174_7d86181b8a7a);
pub const OVERLAY_BALANCED: GUID = GUID::zeroed();
pub const OVERLAY_BETTER_PERFORMANCE: GUID =
    GUID::from_u128(0x3af9b8d9_7c97_431d_ad78_34a8bfea439f);
pub const OVERLAY_MAX_PERFORMANCE: GUID = GUID::from_u128(0xded574b5_45a0_4f42_8737_46345c09c238);

/// Maps an effective overlay scheme GUID to a [`PowerMode`]. Unknown GUIDs
/// (OEM overlays) count as balanced.
pub fn mode_from_guid(g: &GUID) -> PowerMode {
    if *g == OVERLAY_BETTER_BATTERY {
        PowerMode::Efficiency
    } else if *g == OVERLAY_BETTER_PERFORMANCE || *g == OVERLAY_MAX_PERFORMANCE {
        PowerMode::Performance
    } else {
        PowerMode::Balanced
    }
}

/// Windows applies overlays only on top of the Balanced plan, so a classic
/// plan decides the mode on its own.
pub fn mode_from_schemes(base: &GUID, overlay: &GUID) -> PowerMode {
    if *base == SCHEME_HIGH_PERFORMANCE || *base == SCHEME_ULTIMATE {
        PowerMode::Performance
    } else if *base == SCHEME_POWER_SAVER {
        PowerMode::Efficiency
    } else {
        mode_from_guid(overlay)
    }
}

/// Whether reaching `target` requires leaving a classic plan first, since an
/// overlay set on it would have no effect.
pub fn must_switch_to_balanced(base: &GUID, target: PowerMode) -> bool {
    *base != SCHEME_BALANCED && mode_from_schemes(base, &OVERLAY_BALANCED) != target
}

/// The overlay GUID that `set_power_mode` activates for a mode.
pub fn guid_for_mode(m: PowerMode) -> GUID {
    match m {
        PowerMode::Efficiency => OVERLAY_BETTER_BATTERY,
        PowerMode::Balanced => OVERLAY_BALANCED,
        PowerMode::Performance => OVERLAY_BETTER_PERFORMANCE,
    }
}

/// Pure conversion from `SYSTEM_POWER_STATUS` fields.
pub fn state_from_status(ac_line: u8, flag: u8, percent: u8, mode: PowerMode) -> PowerState {
    // BatteryFlag 128 = no system battery, 255 = unknown. Percent 255 = unknown.
    let has_battery = flag != 128 && flag != 255 && percent != 255;
    PowerState {
        battery_percent: has_battery.then_some(percent.min(100)),
        charging: has_battery && (flag & 8 != 0 || ac_line == 1),
        mode,
        seconds_left: None,
    }
}

/// `SYSTEM_POWER_STATUS.BatteryLifeTime`: `u32::MAX` means unknown, which is
/// also what Windows reports while on AC power.
pub fn seconds_left(lifetime: u32, state: &PowerState) -> Option<u32> {
    (state.battery_percent.is_some() && !state.charging && lifetime != u32::MAX && lifetime > 0).then_some(lifetime)
}

fn effective_overlay() -> Option<GUID> {
    let f = powrprof_fn(s!("PowerGetEffectiveOverlayScheme"))?;
    let mut g = GUID::zeroed();
    // SAFETY: the export has this exact signature; valid out pointer.
    let rc = unsafe { std::mem::transmute::<_, GetOverlayFn>(f)(&mut g) };
    (rc == 0).then_some(g)
}

fn active_scheme() -> Option<GUID> {
    let f = powrprof_fn(s!("PowerGetActiveScheme"))?;
    let mut p: *mut GUID = std::ptr::null_mut();
    // SAFETY: the export has this exact signature; a null root key selects the
    // system store, and on success `p` is a LocalAlloc'd GUID that we free.
    unsafe {
        if std::mem::transmute::<_, GetActiveFn>(f)(0, &mut p) != 0 || p.is_null() {
            return None;
        }
        let g = *p;
        let _ = LocalFree(Some(HLOCAL(p.cast())));
        Some(g)
    }
}

pub fn read_mode() -> PowerMode {
    let overlay = effective_overlay().unwrap_or(OVERLAY_BALANCED);
    match active_scheme() {
        Some(base) => mode_from_schemes(&base, &overlay),
        None => mode_from_guid(&overlay),
    }
}

pub fn read_state() -> PowerState {
    let mut s = SYSTEM_POWER_STATUS::default();
    // SAFETY: valid out pointer.
    let ok = unsafe { GetSystemPowerStatus(&mut s) }.is_ok();
    let mode = read_mode();
    if !ok {
        return state_from_status(255, 128, 255, mode);
    }
    let mut st = state_from_status(s.ACLineStatus, s.BatteryFlag, s.BatteryLifePercent, mode);
    st.seconds_left = seconds_left(s.BatteryLifeTime, &st);
    st
}

pub fn set_mode(m: PowerMode) -> Result<(), u32> {
    if active_scheme().is_some_and(|base| must_switch_to_balanced(&base, m)) {
        let f = powrprof_fn(s!("PowerSetActiveScheme")).ok_or(2u32)?;
        // SAFETY: the export has this exact signature; the GUID outlives the call.
        match unsafe { std::mem::transmute::<_, SetActiveFn>(f)(0, &SCHEME_BALANCED) } {
            0 => {}
            e => return Err(e),
        }
    }
    let f = powrprof_fn(s!("PowerSetActiveOverlayScheme")).ok_or(2u32)?;
    // SAFETY: the export has this exact signature.
    match unsafe { std::mem::transmute::<_, SetOverlayFn>(f)(guid_for_mode(m)) } {
        0 => Ok(()),
        e => Err(e),
    }
}

pub fn same(a: &PowerState, b: &PowerState) -> bool {
    a.battery_percent == b.battery_percent && a.charging == b.charging && a.mode == b.mode && a.seconds_left == b.seconds_left
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guid_mapping_roundtrip() {
        for m in [PowerMode::Efficiency, PowerMode::Balanced, PowerMode::Performance] {
            assert_eq!(mode_from_guid(&guid_for_mode(m)), m);
        }
    }

    #[test]
    fn max_performance_and_unknown() {
        assert_eq!(mode_from_guid(&OVERLAY_MAX_PERFORMANCE), PowerMode::Performance);
        assert_eq!(mode_from_guid(&GUID::from_u128(0x1234)), PowerMode::Balanced);
    }

    #[test]
    fn classic_plan_wins_over_overlay() {
        let m = mode_from_schemes(&SCHEME_HIGH_PERFORMANCE, &OVERLAY_BALANCED);
        assert_eq!(m, PowerMode::Performance);
        let m = mode_from_schemes(&SCHEME_POWER_SAVER, &OVERLAY_BETTER_PERFORMANCE);
        assert_eq!(m, PowerMode::Efficiency);
        let m = mode_from_schemes(&SCHEME_BALANCED, &OVERLAY_BETTER_BATTERY);
        assert_eq!(m, PowerMode::Efficiency);
    }

    #[test]
    fn leaves_classic_plan_only_when_needed() {
        assert!(!must_switch_to_balanced(&SCHEME_HIGH_PERFORMANCE, PowerMode::Performance));
        assert!(must_switch_to_balanced(&SCHEME_HIGH_PERFORMANCE, PowerMode::Balanced));
        assert!(must_switch_to_balanced(&SCHEME_POWER_SAVER, PowerMode::Performance));
        assert!(!must_switch_to_balanced(&SCHEME_BALANCED, PowerMode::Performance));
    }

    #[test]
    fn guid_values() {
        assert_eq!(OVERLAY_BETTER_BATTERY.to_u128(), 0x961cc777_2547_4f9d_8174_7d86181b8a7a);
        assert_eq!(OVERLAY_BALANCED, GUID::zeroed());
    }

    #[test]
    fn status_no_battery() {
        let s = state_from_status(1, 128, 255, PowerMode::Balanced);
        assert_eq!(s.battery_percent, None);
        assert!(!s.charging);
    }

    #[test]
    fn time_left_only_on_battery_and_when_known() {
        let on_battery = state_from_status(0, 1, 50, PowerMode::Balanced);
        assert_eq!(seconds_left(7200, &on_battery), Some(7200));
        assert_eq!(seconds_left(u32::MAX, &on_battery), None);
        let charging = state_from_status(1, 9, 50, PowerMode::Balanced);
        assert_eq!(seconds_left(7200, &charging), None);
    }

    #[test]
    fn status_charging_and_discharging() {
        let s = state_from_status(1, 9, 80, PowerMode::Balanced);
        assert_eq!(s.battery_percent, Some(80));
        assert!(s.charging);
        let s = state_from_status(0, 1, 42, PowerMode::Efficiency);
        assert!(!s.charging);
        assert_eq!(s.mode, PowerMode::Efficiency);
    }
}
