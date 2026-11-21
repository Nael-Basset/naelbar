//! Now playing: artwork, metadata, transport, progress.

use nb_core::PlaybackStatus;

use super::{View, empty_state};
use crate::state::{AppState, Command};
use crate::ui::theme::*;
use crate::ui::{Align, Icon, Rect, TextStyle, Ui, format_time, id};

/// Adds the apps not seen before. Returns true when the list changed.
pub fn merge_seen(seen: &mut Vec<String>, apps: &[String]) -> bool {
    let mut changed = false;
    for a in apps {
        if !a.is_empty() && !seen.contains(a) && seen.len() < 64 {
            seen.push(a.clone());
            changed = true;
        }
    }
    changed
}

/// Session choice for `nb-media`, from the Music settings.
pub fn prefs(c: &crate::config::Config) -> nb_media::Prefs {
    nb_media::Prefs { disabled: c.media_disabled.clone(), preferred: c.media_preferred.clone() }
}

/// First app that is switched on, used when "preferred app" is picked.
pub fn first_enabled(c: &crate::config::Config) -> String {
    c.media_seen.iter().find(|a| !c.media_disabled.contains(a)).cloned().unwrap_or_default()
}

pub const APP_ROW: f32 = 40.0;
const NOTE_H: f32 = 30.0;

/// Height of the app list on the Music settings page.
pub fn settings_list_height(c: &crate::config::Config) -> f32 {
    12.0 + NOTE_H + c.media_seen.len() as f32 * APP_ROW
}

/// The seen apps with a switch each, and a dot that picks the preferred one
/// when that priority is chosen. Returns true when the config changed.
pub fn settings_list(ui: &mut Ui, c: &mut crate::config::Config, x: f32, mut y: f32, w: f32, view_h: f32, switch: fn(&mut Ui, crate::ui::Id, Rect, bool) -> bool) -> bool {
    use crate::ui::tr;
    ui.fill(Rect::new(x, y, w, 1.0), 0.0, HAIRLINE);
    y += 12.0;
    let by_pref = !c.media_preferred.is_empty();
    let note = if c.media_seen.is_empty() {
        tr("Lance une musique : son appli apparaîtra ici.", "Play some music: its app shows up here.")
    } else if by_pref {
        tr("Active une appli pour l'afficher. Le rond choisit la préférée.", "Switch an app on to show it. The dot picks the preferred one.")
    } else {
        tr("Active une appli pour l'afficher dans l'îlot.", "Switch an app on to show it in the island.")
    };
    ui.text(Rect::new(x, y, w, 24.0), note, TextStyle::new(12.5, 400, TEXT_DIM));
    y += NOTE_H;
    let mut changed = false;
    let apps = c.media_seen.clone();
    for (i, app) in apps.iter().enumerate() {
        let r = Rect::new(x, y, w, APP_ROW);
        y += APP_ROW;
        if r.bottom() < 0.0 || r.y > view_h {
            continue;
        }
        let on = !c.media_disabled.contains(app);
        let mut label = r;
        if by_pref {
            let dot = label.take_left(16.0, 12.0).centered(16.0, 16.0);
            let resp = ui.interact(id("music.pref", i), dot.inset(-6.0));
            let chosen = c.media_preferred == *app;
            ui.stroke(dot.inset(0.75), 8.0, 1.5, if chosen { accent() } else if resp.hovered { TEXT_DIM } else { TEXT_FAINT });
            if chosen {
                ui.fill(dot.inset(4.0), 4.0, accent());
            }
            if resp.clicked && !chosen {
                c.media_preferred = app.clone();
                c.media_disabled.retain(|d| d != app);
                changed = true;
            }
        }
        label.w -= 60.0;
        ui.text(label, &nb_media::app_label(app), TextStyle::new(13.5, 400, if on { TEXT } else { TEXT_FAINT }));
        if switch(ui, id("music.app", i), r, on) {
            if on {
                c.media_disabled.push(app.clone());
            } else {
                c.media_disabled.retain(|d| d != app);
            }
            changed = true;
        }
    }
    changed
}

pub struct MediaView;

impl View for MediaView {
    fn name(&self) -> &'static str {
        "media"
    }

    fn icon(&self) -> Icon {
        Icon::Note
    }

    fn build(&mut self, ui: &mut Ui, st: &AppState, area: Rect) {
        let Some(m) = &st.media else {
            empty_state(ui, area, Icon::Note, crate::ui::tr("Rien en lecture", "Nothing playing"));
            return;
        };
        let playing = m.status == PlaybackStatus::Playing;
        let mut rest = area;
        let mut top = rest.take_top(136.0, 14.0);
        let art = top.take_left(136.0, 18.0);
        if let Some(bytes) = &m.artwork {
            ui.image(art, R_TILE, bytes);
        } else {
            ui.fill(art, R_TILE, TILE);
            ui.icon(Icon::Note, art.centered(48.0, 48.0), TEXT_FAINT);
        }

        let mut col = top;
        col.take_top(4.0, 0.0);
        ui.text(col.take_top(26.0, 0.0), &m.title, TextStyle::new(18.0, 600, TEXT));
        ui.text(col.take_top(20.0, 0.0), &m.artist, TextStyle::new(13.5, 400, TEXT_DIM));
        ui.text(col.take_top(18.0, 0.0), &m.album, TextStyle::new(12.0, 400, TEXT_FAINT));

        let row = Rect::new(top.x, top.bottom() - 48.0, top.w, 48.0).centered(168.0, 48.0);
        let cells = row.columns(3, 12.0);
        let dim = |on: bool| if on { TEXT } else { TEXT_FAINT };
        if ui.icon_button(id("media.prev", 0), cells[0].centered(44.0, 44.0), Icon::Prev, 22.0, dim(m.can_previous)).clicked && m.can_previous {
            ui.out.push(Command::MediaPrevious);
        }
        let toggle_icon = if playing { Icon::Pause } else { Icon::Play };
        let toggle = cells[1].centered(48.0, 48.0);
        ui.fill(toggle, 24.0, accent_soft());
        if ui.icon_button(id("media.toggle", 0), toggle, toggle_icon, 24.0, accent()).clicked && m.can_toggle {
            ui.out.push(Command::MediaToggle);
        }
        if ui.icon_button(id("media.next", 0), cells[2].centered(44.0, 44.0), Icon::Next, 22.0, dim(m.can_next)).clicked && m.can_next {
            ui.out.push(Command::MediaNext);
        }

        let pos = st.media_position(ui.now_ms);
        let band = rest.take_top(18.0, 0.0);
        let bar = band.centered(band.w, 4.0);
        ui.progress(bar, pos, m.duration_ms, playing, accent());
        let resp = ui.interact(id("media.seek", 0), band);
        if resp.clicked && m.duration_ms > 0 {
            if let Some(p) = ui.input.mouse {
                let frac = ((p.x - bar.x) / bar.w).clamp(0.0, 1.0);
                ui.out.push(Command::MediaSeek((frac as f64 * m.duration_ms as f64) as u64));
            }
        }

        let labels = rest.take_top(18.0, 0.0);
        let small = TextStyle::new(11.5, 400, TEXT_FAINT);
        ui.text(labels, &format_time(pos), small);
        ui.text(labels, &format_time(m.duration_ms), small.aligned(Align::End));
        if playing {
            ui.redraw_next_second(pos);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    #[test]
    fn seen_apps_are_added_once() {
        let mut seen = vec!["Spotify.exe".to_string()];
        assert!(merge_seen(&mut seen, &["Chrome".into(), "Spotify.exe".into(), String::new()]));
        assert_eq!(seen, ["Spotify.exe", "Chrome"]);
        assert!(!merge_seen(&mut seen, &["Chrome".into()]));
    }

    #[test]
    fn preferred_defaults_to_first_enabled_app() {
        let c = Config { media_seen: vec!["a".into(), "b".into()], media_disabled: vec!["a".into()], ..Config::default() };
        assert_eq!(first_enabled(&c), "b");
        assert_eq!(prefs(&c).disabled, ["a"]);
        assert!(prefs(&Config::default()).preferred.is_empty());
    }
}