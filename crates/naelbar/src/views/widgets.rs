//! Calendar, focus timer, weather, system stats on page one; countdowns,
//! elapsed time and a calculator on page two. The mouse wheel and the dots at
//! the bottom right switch pages, and the page is kept for the session.


use super::View;
use crate::state::{AppState, Command, FocusPhase, FocusRun};
use crate::widgets::page2::{self, DOTS_H, PAGES};
use crate::ui::theme::*;
use crate::ui::{Align, Color, Icon, Rect, TextStyle, Ui, id};

#[derive(Default)]
pub struct WidgetsView {
    /// The usage panel replaces the whole tab while open.
    detail: bool,
    /// Page on screen, kept while the island runs.
    page: usize,
    /// Wheel movement not yet turned into a page change.
    wheel: f32,
}

impl View for WidgetsView {
    fn name(&self) -> &'static str {
        "widgets"
    }

    fn icon(&self) -> Icon {
        Icon::Grid
    }

    fn wants_stats(&self) -> bool {
        true
    }

    fn build(&mut self, ui: &mut Ui, st: &AppState, area: Rect) {
        if self.detail {
            self.usage_panel(ui, st, area);
            return;
        }
        if ui.hovered(area) && ui.input.wheel != 0.0 {
            self.wheel += ui.input.wheel;
            if self.wheel.abs() >= 1.0 {
                // Wheel down goes to the next page.
                let next = if self.wheel < 0.0 { self.page + 1 } else { self.page.saturating_sub(1) };
                self.page = next.min(PAGES - 1);
                self.wheel = 0.0;
            }
        }
        if let Some(page) = page2::dots(ui, area, self.page) {
            self.page = page;
        }
        if self.page == 1 {
            page2::build(ui, area);
            return;
        }
        if self.page == 2 {
            // Page three: connected device batteries, above the page dots.
            let r = Rect::new(area.x, area.y, area.w, (area.h - DOTS_H - 4.0).max(0.0));
            crate::widgets::batteries::draw(ui, st, r, true);
            return;
        }
        let mut rest = area;
        let focus_r = rest.take_top(88.0, 8.0);
        focus(ui, st, focus_r);
        // The strip under the weather and stats tiles holds the page dots.
        let cols = rest.take_top((rest.h - DOTS_H).max(0.0), 0.0).columns(2, 8.0);
        weather(ui, st, cols[0]);
        if stats(ui, st, cols[1]) {
            self.detail = true;
            ui.out.push(Command::StatsDetail(true));
        }
    }
}

impl WidgetsView {
    fn usage_panel(&mut self, ui: &mut Ui, st: &AppState, area: Rect) {
        let mut rest = area;
        let mut head = rest.take_top(28.0, 8.0);
        if ui.icon_button(id("stats.back", 0), head.take_left(28.0, 6.0), Icon::Back, 17.0, TEXT_DIM).clicked {
            self.detail = false;
            ui.out.push(Command::StatsDetail(false));
        }
        ui.text(head, crate::ui::tr("Utilisation du PC", "PC usage"), TextStyle::new(13.5, 600, TEXT));

        let tiles = rest.take_top(TILE_H, 8.0).columns(4, 6.0);
        let percent = |v: Option<f32>| v.map_or("–".to_string(), |p| format!("{p:.0} %"));
        let (cpu_mhz, cpu_temp) = st.stats.map_or((None, None), |s| (s.cpu_mhz, s.cpu_temp_c));
        let cpu_lines = [format_clock(cpu_mhz), format_temp(cpu_temp)];
        summary(ui, tiles[0], "CPU", &percent(st.stats.map(|s| s.cpu_percent)), &cpu_lines);
        let (gpu_mhz, gpu_temp) = st.stats.map_or((None, None), |s| (s.gpu_mhz, s.gpu_temp_c));
        // The GPU reports no temperature while it sleeps (Optimus).
        let gpu_lines = if st.stats.is_some() && gpu_mhz.is_none() && gpu_temp.is_none() {
            [crate::ui::tr("veille", "asleep").to_string(), String::new()]
        } else {
            [format_clock(gpu_mhz), format_temp(gpu_temp)]
        };
        summary(ui, tiles[1], "GPU", &percent(st.stats.and_then(|s| s.gpu_percent)), &gpu_lines);
        let unit = crate::ui::tr("Go", "GB");
        let ram_label = st.stats.map_or("RAM".to_string(), |s| format!("RAM · {:.0} {unit}", gigabytes(s.ram_total_bytes)));
        let ram = st.stats.map_or("–".to_string(), |s| format!("{:.1} {unit}", gigabytes(s.ram_used_bytes)));
        summary(ui, tiles[2], &ram_label, &ram, &[]);
        network(ui, tiles[3], st);

        let cols = rest.columns(2, 8.0);
        let mut by_cpu: Vec<_> = st.processes.iter().collect();
        by_cpu.sort_by(|a, b| b.cpu_percent.total_cmp(&a.cpu_percent));
        let cpu_rows: Vec<(&str, String)> = by_cpu.iter().take(LIST_ROWS).map(|p| (p.name.as_str(), format!("{:.1} %", p.cpu_percent))).collect();
        ranking(ui, cols[0], crate::ui::tr("Processeur", "Processor"), &cpu_rows);
        let mut by_ram: Vec<_> = st.processes.iter().collect();
        by_ram.sort_by(|a, b| b.ram_bytes.cmp(&a.ram_bytes));
        let ram_rows: Vec<(&str, String)> = by_ram.iter().take(LIST_ROWS).map(|p| (p.name.as_str(), format_bytes(p.ram_bytes))).collect();
        ranking(ui, cols[1], crate::ui::tr("Mémoire", "Memory"), &ram_rows);
    }
}

/// Summary tile height: label, value and two small lines.
const TILE_H: f32 = 70.0;
const LIST_ROWS: usize = 4;
const LIST_ROW_H: f32 = 15.0;

fn format_clock(mhz: Option<u32>) -> String {
    mhz.map_or("–".to_string(), |m| format!("{:.1} GHz", m as f32 / 1000.0))
}

fn format_temp(c: Option<f32>) -> String {
    c.map_or("–".to_string(), |t| format!("{t:.0} °C"))
}

fn gigabytes(b: u64) -> f64 {
    b as f64 / 1_073_741_824.0
}

fn format_bytes(b: u64) -> String {
    let gb = gigabytes(b);
    if gb >= 1.0 {
        format!("{gb:.1} {}", crate::ui::tr("Go", "GB"))
    } else {
        format!("{:.0} {}", gb * 1024.0, crate::ui::tr("Mo", "MB"))
    }
}

fn summary(ui: &mut Ui, r: Rect, label: &str, value: &str, small: &[String]) {
    ui.fill(r, R_TILE, TILE);
    let mut rest = r.inset_xy(9.0, 7.0);
    ui.text(rest.take_top(15.0, 2.0), label, TextStyle::new(10.5, 500, TEXT_FAINT));
    ui.text(rest.take_top(16.0, 1.0), value, TextStyle::new(12.5, 600, TEXT));
    for line in small {
        ui.text(rest.take_top(12.0, 0.0), line, TextStyle::new(10.0, 500, TEXT_DIM));
    }
}

fn network(ui: &mut Ui, r: Rect, st: &AppState) {
    ui.fill(r, R_TILE, TILE);
    let mut rest = r.inset_xy(9.0, 6.0);
    ui.text(rest.take_top(15.0, 2.0), crate::ui::tr("Réseau", "Network"), TextStyle::new(10.5, 500, TEXT_FAINT));
    let Some(s) = &st.stats else { return };
    let small = TextStyle::new(10.0, 500, TEXT_DIM);
    ui.text(rest.take_top(12.0, 0.0), &format!("↓ {}", format_rate(s.net_down_bps)), small);
    ui.text(rest.take_top(12.0, 0.0), &format!("↑ {}", format_rate(s.net_up_bps)), small);
}

fn ranking(ui: &mut Ui, r: Rect, title: &str, rows: &[(&str, String)]) {
    ui.fill(r, R_TILE, TILE);
    let mut rest = r.inset_xy(10.0, 7.0);
    ui.text(rest.take_top(15.0, 2.0), title, TextStyle::new(11.0, 600, TEXT_DIM));
    if rows.is_empty() {
        ui.text(rest, crate::ui::tr("Mesure…", "Reading…"), TextStyle::new(11.5, 400, TEXT_FAINT).aligned(Align::Center));
        return;
    }
    for (name, value) in rows {
        let mut row = rest.take_top(LIST_ROW_H, 0.0);
        ui.text(row.take_right(50.0, 4.0), value, TextStyle::new(11.0, 500, TEXT).aligned(Align::End));
        ui.text(row, name, TextStyle::new(11.0, 400, TEXT_DIM));
    }
}

fn focus(ui: &mut Ui, st: &AppState, r: Rect) {
    ui.fill(r, R_TILE, TILE);
    let f = &st.focus;
    let remaining = f.remaining(ui.now_ms);
    let shown = remaining.div_ceil(1000);
    let inner = r.inset_xy(14.0, 10.0);
    let mut rest = inner;
    let mut head = rest.take_top(20.0, 0.0);
    ui.icon(Icon::Timer, head.take_left(18.0, 6.0).centered(16.0, 16.0), accent());
    let label = match f.phase {
        FocusPhase::Work => "Focus",
        FocusPhase::Break => crate::ui::tr("Pause", "Break"),
    };
    ui.text(head, label, TextStyle::new(12.5, 500, TEXT_DIM));

    let time = format!("{:02}:{:02}", shown / 60, shown % 60);
    let mut row = rest;
    let buttons = row.take_right(76.0, 4.0);
    ui.text(row, &time, TextStyle::new(26.0, 600, TEXT));

    let cells = buttons.columns(2, 4.0);
    let running = f.is_running();
    let icon = if running { Icon::Pause } else { Icon::Play };
    let start = cells[0].centered(38.0, 38.0);
    ui.fill(start, 19.0, accent_soft());
    if ui.icon_button(id("focus.toggle", 0), start, icon, 18.0, accent()).clicked {
        ui.out.push(Command::FocusToggle);
    }
    if f.run != FocusRun::Idle || f.phase != FocusPhase::Work {
        if ui.icon_button(id("focus.reset", 0), cells[1].centered(38.0, 38.0), Icon::Close, 16.0, TEXT_DIM).clicked {
            ui.out.push(Command::FocusReset);
        }
    }

    if running {
        ui.redraw_at(ui.now_ms + (remaining.saturating_sub(1) % 1000) + 1);
    }
}

/// Index of the Widgets section in the settings window.
const SETTINGS_WIDGETS: usize = 4;

fn weather(ui: &mut Ui, st: &AppState, r: Rect) {
    let resp = ui.interact(id("weather.tile", 0), r);
    ui.fill(r, R_TILE, if resp.hovered { TILE_HOVER } else { TILE });
    if resp.clicked {
        ui.out.push(Command::OpenSettings(SETTINGS_WIDGETS));
    }
    let inner = r.inset(12.0);
    let Some(w) = &st.weather else {
        let mut rest = r.inset(10.0);
        let mut top = rest.take_top(26.0, 4.0);
        ui.icon(Icon::Cloud, top.take_left(18.0, 4.0).centered(18.0, 18.0), TEXT_DIM);
        ui.text(top, crate::ui::tr("Météo", "Weather"), TextStyle::new(13.0, 600, TEXT));
        let hint = if st.weather_configured { crate::ui::tr("Chargement…", "Loading…") } else { crate::ui::tr("Clique pour choisir ta ville", "Click to choose your city") };
        ui.text(rest, hint, TextStyle::new(11.5, 400, TEXT_DIM).wrapped().top());
        return;
    };
    let mut rest = inner;
    let mut top = rest.take_top(34.0, 2.0);
    ui.icon(Icon::Cloud, top.take_left(30.0, 6.0).centered(28.0, 28.0), accent());
    ui.text(top, &format!("{:.0}°", w.temperature_c), TextStyle::new(24.0, 600, TEXT));
    ui.text(rest.take_top(18.0, 0.0), &w.condition, TextStyle::new(12.0, 400, TEXT_DIM));
    ui.text(rest.take_top(16.0, 0.0), &format!("{:.0}° / {:.0}°", w.min_c, w.max_c), TextStyle::new(11.0, 400, TEXT_FAINT));
}

fn stat_bar(ui: &mut Ui, r: Rect, label: &str, frac: Option<f32>, c: Color) {
    let mut row = r;
    ui.text(row.take_left(30.0, 4.0), label, TextStyle::new(10.5, 500, TEXT_FAINT));
    let bar = row.centered(row.w, 4.0);
    ui.fill(bar, 2.0, TILE_HOVER);
    if let Some(f) = frac {
        let w = (bar.w * f.clamp(0.0, 1.0)).max(4.0);
        ui.fill(Rect::new(bar.x, bar.y, w, bar.h), 2.0, c);
    }
}

/// `bps` counts bits, the label counts bytes.
fn format_rate(bps: u64) -> String {
    let b = bps as f32 / 8.0;
    if b >= 1_000_000.0 {
        format!("{:.1} {}", b / 1_000_000.0, crate::ui::tr("Mo/s", "MB/s"))
    } else {
        format!("{:.0} {}", b / 1_000.0, crate::ui::tr("ko/s", "kB/s"))
    }
}

/// Returns true when the tile was clicked.
fn stats(ui: &mut Ui, st: &AppState, r: Rect) -> bool {
    let resp = ui.interact(id("stats.tile", 0), r);
    ui.fill(r, R_TILE, if resp.hovered { TILE_HOVER } else { TILE });
    // Shorter than it was, to leave room for the page dots below.
    let inner = r.inset_xy(12.0, 8.0);
    let Some(s) = &st.stats else {
        ui.text(inner, crate::ui::tr("Mesure…", "Reading…"), TextStyle::new(12.0, 400, TEXT_FAINT).aligned(Align::Center));
        return resp.clicked;
    };
    let mut rest = inner;
    stat_bar(ui, rest.take_top(15.0, 2.0), "CPU", Some(s.cpu_percent / 100.0), accent());
    stat_bar(ui, rest.take_top(15.0, 2.0), "GPU", s.gpu_percent.map(|g| g / 100.0), accent());
    stat_bar(ui, rest.take_top(15.0, 3.0), "RAM", Some(s.ram_percent / 100.0), accent());
    let small = TextStyle::new(10.5, 400, TEXT_DIM);
    ui.text(rest.take_top(14.0, 0.0), &format!("↓ {}", format_rate(s.net_down_bps)), small);
    ui.text(rest.take_top(14.0, 0.0), &format!("↑ {}", format_rate(s.net_up_bps)), small);
    resp.clicked
}
