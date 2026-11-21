//! Brightness: WMI for internal panels, DDC/CI (dxva2) for external monitors.
//! Everything here is blocking and runs on transient worker threads, never on
//! the caller's thread nor on the message-loop thread.

use crate::Shared;
use nb_core::{Event, ScreenBrightness};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use windows::Win32::Devices::Display::{
    DestroyPhysicalMonitors, GetMonitorBrightness, GetNumberOfPhysicalMonitorsFromHMONITOR,
    GetPhysicalMonitorsFromHMONITOR, PHYSICAL_MONITOR, SetMonitorBrightness,
};
use windows::Win32::Foundation::{HANDLE, LPARAM, RECT, TRUE};
use windows::Win32::Graphics::Gdi::{EnumDisplayMonitors, HDC, HMONITOR};
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoSetProxyBlanket,
    EOAC_NONE, RPC_C_AUTHN_LEVEL_CALL, RPC_C_IMP_LEVEL_IMPERSONATE,
};
use windows::Win32::System::Rpc::{RPC_C_AUTHN_WINNT, RPC_C_AUTHZ_NONE};
use windows::Win32::System::Variant::{VARIANT, VT_UI1};
use windows::Win32::System::Wmi::{
    IWbemClassObject, IWbemLocator, IWbemServices, WBEM_FLAG_FORWARD_ONLY,
    WBEM_FLAG_RETURN_IMMEDIATELY, WBEM_INFINITE, WbemLocator,
};
use windows::core::{BSTR, w};

pub const UNKNOWN: u8 = 0;
pub const SUPPORTED: u8 = 1;
pub const UNSUPPORTED: u8 = 2;

pub struct Hub {
    /// UNKNOWN until the first probe completes.
    pub state: AtomicU8,
    /// Latest requested percentage per screen id, sent by the worker.
    pending: Mutex<Vec<(u32, u8)>>,
    worker: AtomicBool,
    read_requested: AtomicBool,
    /// Last known screens, and the DDC range of each external one.
    screens: Mutex<Vec<ScreenBrightness>>,
    ranges: Mutex<Vec<(u32, u32, u32)>>,
}

impl Default for Hub {
    fn default() -> Self {
        Hub {
            state: AtomicU8::new(UNKNOWN),
            pending: Mutex::new(Vec::new()),
            worker: AtomicBool::new(false),
            read_requested: AtomicBool::new(false),
            screens: Mutex::new(Vec::new()),
            ranges: Mutex::new(Vec::new()),
        }
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Scales a raw DDC value into 0..=100.
pub fn to_percent(min: u32, cur: u32, max: u32) -> u8 {
    if max <= min {
        return 0;
    }
    ((cur.saturating_sub(min)).min(max - min) as u64 * 100 / (max - min) as u64) as u8
}

/// Scales 0..=100 into the raw DDC range.
pub fn from_percent(min: u32, max: u32, pct: u8) -> u32 {
    if max <= min {
        return min;
    }
    min + ((max - min) as u64 * pct.min(100) as u64 / 100) as u32
}

/// Starts (or reuses) the single transient worker. It exits when idle.
fn kick(shared: &Arc<Shared>) {
    let hub = &shared.brightness;
    if hub.worker.swap(true, Ordering::AcqRel) {
        return;
    }
    let shared_w = shared.clone();
    let spawned = std::thread::Builder::new().name("nb-brightness".into()).spawn(move || {
        // SAFETY: plain COM init for this worker thread.
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        }
        let shared = shared_w;
        let hub = &shared.brightness;
        loop {
            let jobs = std::mem::take(&mut *lock(&hub.pending));
            if !jobs.is_empty() {
                for (id, pct) in jobs {
                    if set_one(hub, id, pct).is_ok() {
                        let mut list = lock(&hub.screens).clone();
                        if let Some(s) = list.iter_mut().find(|s| s.id == id) {
                            s.percent = pct;
                        }
                        publish(&shared, list);
                    }
                }
                continue;
            }
            if hub.read_requested.swap(false, Ordering::AcqRel) {
                let list = read_all(hub);
                hub.state.store(if list.is_empty() { UNSUPPORTED } else { SUPPORTED }, Ordering::Release);
                publish(&shared, list);
                continue;
            }
            hub.worker.store(false, Ordering::Release);
            // A request may have raced in between the checks and the store.
            if (!lock(&hub.pending).is_empty() || hub.read_requested.load(Ordering::Acquire))
                && !hub.worker.swap(true, Ordering::AcqRel)
            {
                continue;
            }
            break;
        }
    });
    if spawned.is_err() {
        shared.brightness.worker.store(false, Ordering::Release);
    }
}

fn publish(shared: &Shared, list: Vec<ScreenBrightness>) {
    let mut last = lock(&shared.brightness.screens);
    if *last == list {
        return;
    }
    let first_changed = last.first().map(|s| s.percent) != list.first().map(|s| s.percent);
    *last = list.clone();
    drop(last);
    if let Some(first) = list.first().filter(|_| first_changed) {
        (shared.sink)(Event::Brightness(first.percent));
    }
    (shared.sink)(Event::Screens(list));
}

pub fn request_read(shared: &Arc<Shared>) {
    shared.brightness.read_requested.store(true, Ordering::Release);
    kick(shared);
}

/// Sets one screen; calls made during a drag coalesce to the latest value.
pub fn request_set(shared: &Arc<Shared>, id: u32, pct: u8) {
    {
        let mut p = lock(&shared.brightness.pending);
        p.retain(|(i, _)| *i != id);
        p.push((id, pct.min(100)));
    }
    kick(shared);
}

fn read_all(hub: &Hub) -> Vec<ScreenBrightness> {
    let mut list = Vec::new();
    if let Some(p) = wmi_read() {
        list.push(ScreenBrightness { id: 0, name: "PC".into(), percent: p });
    }
    let mut ranges = Vec::new();
    each_physical(|pos, h, desc| {
        let (mut min, mut cur, mut max) = (0u32, 0u32, 0u32);
        // SAFETY: valid out pointers; handle valid until DestroyPhysicalMonitors.
        if unsafe { GetMonitorBrightness(h, &mut min, &mut cur, &mut max) } != 0 {
            let id = pos as u32 + 1;
            ranges.push((id, min, max));
            list.push(ScreenBrightness { id, name: monitor_name(desc), percent: to_percent(min, cur, max) });
        }
    });
    *lock(&hub.ranges) = ranges;
    list
}

/// "CF390_C24F390FH / C24F392FH (HDMI)" becomes "CF390".
fn monitor_name(desc: &str) -> String {
    let first = desc.split(['/', '(']).next().unwrap_or("").trim();
    let short = first.split('_').next().unwrap_or(first).trim();
    if short.is_empty() || short.eq_ignore_ascii_case("Generic PnP Monitor") { "Écran".into() } else { short.chars().take(14).collect() }
}

fn set_one(hub: &Hub, id: u32, pct: u8) -> Result<(), ()> {
    if id == 0 {
        return wmi_set(pct).map_err(|_| ());
    }
    let range = lock(&hub.ranges).iter().find(|r| r.0 == id).map(|r| (r.1, r.2));
    let mut ok = false;
    each_physical(|pos, h, _| {
        if pos as u32 + 1 != id {
            return;
        }
        let (mut min, mut cur, mut max) = (0u32, 0u32, 0u32);
        // SAFETY: valid handle and out pointers.
        unsafe {
            let (lo, hi) = match range {
                Some(r) => r,
                None if GetMonitorBrightness(h, &mut min, &mut cur, &mut max) != 0 => (min, max),
                None => return,
            };
            ok = SetMonitorBrightness(h, from_percent(lo, hi, pct)) != 0;
        }
    });
    if ok { Ok(()) } else { Err(()) }
}

// ---------------------------------------------------------------- WMI

fn wmi_connect() -> windows::core::Result<IWbemServices> {
    // SAFETY: standard WMI connection sequence, all pointers owned by COM wrappers.
    unsafe {
        let loc: IWbemLocator = CoCreateInstance(&WbemLocator, None, CLSCTX_INPROC_SERVER)?;
        let svc = loc.ConnectServer(
            &BSTR::from("ROOT\\WMI"),
            &BSTR::new(),
            &BSTR::new(),
            &BSTR::new(),
            0,
            &BSTR::new(),
            None,
        )?;
        CoSetProxyBlanket(
            &svc,
            RPC_C_AUTHN_WINNT,
            RPC_C_AUTHZ_NONE,
            None,
            RPC_C_AUTHN_LEVEL_CALL,
            RPC_C_IMP_LEVEL_IMPERSONATE,
            None,
            EOAC_NONE,
        )?;
        Ok(svc)
    }
}

fn wmi_first(svc: &IWbemServices, class: &str) -> Option<IWbemClassObject> {
    // SAFETY: COM calls with owned wrappers; `returned` bounds the slice use.
    unsafe {
        let q = BSTR::from(format!("SELECT * FROM {class} WHERE Active=TRUE"));
        let e = svc
            .ExecQuery(
                &BSTR::from("WQL"),
                &q,
                WBEM_FLAG_FORWARD_ONLY | WBEM_FLAG_RETURN_IMMEDIATELY,
                None,
            )
            .ok()?;
        let mut objs: [Option<IWbemClassObject>; 1] = [None];
        let mut returned = 0u32;
        let _ = e.Next(WBEM_INFINITE, &mut objs, &mut returned);
        if returned == 0 { None } else { objs[0].take() }
    }
}

fn wmi_read() -> Option<u8> {
    let svc = wmi_connect().ok()?;
    let obj = wmi_first(&svc, "WmiMonitorBrightness")?;
    let mut v = VARIANT::default();
    // SAFETY: valid out pointer; CurrentBrightness is a uint8 (VT_UI1).
    unsafe {
        obj.Get(w!("CurrentBrightness"), 0, &mut v, None, None).ok()?;
        let inner = &v.Anonymous.Anonymous;
        if inner.vt == VT_UI1 { Some(inner.Anonymous.bVal.min(100)) } else { None }
    }
}

fn wmi_set(pct: u8) -> windows::core::Result<()> {
    let svc = wmi_connect()?;
    let missing = || windows::core::Error::from(windows::Win32::Foundation::E_NOTIMPL);
    let inst = wmi_first(&svc, "WmiMonitorBrightnessMethods").ok_or_else(missing)?;
    // SAFETY: COM calls with owned wrappers.
    unsafe {
        let mut path = VARIANT::default();
        inst.Get(w!("__PATH"), 0, &mut path, None, None)?;
        let path = BSTR::from(path.Anonymous.Anonymous.Anonymous.bstrVal.to_string());
        let mut class = None;
        svc.GetObject(&BSTR::from("WmiMonitorBrightnessMethods"), Default::default(), None, Some(&mut class), None)?;
        let class = class.ok_or_else(missing)?;
        let mut in_sig: Option<IWbemClassObject> = None;
        class.GetMethod(w!("WmiSetBrightness"), 0, &mut in_sig, std::ptr::null_mut())?;
        let params = in_sig.ok_or_else(missing)?.SpawnInstance(0)?;
        params.Put(w!("Timeout"), 0, &VARIANT::from(0i32), 0)?;
        params.Put(w!("Brightness"), 0, &VARIANT::from(pct as i32), 0)?;
        svc.ExecMethod(&path, &BSTR::from("WmiSetBrightness"), Default::default(), None, &params, None, None)?;
    }
    Ok(())
}

// ---------------------------------------------------------------- DDC/CI

unsafe extern "system" fn collect(h: HMONITOR, _: HDC, _: *mut RECT, data: LPARAM) -> windows::core::BOOL {
    // SAFETY: `data` is the address of the Vec passed to EnumDisplayMonitors below.
    let v = unsafe { &mut *(data.0 as *mut Vec<HMONITOR>) };
    v.push(h);
    TRUE
}

/// Runs `f` on every physical monitor, with its position across all screens.
fn each_physical(mut f: impl FnMut(usize, HANDLE, &str)) {
    let mut mons: Vec<HMONITOR> = Vec::new();
    // SAFETY: the callback only touches the Vec, which outlives the call.
    unsafe {
        let _ = EnumDisplayMonitors(None, None, Some(collect), LPARAM(&mut mons as *mut _ as isize));
    }
    let mut pos = 0;
    for m in mons {
        // SAFETY: arrays are sized by the API; handles destroyed after use.
        unsafe {
            let mut n = 0u32;
            if GetNumberOfPhysicalMonitorsFromHMONITOR(m, &mut n).is_err() || n == 0 {
                continue;
            }
            let mut phys = vec![PHYSICAL_MONITOR::default(); n as usize];
            if GetPhysicalMonitorsFromHMONITOR(m, &mut phys).is_err() {
                continue;
            }
            for p in &phys {
                let d = p.szPhysicalMonitorDescription;
                let len = d.iter().position(|c| *c == 0).unwrap_or(d.len());
                f(pos, p.hPhysicalMonitor, &String::from_utf16_lossy(&d[..len]));
                pos += 1;
            }
            let _ = DestroyPhysicalMonitors(&phys);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percent_scaling() {
        assert_eq!(to_percent(0, 50, 100), 50);
        assert_eq!(to_percent(10, 60, 110), 50);
        assert_eq!(to_percent(0, 0, 0), 0);
        assert_eq!(to_percent(0, 500, 100), 100);
        assert_eq!(from_percent(0, 100, 30), 30);
        assert_eq!(from_percent(10, 110, 50), 60);
        assert_eq!(from_percent(5, 5, 50), 5);
    }

    #[test]
    fn short_monitor_names() {
        assert_eq!(monitor_name("CF390_C24F390FH / C24F392FH / C24F394FH (HDMI)"), "CF390");
        assert_eq!(monitor_name("Generic PnP Monitor"), "Écran");
        assert_eq!(monitor_name(""), "Écran");
    }
}
