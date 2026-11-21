//! Reusable controls built on the immediate-mode core.

use super::theme::*;
use super::{Align, Icon, Id, Pt, Rect, Resp, TextStyle, Ui};

impl Ui<'_> {
    pub fn icon_button(&mut self, w: Id, r: Rect, icon: Icon, size: f32, c: super::Color) -> Resp {
        let resp = self.interact(w, r);
        if resp.held {
            self.fill(r, r.h.min(r.w) * 0.5, TILE_HOVER);
        } else if resp.hovered {
            self.fill(r, r.h.min(r.w) * 0.5, TILE);
        }
        self.icon(icon, r.centered(size, size), c);
        resp
    }

    /// Horizontal slider. Returns the new value while it is being dragged.
    pub fn slider(&mut self, w: Id, r: Rect, value: f32) -> Option<f32> {
        self.slider_impl(w, r, value, false)
    }

    /// Same slider with its knob always visible.
    pub fn slider_with_knob(&mut self, w: Id, r: Rect, value: f32) -> Option<f32> {
        self.slider_impl(w, r, value, true)
    }

    fn slider_impl(&mut self, w: Id, r: Rect, value: f32, always_knob: bool) -> Option<f32> {
        let track = r.centered(r.w, 6.0);
        let was_active = self.widgets.active == Some(w);
        let resp = self.interact(w, r);
        let active = self.widgets.active == Some(w);
        let mut result = None;
        let mut shown = value.clamp(0.0, 1.0);
        if active {
            if let Some(m) = self.input.mouse {
                let v = ((m.x - track.x) / track.w.max(1.0)).clamp(0.0, 1.0);
                if !was_active || (v - self.widgets.drag_value).abs() > 0.002 {
                    self.widgets.drag_value = v;
                    result = Some(v);
                }
            }
            shown = self.widgets.drag_value;
        }
        self.fill(track, 3.0, TILE_HOVER);
        let fw = (track.w * shown).max(6.0);
        self.fill(Rect::new(track.x, track.y, fw, track.h), 3.0, accent());
        if always_knob || resp.hovered || active {
            let c = Pt { x: track.x + track.w * shown, y: track.center().y };
            self.fill(Rect::new(c.x - 7.0, c.y - 7.0, 14.0, 14.0), 7.0, TEXT);
        }
        result
    }

    /// Icon and label tile with an on/off state.
    pub fn toggle_tile(&mut self, w: Id, r: Rect, icon: Icon, label: &str, on: bool) -> Resp {
        let resp = self.interact(w, r);
        let bg = if on { accent_soft() } else if resp.hovered { TILE_HOVER } else { TILE };
        self.fill(r, R_TILE, bg);
        if on && resp.hovered {
            self.stroke(r.inset(0.5), R_TILE, 1.0, accent());
        }
        let fg = if on { accent() } else { TEXT_DIM };
        self.icon(icon, Rect::new(r.x + r.w * 0.5 - 11.0, r.y + 12.0, 22.0, 22.0), fg);
        let t = TextStyle::new(11.5, 500, if on { TEXT } else { TEXT_DIM }).aligned(Align::Center);
        self.text(Rect::new(r.x + 4.0, r.bottom() - 28.0, r.w - 8.0, 20.0), label, t);
        resp
    }

    /// Row of mutually exclusive labels. Returns the index that was clicked.
    pub fn segmented(&mut self, w: Id, r: Rect, labels: &[&str], selected: usize) -> Option<usize> {
        self.fill(r, r.h * 0.5, TILE);
        let mut out = None;
        for (i, (cell, label)) in r.columns(labels.len(), 0.0).iter().zip(labels).enumerate() {
            let resp = self.interact(super::id("seg", w as usize ^ i), *cell);
            if resp.clicked {
                out = Some(i);
            }
            let on = i == selected;
            if on {
                self.fill(cell.inset(3.0), (r.h - 6.0) * 0.5, accent_soft());
            } else if resp.hovered {
                self.fill(cell.inset(3.0), (r.h - 6.0) * 0.5, TILE_HOVER);
            }
            let t = TextStyle::new(12.0, 500, if on { accent() } else { TEXT_DIM }).aligned(Align::Center);
            self.text(*cell, label, t);
        }
        out
    }

    /// Bar whose fill is snapped to device pixels. Schedules the next redraw
    /// for when the fill would gain a pixel, never earlier.
    pub fn progress(&mut self, r: Rect, pos_ms: u64, dur_ms: u64, playing: bool, color: super::Color) {
        self.fill(r, r.h * 0.5, TILE_HOVER);
        if dur_ms == 0 {
            return;
        }
        let width_px = (r.w * self.scale).round().max(1.0);
        let filled = ((pos_ms as f64 / dur_ms as f64).clamp(0.0, 1.0) * width_px as f64).floor() as f32;
        if filled > 0.0 {
            let w = (filled / self.scale).max(r.h);
            self.fill(Rect::new(r.x, r.y, w, r.h), r.h * 0.5, color);
        }
        if playing && filled < width_px {
            let next_pos = ((filled as f64 + 1.0) / width_px as f64 * dur_ms as f64).ceil() as u64;
            let wait = next_pos.saturating_sub(pos_ms).max(1);
            self.redraw_at(self.now_ms + wait);
        }
    }

    /// Time label that schedules a redraw for the next change of its second.
    pub fn redraw_next_second(&mut self, pos_ms: u64) {
        let into = pos_ms % 1000;
        self.redraw_at(self.now_ms + (1000 - into).max(1));
    }
}
