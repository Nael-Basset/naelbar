//! System services for naelbar: battery and power mode, radios, brightness,
//! foreground window and on-demand statistics.
//!
//! Idle behaviour: one parked message-only-window thread. Everything is driven
//! by OS notifications (power setting notifications, WinEvent hooks, WinRT
//! radio events). The only timer exists while at least one [`StatsGuard`] is
//! alive. Slow work (WMI, DDC/CI, radio changes) runs on short-lived worker
//! threads that exit when done.

mod activity;
mod brightness;
pub mod devices;
mod foreground;
pub mod fps;
mod keys;
mod power;
mod radios;
mod stats;

pub use keys::{LockKeyWatch, watch_lock_keys};
pub use activity::{CaptureWatch, DownloadWatch, watch_capture, watch_downloads};

use nb_core::{Event, ForegroundState, PowerMode, PowerState, Sink};
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicUsize, Ordering};
use std::sync::{Arc, mpsc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use windows::Devices::Radios::RadioKind;
use windows::Win32::Foundation::{HANDLE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx, CoUninitialize};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Power::{
    EFFECTIVE_POWER_MODE, HPOWERNOTIFY,
    PowerRegisterForEffectivePowerModeNotifications,
    PowerUnregisterFromEffectivePowerModeNotifications, RegisterPowerSettingNotification,
    UnregisterPowerSettingNotification,
};
use windows::Win32::System::SystemServices::{
    GUID_ACDC_POWER_SOURCE, GUID_BATTERY_PERCENTAGE_REMAINING, GUID_POWER_SAVING_STATUS,
};
use windows::Win32::UI::Accessibility::{
    HWINEVENTHOOK, SetWinEventHook, UnhookWinEvent,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DEVICE_NOTIFY_WINDOW_HANDLE, DefWindowProcW, DestroyWindow, DispatchMessageW,
    EVENT_SYSTEM_FOREGROUND, GetForegroundWindow, GetMessageW,
    HWND_MESSAGE, KillTimer, MSG, PostMessageW, PostQuitMessage, RegisterClassExW,
    SetTimer, TranslateMessage, WINEVENT_OUTOFCONTEXT, WINDOW_EX_STYLE, WM_APP, WM_CLOSE,
    WM_DESTROY, WM_POWERBROADCAST, WM_TIMER, WNDCLASSEXW, WS_OVERLAPPED,
};
use windows::core::{PCWSTR, w};

const WM_STATS_SYNC: u32 = WM_APP + 1;
const WM_POWER_REFRESH: u32 = WM_APP + 2;
const STATS_TIMER: usize = 1;
const STATS_PERIOD: Duration = Duration::from_secs(1);

#[derive(Debug)]
pub enum Error {
    Windows(windows::core::Error),
    Thread(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Windows(e) => write!(f, "windows error: {e}"),
            Error::Thread(s) => write!(f, "system thread: {s}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<windows::core::Error> for Error {
    fn from(e: windows::core::Error) -> Self {
        Error::Windows(e)
    }
}

/// State shared between the handle, guards, worker threads and the loop thread.
pub(crate) struct Shared {
    pub sink: Sink,
    hwnd: AtomicIsize,
    stats_users: AtomicUsize,
    stats_detail: AtomicBool,
    timer_active: AtomicBool,
    pub brightness: brightness::Hub,
    pub radios: radios::Hub,
}

impl Shared {
    fn post(&self, msg: u32) {
        let h = self.hwnd.load(Ordering::Acquire);
        if h != 0 {
            // SAFETY: posting to a window id is always sound; failure is ignored.
            unsafe {
                let _ = PostMessageW(Some(HWND(h as *mut _)), msg, WPARAM(0), LPARAM(0));
            }
        }
    }
}

/// Owns the service. Dropping it stops the thread and releases every hook.
pub struct Handle {
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
}

/// Keeps the one-second stats sampler alive. Dropping the last guard stops the
/// timer and releases the PDH queries.
pub struct StatsGuard {
    shared: Arc<Shared>,
}

/// Starts the service. Emits power and foreground state before returning; radios
/// and brightness are read on worker threads and arrive shortly after.
pub fn start(sink: Sink) -> Result<Handle, Error> {
    let shared = Arc::new(Shared {
        sink,
        hwnd: AtomicIsize::new(0),
        stats_users: AtomicUsize::new(0),
        stats_detail: AtomicBool::new(false),
        timer_active: AtomicBool::new(false),
        brightness: brightness::Hub::default(),
        radios: radios::Hub::default(),
    });
    let (tx, rx) = mpsc::channel::<Result<(), Error>>();
    let s2 = shared.clone();
    let thread = std::thread::Builder::new()
        .name("nb-system".into())
        .spawn(move || run(s2, tx))
        .map_err(|e| Error::Thread(e.to_string()))?;
    match rx.recv() {
        Ok(Ok(())) => {}
        Ok(Err(e)) => {
            let _ = thread.join();
            return Err(e);
        }
        Err(_) => return Err(Error::Thread("thread exited during start".into())),
    }
    spawn_worker("nb-radios-init", {
        let s = shared.clone();
        move || radios::init(&s)
    });
    brightness::request_read(&shared);
    Ok(Handle { shared, thread: Some(thread) })
}

fn spawn_worker(name: &str, f: impl FnOnce() + Send + 'static) {
    let _ = std::thread::Builder::new().name(name.into()).spawn(f);
}

impl Handle {
    /// Switches the Windows power mode (overlay scheme). Returns immediately.
    pub fn set_power_mode(&self, mode: PowerMode) {
        let s = self.shared.clone();
        spawn_worker("nb-power-set", move || {
            let _ = power::set_mode(mode);
            s.post(WM_POWER_REFRESH);
        });
    }

    /// Turns the Wi-Fi radio on or off. Returns immediately.
    pub fn set_wifi(&self, on: bool) {
        let s = self.shared.clone();
        spawn_worker("nb-wifi-set", move || radios::set(&s, RadioKind::WiFi, on));
    }

    /// Turns the Bluetooth radio on or off. Returns immediately.
    pub fn set_bluetooth(&self, on: bool) {
        let s = self.shared.clone();
        spawn_worker("nb-bt-set", move || radios::set(&s, RadioKind::Bluetooth, on));
    }

    /// Sets brightness, 0 to 100 (WMI for the panel, DDC/CI for monitors).
    /// Requests coalesce on one transient worker, so dragging a slider is cheap.
    pub fn set_brightness(&self, screen: u32, percent: u8) {
        brightness::request_set(&self.shared, screen, percent);
    }

    /// Asks for a fresh read; the result arrives as `Event::Screens`.
    pub fn request_brightness(&self) {
        brightness::request_read(&self.shared);
    }

    /// `None` until the first probe finished, then whether any brightness backend
    /// works. `Some(false)` is the clean "unsupported" answer (no event is sent).
    pub fn brightness_supported(&self) -> Option<bool> {
        match self.shared.brightness.state.load(Ordering::Acquire) {
            brightness::SUPPORTED => Some(true),
            brightness::UNSUPPORTED => Some(false),
            _ => None,
        }
    }

    /// Starts `Event::Stats` once per second for as long as the guard lives.
    pub fn subscribe_stats(&self) -> StatsGuard {
        if self.shared.stats_users.fetch_add(1, Ordering::AcqRel) == 0 {
            self.shared.post(WM_STATS_SYNC);
        }
        StatsGuard { shared: self.shared.clone() }
    }

    /// While on, each stats tick also emits `Event::Processes`.
    pub fn set_stats_detail(&self, on: bool) {
        self.shared.stats_detail.store(on, Ordering::Release);
    }

    /// True while the stats timer exists. Diagnostic: must be false with no guard.
    pub fn stats_timer_active(&self) -> bool {
        self.shared.timer_active.load(Ordering::Acquire)
    }
}

impl Drop for StatsGuard {
    fn drop(&mut self) {
        if self.shared.stats_users.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.shared.post(WM_STATS_SYNC);
        }
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        // Radio handlers must be removed while the loop thread still holds the MTA.
        radios::shutdown(&self.shared);
        self.shared.post(WM_CLOSE);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

// ------------------------------------------------------------------ loop thread

struct Ctx {
    shared: Arc<Shared>,
    hwnd: HWND,
    last_power: Cell<Option<PowerState>>,
    last_fg: RefCell<Option<(String, bool, bool)>>,
    fg_hwnd: Cell<isize>,
    fg_hook: Cell<isize>,
    power_regs: RefCell<Vec<HPOWERNOTIFY>>,
    mode_reg: Cell<*mut core::ffi::c_void>,
    sampler: RefCell<Option<stats::Sampler>>,
    deadline: Cell<Option<Instant>>,
}

thread_local! {
    static CTX: RefCell<Option<Rc<Ctx>>> = const { RefCell::new(None) };
}

fn ctx() -> Option<Rc<Ctx>> {
    CTX.with(|c| c.borrow().clone())
}

fn run(shared: Arc<Shared>, tx: mpsc::Sender<Result<(), Error>>) {
    // Keeps the process MTA alive for as long as the service runs: WinRT radio
    // objects and their event handlers live there, and short-lived worker
    // threads that initialise COM would otherwise take it down when they exit.
    // SAFETY: balanced with CoUninitialize below when initialisation succeeded.
    let com = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.is_ok();
    match setup(&shared) {
        Err(e) => {
            let _ = tx.send(Err(e));
        }
        Ok(()) => {
            let _ = tx.send(Ok(()));
            let mut msg = MSG::default();
            // SAFETY: standard message loop; blocks in the kernel when idle.
            unsafe {
                while GetMessageW(&mut msg, None, 0, 0).as_bool() {
                    let _ = TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
            }
            CTX.with(|c| *c.borrow_mut() = None);
        }
    }
    shared.hwnd.store(0, Ordering::Release);
    if com {
        // SAFETY: matches the successful CoInitializeEx above.
        unsafe { CoUninitialize() };
    }
}

fn setup(shared: &Arc<Shared>) -> Result<(), Error> {
    // SAFETY: Win32 setup on the thread that owns the window.
    unsafe {
        let hinst = GetModuleHandleW(None)?;
        let class = w!("NaelbarSystemSink");
        let wc = WNDCLASSEXW {
            cbSize: size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(wndproc),
            hInstance: hinst.into(),
            lpszClassName: class,
            ..Default::default()
        };
        // Registration may already exist from an earlier start in this process.
        RegisterClassExW(&wc);
        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            class,
            PCWSTR::null(),
            WS_OVERLAPPED,
            0,
            0,
            0,
            0,
            Some(HWND_MESSAGE),
            None,
            Some(hinst.into()),
            None,
        )?;
        let c = Rc::new(Ctx {
            shared: shared.clone(),
            hwnd,
            last_power: Cell::new(None),
            last_fg: RefCell::new(None),
            fg_hwnd: Cell::new(0),
            fg_hook: Cell::new(0),
            power_regs: RefCell::new(Vec::new()),
            mode_reg: Cell::new(std::ptr::null_mut()),
            sampler: RefCell::new(None),
            deadline: Cell::new(None),
        });
        CTX.with(|x| *x.borrow_mut() = Some(c.clone()));
        shared.hwnd.store(hwnd.0 as isize, Ordering::Release);

        for g in [&GUID_BATTERY_PERCENTAGE_REMAINING, &GUID_ACDC_POWER_SOURCE, &GUID_POWER_SAVING_STATUS, &power::GUID_ACTIVE_PLAN] {
            if let Ok(h) = RegisterPowerSettingNotification(HANDLE(hwnd.0), g, DEVICE_NOTIFY_WINDOW_HANDLE) {
                c.power_regs.borrow_mut().push(h);
            }
        }
        // Effective power mode (overlay) changes; best effort.
        let mut reg: *mut core::ffi::c_void = std::ptr::null_mut();
        if PowerRegisterForEffectivePowerModeNotifications(
            2,
            Some(mode_callback),
            Some(hwnd.0 as *const _),
            &mut reg,
        )
        .is_ok()
        {
            c.mode_reg.set(reg);
        }
        c.fg_hook.set(
            SetWinEventHook(
                EVENT_SYSTEM_FOREGROUND,
                EVENT_SYSTEM_FOREGROUND,
                None,
                Some(win_event),
                0,
                0,
                WINEVENT_OUTOFCONTEXT,
            )
            .0 as isize,
        );
        c.refresh_power();
        c.set_foreground(GetForegroundWindow());
    }
    Ok(())
}

unsafe extern "system" fn mode_callback(_mode: EFFECTIVE_POWER_MODE, ctx: *const core::ffi::c_void) {
    // Runs on a system thread: only post to the loop thread.
    // SAFETY: posting to a window handle is sound even if it was destroyed.
    unsafe {
        let _ = PostMessageW(Some(HWND(ctx as *mut _)), WM_POWER_REFRESH, WPARAM(0), LPARAM(0));
    }
}

unsafe extern "system" fn win_event(
    _hook: HWINEVENTHOOK,
    event: u32,
    hwnd: HWND,
    _id_object: i32,
    _id_child: i32,
    _thread: u32,
    _time: u32,
) {
    let Some(c) = ctx() else { return };
    if event == EVENT_SYSTEM_FOREGROUND {
        c.set_foreground(hwnd);
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if let Some(c) = ctx() {
        match msg {
            WM_POWERBROADCAST | WM_POWER_REFRESH => c.refresh_power(),
            WM_STATS_SYNC => c.sync_stats(),
            WM_TIMER if wp.0 == STATS_TIMER => c.stats_tick(),
            WM_CLOSE => {
                // SAFETY: destroying our own window on its thread.
                unsafe {
                    let _ = DestroyWindow(hwnd);
                }
                return LRESULT(0);
            }
            WM_DESTROY => {
                c.teardown();
                // SAFETY: ends the loop of this thread.
                unsafe { PostQuitMessage(0) };
                return LRESULT(0);
            }
            _ => {}
        }
    }
    // SAFETY: default handling.
    unsafe { DefWindowProcW(hwnd, msg, wp, lp) }
}

impl Ctx {
    fn refresh_power(&self) {
        let now = power::read_state();
        if self.last_power.get().is_none_or(|p| !power::same(&p, &now)) {
            self.last_power.set(Some(now));
            (self.shared.sink)(Event::Power(now));
        }
    }

    fn set_foreground(&self, hwnd: HWND) {
        if hwnd.0.is_null() {
            return;
        }
        self.fg_hwnd.set(hwnd.0 as isize);
        self.eval_foreground();
    }

    fn eval_foreground(&self) {
        let Some(s) = foreground::describe(HWND(self.fg_hwnd.get() as *mut _)) else { return };
        let key = (s.exe.clone(), s.fullscreen, s.desktop);
        if self.last_fg.borrow().as_ref() != Some(&key) {
            *self.last_fg.borrow_mut() = Some(key);
            (self.shared.sink)(Event::Foreground(ForegroundState { exe: s.exe, fullscreen: s.fullscreen, desktop: s.desktop }));
        }
    }

    fn want_stats(&self) -> bool {
        self.shared.stats_users.load(Ordering::Acquire) > 0
    }

    fn sync_stats(&self) {
        let running = self.sampler.borrow().is_some();
        if self.want_stats() && !running {
            *self.sampler.borrow_mut() = Some(stats::Sampler::new());
            self.arm(Instant::now() + STATS_PERIOD);
        } else if !self.want_stats() && running {
            self.stop_stats();
        }
    }

    /// One-shot timer aimed at `at`.
    fn arm(&self, at: Instant) {
        self.deadline.set(Some(at));
        let ms = at.saturating_duration_since(Instant::now()).as_millis().max(1) as u32;
        // SAFETY: timer belongs to our window.
        unsafe {
            SetTimer(Some(self.hwnd), STATS_TIMER, ms, None);
        }
        self.shared.timer_active.store(true, Ordering::Release);
    }

    fn stop_stats(&self) {
        // SAFETY: timer belongs to our window.
        unsafe {
            let _ = KillTimer(Some(self.hwnd), STATS_TIMER);
        }
        self.shared.timer_active.store(false, Ordering::Release);
        self.deadline.set(None);
        *self.sampler.borrow_mut() = None; // drops the PDH query
    }

    fn stats_tick(&self) {
        // One-shot: kill, sample, re-arm only if someone still listens.
        // SAFETY: timer belongs to our window.
        unsafe {
            let _ = KillTimer(Some(self.hwnd), STATS_TIMER);
        }
        self.shared.timer_active.store(false, Ordering::Release);
        if !self.want_stats() {
            self.stop_stats();
            return;
        }
        let detail = self.shared.stats_detail.load(Ordering::Acquire);
        let sample = self.sampler.borrow_mut().as_mut().map(|s| {
            let mut v = s.sample();
            if detail {
                s.sample_sensors(&mut v);
            }
            v
        });
        if let Some(s) = sample {
            (self.shared.sink)(Event::Stats(s));
        }
        if detail {
            let list = self.sampler.borrow_mut().as_mut().map(|s| s.processes());
            if let Some(l) = list {
                (self.shared.sink)(Event::Processes(l));
            }
        }
        let prev = self.deadline.get().unwrap_or_else(Instant::now);
        let mut next = prev + STATS_PERIOD;
        if next <= Instant::now() {
            next = Instant::now() + STATS_PERIOD;
        }
        self.arm(next);
    }

    fn teardown(&self) {
        self.stop_stats();
        // SAFETY: releasing resources this context registered.
        unsafe {
            let h = self.fg_hook.replace(0);
            if h != 0 {
                let _ = UnhookWinEvent(HWINEVENTHOOK(h as *mut _));
            }
            for h in self.power_regs.borrow_mut().drain(..) {
                let _ = UnregisterPowerSettingNotification(h);
            }
            let reg = self.mode_reg.replace(std::ptr::null_mut());
            if !reg.is_null() {
                let _ = PowerUnregisterFromEffectivePowerModeNotifications(reg);
            }
        }
    }
}
