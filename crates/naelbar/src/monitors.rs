//! Connected displays.

use windows::Win32::Foundation::{LPARAM, RECT, TRUE};
use windows::Win32::Graphics::Gdi::{EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORINFO, MONITORINFOEXW};
use windows::core::BOOL;

const MONITORINFOF_PRIMARY: u32 = 1;

#[derive(Clone, Debug)]
pub struct Monitor {
    pub handle: HMONITOR,
    /// Stable device name such as `\\.\DISPLAY1`.
    pub name: String,
    pub rect: RECT,
    pub primary: bool,
}

unsafe extern "system" fn collect(handle: HMONITOR, _dc: HDC, _clip: *mut RECT, out: LPARAM) -> BOOL {
    let list = &mut *(out.0 as *mut Vec<Monitor>);
    let mut info = MONITORINFOEXW::default();
    info.monitorInfo.cbSize = size_of::<MONITORINFOEXW>() as u32;
    if GetMonitorInfoW(handle, &mut info as *mut MONITORINFOEXW as *mut MONITORINFO).as_bool() {
        let len = info.szDevice.iter().position(|c| *c == 0).unwrap_or(info.szDevice.len());
        list.push(Monitor {
            handle,
            name: String::from_utf16_lossy(&info.szDevice[..len]),
            rect: info.monitorInfo.rcMonitor,
            primary: info.monitorInfo.dwFlags & MONITORINFOF_PRIMARY != 0,
        });
    }
    TRUE
}

pub fn list() -> Vec<Monitor> {
    let mut out: Vec<Monitor> = Vec::new();
    unsafe {
        let _ = EnumDisplayMonitors(None, None, Some(collect), LPARAM(&mut out as *mut Vec<Monitor> as isize));
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// The display named `name`, or the primary one when it is unplugged or empty.
pub fn pick(name: &str) -> Option<Monitor> {
    let all = list();
    all.iter().find(|m| !name.is_empty() && m.name == name).or_else(|| all.iter().find(|m| m.primary)).or(all.first()).cloned()
}
