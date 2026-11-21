//! Battery of the PC and of the peripherals that report one (Bluetooth mouse,
//! headset...). The peripherals are read only while this widget is drawn:
//! when it appears, and again after a device arrives or leaves. There is no
//! timer; the desktop window reads only after a device change.

use nb_core::{DeviceBattery, DeviceKind};

use crate::state::{AppState, Command};
use crate::ui::theme::*;
use crate::ui::{Align, Rect, TextStyle, Ui, tr};

/// On the island, a read older than this is renewed when the widget is drawn
/// again (it becomes visible). Nothing is scheduled.
pub const STALE_MS: u64 = 60_000;
const ROW_H: f32 = 18.0;
const LABEL_W: f32 = 52.0;
const VALUE_W: f32 = 36.0;

/// Peripheral levels and when they were read (Unix ms, 0 when stale).
#[derive(Debug, Default, Clone)]
pub struct Batteries {
    pub devices: Vec<DeviceBattery>,
    pub read_at_ms: u64,
}

impl Batteries {
    pub fn update(&mut self, devices: Vec<DeviceBattery>, now_ms: u64) {
        self.devices = devices;
        self.read_at_ms = now_ms;
    }

    /// A device arrived or left: read again the next time it is shown.
    pub fn invalidate(&mut self) {
        self.read_at_ms = 0;
    }

    /// Whether a read must start now. Never read: yes. Invalidated by a device
    /// change (`read_at_ms == 0`): yes. Otherwise only `stale_ok` callers (the
    /// island, when the widget becomes visible) renew an old read.
    pub fn needs_read(&self, now_ms: u64, stale_ok: bool) -> bool {
        self.read_at_ms == 0 || (stale_ok && now_ms >= self.read_at_ms + STALE_MS)
    }
}

/// Starts a read on a short-lived thread, unless one started moments ago:
/// the widget asks on every rebuild until the answer arrives.
pub fn request(sink: &nb_core::Sink) {
    use std::sync::atomic::{AtomicU64, Ordering};
    static LAST: AtomicU64 = AtomicU64::new(0);
    let now = crate::state::unix_ms();
    if now.saturating_sub(LAST.load(Ordering::Relaxed)) < 10_000 {
        return;
    }
    LAST.store(now, Ordering::Relaxed);
    nb_system::devices::refresh(sink.clone());
}

fn kind_label(kind: DeviceKind) -> Option<&'static str> {
    Some(match kind {
        DeviceKind::Headset => tr("Casque", "Headset"),
        DeviceKind::Mouse => tr("Souris", "Mouse"),
        DeviceKind::Keyboard => tr("Clavier", "Keyboard"),
        DeviceKind::Controller => tr("Manette", "Controller"),
        DeviceKind::Other => return None,
    })
}

/// Short labels: the kind of device, or its name when the kind is unknown or
/// shared by two devices.
pub fn labels(devices: &[DeviceBattery]) -> Vec<String> {
    devices
        .iter()
        .map(|d| {
            let shared = devices.iter().filter(|o| o.kind == d.kind).count() > 1;
            match kind_label(d.kind) {
                Some(l) if !shared => l.to_string(),
                _ => d.name.clone(),
            }
        })
        .collect()
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

/// Every row shown: label and percent.
pub fn rows(st: &AppState) -> Vec<(String, u8)> {
    let mut out = Vec::new();
    if let Some(p) = st.power.and_then(|p| p.battery_percent) {
        out.push(("PC".to_string(), p.min(100)));
    }
    for (label, d) in labels(&st.batteries.devices).into_iter().zip(&st.batteries.devices) {
        out.push((label, d.percent));
    }
    out
}

/// Height the list needs, title included.
pub fn height(st: &AppState) -> f32 {
    22.0 + rows(st).len().max(1) as f32 * ROW_H + 16.0
}

/// Compact list "PC 80 %", "Casque 45 %"... with small bars.
pub fn draw(ui: &mut Ui, st: &AppState, r: Rect, stale_ok: bool) {
    if st.batteries.needs_read(ui.now_ms, stale_ok) {
        ui.out.push(Command::RefreshBatteries);
    }
    ui.fill(r, R_TILE, TILE);
    let mut rest = r.inset_xy(12.0, 8.0);
    let rows = rows(st);
    if rest.h >= 22.0 + ROW_H * rows.len().max(1) as f32 {
        ui.text(rest.take_top(18.0, 4.0), tr("Batteries", "Batteries"), TextStyle::new(11.5, 600, TEXT_DIM));
    }
    if rows.is_empty() {
        let hint = if st.batteries.read_at_ms == 0 { tr("Recherche…", "Looking…") } else { tr("Aucun appareil", "No device") };
        ui.text(rest.take_top(ROW_H, 0.0), hint, TextStyle::new(11.0, 400, TEXT_FAINT));
        return;
    }
    let max_rows = (rest.h / ROW_H).floor().max(1.0) as usize;
    for (label, percent) in rows.iter().take(max_rows) {
        let mut row = rest.take_top(ROW_H, 0.0);
        let label_w = LABEL_W.min(row.w * 0.4);
        ui.text(row.take_left(label_w, 6.0), &clip(label, (label_w / 6.2) as usize), TextStyle::new(11.0, 500, TEXT_DIM));
        ui.text(row.take_right(VALUE_W, 6.0), &format!("{percent} %"), TextStyle::new(11.0, 600, TEXT).aligned(Align::End));
        let bar = row.centered(row.w, 4.0);
        ui.fill(bar, 2.0, TILE_HOVER);
        let w = (bar.w * *percent as f32 / 100.0).round().max(4.0);
        ui.fill(Rect::new(bar.x, bar.y, w.min(bar.w), bar.h), 2.0, accent());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dev(name: &str, kind: DeviceKind, percent: u8) -> DeviceBattery {
        DeviceBattery { name: name.into(), kind, percent }
    }

    #[test]
    fn devices_are_labelled_by_kind_unless_ambiguous() {
        let list = [dev("Arctis Nova 7", DeviceKind::Headset, 45), dev("Aerox 5 WL", DeviceKind::Mouse, 70), dev("Tag", DeviceKind::Other, 9)];
        assert_eq!(labels(&list), [tr("Casque", "Headset"), tr("Souris", "Mouse"), "Tag"]);
        let two = [dev("Aerox 5 WL", DeviceKind::Mouse, 70), dev("MX Master", DeviceKind::Mouse, 20)];
        assert_eq!(labels(&two), ["Aerox 5 WL", "MX Master"]);
    }

    #[test]
    fn the_pc_comes_first() {
        let mut st = AppState::default();
        st.power = Some(nb_core::PowerState { battery_percent: Some(80), charging: false, mode: nb_core::PowerMode::Balanced, seconds_left: None });
        st.batteries.update(vec![dev("soundcore P31i", DeviceKind::Headset, 45)], 1);
        assert_eq!(rows(&st), [("PC".to_string(), 80), (tr("Casque", "Headset").to_string(), 45)]);
    }

    #[test]
    fn reads_happen_on_first_draw_device_change_or_stale_island_only() {
        let mut b = Batteries::default();
        assert!(b.needs_read(1_000, false), "never read");
        b.update(Vec::new(), 1_000);
        assert!(!b.needs_read(1_000_000, false), "the desktop window never renews by age");
        assert!(!b.needs_read(60_999, true));
        assert!(b.needs_read(61_000, true), "the island renews when shown again");
        b.update(Vec::new(), 2_000);
        b.invalidate();
        assert!(b.needs_read(2_001, false));
    }

    #[test]
    fn long_labels_are_cut() {
        assert_eq!(clip("Aerox 5 WL", 8), "Aerox 5…");
        assert_eq!(clip("PC", 8), "PC");
    }
}
