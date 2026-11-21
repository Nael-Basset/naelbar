//! Month calendar, the events of the selected day, and post-its.

use windows::Win32::System::SystemInformation::GetLocalTime;

use super::View;
use crate::agenda;
use crate::state::{AppState, Command};
use crate::ui::theme::*;
use crate::ui::{Align, Icon, Rect, TextStyle, Ui, id, is_english, tr};

const CALENDAR_W: f32 = 170.0;
const EVENTS_H: f32 = 104.0;
const HEADER_H: f32 = 18.0;
const ROW_H: f32 = 20.0;
/// Room kept at the right of a list for its scroll position.
const GUTTER: f32 = 5.0;
const CROSS: f32 = 18.0;
const DAY_MS: u64 = 86_400_000;

type Day = (i32, u32, u32);

#[derive(Default)]
pub struct AgendaView {
    /// Month on screen, the current one until an arrow is used.
    month: Option<(i32, u32)>,
    /// Selected day, today until one is clicked.
    selected: Option<Day>,
    first_event: usize,
    first_note: usize,
}

impl View for AgendaView {
    fn name(&self) -> &'static str {
        "agenda"
    }

    fn icon(&self) -> Icon {
        Icon::Calendar
    }

    fn build(&mut self, ui: &mut Ui, st: &AppState, area: Rect) {
        let now = unsafe { GetLocalTime() };
        let today = (now.wYear as i32, now.wMonth as u32, now.wDay as u32);
        // Today's highlight moves at midnight. Whole seconds keep the instant
        // stable from one frame to the next.
        let into_day = now.wHour as u64 * 3_600_000 + now.wMinute as u64 * 60_000 + now.wSecond as u64 * 1000 + now.wMilliseconds as u64;
        ui.redraw_at((ui.now_ms + DAY_MS - into_day).div_ceil(1000) * 1000);
        // Refreshes the read-only .ics events, at most once per half hour.
        crate::widgets::ics::request();

        let mut rest = area;
        if st.agenda_link == nb_core::ClaudeLink::NeedsLogin {
            let r = rest.take_top(22.0, 6.0);
            let resp = ui.interact(id("agenda.login", 0), r);
            ui.fill(r, R_SMALL, if resp.hovered { TILE_HOVER } else { TILE });
            ui.text(r, tr("Synchroniser avec ton téléphone", "Sync with your phone"), TextStyle::new(11.0, 500, TEXT_DIM).aligned(Align::Center));
            if resp.clicked {
                ui.out.push(Command::AgendaLogin);
            }
        }
        let calendar = rest.take_left(CALENDAR_W, 10.0);
        self.calendar(ui, st, calendar, today);

        let (year, month, day) = self.selected.unwrap_or(today);
        let events = rest.take_top(EVENTS_H, 4.0);
        self.events(ui, st, events, (year, month, day));
        ui.fill(rest.take_top(1.0, 3.0), 0.0, HAIRLINE);
        self.notes(ui, st, rest);
    }
}

impl AgendaView {
    fn calendar(&mut self, ui: &mut Ui, st: &AppState, r: Rect, today: Day) {
        ui.fill(r, R_TILE, TILE);
        let (year, month) = self.month.unwrap_or((today.0, today.1));
        let mut rest = r.inset(10.0);

        let mut header = rest.take_top(22.0, 2.0);
        let previous = header.take_left(20.0, 2.0).centered(20.0, 20.0);
        let next = header.take_right(20.0, 2.0).centered(20.0, 20.0);
        if ui.icon_button(id("agenda.previous", 0), previous, Icon::Back, 13.0, TEXT_DIM).clicked {
            self.month = Some(agenda::shift_month(year, month, -1));
        }
        if ui.icon_button(id("agenda.next", 0), next, Icon::Forward, 13.0, TEXT_DIM).clicked {
            self.month = Some(agenda::shift_month(year, month, 1));
        }
        let name = agenda::month_name(month, is_english());
        let title = if year == today.0 { name.to_string() } else { format!("{name} {year}") };
        ui.text(header, &title, TextStyle::new(12.5, 600, TEXT).aligned(Align::Center));

        let cell_w = rest.w / 7.0;
        let weekdays = rest.take_top(18.0, 0.0);
        let letters = if is_english() { ["M", "T", "W", "T", "F", "S", "S"] } else { ["L", "M", "M", "J", "V", "S", "D"] };
        for (i, letter) in letters.iter().enumerate() {
            let c = Rect::new(weekdays.x + i as f32 * cell_w, weekdays.y, cell_w, weekdays.h);
            ui.text(c, letter, TextStyle::new(10.5, 500, TEXT_FAINT).aligned(Align::Center));
        }

        let mut with_events = agenda::days_with_events(&st.agenda.events, year, month);
        with_events.extend(crate::widgets::ics::days_of_month(year, month));
        with_events.sort_unstable();
        with_events.dedup();
        let row_h = rest.h / 6.0;
        let selected = self.selected.unwrap_or(today);
        for (index, cell) in agenda::month_grid(year, month).iter().enumerate() {
            let Some(day) = *cell else { continue };
            let c = Rect::new(rest.x + (index % 7) as f32 * cell_w, rest.y + (index / 7) as f32 * row_h, cell_w, row_h);
            let this = (year, month, day);
            let is_today = this == today;
            let is_selected = this == selected;

            let resp = ui.interact(id("agenda.day", day as usize), c);
            if resp.clicked {
                self.selected = Some(this);
                self.first_event = 0;
            }

            let disc = c.centered(21.0, 21.0);
            if is_today {
                ui.fill(disc, 10.5, accent());
            } else if resp.hovered {
                ui.fill(disc, 10.5, TILE_HOVER);
            }
            if is_selected {
                // Around the disc when it is today, or it would not show.
                let ring = if is_today { c.centered(24.0, 24.0) } else { disc };
                ui.stroke(ring.inset(0.5), ring.w * 0.5, 1.0, TEXT);
            }

            let color = if is_today { SURFACE } else if is_selected { TEXT } else { TEXT_DIM };
            let weight = if is_today || is_selected { 600 } else { 400 };
            ui.text(c, &day.to_string(), TextStyle::new(11.5, weight, color).aligned(Align::Center));
            if with_events.binary_search(&day).is_ok() {
                let center = c.center();
                ui.fill(Rect::new(center.x - 1.5, center.y + 6.5, 3.0, 3.0), 1.5, if is_today { SURFACE } else { accent() });
            }
        }
    }

    fn events(&mut self, ui: &mut Ui, st: &AppState, r: Rect, (year, month, day): Day) {
        let mut rest = r;
        let label = agenda::day_label(year, month, day, is_english());
        let date = agenda::date_key(year, month, day);
        if header(ui, "agenda.add.event", rest.take_top(HEADER_H, 2.0), &label) {
            ui.out.push(Command::AgendaNewEvent(date.clone()));
        }

        let list = agenda::events_of_day(&st.agenda.events, &date);
        let ics = crate::widgets::ics::of_day(&date);
        let total = list.len() + ics.len();
        if total == 0 {
            self.first_event = 0;
            empty_line(ui, rest, tr("Rien ce jour-là", "Nothing that day"));
            return;
        }
        let visible = visible_rows(rest);
        scroll(ui, rest, &mut self.first_event, total, visible);
        for (slot, index) in (self.first_event..total).take(visible).enumerate() {
            let mut row = row_rect(rest, slot);
            let Some(event) = list.get(index) else {
                // Read-only event from the .ics calendar, after the local ones.
                let item = &ics[index - list.len()];
                // Lessons show their kind and room; RN slots are free project hours.
                let tag = if item.is_project() {
                    tr("projet", "project").to_string()
                } else {
                    [item.kind.as_deref().unwrap_or("ics"), item.place.as_str()].iter().filter(|s| !s.is_empty()).copied().collect::<Vec<_>>().join(" · ")
                };
                ui.text(row.take_right(78.0, 4.0), &tag, TextStyle::new(9.5, 500, TEXT_FAINT).aligned(Align::End));
                if let Some(color) = crate::widgets::ics::ui_color(item) {
                    // The subject's colour, chosen in the settings.
                    let bar = row.take_left(3.0, 5.0);
                    ui.fill(Rect::new(bar.x, bar.y + 4.0, 3.0, (bar.h - 8.0).max(4.0)), 1.5, color);
                }
                if let Some(time) = &item.time {
                    ui.text(row.take_left(36.0, 4.0), time, TextStyle::new(11.5, 500, if item.is_project() { TEXT_FAINT } else { TEXT_DIM }));
                }
                ui.text(row, &item.title, TextStyle::new(12.0, 400, if item.is_project() { TEXT_FAINT } else { TEXT_DIM }));
                continue;
            };
            let cross = row.take_right(CROSS, 4.0).centered(CROSS, CROSS);
            if ui.icon_button(id("agenda.event.delete", index), cross, Icon::Close, 10.0, TEXT_FAINT).clicked {
                ui.out.push(Command::AgendaDeleteEvent(event.id.clone()));
            }
            if let Some(time) = &event.time {
                ui.text(row.take_left(36.0, 4.0), time, TextStyle::new(11.5, 500, TEXT_DIM));
            }
            ui.text(row, &event.title, TextStyle::new(12.0, 400, TEXT));
        }
        thumb(ui, rest, self.first_event, total, visible);
    }

    fn notes(&mut self, ui: &mut Ui, st: &AppState, r: Rect) {
        let mut rest = r;
        if header(ui, "agenda.add.note", rest.take_top(HEADER_H, 2.0), "Post-it") {
            ui.out.push(Command::AgendaNewNote);
        }

        // Bottom right: opens the large agenda window.
        let corner = Rect::new(r.right() - CROSS, r.bottom() - CROSS, CROSS, CROSS);
        if ui.icon_button(id("agenda.expand", 0), corner, Icon::Expand, 12.0, TEXT_DIM).clicked {
            ui.out.push(Command::OpenAgendaWindow);
        }

        let list = agenda::visible_notes(&st.agenda.notes);
        if list.is_empty() {
            self.first_note = 0;
            empty_line(ui, rest, tr("Aucun post-it", "No notes"));
            return;
        }
        let visible = visible_rows(rest);
        scroll(ui, rest, &mut self.first_note, list.len(), visible);
        for (slot, (index, note)) in list.iter().enumerate().skip(self.first_note).take(visible).enumerate() {
            let mut row = row_rect(rest, slot);
            if row.bottom() > corner.y {
                row.w -= CROSS + 2.0;
            }
            let tick = row.take_left(CROSS, 4.0).centered(CROSS, CROSS);
            let resp = ui.interact(id("agenda.note.tick", index), tick);
            let border = if resp.hovered { TEXT_DIM } else { TEXT_FAINT };
            ui.stroke(tick.centered(13.0, 13.0).inset(0.5), 3.5, 1.0, border);
            if note.done {
                ui.icon(Icon::Check, tick.centered(11.0, 11.0), accent());
            }
            if resp.clicked {
                ui.out.push(Command::AgendaToggleNote(note.id.clone()));
            }
            let cross = row.take_right(CROSS, 4.0).centered(CROSS, CROSS);
            if ui.icon_button(id("agenda.note.delete", index), cross, Icon::Close, 10.0, TEXT_FAINT).clicked {
                ui.out.push(Command::AgendaDeleteNote(note.id.clone()));
            }
            ui.text(row, &note.text, TextStyle::new(12.0, 400, if note.done { TEXT_FAINT } else { TEXT }));
        }
        thumb(ui, rest, self.first_note, list.len(), visible);
    }
}

/// Section title with its "+" button. Returns true when the button was clicked.
fn header(ui: &mut Ui, name: &str, r: Rect, title: &str) -> bool {
    let mut r = r;
    let add = r.take_right(CROSS, 4.0).centered(CROSS, CROSS);
    ui.text(r, title, TextStyle::new(12.5, 600, TEXT));
    ui.icon_button(id(name, 0), add, Icon::Plus, 12.0, TEXT_DIM).clicked
}

fn empty_line(ui: &mut Ui, list: Rect, text: &str) {
    ui.text(Rect::new(list.x, list.y, list.w, ROW_H), text, TextStyle::new(11.5, 400, TEXT_FAINT));
}

fn visible_rows(list: Rect) -> usize {
    (list.h / ROW_H).floor().max(1.0) as usize
}

fn row_rect(list: Rect, slot: usize) -> Rect {
    Rect::new(list.x, list.y + slot as f32 * ROW_H, list.w - GUTTER, ROW_H)
}

fn scroll(ui: &Ui, list: Rect, first: &mut usize, total: usize, visible: usize) {
    if ui.hovered(list) && ui.input.wheel != 0.0 {
        if ui.input.wheel < 0.0 {
            *first += 1;
        } else {
            *first = first.saturating_sub(1);
        }
    }
    *first = (*first).min(total.saturating_sub(visible));
}

fn thumb(ui: &mut Ui, list: Rect, first: usize, total: usize, visible: usize) {
    if total <= visible {
        return;
    }
    let track = visible as f32 * ROW_H;
    let h = (visible as f32 / total as f32 * track).max(12.0);
    let y = list.y + first as f32 / (total - visible) as f32 * (track - h);
    ui.fill(Rect::new(list.right() - 2.0, y, 2.0, h), 1.0, TILE_HOVER);
}
