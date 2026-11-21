//! Large agenda window, laid out like a desktop calendar. It runs as its own
//! short-lived process (`--agenda-window`), so the island carries none of it
//! while the window is closed. Day, week and month views, read-only .ics
//! events, and the post-its in a sidebar.
//!
//! Nothing runs while nothing happens: the window repaints on input, on the
//! editor process exiting, and on a one-shot timer for the "now" line.

use std::cell::RefCell;
use std::os::windows::io::AsRawHandle;
use std::process::Child;

use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Dwm::{DWMWA_USE_IMMERSIVE_DARK_MODE, DwmSetWindowAttribute};
use windows::Win32::Graphics::Gdi::ValidateRect;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::SystemInformation::GetLocalTime;
use windows::Win32::System::Threading::{CreateMutexW, INFINITE};
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{BOOL, HSTRING, PCWSTR, Result, w};

use crate::agenda::{self, Agenda, WM_AGENDA_CHANGED};
use crate::config::{self, Config};
use crate::render::Renderer;
use crate::state::unix_ms;
use crate::ui::theme::*;
use crate::ui::{Align, Color, Icon, Input, Rect, TextStyle, Ui, WidgetState, id, is_english, tr};

type Day = (i32, u32, u32);

const TOOL_H: f32 = 52.0;
const SIDE_W: f32 = 232.0;
const GUTTER_W: f32 = 54.0;
const HOUR_H: f32 = 48.0;
const HEAD_H: f32 = 54.0;
const CHIP_H: f32 = 16.0;
const MIN_W: f32 = 820.0;
const MIN_H: f32 = 520.0;
const NOTE_H: f32 = 36.0;
const CROSS: f32 = 20.0;
/// Hour the grid shows at the top when the window opens.
const OPEN_HOUR: f32 = 7.0;
/// An event has no end time: it is drawn for one hour.
const DEFAULT_MINUTES: u32 = 60;

const WM_ICS: u32 = WM_APP + 2;
const WM_MOUSELEAVE: u32 = 0x02A3;
const WM_CAPTURECHANGED: u32 = 0x0215;
const TIMER_NOW: usize = 1;

const RED: Color = Color::rgb(0xFF, 0x45, 0x3A);

// ---------------------------------------------------------------------------
// Pure layout logic.
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Day,
    Week,
    Month,
}

/// Days since 1970-01-01 of a civil date.
fn days_from_civil((y, m, d): Day) -> i64 {
    let y = i64::from(y) - i64::from(m <= 2);
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (i64::from(m) + 9) % 12;
    let doy = (153 * mp + 2) / 5 + i64::from(d) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn civil_from_days(z: i64) -> Day {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y as i32, m, d)
}

pub fn add_days(d: Day, n: i64) -> Day {
    civil_from_days(days_from_civil(d) + n)
}

fn weekday_of((y, m, d): Day) -> u32 {
    agenda::weekday(y, m, d)
}

/// Monday of the week holding `d`.
pub fn week_start(d: Day) -> Day {
    add_days(d, -i64::from(weekday_of(d)))
}

/// Six Monday-first weeks showing the month, with the days of the
/// neighbouring months that complete the first and last week.
pub fn month_cells(year: i32, month: u32) -> [Day; 42] {
    let start = week_start((year, month, 1));
    std::array::from_fn(|i| add_days(start, i as i64))
}

/// `HH:MM` as minutes since midnight.
pub fn minutes_of(time: &str) -> Option<u32> {
    let (h, m) = time.split_once(':')?;
    let (h, m): (u32, u32) = (h.parse().ok()?, m.parse().ok()?);
    (h < 24 && m < 60).then_some(h * 60 + m)
}

/// Start and end minute of an event on the time grid. Without an end time the
/// event lasts an hour, cut at midnight.
pub fn slot(time: Option<&str>) -> Option<(u32, u32)> {
    let start = minutes_of(time?)?;
    Some((start, (start + DEFAULT_MINUTES).min(24 * 60)))
}

/// Side-by-side layout of overlapping slots: for each slot its column and the
/// number of columns of the group of overlapping slots it belongs to.
pub fn columns(slots: &[(u32, u32)]) -> Vec<(usize, usize)> {
    let mut order: Vec<usize> = (0..slots.len()).collect();
    order.sort_by_key(|&i| (slots[i].0, slots[i].1, i));
    let mut out = vec![(0, 1); slots.len()];
    let mut group: Vec<usize> = Vec::new();
    let mut group_end = 0;
    let mut ends: Vec<u32> = Vec::new();
    let flush = |group: &mut Vec<usize>, ends: &mut Vec<u32>, out: &mut Vec<(usize, usize)>| {
        for &i in group.iter() {
            out[i].1 = ends.len().max(1);
        }
        group.clear();
        ends.clear();
    };
    for i in order {
        let (start, end) = slots[i];
        if !group.is_empty() && start >= group_end {
            flush(&mut group, &mut ends, &mut out);
        }
        let column = match ends.iter().position(|&e| e <= start) {
            Some(c) => {
                ends[c] = end;
                c
            }
            None => {
                ends.push(end);
                ends.len() - 1
            }
        };
        out[i].0 = column;
        group_end = if group.is_empty() { end } else { group_end.max(end) };
        group.push(i);
    }
    flush(&mut group, &mut ends, &mut out);
    out
}

/// Minute under a vertical offset into the grid, on a half hour.
pub fn minute_at(y: f32, hour_h: f32) -> u32 {
    let minutes = (y / hour_h * 60.0).floor().max(0.0) as u32;
    (minutes / 30 * 30).min(23 * 60 + 30)
}

/// Event lines a month cell shows, and how many are left for the "+n".
pub fn month_plan(count: usize, lines: usize) -> (usize, usize) {
    let cap = lines.min(3);
    if count <= cap {
        return (count, 0);
    }
    let shown = cap.min(lines.saturating_sub(1));
    (shown, count - shown)
}

pub fn clamp_scroll(scroll: f32, content: f32, viewport: f32) -> f32 {
    scroll.clamp(0.0, (content - viewport).max(0.0))
}

fn intersect(a: Rect, b: Rect) -> Option<Rect> {
    let (x, y) = (a.x.max(b.x), a.y.max(b.y));
    let (r, bt) = (a.right().min(b.right()), a.bottom().min(b.bottom()));
    (r > x && bt > y).then(|| Rect::new(x, y, r - x, bt - y))
}

/// The day an arrow moves to.
pub fn shift(anchor: Day, mode: Mode, dir: i32) -> Day {
    match mode {
        Mode::Day => add_days(anchor, i64::from(dir)),
        Mode::Week => add_days(anchor, 7 * i64::from(dir)),
        Mode::Month => {
            let (y, m) = agenda::shift_month(anchor.0, anchor.1, dir);
            (y, m, anchor.2.min(agenda::days_in_month(y, m)))
        }
    }
}

fn day_name(index: u32, english: bool) -> &'static str {
    const FR: [&str; 7] = ["lun.", "mar.", "mer.", "jeu.", "ven.", "sam.", "dim."];
    const EN: [&str; 7] = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];
    (if english { EN } else { FR })[index as usize % 7]
}

fn lower_month(m: u32, english: bool) -> String {
    let name = agenda::month_name(m, english);
    if english { name.to_string() } else { name.to_lowercase() }
}

/// Title of the period on screen, `10 – 16 novembre 2026` for a week.
pub fn period_title(mode: Mode, anchor: Day, english: bool) -> String {
    match mode {
        Mode::Month => format!("{} {}", agenda::month_name(anchor.1, english), anchor.0),
        Mode::Day => {
            let (y, m, d) = anchor;
            format!("{} {y}", agenda::day_label(y, m, d, english))
        }
        Mode::Week => {
            let first = week_start(anchor);
            let last = add_days(first, 6);
            if first.1 == last.1 {
                format!("{} – {} {} {}", first.2, last.2, lower_month(last.1, english), last.0)
            } else if first.0 == last.0 {
                format!("{} {} – {} {} {}", first.2, lower_month(first.1, english), last.2, lower_month(last.1, english), last.0)
            } else {
                format!("{} {} {} – {} {} {}", first.2, lower_month(first.1, english), first.0, last.2, lower_month(last.1, english), last.0)
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Model.
// ---------------------------------------------------------------------------

/// One thing to show on a day, from the local agenda or from the .ics file.
#[derive(Clone, Debug, PartialEq)]
struct Item {
    title: String,
    date: String,
    time: Option<String>,
    /// Id of a local event; none for a read-only .ics one.
    local: Option<String>,
    /// End time and room of an .ics lesson.
    end: Option<String>,
    place: String,
    /// Free project hours (RN) of a university timetable: drawn fainter.
    project: bool,
    /// Colour the user gave this lesson or its subject.
    color: Option<crate::ui::Color>,
    /// The .ics lesson behind this item, for its colour and visibility.
    lesson: Option<crate::widgets::ics::IcsEvent>,
    /// Hidden by the user; listed only while hidden lessons are shown.
    hidden: bool,
}

impl Item {
    fn span(&self) -> Option<(u32, u32)> {
        let (start, default_end) = slot(self.time.as_deref())?;
        let end = self.end.as_deref().and_then(minutes_of).filter(|e| *e > start).unwrap_or(default_end);
        Some((start, end))
    }

    /// "13:00 – 16:30 · BL CH226".
    fn when(&self) -> Option<String> {
        let time = self.time.as_ref()?;
        let mut out = match &self.end {
            Some(end) => format!("{time} – {end}"),
            None => time.clone(),
        };
        if !self.place.is_empty() {
            out.push_str(" · ");
            out.push_str(&self.place);
        }
        Some(out)
    }
}

enum Act {
    NewEvent(String, Option<u32>),
    NewNote,
    DeleteEvent(String),
    DeleteNote(String),
    ToggleNote(String),
    /// Colour and visibility of a lesson, or of every lesson of its subject.
    Style { lesson: crate::widgets::ics::IcsEvent, all: bool, color: Option<String>, hidden: Option<bool> },
    Close,
}

/// Colours a lesson can take, after "none".
const LESSON_COLORS: [&str; 7] = ["#E5484D", "#F59E42", "#FFC53D", "#4ADE80", "#4C9BFF", "#A78BFA", "#F472B6"];
/// Two clicks on the same event within this time open its style panel.
const DOUBLE_CLICK_MS: u64 = 450;

struct Model {
    mode: Mode,
    anchor: Day,
    today: Day,
    now_min: u32,
    scroll: f32,
    notes_scroll: f32,
    selected: Option<Item>,
    /// The selected lesson's style panel is open (after a double click).
    styling: bool,
    /// Its choice applies to every lesson of the same name.
    style_all: bool,
    last_click: Option<(Item, u64)>,
    /// Hidden lessons are listed, faded, so they can be shown again.
    show_hidden: bool,
    agenda: Agenda,
    english: bool,
    /// Instant at which the frame would look different (Unix ms).
    deadline: Option<u64>,
}

fn button(ui: &mut Ui, w: u64, r: Rect, label: &str, on: bool) -> bool {
    let resp = ui.interact(w, r);
    let fill = if on || resp.held { TILE_HOVER } else if resp.hovered { TILE } else { TILE.alpha(0.0) };
    ui.fill(r, R_SMALL, fill);
    let color = if on { TEXT } else { TEXT_DIM };
    ui.text(r, label, TextStyle::new(12.5, if on { 600 } else { 500 }, color).aligned(Align::Center));
    resp.clicked
}

impl Model {
    fn items_of(&self, day: Day) -> Vec<Item> {
        let date = agenda::date_key(day.0, day.1, day.2);
        let mut items: Vec<Item> = agenda::events_of_day(&self.agenda.events, &date)
            .into_iter()
            .map(|e| Item { title: e.title.clone(), date: date.clone(), time: e.time.clone(), local: Some(e.id.clone()), end: e.time.as_ref().and(e.end.clone()), place: String::new(), project: false, color: None, lesson: None, hidden: false })
            .collect();
        let lessons = if self.show_hidden { crate::widgets::ics::of_day_all(&date) } else { crate::widgets::ics::of_day(&date) };
        items.extend(lessons.into_iter().map(|e| Item {
            title: e.label(),
            date: date.clone(),
            project: e.is_project(),
            color: crate::widgets::ics::ui_color(&e),
            hidden: crate::widgets::ics::is_hidden(&e),
            time: e.time.clone(),
            local: None,
            end: e.end.clone(),
            place: e.place.clone(),
            lesson: Some(e),
        }));
        items.sort_by(|a, b| a.time.cmp(&b.time).then_with(|| a.title.cmp(&b.title)));
        items
    }

    /// A click on an event selects it; a second one on the same event opens
    /// the style panel of an .ics lesson.
    fn pick(&mut self, item: Item, now_ms: u64) {
        let double = self.last_click.as_ref().is_some_and(|(last, at)| same_event(last, &item) && now_ms.saturating_sub(*at) <= DOUBLE_CLICK_MS);
        self.styling = double && item.lesson.is_some();
        self.last_click = if double { None } else { Some((item.clone(), now_ms)) };
        self.selected = Some(item);
    }

    fn open_day(&mut self, day: Day) {
        self.anchor = day;
        self.mode = Mode::Day;
        self.selected = None;
    }

    fn build(&mut self, ui: &mut Ui, w: f32, h: f32) -> Vec<Act> {
        let mut acts = Vec::new();
        ui.fill(Rect::new(0.0, 0.0, w, h), 0.0, SURFACE);
        self.toolbar(ui, w);
        ui.fill(Rect::new(0.0, TOOL_H, w, 1.0), 0.0, HAIRLINE);

        let mut main = Rect::new(0.0, TOOL_H + 1.0, w, (h - TOOL_H - 1.0).max(0.0));
        let side = main.take_right(SIDE_W, 0.0);
        ui.fill(Rect::new(side.x, side.y, 1.0, side.h), 0.0, HAIRLINE);

        // The panel of the selected event sits over the grid: it takes the
        // pointer first.
        let panel_h = if self.styling { 132.0 } else { 58.0 };
        let panel = self.selected.as_ref().map(|_| Rect::new(main.x + 16.0, main.bottom() - panel_h - 16.0, (main.w - 32.0).min(460.0), panel_h));
        let blocked = panel.is_some_and(|p| ui.hovered(p));

        match self.mode {
            Mode::Month => self.month(ui, main, blocked, &mut acts),
            _ => self.time_grid(ui, main, blocked, &mut acts),
        }
        if let Some(panel) = panel {
            self.panel(ui, panel, &mut acts);
        }
        self.sidebar(ui, side.inset_xy(0.0, 0.0), &mut acts);

        // The "now" line and today's marks move at the next minute.
        let next = (ui.now_ms / 60_000 + 1) * 60_000 + 250;
        self.deadline = Some(next);
        acts
    }

    fn toolbar(&mut self, ui: &mut Ui, w: f32) {
        let mut x = 16.0;
        if button(ui, id("aw.today", 0), Rect::new(x, 11.0, 104.0, 30.0), tr("Aujourd'hui", "Today"), false) {
            self.anchor = self.today;
        }
        ui.stroke(Rect::new(x, 11.0, 104.0, 30.0).inset(0.5), R_SMALL, 1.0, HAIRLINE);
        x += 116.0;
        if ui.icon_button(id("aw.prev", 0), Rect::new(x, 12.0, 28.0, 28.0), Icon::Back, 15.0, TEXT_DIM).clicked {
            self.anchor = shift(self.anchor, self.mode, -1);
        }
        x += 32.0;
        if ui.icon_button(id("aw.next", 0), Rect::new(x, 12.0, 28.0, 28.0), Icon::Forward, 15.0, TEXT_DIM).clicked {
            self.anchor = shift(self.anchor, self.mode, 1);
        }
        x += 40.0;

        let seg_w = 78.0;
        let seg = Rect::new(w - 16.0 - seg_w * 3.0 - 4.0, 11.0, seg_w * 3.0 + 4.0, 30.0);
        ui.fill(seg, R_SMALL + 2.0, TILE);
        let labels = [(Mode::Day, tr("Jour", "Day")), (Mode::Week, tr("Semaine", "Week")), (Mode::Month, tr("Mois", "Month"))];
        for (i, (mode, label)) in labels.iter().enumerate() {
            let r = Rect::new(seg.x + 2.0 + i as f32 * seg_w, seg.y + 2.0, seg_w, 26.0);
            if button(ui, id("aw.mode", i), r, label, self.mode == *mode) {
                self.mode = *mode;
                self.selected = None;
            }
        }

        // Lessons hidden by the user can be listed again to show them back.
        if crate::widgets::ics::is_configured() {
            let r = Rect::new(seg.x - 12.0 - 96.0, 11.0, 96.0, 30.0);
            if button(ui, id("aw.hidden", 0), r, tr("Masqués", "Hidden"), self.show_hidden) {
                self.show_hidden = !self.show_hidden;
                self.selected = None;
            }
        }
        let seg_x = if crate::widgets::ics::is_configured() { seg.x - 108.0 } else { seg.x };
        let title = Rect::new(x, 0.0, (seg_x - x - 12.0).max(0.0), TOOL_H);
        ui.text(title, &period_title(self.mode, self.anchor, self.english), TextStyle::new(17.0, 600, TEXT));
    }

    fn panel(&mut self, ui: &mut Ui, panel: Rect, acts: &mut Vec<Act>) {
        let Some(item) = self.selected.clone() else { return };
        ui.fill(panel, R_TILE, TILE);
        ui.stroke(panel.inset(0.5), R_TILE, 1.0, HAIRLINE);
        let mut r = panel.inset_xy(14.0, 8.0);
        let close = r.take_right(CROSS, 4.0).centered(CROSS, CROSS);
        if ui.icon_button(id("aw.panel.close", 0), close, Icon::Close, 10.0, TEXT_FAINT).clicked {
            self.selected = None;
        }
        if let Some(local) = &item.local {
            let trash = r.take_right(CROSS, 4.0).centered(CROSS, CROSS);
            if ui.icon_button(id("aw.panel.delete", 0), trash, Icon::Trash, 14.0, TEXT_DIM).clicked {
                acts.push(Act::DeleteEvent(local.clone()));
                self.selected = None;
            }
        }
        let title = r.take_top(20.0, 0.0);
        ui.text(title, &item.title, TextStyle::new(13.0, 600, TEXT));
        let when = match agenda::parse_date(&item.date) {
            Some((y, m, d)) => agenda::day_label(y, m, d, self.english),
            None => item.date.clone(),
        };
        let time = item.when().unwrap_or_else(|| tr("toute la journée", "all day").to_string());
        let kind = if item.local.is_some() { "" } else if self.styling { "" } else { tr("  ·  double-clic pour la couleur", "  ·  double-click for its colour") };
        let line = r.take_top(18.0, 6.0);
        ui.text(line, &format!("{when}  ·  {time}{kind}"), TextStyle::new(11.5, 400, TEXT_DIM));
        let Some(lesson) = item.lesson.clone().filter(|_| self.styling) else { return };

        // Colours: "none" first, then the palette; the current one is ringed.
        let current = crate::widgets::ics::color_of(&lesson).unwrap_or_default();
        let mut row = r.take_top(26.0, 10.0);
        for k in 0..=LESSON_COLORS.len() {
            let swatch = Rect::new(row.x + k as f32 * 28.0, row.center().y - 10.0, 20.0, 20.0);
            let resp = ui.interact(id("aw.color", k), swatch);
            let hex = if k == 0 { "" } else { LESSON_COLORS[k - 1] };
            match crate::ui::theme::parse_hex(hex) {
                Some(rgb) => ui.fill(swatch, 10.0, crate::ui::Color::rgb((rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8)),
                None => ui.stroke(swatch.inset(1.0), 9.0, 1.5, TEXT_DIM),
            }
            if current.eq_ignore_ascii_case(hex) {
                ui.stroke(swatch.inset(-3.0), 13.0, 1.5, TEXT);
            } else if resp.hovered {
                ui.stroke(swatch.inset(-3.0), 13.0, 1.0, TEXT_FAINT);
            }
            if resp.clicked {
                acts.push(Act::Style { lesson: lesson.clone(), all: self.style_all, color: Some(hex.to_string()), hidden: None });
            }
        }
        let hide = row.take_right(104.0, 0.0);
        let label = if item.hidden { tr("Réafficher", "Show again") } else { tr("Masquer", "Hide") };
        if button(ui, id("aw.hide", 0), hide, label, false) {
            acts.push(Act::Style { lesson: lesson.clone(), all: self.style_all, color: None, hidden: Some(!item.hidden) });
            if !item.hidden && !self.show_hidden {
                self.selected = None;
            }
        }
        ui.stroke(hide.inset(0.5), R_SMALL, 1.0, HAIRLINE);

        // Which lessons the choice applies to.
        let seg = r.take_top(28.0, 0.0);
        ui.fill(seg, R_SMALL + 2.0, TILE_HOVER.alpha(0.5));
        let name: String = lesson.title.chars().take(28).collect();
        let labels = [tr("Ce cours", "This lesson").to_string(), format!("{} « {name} »", tr("Tous les", "Every"))];
        let half = (seg.w - 4.0) / 2.0;
        for (i, label) in labels.iter().enumerate() {
            let cell = Rect::new(seg.x + 2.0 + i as f32 * half, seg.y + 2.0, half, seg.h - 4.0);
            if button(ui, id("aw.scope", i), cell, label, self.style_all == (i == 1)) {
                self.style_all = i == 1;
            }
        }
    }

    fn sidebar(&mut self, ui: &mut Ui, side: Rect, acts: &mut Vec<Act>) {
        let mut r = side.inset_xy(14.0, 10.0);
        let mut head = r.take_top(26.0, 4.0);
        let add = head.take_right(CROSS, 4.0).centered(CROSS, CROSS);
        ui.text(head, "Post-it", TextStyle::new(13.5, 600, TEXT));
        if ui.icon_button(id("aw.note.add", 0), add, Icon::Plus, 13.0, TEXT_DIM).clicked {
            acts.push(Act::NewNote);
        }
        let notes = agenda::visible_notes(&self.agenda.notes);
        if notes.is_empty() {
            ui.text(Rect::new(r.x, r.y, r.w, 22.0), tr("Aucun post-it", "No notes"), TextStyle::new(12.0, 400, TEXT_FAINT));
            return;
        }
        let content = notes.len() as f32 * NOTE_H;
        if ui.hovered(r) && ui.input.wheel != 0.0 {
            self.notes_scroll -= ui.input.wheel * NOTE_H;
        }
        self.notes_scroll = clamp_scroll(self.notes_scroll, content, r.h);
        ui.push_clip(r, 0.0);
        for (i, note) in notes.iter().enumerate() {
            let mut row = Rect::new(r.x, r.y + i as f32 * NOTE_H - self.notes_scroll, r.w - 6.0, NOTE_H);
            let Some(visible) = intersect(row, r) else { continue };
            let tick = row.take_left(CROSS, 4.0).centered(CROSS, CROSS);
            let cross = row.take_right(CROSS, 4.0).centered(CROSS, CROSS);
            let tick_hit = intersect(tick, visible);
            let tick_resp = tick_hit.map(|t| ui.interact(id("aw.note.tick", i), t));
            let border = if tick_resp.is_some_and(|t| t.hovered) { TEXT_DIM } else { TEXT_FAINT };
            ui.stroke(tick.centered(14.0, 14.0).inset(0.5), 3.5, 1.0, border);
            if note.done {
                ui.icon(Icon::Check, tick.centered(12.0, 12.0), accent());
            }
            if tick_resp.is_some_and(|t| t.clicked) {
                acts.push(Act::ToggleNote(note.id.clone()));
            }
            if let Some(hit) = intersect(cross, visible) {
                if ui.icon_button(id("aw.note.delete", i), hit, Icon::Close, 10.0, TEXT_FAINT).clicked {
                    acts.push(Act::DeleteNote(note.id.clone()));
                }
            }
            ui.push_clip(row, 0.0);
            ui.text(row, &note.text, TextStyle::new(11.5, 400, if note.done { TEXT_FAINT } else { TEXT }).wrapped());
            ui.pop_clip();
        }
        ui.pop_clip();
        if content > r.h {
            let bar = (r.h / content * r.h).max(16.0);
            let y = r.y + self.notes_scroll / (content - r.h) * (r.h - bar);
            ui.fill(Rect::new(side.right() - 5.0, y, 2.0, bar), 1.0, TILE_HOVER);
        }
    }

    // ----- week and day -----

    fn time_grid(&mut self, ui: &mut Ui, main: Rect, blocked: bool, acts: &mut Vec<Act>) {
        let days: Vec<Day> = match self.mode {
            Mode::Day => vec![self.anchor],
            _ => {
                let first = week_start(self.anchor);
                (0..7).map(|i| add_days(first, i)).collect()
            }
        };
        let n = days.len();
        let items: Vec<Vec<Item>> = days.iter().map(|&d| self.items_of(d)).collect();
        let col_w = ((main.w - GUTTER_W - 12.0) / n as f32).max(20.0);
        let col_x = |i: usize| main.x + GUTTER_W + i as f32 * col_w;

        // Column titles.
        for (i, &day) in days.iter().enumerate() {
            let c = Rect::new(col_x(i), main.y, col_w, HEAD_H);
            let is_today = day == self.today;
            let hit = ui.interact(id("aw.head", i), c);
            if hit.clicked && n > 1 {
                self.open_day(day);
                return;
            }
            let name = Rect::new(c.x, c.y + 6.0, c.w, 16.0);
            ui.text(name, day_name(weekday_of(day), self.english), TextStyle::new(11.5, 500, if is_today { accent() } else { TEXT_DIM }).aligned(Align::Center));
            let disc = Rect::new(c.center().x - 14.0, c.y + 24.0, 28.0, 28.0);
            if is_today {
                ui.fill(disc, 14.0, accent());
            } else if hit.hovered && n > 1 {
                ui.fill(disc, 14.0, TILE_HOVER);
            }
            let color = if is_today { SURFACE } else { TEXT };
            ui.text(disc, &day.2.to_string(), TextStyle::new(15.0, if is_today { 700 } else { 500 }, color).aligned(Align::Center));
        }

        // All-day strip.
        let rows = items.iter().map(|d| d.iter().filter(|i| i.span().is_none()).count()).max().unwrap_or(0).clamp(1, 3);
        let strip = Rect::new(main.x, main.y + HEAD_H, main.w - 12.0, 8.0 + rows as f32 * (CHIP_H + 2.0));
        ui.fill(Rect::new(main.x, strip.y, main.w, 1.0), 0.0, HAIRLINE);
        ui.text(Rect::new(main.x, strip.y, GUTTER_W - 8.0, 22.0), tr("journée", "all day"), TextStyle::new(10.0, 400, TEXT_FAINT).aligned(Align::End));
        for (i, &day) in days.iter().enumerate() {
            let c = Rect::new(col_x(i), strip.y, col_w, strip.h);
            let all_day: Vec<&Item> = items[i].iter().filter(|it| it.span().is_none()).collect();
            let (shown, extra) = month_plan(all_day.len(), rows);
            for (k, item) in all_day.iter().take(shown).enumerate() {
                let r = Rect::new(c.x + 2.0, c.y + 4.0 + k as f32 * (CHIP_H + 2.0), c.w - 4.0, CHIP_H);
                if self.chip(ui, id("aw.allday", i * 8 + k), r, item, blocked) {
                    self.pick((*item).clone(), ui.now_ms);
                }
            }
            if extra > 0 {
                let r = Rect::new(c.x + 2.0, c.y + 4.0 + shown as f32 * (CHIP_H + 2.0), c.w - 4.0, CHIP_H);
                let resp = ui.interact(id("aw.more", i), r);
                ui.text(r.inset_xy(4.0, 0.0), &format!("+{extra}"), TextStyle::new(10.5, 500, if resp.hovered { TEXT } else { TEXT_DIM }));
                if resp.clicked && n > 1 {
                    self.open_day(day);
                    return;
                }
            }
            if !blocked {
                let resp = ui.interact(id("aw.allday.empty", i), c);
                if resp.clicked {
                    acts.push(Act::NewEvent(agenda::date_key(day.0, day.1, day.2), None));
                }
            }
        }
        ui.fill(Rect::new(main.x, strip.bottom(), main.w, 1.0), 0.0, HAIRLINE);

        // Scrollable hours.
        let vp = Rect::new(main.x, strip.bottom() + 1.0, main.w, (main.bottom() - strip.bottom() - 1.0).max(0.0));
        let content = 24.0 * HOUR_H;
        if ui.hovered(vp) && ui.input.wheel != 0.0 {
            self.scroll -= ui.input.wheel * HOUR_H;
        }
        self.scroll = clamp_scroll(self.scroll, content, vp.h);
        let scroll = self.scroll;
        let y_of = |minute: f32| vp.y + minute / 60.0 * HOUR_H - scroll;

        // Place the timed events of every column.
        struct Placed {
            rect: Rect,
            item: Item,
        }
        let mut placed: Vec<Placed> = Vec::new();
        for (i, day_items) in items.iter().enumerate() {
            let timed: Vec<&Item> = day_items.iter().filter(|it| it.span().is_some()).collect();
            let spans: Vec<(u32, u32)> = timed.iter().filter_map(|it| it.span()).collect();
            for ((item, &(start, end)), (col, total)) in timed.iter().zip(&spans).zip(columns(&spans)) {
                let lane = (col_w - 4.0) / total as f32;
                let y = y_of(start as f32);
                let h = ((end - start) as f32 / 60.0 * HOUR_H - 2.0).max(CHIP_H);
                placed.push(Placed { rect: Rect::new(col_x(i) + 2.0 + col as f32 * lane, y, (lane - 2.0).max(8.0), h), item: (*item).clone() });
            }
        }

        // Pointer first (events over empty slots), drawing after.
        let mut event_hover = false;
        let mut clicked_event: Option<Item> = None;
        for (k, p) in placed.iter().enumerate() {
            let Some(hit) = intersect(p.rect, vp).filter(|_| !blocked) else { continue };
            let resp = ui.interact(id("aw.event", k), hit);
            event_hover |= resp.hovered;
            if resp.clicked {
                clicked_event = Some(p.item.clone());
            }
        }
        let mut slot_hover: Option<(usize, u32)> = None;
        for (i, &day) in days.iter().enumerate() {
            let Some(hit) = intersect(Rect::new(col_x(i), vp.y, col_w, content), vp).filter(|_| !blocked) else { continue };
            let resp = ui.interact(id("aw.slot", i), hit);
            let minute = ui.input.mouse.map(|m| minute_at(m.y - vp.y + scroll, HOUR_H)).unwrap_or(0);
            if resp.hovered && !event_hover {
                slot_hover = Some((i, minute));
            }
            if resp.clicked {
                self.selected = None;
                acts.push(Act::NewEvent(agenda::date_key(day.0, day.1, day.2), Some(minute)));
            }
        }
        if let Some(item) = clicked_event {
            self.pick(item, ui.now_ms);
        }

        ui.push_clip(vp, 0.0);
        for (i, &day) in days.iter().enumerate() {
            if day == self.today {
                ui.fill(Rect::new(col_x(i), vp.y, col_w, vp.h), 0.0, accent().alpha(0.045));
            }
            ui.fill(Rect::new(col_x(i), vp.y, 1.0, vp.h), 0.0, HAIRLINE);
        }
        ui.fill(Rect::new(col_x(n), vp.y, 1.0, vp.h), 0.0, HAIRLINE);
        for hour in 0..=24 {
            let y = y_of(hour as f32 * 60.0);
            if y < vp.y - 20.0 || y > vp.bottom() + 20.0 {
                continue;
            }
            ui.fill(Rect::new(main.x + GUTTER_W - 6.0, y, col_x(n) - main.x - GUTTER_W + 6.0, 1.0), 0.0, HAIRLINE);
            if hour > 0 && hour < 24 {
                ui.text(Rect::new(main.x, y - 8.0, GUTTER_W - 12.0, 16.0), &format!("{hour:02}:00"), TextStyle::new(10.5, 400, TEXT_FAINT).aligned(Align::End));
            }
        }
        if let Some((i, minute)) = slot_hover {
            let y = y_of(minute as f32);
            ui.fill(Rect::new(col_x(i) + 1.0, y, col_w - 1.0, HOUR_H / 2.0), 0.0, TILE.alpha(0.8));
            ui.text(Rect::new(col_x(i) + 8.0, y, col_w - 16.0, HOUR_H / 2.0), &format!("+  {:02}:{:02}", minute / 60, minute % 60), TextStyle::new(10.5, 500, TEXT_FAINT));
        }
        for p in &placed {
            if intersect(p.rect, vp).is_some() {
                let selected = self.selected.as_ref().is_some_and(|s| same_event(s, &p.item));
                paint_event(ui, p.rect, &p.item, selected, false);
            }
        }
        // Now.
        if let Some(i) = days.iter().position(|&d| d == self.today) {
            let y = y_of(self.now_min as f32);
            ui.fill(Rect::new(col_x(i), y - 1.0, col_w, 2.0), 0.0, RED);
            ui.fill(Rect::new(col_x(i) - 4.0, y - 4.0, 8.0, 8.0), 4.0, RED);
        }
        ui.pop_clip();
        if content > vp.h {
            let bar = (vp.h / content * vp.h).max(24.0);
            let y = vp.y + scroll / (content - vp.h) * (vp.h - bar);
            ui.fill(Rect::new(main.right() - 5.0, y, 3.0, bar), 1.5, TILE_HOVER);
        }
    }

    /// A one-line event, for the all-day strip and the month cells. Returns
    /// true when clicked.
    fn chip(&self, ui: &mut Ui, w: u64, r: Rect, item: &Item, blocked: bool) -> bool {
        let clicked = if blocked { false } else { ui.interact(w, r).clicked };
        let selected = self.selected.as_ref().is_some_and(|s| same_event(s, item));
        paint_event(ui, r, item, selected, true);
        clicked
    }

    // ----- month -----

    fn month(&mut self, ui: &mut Ui, main: Rect, blocked: bool, acts: &mut Vec<Act>) {
        let (year, month) = (self.anchor.0, self.anchor.1);
        let cells = month_cells(year, month);
        let head_h = 28.0;
        let col_w = main.w / 7.0;
        let row_h = ((main.h - head_h) / 6.0).max(40.0);
        for c in 0..7 {
            let r = Rect::new(main.x + c as f32 * col_w, main.y, col_w, head_h);
            ui.text(r, day_name(c as u32, self.english), TextStyle::new(11.5, 500, TEXT_DIM).aligned(Align::Center));
        }
        let lines = (((row_h - 26.0) / (CHIP_H + 2.0)).floor().max(0.0)) as usize;
        for (index, &day) in cells.iter().enumerate() {
            let cell = Rect::new(main.x + (index % 7) as f32 * col_w, main.y + head_h + (index / 7) as f32 * row_h, col_w, row_h);
            let this_month = day.1 == month;
            let is_today = day == self.today;
            let items = self.items_of(day);
            let (shown, extra) = month_plan(items.len(), lines);

            // Interactions first: chips, "+n", then the empty cell.
            let mut chip_click: Option<Item> = None;
            for (k, item) in items.iter().take(shown).enumerate() {
                let r = Rect::new(cell.x + 3.0, cell.y + 26.0 + k as f32 * (CHIP_H + 2.0), cell.w - 6.0, CHIP_H);
                if self.chip(ui, id("aw.chip", index * 4 + k), r, item, blocked) {
                    chip_click = Some(item.clone());
                }
            }
            if extra > 0 {
                let r = Rect::new(cell.x + 3.0, cell.y + 26.0 + shown as f32 * (CHIP_H + 2.0), cell.w - 6.0, CHIP_H);
                let resp = ui.interact(id("aw.more", index), r);
                ui.text(r.inset_xy(4.0, 0.0), &format!("+{extra}"), TextStyle::new(10.5, 500, if resp.hovered { TEXT } else { TEXT_DIM }));
                if resp.clicked {
                    self.open_day(day);
                    return;
                }
            }
            let number = Rect::new(cell.x + 6.0, cell.y + 3.0, 24.0, 22.0);
            let num_resp = ui.interact(id("aw.daynum", index), number);
            if num_resp.clicked {
                self.open_day(day);
                return;
            }
            let resp = if blocked { Default::default() } else { ui.interact(id("aw.cell", index), cell) };
            if resp.clicked {
                self.selected = None;
                acts.push(Act::NewEvent(agenda::date_key(day.0, day.1, day.2), None));
            }
            if let Some(item) = chip_click {
                self.pick(item, ui.now_ms);
            }

            if resp.hovered || is_today {
                ui.fill(cell.inset(0.5), 0.0, if is_today { accent().alpha(0.045) } else { TILE.alpha(0.6) });
            }
            ui.fill(Rect::new(cell.x, cell.y, cell.w, 1.0), 0.0, HAIRLINE);
            ui.fill(Rect::new(cell.x, cell.y, 1.0, cell.h), 0.0, HAIRLINE);
            if is_today {
                ui.fill(number.centered(22.0, 22.0), 11.0, accent());
            } else if num_resp.hovered {
                ui.fill(number.centered(22.0, 22.0), 11.0, TILE_HOVER);
            }
            let color = if is_today { SURFACE } else if this_month { TEXT } else { TEXT_FAINT };
            ui.text(number, &day.2.to_string(), TextStyle::new(12.0, if is_today { 700 } else { 500 }, color).aligned(Align::Center));
        }
    }
}

/// The same event on screen, whatever its colour or visibility is now.
fn same_event(a: &Item, b: &Item) -> bool {
    a.title == b.title && a.date == b.date && a.time == b.time && a.local == b.local
}

/// An event as a rounded block. Local events carry a bar and the accent
/// tint; .ics events are fainter and outlined.
fn paint_event(ui: &mut Ui, r: Rect, item: &Item, selected: bool, compact: bool) {
    if item.hidden {
        ui.stroke(r.inset(0.5), 4.0, 1.0, HAIRLINE);
        let inner = r.inset_xy(7.0, 0.0);
        ui.push_clip(r, 4.0);
        ui.text(inner, &format!("{}  ·  {}", item.title, tr("masqué", "hidden")), TextStyle::new(10.5, 400, TEXT_FAINT));
        ui.pop_clip();
        if selected {
            ui.stroke(r.inset(0.75), 4.0, 1.5, TEXT);
        }
        return;
    }
    let local = item.local.is_some();
    let tint = if local { Some(accent()) } else { item.color };
    let fill = match tint {
        Some(c) => c.alpha(if item.project { 0.1 } else { 0.2 }),
        None if item.project => TILE.alpha(0.4),
        None => TILE,
    };
    ui.fill(r, 4.0, fill);
    if let Some(c) = tint {
        ui.fill(Rect::new(r.x, r.y + 2.0, 2.5, (r.h - 4.0).max(1.0)), 1.25, c);
    } else {
        ui.stroke(r.inset(0.5), 4.0, 1.0, HAIRLINE);
    }
    if selected {
        ui.stroke(r.inset(0.75), 4.0, 1.5, TEXT);
    }
    let (title_color, time_color) = if local { (TEXT, TEXT_DIM) } else if item.project { (TEXT_FAINT, TEXT_FAINT) } else { (TEXT_DIM, TEXT_FAINT) };
    let weight = if local { 600 } else { 400 };
    let inner = r.inset_xy(7.0, 0.0);
    ui.push_clip(r, 4.0);
    match (&item.time, compact || r.h < 34.0) {
        (Some(time), true) => {
            ui.text(inner, &format!("{time}  {}", item.title), TextStyle::new(10.5, weight, title_color));
        }
        (None, true) => ui.text(inner, &item.title, TextStyle::new(10.5, weight, title_color)),
        (time, false) => {
            ui.text(Rect::new(inner.x, r.y + 3.0, inner.w, 15.0), &item.title, TextStyle::new(11.5, weight, title_color));
            if let (Some(_), Some(when)) = (time, item.when()) {
                ui.text(Rect::new(inner.x, r.y + 18.0, inner.w, 14.0), &when, TextStyle::new(10.0, 400, time_color));
            }
        }
    }
    ui.pop_clip();
}

// ---------------------------------------------------------------------------
// Window.
// ---------------------------------------------------------------------------

struct Window {
    hwnd: HWND,
    renderer: Renderer,
    input: Input,
    widgets: WidgetState,
    last: Vec<crate::ui::Cmd>,
    scale: f32,
    /// Client size in device pixels.
    size: (u32, u32),
    model: Model,
    /// The editor process started from here; its exit reloads the file.
    child: Option<Child>,
    dirty: bool,
    tracking: bool,
}

thread_local! {
    static STATE: RefCell<Option<Window>> = const { RefCell::new(None) };
}

fn with<R>(f: impl FnOnce(&mut Window) -> R) -> Option<R> {
    STATE.with(|c| c.try_borrow_mut().ok().and_then(|mut g| g.as_mut().map(f)))
}

fn local_today() -> (Day, u32) {
    let now = unsafe { GetLocalTime() };
    ((now.wYear as i32, now.wMonth as u32, now.wDay as u32), now.wHour as u32 * 60 + now.wMinute as u32)
}

impl Window {
    fn reload(&mut self) {
        self.model.agenda = Agenda::load();
        self.dirty = true;
    }

    fn editor_done(&mut self) {
        self.child = None;
        self.reload();
    }

    /// Applies a change on top of the file, then tells the island.
    fn change(&mut self, change: impl FnOnce(&mut Agenda, u64)) {
        let mut agenda = Agenda::load();
        agenda.merge(&self.model.agenda);
        change(&mut agenda, unix_ms());
        if let Err(e) = agenda.save() {
            eprintln!("naelbar: could not save the agenda: {e}");
        }
        self.model.agenda = agenda;
        unsafe {
            if let Ok(island) = FindWindowW(w!("naelbar.window"), w!("naelbar control")) {
                let _ = PostMessageW(Some(island), WM_AGENDA_CHANGED, WPARAM(0), LPARAM(0));
            }
        }
        self.dirty = true;
    }

    fn run_act(&mut self, act: Act) {
        match act {
            Act::NewEvent(date, minute) => {
                let spec = match minute {
                    Some(m) => format!("event:{date}@{:02}:{:02}", m / 60, m % 60),
                    None => format!("event:{date}"),
                };
                if let Ok(exe) = std::env::current_exe() {
                    self.child = std::process::Command::new(exe).arg(format!("--edit={spec}")).spawn().ok();
                }
            }
            Act::NewNote => {
                if let Ok(exe) = std::env::current_exe() {
                    self.child = std::process::Command::new(exe).arg("--edit=note").spawn().ok();
                }
            }
            Act::DeleteEvent(id) => self.change(|a, now| a.delete_event(&id, now)),
            Act::DeleteNote(id) => self.change(|a, now| a.delete_note(&id, now)),
            Act::ToggleNote(id) => self.change(|a, now| a.toggle_note(&id, now)),
            Act::Style { lesson, all, color, hidden } => {
                // The config is shared with the island and the settings: read it fresh.
                let mut cfg = crate::config::Config::load();
                crate::widgets::ics::apply_style(&mut cfg.course_styles, &lesson, all, color, hidden);
                cfg.course_styles_at = crate::state::unix_ms();
                cfg.save();
                crate::widgets::ics::set_styles(&cfg.course_styles);
                if let Some(sel) = self.model.selected.as_mut() {
                    sel.color = crate::widgets::ics::ui_color(&lesson);
                    sel.hidden = crate::widgets::ics::is_hidden(&lesson);
                }
                unsafe {
                    if let Ok(island) = FindWindowW(w!("naelbar.window"), w!("naelbar control")) {
                        let _ = PostMessageW(Some(island), crate::config::WM_CONFIG_CHANGED, WPARAM(0), LPARAM(0));
                        // A sync sends the new colours to the phone.
                        let _ = PostMessageW(Some(island), WM_AGENDA_CHANGED, WPARAM(0), LPARAM(0));
                    }
                }
                self.dirty = true;
            }
            Act::Close => unsafe {
                let _ = DestroyWindow(self.hwnd);
            },
        }
    }

    /// Returns true if a frame went to the screen.
    fn frame(&mut self) -> bool {
        self.dirty = false;
        let (today, now_min) = local_today();
        self.model.today = today;
        self.model.now_min = now_min;
        let (w, h) = (self.size.0 as f32 / self.scale, self.size.1 as f32 / self.scale);
        let mut out = Vec::new();
        let mut ui = Ui::new(&self.input, &mut self.widgets, &mut out, self.scale, unix_ms());
        let acts = self.model.build(&mut ui, w, h);
        let cmds = std::mem::take(&mut ui.cmds);
        drop(ui);
        self.input.pressed = false;
        self.input.released = false;
        self.input.wheel = 0.0;
        if !self.input.down {
            self.widgets.active = None;
        }
        // One-shot timer for the next minute; it re-arms itself on the next frame.
        if let Some(at) = self.model.deadline.take() {
            let delay = at.saturating_sub(unix_ms()).clamp(50, 61_000) as u32;
            unsafe {
                SetTimer(Some(self.hwnd), TIMER_NOW, delay, None);
            }
        }
        for act in acts {
            self.run_act(act);
        }

        let mut presented = false;
        if cmds != self.last {
            if self.renderer.render(&cmds).is_ok() {
                self.last = cmds;
                presented = true;
            } else {
                self.last.clear();
            }
        }
        presented
    }

    fn resize(&mut self, w: u32, h: u32) {
        self.size = (w.max(1), h.max(1));
        self.renderer.set_size(self.size.0, self.size.1, self.scale);
        self.last.clear();
        self.dirty = true;
    }

    fn on_message(&mut self, msg: u32, wp: WPARAM, lp: LPARAM) -> Option<isize> {
        let lo = |v: isize| (v & 0xFFFF) as i16 as f32;
        let hi = |v: isize| ((v >> 16) & 0xFFFF) as i16 as f32;
        match msg {
            WM_MOUSEMOVE => {
                self.input.mouse = Some(crate::ui::Pt { x: lo(lp.0) / self.scale, y: hi(lp.0) / self.scale });
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
                self.input.mouse = Some(crate::ui::Pt { x: lo(lp.0) / self.scale, y: hi(lp.0) / self.scale });
                self.input.down = true;
                self.input.pressed = true;
                self.input.press_pos = self.input.mouse.unwrap_or_default();
                unsafe { SetCapture(self.hwnd) };
                self.dirty = true;
                Some(0)
            }
            WM_LBUTTONUP => {
                self.input.mouse = Some(crate::ui::Pt { x: lo(lp.0) / self.scale, y: hi(lp.0) / self.scale });
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
            WM_MOUSEWHEEL => {
                self.input.wheel += hi(wp.0 as isize) / 120.0;
                self.dirty = true;
                Some(0)
            }
            WM_KEYDOWN => {
                match VIRTUAL_KEY(wp.0 as u16) {
                    VK_ESCAPE => self.run_act(Act::Close),
                    VK_LEFT => {
                        self.model.anchor = shift(self.model.anchor, self.model.mode, -1);
                        self.dirty = true;
                    }
                    VK_RIGHT => {
                        self.model.anchor = shift(self.model.anchor, self.model.mode, 1);
                        self.dirty = true;
                    }
                    _ => {}
                }
                Some(0)
            }
            WM_ACTIVATE if wp.0 & 0xFFFF != 0 => {
                self.reload();
                None
            }
            WM_ICS => {
                self.dirty = true;
                Some(0)
            }
            WM_TIMER if wp.0 == TIMER_NOW => {
                unsafe {
                    let _ = KillTimer(Some(self.hwnd), TIMER_NOW);
                }
                self.dirty = true;
                Some(0)
            }
            WM_SIZE => {
                if wp.0 != SIZE_MINIMIZED as usize {
                    self.resize((lp.0 & 0xFFFF) as u32, ((lp.0 >> 16) & 0xFFFF) as u32);
                }
                Some(0)
            }
            WM_GETMINMAXINFO => {
                unsafe {
                    let info = &mut *(lp.0 as *mut MINMAXINFO);
                    info.ptMinTrackSize = POINT { x: (MIN_W * self.scale) as i32, y: (MIN_H * self.scale) as i32 };
                }
                Some(0)
            }
            WM_DPICHANGED => {
                self.scale = (wp.0 & 0xFFFF) as f32 / 96.0;
                let r = unsafe { *(lp.0 as *const RECT) };
                unsafe {
                    let _ = SetWindowPos(self.hwnd, None, r.left, r.top, r.right - r.left, r.bottom - r.top, SWP_NOZORDER | SWP_NOACTIVATE);
                }
                self.renderer.set_size(self.size.0, self.size.1, self.scale);
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
    match with(|s| s.on_message(msg, wp, lp)).flatten() {
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

/// Opens the agenda window, or brings the existing one forward.
pub fn run() -> Result<()> {
    unsafe {
        let class = w!("naelbar.agendawin");
        let _mutex = CreateMutexW(None, true, w!("Local\\naelbar.agenda-window"));
        if GetLastError() == ERROR_ALREADY_EXISTS {
            if let Ok(existing) = FindWindowW(class, PCWSTR::null()) {
                if IsIconic(existing).as_bool() {
                    let _ = ShowWindow(existing, SW_RESTORE);
                }
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

        let cfg = Config::load();
        config::apply_appearance(&cfg);
        crate::widgets::ics::set_source(&cfg.ics_url);
        crate::widgets::ics::set_styles(&cfg.course_styles);
        let style = WS_OVERLAPPEDWINDOW;
        let ex = WS_EX_NOREDIRECTIONBITMAP | WS_EX_APPWINDOW;
        let title = HSTRING::from(tr("Agenda naelbar", "naelbar agenda"));
        let hwnd = CreateWindowExW(ex, class, &title, style, CW_USEDEFAULT, CW_USEDEFAULT, 100, 100, None, None, Some(instance.into()), None)?;

        let dark: BOOL = true.into();
        let _ = DwmSetWindowAttribute(hwnd, DWMWA_USE_IMMERSIVE_DARK_MODE, &dark as *const BOOL as *const _, size_of::<BOOL>() as u32);

        // About 80 % of the work area, centred.
        let mut work = RECT::default();
        let _ = SystemParametersInfoW(SPI_GETWORKAREA, 0, Some(&mut work as *mut RECT as *mut _), SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0));
        let (ww, wh) = (work.right - work.left, work.bottom - work.top);
        let (fw, fh) = (ww * 4 / 5, wh * 4 / 5);
        let _ = SetWindowPos(hwnd, None, work.left + (ww - fw) / 2, work.top + (wh - fh) / 2, fw, fh, SWP_NOZORDER | SWP_NOACTIVATE);

        let scale = GetDpiForWindow(hwnd).max(96) as f32 / 96.0;
        let mut client = RECT::default();
        let _ = GetClientRect(hwnd, &mut client);
        let size = ((client.right - client.left).max(1) as u32, (client.bottom - client.top).max(1) as u32);
        let mut renderer = Renderer::new(hwnd);
        renderer.set_size(size.0, size.1, scale);

        let (today, now_min) = local_today();
        let state = Window {
            hwnd,
            renderer,
            input: Input::default(),
            widgets: WidgetState::default(),
            last: Vec::new(),
            scale,
            size,
            model: Model {
                mode: Mode::Week,
                anchor: today,
                today,
                now_min,
                scroll: OPEN_HOUR * HOUR_H,
                notes_scroll: 0.0,
                selected: None,
                styling: false,
                style_all: false,
                last_click: None,
                show_hidden: false,
                agenda: Agenda::load(),
                english: is_english(),
                deadline: None,
            },
            child: None,
            dirty: true,
            tracking: false,
        };
        STATE.with(|c| *c.borrow_mut() = Some(state));
        crate::widgets::ics::watch(hwnd.0 as isize, WM_ICS);
        crate::widgets::ics::request();
        with(|s| s.frame());
        let _ = ShowWindow(hwnd, SW_SHOW);
        bring_forward(hwnd);

        // Same pacing as the settings window: a frame is only drawn once the
        // swapchain can take it. When idle the thread sleeps in a kernel wait,
        // on the messages and on the editor process if one was started.
        let mut msg = MSG::default();
        let mut slot = false;
        'run: loop {
            let (dirty, child) = with(|s| (s.dirty, s.child.as_ref().map(|c| HANDLE(c.as_raw_handle())))).unwrap_or((false, None));
            if dirty && !slot {
                slot = match with(|s| s.renderer.waitable()).flatten() {
                    Some(h) => {
                        let r = MsgWaitForMultipleObjectsEx(Some(&[h]), 32, QS_ALLINPUT, MWMO_INPUTAVAILABLE);
                        r.0 != WAIT_OBJECT_0.0 + 1
                    }
                    None => true,
                };
            } else if !dirty {
                match child {
                    Some(h) => {
                        let r = MsgWaitForMultipleObjectsEx(Some(&[h]), INFINITE, QS_ALLINPUT, MWMO_INPUTAVAILABLE);
                        if r.0 != WAIT_OBJECT_0.0 + 1 {
                            // Exited, or the handle is unusable: either way stop waiting on it.
                            with(|s| s.editor_done());
                        }
                    }
                    None => {
                        if GetMessageW(&mut msg, None, 0, 0).0 <= 0 {
                            break;
                        }
                        let _ = TranslateMessage(&msg);
                        DispatchMessageW(&msg);
                    }
                }
            }
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                if msg.message == WM_QUIT {
                    break 'run;
                }
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
            if slot && with(|s| s.dirty).unwrap_or(false) && with(|s| s.frame()).unwrap_or(false) {
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
    fn week_starts_on_monday() {
        // 16 November 2026 is a Monday.
        assert_eq!(week_start((2026, 11, 16)), (2026, 11, 16));
        assert_eq!(week_start((2026, 11, 18)), (2026, 11, 16));
        assert_eq!(week_start((2026, 11, 22)), (2026, 11, 16));
        // A Sunday belongs to the week that ends on it.
        assert_eq!(week_start((2026, 11, 1)), (2026, 10, 26));
        assert_eq!(week_start((2027, 1, 1)), (2026, 12, 28));
    }

    #[test]
    fn days_add_across_months_years_and_leap_days() {
        assert_eq!(add_days((2026, 11, 30), 1), (2026, 12, 1));
        assert_eq!(add_days((2026, 12, 31), 1), (2027, 1, 1));
        assert_eq!(add_days((2024, 2, 28), 1), (2024, 2, 29));
        assert_eq!(add_days((2024, 3, 1), -1), (2024, 2, 29));
        assert_eq!(add_days((2026, 3, 1), -1), (2026, 2, 28));
        assert_eq!(add_days((2026, 11, 16), -400), add_days(add_days((2026, 11, 16), -200), -200));
    }

    #[test]
    fn month_grid_has_six_weeks_from_a_monday() {
        let cells = month_cells(2026, 11);
        assert_eq!(cells[0], (2026, 10, 26));
        assert_eq!(cells[6], (2026, 11, 1));
        assert_eq!(cells[41], (2026, 12, 6));
        assert!(cells.iter().step_by(7).all(|&d| weekday_of(d) == 0));
        assert_eq!(cells.iter().filter(|d| d.1 == 11).count(), 30);
        // A month that starts on a Monday begins on its first day.
        assert_eq!(month_cells(2026, 6)[0], (2026, 6, 1));
    }

    #[test]
    fn events_map_to_slots_of_an_hour() {
        assert_eq!(slot(Some("09:30")), Some((570, 630)));
        assert_eq!(slot(Some("00:00")), Some((0, 60)));
        // Cut at midnight.
        assert_eq!(slot(Some("23:30")), Some((1410, 1440)));
        assert_eq!(slot(None), None);
        assert_eq!(slot(Some("25:00")), None);
        assert_eq!(slot(Some("nine")), None);
        assert_eq!(minutes_of("07:05"), Some(425));
    }

    #[test]
    fn a_click_maps_to_a_half_hour() {
        assert_eq!(minute_at(0.0, HOUR_H), 0);
        assert_eq!(minute_at(HOUR_H * 7.0 + 10.0, HOUR_H), 420);
        assert_eq!(minute_at(HOUR_H * 7.0 + 30.0, HOUR_H), 450);
        assert_eq!(minute_at(-5.0, HOUR_H), 0);
        assert_eq!(minute_at(HOUR_H * 24.0, HOUR_H), 23 * 60 + 30);
    }

    #[test]
    fn overlapping_events_sit_side_by_side() {
        // Alone.
        assert_eq!(columns(&[(540, 600)]), [(0, 1)]);
        // Touching is not overlapping.
        assert_eq!(columns(&[(540, 600), (600, 660)]), [(0, 1), (0, 1)]);
        // Two at the same time.
        assert_eq!(columns(&[(540, 600), (540, 600)]), [(0, 2), (1, 2)]);
        // A chain: 9-10 and 9:30-10:30 overlap, 10:30-11:30 reuses the first column.
        assert_eq!(columns(&[(540, 600), (570, 630), (630, 690)]), [(0, 2), (1, 2), (0, 1)]);
        // Order of the input does not matter.
        assert_eq!(columns(&[(570, 630), (540, 600)]), [(1, 2), (0, 2)]);
        // Three at once.
        let three = columns(&[(600, 660), (600, 660), (620, 680)]);
        assert!(three.iter().all(|&(_, n)| n == 3));
        let mut used: Vec<usize> = three.iter().map(|&(c, _)| c).collect();
        used.sort_unstable();
        assert_eq!(used, [0, 1, 2]);
        assert!(columns(&[]).is_empty());
    }

    #[test]
    fn a_month_cell_shows_three_lines_then_a_count() {
        assert_eq!(month_plan(0, 4), (0, 0));
        assert_eq!(month_plan(3, 4), (3, 0));
        assert_eq!(month_plan(5, 4), (3, 2));
        // Not enough room: one line keeps the "+n".
        assert_eq!(month_plan(3, 2), (1, 2));
        assert_eq!(month_plan(1, 0), (0, 1));
    }

    #[test]
    fn scrolling_stays_inside_the_grid() {
        assert_eq!(clamp_scroll(-10.0, 1152.0, 500.0), 0.0);
        assert_eq!(clamp_scroll(900.0, 1152.0, 500.0), 652.0);
        assert_eq!(clamp_scroll(100.0, 300.0, 500.0), 0.0);
    }

    #[test]
    fn rects_intersect_only_when_they_overlap() {
        let a = Rect::new(0.0, 0.0, 10.0, 10.0);
        assert_eq!(intersect(a, Rect::new(5.0, 5.0, 10.0, 10.0)), Some(Rect::new(5.0, 5.0, 5.0, 5.0)));
        assert_eq!(intersect(a, Rect::new(10.0, 0.0, 5.0, 5.0)), None);
    }

    #[test]
    fn arrows_move_by_the_period() {
        assert_eq!(shift((2026, 11, 16), Mode::Day, -1), (2026, 11, 15));
        assert_eq!(shift((2026, 11, 16), Mode::Week, 1), (2026, 11, 23));
        assert_eq!(shift((2026, 1, 31), Mode::Month, 1), (2026, 2, 28));
        assert_eq!(shift((2026, 1, 15), Mode::Month, -1), (2025, 12, 15));
    }

    #[test]
    fn titles_are_in_french() {
        assert_eq!(period_title(Mode::Month, (2026, 11, 16), false), "Novembre 2026");
        assert_eq!(period_title(Mode::Week, (2026, 11, 18), false), "16 – 22 novembre 2026");
        assert_eq!(period_title(Mode::Week, (2026, 11, 1), false), "26 octobre – 1 novembre 2026");
        assert_eq!(period_title(Mode::Week, (2026, 12, 30), false), "28 décembre 2026 – 3 janvier 2027");
        assert_eq!(period_title(Mode::Day, (2026, 11, 16), false), "lun. 16 novembre 2026");
        assert_eq!(period_title(Mode::Month, (2026, 11, 16), true), "November 2026");
    }

    #[test]
    fn agenda_items_keep_their_origin() {
        let local = Item { title: "a".into(), date: "2026-11-16".into(), time: Some("10:00".into()), local: Some("x".into()), end: None, place: String::new(), project: false, color: None, lesson: None, hidden: false };
        let ics = Item { local: None, ..local.clone() };
        assert_ne!(local, ics);
        assert_eq!(local.span(), Some((600, 660)));
    }
}
