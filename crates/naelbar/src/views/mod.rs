//! One tab per file. A view reads [`AppState`], records into [`Ui`], and emits
//! commands through `ui.out`.

pub mod agenda;
pub mod call;
pub mod claude;
pub mod clipboard;
pub mod dock;
pub mod media;
pub mod notifications;
pub mod quick;
pub mod shelf;
pub mod widgets;

use crate::state::AppState;
use crate::ui::theme::*;
use crate::ui::{Align, Icon, Rect, TextStyle, Ui};

pub trait View {
    /// Stable identifier, stored as the last tab in the config file.
    fn name(&self) -> &'static str;
    fn icon(&self) -> Icon;
    fn build(&mut self, ui: &mut Ui, st: &AppState, area: Rect);
    /// True while the view needs the stats subscription.
    fn wants_stats(&self) -> bool {
        false
    }
}

pub fn all() -> Vec<Box<dyn View>> {
    vec![
        Box::new(media::MediaView),
        Box::new(quick::QuickView::default()),
        Box::new(widgets::WidgetsView::default()),
        Box::new(agenda::AgendaView::default()),
        Box::new(claude::ClaudeView::default()),
        Box::new(clipboard::ClipboardView::default()),
        Box::new(notifications::NotificationsView::default()),
        Box::new(shelf::ShelfView::default()),
        Box::new(dock::DockView::default()),
    ]
}

/// Centred glyph and caption for a view with nothing to show.
pub fn empty_state(ui: &mut Ui, area: Rect, icon: Icon, text: &str) {
    let c = area.center();
    ui.icon(icon, Rect::new(c.x - 18.0, c.y - 34.0, 36.0, 36.0), TEXT_FAINT);
    let t = TextStyle::new(13.0, 400, TEXT_DIM).aligned(Align::Center);
    ui.text(Rect::new(area.x, c.y + 8.0, area.w, 22.0), text, t);
}
