//! Call strip: shown at the top of the open card while an application holds
//! the microphone. It is not a tab. The app name comes from the Windows
//! privacy registry (read once when the island opens, never polled).

use windows::Win32::Foundation::ERROR_SUCCESS;
use windows::Win32::System::Registry::*;
use windows::core::{PCWSTR, w};

use crate::state::{AppState, Command};
use crate::ui::theme::*;
use crate::ui::{Align, Icon, Rect, TextStyle, Ui, id};

/// Extra card height while a call is in progress, in DIPs (strip plus gap).
pub const CALL_H: f32 = 40.0;
const STRIP_H: f32 = 36.0;
const MIC_GREEN: crate::ui::Color = crate::ui::Color::rgb(74, 222, 128);

/// Extra height the card gets for the strip. Zero when there is no call, or
/// when the taller card would not fit the visual window (`room` is the free
/// height below the island top, in DIPs, and `k` the card scale).
pub fn extra_height(in_call: bool, k: f32, room: f32) -> f32 {
    if in_call && (270.0 + CALL_H) * k <= room { CALL_H } else { 0.0 }
}

/// Draws the strip in `r` and returns the mute command when the button is hit.
pub fn draw(ui: &mut Ui, st: &AppState, r: Rect, app: Option<&str>) {
    let r = Rect::new(r.x, r.y, r.w, STRIP_H);
    ui.fill(r, R_TILE, TILE);
    let muted = st.audio.as_ref().is_some_and(|a| a.mic_muted);
    let since = st.mic_timer().unwrap_or(ui.now_ms);
    let elapsed = ui.now_ms.saturating_sub(since);

    ui.icon(Icon::Mic, Rect::new(r.x + 10.0, r.center().y - 9.0, 18.0, 18.0), if muted { TEXT_FAINT } else { MIC_GREEN });
    let name = app.filter(|s| !s.is_empty()).unwrap_or(crate::ui::tr("Micro en cours d'utilisation", "Microphone in use"));
    let name = fit(name, 17);
    ui.text(Rect::new(r.x + 36.0, r.y + 3.0, 160.0, 16.0), &name, TextStyle::new(11.5, 500, TEXT));
    ui.text(Rect::new(r.x + 36.0, r.y + 19.0, 160.0, 14.0), &crate::ui::format_time(elapsed), TextStyle::new(10.5, 400, TEXT_DIM));
    ui.redraw_next_second(elapsed);

    if let Some(app) = app.filter(|s| !s.is_empty()) {
        let link = Rect::new(r.right() - 178.0, r.y + 9.0, 64.0, 18.0);
        let resp = ui.interact(id("call.ignore", 0), link);
        ui.text(link, crate::ui::tr("Ignorer", "Ignore"), TextStyle::new(10.5, 500, if resp.hovered { TEXT } else { TEXT_DIM }).aligned(Align::Center));
        if resp.clicked {
            ui.out.push(Command::IgnoreApp(app.to_string()));
        }
    }

    let btn = Rect::new(r.right() - 108.0, r.y + 4.0, 100.0, STRIP_H - 8.0);
    let resp = ui.interact(id("call.mute", 0), btn);
    let (bg, fg) = if muted {
        (TEXT, SURFACE)
    } else if resp.hovered {
        (TILE_HOVER, TEXT)
    } else {
        (HAIRLINE, TEXT)
    };
    ui.fill(btn, R_SMALL, bg);
    ui.icon(Icon::Mic, Rect::new(btn.x + 8.0, btn.center().y - 8.0, 16.0, 16.0), fg);
    let label = if muted { crate::ui::tr("Réactiver", "Unmute") } else { crate::ui::tr("Couper", "Mute") };
    ui.text(Rect::new(btn.x + 28.0, btn.y, btn.w - 34.0, btn.h), label, TextStyle::new(11.5, 600, fg).aligned(Align::Center));
    if resp.clicked {
        ui.out.push(Command::SetMicMuted(!muted));
    }
}

/// Cuts a name to `max` characters with an ellipsis.
fn fit(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut t: String = s.chars().take(max.saturating_sub(1)).collect();
        t.push('…');
        t
    }
}

/// A capture is live when it started and has not stopped yet.
pub fn in_use(start: u64, stop: u64) -> bool {
    start > 0 && stop == 0
}

/// Readable name from a ConsentStore key: a `#`-escaped path for classic
/// programs, a package family name for store apps.
pub fn pretty_app(key: &str) -> String {
    if key.contains('#') {
        let path = key.replace('#', "\\");
        let file = path.rsplit('\\').next().unwrap_or(&path);
        return file.rsplit_once('.').map_or(file, |(stem, _)| stem).to_string();
    }
    let family = key.split('_').next().unwrap_or(key);
    family.rsplit('.').next().unwrap_or(family).to_string()
}

const BASE: PCWSTR = w!("Software\\Microsoft\\Windows\\CurrentVersion\\CapabilityAccessManager\\ConsentStore\\microphone");

fn read_u64(key: HKEY, name: PCWSTR) -> u64 {
    let mut v = 0u64;
    let mut size = 8u32;
    let ok = unsafe { RegQueryValueExW(key, name, None, None, Some(&mut v as *mut u64 as *mut u8), Some(&mut size)) };
    if ok == ERROR_SUCCESS { v } else { 0 }
}

fn subkeys(key: HKEY) -> Vec<String> {
    let mut out = Vec::new();
    for i in 0.. {
        let mut buf = [0u16; 512];
        let mut len = buf.len() as u32;
        let r = unsafe { RegEnumKeyExW(key, i, Some(windows::core::PWSTR(buf.as_mut_ptr())), &mut len, None, None, None, None) };
        if r != ERROR_SUCCESS {
            break;
        }
        out.push(String::from_utf16_lossy(&buf[..len as usize]));
    }
    out
}

/// Name of the application using the microphone now, when Windows says so.
/// A handful of registry reads; called when the card opens during a call.
pub fn mic_app() -> Option<String> {
    let mut best: Option<(u64, String)> = None;
    unsafe {
        let mut root = HKEY::default();
        if RegOpenKeyExW(HKEY_CURRENT_USER, BASE, None, KEY_READ, &mut root) != ERROR_SUCCESS {
            return None;
        }
        let mut consider = |parent: HKEY, name: &str| {
            let wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
            let mut k = HKEY::default();
            if RegOpenKeyExW(parent, PCWSTR(wide.as_ptr()), None, KEY_READ, &mut k) != ERROR_SUCCESS {
                return;
            }
            let start = read_u64(k, w!("LastUsedTimeStart"));
            let stop = read_u64(k, w!("LastUsedTimeStop"));
            let _ = RegCloseKey(k);
            if in_use(start, stop) && best.as_ref().is_none_or(|b| start > b.0) {
                best = Some((start, pretty_app(name)));
            }
        };
        for name in subkeys(root) {
            if name == "NonPackaged" {
                let mut np = HKEY::default();
                if RegOpenKeyExW(root, w!("NonPackaged"), None, KEY_READ, &mut np) == ERROR_SUCCESS {
                    for n in subkeys(np) {
                        consider(np, &n);
                    }
                    let _ = RegCloseKey(np);
                }
            } else {
                consider(root, &name);
            }
        }
        let _ = RegCloseKey(root);
    }
    best.map(|b| b.1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_readable() {
        assert_eq!(pretty_app("C:#Program Files#Zoom#bin#Zoom.exe"), "Zoom");
        assert_eq!(pretty_app("Microsoft.WindowsCamera_8wekyb3d8bbwe"), "WindowsCamera");
        assert_eq!(pretty_app("Discord"), "Discord");
    }

    #[test]
    fn live_capture_only() {
        assert!(in_use(5, 0));
        assert!(!in_use(5, 9));
        assert!(!in_use(0, 0));
    }

    #[test]
    fn extra_height_needs_a_call_and_room() {
        assert_eq!(extra_height(false, 1.0, 400.0), 0.0);
        assert_eq!(extra_height(true, 1.0, 394.0), CALL_H);
        assert_eq!(extra_height(true, 1.25, 340.0), 0.0);
    }

    #[test]
    fn long_names_are_cut() {
        assert_eq!(fit("short", 17), "short");
        assert_eq!(fit("a very long application name", 10).chars().count(), 10);
    }
}
