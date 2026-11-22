//! `%APPDATA%\naelbar\config.json`.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use windows::Win32::Globalization::GetUserDefaultUILanguage;
use windows::Win32::UI::WindowsAndMessaging::WM_APP;

use crate::ui::theme::{DEFAULT_ACCENT, parse_hex, set_accent};

/// Posted by the settings process when the file has changed.
pub const WM_CONFIG_CHANGED: u32 = WM_APP + 4;

const LANG_FRENCH: u16 = 0x0C;

/// Applies the settings that live in process-wide state.
pub fn apply_appearance(c: &Config) {
    set_accent(parse_hex(&c.accent).unwrap_or(DEFAULT_ACCENT));
    crate::ui::theme::set_glass(c.surface == "glass", c.glass_opacity);
    let english = match c.language.as_str() {
        "fr" => false,
        "en" => true,
        _ => unsafe { GetUserDefaultUILanguage() & 0x3FF != LANG_FRENCH },
    };
    crate::ui::set_english(english);
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Hotkeys {
    pub toggle: String,
    pub play_pause: String,
    pub timer: String,
    pub hide: String,
    pub mic: String,
    pub game_strip: String,
}

impl Default for Hotkeys {
    fn default() -> Self {
        Self {
            toggle: "Ctrl+Alt+N".into(),
            play_pause: "Ctrl+Alt+P".into(),
            timer: "Ctrl+Alt+T".into(),
            hide: "Ctrl+Alt+H".into(),
            mic: "Ctrl+Alt+M".into(),
            game_strip: "Ctrl+Alt+G".into(),
        }
    }
}

/// How lessons of the .ics timetable look: every lesson of a subject, or one
/// lesson when `date` is set.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct CourseStyle {
    /// Cleaned title, such as "Anglais 5".
    pub title: String,
    /// `YYYY-MM-DD` and `HH:MM` of the one lesson this applies to; empty for all.
    pub date: String,
    pub time: String,
    /// `#RRGGBB`, empty for the default look.
    pub color: String,
    pub hidden: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub last_tab: String,
    pub hotkeys: Hotkeys,
    /// Latitude and longitude for the weather widget. None disables it.
    pub weather_location: Option<(f64, f64)>,
    /// Accent color as `#RRGGBB`.
    pub accent: String,
    /// `auto`, `fr` or `en`.
    pub language: String,
    /// `left`, `center` or `right`.
    pub position: String,
    pub edge_margin: f32,
    pub top_distance: f32,
    /// Closed size of the notch shape, in DIPs.
    pub notch_width: f32,
    pub notch_height: f32,
    /// `island` or `notch`.
    pub shape: String,
    /// Device name of the display, empty for the primary one.
    pub monitor: String,
    /// `chosen` for one island, `all` for one island per display.
    pub islands: String,
    /// `nothing`, `clock` or `date`.
    pub idle_show: String,
    pub pill_scale: f32,
    pub card_scale: f32,
    /// 0.0 is square, 1.0 is the full radius.
    pub corners: f32,
    /// `opaque` or `glass`.
    pub surface: String,
    /// Glass opacity, 0.6 to 1.0.
    pub glass_opacity: f32,
    /// `calm`, `normal` or `fast`.
    pub anim_speed: String,
    /// `smooth`, `balanced` or `saver`.
    pub motion: String,
    pub weather_city: String,
    pub focus_minutes: u32,
    pub break_minutes: u32,
    /// Web address or file path of an .ics calendar shown read-only in the agenda.
    pub ics_url: String,
    /// Colour and visibility of the subjects of that calendar.
    pub course_styles: Vec<CourseStyle>,
    /// When those styles last changed here or on the phone, in Unix ms: the
    /// newer side wins when they meet.
    pub course_styles_at: u64,
    /// Folder that lesson scans from the phone are filed under; empty for
    /// `Downloads\naelbar`.
    pub courses_root: String,
    /// Leaves the island out of screenshots, recordings and screen sharing.
    pub hide_from_capture: bool,
    /// Transient HUDs in the closed island: `all` or `none`.
    pub hud: String,
    /// Caps, Num and Scroll Lock HUDs, which need a keyboard hook.
    pub hud_locks: bool,
    /// Low-battery and connection HUDs for Bluetooth peripherals.
    pub hud_devices: bool,
    /// New screenshots land on the shelf by themselves.
    pub shelf_captures: bool,
    /// Seconds a new notification stays in the closed island: 3, 5 or 8.
    pub notification_seconds: u32,
    /// Widget shown on the desktop by its own process: `none`, `focus`,
    /// `elapsed`, `batteries` or `clock`.
    pub desktop_widget: String,
    /// Executable names or application ids (AUMID) the island ignores: their
    /// notifications are not mirrored and they get no privacy indicator.
    #[serde(alias = "notification_ignore")]
    pub ignore_apps: Vec<String>,
    /// Shows browser downloads in the closed island.
    pub activity_downloads: bool,
    /// Shows the microphone, camera and screen capture indicators.
    pub activity_indicators: bool,
    /// Media apps (AUMID) seen so far, listed on the Music settings page.
    pub media_seen: Vec<String>,
    /// Media apps that never drive the music card.
    pub media_disabled: Vec<String>,
    /// Media app that wins while it plays. Empty: the most recent one.
    pub media_preferred: String,
    /// Pill visualizer: `bars`, `pixels` or `none`.
    pub visualizer: String,
    /// Stats strip over fullscreen applications.
    pub game_strip: bool,
    /// `left` or `right`, top corner.
    pub game_strip_corner: String,
    /// The six launcher slots at the top of the Dock tab.
    pub slots: Vec<crate::launcher::Slot>,
    /// Phone remote server (trackpad, remote, launcher) on the LAN. Off by default.
    pub phone_remote: bool,
    /// TCP port of the phone remote.
    pub phone_port: u16,
    /// Installs new releases from GitHub by itself. On by default.
    pub auto_update: bool,
    /// Island tabs the user hid, by name (`media`, `dock`…).
    pub hidden_tabs: Vec<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            last_tab: "media".into(),
            hotkeys: Hotkeys::default(),
            weather_location: None,
            accent: "#F5F5F5".into(),
            language: "auto".into(),
            position: "center".into(),
            edge_margin: 24.0,
            top_distance: 6.0,
            notch_width: 104.0,
            notch_height: 26.0,
            shape: "island".into(),
            monitor: String::new(),
            islands: "chosen".into(),
            idle_show: "nothing".into(),
            pill_scale: 1.0,
            card_scale: 1.0,
            corners: 1.0,
            surface: "opaque".into(),
            glass_opacity: 0.85,
            anim_speed: "normal".into(),
            motion: "smooth".into(),
            weather_city: String::new(),
            focus_minutes: 25,
            break_minutes: 5,
            ics_url: String::new(),
            course_styles: Vec::new(),
            course_styles_at: 0,
            courses_root: String::new(),
            hide_from_capture: false,
            hud: "all".into(),
            hud_locks: true,
            hud_devices: true,
            shelf_captures: true,
            notification_seconds: 5,
            desktop_widget: "none".into(),
            ignore_apps: Vec::new(),
            activity_downloads: true,
            activity_indicators: true,
            media_seen: Vec::new(),
            media_disabled: Vec::new(),
            media_preferred: String::new(),
            visualizer: "bars".into(),
            game_strip: false,
            game_strip_corner: "right".into(),
            slots: crate::launcher::default_slots(),
            phone_remote: false,
            phone_port: crate::phone::DEFAULT_PORT,
            auto_update: true,
            hidden_tabs: Vec::new(),
        }
    }
}

pub fn path() -> Option<PathBuf> {
    std::env::var_os("APPDATA").map(|d| PathBuf::from(d).join("naelbar").join("config.json"))
}

impl Config {
    pub fn load() -> Self {
        path()
            .and_then(|p| std::fs::read(p).ok())
            .and_then(|b| serde_json::from_slice::<Self>(&b).ok())
            .unwrap_or_default()
            .sanitized()
    }

    /// The file can be edited by hand, so no value is trusted.
    fn sanitized(mut self) -> Self {
        self.edge_margin = self.edge_margin.clamp(0.0, 200.0);
        self.top_distance = self.top_distance.clamp(0.0, 40.0);
        self.notch_width = self.notch_width.clamp(60.0, 240.0);
        self.notch_height = self.notch_height.clamp(14.0, 44.0);
        self.pill_scale = self.pill_scale.clamp(0.8, 1.5);
        self.card_scale = self.card_scale.clamp(0.8, 1.25);
        self.corners = self.corners.clamp(0.0, 1.0);
        self.glass_opacity = self.glass_opacity.clamp(0.6, 1.0);
        if !["opaque", "glass"].contains(&self.surface.as_str()) {
            self.surface = "opaque".into();
        }
        self.focus_minutes = self.focus_minutes.clamp(1, 180);
        self.break_minutes = self.break_minutes.clamp(1, 60);
        self.ics_url = self.ics_url.trim().chars().take(2048).collect();
        if !std::path::Path::new(self.courses_root.trim()).is_absolute() {
            self.courses_root.clear();
        }
        self.course_styles.retain(|s| !s.title.is_empty() && s.date.len() <= 10 && s.time.len() <= 5 && (s.hidden || parse_hex(&s.color).is_some()));
        self.course_styles.truncate(2000);
        self.slots = crate::launcher::sanitize(std::mem::take(&mut self.slots));
        if ![3, 5, 8].contains(&self.notification_seconds) {
            self.notification_seconds = 5;
        }
        if !crate::desktop::WIDGETS.contains(&self.desktop_widget.as_str()) {
            self.desktop_widget = "none".into();
        }
        if !["bars", "pixels", "none"].contains(&self.visualizer.as_str()) {
            self.visualizer = "bars".into();
        }
        self.media_seen.truncate(64);
        if self.phone_port < 1024 {
            self.phone_port = crate::phone::DEFAULT_PORT;
        }
        self
    }

    pub fn save(&self) {
        let Some(p) = path() else { return };
        if let Some(dir) = p.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        // Replaced in one step: the island may read it while a slider moves.
        if let Ok(json) = serde_json::to_vec_pretty(self) {
            let tmp = p.with_extension(format!("{}.tmp", std::process::id()));
            if std::fs::write(&tmp, json).is_ok() {
                let _ = std::fs::rename(&tmp, &p);
            }
        }
    }
}
