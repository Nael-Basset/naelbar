//! One widget detached to the desktop, in its own process:
//! `naelbar.exe --desktop-widget=<focus|elapsed|batteries|clock>`.
//!
//! The island starts and stops that process to follow `Config::desktop_widget`
//! ([`sync`]) and sends it the focus timer through `WM_COPYDATA` ([`push_focus`]).
//!
//! Z-order: a bottom-most tool window. Parenting to the desktop's `WorkerW`
//! relies on an undocumented Progman message whose window layout changed in
//! Windows 11 24H2, and it breaks for some wallpaper engines, so it is not
//! used. Instead the window keeps itself at the bottom on every
//! `WM_WINDOWPOSCHANGING`, and a foreground WinEvent hook notices "show
//! desktop" (Win+D): the desktop comes to the front with no normal window
//! above it, so the widget goes topmost until another window takes the
//! foreground. Nothing runs between those notifications.
//!
//! Repaints: only when a displayed value changes, through a one-shot timer
//! aimed at that instant (next minute, next second of a running focus, next
//! whole percent, next battery read).

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Dwm::{DWMWA_CLOAKED, DwmGetWindowAttribute};
use windows::Win32::Graphics::Gdi::{GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromPoint};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Power::{GetSystemPowerStatus, SYSTEM_POWER_STATUS};
use windows::Win32::System::SystemInformation::GetLocalTime;
use windows::Win32::System::Threading::{CreateMutexW, ReleaseMutex, WaitForSingleObject};
use windows::Win32::UI::Accessibility::{HWINEVENTHOOK, SetWinEventHook, UnhookWinEvent};
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::Input::KeyboardAndMouse::{ReleaseCapture, SetCapture, TME_LEAVE, TME_NONCLIENT, TRACKMOUSEEVENT, TrackMouseEvent};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{PCWSTR, Result, w};

use nb_core::{Event, PowerMode, PowerState, Sink};

use crate::config::{Config, WM_CONFIG_CHANGED};
use crate::render::Renderer;
use crate::state::{AppState, Command, FocusPhase, FocusRun, FocusTimer, unix_ms};
use crate::ui::theme::*;
use crate::ui::{Align, Cmd, Icon, Input, Pt, Rect, TextStyle, Ui, WidgetState, id, tr};

/// Values of `Config::desktop_widget`.
pub const WIDGETS: [&str; 5] = ["none", "focus", "elapsed", "batteries", "clock"];

const CLASS: PCWSTR = w!("naelbar.desktop");
/// `WM_COPYDATA` tag of a focus timer snapshot.
const COPY_FOCUS: usize = 0x6E62_666F;
const WM_DEVICES: u32 = WM_APP + 1;
const TIMER_REDRAW: usize = 1;
const W: f32 = 200.0;
const MARGIN_DIP: f32 = 24.0;
const CROSS: f32 = 18.0;
const WM_MOUSELEAVE: u32 = 0x02A3;
const WM_NCMOUSELEAVE: u32 = 0x02A2;

/// `COPYDATASTRUCT`, which lives behind another `windows` feature.
#[repr(C)]
struct CopyData {
    tag: usize,
    len: u32,
    data: *const u8,
}
const DBT_DEVNODES_CHANGED: usize = 0x0007;
const DBT_DEVICEARRIVAL: usize = 0x8000;
const DBT_DEVICEREMOVECOMPLETE: usize = 0x8004;

// ---------------------------------------------------------------- island side

/// Message the widget posts to the island once its window exists, so the
/// island sends the focus timer at once.
pub fn hello_message() -> u32 {
    static MSG: AtomicU32 = AtomicU32::new(0);
    let m = MSG.load(Ordering::Relaxed);
    if m != 0 {
        return m;
    }
    let m = unsafe { RegisterWindowMessageW(w!("naelbar.desktop.hello")) };
    MSG.store(m, Ordering::Relaxed);
    m
}

fn running_widget() -> Option<(HWND, String)> {
    unsafe {
        let hwnd = FindWindowW(CLASS, PCWSTR::null()).ok()?;
        let mut buf = [0u16; 32];
        let n = GetWindowTextW(hwnd, &mut buf).max(0) as usize;
        Some((hwnd, String::from_utf16_lossy(&buf[..n])))
    }
}

/// Starts, replaces or closes the desktop widget process to match `name`.
pub fn sync(name: &str) {
    let running = running_widget();
    unsafe {
        if let Some((hwnd, current)) = &running {
            if current == name {
                // Accent, language or focus lengths may have changed.
                let _ = PostMessageW(Some(*hwnd), WM_CONFIG_CHANGED, WPARAM(0), LPARAM(0));
                return;
            }
            let _ = PostMessageW(Some(*hwnd), WM_CLOSE, WPARAM(0), LPARAM(0));
        }
    }
    if name != "none" && WIDGETS.contains(&name) {
        if let Ok(exe) = std::env::current_exe() {
            let _ = std::process::Command::new(exe).arg(format!("--desktop-widget={name}")).spawn();
        }
    }
}

/// Closes the widget when the island quits.
pub fn close() {
    if let Some((hwnd, _)) = running_widget() {
        unsafe {
            let _ = PostMessageW(Some(hwnd), WM_CLOSE, WPARAM(0), LPARAM(0));
        }
    }
}

thread_local! {
    static LAST_FOCUS: RefCell<Option<String>> = const { RefCell::new(None) };
}

/// Sends the focus timer to the widget when it changed since the last call,
/// or always with `force`.
pub fn push_focus(f: &FocusTimer, force: bool) {
    let text = encode_focus(f.phase, f.run);
    let changed = LAST_FOCUS.with(|l| {
        let mut l = l.borrow_mut();
        let changed = l.as_deref() != Some(text.as_str());
        *l = Some(text.clone());
        changed
    });
    if !changed && !force {
        return;
    }
    let Some((hwnd, name)) = running_widget() else { return };
    if name != "focus" {
        return;
    }
    let data = CopyData { tag: COPY_FOCUS, len: text.len() as u32, data: text.as_ptr() };
    unsafe {
        let _ = SendMessageTimeoutW(hwnd, WM_COPYDATA, WPARAM(0), LPARAM(&data as *const _ as isize), SMTO_ABORTIFHUNG, 200, None);
    }
}

/// `work|idle`, `break|run:<ends at, Unix ms>`, `work|pause:<remaining ms>`.
pub fn encode_focus(phase: FocusPhase, run: FocusRun) -> String {
    let p = match phase {
        FocusPhase::Work => "work",
        FocusPhase::Break => "break",
    };
    match run {
        FocusRun::Idle => format!("{p}|idle"),
        FocusRun::Running { ends_at_ms } => format!("{p}|run:{ends_at_ms}"),
        FocusRun::Paused { remaining_ms } => format!("{p}|pause:{remaining_ms}"),
    }
}

pub fn decode_focus(s: &str) -> Option<(FocusPhase, FocusRun)> {
    let (p, r) = s.split_once('|')?;
    let phase = match p {
        "work" => FocusPhase::Work,
        "break" => FocusPhase::Break,
        _ => return None,
    };
    let run = if r == "idle" {
        FocusRun::Idle
    } else if let Some(v) = r.strip_prefix("run:") {
        FocusRun::Running { ends_at_ms: v.parse().ok()? }
    } else if let Some(v) = r.strip_prefix("pause:") {
        FocusRun::Paused { remaining_ms: v.parse().ok()? }
    } else {
        return None;
    };
    Some((phase, run))
}

// ------------------------------------------------------------- pure helpers

/// Saved top-left corners in physical pixels, by widget name.
pub type Positions = BTreeMap<String, (i32, i32)>;

fn positions_path() -> Option<std::path::PathBuf> {
    std::env::var_os("APPDATA").map(|d| std::path::PathBuf::from(d).join("naelbar").join("desktop.json"))
}

pub fn parse_positions(text: &str) -> Positions {
    serde_json::from_str(text).unwrap_or_default()
}

pub fn positions_json(p: &Positions) -> String {
    serde_json::to_string_pretty(p).unwrap_or_else(|_| "{}".into())
}

fn load_positions() -> Positions {
    positions_path().and_then(|p| std::fs::read_to_string(p).ok()).map(|t| parse_positions(&t)).unwrap_or_default()
}

fn save_position(name: &str, pos: (i32, i32)) {
    let Some(path) = positions_path() else { return };
    let mut all = load_positions();
    all.insert(name.to_string(), pos);
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let tmp = path.with_extension(format!("{}.tmp", std::process::id()));
    if std::fs::write(&tmp, positions_json(&all)).is_ok() {
        let _ = std::fs::rename(&tmp, &path);
    }
}

/// Where the window goes: the saved corner kept fully inside the work area,
/// or the top right corner of the work area. `work` is left, top, right, bottom.
pub fn place(saved: Option<(i32, i32)>, size: (i32, i32), work: [i32; 4], margin: i32) -> (i32, i32) {
    let [l, t, r, b] = work;
    let (x, y) = saved.unwrap_or((r - size.0 - margin, t + margin));
    (x.clamp(l, (r - size.0).max(l)), y.clamp(t, (b - size.1).max(t)))
}

/// Milliseconds until the minute changes.
pub fn next_minute_in(second: u32, millis: u32) -> u64 {
    (60u64.saturating_sub(second as u64) * 1000).saturating_sub(millis as u64).max(1)
}

/// Milliseconds until a running countdown shows another second.
pub fn next_second_in(remaining_ms: u64) -> u64 {
    (remaining_ms.saturating_sub(1) % 1000) + 1
}

/// Height of the widget in DIPs.
fn height(name: &str, st: &AppState) -> f32 {
    match name {
        "elapsed" => 30.0 + 5.0 * 18.0 + 14.0,
        "batteries" => crate::widgets::batteries::height(st),
        _ => 92.0,
    }
}

// ------------------------------------------------------------ widget process

struct Desk {
    hwnd: HWND,
    name: String,
    renderer: Renderer,
    state: AppState,
    input: Input,
    widgets: WidgetState,
    last: Vec<Cmd>,
    scale: f32,
    size: (i32, i32),
    hovered: bool,
    tracking: bool,
    dirty: bool,
    armed: Option<u64>,
    /// "Show desktop" is in effect: the widget is topmost meanwhile.
    over: bool,
    devices: Arc<Mutex<Option<Vec<nb_core::DeviceBattery>>>>,
    sink: Sink,
}

thread_local! {
    static DESK: RefCell<Option<Desk>> = const { RefCell::new(None) };
}

fn with<R>(f: impl FnOnce(&mut Desk) -> R) -> Option<R> {
    DESK.with(|c| c.try_borrow_mut().ok().and_then(|mut g| g.as_mut().map(f)))
}

fn lo(v: isize) -> i32 {
    (v & 0xFFFF) as i16 as i32
}

fn hi(v: isize) -> i32 {
    ((v >> 16) & 0xFFFF) as i16 as i32
}

fn power_now() -> Option<PowerState> {
    let mut s = SYSTEM_POWER_STATUS::default();
    unsafe { GetSystemPowerStatus(&mut s).ok()? };
    let battery = (s.BatteryFlag & 128 == 0 && s.BatteryLifePercent <= 100).then_some(s.BatteryLifePercent);
    Some(PowerState { battery_percent: battery, charging: s.ACLineStatus == 1, mode: PowerMode::Balanced, seconds_left: None })
}

fn work_area(pt: POINT) -> [i32; 4] {
    unsafe {
        let mon = MonitorFromPoint(pt, MONITOR_DEFAULTTONEAREST);
        let mut info = MONITORINFO { cbSize: size_of::<MONITORINFO>() as u32, ..Default::default() };
        if GetMonitorInfoW(mon, &mut info).as_bool() {
            let r = info.rcWork;
            [r.left, r.top, r.right, r.bottom]
        } else {
            [0, 0, GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN)]
        }
    }
}

fn notify_island_config() {
    unsafe {
        if let Ok(island) = FindWindowW(w!("naelbar.window"), w!("naelbar control")) {
            let _ = PostMessageW(Some(island), WM_CONFIG_CHANGED, WPARAM(0), LPARAM(0));
        }
    }
}

/// The desktop is in front with no normal window above it: "show desktop".
unsafe fn desktop_in_front(fg: HWND, own: HWND) -> bool {
    let mut class = [0u16; 32];
    let n = GetClassNameW(fg, &mut class).max(0) as usize;
    let class = String::from_utf16_lossy(&class[..n]);
    if class != "WorkerW" && class != "Progman" {
        return false;
    }
    let mut h = GetWindow(fg, GW_HWNDPREV).ok();
    while let Some(w) = h {
        if w != own && IsWindowVisible(w).as_bool() && !IsIconic(w).as_bool() {
            let ex = GetWindowLongW(w, GWL_EXSTYLE) as u32;
            let mut cloaked = 0u32;
            let _ = DwmGetWindowAttribute(w, DWMWA_CLOAKED, &mut cloaked as *mut u32 as *mut _, 4);
            if ex & WS_EX_TOPMOST.0 == 0 && ex & WS_EX_TOOLWINDOW.0 == 0 && cloaked == 0 {
                return false;
            }
        }
        h = GetWindow(w, GW_HWNDPREV).ok();
    }
    true
}

unsafe extern "system" fn on_foreground(_: HWINEVENTHOOK, _: u32, fg: HWND, _: i32, _: i32, _: u32, _: u32) {
    let Some(own) = with(|d| d.hwnd) else { return };
    if fg == own {
        return;
    }
    let over = desktop_in_front(fg, own);
    if with(|d| std::mem::replace(&mut d.over, over)) == Some(over) {
        return;
    }
    let flags = SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE;
    if over {
        let _ = SetWindowPos(own, Some(HWND_TOPMOST), 0, 0, 0, 0, flags);
    } else {
        let _ = SetWindowPos(own, Some(HWND_NOTOPMOST), 0, 0, 0, 0, flags);
        let _ = SetWindowPos(own, Some(HWND_BOTTOM), 0, 0, 0, 0, flags);
    }
}

impl Desk {
    fn cross(&self) -> Rect {
        Rect::new(W - CROSS - 7.0, 7.0, CROSS, CROSS)
    }

    fn fit_size(&mut self) {
        let h = height(&self.name, &self.state);
        let size = ((W * self.scale).round() as i32, (h * self.scale).round() as i32);
        if size != self.size {
            self.size = size;
            unsafe {
                let _ = SetWindowPos(self.hwnd, None, 0, 0, size.0, size.1, SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE);
            }
            self.renderer.set_size(size.0 as u32, size.1 as u32, self.scale);
            self.last.clear();
        }
    }

    fn arm(&mut self, deadline: Option<u64>) {
        if deadline == self.armed {
            return;
        }
        self.armed = deadline;
        unsafe {
            let _ = KillTimer(Some(self.hwnd), TIMER_REDRAW);
            if let Some(d) = deadline {
                let delay = d.saturating_sub(unix_ms()).clamp(1, i32::MAX as u64) as u32;
                SetTimer(Some(self.hwnd), TIMER_REDRAW, delay, None);
            }
        }
    }

    fn frame(&mut self) {
        self.dirty = false;
        let now = unix_ms();
        self.state.focus.advance(now);
        if self.name == "batteries" {
            self.state.power = power_now();
            if let Some(d) = self.devices.lock().unwrap().take() {
                self.state.batteries.update(d, now);
            }
        }
        self.fit_size();
        let mut out = Vec::new();
        let mut closed = false;
        let cross = self.cross();
        let (cmds, deadline) = {
            let mut ui = Ui::new(&self.input, &mut self.widgets, &mut out, self.scale, now);
            let full = Rect::new(0.0, 0.0, W, self.size.1 as f32 / self.scale);
            match self.name.as_str() {
                "batteries" => crate::widgets::batteries::draw(&mut ui, &self.state, full, false),
                "focus" => focus(&mut ui, &self.state.focus, full),
                "elapsed" => elapsed(&mut ui, full),
                _ => clock(&mut ui, full),
            }
            ui.stroke(full.inset(0.5), R_TILE, 1.0, HAIRLINE);
            if self.hovered {
                closed = ui.icon_button(id("desktop.close", 0), cross, Icon::Close, 11.0, TEXT_DIM).clicked;
            }
            (std::mem::take(&mut ui.cmds), ui.deadline)
        };
        self.input.pressed = false;
        self.input.released = false;
        self.input.wheel = 0.0;
        if out.contains(&Command::RefreshBatteries) {
            crate::widgets::batteries::request(&self.sink);
        }
        // The cross: the setting goes off and the window closes.
        if closed {
            let mut c = Config::load();
            c.desktop_widget = "none".into();
            c.save();
            notify_island_config();
            unsafe {
                let _ = PostMessageW(Some(self.hwnd), WM_CLOSE, WPARAM(0), LPARAM(0));
            }
        }
        if cmds != self.last && self.renderer.render(&cmds).is_ok() {
            self.last = cmds;
        }
        self.arm(deadline);
    }

    fn pointer(&mut self, client: Pt, nonclient: bool) {
        self.hovered = true;
        self.input.mouse = Some(client);
        if !self.tracking {
            self.tracking = true;
            let mut t = TRACKMOUSEEVENT { cbSize: size_of::<TRACKMOUSEEVENT>() as u32, dwFlags: if nonclient { TME_LEAVE | TME_NONCLIENT } else { TME_LEAVE }, hwndTrack: self.hwnd, dwHoverTime: 0 };
            unsafe {
                let _ = TrackMouseEvent(&mut t);
            }
        }
        self.dirty = true;
    }

    fn screen_to_dip(&self, x: i32, y: i32) -> Pt {
        let mut p = POINT { x, y };
        unsafe {
            let _ = windows::Win32::Graphics::Gdi::ScreenToClient(self.hwnd, &mut p);
        }
        Pt { x: p.x as f32 / self.scale, y: p.y as f32 / self.scale }
    }

    fn on_message(&mut self, msg: u32, wp: WPARAM, lp: LPARAM) -> Option<isize> {
        match msg {
            WM_NCHITTEST => {
                let p = self.screen_to_dip(lo(lp.0), hi(lp.0));
                Some(if self.cross().contains(p) { HTCLIENT } else { HTCAPTION } as isize)
            }
            WM_MOUSEACTIVATE => Some(MA_NOACTIVATE as isize),
            WM_WINDOWPOSCHANGING => {
                let pos = unsafe { &mut *(lp.0 as *mut WINDOWPOS) };
                if !self.over && pos.flags.0 & SWP_NOZORDER.0 == 0 {
                    pos.hwndInsertAfter = HWND_BOTTOM;
                }
                Some(0)
            }
            WM_EXITSIZEMOVE => {
                let mut r = RECT::default();
                unsafe {
                    let _ = GetWindowRect(self.hwnd, &mut r);
                }
                save_position(&self.name, (r.left, r.top));
                Some(0)
            }
            WM_MOUSEMOVE => {
                self.pointer(Pt { x: lo(lp.0) as f32 / self.scale, y: hi(lp.0) as f32 / self.scale }, false);
                Some(0)
            }
            WM_NCMOUSEMOVE => {
                let p = self.screen_to_dip(lo(lp.0), hi(lp.0));
                self.pointer(p, true);
                None
            }
            WM_MOUSELEAVE | WM_NCMOUSELEAVE => {
                self.tracking = false;
                let mut cursor = POINT::default();
                let mut r = RECT::default();
                unsafe {
                    let _ = GetCursorPos(&mut cursor);
                    let _ = GetWindowRect(self.hwnd, &mut r);
                }
                let inside = cursor.x >= r.left && cursor.x < r.right && cursor.y >= r.top && cursor.y < r.bottom;
                if !inside && !self.input.down {
                    self.hovered = false;
                    self.input.mouse = None;
                    self.dirty = true;
                }
                Some(0)
            }
            WM_LBUTTONDOWN => {
                self.input.down = true;
                self.input.pressed = true;
                self.dirty = true;
                unsafe { SetCapture(self.hwnd) };
                Some(0)
            }
            WM_LBUTTONUP => {
                self.input.down = false;
                self.input.released = true;
                self.dirty = true;
                unsafe {
                    let _ = ReleaseCapture();
                }
                Some(0)
            }
            WM_TIMER => {
                unsafe {
                    let _ = KillTimer(Some(self.hwnd), wp.0);
                }
                self.armed = None;
                self.dirty = true;
                Some(0)
            }
            WM_COPYDATA => {
                let data = unsafe { &*(lp.0 as *const CopyData) };
                if data.tag == COPY_FOCUS && data.len <= 64 && !data.data.is_null() {
                    let bytes = unsafe { std::slice::from_raw_parts(data.data, data.len as usize) };
                    if let Some((phase, run)) = std::str::from_utf8(bytes).ok().and_then(decode_focus) {
                        self.state.focus.phase = phase;
                        self.state.focus.run = run;
                        self.dirty = true;
                    }
                }
                Some(1)
            }
            WM_DEVICES => {
                self.dirty = true;
                Some(0)
            }
            WM_DEVICECHANGE => {
                if matches!(wp.0, DBT_DEVNODES_CHANGED | DBT_DEVICEARRIVAL | DBT_DEVICEREMOVECOMPLETE) && self.name == "batteries" {
                    self.state.batteries.invalidate();
                    self.dirty = true;
                }
                Some(1)
            }
            WM_POWERBROADCAST => {
                self.dirty = self.dirty || self.name == "batteries";
                Some(1)
            }
            WM_TIMECHANGE => {
                self.dirty = true;
                Some(0)
            }
            WM_CONFIG_CHANGED => {
                let c = Config::load();
                crate::config::apply_appearance(&c);
                self.state.focus.set_durations(c.focus_minutes, c.break_minutes);
                self.last.clear();
                self.dirty = true;
                Some(0)
            }
            WM_DPICHANGED => {
                self.scale = hi(wp.0 as isize).max(96) as f32 / 96.0;
                let r = unsafe { &*(lp.0 as *const RECT) };
                unsafe {
                    let _ = SetWindowPos(self.hwnd, None, r.left, r.top, 0, 0, SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE);
                }
                self.size = (0, 0);
                self.dirty = true;
                Some(0)
            }
            WM_ERASEBKGND => Some(1),
            WM_PAINT => unsafe {
                let mut ps = windows::Win32::Graphics::Gdi::PAINTSTRUCT::default();
                windows::Win32::Graphics::Gdi::BeginPaint(self.hwnd, &mut ps);
                let _ = windows::Win32::Graphics::Gdi::EndPaint(self.hwnd, &ps);
                Some(0)
            },
            WM_CLOSE => {
                unsafe {
                    let _ = DestroyWindow(self.hwnd);
                }
                Some(0)
            }
            WM_DESTROY => {
                unsafe { PostQuitMessage(0) };
                Some(0)
            }
            _ => None,
        }
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match with(|d| d.on_message(msg, wp, lp)).flatten() {
        Some(v) => LRESULT(v),
        None => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

/// Runs the widget window until it is closed.
pub fn run(name: &str) -> Result<()> {
    if !WIDGETS.contains(&name) || name == "none" {
        return Ok(());
    }
    unsafe {
        // The previous widget may still be closing: wait for it briefly.
        let mutex = CreateMutexW(None, false, w!("Local\\naelbar.desktop-widget"))?;
        let r = WaitForSingleObject(mutex, 3000);
        if r != WAIT_OBJECT_0 && r != WAIT_ABANDONED {
            return Ok(());
        }

        let instance = GetModuleHandleW(None)?;
        let wc = WNDCLASSEXW {
            cbSize: size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(wndproc),
            hInstance: instance.into(),
            lpszClassName: CLASS,
            hCursor: LoadCursorW(None, IDC_ARROW)?,
            ..Default::default()
        };
        if RegisterClassExW(&wc) == 0 {
            return Err(windows::core::Error::from_thread());
        }
        let config = Config::load();
        crate::config::apply_appearance(&config);

        let title: Vec<u16> = name.encode_utf16().chain(Some(0)).collect();
        let ex = WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_NOREDIRECTIONBITMAP;
        let hwnd = CreateWindowExW(ex, CLASS, PCWSTR(title.as_ptr()), WS_POPUP, 0, 0, 1, 1, None, None, Some(instance.into()), None)?;
        let scale = GetDpiForWindow(hwnd).max(96) as f32 / 96.0;

        let mut state = AppState::default();
        state.focus.set_durations(config.focus_minutes, config.break_minutes);
        let size = ((W * scale).round() as i32, (height(name, &state) * scale).round() as i32);
        let saved = load_positions().get(name).copied();
        let probe = saved.map_or(POINT { x: 0, y: 0 }, |(x, y)| POINT { x, y });
        let (x, y) = place(saved, size, work_area(probe), (MARGIN_DIP * scale).round() as i32);
        let _ = SetWindowPos(hwnd, Some(HWND_BOTTOM), x, y, size.0, size.1, SWP_NOACTIVATE);

        let mut renderer = Renderer::new(hwnd);
        renderer.set_size(size.0 as u32, size.1 as u32, scale);
        let devices: Arc<Mutex<Option<Vec<nb_core::DeviceBattery>>>> = Arc::new(Mutex::new(None));
        let sink: Sink = {
            let devices = devices.clone();
            let target = hwnd.0 as isize;
            Arc::new(move |e| {
                if let Event::Devices(d) = e {
                    *devices.lock().unwrap() = Some(d);
                    let _ = PostMessageW(Some(HWND(target as *mut _)), WM_DEVICES, WPARAM(0), LPARAM(0));
                }
            })
        };
        let desk = Desk {
            hwnd,
            name: name.to_string(),
            renderer,
            state,
            input: Input::default(),
            widgets: WidgetState::default(),
            last: Vec::new(),
            scale,
            size,
            hovered: false,
            tracking: false,
            dirty: true,
            armed: None,
            over: false,
            devices,
            sink,
        };
        DESK.with(|c| *c.borrow_mut() = Some(desk));
        with(|d| d.frame());
        let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        let _ = SetWindowPos(hwnd, Some(HWND_BOTTOM), 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);

        let hook = SetWinEventHook(EVENT_SYSTEM_FOREGROUND, EVENT_SYSTEM_FOREGROUND, None, Some(on_foreground), 0, 0, WINEVENT_OUTOFCONTEXT);
        if name == "focus" {
            if let Ok(island) = FindWindowW(w!("naelbar.window"), w!("naelbar control")) {
                let _ = PostMessageW(Some(island), hello_message(), WPARAM(hwnd.0 as usize), LPARAM(0));
            }
        }

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).0 > 0 {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
            if with(|d| d.dirty).unwrap_or(false) && !PeekMessageW(&mut msg, None, 0, 0, PM_NOREMOVE).as_bool() {
                with(|d| d.frame());
            }
        }
        if !hook.is_invalid() {
            let _ = UnhookWinEvent(hook);
        }
        DESK.with(|c| *c.borrow_mut() = None);
        let _ = ReleaseMutex(mutex);
        let _ = CloseHandle(mutex);
    }
    Ok(())
}

// ----------------------------------------------------------------- drawing

fn tile(ui: &mut Ui, r: Rect) {
    ui.fill(r, R_TILE, TILE);
}

fn focus(ui: &mut Ui, f: &FocusTimer, r: Rect) {
    tile(ui, r);
    let remaining = f.remaining(ui.now_ms);
    let shown = remaining.div_ceil(1000);
    let mut rest = r.inset_xy(14.0, 10.0);
    let mut head = rest.take_top(20.0, 4.0);
    ui.icon(Icon::Timer, head.take_left(18.0, 6.0).centered(16.0, 16.0), accent());
    let label = match f.phase {
        FocusPhase::Work => "Focus",
        FocusPhase::Break => tr("Pause", "Break"),
    };
    let label = match f.run {
        FocusRun::Paused { .. } => format!("{label} · {}", tr("en pause", "paused")),
        _ => label.to_string(),
    };
    ui.text(head, &label, TextStyle::new(12.0, 500, TEXT_DIM));
    let color = if f.is_running() { TEXT } else { TEXT_DIM };
    ui.text(rest, &format!("{:02}:{:02}", shown / 60, shown % 60), TextStyle::new(28.0, 600, color));
    if f.is_running() {
        ui.redraw_at(ui.now_ms + next_second_in(remaining));
    }
}

fn clock(ui: &mut Ui, r: Rect) {
    tile(ui, r);
    let t = unsafe { GetLocalTime() };
    let english = crate::ui::is_english();
    let mut rest = r.inset_xy(14.0, 12.0);
    ui.text(rest.take_top(40.0, 2.0), &format!("{:02}:{:02}", t.wHour, t.wMinute), TextStyle::new(30.0, 600, TEXT));
    const DAYS_FR: [&str; 7] = ["Dimanche", "Lundi", "Mardi", "Mercredi", "Jeudi", "Vendredi", "Samedi"];
    const DAYS_EN: [&str; 7] = ["Sunday", "Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday"];
    let day = (if english { DAYS_EN } else { DAYS_FR })[(t.wDayOfWeek as usize) % 7];
    let month = crate::agenda::month_name(t.wMonth as u32, english);
    let date = if english { format!("{day}, {month} {}", t.wDay) } else { format!("{day} {} {}", t.wDay, month.to_lowercase()) };
    ui.text(rest.take_top(18.0, 0.0), &date, TextStyle::new(12.0, 400, TEXT_DIM));
    ui.redraw_at(ui.now_ms + next_minute_in(t.wSecond as u32, t.wMilliseconds as u32));
}

fn elapsed(ui: &mut Ui, r: Rect) {
    use crate::widgets::elapsed;
    tile(ui, r);
    let now = crate::widgets::page2::naive_now();
    let mut rest = r.inset_xy(12.0, 8.0);
    ui.text(rest.take_top(18.0, 4.0), tr("Part écoulée", "Elapsed"), TextStyle::new(11.5, 600, TEXT_DIM));
    let mut next: Option<u64> = None;
    for (fr, en, span) in elapsed::all(now) {
        let mut row = rest.take_top(18.0, 0.0);
        let pct = span.percent(now);
        ui.text(row.take_left(62.0, 6.0), tr(fr, en), TextStyle::new(11.0, 500, TEXT_DIM));
        ui.text(row.take_right(34.0, 6.0), &format!("{pct} %"), TextStyle::new(11.0, 600, TEXT).aligned(Align::End));
        let bar = row.centered(row.w, 4.0);
        ui.fill(bar, 2.0, TILE_HOVER);
        // Whole percents only, so the bar changes with the number.
        let w = (bar.w * pct as f32 / 100.0).round().max(4.0);
        ui.fill(Rect::new(bar.x, bar.y, w.min(bar.w), bar.h), 2.0, accent());
        if let Some(wait) = span.next_percent_in(now) {
            next = Some(next.map_or(wait, |n| n.min(wait)));
        }
    }
    if let Some(wait) = next {
        ui.redraw_at(ui.now_ms + wait);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn focus_snapshots_round_trip() {
        for (p, r) in [
            (FocusPhase::Work, FocusRun::Idle),
            (FocusPhase::Break, FocusRun::Running { ends_at_ms: 1_763_300_000_123 }),
            (FocusPhase::Work, FocusRun::Paused { remaining_ms: 61_500 }),
        ] {
            assert_eq!(decode_focus(&encode_focus(p, r)), Some((p, r)));
        }
        for bad in ["", "work", "rest|idle", "work|run:", "work|pause:x", "work|stop"] {
            assert_eq!(decode_focus(bad), None, "{bad}");
        }
    }

    #[test]
    fn positions_survive_a_round_trip() {
        let mut p = Positions::new();
        p.insert("clock".into(), (-1200, 40));
        p.insert("focus".into(), (300, 500));
        assert_eq!(parse_positions(&positions_json(&p)), p);
        assert!(parse_positions("not json").is_empty());
        assert!(parse_positions("").is_empty());
    }

    #[test]
    fn the_window_stays_inside_the_work_area() {
        let work = [0, 0, 1920, 1040];
        assert_eq!(place(None, (200, 92), work, 24), (1696, 24), "top right by default");
        assert_eq!(place(Some((100, 200)), (200, 92), work, 24), (100, 200));
        assert_eq!(place(Some((1900, 1030)), (200, 92), work, 24), (1720, 948));
        assert_eq!(place(Some((-500, -10)), (200, 92), work, 24), (0, 0));
        // A second monitor on the left.
        assert_eq!(place(Some((-1500, 300)), (200, 92), [-1920, 0, 0, 1080], 24), (-1500, 300));
    }

    #[test]
    fn repaints_aim_at_the_next_visible_change() {
        assert_eq!(next_minute_in(0, 0), 60_000);
        assert_eq!(next_minute_in(59, 999), 1);
        assert_eq!(next_minute_in(30, 250), 29_750);
        // 25:00.000 left shows 25:00 until 24:59.999.
        assert_eq!(next_second_in(1_500_000), 1000);
        assert_eq!(next_second_in(1_499_001), 1);
        assert_eq!(next_second_in(1_499_000), 1000);
        assert_eq!(next_second_in(1), 1);
        assert_eq!(next_second_in(0), 1);
    }
}
