//! Clipboard history as a horizontally scrolling row of cards.

use nb_core::{ClipContent, ClipItem};

use super::{View, empty_state};
use crate::state::{AppState, Command};
use crate::ui::theme::*;
use crate::ui::{Icon, Rect, TextStyle, Ui, id};

const CARD_W: f32 = 136.0;
const CARD_GAP: f32 = 8.0;
/// Holding an image this long opens it full size instead of copying it.
const HOLD_MS: u64 = 220;
/// Pointer travel, in pixels, that turns a press into a drag.
const DRAG_THRESHOLD: f32 = 5.0;

#[derive(Default)]
pub struct ClipboardView {
    filter: usize,
    scroll: f32,
    copied: Option<u64>,
    /// Image being held and the instant the press started.
    held: Option<(u64, u64)>,
    /// The press in progress already opened a preview, so it is not a click.
    peeked: bool,
}

fn matches_filter(item: &ClipItem, filter: usize) -> bool {
    match filter {
        1 => matches!(item.content, ClipContent::Text(_)),
        2 => matches!(item.content, ClipContent::Image { .. }),
        3 => matches!(item.content, ClipContent::Files(_)),
        _ => true,
    }
}

impl View for ClipboardView {
    fn name(&self) -> &'static str {
        "clipboard"
    }

    fn icon(&self) -> Icon {
        Icon::Clipboard
    }

    fn build(&mut self, ui: &mut Ui, st: &AppState, area: Rect) {
        let mut rest = area;
        let mut head = rest.take_top(30.0, 10.0);
        let clear = head.take_right(30.0, 8.0);
        if ui.icon_button(id("clip.clear", 0), clear, Icon::Trash, 17.0, TEXT_DIM).clicked {
            ui.out.push(Command::ClearClips);
            self.scroll = 0.0;
        }
        let seg = head.take_left(280.0, 0.0);
        if let Some(i) = ui.segmented(id("clip.filter", 0), seg, &[crate::ui::tr("Tout", "All"), crate::ui::tr("Texte", "Text"), "Images", crate::ui::tr("Fichiers", "Files")], self.filter) {
            self.filter = i;
            self.scroll = 0.0;
        }

        let mut items: Vec<&ClipItem> = st.clips.iter().filter(|c| matches_filter(c, self.filter)).collect();
        // Pinned first, each group keeps its newest-first order.
        items.sort_by_key(|c| !c.pinned);
        if items.is_empty() {
            empty_state(ui, rest, Icon::Clipboard, crate::ui::tr("Rien de copié pour l'instant", "Nothing copied yet"));
            return;
        }

        let list = rest;
        let content = items.len() as f32 * (CARD_W + CARD_GAP) - CARD_GAP;
        let max_scroll = (content - list.w).max(0.0);
        if ui.hovered(list) && ui.input.wheel != 0.0 {
            self.scroll -= ui.input.wheel * (CARD_W + CARD_GAP);
        }
        self.scroll = self.scroll.clamp(0.0, max_scroll);

        let mut still_held = None;
        ui.push_clip(list, 0.0);
        for (i, item) in items.iter().enumerate() {
            let card = Rect::new(list.x + i as f32 * (CARD_W + CARD_GAP) - self.scroll, list.y, CARD_W, list.h);
            if card.right() < list.x || card.x > list.right() {
                continue;
            }
            let over = ui.hovered(list) && ui.hovered(card);
            let pin_r = Rect::new(card.x + 6.0, card.y + 6.0, 22.0, 22.0);
            let del_r = Rect::new(card.right() - 28.0, card.y + 6.0, 22.0, 22.0);
            // Buttons claim the press before the card underneath does.
            let show_pin = over || item.pinned;
            let pin = if show_pin && ui.hovered(list) { ui.interact(id("clip.pin", item.id as usize), pin_r) } else { Default::default() };
            let del = if over { ui.interact(id("clip.del", item.id as usize), del_r) } else { Default::default() };
            let resp = if ui.hovered(list) { ui.interact(id("clip.card", item.id as usize), card) } else { Default::default() };
            ui.fill(card, R_TILE, if resp.hovered { TILE_HOVER } else { TILE });
            let inner = card.inset(12.0);
            match &item.content {
                ClipContent::Text(t) => {
                    let snippet: String = t.chars().take(220).collect();
                    ui.text(inner, &snippet, TextStyle::new(12.5, 400, TEXT).wrapped().top());
                }
                ClipContent::Image { png, .. } => {
                    ui.image_contain(card.inset(6.0), R_SMALL, png);
                    if resp.held {
                        let since = self.held.filter(|h| h.0 == item.id).map_or(ui.now_ms, |h| h.1);
                        still_held = Some((item.id, since));
                        if !self.peeked {
                            if ui.now_ms >= since + HOLD_MS {
                                self.peeked = true;
                                ui.out.push(Command::PreviewClip(item.id));
                            } else {
                                ui.redraw_at(since + HOLD_MS);
                            }
                        }
                    }
                }
                ClipContent::Files(paths) => {
                    let mut body = inner;
                    ui.icon(Icon::File, body.take_top(30.0, 8.0).centered(28.0, 28.0), accent());
                    let names: Vec<String> = paths.iter().filter_map(|p| p.file_name().map(|n| n.to_string_lossy().into_owned())).collect();
                    ui.text(body, &names.join("\n"), TextStyle::new(11.5, 400, TEXT_DIM).wrapped().top());
                }
            }
            // Pressing and moving takes the item out to another app; pressing
            // still opens the large preview of an image instead.
            if resp.held && !self.peeked {
                let press = ui.input.press_pos;
                if ui.input.mouse.is_some_and(|m| (m.x - press.x).hypot(m.y - press.y) > DRAG_THRESHOLD) {
                    ui.widgets.active = None;
                    still_held = None;
                    ui.out.push(Command::ClipDragOut(item.id));
                }
            }
            if show_pin {
                ui.fill(pin_r, 11.0, if pin.hovered { TILE_HOVER } else { SURFACE });
                ui.icon(Icon::Pin, pin_r.centered(14.0, 14.0), if item.pinned { accent() } else { TEXT_DIM });
            }
            if over {
                ui.fill(del_r, 11.0, if del.hovered { TILE_HOVER } else { SURFACE });
                ui.icon(Icon::Trash, del_r.centered(14.0, 14.0), TEXT_DIM);
            }
            if pin.clicked {
                ui.out.push(Command::PinClip(item.id, !item.pinned));
            }
            if del.clicked {
                ui.out.push(Command::DeleteClip(item.id));
            }
            if self.copied == Some(item.id) {
                let mark = Rect::new(card.right() - 28.0, card.bottom() - 28.0, 20.0, 20.0);
                ui.fill(mark, 10.0, accent());
                ui.icon(Icon::Check, mark.centered(14.0, 14.0), SURFACE);
            }
            if resp.clicked && !self.peeked && !pin.clicked && !del.clicked {
                ui.out.push(Command::CopyClip(item.id));
                self.copied = Some(item.id);
            }
        }
        ui.pop_clip();
        self.held = still_held;
        if !ui.input.down {
            self.peeked = false;
        }
    }
}
