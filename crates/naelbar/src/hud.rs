//! Transient HUDs in the closed island: volume, brightness, charger, low
//! battery, lock keys and microphone. Each one is shown for [`HUD_MS`] after a
//! change, then the island returns to what it showed before. No timer exists
//! besides the one-shot redraw aimed at the end of the visible HUD.

use nb_core::{AudioState, DeviceBattery, LockKey, PowerState};

use crate::ui::theme::*;
use crate::ui::{Align, Icon, Rect, TextStyle, Ui, tr};

pub const HUD_MS: u64 = 1600;
/// Width of the closed island while a HUD shows, before the pill zoom.
pub const HUD_W: f32 = 200.0;
/// Battery levels announced once each time the battery drops through them.
const LOW_LEVELS: [u8; 2] = [20, 10];

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum HudKind {
    /// 0.0 to 1.0.
    Volume { level: f32, muted: bool },
    Brightness(u8),
    Charger { plugged: bool, percent: u8, seconds_left: Option<u32> },
    BatteryLow(u8),
    Lock(LockKey, bool),
    Mic { muted: bool },
    /// A Bluetooth device crossed 20 % or 10 %, or connected / disconnected.
    Device { name: ShortName, percent: u8, change: DeviceChange },
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum DeviceChange {
    Low,
    Connected,
    Disconnected,
}

/// A device name cut to 14 characters, stored inline so `HudKind` stays `Copy`.
#[derive(Clone, Copy, PartialEq)]
pub struct ShortName {
    buf: [u8; 48],
    len: u8,
}

impl ShortName {
    pub fn new(name: &str) -> Self {
        let mut s = String::new();
        let count = name.chars().count();
        for (i, c) in name.chars().enumerate() {
            if i >= 14 {
                break;
            }
            let c = if count > 14 && i == 13 { '…' } else { c };
            if s.len() + c.len_utf8() > 48 {
                break;
            }
            s.push(c);
        }
        let mut buf = [0u8; 48];
        buf[..s.len()].copy_from_slice(s.as_bytes());
        Self { buf, len: s.len() as u8 }
    }

    pub fn as_str(&self) -> &str {
        std::str::from_utf8(&self.buf[..self.len as usize]).unwrap_or("")
    }
}

impl std::fmt::Debug for ShortName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}", self.as_str())
    }
}

/// What changed between two reads of the Bluetooth batteries, most important
/// first: low-level crossings, then connections and disconnections. Devices
/// are matched by name. A level is announced once, when it falls through 20 %
/// or 10 % (again only after it rose above and fell again).
pub fn device_changes(prev: &[DeviceBattery], new: &[DeviceBattery]) -> Vec<HudKind> {
    let mut low = Vec::new();
    let mut links = Vec::new();
    for d in new {
        let name = ShortName::new(&d.name);
        match prev.iter().find(|p| p.name == d.name) {
            None => links.push(HudKind::Device { name, percent: d.percent, change: DeviceChange::Connected }),
            Some(p) if LOW_LEVELS.iter().any(|&l| p.percent > l && d.percent <= l) => {
                low.push(HudKind::Device { name, percent: d.percent, change: DeviceChange::Low })
            }
            Some(_) => {}
        }
    }
    for p in prev.iter().filter(|p| !new.iter().any(|d| d.name == p.name)) {
        links.push(HudKind::Device { name: ShortName::new(&p.name), percent: p.percent, change: DeviceChange::Disconnected });
    }
    low.extend(links);
    low
}

#[derive(Debug, Default)]
pub struct HudState {
    current: Option<(HudKind, u64)>,
    /// Set from the "HUD" setting.
    pub disabled: bool,
    /// Set from the "Alertes batterie des appareils" setting.
    pub devices: bool,
    /// Previous Bluetooth read; `None` before the first one, which shows nothing.
    known: Option<Vec<DeviceBattery>>,
}

impl HudState {
    /// Applies the "HUD" settings. Returns whether the lock key hook is wanted.
    pub fn configure(&mut self, c: &crate::config::Config) -> bool {
        self.disabled = c.hud == "none";
        self.devices = c.hud_devices;
        !self.disabled && c.hud_locks
    }

    fn show(&mut self, kind: HudKind, now_ms: u64) {
        if !self.disabled {
            self.current = Some((kind, now_ms + HUD_MS));
        }
    }

    /// The HUD on screen at `now_ms` and the instant it ends.
    pub fn visible(&self, now_ms: u64) -> Option<(HudKind, u64)> {
        self.current.filter(|&(_, until)| !self.disabled && until > now_ms)
    }

    /// `prev` is `None` for the first reading, which never shows anything.
    pub fn on_audio(&mut self, prev: Option<&AudioState>, new: &AudioState, now_ms: u64) {
        let Some(prev) = prev else { return };
        if prev.mic_muted != new.mic_muted {
            self.show(HudKind::Mic { muted: new.mic_muted }, now_ms);
        }
        if prev.output_muted != new.output_muted || (prev.output_volume - new.output_volume).abs() > 0.004 {
            self.show(HudKind::Volume { level: new.output_volume.clamp(0.0, 1.0), muted: new.output_muted }, now_ms);
        }
    }

    pub fn on_brightness(&mut self, prev: Option<u8>, new: u8, now_ms: u64) {
        if prev.is_some_and(|p| p != new) {
            self.show(HudKind::Brightness(new.min(100)), now_ms);
        }
    }

    pub fn on_power(&mut self, prev: Option<PowerState>, new: &PowerState, now_ms: u64) {
        let (Some(prev), Some(percent)) = (prev, new.battery_percent) else { return };
        if prev.charging != new.charging {
            self.show(HudKind::Charger { plugged: new.charging, percent, seconds_left: new.seconds_left }, now_ms);
            return;
        }
        if new.charging {
            return;
        }
        let Some(before) = prev.battery_percent else { return };
        if LOW_LEVELS.iter().any(|&l| before > l && percent <= l) {
            self.show(HudKind::BatteryLow(percent), now_ms);
        }
    }

    /// Whether Bluetooth batteries must be read in the background.
    pub fn wants_devices(&self) -> bool {
        self.devices && !self.disabled
    }

    pub fn on_devices(&mut self, new: &[DeviceBattery], now_ms: u64) {
        let prev = self.known.replace(new.to_vec());
        if let (Some(prev), true) = (prev, self.devices) {
            if let Some(first) = device_changes(&prev, new).into_iter().next() {
                self.show(first, now_ms);
            }
        }
    }

    pub fn on_lock(&mut self, key: LockKey, on: bool, now_ms: u64) {
        self.show(HudKind::Lock(key, on), now_ms);
    }
}

/// "3h20" or "45 min".
pub fn format_left(seconds: u32) -> String {
    let minutes = seconds / 60;
    if minutes >= 60 { format!("{}h{:02}", minutes / 60, minutes % 60) } else { format!("{minutes} min") }
}

/// Draws a HUD in the closed island and schedules the redraw that ends it.
pub fn draw(ui: &mut Ui, kind: HudKind, until_ms: u64, island: Rect) {
    ui.redraw_at(until_ms);
    let icon_r = Rect::new(island.x + 12.0, island.y + (island.h - 16.0) * 0.5, 16.0, 16.0);
    let text = Rect::new(island.x + 34.0, island.y, island.w - 46.0, island.h);
    let label = TextStyle::new(12.5, 500, TEXT);
    let value = TextStyle::new(12.0, 500, TEXT_DIM).aligned(Align::End);
    match kind {
        HudKind::Volume { level, muted } => {
            let icon = if muted { Icon::Muted } else { Icon::Volume };
            ui.icon(icon, icon_r, if muted { TEXT_DIM } else { TEXT });
            bar(ui, text, level, if muted { TEXT_FAINT } else { accent() });
        }
        HudKind::Brightness(b) => {
            ui.icon(Icon::Sun, icon_r, TEXT);
            bar(ui, text, b as f32 / 100.0, accent());
        }
        HudKind::Charger { plugged, percent, seconds_left } => {
            ui.icon(Icon::Battery, icon_r, if plugged { accent() } else { TEXT });
            ui.text(text, if plugged { tr("En charge", "Charging") } else { tr("Sur batterie", "On battery") }, label);
            let v = match seconds_left.filter(|_| !plugged) {
                Some(s) => format!("{percent}% · {}", format_left(s)),
                None => format!("{percent}%"),
            };
            ui.text(text, &v, value);
        }
        HudKind::BatteryLow(percent) => {
            ui.icon(Icon::Battery, icon_r, accent());
            ui.text(text, tr("Batterie faible", "Battery low"), label);
            ui.text(text, &format!("{percent}%"), value);
        }
        HudKind::Lock(key, on) => {
            ui.icon(Icon::Keycap, icon_r, if on { accent() } else { TEXT_DIM });
            let name = match key {
                LockKey::Caps => tr("Verr. maj.", "Caps Lock"),
                LockKey::Num => tr("Verr. num.", "Num Lock"),
                LockKey::Scroll => tr("Arrêt défil.", "Scroll Lock"),
            };
            ui.text(text, name, label);
            ui.text(text, if on { tr("activé", "on") } else { tr("désactivé", "off") }, value);
        }
        HudKind::Device { name, percent, change } => {
            let gone = change == DeviceChange::Disconnected;
            ui.icon(Icon::Battery, icon_r, if gone { TEXT_DIM } else { accent() });
            ui.text(text, name.as_str(), label);
            let v = match change {
                DeviceChange::Low => format!("{percent}%"),
                DeviceChange::Connected => tr("connecté", "connected").to_string(),
                DeviceChange::Disconnected => tr("déconnecté", "disconnected").to_string(),
            };
            ui.text(text, &v, value);
        }
        HudKind::Mic { muted } => {
            ui.icon(Icon::Mic, icon_r, if muted { TEXT_DIM } else { TEXT });
            ui.text(text, if muted { tr("Micro coupé", "Mic muted") } else { tr("Micro actif", "Mic on") }, label);
        }
    }
}

/// Thin level bar with the percent after it.
fn bar(ui: &mut Ui, mut r: Rect, level: f32, color: crate::ui::Color) {
    let pct = r.take_right(34.0, 8.0);
    ui.text(pct, &format!("{}%", (level * 100.0).round() as u32), TextStyle::new(12.0, 500, TEXT_DIM).aligned(Align::End));
    let track = Rect::new(r.x, r.center().y - 1.5, r.w, 3.0);
    ui.fill(track, 1.5, TILE_HOVER);
    let w = track.w * level.clamp(0.0, 1.0);
    if w > 0.5 {
        ui.fill(Rect::new(track.x, track.y, w.max(track.h), track.h), 1.5, color);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nb_core::PowerMode;

    fn audio(vol: f32, muted: bool, mic_muted: bool) -> AudioState {
        AudioState {
            output_volume: vol,
            output_muted: muted,
            mic_volume: 0.5,
            mic_muted,
            mic_in_use: false,
            output: None,
            input: None,
            outputs: Vec::new(),
            inputs: Vec::new(),
        }
    }

    fn power(percent: u8, charging: bool) -> PowerState {
        PowerState { battery_percent: Some(percent), charging, mode: PowerMode::Balanced, seconds_left: None }
    }

    #[test]
    fn first_reading_shows_nothing() {
        let mut h = HudState::default();
        h.on_audio(None, &audio(0.3, false, false), 0);
        h.on_brightness(None, 40, 0);
        h.on_power(None, &power(15, false), 0);
        assert_eq!(h.visible(0), None);
    }

    #[test]
    fn volume_change_shows_then_expires() {
        let mut h = HudState::default();
        h.on_audio(Some(&audio(0.3, false, false)), &audio(0.4, false, false), 1000);
        assert_eq!(h.visible(1000), Some((HudKind::Volume { level: 0.4, muted: false }, 1000 + HUD_MS)));
        assert_eq!(h.visible(1000 + HUD_MS), None);
    }

    #[test]
    fn unrelated_audio_changes_are_ignored() {
        let mut h = HudState::default();
        let mut b = audio(0.3, false, false);
        b.mic_in_use = true;
        h.on_audio(Some(&audio(0.3, false, false)), &b, 0);
        assert_eq!(h.visible(0), None);
    }

    #[test]
    fn mic_mute_shows() {
        let mut h = HudState::default();
        h.on_audio(Some(&audio(0.3, false, false)), &audio(0.3, false, true), 0);
        assert_eq!(h.visible(0).map(|v| v.0), Some(HudKind::Mic { muted: true }));
    }

    #[test]
    fn brightness_only_on_change() {
        let mut h = HudState::default();
        h.on_brightness(Some(40), 40, 0);
        assert_eq!(h.visible(0), None);
        h.on_brightness(Some(40), 60, 0);
        assert_eq!(h.visible(0).map(|v| v.0), Some(HudKind::Brightness(60)));
    }

    #[test]
    fn charger_plug_and_unplug() {
        let mut h = HudState::default();
        h.on_power(Some(power(50, false)), &power(50, true), 0);
        assert_eq!(h.visible(0).map(|v| v.0), Some(HudKind::Charger { plugged: true, percent: 50, seconds_left: None }));
        let mut p = power(50, false);
        p.seconds_left = Some(7200);
        h.on_power(Some(power(50, true)), &p, 10);
        assert_eq!(h.visible(10).map(|v| v.0), Some(HudKind::Charger { plugged: false, percent: 50, seconds_left: Some(7200) }));
    }

    #[test]
    fn low_battery_once_per_crossing() {
        let mut h = HudState::default();
        h.on_power(Some(power(21, false)), &power(20, false), 0);
        assert_eq!(h.visible(0).map(|v| v.0), Some(HudKind::BatteryLow(20)));
        let mut h = HudState::default();
        h.on_power(Some(power(20, false)), &power(19, false), 0);
        assert_eq!(h.visible(0), None, "already below 20");
        h.on_power(Some(power(11, false)), &power(10, false), 0);
        assert_eq!(h.visible(0).map(|v| v.0), Some(HudKind::BatteryLow(10)));
        let mut h = HudState::default();
        h.on_power(Some(power(25, false)), &power(8, false), 0);
        assert_eq!(h.visible(0).map(|v| v.0), Some(HudKind::BatteryLow(8)), "a big drop is announced once");
        let mut h = HudState::default();
        h.on_power(Some(power(21, true)), &power(20, true), 0);
        assert_eq!(h.visible(0), None, "not while charging");
    }

    fn dev(name: &str, percent: u8) -> DeviceBattery {
        DeviceBattery { name: name.into(), kind: nb_core::DeviceKind::Mouse, percent }
    }

    fn change(h: &HudState) -> Option<(String, u8, DeviceChange)> {
        match h.visible(0)?.0 {
            HudKind::Device { name, percent, change } => Some((name.as_str().to_string(), percent, change)),
            _ => None,
        }
    }

    #[test]
    fn device_levels_once_per_crossing() {
        let mut h = HudState { devices: true, ..Default::default() };
        h.on_devices(&[dev("Aerox", 25)], 0);
        assert_eq!(change(&h), None, "first read is a baseline");
        h.on_devices(&[dev("Aerox", 21)], 0);
        assert_eq!(change(&h), None);
        h.on_devices(&[dev("Aerox", 20)], 0);
        assert_eq!(change(&h), Some(("Aerox".into(), 20, DeviceChange::Low)));
        let mut h = HudState { devices: true, ..Default::default() };
        h.on_devices(&[dev("Aerox", 19)], 0);
        h.on_devices(&[dev("Aerox", 18)], 0);
        assert_eq!(change(&h), None, "already below 20");
        h.on_devices(&[dev("Aerox", 10)], 0);
        assert_eq!(change(&h), Some(("Aerox".into(), 10, DeviceChange::Low)));
        let mut h = HudState { devices: true, ..Default::default() };
        h.on_devices(&[dev("Aerox", 30)], 0);
        h.on_devices(&[dev("Aerox", 8)], 0);
        assert_eq!(change(&h), Some(("Aerox".into(), 8, DeviceChange::Low)), "a big drop is announced once");
    }

    #[test]
    fn device_connect_and_disconnect() {
        let mut h = HudState { devices: true, ..Default::default() };
        h.on_devices(&[], 0);
        h.on_devices(&[dev("Buds", 70)], 0);
        assert_eq!(change(&h), Some(("Buds".into(), 70, DeviceChange::Connected)));
        h.on_devices(&[], 5);
        assert_eq!(h.visible(5).map(|v| v.0), Some(HudKind::Device { name: ShortName::new("Buds"), percent: 70, change: DeviceChange::Disconnected }));
        let both = device_changes(&[dev("A", 25), dev("B", 50)], &[dev("A", 15)]);
        assert!(matches!(both[0], HudKind::Device { change: DeviceChange::Low, .. }), "levels come first");
        assert!(matches!(both[1], HudKind::Device { change: DeviceChange::Disconnected, .. }));
    }

    #[test]
    fn device_alerts_can_be_off() {
        let mut h = HudState::default();
        h.on_devices(&[], 0);
        h.on_devices(&[dev("Buds", 70)], 0);
        assert_eq!(h.visible(0), None);
        assert_eq!(ShortName::new("Soundcore Liberty 4 NC").as_str(), "Soundcore Lib…");
    }

    #[test]
    fn disabled_shows_nothing() {
        let mut h = HudState { disabled: true, ..Default::default() };
        h.on_lock(LockKey::Caps, true, 0);
        assert_eq!(h.visible(0), None);
    }

    #[test]
    fn time_left_format() {
        assert_eq!(format_left(45 * 60), "45 min");
        assert_eq!(format_left(3 * 3600 + 20 * 60), "3h20");
        assert_eq!(format_left(3600 + 5 * 60 + 59), "1h05");
    }
}
