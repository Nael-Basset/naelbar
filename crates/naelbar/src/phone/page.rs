//! The "Téléphone" settings page below its switch: the one QR code, to the
//! hosted site.

use std::cell::RefCell;

use crate::ui::theme::*;
use crate::ui::{Color, Rect, TextStyle, Ui, tr};

/// The hosted agenda: works away from home, signed in once with Google.
pub const AGENDA_URL: &str = "https://naelbar-agenda-eadafc.web.app";
/// Height of the agenda block, drawn on the phone page whatever the switch.
pub const AGENDA_HEIGHT: f32 = 12.0 + 160.0 + 12.0;
const AGENDA_QR: f32 = 148.0;


struct Cached {
    url: String,
    modules: usize,
    runs: Vec<(u16, u16, u16)>,
}

thread_local! {
    static QR: RefCell<Vec<Cached>> = const { RefCell::new(Vec::new()) };
}

fn qr_for(url: &str) -> Option<(usize, Vec<(u16, u16, u16)>)> {
    QR.with(|c| {
        let mut c = c.borrow_mut();
        if !c.iter().any(|q| q.url == url) {
            let qr = qrcodegen::QrCode::encode_text(url, qrcodegen::QrCodeEcc::Medium).ok()?;
            if c.len() >= 2 {
                c.remove(0);
            }
            c.push(Cached { url: url.to_string(), modules: qr.size() as usize, runs: crate::share::qr_runs(&qr) });
        }
        c.iter().find(|q| q.url == url).map(|q| (q.modules, q.runs.clone()))
    })
}

fn paint_qr(ui: &mut Ui, url: &str, x: f32, y: f32, side_max: f32) {
    let Some((modules, runs)) = qr_for(url) else { return };
    // Quiet zone of four modules on each side.
    let cells = modules + 8;
    let cell = (side_max / cells as f32).floor().max(1.0);
    let side = cell * cells as f32;
    let qr = Rect::new(x, y + (side_max - side) * 0.5, side, side);
    ui.fill(qr, R_SMALL, Color::rgb(255, 255, 255));
    for (cx, cy, len) in runs {
        ui.fill(Rect::new(qr.x + (cx as f32 + 4.0) * cell, qr.y + (cy as f32 + 4.0) * cell, len as f32 * cell, cell), 0.0, Color::rgb(0, 0, 0));
    }
}

/// The hosted agenda's code, which needs neither the same Wi-Fi nor this PC.
pub fn draw_agenda(ui: &mut Ui, x: f32, mut y: f32, w: f32, view_h: f32) {
    ui.fill(Rect::new(x, y, w, 1.0), 0.0, HAIRLINE);
    y += 12.0;
    if y + 160.0 < 0.0 || y > view_h {
        return;
    }
    paint_qr(ui, AGENDA_URL, x, y + 6.0, AGENDA_QR);
    let text_x = x + AGENDA_QR + 24.0;
    let text_w = (w - AGENDA_QR - 24.0).max(80.0);
    ui.text(Rect::new(text_x, y + 12.0, text_w, 20.0), tr("naelbar sur le téléphone", "naelbar on the phone"), TextStyle::new(14.0, 500, TEXT));
    ui.text(
        Rect::new(text_x, y + 36.0, text_w, 72.0),
        tr(
            "Agenda, cours et post-its partout, même PC éteint. Sur le même Wi-Fi, avec le lien activé : souris, clavier et fichiers. Connexion Google une seule fois.",
            "Agenda, lessons and notes anywhere, even with this PC off. On the same Wi-Fi, with the link on: mouse, keyboard and files. Sign in with Google once.",
        ),
        TextStyle::new(12.5, 400, TEXT_DIM).top().wrapped(),
    );
    ui.text(Rect::new(text_x, y + 112.0, text_w, 20.0), AGENDA_URL.trim_start_matches("https://"), TextStyle::new(12.5, 500, TEXT_DIM));
}
