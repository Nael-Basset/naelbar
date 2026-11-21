//! Pinned apps as a grid of tiles, Brave bookmarks as chips underneath.

use super::{View, empty_state};
use crate::dock::{self, monogram};
use crate::launcher;
use crate::state::AppState;
use crate::ui::theme::*;
use crate::ui::{Align, Icon, Rect, TextStyle, Ui, id, tr};

const COLS: usize = 6;
const GAP: f32 = 6.0;
const TILE_H: f32 = 60.0;
const CHIP_H: f32 = 26.0;
/// Launcher strip: round buttons, then a gap before the grid.
const STRIP_BTN: f32 = 28.0;
const STRIP_H: f32 = 36.0;
/// A build gap longer than this means the card was closed in between.
const REOPEN_MS: u64 = 1500;

#[derive(Default)]
pub struct DockView {
    scroll: f32,
    last_ms: u64,
}

fn clip(r: Rect, area: Rect) -> Rect {
    let top = r.y.max(area.y);
    let bottom = r.bottom().min(area.bottom());
    Rect::new(r.x, top, r.w, (bottom - top).max(0.0))
}

fn short(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut t: String = s.chars().take(max.saturating_sub(1)).collect();
        t.push('…');
        t
    }
}

impl DockView {
    /// The six launcher slots in a row, aligned on the grid columns. Returns what is left.
    fn launcher_strip(&self, ui: &mut Ui, area: Rect) -> Rect {
        let slots = launcher::slots();
        let col_w = (area.w - GAP * (COLS - 1) as f32) / COLS as f32;
        for (i, slot) in slots.iter().enumerate().take(launcher::SLOT_COUNT) {
            let kind = slot.kind();
            let cx = area.x + i as f32 * (col_w + GAP) + col_w * 0.5;
            let r = Rect::new(cx - STRIP_BTN * 0.5, area.y + 2.0, STRIP_BTN, STRIP_BTN);
            let resp = ui.interact(id("dock.slot", i), r);
            ui.fill(r, STRIP_BTN * 0.5, if resp.held || resp.hovered { TILE_HOVER } else { TILE });
            let glyph = r.inset(6.5);
            ui.icon(kind.icon(), glyph, if resp.hovered { TEXT } else { TEXT_DIM });
            if resp.clicked {
                launcher::run(slot);
            }
        }
        Rect::new(area.x, area.y + STRIP_H, area.w, (area.h - STRIP_H).max(0.0))
    }
}

impl View for DockView {
    fn name(&self) -> &'static str {
        "dock"
    }

    fn icon(&self) -> Icon {
        Icon::Dock
    }

    fn build(&mut self, ui: &mut Ui, _st: &AppState, area: Rect) {
        if self.last_ms == 0 || ui.now_ms.saturating_sub(self.last_ms) > REOPEN_MS {
            dock::refresh();
        }
        self.last_ms = ui.now_ms;
        let area = self.launcher_strip(ui, area);
        let snap = dock::snapshot();
        if snap.tiles.is_empty() && snap.bookmarks.is_empty() {
            let text = if snap.loading { tr("Chargement…", "Loading…") } else { tr("Rien d'épinglé. Choisis tes apps dans les réglages.", "Nothing pinned. Pick your apps in settings.") };
            empty_state(ui, area, Icon::Dock, text);
            return;
        }

        let tile_w = (area.w - GAP * (COLS - 1) as f32) / COLS as f32;
        let rows = snap.tiles.len().div_ceil(COLS);
        let grid_h = if rows == 0 { 0.0 } else { rows as f32 * TILE_H + (rows - 1) as f32 * GAP };

        // Chip layout first, so the content height is known for scrolling.
        let mut chips: Vec<(Rect, usize)> = Vec::new();
        let (mut cx, mut cy) = (0.0f32, 0.0f32);
        for (i, b) in snap.bookmarks.iter().enumerate() {
            let w = (34.0 + short(&b.name, 16).chars().count() as f32 * 6.0).min(area.w);
            if cx > 0.0 && cx + w > area.w {
                cx = 0.0;
                cy += CHIP_H + GAP;
            }
            chips.push((Rect::new(cx, cy, w, CHIP_H), i));
            cx += w + GAP;
        }
        let chips_h = if chips.is_empty() { 0.0 } else { cy + CHIP_H };
        let chips_top = if grid_h > 0.0 { grid_h + 12.0 } else { 0.0 };
        let content = chips_top + chips_h;

        let max_scroll = (content - area.h).max(0.0);
        if ui.hovered(area) && ui.input.wheel != 0.0 {
            self.scroll -= ui.input.wheel * 40.0;
        }
        self.scroll = self.scroll.clamp(0.0, max_scroll);
        let (ox, oy) = (area.x, area.y - self.scroll);

        ui.push_clip(area, 0.0);
        for (i, t) in snap.tiles.iter().enumerate() {
            let (col, row) = (i % COLS, i / COLS);
            let r = Rect::new(ox + col as f32 * (tile_w + GAP), oy + row as f32 * (TILE_H + GAP), tile_w, TILE_H);
            if r.bottom() < area.y || r.y > area.bottom() {
                continue;
            }
            let resp = ui.interact(id("dock.app", i), clip(r, area));
            ui.fill(r, R_SMALL, if resp.held || resp.hovered { TILE_HOVER } else { TILE });
            let icon = Rect::new(r.x + (r.w - 30.0) * 0.5, r.y + 8.0, 30.0, 30.0);
            match &t.icon {
                Some(img) => ui.image_contain(icon, 6.0, img),
                None => {
                    ui.fill(icon, 8.0, TILE_HOVER);
                    ui.text(icon, &monogram(&t.name), TextStyle::new(14.0, 600, TEXT_DIM).aligned(Align::Center));
                }
            }
            ui.text(Rect::new(r.x + 2.0, r.y + 41.0, r.w - 4.0, 16.0), &short(&t.name, 9), TextStyle::new(10.5, 400, if resp.hovered { TEXT } else { TEXT_DIM }).aligned(Align::Center));
            if resp.clicked {
                dock::launch(&t.launch);
            }
        }

        for (rel, i) in &chips {
            let r = Rect::new(ox + rel.x, oy + chips_top + rel.y, rel.w, rel.h);
            if r.bottom() < area.y || r.y > area.bottom() {
                continue;
            }
            let b = &snap.bookmarks[*i];
            let resp = ui.interact(id("dock.mark", *i), clip(r, area));
            ui.fill(r, CHIP_H * 0.5, if resp.held || resp.hovered { TILE_HOVER } else { TILE });
            let dot = Rect::new(r.x + 4.0, r.y + 4.0, 18.0, 18.0);
            ui.fill(dot, 9.0, TILE_HOVER);
            ui.text(dot, &monogram(&b.name), TextStyle::new(10.5, 600, TEXT).aligned(Align::Center));
            ui.text(Rect::new(r.x + 28.0, r.y, r.w - 32.0, r.h), &short(&b.name, 16), TextStyle::new(11.0, 400, if resp.hovered { TEXT } else { TEXT_DIM }));
            if resp.clicked {
                dock::open_url(&b.url, snap.brave);
            }
        }
        ui.pop_clip();

        if max_scroll > 0.0 {
            let bar_h = (area.h / content * area.h).max(24.0);
            let bar_y = area.y + (self.scroll / max_scroll) * (area.h - bar_h);
            ui.fill(Rect::new(area.right() - 2.0, bar_y, 2.0, bar_h), 1.0, TILE_HOVER);
        }
    }
}
