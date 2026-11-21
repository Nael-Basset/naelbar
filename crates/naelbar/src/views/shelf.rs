//! Files parked on the island: drop onto it, drag back out later. New
//! screenshots arrive by themselves, and any item can be sent to a phone.

use super::{View, empty_state};
use crate::share::{self, Card, Status};
use crate::shelf_store;
use crate::state::{AppState, Command};
use crate::ui::theme::*;
use crate::ui::{Align, Color, Icon, Pt, Rect, TextStyle, Ui, id};

const TILE_W: f32 = 62.0;
const TILE_H: f32 = 72.0;
const GAP: f32 = 14.5;
const ROW_GAP: f32 = 8.0;
const PER_ROW: usize = 5;
const DRAG_THRESHOLD: f32 = 5.0;
/// Line under the tiles that describes the hovered item.
const BAR_H: f32 = 20.0;

#[derive(Default)]
pub struct ShelfView {
    /// First visible row.
    row: usize,
}

impl View for ShelfView {
    fn name(&self) -> &'static str {
        "shelf"
    }

    fn icon(&self) -> Icon {
        Icon::Tray
    }

    fn build(&mut self, ui: &mut Ui, st: &AppState, area: Rect) {
        let mut area = area;
        if let Some(card) = share::current() {
            share_card(ui, area, &card);
            return;
        }
        let head = area.take_top(20.0, 6.0);
        ui.text(head, crate::ui::tr("Étagère", "Shelf"), TextStyle::new(13.0, 600, TEXT));
        if !st.shelf.is_empty() {
            ui.text(head, &st.shelf.len().to_string(), TextStyle::new(12.0, 400, TEXT_FAINT).aligned(Align::End));
        }
        if ui.drag_hover {
            ui.stroke(area.inset(1.0), R_TILE, 1.5, accent());
        }
        if st.shelf.is_empty() {
            empty_state(ui, area, Icon::Tray, if ui.drag_hover { crate::ui::tr("Lâche pour ajouter", "Drop to add") } else { crate::ui::tr("Dépose tes fichiers ici, ressors-les en glissant", "Drop files here, drag them back out later") });
            return;
        }

        let bar = Rect::new(area.x, area.bottom() - BAR_H, area.w, BAR_H);
        let list_h = area.h - BAR_H - 4.0;
        let rows_visible = ((list_h + ROW_GAP) / (TILE_H + ROW_GAP)).floor().max(1.0) as usize;
        let total_rows = st.shelf.len().div_ceil(PER_ROW);
        if ui.hovered(area) && ui.input.wheel != 0.0 {
            if ui.input.wheel < 0.0 {
                self.row += 1;
            } else {
                self.row = self.row.saturating_sub(1);
            }
        }
        self.row = self.row.min(total_rows.saturating_sub(rows_visible));

        let press = ui.input.press_pos;
        let moved = |m: Pt| ((m.x - press.x).powi(2) + (m.y - press.y).powi(2)).sqrt() > DRAG_THRESHOLD;
        let mut hovered_item = None;

        for (i, path) in st.shelf.iter().enumerate().skip(self.row * PER_ROW).take(rows_visible * PER_ROW) {
            let slot = i - self.row * PER_ROW;
            let (col, row) = (slot % PER_ROW, slot / PER_ROW);
            let r = Rect::new(area.x + col as f32 * (TILE_W + GAP), area.y + row as f32 * (TILE_H + ROW_GAP), TILE_W, TILE_H);

            let remove = Rect::new(r.right() - 20.0, r.y + 2.0, 18.0, 18.0);
            let share = Rect::new(r.x + 2.0, r.y + 2.0, 18.0, 18.0);
            let tile_hovered = ui.hovered(r);
            let (mut removed, mut shared) = (false, false);
            if tile_hovered {
                hovered_item = Some(path);
                removed = ui.interact(id("shelf.remove", i), remove).clicked;
                shared = ui.interact(id("shelf.share", i), share).clicked;
            }

            let w = id("shelf.tile", i);
            let resp = ui.interact(w, r);
            ui.fill(r, R_SMALL, if resp.hovered { TILE_HOVER } else { TILE });
            ui.icon(Icon::File, Rect::new(r.x + r.w * 0.5 - 14.0, r.y + 10.0, 28.0, 28.0), accent());
            let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            let label = Rect::new(r.x + 4.0, r.y + 44.0, r.w - 8.0, 22.0);
            ui.text(label, &name, TextStyle::new(11.0, 400, TEXT_DIM).aligned(Align::Center));

            if tile_hovered {
                ui.fill(remove, 9.0, SURFACE);
                ui.icon(Icon::Close, remove.centered(11.0, 11.0), TEXT_DIM);
                ui.fill(share, 9.0, SURFACE);
                ui.icon(Icon::Phone, share.centered(11.0, 11.0), TEXT_DIM);
            }

            if removed {
                ui.out.push(Command::ShelfRemove(i));
            } else if shared {
                ui.out.push(Command::ShelfShare(i));
            } else if resp.held && ui.input.mouse.is_some_and(moved) {
                ui.widgets.active = None;
                ui.out.push(Command::ShelfDragOut(i));
            }
        }

        if let Some(info) = hovered_item.and_then(|p| shelf_store::info(p)) {
            ui.text(bar, &shelf_store::describe(&info, crate::ui::is_english()), TextStyle::new(11.0, 400, TEXT_FAINT).aligned(Align::Center));
        } else if hovered_item.is_none() && total_rows > rows_visible {
            ui.text(bar, crate::ui::tr("Molette pour défiler", "Scroll for more"), TextStyle::new(11.0, 400, TEXT_FAINT).aligned(Align::Center));
        }
    }
}

/// The QR code, what it does and how long it lives.
fn share_card(ui: &mut Ui, area: Rect, card: &Card) {
    let mut rest = area;
    let mut head = rest.take_top(28.0, 6.0);
    if ui.icon_button(id("shelf.share.back", 0), head.take_left(28.0, 6.0), Icon::Back, 17.0, TEXT_DIM).clicked {
        ui.out.push(Command::ShelfShareStop);
    }
    ui.text(head, crate::ui::tr("Envoyer au téléphone", "Send to phone"), TextStyle::new(13.0, 600, TEXT));

    // Quiet zone of four modules on each side, as the QR standard asks.
    let cells = card.modules + 8;
    let mut right = rest;
    if card.modules > 0 {
        let cell = (rest.h / cells as f32).floor().max(1.0);
        let side = cell * cells as f32;
        let qr = Rect::new(rest.x, rest.y, side, side);
        ui.fill(qr, R_SMALL, Color::rgb(255, 255, 255));
        for &(x, y, len) in card.runs.iter() {
            ui.fill(Rect::new(qr.x + (x as f32 + 4.0) * cell, qr.y + (y as f32 + 4.0) * cell, len as f32 * cell, cell), 0.0, Color::rgb(0, 0, 0));
        }
        right.take_left(side, 12.0);
    }

    let mut col = right;
    ui.text(col.take_top(18.0, 2.0), &card.name, TextStyle::new(12.5, 600, TEXT));
    let (status, color) = match card.status {
        Status::Waiting => (crate::ui::tr("En attente du téléphone", "Waiting for the phone"), TEXT_DIM),
        Status::Done => (crate::ui::tr("Envoyé", "Sent"), accent()),
        Status::Expired => (crate::ui::tr("Lien expiré", "Link expired"), TEXT_DIM),
        Status::Failed => (crate::ui::tr("Réseau local introuvable", "No local network found"), TEXT_DIM),
    };
    ui.text(col.take_top(16.0, 2.0), status, TextStyle::new(11.0, 500, color));
    if !card.host.is_empty() {
        ui.text(col.take_top(15.0, 8.0), &card.host, TextStyle::new(10.5, 400, TEXT_FAINT));
    }
    let hint = match card.status {
        Status::Waiting => crate::ui::tr(
            "Le téléphone doit être sur le même Wi-Fi. Le lien s'arrête après un téléchargement ou 10 minutes.",
            "The phone must be on the same Wi-Fi. The link stops after one download or 10 minutes.",
        ),
        Status::Failed => crate::ui::tr("Connecte-toi à un réseau privé puis réessaie.", "Connect to a private network and try again."),
        _ => "",
    };
    let hint_r = col.take_top(64.0, 8.0);
    ui.text(hint_r, hint, TextStyle::new(11.0, 400, TEXT_FAINT).top().wrapped());

    let stop = Rect::new(col.x, rest.bottom() - 26.0, col.w.min(120.0), 26.0);
    let label = if card.status == Status::Waiting { crate::ui::tr("Arrêter", "Stop") } else { crate::ui::tr("Fermer", "Close") };
    let resp = ui.interact(id("shelf.share.stop", 0), stop);
    ui.fill(stop, R_SMALL, if resp.hovered { TILE_HOVER } else { TILE });
    ui.text(stop, label, TextStyle::new(11.0, 500, TEXT).aligned(Align::Center));
    if resp.clicked {
        ui.out.push(Command::ShelfShareStop);
    }
}
