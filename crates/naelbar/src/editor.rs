//! Small text entry window. The island never takes focus, so typing happens
//! in this short-lived process (`--edit=event:YYYY-MM-DD` or `--edit=note`). It
//! adds its item to the agenda file and tells the island to reload it.

use std::cell::RefCell;

use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Dwm::{DWMWA_USE_IMMERSIVE_DARK_MODE, DwmSetWindowAttribute};
use windows::Win32::Graphics::Gdi::ValidateRect;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::UI::HiDpi::{AdjustWindowRectExForDpi, GetDpiForWindow};
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{BOOL, PCWSTR, Result, w};

use crate::agenda::{self, Agenda, WM_AGENDA_CHANGED};
use crate::config::{self, Config};
use crate::render::Renderer;
use crate::state::unix_ms;
use crate::ui::theme::*;
use crate::ui::{Align, Cmd, Icon, Id, Input, Pt, Rect, TextStyle, Ui, WidgetState, id, is_english, tr};

const W: f32 = 380.0;
const H: f32 = 170.0;
/// The ignore list is taller: its entries are listed under the field.
const IGNORE_H: f32 = 270.0;
const IGNORE_ROW_H: f32 = 24.0;
const IGNORE_ROWS: usize = 6;

fn height(target: &Target) -> f32 {
    if matches!(target, Target::Ignore) { IGNORE_H } else { H }
}
const FIELD_H: f32 = 34.0;
const NOTE_H: f32 = 76.0;
/// What the fields can show at once. The end of the text stays in view.
const TITLE_CHARS: usize = 46;
const NOTE_CHARS: usize = 120;

const WM_MOUSELEAVE: u32 = 0x02A3;
const WM_CAPTURECHANGED: u32 = 0x0215;

#[derive(Debug, Clone, PartialEq, Eq)]
enum Target {
    Event { date: String },
    Note,
    /// Launcher slot, 1 to 6.
    Slot(usize),
    /// The list of applications the island ignores.
    Ignore,
    /// A new countdown: name, date, time.
    Countdown,
    /// The calculator: an expression or a conversion.
    Calc,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Field {
    Main,
    Time,
    Date,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Problem {
    Empty,
    Time,
}

/// `event:YYYY-MM-DD` or `note`.
fn parse_target(spec: &str) -> Option<Target> {
    if spec == "note" {
        return Some(Target::Note);
    }
    if spec == "ignore" {
        return Some(Target::Ignore);
    }
    match spec {
        "countdown" => return Some(Target::Countdown),
        "calc" => return Some(Target::Calc),
        _ => {}
    }
    if let Some(n) = spec.strip_prefix("slot:") {
        let n: usize = n.parse().ok()?;
        return (1..=crate::launcher::SLOT_COUNT).contains(&n).then_some(Target::Slot(n));
    }
    let date = spec.strip_prefix("event:")?;
    agenda::parse_date(date)?;
    Some(Target::Event { date: date.to_string() })
}

/// `event:YYYY-MM-DD@HH:MM` carries a prefilled time. Returns the spec without
/// it, and the time normalised (empty when absent or invalid).
fn split_prefill(spec: &str) -> (&str, String) {
    match spec.split_once('@') {
        Some((head, time)) => (head, normalize_time(time).ok().flatten().unwrap_or_default()),
        None => (spec, String::new()),
    }
}

/// Empty means no time. Anything else must be a real time, stored as `HH:MM`.
fn normalize_time(s: &str) -> std::result::Result<Option<String>, Problem> {
    let s = s.trim();
    if s.is_empty() {
        return Ok(None);
    }
    let (h, m) = s.split_once(':').ok_or(Problem::Time)?;
    if !(1..=2).contains(&h.len()) || m.len() != 2 || !h.bytes().chain(m.bytes()).all(|c| c.is_ascii_digit()) {
        return Err(Problem::Time);
    }
    let (h, m): (u32, u32) = (h.parse().map_err(|_| Problem::Time)?, m.parse().map_err(|_| Problem::Time)?);
    if h > 23 || m > 59 {
        return Err(Problem::Time);
    }
    Ok(Some(format!("{h:02}:{m:02}")))
}

/// The record to add, as a one-item agenda ready to merge.
fn build_item(target: &Target, text: &str, time: &str, now_ms: u64) -> std::result::Result<Agenda, Problem> {
    let text = text.trim();
    if text.is_empty() || matches!(target, Target::Slot(_) | Target::Ignore | Target::Countdown | Target::Calc) {
        return Err(Problem::Empty);
    }
    let mut item = Agenda::default();
    match target {
        Target::Event { date } => item.events.push(agenda::Event {
            id: agenda::new_id(),
            title: text.to_string(),
            date: date.clone(),
            time: normalize_time(time)?,
            end: None,
            updated_at_ms: now_ms,
            deleted: false,
        }),
        Target::Slot(_) | Target::Ignore | Target::Countdown | Target::Calc => {}
        Target::Note => item.notes.push(agenda::Note { id: agenda::new_id(), text: text.to_string(), done: false, updated_at_ms: now_ms, deleted: false }),
    }
    Ok(item)
}

/// The last `max` characters, so what is being typed stays visible.
fn tail(text: &str, max: usize) -> &str {
    match text.char_indices().rev().nth(max.saturating_sub(1)) {
        Some((i, _)) => &text[i..],
        None => text,
    }
}

struct Editor {
    hwnd: HWND,
    renderer: Renderer,
    target: Target,
    input: Input,
    widgets: WidgetState,
    last: Vec<Cmd>,
    scale: f32,
    main: String,
    time: String,
    /// Date field of a countdown.
    date: String,
    focus: Field,
    error: Option<&'static str>,
    /// First half of a character outside the 16-bit range.
    high_surrogate: Option<u16>,
    dirty: bool,
    tracking: bool,
    /// The ignore list being edited.
    ignored: Vec<String>,
}

thread_local! {
    static STATE: RefCell<Option<Editor>> = const { RefCell::new(None) };
}

fn with<R>(f: impl FnOnce(&mut Editor) -> R) -> Option<R> {
    STATE.with(|c| c.try_borrow_mut().ok().and_then(|mut g| g.as_mut().map(f)))
}

struct FieldStyle {
    size: f32,
    wrap: bool,
    max_chars: usize,
}

fn text_field(ui: &mut Ui, w: Id, r: Rect, text: &str, hint: &str, focused: bool, look: FieldStyle) -> bool {
    let resp = ui.interact(w, r);
    ui.fill(r, R_SMALL, if resp.hovered { TILE_HOVER } else { TILE });
    if focused {
        ui.stroke(r.inset(0.5), R_SMALL, 1.0, TEXT_FAINT);
    }
    let inner = if look.wrap { r.inset_xy(12.0, 9.0) } else { r.inset_xy(12.0, 0.0) };
    let style = |color| {
        let s = TextStyle::new(look.size, 400, color);
        if look.wrap { s.top().wrapped() } else { s }
    };
    if text.is_empty() {
        ui.text(inner, hint, style(TEXT_FAINT));
    } else {
        let shown = tail(text, look.max_chars);
        let shown = if focused { format!("{shown}|") } else { shown.to_string() };
        ui.text(inner, &shown, style(TEXT));
    }
    resp.clicked
}

impl Editor {
    fn close(&self) {
        // Posted: destroying from here would re-enter this borrowed state and
        // the quit message would never be sent.
        unsafe {
            let _ = PostMessageW(Some(self.hwnd), WM_CLOSE, WPARAM(0), LPARAM(0));
        }
    }

    /// Stores the text as the target of a launcher slot.
    fn submit_slot(&mut self, n: usize) {
        let text = self.main.trim().to_string();
        let mut cfg = Config::load();
        let slot = &mut cfg.slots[n - 1];
        if let Err(e) = crate::launcher::validate(slot.kind(), &text) {
            self.error = Some(e.message());
            return;
        }
        slot.target = text;
        cfg.save();
        unsafe {
            if let Ok(island) = FindWindowW(w!("naelbar.window"), w!("naelbar control")) {
                let _ = PostMessageW(Some(island), config::WM_CONFIG_CHANGED, WPARAM(0), LPARAM(0));
            }
        }
        self.close();
    }

    /// Saves the ignore list and tells the island, which applies it at once.
    fn save_ignored(&mut self) {
        let mut cfg = Config::load();
        cfg.ignore_apps = self.ignored.clone();
        cfg.save();
        unsafe {
            if let Ok(island) = FindWindowW(w!("naelbar.window"), w!("naelbar control")) {
                let _ = PostMessageW(Some(island), config::WM_CONFIG_CHANGED, WPARAM(0), LPARAM(0));
            }
        }
    }

    fn submit_ignore(&mut self) {
        // Re-read: the settings window may have changed the list meanwhile.
        self.ignored = Config::load().ignore_apps;
        if crate::activity::add_ignored(&mut self.ignored, &self.main) {
            self.save_ignored();
            self.main.clear();
            self.error = None;
        } else if self.main.trim().is_empty() {
            self.close();
        } else {
            self.error = Some(tr("Déjà dans la liste.", "Already in the list."));
        }
    }

    fn submit_countdown(&mut self) {
        use crate::widgets::countdown::{self, Problem};
        let now = crate::widgets::page2::naive_now();
        match countdown::with_added(countdown::load(), &self.main, &self.date, &self.time, now) {
            Ok(list) => {
                if countdown::save(&list).is_err() {
                    self.error = Some(tr("Enregistrement impossible.", "Could not save."));
                    return;
                }
                crate::widgets::notify_island(crate::widgets::WM_WIDGETS_CHANGED);
                self.close();
            }
            Err(problem) => {
                self.focus = match problem {
                    Problem::Name | Problem::Full => Field::Main,
                    Problem::Date | Problem::Past => Field::Date,
                    Problem::Time => Field::Time,
                };
                self.error = Some(match problem {
                    Problem::Name => tr("Donne un nom.", "Give it a name."),
                    Problem::Date => tr("Date invalide, ex. 25/12/2026.", "Invalid date, e.g. 2026-12-25."),
                    Problem::Time => tr("Heure invalide, format HH:MM.", "Invalid time, use HH:MM."),
                    Problem::Past => tr("Cette date est déjà passée.", "That date has already passed."),
                    Problem::Full => tr("Trois comptes à rebours au maximum.", "Three countdowns at most."),
                });
            }
        }
    }

    /// Copies the result, keeps it for the tile and closes.
    fn submit_calc(&mut self) {
        let Ok(result) = crate::widgets::calc::compute(&self.main, is_english()) else {
            self.error = Some(tr("Rien à copier.", "Nothing to copy."));
            return;
        };
        if !crate::widgets::clip::set_text(self.hwnd, &result) {
            self.error = Some(tr("Copie impossible.", "Could not copy."));
            return;
        }
        let last = crate::widgets::store::LastCalc { expr: self.main.trim().to_string(), result };
        if crate::widgets::store::save_calc(&last).is_ok() {
            crate::widgets::notify_island(crate::widgets::WM_WIDGETS_CHANGED);
        }
        self.close();
    }

    fn submit(&mut self) {
        if let Target::Slot(n) = self.target {
            return self.submit_slot(n);
        }
        match self.target {
            Target::Countdown => return self.submit_countdown(),
            Target::Calc => return self.submit_calc(),
            _ => {}
        }
        if self.target == Target::Ignore {
            return self.submit_ignore();
        }
        let item = match build_item(&self.target, &self.main, &self.time, unix_ms()) {
            Ok(item) => item,
            Err(Problem::Empty) => {
                self.focus = Field::Main;
                self.error = Some(tr("Écris d'abord quelque chose.", "Type something first."));
                return;
            }
            Err(Problem::Time) => {
                self.focus = Field::Time;
                self.error = Some(tr("Heure invalide, format HH:MM.", "Invalid time, use HH:MM."));
                return;
            }
        };
        let mut stored = Agenda::load();
        stored.merge(&item);
        if stored.save().is_err() {
            self.error = Some(tr("Enregistrement impossible.", "Could not save."));
            return;
        }
        unsafe {
            if let Ok(island) = FindWindowW(w!("naelbar.window"), w!("naelbar control")) {
                let _ = PostMessageW(Some(island), WM_AGENDA_CHANGED, WPARAM(0), LPARAM(0));
            }
        }
        self.close();
    }

    fn on_char(&mut self, unit: u32) {
        self.dirty = true;
        match unit {
            13 => return self.submit(),
            27 => return self.close(),
            9 => {
                if matches!(self.target, Target::Event { .. }) {
                    self.focus = if self.focus == Field::Main { Field::Time } else { Field::Main };
                } else if self.target == Target::Countdown {
                    self.focus = match self.focus {
                        Field::Main => Field::Date,
                        Field::Date => Field::Time,
                        Field::Time => Field::Main,
                    };
                }
                return;
            }
            _ => {}
        }
        self.error = None;
        let target = match self.focus {
            Field::Main => &mut self.main,
            Field::Time => &mut self.time,
            Field::Date => &mut self.date,
        };
        match unit {
            8 => {
                target.pop();
            }
            // Ctrl+V.
            22 => {
                if let Some(text) = crate::widgets::clip::get_text(self.hwnd, 200) {
                    target.extend(text.chars().filter(|c| !c.is_control()));
                }
            }
            0xD800..=0xDBFF => self.high_surrogate = Some(unit as u16),
            0xDC00..=0xDFFF => {
                if let Some(high) = self.high_surrogate.take() {
                    target.extend(char::decode_utf16([high, unit as u16]).filter_map(|c| c.ok()));
                }
            }
            127 => {}
            c if c >= 32 => target.extend(char::from_u32(c)),
            _ => {}
        }
    }

    /// Returns true if a frame went to the screen.
    fn frame(&mut self) -> bool {
        self.dirty = false;
        let mut out = Vec::new();
        let mut ui = Ui::new(&self.input, &mut self.widgets, &mut out, self.scale, unix_ms());
        let h = height(&self.target);
        let mut removed: Option<usize> = None;
        ui.fill(Rect::new(0.0, 0.0, W, h), 0.0, SURFACE);

        let mut rest = Rect::new(0.0, 0.0, W, h).inset_xy(16.0, 14.0);
        let head = rest.take_top(20.0, 10.0);
        match &self.target {
            Target::Event { date } => {
                ui.text(head, tr("Nouvel événement", "New event"), TextStyle::new(14.0, 600, TEXT));
                if let Some((y, m, d)) = agenda::parse_date(date) {
                    ui.text(head, &agenda::day_label(y, m, d, is_english()), TextStyle::new(12.5, 400, TEXT_DIM).aligned(Align::End));
                }
                let title = rest.take_top(FIELD_H, 8.0);
                let look = FieldStyle { size: 13.0, wrap: false, max_chars: TITLE_CHARS };
                if text_field(&mut ui, id("edit.main", 0), title, &self.main, tr("Titre", "Title"), self.focus == Field::Main, look) {
                    self.focus = Field::Main;
                }
                let mut row = rest.take_top(FIELD_H, 8.0);
                let time = row.take_left(100.0, 10.0);
                let look = FieldStyle { size: 13.0, wrap: false, max_chars: 8 };
                if text_field(&mut ui, id("edit.time", 0), time, &self.time, "HH:MM", self.focus == Field::Time, look) {
                    self.focus = Field::Time;
                }
                ui.text(row, tr("Heure, facultative", "Time, optional"), TextStyle::new(12.0, 400, TEXT_FAINT));
            }
            Target::Slot(n) => {
                let kind = Config::load().slots[n - 1].kind();
                ui.text(head, &format!("{} {n}", tr("Lanceur", "Launcher")), TextStyle::new(14.0, 600, TEXT));
                ui.text(head, kind.label(), TextStyle::new(12.5, 400, TEXT_DIM).aligned(Align::End));
                let title = rest.take_top(FIELD_H, 8.0);
                let look = FieldStyle { size: 13.0, wrap: false, max_chars: TITLE_CHARS };
                text_field(&mut ui, id("edit.main", 0), title, &self.main, kind.hint(), true, look);
            }
            Target::Ignore => {
                ui.text(head, tr("Applications ignorées", "Ignored apps"), TextStyle::new(14.0, 600, TEXT));
                let field = rest.take_top(FIELD_H, 8.0);
                let look = FieldStyle { size: 13.0, wrap: false, max_chars: TITLE_CHARS };
                text_field(&mut ui, id("edit.main", 0), field, &self.main, tr("Zoom.exe ou identifiant AUMID", "Zoom.exe or AUMID"), true, look);
                for (i, entry) in self.ignored.iter().enumerate().take(IGNORE_ROWS) {
                    let mut row = rest.take_top(IGNORE_ROW_H, 0.0);
                    let x = row.take_right(IGNORE_ROW_H, 0.0);
                    if ui.icon_button(id("edit.ignore", i), x, Icon::Close, 14.0, TEXT_DIM).clicked {
                        removed = Some(i);
                    }
                    ui.text(row.inset_xy(4.0, 0.0), &crate::activity::shorten(entry, 48), TextStyle::new(12.5, 400, TEXT));
                }
                if self.ignored.len() > IGNORE_ROWS {
                    ui.text(rest.take_top(IGNORE_ROW_H, 0.0).inset_xy(4.0, 0.0), &format!("+{}", self.ignored.len() - IGNORE_ROWS), TextStyle::new(12.0, 400, TEXT_FAINT));
                }
            }
            Target::Countdown => {
                ui.text(head, tr("Compte à rebours", "Countdown"), TextStyle::new(14.0, 600, TEXT));
                let title = rest.take_top(FIELD_H, 8.0);
                let look = FieldStyle { size: 13.0, wrap: false, max_chars: TITLE_CHARS };
                if text_field(&mut ui, id("edit.main", 0), title, &self.main, tr("Nom, par exemple Partiels", "Name, for example Exams"), self.focus == Field::Main, look) {
                    self.focus = Field::Main;
                }
                let mut row = rest.take_top(FIELD_H, 8.0);
                let date = row.take_left(124.0, 8.0);
                let look = FieldStyle { size: 13.0, wrap: false, max_chars: 12 };
                if text_field(&mut ui, id("edit.date", 0), date, &self.date, tr("JJ/MM/AAAA", "DD/MM/YYYY"), self.focus == Field::Date, look) {
                    self.focus = Field::Date;
                }
                let time = row.take_left(78.0, 8.0);
                let look = FieldStyle { size: 13.0, wrap: false, max_chars: 8 };
                if text_field(&mut ui, id("edit.time", 0), time, &self.time, "HH:MM", self.focus == Field::Time, look) {
                    self.focus = Field::Time;
                }
                ui.text(row, tr("Tab change de champ", "Tab switches field"), TextStyle::new(10.5, 400, TEXT_FAINT));
            }
            Target::Calc => {
                ui.text(head, tr("Calculatrice", "Calculator"), TextStyle::new(14.0, 600, TEXT));
                let field = rest.take_top(FIELD_H, 8.0);
                let look = FieldStyle { size: 13.0, wrap: false, max_chars: TITLE_CHARS };
                text_field(&mut ui, id("edit.main", 0), field, &self.main, tr("12 * (3 + 4) ou 12 km en mi", "12 * (3 + 4) or 12 km to mi"), true, look);
                let line = rest.take_top(24.0, 4.0);
                match crate::widgets::calc::compute(&self.main, is_english()) {
                    Ok(result) => ui.text(line, &format!("= {result}"), TextStyle::new(16.0, 600, TEXT)),
                    Err(crate::widgets::calc::CalcError::Empty) => {}
                    Err(e) => ui.text(line, &e.message(), TextStyle::new(12.0, 400, TEXT_DIM)),
                }
            }
            Target::Note => {
                ui.text(head, "Post-it", TextStyle::new(14.0, 600, TEXT));
                let field = rest.take_top(NOTE_H, 8.0);
                let look = FieldStyle { size: 13.0, wrap: true, max_chars: NOTE_CHARS };
                text_field(&mut ui, id("edit.main", 0), field, &self.main, tr("Écris ton post-it", "Write your note"), true, look);
            }
        }

        let footer = rest.take_top(20.0, 0.0);
        match self.error {
            Some(message) => ui.text(footer, message, TextStyle::new(12.0, 500, TEXT)),
            None if self.target == Target::Calc => ui.text(footer, tr("Entrée copie le résultat, Échap annule", "Enter copies the result, Escape cancels"), TextStyle::new(12.0, 400, TEXT_FAINT)),
            None => ui.text(footer, tr("Entrée pour enregistrer, Échap pour annuler", "Enter to save, Escape to cancel"), TextStyle::new(12.0, 400, TEXT_FAINT)),
        }

        let cmds = std::mem::take(&mut ui.cmds);
        drop(ui);
        if let Some(i) = removed {
            if crate::activity::remove_ignored(&mut self.ignored, i) {
                self.save_ignored();
                self.dirty = true;
            }
        }
        // Clicking a field moves the focus, which only shows on the next pass.
        if self.input.released {
            self.dirty = true;
        }
        self.input.pressed = false;
        self.input.released = false;
        self.input.wheel = 0.0;
        if !self.input.down {
            self.widgets.active = None;
        }

        if cmds == self.last {
            return false;
        }
        if self.renderer.render(&cmds).is_ok() {
            self.last = cmds;
            true
        } else {
            self.last.clear();
            false
        }
    }

    fn point(&self, lp: LPARAM) -> Pt {
        Pt { x: (lp.0 & 0xFFFF) as i16 as f32 / self.scale, y: ((lp.0 >> 16) & 0xFFFF) as i16 as f32 / self.scale }
    }

    fn on_message(&mut self, msg: u32, wp: WPARAM, lp: LPARAM) -> Option<isize> {
        match msg {
            WM_MOUSEMOVE => {
                self.input.mouse = Some(self.point(lp));
                if !self.tracking {
                    let mut t = TRACKMOUSEEVENT { cbSize: size_of::<TRACKMOUSEEVENT>() as u32, dwFlags: TME_LEAVE, hwndTrack: self.hwnd, dwHoverTime: 0 };
                    unsafe {
                        let _ = TrackMouseEvent(&mut t);
                    }
                    self.tracking = true;
                }
                self.dirty = true;
                Some(0)
            }
            WM_MOUSELEAVE => {
                self.tracking = false;
                if !self.input.down {
                    self.input.mouse = None;
                }
                self.dirty = true;
                Some(0)
            }
            WM_LBUTTONDOWN => {
                self.input.mouse = Some(self.point(lp));
                self.input.down = true;
                self.input.pressed = true;
                self.input.press_pos = self.input.mouse.unwrap_or_default();
                unsafe { SetCapture(self.hwnd) };
                self.dirty = true;
                Some(0)
            }
            WM_LBUTTONUP => {
                self.input.mouse = Some(self.point(lp));
                self.input.down = false;
                self.input.released = true;
                unsafe {
                    let _ = ReleaseCapture();
                }
                self.dirty = true;
                Some(0)
            }
            WM_CAPTURECHANGED if self.input.down => {
                self.input.down = false;
                self.dirty = true;
                Some(0)
            }
            WM_CHAR => {
                self.on_char(wp.0 as u32);
                Some(0)
            }
            WM_DPICHANGED => {
                self.scale = (wp.0 & 0xFFFF) as f32 / 96.0;
                let r = unsafe { *(lp.0 as *const RECT) };
                unsafe {
                    let _ = SetWindowPos(self.hwnd, None, r.left, r.top, r.right - r.left, r.bottom - r.top, SWP_NOZORDER | SWP_NOACTIVATE);
                }
                self.renderer.set_size((W * self.scale).round() as u32, (height(&self.target) * self.scale).round() as u32, self.scale);
                self.last.clear();
                self.dirty = true;
                Some(0)
            }
            WM_ERASEBKGND => Some(1),
            WM_PAINT => {
                unsafe {
                    let _ = ValidateRect(Some(self.hwnd), None);
                }
                self.last.clear();
                self.dirty = true;
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
    match with(|e| e.on_message(msg, wp, lp)).flatten() {
        Some(v) => LRESULT(v),
        None => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

/// Windows refuses focus to a process started from the no-activate island, so
/// the window is first lifted above the others, then asked for focus.
unsafe fn bring_forward(hwnd: HWND) {
    let flags = SWP_NOMOVE | SWP_NOSIZE;
    let _ = SetWindowPos(hwnd, Some(HWND_TOPMOST), 0, 0, 0, 0, flags);
    let _ = SetWindowPos(hwnd, Some(HWND_NOTOPMOST), 0, 0, 0, 0, flags);
    let _ = SetForegroundWindow(hwnd);
}

/// Opens the editor described by `spec`, or brings the existing one forward.
pub fn run(spec: &str) -> Result<()> {
    let (spec, prefill) = split_prefill(spec);
    let Some(target) = parse_target(spec) else {
        return Err(windows::core::Error::new(E_INVALIDARG, "expected event:YYYY-MM-DD[@HH:MM] or note"));
    };
    unsafe {
        let class = w!("naelbar.editor");
        let _mutex = CreateMutexW(None, true, w!("Local\\naelbar.editor"));
        if GetLastError() == ERROR_ALREADY_EXISTS {
            if let Ok(existing) = FindWindowW(class, PCWSTR::null()) {
                let _ = ShowWindow(existing, SW_RESTORE);
                bring_forward(existing);
            }
            return Ok(());
        }

        let instance = GetModuleHandleW(None)?;
        let wc = WNDCLASSEXW {
            cbSize: size_of::<WNDCLASSEXW>() as u32,
            hIcon: crate::app_icon(),
            hIconSm: crate::app_icon(),
            lpfnWndProc: Some(wndproc),
            hInstance: instance.into(),
            lpszClassName: class,
            hCursor: LoadCursorW(None, IDC_ARROW)?,
            ..Default::default()
        };
        if RegisterClassExW(&wc) == 0 {
            return Err(windows::core::Error::from_thread());
        }

        config::apply_appearance(&Config::load());
        let style = WS_CAPTION | WS_SYSMENU;
        let ex = WS_EX_NOREDIRECTIONBITMAP | WS_EX_APPWINDOW;
        let hwnd = CreateWindowExW(ex, class, w!("naelbar agenda"), style, CW_USEDEFAULT, CW_USEDEFAULT, 100, 100, None, None, Some(instance.into()), None)?;

        let dark: BOOL = true.into();
        let _ = DwmSetWindowAttribute(hwnd, DWMWA_USE_IMMERSIVE_DARK_MODE, &dark as *const BOOL as *const _, size_of::<BOOL>() as u32);

        let dpi = GetDpiForWindow(hwnd).max(96);
        let scale = dpi as f32 / 96.0;
        let (cw, ch) = ((W * scale).round() as i32, (height(&target) * scale).round() as i32);
        let mut frame = RECT { left: 0, top: 0, right: cw, bottom: ch };
        let _ = AdjustWindowRectExForDpi(&mut frame, style, false, ex, dpi);
        let (fw, fh) = (frame.right - frame.left, frame.bottom - frame.top);
        let (sw, sh) = (GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN));
        let _ = SetWindowPos(hwnd, None, (sw - fw) / 2, (sh - fh) / 2, fw, fh, SWP_NOZORDER | SWP_NOACTIVATE);

        let mut renderer = Renderer::new(hwnd);
        renderer.set_size(cw as u32, ch as u32, scale);
        let state = Editor {
            hwnd,
            renderer,
            target: target.clone(),
            input: Input::default(),
            widgets: WidgetState::default(),
            last: Vec::new(),
            scale,
            main: match &target {
                Target::Slot(n) => Config::load().slots[n - 1].target.clone(),
                _ => String::new(),
            },
            time: prefill,
            date: String::new(),
            focus: Field::Main,
            error: None,
            high_surrogate: None,
            dirty: true,
            tracking: false,
            ignored: Config::load().ignore_apps,
        };
        STATE.with(|c| *c.borrow_mut() = Some(state));
        with(|e| e.frame());
        let _ = ShowWindow(hwnd, SW_SHOW);
        bring_forward(hwnd);

        // Same pacing as the settings window: a frame is only drawn once the
        // swapchain can take it, and nothing runs while nothing happens.
        let mut msg = MSG::default();
        let mut slot = false;
        'run: loop {
            let dirty = with(|e| e.dirty).unwrap_or(false);
            if dirty && !slot {
                slot = match with(|e| e.renderer.waitable()).flatten() {
                    Some(h) => {
                        let r = MsgWaitForMultipleObjectsEx(Some(&[h]), 32, QS_ALLINPUT, MWMO_INPUTAVAILABLE);
                        r.0 != WAIT_OBJECT_0.0 + 1
                    }
                    None => true,
                };
            } else if !dirty {
                if GetMessageW(&mut msg, None, 0, 0).0 <= 0 {
                    break;
                }
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                if msg.message == WM_QUIT {
                    break 'run;
                }
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
            if slot && with(|e| e.dirty).unwrap_or(false) && with(|e| e.frame()).unwrap_or(false) {
                slot = false;
            }
        }
        STATE.with(|c| *c.borrow_mut() = None);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_times_are_stored_as_hh_mm() {
        assert_eq!(normalize_time("00:00"), Ok(Some("00:00".into())));
        assert_eq!(normalize_time("23:59"), Ok(Some("23:59".into())));
        assert_eq!(normalize_time("9:05"), Ok(Some("09:05".into())));
        assert_eq!(normalize_time("  14:30 "), Ok(Some("14:30".into())));
    }

    #[test]
    fn an_empty_time_means_no_time() {
        assert_eq!(normalize_time(""), Ok(None));
        assert_eq!(normalize_time("   "), Ok(None));
    }

    #[test]
    fn invalid_times_are_refused() {
        for bad in ["24:00", "12:60", "12", "12:5", "12:345", "123:00", ":30", "12:", "ab:cd", "-1:30", "+1:30", "12:3é", "1:2:3", "12h30"] {
            assert_eq!(normalize_time(bad), Err(Problem::Time), "{bad}");
        }
    }

    #[test]
    fn a_prefilled_time_follows_the_at_sign() {
        assert_eq!(split_prefill("event:2026-11-10@09:30"), ("event:2026-11-10", "09:30".to_string()));
        assert_eq!(split_prefill("event:2026-11-10@9:05").1, "09:05");
        assert_eq!(split_prefill("event:2026-11-10@nope").1, "");
        assert_eq!(split_prefill("note"), ("note", String::new()));
    }

    #[test]
    fn targets_come_from_the_flag() {
        assert_eq!(parse_target("note"), Some(Target::Note));
        assert_eq!(parse_target("event:2024-02-29"), Some(Target::Event { date: "2024-02-29".into() }));
        assert_eq!(parse_target("slot:1"), Some(Target::Slot(1)));
        assert_eq!(parse_target("slot:6"), Some(Target::Slot(6)));
        for bad in ["slot:0", "slot:7", "slot:", "slot:x", "slot:-1"] {
            assert_eq!(parse_target(bad), None, "{bad}");
        }
        for bad in ["", "event:", "event:2023-02-29", "event:tomorrow", "notes", "Note"] {
            assert_eq!(parse_target(bad), None, "{bad}");
        }
    }

    #[test]
    fn an_event_keeps_its_text_date_and_time() {
        let target = Target::Event { date: "2026-11-10".into() };
        let item = build_item(&target, "  Réunion à l'école ", "8:00", 42).unwrap();
        assert!(item.notes.is_empty());
        let e = &item.events[0];
        assert_eq!((e.title.as_str(), e.date.as_str(), e.time.as_deref(), e.updated_at_ms, e.deleted), ("Réunion à l'école", "2026-11-10", Some("08:00"), 42, false));
        assert_eq!(e.id.len(), 32);
    }

    #[test]
    fn a_note_ignores_the_time_field() {
        let item = build_item(&Target::Note, "Acheter du pain", "not used", 7).unwrap();
        assert!(item.events.is_empty());
        assert_eq!(item.notes[0].text, "Acheter du pain");
        assert!(!item.notes[0].done);
    }

    #[test]
    fn nothing_is_built_from_bad_input() {
        let event = Target::Event { date: "2026-11-10".into() };
        assert_eq!(build_item(&event, "   ", "", 1), Err(Problem::Empty));
        assert_eq!(build_item(&Target::Note, "", "", 1), Err(Problem::Empty));
        assert_eq!(build_item(&event, "Titre", "25:00", 1), Err(Problem::Time));
    }

    #[test]
    fn long_text_shows_its_end() {
        assert_eq!(tail("abc", 5), "abc");
        assert_eq!(tail("abcdef", 3), "def");
        assert_eq!(tail("éàü", 2), "àü");
        assert_eq!(tail("", 3), "");
    }
}
