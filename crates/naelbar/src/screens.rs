//! One island per display ("Tous les écrans").
//!
//! The island that lives in `App`'s own fields is the home island, on the
//! chosen display. Every other display gets a parked [`Island`] holding the
//! per-window fields. To handle a message or a frame for a parked island, its
//! fields are swapped into `App`, the usual code runs, and they are swapped
//! back. With a single island nothing is parked and nothing changes.

use super::*;
use windows::Win32::Graphics::Gdi::{MONITOR_DEFAULTTONEAREST, MonitorFromPoint, MonitorFromWindow};

macro_rules! island_fields {
    ($($f:ident : $t:ty),* $(,)?) => {
        /// The per-window part of `App`, for an island that is not swapped in.
        pub(super) struct Island {
            $($f: $t,)*
        }

        impl Island {
            fn swap_with(&mut self, app: &mut App) {
                $(std::mem::swap(&mut self.$f, &mut app.$f);)*
            }
        }
    };
}

island_fields! {
    monitor: String,
    hmonitor: HMONITOR,
    visual: HWND,
    hit: HWND,
    renderer: Renderer,
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
    animating: bool,
    last_step: Instant,
    dirty: bool,
    input: Input,
    tracking: bool,
    widgets: WidgetState,
    last_cmds: Vec<Cmd>,
    armed_deadline: Option<u64>,
    trim_armed: bool,
    trimmed_once: bool,
    render_retries: u32,
    preview: Option<Preview>,
    thin: bool,
    appbar_fullscreen: bool,
    wide: bool,
    call_extra: f32,
    mic_app: Option<String>,
    mic_app_key: Option<u64>,
}

#[derive(Default)]
pub(super) struct Screens {
    parked: Vec<Island>,
    /// A frame ran commands, which may have changed what the others show.
    pub(super) touched: bool,
    /// Hit window of the island under a file drag.
    drag_target: Option<HWND>,
}

/// Index of the island owning `hwnd`, given each island's (visual, hit).
pub(crate) fn route<I: IntoIterator<Item = (isize, isize)>>(windows: I, hwnd: isize) -> Option<usize> {
    windows.into_iter().position(|(v, h)| v == hwnd || h == hwnd)
}

/// Displays to add, and indices of current islands to remove (ascending).
pub(crate) fn diff(current: &[String], wanted: &[String]) -> (Vec<String>, Vec<usize>) {
    let mut remove = Vec::new();
    for (i, name) in current.iter().enumerate() {
        if !wanted.contains(name) || current[..i].contains(name) {
            remove.push(i);
        }
    }
    let mut add: Vec<String> = Vec::new();
    for name in wanted {
        if !current.contains(name) && !add.contains(name) {
            add.push(name.clone());
        }
    }
    (add, remove)
}

impl Island {
    unsafe fn create(app: &App, monitor: String) -> Result<Island> {
        let instance = GetModuleHandleW(None)?;
        let class = w!("naelbar.window");
        let topmost = WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_LAYERED;
        let visual = CreateWindowExW(topmost | WS_EX_TRANSPARENT | WS_EX_NOREDIRECTIONBITMAP, class, w!("naelbar visual"), WS_POPUP, 0, 0, 1, 1, None, None, Some(instance.into()), None)?;
        let hit = match CreateWindowExW(topmost, class, w!("naelbar"), WS_POPUP, 0, 0, 1, 1, None, None, Some(instance.into()), None) {
            Ok(h) => h,
            Err(e) => {
                let _ = DestroyWindow(visual);
                return Err(e);
            }
        };
        let _ = SetLayeredWindowAttributes(visual, COLORREF(0), 255, LWA_ALPHA);
        let _ = SetLayeredWindowAttributes(hit, COLORREF(0), 1, LWA_ALPHA);
        let (pill_w, pill_h, pill_r) = pill_metrics(&app.config);
        Ok(Island {
            monitor,
            hmonitor: HMONITOR::default(),
            visual,
            hit,
            renderer: Renderer::new(visual),
            tab: app.views.iter().position(|v| v.name() == app.config.last_tab).unwrap_or(0),
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
            hidden: app.hidden,
            drag_hover: false,
            animating: false,
            last_step: Instant::now(),
            dirty: true,
            input: Input::default(),
            tracking: false,
            widgets: WidgetState::default(),
            last_cmds: Vec::new(),
            armed_deadline: None,
            trim_armed: false,
            trimmed_once: false,
            render_retries: 0,
            preview: None,
            thin: false,
            appbar_fullscreen: false,
            wide: false,
            call_extra: 0.0,
            mic_app: None,
            mic_app_key: None,
        })
    }

    unsafe fn destroy(self) {
        let Island { visual, hit, renderer, preview, .. } = self;
        appbar_message(hit, ABM_REMOVE);
        dragdrop::revoke(hit);
        drop(preview);
        drop(renderer);
        let _ = DestroyWindow(hit);
        let _ = DestroyWindow(visual);
    }
}

impl App {
    /// The island in `App`'s fields is the one on the chosen display.
    pub(super) fn is_home(&self) -> bool {
        self.monitor.is_empty()
    }

    /// Device name this island sits on.
    pub(super) fn monitor_name(&self) -> &str {
        if self.is_home() { &self.config.monitor } else { &self.monitor }
    }

    /// False when the foreground window is on another display, so an app in
    /// front there does not shrink this island. Always true with one display.
    pub(super) fn foreground_here(&self) -> bool {
        unsafe {
            if self.hmonitor.is_invalid() || GetSystemMetrics(SM_CMONITORS) <= 1 {
                return true;
            }
            let fg = GetForegroundWindow();
            fg.is_invalid() || MonitorFromWindow(fg, MONITOR_DEFAULTTONEAREST) == self.hmonitor
        }
    }

    /// Another island has an open tab that shows live stats.
    pub(super) fn others_want_stats(&self) -> bool {
        self.screens.parked.iter().any(|p| p.open && self.views.get(p.tab).is_some_and(|v| v.wants_stats()))
    }

    /// Runs `f` with parked island `i` swapped in.
    fn on_island<R>(&mut self, i: usize, f: impl FnOnce(&mut App) -> R) -> R {
        let mut parked = std::mem::take(&mut self.screens.parked);
        parked[i].swap_with(self);
        self.screens.parked = parked;
        let r = f(self);
        let mut parked = std::mem::take(&mut self.screens.parked);
        parked[i].swap_with(self);
        self.screens.parked = parked;
        r
    }

    fn parked_index(&self, hwnd: HWND) -> Option<usize> {
        route(self.screens.parked.iter().map(|p| (p.visual.0 as isize, p.hit.0 as isize)), hwnd.0 as isize)
    }

    /// Runs `f` on the island owning `hwnd`, or on the home island.
    pub(super) fn with_island_of<R>(&mut self, hwnd: HWND, f: impl FnOnce(&mut App) -> R) -> R {
        match self.parked_index(hwnd) {
            Some(i) => self.on_island(i, f),
            None => f(self),
        }
    }

    /// Runs `f` on every island, home first. Commands run by one island's
    /// frame mark the others dirty, since they share the state.
    pub(super) fn each_island(&mut self, mut f: impl FnMut(&mut App)) {
        self.screens.touched = false;
        f(self);
        if std::mem::take(&mut self.screens.touched) {
            self.mark_dirty_except(None);
        }
        for i in 0..self.screens.parked.len() {
            self.on_island(i, &mut f);
            if std::mem::take(&mut self.screens.touched) {
                self.mark_dirty_except(Some(i));
            }
        }
    }

    fn mark_dirty_except(&mut self, skip: Option<usize>) {
        if skip.is_some() {
            self.dirty = true;
        }
        for (j, p) in self.screens.parked.iter_mut().enumerate() {
            if Some(j) != skip {
                p.dirty = true;
            }
        }
    }

    pub(super) fn any_animating(&self) -> bool {
        self.animating || self.screens.parked.iter().any(|p| p.animating)
    }

    /// Frame-latency handle of an animating island.
    pub(super) fn animating_waitable(&self) -> Option<HANDLE> {
        if self.animating {
            return self.renderer.waitable();
        }
        self.screens.parked.iter().find(|p| p.animating).and_then(|p| p.renderer.waitable())
    }

    /// Frame-latency handle of the main island when it has a still frame to draw.
    pub(super) fn dirty_waitable(&self) -> Option<HANDLE> {
        // Only worth waiting for while the previous frame is still on its way.
        if self.dirty && !self.animating && self.in_flight { self.renderer.waitable() } else { None }
    }

    fn island_under_cursor(&self) -> Option<usize> {
        let mut p = POINT::default();
        unsafe {
            GetCursorPos(&mut p).ok()?;
            let m = MonitorFromPoint(p, MONITOR_DEFAULTTONEAREST);
            self.screens.parked.iter().position(|i| i.hmonitor == m)
        }
    }

    /// Window messages, routed to the island they belong to.
    pub(super) fn route_message(&mut self, hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> Option<isize> {
        if let Some(i) = self.parked_index(hwnd) {
            return self.on_island(i, |a| a.on_message(hwnd, msg, wp, lp));
        }
        if msg == WM_HOTKEY && wp.0 as i32 == hotkeys::HK_TOGGLE {
            if let Some(i) = self.island_under_cursor() {
                return self.on_island(i, |a| a.on_message(hwnd, msg, wp, lp));
            }
        }
        let r = self.on_message(hwnd, msg, wp, lp);
        if hwnd == self.ctrl || msg == WM_QUEUE {
            match msg {
                WM_CONFIG_CHANGED => {
                    self.sync_screens();
                    for i in 0..self.screens.parked.len() {
                        self.on_island(i, |a| a.relayout());
                    }
                }
                WM_DISPLAYCHANGE => {
                    self.sync_screens();
                    for i in 0..self.screens.parked.len() {
                        self.on_island(i, |a| {
                            a.renderer.reset();
                            a.reposition();
                        });
                    }
                }
                WM_HOTKEY if wp.0 as i32 == hotkeys::HK_HIDE => {
                    let hidden = self.hidden;
                    for i in 0..self.screens.parked.len() {
                        self.on_island(i, |a| {
                            if a.hidden != hidden {
                                a.set_hidden(hidden);
                            }
                        });
                    }
                }
                _ => {}
            }
            // Shared state changed: every island may show it.
            if self.dirty {
                self.mark_dirty_except(None);
            }
        }
        r
    }

    /// Adds or removes islands to match the setting and the displays.
    pub(super) fn sync_screens(&mut self) {
        let all = self.config.islands == "all";
        if !all && self.screens.parked.is_empty() {
            return;
        }
        let wanted: Vec<String> = if all {
            let home = crate::monitors::pick(&self.config.monitor).map(|m| m.name);
            crate::monitors::list().into_iter().map(|m| m.name).filter(|n| Some(n) != home.as_ref()).collect()
        } else {
            Vec::new()
        };
        let current: Vec<String> = self.screens.parked.iter().map(|p| p.monitor.clone()).collect();
        let (add, remove) = diff(&current, &wanted);
        for i in remove.into_iter().rev() {
            let island = self.screens.parked.remove(i);
            if self.screens.drag_target == Some(island.hit) {
                self.screens.drag_target = None;
            }
            unsafe { island.destroy() };
        }
        for name in add {
            let island = match unsafe { Island::create(self, name) } {
                Ok(i) => i,
                Err(e) => {
                    eprintln!("naelbar: could not create an island: {e}");
                    continue;
                }
            };
            self.screens.parked.push(island);
            let i = self.screens.parked.len() - 1;
            self.on_island(i, |a| unsafe {
                a.reposition();
                if !a.hidden {
                    let _ = ShowWindow(a.visual, SW_SHOWNOACTIVATE);
                    let _ = ShowWindow(a.hit, SW_SHOWNOACTIVATE);
                }
                dragdrop::register(a.hit);
                appbar_message(a.hit, ABM_NEW);
            });
        }
    }

    /// Closes every parked island, on quit.
    pub(super) fn drop_screens(&mut self) {
        for island in std::mem::take(&mut self.screens.parked) {
            unsafe { island.destroy() };
        }
    }

    fn drag_island(&mut self, f: impl FnOnce(&mut App)) {
        let target = self.screens.drag_target.unwrap_or(self.hit);
        self.with_island_of(target, f);
    }

    pub fn on_drag_enter(&mut self) {
        let mut p = POINT::default();
        let under = unsafe { if GetCursorPos(&mut p).is_ok() { WindowFromPoint(p) } else { HWND::default() } };
        let target = self.parked_index(under).map(|i| self.screens.parked[i].hit);
        self.screens.drag_target = target;
        self.drag_island(|a| a.drag_enter_here());
    }

    pub fn on_drag_leave(&mut self) {
        self.drag_island(|a| a.drag_leave_here());
    }

    pub fn on_drop(&mut self, files: Vec<PathBuf>) {
        if self.drag_modal {
            // Our own item dropped back on the island: nothing to add.
            self.screens.drag_target = None;
            return;
        }
        self.drag_island(|a| a.drop_here(files));
        self.screens.drag_target = None;
        // The shelf is shared: every island shows the new files.
        self.dirty = true;
        self.mark_dirty_except(None);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn routes_by_either_window() {
        let w = [(10, 11), (20, 21), (30, 31)];
        assert_eq!(route(w, 10), Some(0));
        assert_eq!(route(w, 21), Some(1));
        assert_eq!(route(w, 31), Some(2));
        assert_eq!(route(w, 99), None);
        assert_eq!(route([], 10), None);
    }

    #[test]
    fn diff_adds_and_removes() {
        let (add, remove) = diff(&names(&["A", "B"]), &names(&["B", "C"]));
        assert_eq!(add, names(&["C"]));
        assert_eq!(remove, vec![0]);
    }

    #[test]
    fn diff_stable_when_unchanged() {
        let (add, remove) = diff(&names(&["A", "B"]), &names(&["B", "A"]));
        assert!(add.is_empty() && remove.is_empty());
    }

    #[test]
    fn diff_unplug_all_and_plug_new() {
        let (add, remove) = diff(&names(&["A", "B", "C"]), &[]);
        assert!(add.is_empty());
        assert_eq!(remove, vec![0, 1, 2]);
        let (add, remove) = diff(&[], &names(&["X", "X", "Y"]));
        assert_eq!(add, names(&["X", "Y"]));
        assert!(remove.is_empty());
    }

    #[test]
    fn diff_drops_duplicates() {
        let (add, remove) = diff(&names(&["A", "A"]), &names(&["A"]));
        assert!(add.is_empty());
        assert_eq!(remove, vec![1]);
    }
}
