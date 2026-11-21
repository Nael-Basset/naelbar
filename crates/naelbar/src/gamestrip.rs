//! Game strip: a small click-through always-on-top window in a screen corner
//! with CPU, GPU, temperature, RAM and network, shown only while a fullscreen
//! application is in front and the setting is on. It exists only while shown;
//! the 1 s stats subscription runs only while it does.
//!
//! FPS of the foreground process comes from an ETW session
//! ([`nb_system::fps::FpsSession`]) that lives exactly as long as the strip.
//! It needs administrator or Performance Log Users rights; without them the
//! strip says so and a settings row explains the one-time fix.

use nb_core::SystemStats;
use windows::Win32::Foundation::HWND;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::w;

use crate::render::Renderer;
use crate::ui::theme::*;
use crate::ui::{Align, Input, Rect, TextStyle, Ui, WidgetState};

const W_DIP: f32 = 460.0;
const H_DIP: f32 = 26.0;
const MARGIN: f32 = 12.0;

/// Whether the strip is on screen: the setting (or the hotkey override) is on
/// and a fullscreen application is in front.
pub fn should_show(enabled: bool, fullscreen: bool) -> bool {
    enabled && fullscreen
}

/// Hotkey result: hides a shown strip, otherwise asks for it. `None` never.
pub fn hotkey_override(shown: bool) -> bool {
    !shown
}

/// `left` or `right` from the setting; anything else is the right corner.
pub fn on_left(corner: &str) -> bool {
    corner == "left"
}

/// `bps` counts bits, the label counts bytes.
pub fn rate(bps: u64, english: bool) -> String {
    let b = bps as f32 / 8.0;
    if b >= 1_000_000.0 {
        format!("{:.1} {}", b / 1_000_000.0, if english { "MB/s" } else { "Mo/s" })
    } else {
        format!("{:.0} {}", b / 1_000.0, if english { "kB/s" } else { "ko/s" })
    }
}

/// What the strip knows about the frame rate.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Fps {
    /// Session running, frames counted (or none yet).
    Rate(Option<f32>),
    /// The session could not start for lack of rights.
    NoRights,
    /// The session could not start for another reason.
    Unavailable,
}

/// The FPS part of the line.
pub fn fps_label(f: Fps) -> String {
    match f {
        Fps::Rate(Some(v)) => format!("FPS {v:.0}"),
        Fps::Rate(None) | Fps::Unavailable => "FPS -".to_string(),
        Fps::NoRights => crate::ui::tr("FPS : droits requis", "FPS: rights needed").to_string(),
    }
}

/// The single line shown in the strip.
pub fn line(s: &SystemStats, fps: Fps, english: bool) -> String {
    let gpu = match (s.gpu_percent, s.gpu_temp_c) {
        (Some(p), Some(t)) => format!("GPU {p:.0}% {t:.0}°C"),
        (Some(p), None) => format!("GPU {p:.0}%"),
        (None, _) => "GPU -".to_string(),
    };
    format!("{}   CPU {:.0}%   {gpu}   RAM {:.0}%   ↓ {}", fps_label(fps), s.cpu_percent, s.ram_percent, rate(s.net_down_bps, english))
}

pub struct GameStrip {
    hwnd: HWND,
    renderer: Renderer,
    last: String,
    fps: Result<nb_system::fps::FpsSession, u32>,
}

/// Process id of the foreground window, 0 when none.
fn foreground_pid() -> u32 {
    let mut pid = 0u32;
    unsafe {
        let _ = GetWindowThreadProcessId(GetForegroundWindow(), Some(&mut pid));
    }
    pid
}

impl GameStrip {
    /// Creates the window at a corner of the chosen monitor, without taking focus.
    pub fn create(monitor_name: &str, left: bool) -> Option<Self> {
        unsafe {
            let m = crate::monitors::pick(monitor_name)?;
            let (mut dx, mut dy) = (96u32, 96u32);
            let _ = GetDpiForMonitor(m.handle, MDT_EFFECTIVE_DPI, &mut dx, &mut dy);
            let scale = dx as f32 / 96.0;
            let (w, h) = ((W_DIP * scale).round() as i32, (H_DIP * scale).round() as i32);
            let margin = (MARGIN * scale).round() as i32;
            let x = if left { m.rect.left + margin } else { m.rect.right - w - margin };
            let y = m.rect.top + margin;
            let instance = GetModuleHandleW(None).ok()?;
            let ex = WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_NOREDIRECTIONBITMAP;
            let hwnd = CreateWindowExW(ex, w!("naelbar.window"), w!("naelbar game strip"), WS_POPUP, x, y, w, h, None, None, Some(instance.into()), None).ok()?;
            let _ = SetLayeredWindowAttributes(hwnd, windows::Win32::Foundation::COLORREF(0), 255, LWA_ALPHA);
            let mut renderer = Renderer::new(hwnd);
            renderer.set_size(w as u32, h as u32, scale);
            let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
            let fps = nb_system::fps::FpsSession::start(foreground_pid());
            Some(Self { hwnd, renderer, last: String::new(), fps })
        }
    }

    /// Repaints when the text changed.
    pub fn update(&mut self, stats: Option<&SystemStats>) {
        let english = crate::ui::is_english();
        let fps = match &self.fps {
            Ok(session) => {
                session.set_pid(foreground_pid());
                Fps::Rate(session.fps())
            }
            Err(code) if *code == nb_system::fps::ACCESS_DENIED => Fps::NoRights,
            Err(_) => Fps::Unavailable,
        };
        let text = stats.map_or_else(|| crate::ui::tr("Mesure…", "Reading…").to_string(), |s| line(s, fps, english));
        if text == self.last {
            return;
        }
        let input = Input::default();
        let mut widgets = WidgetState::default();
        let mut out = Vec::new();
        let scale = self.renderer_scale();
        let mut ui = Ui::new(&input, &mut widgets, &mut out, scale, 0);
        let r = Rect::new(0.0, 0.0, W_DIP, H_DIP);
        ui.fill(r, H_DIP * 0.5, SURFACE);
        ui.stroke(r.inset(0.5), H_DIP * 0.5, 1.0, HAIRLINE);
        ui.text(r, &text, TextStyle::new(11.0, 500, TEXT).aligned(Align::Center));
        let cmds = std::mem::take(&mut ui.cmds);
        drop(ui);
        if self.renderer.render(&cmds).is_ok() {
            self.last = text;
        }
    }

    fn renderer_scale(&self) -> f32 {
        let mut r = windows::Win32::Foundation::RECT::default();
        unsafe {
            let _ = GetWindowRect(self.hwnd, &mut r);
        }
        ((r.right - r.left) as f32 / W_DIP).max(1.0)
    }
}

impl Drop for GameStrip {
    fn drop(&mut self) {
        unsafe {
            let _ = DestroyWindow(self.hwnd);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stats() -> SystemStats {
        SystemStats {
            cpu_percent: 12.4,
            gpu_percent: Some(87.6),
            gpu_temp_c: Some(64.0),
            gpu_mhz: None,
            cpu_mhz: None,
            cpu_temp_c: None,
            ram_percent: 46.0,
            ram_used_bytes: 0,
            ram_total_bytes: 0,
            net_down_bps: 8_000_000,
            net_up_bps: 0,
        }
    }

    #[test]
    fn shows_only_when_enabled_and_fullscreen() {
        assert!(should_show(true, true));
        assert!(!should_show(true, false));
        assert!(!should_show(false, true));
    }

    #[test]
    fn hotkey_flips_visibility() {
        assert!(hotkey_override(false));
        assert!(!hotkey_override(true));
    }

    #[test]
    fn corner_setting() {
        assert!(on_left("left"));
        assert!(!on_left("right"));
        assert!(!on_left("junk"));
    }

    #[test]
    fn formats_line() {
        assert_eq!(line(&stats(), Fps::Rate(Some(143.6)), true), "FPS 144   CPU 12%   GPU 88% 64°C   RAM 46%   ↓ 1.0 MB/s");
        let mut s = stats();
        s.gpu_percent = None;
        assert!(line(&s, Fps::Rate(None), false).starts_with("FPS -   CPU"));
        assert!(line(&s, Fps::Unavailable, false).contains("GPU -"));
        assert_eq!(rate(80_000, false), "10 ko/s");
    }
}
