//! Quick settings: brightness, volume, radios, mute, battery, power mode.

use nb_core::CaptureKind;

use super::View;
use crate::state::{AppState, Command};
use crate::ui::theme::*;
use crate::ui::{Align, Icon, Rect, TextStyle, Ui, id};

#[derive(Default)]
pub struct QuickView {
    /// Shows the Sonar channels instead of the system controls.
    sonar: bool,
    /// When Sonar was last read, so it is not asked on every frame.
    asked_ms: u64,
    /// When the screens' brightness was last read.
    bright_asked: u64,
}

const SONAR_MAX_AGE_MS: u64 = 30_000;
/// While the Sonar panel is on screen it follows changes made in GG itself.
const SONAR_LIVE_MS: u64 = 1_000;

impl View for QuickView {
    fn name(&self) -> &'static str {
        "quick"
    }

    fn icon(&self) -> Icon {
        Icon::Sliders
    }

    fn build(&mut self, ui: &mut Ui, st: &AppState, area: Rect) {
        let max_age = if self.sonar { SONAR_LIVE_MS } else { SONAR_MAX_AGE_MS };
        if ui.now_ms.saturating_sub(self.asked_ms) > max_age {
            self.asked_ms = ui.now_ms;
            ui.out.push(Command::SonarRefresh);
        }
        if self.sonar && !st.sonar.is_empty() {
            self.sonar_panel(ui, st, area);
            return;
        }
        let mut rest = area;

        let value_style = TextStyle::new(12.0, 500, TEXT_DIM).aligned(Align::End);

        // Screens plugged in or out show up the next time the tab is opened.
        if self.bright_asked == 0 || ui.now_ms.saturating_sub(self.bright_asked) > SONAR_MAX_AGE_MS {
            self.bright_asked = ui.now_ms;
            ui.out.push(Command::BrightnessRefresh);
        }
        let screens: Vec<(u32, &str, Option<u8>)> = if st.screens.is_empty() {
            vec![(0, "", st.brightness)]
        } else {
            st.screens.iter().take(3).map(|s| (s.id, s.name.as_str(), Some(s.percent))).collect()
        };
        let several = screens.len() > 1;
        for (i, (screen, name, pct)) in screens.into_iter().enumerate() {
            let mut row = if several { rest.take_top(28.0, 2.0) } else { rest.take_top(34.0, 4.0) };
            let lead = row.take_left(36.0, 8.0);
            if several {
                // Which screen this slider drives: "PC" or the monitor's model.
                ui.text(lead, name, TextStyle::new(10.5, 500, TEXT_DIM).aligned(Align::Center));
            } else {
                ui.icon(Icon::Sun, lead.centered(20.0, 20.0), TEXT_DIM);
            }
            let label = row.take_right(40.0, 8.0);
            if let Some(p) = pct {
                ui.text(label, &format!("{p}%"), value_style);
            }
            if let Some(v) = ui.slider(id("quick.brightness", i), row, pct.unwrap_or(0) as f32 / 100.0) {
                ui.out.push(Command::SetBrightness(screen, (v * 100.0).round() as u8));
            }
        }
        if several {
            rest.take_top(0.0, 2.0);
        }

        let muted = st.audio.as_ref().is_some_and(|a| a.output_muted);
        let volume = st.audio.as_ref().map_or(0.0, |a| a.output_volume);
        let mut row = rest.take_top(34.0, 12.0);
        let icon = if muted { Icon::Muted } else { Icon::Volume };
        if ui.icon_button(id("quick.mute_icon", 0), row.take_left(36.0, 8.0), icon, 20.0, if muted { accent() } else { TEXT_DIM }).clicked {
            ui.out.push(Command::SetMuted(!muted));
        }
        if !st.sonar.is_empty() {
            let chip = row.take_right(52.0, 8.0).centered(52.0, 24.0);
            let resp = ui.interact(id("quick.sonar", 0), chip);
            ui.fill(chip, 12.0, if resp.hovered { TILE_HOVER } else { TILE });
            ui.text(chip, "Sonar", TextStyle::new(11.5, 500, TEXT_DIM).aligned(Align::Center));
            if resp.clicked {
                self.sonar = true;
                self.asked_ms = ui.now_ms;
                ui.out.push(Command::SonarRefresh);
            }
        }
        let label = row.take_right(40.0, 8.0);
        if st.audio.is_some() {
            ui.text(label, &format!("{}%", (volume * 100.0).round() as u32), value_style);
        }
        if let Some(v) = ui.slider(id("quick.volume", 0), row, volume) {
            ui.out.push(Command::SetVolume(v));
        }

        let tiles_row = rest.take_top(66.0, 10.0);
        let tiles = tiles_row.columns(4, 8.0);
        let wifi = st.radios.and_then(|r| r.wifi);
        if ui.toggle_tile(id("quick.wifi", 0), tiles[0], Icon::Wifi, "Wi-Fi", wifi == Some(true)).clicked {
            if let Some(on) = wifi {
                ui.out.push(Command::SetWifi(!on));
            }
        }
        let bt = st.radios.and_then(|r| r.bluetooth);
        if ui.toggle_tile(id("quick.bt", 0), tiles[1], Icon::Bluetooth, "Bluetooth", bt == Some(true)).clicked {
            if let Some(on) = bt {
                ui.out.push(Command::SetBluetooth(!on));
            }
        }
        let mic_muted = st.audio.as_ref().is_some_and(|a| a.mic_muted);
        if ui.toggle_tile(id("quick.mic", 0), tiles[2], Icon::Mic, if mic_muted { crate::ui::tr("Micro coupé", "Mic muted") } else { crate::ui::tr("Micro actif", "Mic on") }, mic_muted).clicked {
            ui.out.push(Command::SetMicMuted(!mic_muted));
        }
        let (battery, charging) = match st.power {
            Some(p) => (p.battery_percent, p.charging),
            None => (None, false),
        };
        let label = match battery {
            Some(pct) => match st.power.and_then(|p| p.seconds_left) {
                Some(s) => format!("{pct}% · {}", crate::hud::format_left(s)),
                None => format!("{pct}%"),
            },
            None => crate::ui::tr("Secteur", "AC").to_string(),
        };
        if ui.toggle_tile(id("quick.battery", 0), tiles[3], Icon::Battery, &label, charging).clicked {
            ui.out.push(Command::OpenPowerSettings);
        }

        let line = rest.take_top(16.0, 0.0);
        if let Some(text) = crate::activity::quick_text(&st.activity) {
            // Names the applications using the microphone, camera or screen.
            let mut x = line.x;
            for kind in [CaptureKind::Microphone, CaptureKind::Camera, CaptureKind::Screen] {
                if st.activity.capture_active(kind) {
                    ui.fill(Rect::new(x, line.center().y - 3.0, 6.0, 6.0), 3.0, crate::activity::kind_color(kind));
                    x += 11.0;
                }
            }
            ui.text(Rect::new(x + 2.0, line.y, line.right() - x - 2.0, line.h), &text, TextStyle::new(11.0, 400, TEXT_DIM));
        } else if let Some(done) = st.activity.last_done(ui.now_ms) {
            // The pill only says "Downloaded" for a moment: this stays a while.
            let hit = Rect::new(line.x, line.y - 3.0, line.w, line.h + 6.0);
            let resp = ui.interact(id("quick.download", 0), hit);
            let label = format!("{} · {}", crate::ui::tr("Téléchargé", "Downloaded"), crate::activity::shorten(&done.name, 28));
            ui.text(line, &label, TextStyle::new(11.0, 400, if resp.hovered { TEXT } else { TEXT_DIM }));
            ui.text(line, crate::ui::tr("Afficher", "Show"), TextStyle::new(11.0, 500, if resp.hovered { TEXT } else { TEXT_DIM }).aligned(Align::End));
            if resp.clicked {
                ui.out.push(Command::OpenDownload(done.path.clone()));
            }
        }
    }
}

impl QuickView {
    /// Uses SteelSeries Sonar's undocumented local API (see nb-integrations steelseries.rs).
    fn sonar_panel(&mut self, ui: &mut Ui, st: &AppState, area: Rect) {
        let mut rest = area;
        let mut head = rest.take_top(26.0, 8.0);
        if ui.icon_button(id("sonar.back", 0), head.take_left(26.0, 6.0), Icon::Back, 16.0, TEXT_DIM).clicked {
            self.sonar = false;
        }
        ui.text(head, "Sonar", TextStyle::new(13.5, 600, TEXT));
        ui.text(head, crate::ui::tr("API non officielle de GG", "Unofficial GG API"), TextStyle::new(10.5, 400, TEXT_FAINT).aligned(Align::End));
        let value_style = TextStyle::new(12.0, 500, TEXT_DIM).aligned(Align::End);
        // The next read is due then; nothing runs once the panel is closed.
        ui.redraw_at(self.asked_ms + SONAR_LIVE_MS + 20);
        for (i, c) in st.sonar.iter().enumerate() {
            let name = match c.id.as_str() {
                "game" => crate::ui::tr("Jeu", "Game"),
                "chatRender" => "Chat",
                "media" => crate::ui::tr("Média", "Media"),
                "aux" => "Aux",
                "chatCapture" => crate::ui::tr("Micro", "Mic"),
                _ => continue,
            };
            let mut row = rest.take_top(28.0, 6.0);
            ui.text(row.take_left(56.0, 8.0), name, TextStyle::new(12.0, 500, if c.muted { TEXT_FAINT } else { TEXT }));
            let label = row.take_right(48.0, 8.0);
            let text = if c.muted { crate::ui::tr("muet", "muted").to_string() } else { format!("{}%", (c.volume * 100.0).round() as u32) };
            ui.text(label, &text, value_style);
            if let Some(v) = ui.slider(id("sonar.ch", i), row, c.volume) {
                ui.out.push(Command::SonarVolume(c.id.clone(), v));
            }
        }
    }
}
