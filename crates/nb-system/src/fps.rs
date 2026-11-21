//! Frame rate of one process from a real-time ETW session, the way PresentMon
//! does it: `Microsoft-Windows-DXGI` Present_Start (id 42) and
//! `Microsoft-Windows-D3D9` Present_Start (id 1), filtered to a process id,
//! counted over a sliding one second window.
//!
//! The session exists only while an [`FpsSession`] is alive: one consumer
//! thread blocked in `ProcessTrace`, stopped and joined on drop. Starting it
//! needs administrator rights or membership of the "Performance Log Users"
//! group; otherwise [`FpsSession::start`] returns `ERROR_ACCESS_DENIED` (5).

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use windows::Win32::Foundation::{ERROR_ALREADY_EXISTS, ERROR_SUCCESS};
use windows::Win32::System::Diagnostics::Etw::*;
use windows::core::{GUID, PCWSTR, PWSTR};

/// Present events closer than this to the newest one are counted (100 ns units).
const WINDOW: i64 = 10_000_000;
/// No present for this long (wall clock, 100 ns units) means no rate to show.
const STALE: i64 = 25_000_000;

const DXGI: GUID = GUID::from_u128(0xca11c036_0102_4a2d_a6ad_f03cfed5d3c9);
const D3D9: GUID = GUID::from_u128(0x783aca0a_790e_4d7f_8451_aa850511c6b9);
const DXGI_PRESENT_START: u16 = 42;
const D3D9_PRESENT_START: u16 = 1;

/// Win32 error code for "access denied", the result without the group.
pub const ACCESS_DENIED: u32 = 5;

const SESSION_NAME: &str = "naelbar-fps";

/// Present timestamps of one process over the last second.
#[derive(Default, Debug)]
pub struct FpsWindow {
    times: VecDeque<i64>,
}

impl FpsWindow {
    /// Adds a present at `t` (100 ns units). Out-of-order events are kept in
    /// order; anything older than one second before the newest is dropped.
    pub fn push(&mut self, t: i64) {
        let at = self.times.iter().rposition(|&x| x <= t).map_or(0, |i| i + 1);
        self.times.insert(at, t);
        let newest = *self.times.back().unwrap_or(&t);
        while self.times.front().is_some_and(|&f| newest - f > WINDOW) {
            self.times.pop_front();
        }
    }

    pub fn clear(&mut self) {
        self.times.clear();
    }

    /// Frames per second over the window, or `None` with fewer than two
    /// presents or when the newest one is older than 2.5 s at `now`.
    pub fn fps(&self, now: i64) -> Option<f32> {
        let (first, last) = (*self.times.front()?, *self.times.back()?);
        if self.times.len() < 2 || now - last > STALE || last <= first {
            return None;
        }
        Some((self.times.len() - 1) as f32 * 1e7 / (last - first) as f32)
    }
}

struct Shared {
    pid: AtomicU32,
    window: Mutex<FpsWindow>,
}

#[repr(C)]
struct Props {
    p: EVENT_TRACE_PROPERTIES,
    name: [u16; 64],
}

impl Props {
    fn new() -> Box<Self> {
        let mut b = Box::new(Props { p: EVENT_TRACE_PROPERTIES::default(), name: [0; 64] });
        b.p.Wnode.BufferSize = size_of::<Props>() as u32;
        b.p.Wnode.Flags = WNODE_FLAG_TRACED_GUID;
        // System time: timestamps are FILETIME, comparable with the wall clock.
        b.p.Wnode.ClientContext = 2;
        b.p.LogFileMode = EVENT_TRACE_REAL_TIME_MODE;
        b.p.FlushTimer = 1;
        b.p.LoggerNameOffset = size_of::<EVENT_TRACE_PROPERTIES>() as u32;
        b
    }
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn now_filetime() -> i64 {
    let ft = unsafe { windows::Win32::System::SystemInformation::GetSystemTimePreciseAsFileTime() };
    ((ft.dwHighDateTime as i64) << 32) | ft.dwLowDateTime as i64
}

/// A running ETW session counting presents of one process.
pub struct FpsSession {
    handle: CONTROLTRACE_HANDLE,
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
}

impl FpsSession {
    /// Starts the session (stopping a stale one with the same name first).
    /// The error is the Win32 code, [`ACCESS_DENIED`] without the rights.
    pub fn start(pid: u32) -> Result<Self, u32> {
        let name = wide(SESSION_NAME);
        unsafe {
            stop_named(&name);
            let mut handle = CONTROLTRACE_HANDLE::default();
            let mut props = Props::new();
            let mut err = StartTraceW(&mut handle, PCWSTR(name.as_ptr()), &mut props.p);
            if err == ERROR_ALREADY_EXISTS {
                stop_named(&name);
                props = Props::new();
                err = StartTraceW(&mut handle, PCWSTR(name.as_ptr()), &mut props.p);
            }
            if err != ERROR_SUCCESS {
                return Err(err.0);
            }
            for guid in [DXGI, D3D9] {
                let e = EnableTraceEx2(handle, &guid, EVENT_CONTROL_CODE_ENABLE_PROVIDER.0, 4, 0, 0, 0, None);
                if e != ERROR_SUCCESS {
                    let mut p = Props::new();
                    let _ = ControlTraceW(handle, PCWSTR::null(), &mut p.p, EVENT_TRACE_CONTROL_STOP);
                    return Err(e.0);
                }
            }
            let shared = Arc::new(Shared { pid: AtomicU32::new(pid), window: Mutex::new(FpsWindow::default()) });
            let ctx = shared.clone();
            let thread = std::thread::Builder::new().name("nb-fps".into()).spawn(move || consume(ctx)).ok();
            Ok(Self { handle, shared, thread })
        }
    }

    /// Follows another process; the window restarts empty.
    pub fn set_pid(&self, pid: u32) {
        if self.shared.pid.swap(pid, Ordering::Relaxed) != pid {
            if let Ok(mut w) = self.shared.window.lock() {
                w.clear();
            }
        }
    }

    /// Current frame rate of the followed process, if it presents.
    pub fn fps(&self) -> Option<f32> {
        self.shared.window.lock().ok()?.fps(now_filetime())
    }
}

impl Drop for FpsSession {
    fn drop(&mut self) {
        unsafe {
            let mut p = Props::new();
            let _ = ControlTraceW(self.handle, PCWSTR::null(), &mut p.p, EVENT_TRACE_CONTROL_STOP);
        }
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

unsafe fn stop_named(name: &[u16]) {
    let mut p = Props::new();
    let _ = unsafe { ControlTraceW(CONTROLTRACE_HANDLE::default(), PCWSTR(name.as_ptr()), &mut p.p, EVENT_TRACE_CONTROL_STOP) };
}

/// Consumer thread: blocks in `ProcessTrace` until the session stops.
fn consume(shared: Arc<Shared>) {
    let mut name = wide(SESSION_NAME);
    let mut log = EVENT_TRACE_LOGFILEW::default();
    log.LoggerName = PWSTR(name.as_mut_ptr());
    log.Anonymous1.ProcessTraceMode = PROCESS_TRACE_MODE_REAL_TIME | PROCESS_TRACE_MODE_EVENT_RECORD;
    log.Anonymous2.EventRecordCallback = Some(on_event);
    log.Context = Arc::as_ptr(&shared) as *mut _;
    unsafe {
        let h = OpenTraceW(&mut log);
        if h.Value == u64::MAX {
            return;
        }
        let _ = ProcessTrace(&[h], None, None);
        let _ = CloseTrace(h);
    }
    drop(shared);
}

unsafe extern "system" fn on_event(rec: *mut EVENT_RECORD) {
    let Some(rec) = (unsafe { rec.as_ref() }) else { return };
    let Some(shared) = (unsafe { (rec.UserContext as *const Shared).as_ref() }) else { return };
    let h = &rec.EventHeader;
    let id = h.EventDescriptor.Id;
    let present = (h.ProviderId == DXGI && id == DXGI_PRESENT_START) || (h.ProviderId == D3D9 && id == D3D9_PRESENT_START);
    if present && h.ProcessId == shared.pid.load(Ordering::Relaxed) {
        if let Ok(mut w) = shared.window.lock() {
            w.push(h.TimeStamp);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: i64 = 10_000;

    #[test]
    fn fps_steady_sixty() {
        let mut w = FpsWindow::default();
        for i in 0..300 {
            w.push(i * 10_000_000 / 60);
        }
        let last = 299 * 10_000_000 / 60;
        let f = w.fps(last).unwrap();
        assert!((f - 60.0).abs() < 0.5, "{f}");
        assert!(w.times.len() <= 61);
    }

    #[test]
    fn fps_window_slides() {
        let mut w = FpsWindow::default();
        for i in 0..100 {
            w.push(i * 10 * MS); // 100 fps for 1 s
        }
        for i in 1..=30 {
            w.push(1000 * MS + i * 1000 * MS / 30); // then 30 fps
        }
        let f = w.fps(2000 * MS).unwrap();
        assert!((f - 30.0).abs() < 1.5, "{f}");
    }

    #[test]
    fn fps_none_when_empty_single_or_stale() {
        let mut w = FpsWindow::default();
        assert_eq!(w.fps(0), None);
        w.push(0);
        assert_eq!(w.fps(0), None);
        w.push(16 * MS);
        assert!(w.fps(16 * MS).is_some());
        assert_eq!(w.fps(16 * MS + STALE + 1), None);
        w.clear();
        assert_eq!(w.fps(0), None);
    }

    #[test]
    fn fps_out_of_order() {
        let mut w = FpsWindow::default();
        for t in [0, 20, 10, 40, 30] {
            w.push(t * MS);
        }
        assert_eq!(w.times, [0, 10, 20, 30, 40].map(|t| t * MS));
        assert!((w.fps(40 * MS).unwrap() - 100.0).abs() < 0.01);
    }
}
