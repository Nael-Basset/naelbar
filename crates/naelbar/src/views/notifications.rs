//! Windows notifications mirrored from the system, newest first.

use nb_core::Notification;

use super::{View, empty_state};
use crate::state::{AppState, Command};
use crate::ui::theme::*;
use crate::ui::{Align, Icon, Rect, TextStyle, Ui, id};

const ROW_H: f32 = 40.0;
const ROW_GAP: f32 = 4.0;
const MINUTE: u64 = 60_000;
const HOUR: u64 = 60 * MINUTE;
const DAY: u64 = 24 * HOUR;

#[derive(Default)]
pub struct NotificationsView {
    scroll: f32,
}

/// How long ago, coarsely.
#[derive(Debug, PartialEq, Eq)]
enum Ago {
    Now,
    Minutes(u64),
    Hours(u64),
    Days(u64),
}

fn ago(now_ms: u64, then_ms: u64) -> Ago {
    let age = now_ms.saturating_sub(then_ms);
    match age {
        a if a < MINUTE => Ago::Now,
        a if a < HOUR => Ago::Minutes(a / MINUTE),
        a if a < DAY => Ago::Hours(a / HOUR),
        a => Ago::Days(a / DAY),
    }
}

fn ago_label(a: &Ago) -> String {
    match a {
        Ago::Now => crate::ui::tr("à l'instant", "just now").into(),
        Ago::Minutes(m) => format!("{m} min"),
        Ago::Hours(h) => format!("{h} h"),
        Ago::Days(d) => format!("{d} {}", crate::ui::tr("j", "d")),
    }
}

/// The first instant at which the label of a notification changes.
fn next_label_change(now_ms: u64, then_ms: u64) -> u64 {
    let age = now_ms.saturating_sub(then_ms);
    let step = if age < HOUR { MINUTE } else if age < DAY { HOUR } else { DAY };
    then_ms + (age / step + 1) * step
}

impl View for NotificationsView {
    fn name(&self) -> &'static str {
        "notifications"
    }

    fn icon(&self) -> Icon {
        Icon::Bell
    }

    fn build(&mut self, ui: &mut Ui, st: &AppState, area: Rect) {
        let mut rest = area;
        let mut head = rest.take_top(30.0, 8.0);
        let clear = head.take_right(30.0, 8.0);
        if !st.notifications.is_empty() {
            if ui.icon_button(id("notif.clear", 0), clear, Icon::Trash, 17.0, TEXT_DIM).clicked {
                ui.out.push(Command::ClearNotifications);
                self.scroll = 0.0;
            }
            let n = st.notifications.len();
            let label = format!("{n} notification{}", if n > 1 { "s" } else { "" });
            ui.text(head, &label, TextStyle::new(12.0, 500, TEXT_DIM));
        } else {
            ui.text(head, crate::ui::tr("Notifications", "Notifications"), TextStyle::new(12.0, 500, TEXT_DIM));
        }

        if st.notifications.is_empty() {
            empty_state(ui, rest, Icon::Bell, crate::ui::tr("Aucune notification", "No notifications"));
            return;
        }

        let list = rest;
        let content = st.notifications.len() as f32 * (ROW_H + ROW_GAP) - ROW_GAP;
        let max_scroll = (content - list.h).max(0.0);
        if ui.hovered(list) && ui.input.wheel != 0.0 {
            self.scroll -= ui.input.wheel * (ROW_H + ROW_GAP);
        }
        self.scroll = self.scroll.clamp(0.0, max_scroll);

        let mut next: Option<u64> = None;
        ui.push_clip(list, 0.0);
        for (i, n) in st.notifications.iter().enumerate() {
            let row = Rect::new(list.x, list.y + i as f32 * (ROW_H + ROW_GAP) - self.scroll, list.w, ROW_H);
            if row.bottom() < list.y || row.y > list.bottom() {
                continue;
            }
            next = Some(next.map_or(next_label_change(ui.now_ms, n.received_at_ms), |d| d.min(next_label_change(ui.now_ms, n.received_at_ms))));
            let can_open = !n.app_id.is_empty();
            let resp = if ui.hovered(list) && can_open { ui.interact(id("notif.row", i), row) } else { Default::default() };
            ui.fill(row, R_TILE, if resp.hovered { TILE_HOVER } else { TILE });
            draw_row(ui, row.inset(10.0), n);
            if resp.clicked {
                ui.out.push(Command::OpenNotification(n.app_id.clone()));
            }
        }
        ui.pop_clip();
        if let Some(d) = next {
            ui.redraw_at(d);
        }
    }
}

fn draw_row(ui: &mut Ui, inner: Rect, n: &Notification) {
    let time = ago_label(&ago(ui.now_ms, n.received_at_ms));
    let top = Rect::new(inner.x, inner.y, inner.w, 13.0);
    ui.text(top, &time, TextStyle::new(10.5, 400, TEXT_FAINT).aligned(Align::End));
    let name = if n.source.is_empty() { "Windows" } else { n.source.as_str() };
    ui.text(Rect::new(top.x, top.y, (top.w - 70.0).max(20.0), top.h), name, TextStyle::new(10.5, 500, TEXT_DIM));
    let text = if n.title.is_empty() { n.body.as_str() } else { n.title.as_str() };
    ui.text(Rect::new(inner.x, inner.y + 14.0, inner.w, 16.0), text, TextStyle::new(12.5, 400, TEXT));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ago_is_coarse_and_never_negative() {
        assert_eq!(ago(1_000, 5_000), Ago::Now);
        assert_eq!(ago(100 * MINUTE, 100 * MINUTE - 59_999), Ago::Now);
        assert_eq!(ago(100 * MINUTE, 100 * MINUTE - 5 * MINUTE), Ago::Minutes(5));
        assert_eq!(ago(100 * HOUR, 100 * HOUR - 3 * HOUR - 1), Ago::Hours(3));
        assert_eq!(ago(100 * DAY, 100 * DAY - 2 * DAY), Ago::Days(2));
    }

    #[test]
    fn the_label_is_redrawn_exactly_when_it_changes() {
        let then = 10 * HOUR;
        assert_eq!(next_label_change(then + 5_000, then), then + MINUTE);
        assert_eq!(next_label_change(then + 61_000, then), then + 2 * MINUTE);
        assert_eq!(next_label_change(then + 2 * HOUR, then), then + 3 * HOUR);
        assert_eq!(ago(next_label_change(then + 5_000, then), then), Ago::Minutes(1));
    }
}
