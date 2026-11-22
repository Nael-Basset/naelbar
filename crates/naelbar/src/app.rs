//! Windows, message loop, island state machine, frame scheduling.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use nb_core::{Event, PlaybackStatus, Sink};
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::{GetCurrentProcess, SetProcessWorkingSetSize};
use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::Shell::{ABM_NEW, ABM_REMOVE, ABN_FULLSCREENAPP, APPBARDATA, SHAppBarMessage};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{BOOL, Result, w};

use crate::agenda::{Agenda, WM_AGENDA_CHANGED};
use crate::anim::Spring;
use crate::config::{self, Config, WM_CONFIG_CHANGED};
use crate::preview::{self, Preview};
use crate::render::Renderer;
use crate::services::Services;
use crate::state::{AppState, Command, unix_ms};
use crate::tray::{self, Tray};
use crate::ui::theme::*;
use crate::ui::{Align, Cmd, Icon, Input, Pt, Rect, TextStyle, Ui, WidgetState, id};
use crate::views::{self, View};
use crate::{dragdrop, hotkeys};

#[path = "screens.rs"]
mod screens;

// Sizes in DIPs. The visual window leaves room for overshoot and the shadow.
const VISUAL_W: f32 = 560.0;
const VISUAL_H: f32 = 400.0;
/// Room kept for the shadow when the island hugs a side of the visual window.
const SIDE_PAD: f32 = 16.0;
const PILL_W: f32 = 126.0;
const PILL_H: f32 = 30.0;
const PILL_R: f32 = 15.0;
const CARD_W: f32 = 400.0;
const CARD_H: f32 = 270.0;
const CARD_R: f32 = 26.0;
const TAB_ROW_H: f32 = 40.0;
/// Height of the closed island while an application is in front.
const THIN_H: f32 = 6.0;
/// Width of the closed island while it shows a notification.
const NOTICE_W: f32 = 240.0;
use crate::activity::MIC_GREEN;

const CLOSE_GRACE_MS: u32 = 300;
/// How long after the last pointer move we confirm the pointer is still here.
const HOVER_CHECK_MS: u32 = 400;
const TRIM_DELAY_MS: u32 = 4000;

const MAX_RENDER_RETRIES: u32 = 3;

const TIMER_CLOSE: usize = 1;
const TIMER_REDRAW: usize = 2;
const TIMER_TRIM: usize = 3;
const TIMER_HOVER_CHECK: usize = 4;
/// One-shot, 3 s after a device change or a resume: debounces the burst of
/// notifications, then reads the Bluetooth batteries once.
const TIMER_DEVICES_SOON: usize = 5;
/// One-shot, 10 min: levels change without any notification. Armed only while
/// at least one device with a battery is connected; each read re-arms it.
const TIMER_DEVICES_POLL: usize = 6;
const DEVICES_SOON_MS: u32 = 3_000;
const DEVICES_POLL_MS: u32 = 10 * 60_000;

const WM_QUEUE: u32 = WM_APP;
/// The limits are read at most this often, and only on an event.
const USAGE_MIN_AGE: Duration = Duration::from_secs(300);
const WM_APPBAR: u32 = WM_APP + 5;
/// Posted by the thread that waited for a sync process; wParam is its exit code.
const WM_AGENDA_SYNCED: u32 = WM_APP + 7;
/// The agenda is synced at most this often unless something changed locally.
const AGENDA_SYNC_MIN_AGE: Duration = Duration::from_secs(60);
const WM_MOUSELEAVE: u32 = 0x02A3;
const WM_CAPTURECHANGED: u32 = 0x0215;

/// Thread-safe event queue. The sink wakes the loop once per batch.
struct Queue {
    events: Mutex<VecDeque<Event>>,
    wake_posted: AtomicBool,
    hwnd: isize,
}

impl Queue {
    fn push(&self, e: Event) {
        self.events.lock().unwrap().push_back(e);
        if !self.wake_posted.swap(true, Ordering::AcqRel) {
            unsafe {
                let _ = PostMessageW(Some(HWND(self.hwnd as *mut _)), WM_QUEUE, WPARAM(0), LPARAM(0));
            }
        }
    }

    fn drain(&self) -> Vec<Event> {
        self.wake_posted.store(false, Ordering::Release);
        self.events.lock().unwrap().drain(..).collect()
    }
}

enum Deferred {
    TrayMenu,
    /// The file, and the hit window of the island it was dragged from.
    DragOut(dragdrop::Payload, HWND),
}

thread_local! {
    static APP: RefCell<Option<App>> = const { RefCell::new(None) };
    static DEFERRED: RefCell<Vec<Deferred>> = const { RefCell::new(Vec::new()) };
}

/// Runs `f` on the app unless it is already borrowed further up the stack, in
/// which case a nested window message simply gets default handling.
pub fn with_app<R>(f: impl FnOnce(&mut App) -> R) -> Option<R> {
    APP.with(|cell| cell.try_borrow_mut().ok().and_then(|mut g| g.as_mut().map(f)))
}

fn defer(d: Deferred) {
    DEFERRED.with(|q| q.borrow_mut().push(d));
}

pub struct App {
    ctrl: HWND,
    visual: HWND,
    hit: HWND,
    tray: Tray,
    taskbar_created: u32,
    renderer: Renderer,
    queue: Arc<Queue>,
    services: Services,
    config: Config,
    saved_tab: String,
    state: AppState,
    views: Vec<Box<dyn View>>,
    tab: usize,

    scale: f32,
    win_x: i32,
    win_y: i32,
    hit_pos: (i32, i32),
    hit_size: (i32, i32),
    hit_radius: i32,

    w: Spring,
    h: Spring,
    radius: Spring,
    op: Spring,
    open: bool,
    pinned: bool,
    hovered: bool,
    hidden: bool,
    drag_hover: bool,
    drag_modal: bool,
    /// A still frame was presented and the display may not have taken it yet.
    pub(crate) in_flight: bool,
    animating: bool,
    last_step: Instant,

    dirty: bool,
    input: Input,
    tracking: bool,
    widgets: WidgetState,
    last_cmds: Vec<Cmd>,
    armed_deadline: Option<u64>,
    stats_on: bool,
    trim_armed: bool,
    trimmed_once: bool,
    render_retries: u32,
    preview: Option<Preview>,
    /// Closed island reduced to a thin bar.
    thin: bool,
    /// The taskbar reported a fullscreen application on this monitor.
    appbar_fullscreen: bool,
    /// The closed island is widened to show a notification.
    wide: bool,
    usage_running: Arc<AtomicBool>,
    last_usage_fetch: Option<Instant>,
    agenda_running: Arc<AtomicBool>,
    agenda_pending: bool,
    last_agenda_sync: Option<Instant>,
    /// Stats strip over a fullscreen application, and the hotkey's choice.
    strip: Option<crate::gamestrip::GameStrip>,
    strip_override: Option<bool>,
    strip_fresh: bool,
    /// Extra card height for the call strip, and the app using the mic.
    call_extra: f32,
    mic_app: Option<String>,
    mic_app_key: Option<u64>,
    /// Display of this island, empty for the home island (see screens.rs).
    monitor: String,
    hmonitor: HMONITOR,
    screens: screens::Screens,
}

/// Registering as an appbar, without reserving any screen space, is how
/// Windows tells a program about fullscreen changes without any polling.
unsafe fn appbar_message(hit: HWND, message: u32) {
    let mut data = APPBARDATA { cbSize: size_of::<APPBARDATA>() as u32, hWnd: hit, uCallbackMessage: WM_APPBAR, ..Default::default() };
    SHAppBarMessage(message, &mut data);
}

/// Runs the app until quit.
pub fn run(demo: bool, start_open: bool, start_tab: Option<String>) -> Result<()> {
    unsafe {
        let instance = GetModuleHandleW(None)?;
        let class = w!("naelbar.window");
        let wc = WNDCLASSEXW {
            cbSize: size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(wndproc),
            hInstance: instance.into(),
            lpszClassName: class,
            hCursor: LoadCursorW(None, IDC_ARROW)?,
            ..Default::default()
        };
        if RegisterClassExW(&wc) == 0 {
            return Err(windows::core::Error::from_thread());
        }

        let topmost = WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_LAYERED;
        let ctrl = CreateWindowExW(WS_EX_TOOLWINDOW, class, w!("naelbar control"), WS_POPUP, 0, 0, 0, 0, None, None, Some(instance.into()), None)?;
        let visual = CreateWindowExW(topmost | WS_EX_TRANSPARENT | WS_EX_NOREDIRECTIONBITMAP, class, w!("naelbar visual"), WS_POPUP, 0, 0, 1, 1, None, None, Some(instance.into()), None)?;
        let hit = CreateWindowExW(topmost, class, w!("naelbar"), WS_POPUP, 0, 0, 1, 1, None, None, Some(instance.into()), None)?;
        let _ = SetLayeredWindowAttributes(visual, COLORREF(0), 255, LWA_ALPHA);
        let _ = SetLayeredWindowAttributes(hit, COLORREF(0), 1, LWA_ALPHA);

        let queue = Arc::new(Queue { events: Mutex::new(VecDeque::new()), wake_posted: AtomicBool::new(false), hwnd: hit.0 as isize });
        let sink: Sink = {
            let q = queue.clone();
            Arc::new(move |e| q.push(e))
        };

        let mut config = Config::load();
        config::apply_appearance(&config);
        crate::widgets::on_config(&config);
        if let Some(t) = start_tab {
            config.last_tab = t;
        }
        let failed = hotkeys::register_all(ctrl, &config.hotkeys);
        for f in failed {
            eprintln!("naelbar: could not register hotkey {f}");
        }

        let views = views::all();
        let tab = views.iter().position(|v| v.name() == config.last_tab).unwrap_or(0);
        let saved_tab = config.last_tab.clone();
        let tray = Tray::add(ctrl);
        crate::updater::start(sink.clone());
        let mut services = Services::start(sink, demo, config.weather_location);

        let (pill_w, pill_h, pill_r) = pill_metrics(&config);
        let mut state = AppState::default();
        state.focus.set_durations(config.focus_minutes, config.break_minutes);
        state.notice_ms = config.notification_seconds as u64 * 1000;
        services.set_lock_keys(state.hud.configure(&config));
        if state.hud.wants_devices() {
            services.read_devices(); // baseline for the device alerts
        }
        services.set_media_prefs(crate::views::media::prefs(&config));
        services.set_activity(config.activity_downloads, config.activity_indicators);
        state.activity.ignore = config.ignore_apps.clone();
        state.weather_configured = config.weather_location.is_some();
        state.claude.load();
        state.agenda = Agenda::load();
        state.shelf = crate::shelf_store::load();
        crate::shelf_store::read_info(state.shelf.clone(), ctrl.0 as isize);
        crate::shelf_store::set_captures(config.shelf_captures, ctrl.0 as isize);
        crate::phone::sync(&config);

        let app = App {
            ctrl,
            visual,
            hit,
            tray,
            taskbar_created: RegisterWindowMessageW(w!("TaskbarCreated")),
            renderer: Renderer::new(visual),
            queue,
            services,
            config,
            saved_tab,
            state,
            views,
            tab,
            scale: 1.0,
            win_x: 0,
            win_y: 0,
            hit_pos: (0, 0),
            hit_size: (0, 0),
            hit_radius: 0,
            w: Spring::new(pill_w, 0.05),
            h: Spring::new(pill_h, 0.05),
            radius: Spring::new(pill_r, 0.05),
            op: Spring::new(0.0, 0.003),
            open: false,
            pinned: false,
            hovered: false,
            hidden: false,
            drag_hover: false,
            drag_modal: false,
            in_flight: false,
            animating: false,
            last_step: Instant::now(),
            dirty: true,
            input: Input::default(),
            tracking: false,
            widgets: WidgetState::default(),
            last_cmds: Vec::new(),
            armed_deadline: None,
            stats_on: false,
            trim_armed: false,
            trimmed_once: false,
            render_retries: 0,
            preview: None,
            thin: false,
            appbar_fullscreen: false,
            wide: false,
            usage_running: Arc::new(AtomicBool::new(false)),
            last_usage_fetch: None,
            agenda_running: Arc::new(AtomicBool::new(false)),
            agenda_pending: false,
            last_agenda_sync: None,
            strip: None,
            strip_override: None,
            strip_fresh: false,
            call_extra: 0.0,
            mic_app: None,
            mic_app_key: None,
            monitor: String::new(),
            hmonitor: HMONITOR::default(),
            screens: screens::Screens::default(),
        };
        APP.with(|c| *c.borrow_mut() = Some(app));

        with_app(|a| {
            a.reposition();
            crate::desktop::sync(&a.config.desktop_widget);
            let _ = ShowWindow(a.visual, SW_SHOWNOACTIVATE);
            let _ = ShowWindow(a.hit, SW_SHOWNOACTIVATE);
            if start_open {
                a.pinned = true;
                a.open_island();
            }
            a.sync_screens();
        });
        dragdrop::register(hit);
        appbar_message(hit, ABM_NEW);

        run_loop();

        with_app(|a| a.drop_screens());

        appbar_message(hit, ABM_REMOVE);
        dragdrop::revoke(hit);
        with_app(|a| {
            hotkeys::unregister_all(a.ctrl);
            a.tray.remove();
            a.save_config();
        });
        crate::desktop::close();
        APP.with(|c| *c.borrow_mut() = None);
    }
    Ok(())
}

fn run_deferred() {
    while let Some(d) = DEFERRED.with(|q| q.borrow_mut().pop()) {
        match d {
            Deferred::TrayMenu => {
                let Some(ctrl) = with_app(|a| a.ctrl) else { continue };
                match tray::show_menu(ctrl) {
                    tray::MENU_SETTINGS => open_settings(0),
                    tray::MENU_QUIT => unsafe { PostQuitMessage(0) },
                    _ => {}
                }
            }
            Deferred::DragOut(path, from) => {
                with_app(|a| a.drag_modal = true);
                dragdrop::drag_out(&path);
                with_app(|a| {
                    a.drag_modal = false;
                    a.with_island_of(from, |a| {
                        a.input.down = false;
                        a.widgets.active = None;
                        a.after_pointer_gesture();
                    });
                });
            }
        }
    }
}

/// The settings window is a separate process that lives only while open.
fn open_settings(section: usize) {
    if let Ok(exe) = std::env::current_exe() {
        let _ = std::process::Command::new(exe).arg("--settings").arg(format!("--section={section}")).spawn();
    }
}

/// Typing needs focus, so it happens in its own short-lived process.
fn open_editor(spec: &str) {
    if let Ok(exe) = std::env::current_exe() {
        let _ = std::process::Command::new(exe).arg(format!("--edit={spec}")).spawn();
    }
}

fn pill_metrics(c: &Config) -> (f32, f32, f32) {
    if c.shape == "notch" {
        return (c.notch_width, c.notch_height, (PILL_R * c.corners).min(c.notch_height * 0.8));
    }
    (PILL_W * c.pill_scale, PILL_H * c.pill_scale, PILL_R * c.pill_scale * c.corners)
}

fn card_metrics(c: &Config) -> (f32, f32, f32) {
    (CARD_W * c.card_scale, CARD_H * c.card_scale, CARD_R * c.card_scale * c.corners)
}

/// Scale of the closed island's content.
fn pill_zoom(c: &Config) -> f32 {
    if c.shape == "notch" { (c.notch_height / PILL_H).clamp(0.55, 1.4) } else { c.pill_scale }
}

/// Dispatches everything queued. Returns false on quit.
unsafe fn pump() -> bool {
    let mut msg = MSG::default();
    while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
        if msg.message == WM_QUIT {
            return false;
        }
        let _ = TranslateMessage(&msg);
        DispatchMessageW(&msg);
        run_deferred();
    }
    true
}

unsafe fn run_loop() {
    const SAVER_FRAME: Duration = Duration::from_millis(33);
    let mut last_frame = Instant::now();
    // A still frame waits until the swapchain can take it. Drawing on every
    // pointer move would queue images ahead of the display, and a slider
    // being dragged would trail behind the pointer.
    // The wait has a fixed budget: a stream of pointer moves must never keep
    // postponing the frame.
    const FRAME_WAIT: Duration = Duration::from_millis(20);
    let mut waiting_since: Option<Instant> = None;
    loop {
        if let Some(h) = with_app(|a| a.dirty_waitable()).flatten() {
            let since = *waiting_since.get_or_insert_with(Instant::now);
            let left = FRAME_WAIT.saturating_sub(since.elapsed()).as_millis() as u32;
            if left > 0 {
                let r = MsgWaitForMultipleObjectsEx(Some(&[h]), left, QS_ALLINPUT, MWMO_INPUTAVAILABLE);
                if r.0 == WAIT_OBJECT_0.0 + 1 {
                    // Input first; it may dirty more before the frame is drawn.
                    if !pump() {
                        return;
                    }
                    continue;
                }
            }
            with_app(|a| a.in_flight = false);
        }
        waiting_since = None;
        // First paint, and anything dirtied by the previous pass.
        with_app(|a| {
            a.each_island(|a| {
                if a.dirty && !a.animating {
                    a.frame();
                }
            })
        });
        run_deferred();

        let animating = with_app(|a| a.any_animating()).unwrap_or(false);
        if animating {
            let saver = with_app(|a| a.saves_power()).unwrap_or(false);
            let handle = if saver { None } else { with_app(|a| a.animating_waitable()).flatten() };
            let handles: Option<&[HANDLE]> = handle.as_ref().map(std::slice::from_ref);
            let timeout = if saver { SAVER_FRAME.saturating_sub(last_frame.elapsed()).as_millis() as u32 } else { 32 };
            let r = MsgWaitForMultipleObjectsEx(handles, timeout, QS_ALLINPUT, MWMO_INPUTAVAILABLE);
            let frame_ready = handle.is_some() && r == WAIT_OBJECT_0;
            if !pump() {
                return;
            }
            let due = if saver { last_frame.elapsed() >= SAVER_FRAME } else { frame_ready || r == WAIT_TIMEOUT };
            if due {
                last_frame = Instant::now();
                with_app(|a| {
                    a.each_island(|a| {
                        if a.animating || a.dirty {
                            a.frame();
                        }
                    })
                });
                run_deferred();
            }
        } else {
            // Parked in the kernel until the OS has something for us.
            let mut msg = MSG::default();
            let got = GetMessageW(&mut msg, None, 0, 0);
            if got.0 <= 0 {
                return;
            }
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
            run_deferred();
            if !pump() {
                return;
            }
        }
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match with_app(|a| a.route_message(hwnd, msg, wp, lp)).flatten() {
        Some(v) => LRESULT(v),
        None => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

fn lo(v: isize) -> i32 {
    (v & 0xFFFF) as i16 as i32
}

fn hi(v: isize) -> i32 {
    ((v >> 16) & 0xFFFF) as i16 as i32
}

impl App {
    fn island(&self) -> Rect {
        let snap = |v: f32| (v * self.scale).round() / self.scale;
        let w = snap(self.w.x);
        let h = snap(self.h.x);
        Rect::new(snap(self.anchor_x(w)), snap(self.top()), w, h)
    }

    fn anchor_x(&self, w: f32) -> f32 {
        match self.config.position.as_str() {
            "left" => SIDE_PAD,
            "right" => VISUAL_W - SIDE_PAD - w,
            _ => (VISUAL_W - w) * 0.5,
        }
    }

    fn is_notch(&self) -> bool {
        self.config.shape == "notch"
    }

    /// Stuck to the top of the screen, square corners hidden above it.
    fn hugs_top(&self) -> bool {
        self.is_notch() || self.thin
    }

    fn top(&self) -> f32 {
        if self.hugs_top() { 0.0 } else { self.config.top_distance }
    }

    /// True when the closed island should get out of the way. Only the
    /// microphone beats a fullscreen application.
    fn wants_thin(&self) -> bool {
        let st = &self.state;
        if st.mic_timer().is_some() || st.claude.has_waiting() || st.claude.finished_until(unix_ms()).is_some() || st.notice(unix_ms()).is_some() || st.hud.visible(unix_ms()).is_some() || st.activity.any_capture() {
            return false;
        }
        // An app in front on another display leaves this island alone.
        if self.appbar_fullscreen || (st.foreground.as_ref().is_some_and(|f| f.fullscreen) && self.foreground_here()) {
            return true;
        }
        if st.activity.line(unix_ms()).is_some() {
            return false;
        }
        let playing = st.media.as_ref().is_some_and(|m| m.status == PlaybackStatus::Playing);
        if playing || st.focus.is_running() {
            return false;
        }
        !st.foreground.as_ref().is_none_or(|f| f.desktop) && self.foreground_here()
    }

    fn closed_metrics(&self) -> (f32, f32, f32) {
        let (w, h, r) = pill_metrics(&self.config);
        if self.thin {
            (w * 0.8, THIN_H, THIN_H * 0.5)
        } else if self.state.hud.visible(unix_ms()).is_some() {
            (w.max(crate::hud::HUD_W * pill_zoom(&self.config)), h, r)
        } else if self.state.notice(unix_ms()).is_some() || self.state.activity.line(unix_ms()).is_some() {
            (w.max(NOTICE_W), h, r)
        } else {
            (w, h, r)
        }
    }

    fn refresh_closed_shape(&mut self) {
        let thin = self.wants_thin();
        let wide = !thin && (self.state.notice(unix_ms()).is_some() || self.state.hud.visible(unix_ms()).is_some() || self.state.activity.line(unix_ms()).is_some());
        if self.open || (thin == self.thin && wide == self.wide) {
            return;
        }
        self.thin = thin;
        self.wide = wide;
        let (w, h, r) = self.closed_metrics();
        let f = self.speed();
        self.w.retarget(w, 24.0 * f, 1.0);
        self.h.retarget(h, 24.0 * f, 1.0);
        self.radius.retarget(r, 24.0 * f, 1.0);
        self.start_animation();
    }

    fn speed(&self) -> f32 {
        match self.config.anim_speed.as_str() {
            "calm" => 0.7,
            "fast" => 1.45,
            _ => 1.0,
        }
    }

    fn saves_power(&self) -> bool {
        match self.config.motion.as_str() {
            "saver" => true,
            "balanced" => self.state.power.is_some_and(|p| p.battery_percent.is_some() && !p.charging),
            _ => false,
        }
    }

    /// The settings window changed the file: apply it without a restart.
    fn reload_config(&mut self) {
        let mut fresh = Config::load();
        fresh.last_tab = self.config.last_tab.clone();
        config::apply_appearance(&fresh);
        crate::widgets::on_config(&fresh);
        crate::launcher::set_slots(fresh.slots.clone());
        self.state.focus.set_durations(fresh.focus_minutes, fresh.break_minutes);
        self.state.notice_ms = fresh.notification_seconds as u64 * 1000;
        let locks = self.state.hud.configure(&fresh);
        self.services.set_lock_keys(locks);
        if self.state.hud.wants_devices() {
            self.services.read_devices();
        } else {
            self.arm_device_poll(false);
        }
        if crate::views::media::prefs(&fresh) != crate::views::media::prefs(&self.config) {
            self.services.set_media_prefs(crate::views::media::prefs(&fresh));
        }
        self.services.set_activity(fresh.activity_downloads, fresh.activity_indicators);
        if !fresh.activity_downloads {
            self.state.activity.clear_downloads();
        }
        self.state.activity.ignore = fresh.ignore_apps.clone();
        if fresh.weather_location != self.config.weather_location {
            self.services.set_weather_location(fresh.weather_location);
            self.state.weather = None;
        }
        self.state.weather_configured = fresh.weather_location.is_some();
        crate::shelf_store::set_captures(fresh.shelf_captures, self.ctrl.0 as isize);
        self.config = fresh;
        crate::phone::sync(&self.config);
        crate::desktop::sync(&self.config.desktop_widget);
        self.strip_override = None;
        self.strip = None;
        self.sync_strip();
        self.relayout();
    }

    /// Resizes and moves this island after a settings change.
    fn relayout(&mut self) {
        if !self.open {
            self.thin = self.wants_thin();
        }
        let (w, h, r) = if self.open { self.open_metrics() } else { self.closed_metrics() };
        if self.animating {
            let f = self.speed();
            self.w.retarget(w, 24.0 * f, 0.9);
            self.h.retarget(h, 24.0 * f, 0.9);
            self.radius.retarget(r, 24.0 * f, 1.0);
        } else {
            self.w.snap(w);
            self.h.snap(h);
            self.radius.snap(r);
        }
        self.reposition();
    }

    fn on_message(&mut self, hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> Option<isize> {
        let is_hit = hwnd == self.hit;
        match msg {
            WM_QUEUE => {
                for e in self.queue.drain() {
                    // A read-back must not pull a slider away from the pointer.
                    if matches!(e, Event::Sonar(_)) && self.input.down {
                        continue;
                    }
                    if let Event::UpdateReady(_) = &e {
                        crate::updater::RESTART.store(true, std::sync::atomic::Ordering::SeqCst);
                        unsafe { PostQuitMessage(0) };
                        continue;
                    }
                    if let Event::YtmRoom(room) = &e {
                        crate::phone::share_ytm_room(room);
                        continue;
                    }
                    if matches!(&e, Event::Notification(n) if nb_integrations::is_ignored_app(&n.app_id, &self.config.ignore_apps)) {
                        continue;
                    }
                    if matches!(&e, Event::Stats(_)) && self.strip.is_some() {
                        self.strip_fresh = true;
                    }
                    let finished = matches!(&e, Event::ClaudeHook(h) if h.agent.is_none() && h.kind == nb_core::ClaudeHookKind::Stop);
                    if let Event::MediaApps(apps) = &e {
                        if crate::views::media::merge_seen(&mut self.config.media_seen, apps) {
                            self.config.save();
                        }
                    }
                    let devices_read = matches!(&e, Event::Devices(_));
                    // Another always-on-top window that came forward (a
                    // launcher, an overlay) would stay above the island:
                    // take the top back whenever the active window changes.
                    if matches!(&e, Event::Foreground(_)) {
                        self.raise();
                    }
                    self.state.apply(e);
                    if devices_read {
                        self.arm_device_poll(self.state.hud.wants_devices() && !self.state.batteries.devices.is_empty());
                    }
                    if finished {
                        self.refresh_usage();
                    }
                }
                self.sync_strip();
                self.dirty = true;
                Some(0)
            }
            // A device arrived or left: battery levels are read again when shown.
            WM_DEVICECHANGE if hwnd == self.ctrl => {
                self.state.batteries.invalidate();
                self.devices_soon();
                self.dirty = true;
                Some(1)
            }
            // Resume from sleep: levels and connections changed meanwhile.
            WM_POWERBROADCAST if hwnd == self.ctrl && matches!(wp.0 as u32, 0x7 | 0x12) => {
                self.devices_soon();
                Some(1)
            }
            m if m == crate::desktop::hello_message() && hwnd == self.ctrl => {
                crate::desktop::push_focus(&self.state.focus, true);
                Some(0)
            }
            WM_APPBAR if is_hit => {
                if wp.0 as u32 == ABN_FULLSCREENAPP as u32 {
                    self.appbar_fullscreen = lp.0 != 0;
                    self.sync_strip();
                    self.dirty = true;
                }
                Some(0)
            }
            crate::shelf_store::WM_SHELF_CHANGED if hwnd == self.ctrl => {
                crate::shelf_store::absorb(&mut self.state.shelf, self.ctrl.0 as isize);
                self.dirty = true;
                Some(0)
            }
            WM_CONFIG_CHANGED if hwnd == self.ctrl => {
                self.reload_config();
                Some(0)
            }
            crate::widgets::WM_WIDGETS_CHANGED if hwnd == self.ctrl => {
                crate::widgets::on_files_changed();
                self.dirty = true;
                Some(0)
            }
            crate::dock::WM_DOCK_READY if hwnd == self.ctrl => {
                self.dirty = true;
                Some(0)
            }
            crate::widgets::WM_ICS_READY if hwnd == self.ctrl => {
                self.dirty = true;
                Some(0)
            }
            WM_AGENDA_CHANGED if hwnd == self.ctrl => {
                self.state.agenda = Agenda::load();
                self.dirty = true;
                // wParam 1 comes from the sync process itself: no echo.
                if wp.0 != 1 {
                    self.request_agenda_sync(true);
                }
                Some(0)
            }
            WM_AGENDA_SYNCED if hwnd == self.ctrl => {
                self.state.agenda_link = match wp.0 {
                    0 => nb_core::ClaudeLink::Connected,
                    2 => nb_core::ClaudeLink::NeedsLogin,
                    _ => nb_core::ClaudeLink::Failed,
                };
                self.state.agenda = Agenda::load();
                self.dirty = true;
                if std::mem::take(&mut self.agenda_pending) && self.state.agenda_link != nb_core::ClaudeLink::NeedsLogin {
                    self.request_agenda_sync(true);
                }
                Some(0)
            }
            WM_MOUSEMOVE if is_hit => {
                self.on_mouse_move(lo(lp.0), hi(lp.0));
                Some(0)
            }
            WM_MOUSELEAVE if is_hit => {
                self.tracking = false;
                self.pointer_left();
                Some(0)
            }
            WM_CAPTURECHANGED if is_hit && self.input.down && !self.drag_modal => {
                // The button release will never reach us.
                self.input.down = false;
                self.after_pointer_gesture();
                self.dirty = true;
                Some(0)
            }
            WM_LBUTTONDOWN if is_hit => {
                self.on_mouse_move(lo(lp.0), hi(lp.0));
                self.input.down = true;
                self.input.pressed = true;
                self.input.press_pos = self.input.mouse.unwrap_or_default();
                unsafe { SetCapture(self.hit) };
                self.dirty = true;
                Some(0)
            }
            WM_LBUTTONUP if is_hit => {
                self.on_mouse_move(lo(lp.0), hi(lp.0));
                self.input.down = false;
                self.input.released = true;
                unsafe {
                    let _ = ReleaseCapture();
                }
                self.after_pointer_gesture();
                self.dirty = true;
                Some(0)
            }
            WM_MOUSEWHEEL if is_hit => {
                self.input.wheel += hi(wp.0 as isize) as f32 / 120.0;
                self.dirty = true;
                Some(0)
            }
            WM_MOUSEACTIVATE if is_hit => Some(MA_NOACTIVATE as isize),
            WM_TIMER => {
                self.on_timer(wp.0);
                Some(0)
            }
            WM_HOTKEY => {
                self.on_hotkey(wp.0 as i32);
                Some(0)
            }
            tray::WM_TRAY => {
                if lo(lp.0) as u32 == WM_RBUTTONUP {
                    defer(Deferred::TrayMenu);
                }
                Some(0)
            }
            WM_DISPLAYCHANGE | WM_DPICHANGED => {
                self.renderer.reset();
                self.reposition();
                Some(0)
            }
            WM_SETTINGCHANGE => None,
            WM_ERASEBKGND => Some(1),
            WM_PAINT => unsafe {
                let _ = ValidateRect(Some(hwnd), None);
                Some(0)
            },
            WM_DESTROY if hwnd == self.ctrl => {
                unsafe { PostQuitMessage(0) };
                Some(0)
            }
            m if m == self.taskbar_created && m != 0 => {
                self.tray.restore();
                self.appbar_fullscreen = false;
                unsafe { appbar_message(self.hit, ABM_NEW) };
                self.dirty = true;
                Some(0)
            }
            _ => None,
        }
    }

    fn on_mouse_move(&mut self, x: i32, y: i32) {
        let px = Pt {
            x: (x + self.hit_pos.0 - self.win_x) as f32 / self.scale,
            y: (y + self.hit_pos.1 - self.win_y) as f32 / self.scale,
        };
        self.input.mouse = Some(px);
        if !self.hovered {
            self.hovered = true;
            self.open_island();
        }
        self.cancel_close();
        if !self.tracking {
            self.track_leave();
        }
        // Rearmed by each move, so it only fires once the pointer has stopped
        // or gone, and costs nothing while nothing happens.
        unsafe { SetTimer(Some(self.hit), TIMER_HOVER_CHECK, HOVER_CHECK_MS, None) };
        self.dirty = true;
    }

    fn pointer_left(&mut self) {
        if !self.input.down {
            self.hovered = false;
            self.input.mouse = None;
            self.dirty = true;
            self.arm_close();
        }
    }

    /// Safety net for a leave notification that never arrived.
    fn pointer_still_here(&self) -> bool {
        let mut p = POINT::default();
        unsafe { GetCursorPos(&mut p).is_err() || WindowFromPoint(p) == self.hit }
    }

    fn track_leave(&mut self) {
        let mut t = TRACKMOUSEEVENT {
            cbSize: size_of::<TRACKMOUSEEVENT>() as u32,
            dwFlags: TME_LEAVE,
            hwndTrack: self.hit,
            dwHoverTime: 0,
        };
        unsafe {
            let _ = TrackMouseEvent(&mut t);
        }
        self.tracking = true;
    }

    /// After a press ends, the pointer may have left while captured.
    pub fn after_pointer_gesture(&mut self) {
        self.tracking = false;
        self.track_leave();
    }

    fn on_timer(&mut self, id: usize) {
        unsafe {
            let _ = KillTimer(Some(self.hit), id);
        }
        match id {
            TIMER_CLOSE => {
                if !self.hovered && !self.pinned && !self.drag_hover && !self.input.down && !self.drag_modal {
                    self.close_island();
                }
            }
            TIMER_HOVER_CHECK => {
                if self.hovered && !self.drag_modal && !self.pointer_still_here() {
                    self.tracking = false;
                    self.pointer_left();
                }
            }
            TIMER_REDRAW => {
                self.armed_deadline = None;
                self.dirty = true;
            }
            TIMER_DEVICES_SOON | TIMER_DEVICES_POLL => {
                if self.state.hud.wants_devices() {
                    self.services.read_devices();
                }
            }
            TIMER_TRIM => {
                self.trim_armed = false;
                self.trimmed_once = true;
                if !self.open && !self.animating {
                    if self.armed_deadline.is_none() {
                        self.renderer.enter_idle();
                    }
                    unsafe {
                        let _ = SetProcessWorkingSetSize(GetCurrentProcess(), usize::MAX, usize::MAX);
                    }
                }
            }
            _ => {}
        }
    }

    fn on_hotkey(&mut self, key: i32) {
        match key {
            hotkeys::HK_TOGGLE => {
                if self.open {
                    self.pinned = false;
                    self.close_island();
                } else {
                    self.pinned = true;
                    self.open_island();
                }
            }
            hotkeys::HK_PLAY_PAUSE => self.services.execute(&Command::MediaToggle),
            hotkeys::HK_TIMER => {
                self.state.focus.toggle(unix_ms());
                self.dirty = true;
            }
            hotkeys::HK_HIDE => self.set_hidden(!self.hidden),
            hotkeys::HK_GAME => {
                self.strip_override = Some(crate::gamestrip::hotkey_override(self.strip.is_some()));
                self.sync_strip();
                self.dirty = true;
            }
            hotkeys::HK_MIC => {
                let muted = self.state.audio.as_ref().is_some_and(|a| a.mic_muted);
                self.services.execute(&Command::SetMicMuted(!muted));
            }
            _ => {}
        }
    }

    /// The island's card size, taller while the call strip is showing.
    fn open_metrics(&self) -> (f32, f32, f32) {
        let (w, h, r) = card_metrics(&self.config);
        (w, h + self.call_extra * self.config.card_scale, r)
    }

    /// Follows the microphone: grows the open card for the call strip and
    /// looks the app name up once per call, while the card is open.
    fn sync_call(&mut self) {
        let in_call = self.state.mic_timer().is_some();
        let extra = views::call::extra_height(in_call, self.config.card_scale, VISUAL_H - self.top() - 8.0);
        let key = if self.open && in_call { self.state.mic_timer() } else { None };
        if key != self.mic_app_key {
            self.mic_app_key = key;
            self.mic_app = key.and_then(|_| views::call::mic_app());
        }
        if extra != self.call_extra {
            self.call_extra = extra;
            if self.open {
                let (_, h, _) = self.open_metrics();
                let f = self.speed();
                self.h.retarget(h, 24.0 * f, 0.78);
                self.start_animation();
            }
        }
    }

    /// Creates or destroys the game strip to match the setting, the hotkey
    /// and the fullscreen state, and repaints it.
    fn sync_strip(&mut self) {
        // The strip belongs to the chosen display, which is the home island's.
        if !self.is_home() {
            return;
        }
        let fullscreen = self.appbar_fullscreen || (self.state.foreground.as_ref().is_some_and(|f| f.fullscreen) && self.foreground_here());
        let enabled = self.strip_override.unwrap_or(self.config.game_strip);
        if !crate::gamestrip::should_show(enabled, fullscreen) {
            self.strip = None;
            return;
        }
        if self.strip.is_none() {
            self.strip_fresh = false;
            self.strip = crate::gamestrip::GameStrip::create(&self.config.monitor, crate::gamestrip::on_left(&self.config.game_strip_corner));
        }
        let stats = if self.strip_fresh { self.state.stats.as_ref() } else { None };
        if let Some(s) = &mut self.strip {
            s.update(stats);
        }
    }

    fn set_hidden(&mut self, hidden: bool) {
        self.hidden = hidden;
        unsafe {
            let cmd = if hidden { SW_HIDE } else { SW_SHOWNOACTIVATE };
            let _ = ShowWindow(self.visual, cmd);
            let _ = ShowWindow(self.hit, cmd);
        }
        if hidden {
            self.pinned = false;
            self.close_island();
        }
    }

    fn drag_enter_here(&mut self) {
        self.drag_hover = true;
        if let Some(i) = self.views.iter().position(|v| v.name() == "shelf") {
            self.tab = i;
        }
        self.cancel_close();
        self.open_island();
        self.dirty = true;
    }

    fn drag_leave_here(&mut self) {
        self.drag_hover = false;
        self.dirty = true;
        self.arm_close();
    }

    fn drop_here(&mut self, files: Vec<PathBuf>) {
        self.drag_hover = false;
        crate::shelf_store::add(&mut self.state.shelf, files, self.ctrl.0 as isize);
        self.dirty = true;
        self.arm_close();
    }

    fn arm_close(&mut self) {
        if !self.hovered && !self.pinned && !self.drag_hover && !self.drag_modal {
            unsafe { SetTimer(Some(self.hit), TIMER_CLOSE, CLOSE_GRACE_MS, None) };
        }
    }

    fn cancel_close(&mut self) {
        unsafe {
            let _ = KillTimer(Some(self.hit), TIMER_CLOSE);
        }
    }

    fn reduced_motion() -> bool {
        let mut enabled = BOOL(1);
        unsafe {
            let _ = SystemParametersInfoW(SPI_GETCLIENTAREAANIMATION, 0, Some(&mut enabled as *mut BOOL as *mut _), SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0));
        }
        !enabled.as_bool()
    }

    /// Bluetooth battery alerts: one read shortly after a device change or a
    /// resume (a new call restarts the 3 s wait).
    fn devices_soon(&mut self) {
        if self.state.hud.wants_devices() {
            unsafe { SetTimer(Some(self.hit), TIMER_DEVICES_SOON, DEVICES_SOON_MS, None) };
        }
    }

    /// The only periodic check: every 10 min, and only while a device with a
    /// battery is connected (levels change without any notification).
    fn arm_device_poll(&mut self, on: bool) {
        unsafe {
            if on {
                SetTimer(Some(self.hit), TIMER_DEVICES_POLL, DEVICES_POLL_MS, None);
            } else {
                let _ = KillTimer(Some(self.hit), TIMER_DEVICES_POLL);
            }
        }
    }

    fn open_island(&mut self) {
        if self.state.hud.wants_devices() {
            self.services.request_devices();
        }
        if self.trim_armed {
            self.trim_armed = false;
            unsafe {
                let _ = KillTimer(Some(self.hit), TIMER_TRIM);
            }
        }
        if self.open {
            return;
        }
        self.open = true;
        self.thin = false;
        if self.state.claude.has_waiting() {
            if let Some(i) = self.views.iter().position(|v| v.name() == "claude") {
                self.tab = i;
            }
        }
        self.sync_call();
        let (w, h, r) = self.open_metrics();
        let f = self.speed();
        self.w.retarget(w, 24.0 * f, 0.78);
        self.h.retarget(h, 24.0 * f, 0.78);
        self.radius.retarget(r, 24.0 * f, 1.0);
        self.op.retarget(1.0, 20.0 * f, 1.0);
        self.start_animation();
    }

    fn close_island(&mut self) {
        if !self.open {
            return;
        }
        self.open = false;
        self.thin = self.wants_thin();
        let (w, h, r) = self.closed_metrics();
        let f = self.speed();
        self.w.retarget(w, 28.0 * f, 1.0);
        self.h.retarget(h, 28.0 * f, 1.0);
        self.radius.retarget(r, 28.0 * f, 1.0);
        self.op.retarget(0.0, 34.0 * f, 1.0);
        self.start_animation();
    }

    fn start_animation(&mut self) {
        if Self::reduced_motion() {
            for s in [&mut self.w, &mut self.h, &mut self.radius, &mut self.op] {
                let t = s.target;
                s.snap(t);
            }
            self.dirty = true;
            self.animating = false;
            self.on_settled();
            return;
        }
        if !self.animating {
            self.animating = true;
            self.last_step = Instant::now();
        }
    }

    fn springs_settled(&self) -> bool {
        self.w.settled() && self.h.settled() && self.radius.settled() && self.op.settled()
    }

    fn on_settled(&mut self) {
        // The saved tab follows the home island only.
        if !self.open && self.is_home() {
            if self.views[self.tab].name() != self.saved_tab {
                self.save_config();
            }
        }
    }

    fn save_config(&mut self) {
        self.config.last_tab = self.views[self.tab].name().to_string();
        self.saved_tab = self.config.last_tab.clone();
        self.config.save();
    }

    /// Puts both island windows back at the top of the always-on-top band,
    /// without moving, sizing or activating them.
    fn raise(&self) {
        let flags = SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_NOOWNERZORDER;
        unsafe {
            let _ = SetWindowPos(self.visual, Some(HWND_TOPMOST), 0, 0, 0, 0, flags);
            let _ = SetWindowPos(self.hit, Some(HWND_TOPMOST), 0, 0, 0, 0, flags);
        }
    }

    fn reposition(&mut self) {
        unsafe {
            let Some(monitor) = crate::monitors::pick(self.monitor_name()) else { return };
            self.hmonitor = monitor.handle;
            let (mut dpi_x, mut dpi_y) = (96u32, 96u32);
            let _ = GetDpiForMonitor(monitor.handle, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y);
            self.scale = dpi_x as f32 / 96.0;
            let w = (VISUAL_W * self.scale).round() as i32;
            let h = (VISUAL_H * self.scale).round() as i32;
            let m = monitor.rect;
            let inset = ((self.config.edge_margin - SIDE_PAD) * self.scale).round() as i32;
            self.win_x = match self.config.position.as_str() {
                "left" => m.left + inset,
                "right" => m.right - w - inset,
                _ => m.left + (m.right - m.left - w) / 2,
            };
            self.win_y = m.top;
            let _ = SetWindowPos(self.visual, Some(HWND_TOPMOST), self.win_x, self.win_y, w, h, SWP_NOACTIVATE);
            let affinity = if self.config.hide_from_capture { WDA_EXCLUDEFROMCAPTURE } else { WDA_NONE };
            let _ = SetWindowDisplayAffinity(self.visual, affinity);
            let _ = SetWindowDisplayAffinity(self.hit, affinity);
            self.renderer.set_size(w as u32, h as u32, self.scale);
        }
        self.last_cmds.clear();
        self.hit_size = (0, 0);
        self.sync_hit_window();
        unsafe {
            let _ = SetWindowPos(self.hit, Some(HWND_TOPMOST), 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
        }
        self.dirty = true;
    }

    fn sync_hit_window(&mut self) {
        let r = self.island();
        let x = self.win_x + (r.x * self.scale).round() as i32;
        let y = self.win_y + (r.y * self.scale).round() as i32;
        let w = (r.w * self.scale).round() as i32;
        let h = (r.h * self.scale).round() as i32;
        let radius = (self.radius.x * self.scale).round() as i32;
        if (x, y) == self.hit_pos && (w, h) == self.hit_size && radius == self.hit_radius {
            return;
        }
        let sized = (w, h) != self.hit_size || radius != self.hit_radius;
        self.hit_pos = (x, y);
        self.hit_size = (w, h);
        self.hit_radius = radius;
        unsafe {
            let _ = SetWindowPos(self.hit, None, x, y, w, h, SWP_NOACTIVATE | SWP_NOZORDER | SWP_NOREDRAW | SWP_NOCOPYBITS);
            if sized {
                // A notch has square top corners: push the rounded ones off the window.
                let top = if self.hugs_top() { -2 * radius } else { 0 };
                let rgn = CreateRoundRectRgn(0, top, w + 1, h + 1, radius * 2, radius * 2);
                // The system owns the region after this call.
                SetWindowRgn(self.hit, Some(rgn), false);
            }
        }
    }

    /// Advances animation if running, rebuilds the display list, and presents
    /// only if the list differs from what is on screen.
    fn frame(&mut self) {
        self.dirty = false;
        let now_ms = unix_ms();
        if self.preview.is_some() && !self.input.down {
            self.preview = None;
        }

        if self.animating {
            let now = Instant::now();
            let dt = now.duration_since(self.last_step).as_secs_f32().min(0.05);
            self.last_step = now;
            self.w.step(dt);
            self.h.step(dt);
            self.radius.step(dt);
            self.op.step(dt);
        }

        self.state.focus.advance(now_ms);
        self.sync_call();
        self.refresh_closed_shape();

        let (cmds, commands, deadline) = self.build(now_ms);
        let mut presented = false;
        if cmds != self.last_cmds {
            match self.renderer.render(&cmds) {
                Ok(()) => {
                    self.last_cmds = cmds;
                    self.render_retries = 0;
                    presented = true;
                    self.in_flight = !self.animating;
                }
                Err(_) => {
                    // Device loss was handled by the renderer; retry a few
                    // times so a lost device recovers without a new event.
                    self.last_cmds.clear();
                    self.render_retries += 1;
                    if self.render_retries <= MAX_RENDER_RETRIES {
                        self.dirty = true;
                    }
                }
            }
        }
        self.sync_hit_window();

        self.screens.touched |= !commands.is_empty();
        for c in commands {
            self.run_command(c);
        }
        crate::desktop::push_focus(&self.state.focus, false);
        if self.open && self.views[self.tab].name() == "claude" {
            self.refresh_usage();
        }
        if self.open && self.views[self.tab].name() == "agenda" {
            self.request_agenda_sync(false);
        }

        let mut settled_now = false;
        if self.animating && self.springs_settled() {
            self.animating = false;
            settled_now = true;
            self.on_settled();
        }

        // Stats sampling follows widget-tab visibility.
        let want_stats = (self.open && self.views[self.tab].wants_stats()) || self.strip.is_some() || self.others_want_stats();
        if want_stats != self.stats_on {
            self.stats_on = want_stats;
            self.services.execute(&Command::StatsSubscribe(want_stats));
        }

        self.arm_redraw(deadline);

        // A few seconds after settling closed, trim. While something keeps
        // redrawing the pill, only do it once per close, not after each redraw.
        let rearm = (presented && (deadline.is_none() || !self.trimmed_once)) || settled_now;
        if rearm && !self.open && !self.animating && !self.trim_armed {
            self.trim_armed = true;
            unsafe { SetTimer(Some(self.hit), TIMER_TRIM, TRIM_DELAY_MS, None) };
        }
    }

    fn run_command(&mut self, c: Command) {
        match c {
            Command::FocusToggle => {
                self.state.focus.toggle(unix_ms());
                self.dirty = true;
            }
            Command::FocusReset => {
                self.state.focus.reset();
                self.dirty = true;
            }
            Command::OpenSettings(section) => open_settings(section),
            Command::OpenPowerSettings => unsafe {
                let _ = windows::Win32::UI::Shell::ShellExecuteW(None, w!("open"), w!("ms-settings:powersleep"), windows::core::PCWSTR::null(), windows::core::PCWSTR::null(), SW_SHOWNORMAL);
            },
            Command::PreviewClip(clip) => self.show_preview(clip),
            Command::IgnoreApp(name) => {
                if crate::activity::add_ignored(&mut self.config.ignore_apps, &name) {
                    self.config.save();
                    self.state.activity.ignore = self.config.ignore_apps.clone();
                    self.dirty = true;
                }
            }
            Command::SonarVolume(channel, v) => {
                // Shown at once; Sonar is read back once the writes are done.
                if let Some(c) = self.state.sonar.iter_mut().find(|c| c.id == channel) {
                    c.volume = v;
                }
                self.dirty = true;
                self.services.execute(&Command::SonarVolume(channel, v));
            }
            Command::OpenAgenda => {
                if let Some(i) = self.views.iter().position(|v| v.name() == "agenda") {
                    self.tab = i;
                    self.dirty = true;
                }
            }
            Command::AgendaNewEvent(date) => open_editor(&format!("event:{date}")),
            Command::AgendaNewNote => open_editor("note"),
            Command::OpenAgendaWindow => {
                if let Ok(exe) = std::env::current_exe() {
                    let _ = std::process::Command::new(exe).arg("--agenda-window").spawn();
                }
            }
            Command::OpenEditor(spec) => open_editor(&spec),
            Command::AgendaDeleteEvent(id) => self.change_agenda(|a, now| a.delete_event(&id, now)),
            Command::AgendaDeleteNote(id) => self.change_agenda(|a, now| a.delete_note(&id, now)),
            Command::AgendaToggleNote(id) => self.change_agenda(|a, now| a.toggle_note(&id, now)),
            Command::ClaudeLogin => self.spawn_usage_process("--claude-login"),
            Command::AgendaLogin => self.spawn_agenda_process("--agenda-login"),
            Command::ClaudeDismiss(session) => {
                self.state.claude.dismiss(&session);
                self.dirty = true;
            }
            Command::ShelfRemove(i) => {
                if crate::shelf_store::remove(&mut self.state.shelf, i) {
                    self.dirty = true;
                }
            }
            Command::ShelfShare(i) => {
                if let Some(p) = self.state.shelf.get(i).cloned() {
                    crate::share::start(p, self.ctrl.0 as isize);
                    self.dirty = true;
                }
            }
            Command::ShelfShareStop => {
                crate::share::stop();
                self.dirty = true;
            }
            Command::ShelfDragOut(i) => {
                if let Some(p) = self.state.shelf.get(i).cloned() {
                    unsafe {
                        let _ = ReleaseCapture();
                    }
                    self.input.down = false;
                    defer(Deferred::DragOut(dragdrop::Payload::Files(vec![p]), self.hit));
                }
            }
            Command::ClipDragOut(id) => {
                let payload = self.state.clips.iter().find(|c| c.id == id).and_then(|c| match &c.content {
                    nb_core::ClipContent::Text(t) => Some(dragdrop::Payload::Text(t.clone())),
                    nb_core::ClipContent::Files(f) => Some(dragdrop::Payload::Files(f.clone())),
                    nb_core::ClipContent::Image { png, .. } => dragdrop::image_file(id, png).map(|p| dragdrop::Payload::Files(vec![p])),
                });
                if let Some(payload) = payload {
                    unsafe {
                        let _ = ReleaseCapture();
                    }
                    self.input.down = false;
                    defer(Deferred::DragOut(payload, self.hit));
                }
            }
            Command::OpenDownload(path) => crate::activity::reveal(&path),
            Command::ClearNotifications => {
                self.state.notifications.clear();
                self.state.notice_at = 0;
                self.dirty = true;
            }
            Command::OpenNotification(app_id) => {
                // Only the id goes after the fixed prefix: no shell is involved.
                if valid_aumid(&app_id) {
                    let _ = std::process::Command::new("explorer.exe").arg(format!("shell:AppsFolder\\{app_id}")).spawn();
                }
            }
            Command::ClearClips => {
                self.state.clips.retain(|c| c.pinned);
                self.services.execute(&Command::ClearClips);
                self.dirty = true;
            }
            Command::PinClip(id, on) => {
                if let Some(c) = self.state.clips.iter_mut().find(|c| c.id == id) {
                    c.pinned = on;
                }
                self.services.execute(&Command::PinClip(id, on));
                self.dirty = true;
            }
            Command::DeleteClip(id) => {
                self.state.clips.retain(|c| c.id != id);
                self.services.execute(&Command::DeleteClip(id));
                self.dirty = true;
            }
            other => self.services.execute(&other),
        }
    }

    /// Applies a change on top of the file, which the editor process may have
    /// just written. The copy in memory covers a read that failed.
    fn change_agenda(&mut self, change: impl FnOnce(&mut Agenda, u64)) {
        let mut agenda = Agenda::load();
        agenda.merge(&self.state.agenda);
        change(&mut agenda, unix_ms());
        if let Err(e) = agenda.save() {
            eprintln!("naelbar: could not save the agenda: {e}");
        }
        self.state.agenda = agenda;
        self.dirty = true;
        self.request_agenda_sync(true);
    }

    /// Syncs on an event: `force` for a local change, otherwise at most once
    /// per [`AGENDA_SYNC_MIN_AGE`]. A request during a sync runs once afterwards.
    fn request_agenda_sync(&mut self, force: bool) {
        if self.agenda_running.load(Ordering::Acquire) {
            // Only a local change is worth a second run; the tab-visible request is not.
            self.agenda_pending |= pending_on_busy(force);
            return;
        }
        if self.state.agenda_link == nb_core::ClaudeLink::NeedsLogin
            || (!force && self.last_agenda_sync.is_some_and(|t| t.elapsed() < AGENDA_SYNC_MIN_AGE))
        {
            return;
        }
        self.spawn_agenda_process("--agenda-sync");
    }

    /// Like the usage reader: a short-lived process, and a thread that only waits for it.
    fn spawn_agenda_process(&mut self, flag: &'static str) {
        if self.agenda_running.swap(true, Ordering::AcqRel) {
            return;
        }
        self.last_agenda_sync = Some(Instant::now());
        let running = self.agenda_running.clone();
        let ctrl = self.ctrl.0 as isize;
        std::thread::spawn(move || {
            let code = std::env::current_exe().ok().and_then(|exe| std::process::Command::new(exe).arg(flag).status().ok()).and_then(|s| s.code());
            running.store(false, Ordering::Release);
            unsafe {
                let _ = PostMessageW(Some(HWND(ctrl as *mut _)), WM_AGENDA_SYNCED, WPARAM(code.unwrap_or(3) as usize), LPARAM(0));
            }
        });
    }

    /// Reads the limits unless that was done recently or a sign-in is needed.
    fn refresh_usage(&mut self) {
        if self.state.claude.link == nb_core::ClaudeLink::NeedsLogin || self.last_usage_fetch.is_some_and(|t| t.elapsed() < USAGE_MIN_AGE) {
            return;
        }
        self.spawn_usage_process("--claude-usage");
    }

    /// The reader is a short-lived process; this thread only waits for it.
    fn spawn_usage_process(&mut self, flag: &'static str) {
        if self.usage_running.swap(true, Ordering::AcqRel) {
            return;
        }
        self.last_usage_fetch = Some(Instant::now());
        let running = self.usage_running.clone();
        let queue = self.queue.clone();
        std::thread::spawn(move || {
            let code = std::env::current_exe().ok().and_then(|exe| std::process::Command::new(exe).arg(flag).status().ok()).and_then(|s| s.code());
            running.store(false, Ordering::Release);
            queue.push(Event::ClaudeLink(match code {
                Some(0) => nb_core::ClaudeLink::Connected,
                Some(2) => nb_core::ClaudeLink::NeedsLogin,
                _ => nb_core::ClaudeLink::Failed,
            }));
        });
    }

    fn show_preview(&mut self, clip: u64) {
        if !self.input.down {
            return;
        }
        let Some(nb_core::ClipContent::Image { png, width, height }) = self.state.clips.iter().find(|c| c.id == clip).map(|c| &c.content) else { return };
        let Some(monitor) = crate::monitors::pick(self.monitor_name()) else { return };
        self.preview =preview::show(monitor.rect, self.scale, png, *width, *height);
    }

    /// One-shot timer aimed at the next instant the display would change.
    fn arm_redraw(&mut self, deadline: Option<u64>) {
        if deadline == self.armed_deadline {
            return;
        }
        self.armed_deadline = deadline;
        unsafe {
            let _ = KillTimer(Some(self.hit), TIMER_REDRAW);
            if let Some(d) = deadline {
                let delay = d.saturating_sub(unix_ms()).clamp(1, i32::MAX as u64) as u32;
                SetTimer(Some(self.hit), TIMER_REDRAW, delay, None);
            }
        }
    }

    fn build(&mut self, now_ms: u64) -> (Vec<Cmd>, Vec<Command>, Option<u64>) {
        let island = self.island();
        let op = self.op.x.clamp(0.0, 1.0);
        let radius = self.radius.x.max(0.0);
        let mut views = std::mem::take(&mut self.views);
        let mut out = Vec::new();
        let mut tab = self.tab;

        let call_extra = self.call_extra;
        let k = self.config.card_scale;
        let origin = crate::ui::Pt { x: self.anchor_x(CARD_W * k), y: island.y };
        let unzoom = |p: crate::ui::Pt| crate::ui::Pt { x: origin.x + (p.x - origin.x) / k, y: origin.y + (p.y - origin.y) / k };
        let card_input = Input { mouse: self.input.mouse.map(unzoom), press_pos: unzoom(self.input.press_pos), ..self.input.clone() };
        // A notch extends above the screen, which hides its top corners.
        let body = if self.hugs_top() { Rect::new(island.x, island.y - radius, island.w, island.h + radius) } else { island };

        let mut ui = Ui::new(&self.input, &mut self.widgets, &mut out, self.scale, now_ms);
        ui.drag_hover = self.drag_hover;

        ui.fill(body, radius, island_fill());
        ui.stroke(body.inset(0.5), radius, 1.0, island_edge());

        // Open card, cross-fading over the pill.
        if op > 0.01 {
            let card = Rect::new(origin.x, origin.y, CARD_W, CARD_H + call_extra);
            ui.push_clip(body, radius);
            ui.set_alpha(op);
            ui.set_zoom(origin, k);
            ui.input = &card_input;

            let bar = Rect::new(card.x, card.y + 6.0, card.w, TAB_ROW_H - 6.0);
            // Tabs narrow as more are added, and stay clear of the gear.
            // Centred while it fits; otherwise it starts at the left edge, so
            // the row never runs under the gear (which sits at right - 44).
            let cell_w = 44.0f32.min((card.w - 64.0) / views.len() as f32);
            let row_w = cell_w * views.len() as f32;
            let row_x = (bar.center().x - row_w * 0.5).min(card.right() - 50.0 - row_w).max(card.x + 10.0);
            for (i, view) in views.iter().enumerate() {
                let cell = Rect::new(row_x + i as f32 * cell_w, bar.y, cell_w, bar.h);
                let resp = ui.interact(id("tab", i), cell);
                if resp.clicked {
                    tab = i;
                }
                let active = i == tab;
                let color = if active { accent() } else if resp.hovered { TEXT } else { TEXT_DIM };
                ui.icon(view.icon(), Rect::new(cell.center().x - 10.0, cell.y + 6.0, 20.0, 20.0), color);
                if active {
                    let c = cell.center().x;
                    ui.fill(Rect::new(c - 2.0, cell.bottom() - 4.0, 4.0, 4.0), 2.0, accent());
                }
            }

            let gear = Rect::new(card.right() - 44.0, bar.y + 1.0, 30.0, 30.0);
            if ui.icon_button(id("settings", 0), gear, Icon::Gear, 18.0, TEXT_DIM).clicked {
                ui.out.push(Command::OpenSettings(0));
            }

            let mut area = Rect::new(card.x + 16.0, card.y + TAB_ROW_H + 6.0, card.w - 32.0, card.h - TAB_ROW_H - 18.0);
            if call_extra > 0.0 {
                // Ignored apps (Sonar's virtual mic, for one) are not the one named.
                let named = self.state.activity.mic_app().map(|c| c.name.clone()).or_else(|| self.mic_app.clone());
                views::call::draw(&mut ui, &self.state, area, named.as_deref());
                area = Rect::new(area.x, area.y + call_extra, area.w, area.h - call_extra);
            }
            views[tab].build(&mut ui, &self.state, area);
            ui.clear_zoom();
            ui.input = &self.input;
            ui.pop_clip();
            ui.set_alpha(1.0);
        }

        if op < 0.34 && !self.thin && !self.hidden {
            ui.set_alpha((1.0 - op * 3.0).clamp(0.0, 1.0));
            let p = pill_zoom(&self.config);
            let pill_origin = crate::ui::Pt { x: island.x, y: island.y };
            ui.set_zoom(pill_origin, p);
            // One audio meter: only the home island draws the visualizer.
            let viz = if self.open || self.hidden || !self.monitor.is_empty() { None } else { crate::visualizer::style(&self.config.visualizer) };
            pill_content(&mut ui, &self.state, Rect::new(island.x, island.y, island.w / p, island.h / p), &self.config.idle_show, viz);
            ui.clear_zoom();
            ui.set_alpha(1.0);
        }
        // Releases the audio meter when the visualizer was not drawn.
        if self.monitor.is_empty() {
            crate::visualizer::end_frame();
        }

        if !self.hidden {
            if let Some(d) = self.state.focus.deadline() {
                ui.redraw_at(d);
            }
        }
        // A hidden island does no work: no deadline at all.
        let deadline = if self.hidden { None } else { ui.deadline };
        let cmds = std::mem::take(&mut ui.cmds);
        drop(ui);

        self.views = views;
        self.tab = tab;
        self.input.pressed = false;
        self.input.released = false;
        self.input.wheel = 0.0;
        // A held widget may have vanished (tab change, hotkey): never leave it stuck.
        if !self.input.down {
            self.widgets.active = None;
        }
        (cmds, out, deadline)
    }
}

/// Whether a sync request made while one is running must be replayed afterwards.
fn pending_on_busy(force: bool) -> bool {
    force
}

/// Only letters, digits and `. _ ! { } \ -` may reach explorer.exe as an AUMID.
fn valid_aumid(id: &str) -> bool {
    !id.is_empty() && id.len() <= 512 && id.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '!' | '{' | '}' | '\\' | '-'))
}

/// Compact content while the island is closed. The clock is opt-in: by default
/// an idle pill has nothing that needs a timer.
fn pill_content(ui: &mut Ui, st: &AppState, island: Rect, idle_show: &str, viz: Option<crate::visualizer::Style>) {
    pill_body(ui, st, island, idle_show, viz);
    crate::activity::draw_dots(ui, &st.activity, island, st.mic_timer().is_some());
}

fn pill_body(ui: &mut Ui, st: &AppState, island: Rect, idle_show: &str, viz: Option<crate::visualizer::Style>) {
    // A HUD answers what the user just did, so it goes before everything.
    if let Some((kind, until)) = st.hud.visible(ui.now_ms) {
        crate::hud::draw(ui, kind, until, island);
        return;
    }
    let claude_label = if st.claude.has_waiting() {
        Some(crate::ui::tr("À valider", "Approve"))
    } else {
        st.claude.finished_until(ui.now_ms).map(|until| {
            ui.redraw_at(until);
            crate::ui::tr("Terminé", "Done")
        })
    };
    if let Some(label) = claude_label {
        let icon = Rect::new(island.x + 12.0, island.y + (island.h - 16.0) * 0.5, 16.0, 16.0);
        ui.icon(Icon::Spark, icon, accent());
        let text = Rect::new(island.x + 34.0, island.y, island.w - 46.0, island.h);
        ui.text(text, label, TextStyle::new(12.5, 500, TEXT));
        return;
    }
    if let Some((n, until)) = st.notice(ui.now_ms) {
        ui.redraw_at(until);
        let icon = Rect::new(island.x + 12.0, island.y + (island.h - 16.0) * 0.5, 16.0, 16.0);
        ui.icon(Icon::Bell, icon, accent());
        let name = if n.source.is_empty() { "Windows" } else { n.source.as_str() };
        let line = if n.title.is_empty() { n.body.as_str() } else { n.title.as_str() };
        let x = island.x + 34.0;
        let w = island.w - 46.0;
        ui.text(Rect::new(x, island.y + island.h * 0.5 - 14.0, w, 14.0), name, TextStyle::new(10.0, 500, TEXT_DIM));
        ui.text(Rect::new(x, island.y + island.h * 0.5, w, 15.0), line, TextStyle::new(11.5, 500, TEXT));
        return;
    }
    if let Some(since) = st.mic_timer() {
        let icon = Rect::new(island.x + 12.0, island.y + (island.h - 16.0) * 0.5, 16.0, 16.0);
        ui.icon(Icon::Mic, icon, MIC_GREEN);
        let elapsed = ui.now_ms.saturating_sub(since);
        let text = Rect::new(island.x + 34.0, island.y, island.w - 46.0, island.h);
        ui.text(text, &crate::ui::format_time(elapsed), TextStyle::new(12.5, 500, TEXT));
        ui.text(text, crate::ui::tr("actif", "active"), TextStyle::new(11.5, 500, MIC_GREEN).aligned(Align::End));
        ui.redraw_next_second(elapsed);
        return;
    }
    if let Some(line) = st.activity.line(ui.now_ms) {
        crate::activity::draw_download(ui, &line, island);
        return;
    }
    let media = st.media.as_ref().filter(|m| m.status != PlaybackStatus::Stopped);
    if let Some(m) = media {
        let art = Rect::new(island.x + 8.0, island.y + (island.h - 18.0) * 0.5, 18.0, 18.0);
        if let Some(bytes) = &m.artwork {
            ui.image(art, 5.0, bytes);
        } else {
            ui.icon(Icon::Note, art, accent());
        }
        let playing = m.status == PlaybackStatus::Playing;
        // Right of the artwork while playing; the progress bar moves over.
        let mut left = 46.0;
        if let Some(style) = viz.filter(|_| playing) {
            crate::visualizer::draw(ui, art.right() + 7.0, island.center().y, style);
            // Artwork, gap, visualizer, gap, then the 12 DIP right margin.
            left = 8.0 + 18.0 + 7.0 + crate::visualizer::width(style) + 8.0 + 12.0;
        }
        // The notch can be narrower than the pill: the bar takes what is left.
        let bar_w = (island.w - left).min(72.0);
        if bar_w >= 16.0 {
            let bar = Rect::new(island.right() - 12.0 - bar_w, island.center().y - 1.5, bar_w, 3.0);
            ui.progress(bar, st.media_position(ui.now_ms), m.duration_ms, playing, accent());
        }
        return;
    }
    if st.focus.is_running() {
        let remaining = st.focus.remaining(ui.now_ms);
        let icon = Rect::new(island.x + 12.0, island.y + (island.h - 16.0) * 0.5, 16.0, 16.0);
        ui.icon(Icon::Timer, icon, accent());
        let minutes = remaining.div_ceil(60_000);
        let text = Rect::new(island.x + 34.0, island.y, island.w - 46.0, island.h);
        ui.text(text, &format!("{minutes} min"), TextStyle::new(12.0, 500, TEXT).aligned(Align::End));
        ui.redraw_at(ui.now_ms + (remaining.saturating_sub(1) % 60_000) + 1);
        return;
    }
    if idle_show == "clock" || idle_show == "date" {
        let t = unsafe { windows::Win32::System::SystemInformation::GetLocalTime() };
        let time = format!("{:02}:{:02}", t.wHour, t.wMinute);
        let label = if idle_show == "date" {
            const DAYS_FR: [&str; 7] = ["dim.", "lun.", "mar.", "mer.", "jeu.", "ven.", "sam."];
            const DAYS_EN: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
            let days = if crate::ui::is_english() { DAYS_EN } else { DAYS_FR };
            format!("{} {}  {}", days[t.wDayOfWeek as usize % 7], t.wDay, time)
        } else {
            time
        };
        ui.text(island, &label, TextStyle::new(12.5, 500, TEXT).aligned(Align::Center));
        let into_minute = t.wSecond as u64 * 1000 + t.wMilliseconds as u64;
        ui.redraw_at(ui.now_ms + (60_000 - into_minute).max(1));
    }
}

#[cfg(test)]
mod fix_tests {
    use super::*;

    #[test]
    fn only_a_forced_request_is_replayed_after_a_running_sync() {
        assert!(pending_on_busy(true));
        assert!(!pending_on_busy(false));
    }

    #[test]
    fn aumid_allow_list() {
        assert!(valid_aumid("Microsoft.WindowsCalculator_8wekyb3d8bbwe!App"));
        assert!(valid_aumid("{1AC14E77-02E7-4E5D-B744-2EB1AE5198B7}\\notepad.exe"));
        assert!(!valid_aumid(""));
        assert!(!valid_aumid("a b"));
        assert!(!valid_aumid("a&calc"));
        assert!(!valid_aumid("a\"b"));
        assert!(!valid_aumid("a/b"));
        assert!(!valid_aumid("a:b"));
    }
}
