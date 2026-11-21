//! Second page of the widgets tab: countdowns, elapsed time, calculator.

use windows::Win32::System::SystemInformation::GetLocalTime;

use super::civil;
use super::{countdown, elapsed, store};
use crate::state::Command;
use crate::ui::theme::*;
use crate::ui::{Align, Icon, Rect, TextStyle, Ui, id, is_english, tr};

/// Pages of the tab.
pub const PAGES: usize = 3;
/// Index of the Widgets section in the settings window.
const SETTINGS_WIDGETS: usize = 4;
/// Room kept at the bottom right for the page dots.
pub const DOTS_H: f32 = 14.0;

const LEFT_W: f32 = 190.0;
const ROW_H: f32 = 22.0;
const CROSS: f32 = 16.0;
const VALUE_W: f32 = 54.0;

/// The wall clock as naive local milliseconds.
pub fn naive_now() -> i64 {
    let t = unsafe { GetLocalTime() };
    civil::to_ms(t.wYear as i32, t.wMonth as u32, t.wDay as u32, t.wHour as u32, t.wMinute as u32, t.wSecond as u32) + t.wMilliseconds as i64
}

/// `text` cut to `max` characters, with an ellipsis.
fn clip(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut s: String = text.chars().take(max.saturating_sub(1)).collect();
    s.push('…');
    s
}

/// Page dots at the bottom right of `area`. Returns the page that was clicked.
pub fn dots(ui: &mut Ui, area: Rect, page: usize) -> Option<usize> {
    const CELL_W: f32 = 14.0;
    let strip = Rect::new(area.right() - CELL_W * PAGES as f32, area.bottom() - DOTS_H + 2.0, CELL_W * PAGES as f32, DOTS_H - 2.0);
    let mut clicked = None;
    for (i, cell) in strip.columns(PAGES, 0.0).into_iter().enumerate() {
        let resp = ui.interact(id("widgets.dot", i), cell);
        if resp.clicked {
            clicked = Some(i);
        }
        let (size, color) = if i == page { (5.0, accent()) } else if resp.hovered { (5.0, TEXT_DIM) } else { (4.0, TEXT_FAINT) };
        ui.fill(cell.centered(size, size), size * 0.5, color);
    }
    clicked
}

pub fn build(ui: &mut Ui, area: Rect) {
    let snap = store::snapshot();
    let mut rest = area;
    let mut left = rest.take_left(LEFT_W, 8.0);
    let countdowns = left.take_top(104.0, 8.0);
    let calc = left.take_top(54.0, 8.0);
    let now = naive_now();
    let mut next: Option<u64> = None;
    let mut soonest = |wait: Option<u64>| {
        if let Some(w) = wait {
            next = Some(next.map_or(w, |n| n.min(w)));
        }
    };
    soonest(countdown_tile(ui, countdowns, &snap.countdowns, now));
    calc_tile(ui, calc, &snap.calc);
    soonest(ics_tile(ui, left, now));
    super::ics::request();

    let bars = Rect::new(rest.x, rest.y, rest.w, (area.h - DOTS_H - 4.0).max(0.0));
    soonest(elapsed_tile(ui, bars, now));
    if let Some(wait) = next {
        ui.redraw_at(ui.now_ms + wait);
    }
}

/// Returns the wait until a displayed value changes.
fn countdown_tile(ui: &mut Ui, r: Rect, list: &[countdown::Countdown], now: i64) -> Option<u64> {
    ui.fill(r, R_TILE, TILE);
    let mut rest = r.inset_xy(10.0, 8.0);
    let mut head = rest.take_top(20.0, 2.0);
    if list.len() < countdown::MAX {
        let add = head.take_right(CROSS + 2.0, 0.0).centered(CROSS + 2.0, CROSS + 2.0);
        if ui.icon_button(id("cd.add", 0), add, Icon::Plus, 12.0, TEXT_DIM).clicked {
            ui.out.push(Command::OpenEditor("countdown".into()));
        }
    }
    ui.icon(Icon::Timer, head.take_left(16.0, 5.0).centered(14.0, 14.0), accent());
    ui.text(head, tr("Compte à rebours", "Countdown"), TextStyle::new(11.5, 600, TEXT_DIM));

    if list.is_empty() {
        ui.text(rest, tr("Appuie sur + pour ajouter une date.", "Press + to add a date."), TextStyle::new(11.5, 400, TEXT_FAINT).wrapped().top());
        return None;
    }
    let mut wait: Option<u64> = None;
    let english = is_english();
    for (index, item) in list.iter().enumerate() {
        let mut row = rest.take_top(ROW_H, 0.0);
        let hovered = ui.hovered(row);
        let cross = row.take_right(CROSS, 2.0).centered(CROSS, CROSS);
        let shown = countdown::target_ms(&item.target).map(|t| countdown::show(t, now, english));
        let value = row.take_right(VALUE_W, 4.0);
        let (text, color) = match &shown {
            Some(s) if s.done => (s.text.as_str(), TEXT_FAINT),
            Some(s) => (s.text.as_str(), TEXT),
            None => ("–", TEXT_FAINT),
        };
        ui.text(value, text, TextStyle::new(12.0, 600, color).aligned(Align::End));
        ui.text(row, &clip(&item.name, (row.w / 6.2) as usize), TextStyle::new(11.5, 400, TEXT_DIM));
        if hovered {
            if ui.icon_button(id("cd.del", index), cross, Icon::Close, 10.0, TEXT_FAINT).clicked {
                store::remove_countdown(index);
            }
        }
        if let Some(w) = shown.and_then(|s| s.wait_ms) {
            wait = Some(wait.map_or(w, |n| n.min(w)));
        }
    }
    wait
}

fn calc_tile(ui: &mut Ui, r: Rect, last: &store::LastCalc) {
    let resp = ui.interact(id("calc.tile", 0), r);
    ui.fill(r, R_TILE, if resp.hovered { TILE_HOVER } else { TILE });
    if resp.clicked {
        ui.out.push(Command::OpenEditor("calc".into()));
    }
    let mut rest = r.inset_xy(10.0, 7.0);
    let mut head = rest.take_top(15.0, 1.0);
    let title = tr("Calculatrice", "Calculator");
    if last.result.is_empty() {
        ui.text(head, title, TextStyle::new(11.5, 600, TEXT_DIM));
        ui.text(rest, tr("Calcule ou convertis", "Calculate or convert"), TextStyle::new(11.5, 400, TEXT_FAINT));
        return;
    }
    let title_w = head.take_left(78.0, 4.0);
    ui.text(title_w, title, TextStyle::new(11.5, 600, TEXT_DIM));
    ui.text(head, &clip(&last.expr, (head.w / 5.6) as usize), TextStyle::new(10.0, 400, TEXT_FAINT).aligned(Align::End));
    let size = if last.result.chars().count() > 14 { 13.0 } else { 19.0 };
    ui.text(rest, &clip(&last.result, ((rest.w / (size * 0.56)) as usize).max(4)), TextStyle::new(size, 600, TEXT));
}

/// The next event of the .ics agenda. Returns the wait until it changes.
fn ics_tile(ui: &mut Ui, r: Rect, now: i64) -> Option<u64> {
    let configured = super::ics::is_configured();
    let next = if configured { super::ics::next_after(now) } else { None };
    let resp = ui.interact(id("ics.tile", 0), r);
    ui.fill(r, R_TILE, if resp.hovered { TILE_HOVER } else { TILE });
    if resp.clicked {
        ui.out.push(if configured { Command::OpenAgenda } else { Command::OpenSettings(SETTINGS_WIDGETS) });
    }
    let mut rest = r.inset_xy(10.0, 5.0);
    ui.text(rest.take_top(13.0, 0.0), tr("Agenda .ics", ".ics agenda"), TextStyle::new(10.0, 600, TEXT_FAINT));
    let line = TextStyle::new(11.5, 400, TEXT_DIM);
    let Some(next) = next else {
        let hint = if configured { tr("Rien à venir", "Nothing coming up") } else { tr("Ajoute une adresse dans les réglages", "Add an address in settings") };
        ui.text(rest, hint, line);
        return None;
    };
    let today = now_key(now) == next.event.date;
    let when = match (&next.event.time, crate::agenda::parse_date(&next.event.date)) {
        (Some(t), _) if today => t.clone(),
        (None, _) if today => tr("Aujourd'hui", "Today").to_string(),
        (time, Some((y, m, d))) => {
            let day = crate::agenda::day_label(y, m, d, is_english());
            time.as_ref().map_or(day.clone(), |t| format!("{day} {t}"))
        }
        (_, None) => String::new(),
    };
    let text = format!("{when} · {}", next.event.title);
    ui.text(rest, &clip(&text, (rest.w / 6.0) as usize), TextStyle::new(11.5, 500, TEXT));
    Some(next.change_in_ms)
}

fn now_key(now: i64) -> String {
    let ((y, m, d), _) = civil::from_ms(now);
    crate::agenda::date_key(y, m, d)
}

fn elapsed_tile(ui: &mut Ui, r: Rect, now: i64) -> Option<u64> {
    ui.fill(r, R_TILE, TILE);
    let mut rest = r.inset_xy(10.0, 8.0);
    ui.text(rest.take_top(18.0, 4.0), tr("Part écoulée", "Time elapsed"), TextStyle::new(11.5, 600, TEXT_DIM));
    let spans = elapsed::all(now);
    let row_h = rest.h / spans.len() as f32;
    let mut wait: Option<u64> = None;
    for (fr, en, span) in spans {
        let mut row = rest.take_top(row_h, 0.0);
        let mut line = row.take_top(15.0, 2.0);
        let pct = span.percent(now);
        ui.text(line.take_right(36.0, 0.0), &format!("{pct} %"), TextStyle::new(10.5, 600, TEXT).aligned(Align::End));
        ui.text(line, tr(fr, en), TextStyle::new(10.5, 500, TEXT_DIM));
        let bar = Rect::new(row.x, row.y, row.w, 4.0);
        ui.fill(bar, 2.0, TILE_HOVER);
        if pct > 0 {
            ui.fill(Rect::new(bar.x, bar.y, (bar.w * pct as f32 / 100.0).max(4.0), bar.h), 2.0, accent());
        }
        if let Some(w) = span.next_percent_in(now) {
            wait = Some(wait.map_or(w, |n| n.min(w)));
        }
    }
    wait
}

#[cfg(test)]
mod tests {
    use super::clip;

    #[test]
    fn long_names_are_cut_with_an_ellipsis() {
        assert_eq!(clip("Partiels", 12), "Partiels");
        assert_eq!(clip("Vacances de Noël en famille", 10), "Vacances …");
        assert_eq!(clip("é", 0), "…");
    }
}
