//! Application state, fed by `nb_core::Event`, and the command enum views emit.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use nb_core::{
    AgentUpdate, AudioState, ClipItem, Event, ForegroundState, MediaState, Notification, PlaybackStatus, PowerState,
    ProcessUse, RadioState, SystemStats, Weather,
};

/// Everything views may ask the outside world (or the shell) to do.
#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    MediaToggle,
    MediaNext,
    MediaPrevious,
    MediaSeek(u64),
    /// 0.0 to 1.0.
    SetVolume(f32),
    SetMuted(bool),
    SetMicMuted(bool),
    /// 0 to 100.
    /// Screen id from `Event::Screens`, then 0 to 100.
    SetBrightness(u32, u8),
    BrightnessRefresh,
    SetWifi(bool),
    SetBluetooth(bool),
    CopyClip(u64),
    ClearClips,
    PinClip(u64, bool),
    DeleteClip(u64),
    /// Stats are only sampled while the widgets tab is visible.
    StatsSubscribe(bool),
    /// Per-application figures, only gathered while the detail panel is open.
    StatsDetail(bool),
    // Handled by the shell itself, never reach `services.rs`.
    FocusToggle,
    FocusReset,
    ShelfRemove(usize),
    ShelfDragOut(usize),
    /// Drags a copied item out of the island: text, files or the image as a PNG file.
    ClipDragOut(u64),
    /// Serves this shelf item to a phone on the same network.
    ShelfShare(usize),
    ShelfShareStop,
    /// Index of the settings section to show first.
    OpenSettings(usize),
    OpenPowerSettings,
    /// Adds an application to the ignore list (hides its mic timer and indicators).
    IgnoreApp(String),
    SonarRefresh,
    SonarVolume(String, f32),
    /// Forgets a Claude approval that will never be answered.
    ClaudeDismiss(String),
    /// Opens the one-time sign-in window for claude.ai.
    ClaudeLogin,
    /// Shows a copied image full size for as long as the button stays down.
    PreviewClip(u64),
    /// Switches to the agenda tab.
    OpenAgenda,
    /// Opens the large agenda window (`--agenda-window`).
    OpenAgendaWindow,
    /// Opens the editor for a new event on this `YYYY-MM-DD` day.
    AgendaNewEvent(String),
    AgendaNewNote,
    /// Opens the text window `--edit=<spec>`, for example `countdown` or `calc`.
    OpenEditor(String),
    AgendaDeleteEvent(String),
    AgendaDeleteNote(String),
    AgendaToggleNote(String),
    /// Opens the one-time sign-in window of the hosted agenda.
    AgendaLogin,
    /// Opens the application that sent a mirrored notification (its AUMID).
    OpenNotification(String),
    ClearNotifications,
    /// Reads the peripheral battery levels again (throttled).
    RefreshBatteries,
    /// Opens the Downloads folder with this finished download selected.
    OpenDownload(PathBuf),
}

pub const MAX_CLIPS: usize = 64;
pub const MAX_NOTIFICATIONS: usize = 24;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FocusPhase {
    Work,
    Break,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FocusRun {
    Idle,
    Running { ends_at_ms: u64 },
    Paused { remaining_ms: u64 },
}

#[derive(Debug, Clone, Copy)]
pub struct FocusTimer {
    pub phase: FocusPhase,
    pub run: FocusRun,
    work_ms: u64,
    break_ms: u64,
}

impl Default for FocusTimer {
    fn default() -> Self {
        Self { phase: FocusPhase::Work, run: FocusRun::Idle, work_ms: 25 * 60_000, break_ms: 5 * 60_000 }
    }
}

impl FocusTimer {
    pub fn phase_len(&self) -> u64 {
        match self.phase {
            FocusPhase::Work => self.work_ms,
            FocusPhase::Break => self.break_ms,
        }
    }

    pub fn remaining(&self, now_ms: u64) -> u64 {
        match self.run {
            FocusRun::Idle => self.phase_len(),
            FocusRun::Running { ends_at_ms } => ends_at_ms.saturating_sub(now_ms),
            FocusRun::Paused { remaining_ms } => remaining_ms,
        }
    }

    pub fn deadline(&self) -> Option<u64> {
        match self.run {
            FocusRun::Running { ends_at_ms } => Some(ends_at_ms),
            _ => None,
        }
    }

    pub fn is_running(&self) -> bool {
        matches!(self.run, FocusRun::Running { .. })
    }

    /// Advance across a phase boundary if the deadline passed. Work rolls into
    /// a running break; a finished break returns to idle.
    pub fn advance(&mut self, now_ms: u64) {
        if let FocusRun::Running { ends_at_ms } = self.run {
            if now_ms >= ends_at_ms {
                match self.phase {
                    FocusPhase::Work => {
                        self.phase = FocusPhase::Break;
                        self.run = FocusRun::Running { ends_at_ms: ends_at_ms + self.break_ms };
                    }
                    FocusPhase::Break => {
                        self.phase = FocusPhase::Work;
                        self.run = FocusRun::Idle;
                    }
                }
            }
        }
    }

    pub fn toggle(&mut self, now_ms: u64) {
        self.run = match self.run {
            FocusRun::Idle => FocusRun::Running { ends_at_ms: now_ms + self.phase_len() },
            FocusRun::Running { ends_at_ms } => FocusRun::Paused { remaining_ms: ends_at_ms.saturating_sub(now_ms) },
            FocusRun::Paused { remaining_ms } => FocusRun::Running { ends_at_ms: now_ms + remaining_ms },
        };
    }

    pub fn reset(&mut self) {
        self.phase = FocusPhase::Work;
        self.run = FocusRun::Idle;
    }

    /// Takes effect on the next phase. A running countdown is left alone.
    pub fn set_durations(&mut self, work_minutes: u32, break_minutes: u32) {
        self.work_ms = work_minutes as u64 * 60_000;
        self.break_ms = break_minutes as u64 * 60_000;
    }
}

#[derive(Default)]
pub struct AppState {
    pub media: Option<MediaState>,
    pub audio: Option<AudioState>,
    pub brightness: Option<u8>,
    /// One entry per screen whose brightness can be set.
    pub screens: Vec<nb_core::ScreenBrightness>,
    pub power: Option<PowerState>,
    pub radios: Option<RadioState>,
    pub foreground: Option<ForegroundState>,
    pub stats: Option<SystemStats>,
    pub weather: Option<Weather>,
    /// True once a city is set, so the tile can tell "loading" from "not set".
    pub weather_configured: bool,
    pub sonar: Vec<nb_core::SonarChannel>,
    pub processes: Vec<ProcessUse>,
    pub claude: crate::claude::ClaudeState,
    /// Unix ms at which an application started using the microphone.
    pub mic_since: Option<u64>,
    /// Newest first, deduplicated by id, capped at [`MAX_CLIPS`].
    pub clips: VecDeque<ClipItem>,
    /// Newest first, capped at [`MAX_NOTIFICATIONS`].
    pub notifications: VecDeque<Notification>,
    pub agents: Vec<AgentUpdate>,
    pub shelf: Vec<PathBuf>,
    pub focus: FocusTimer,
    pub agenda: crate::agenda::Agenda,
    /// State of the link with the hosted agenda, from the last sync process.
    pub agenda_link: nb_core::ClaudeLink,
    /// Transient volume, brightness, battery, lock key and microphone HUDs.
    pub hud: crate::hud::HudState,
    /// When the newest notification arrived here, and how long the closed
    /// island shows it (0 shows nothing).
    pub notice_at: u64,
    pub notice_ms: u64,
    /// Peripheral battery levels, read only while their widget is shown.
    pub batteries: crate::widgets::batteries::Batteries,
    /// Downloads and the microphone, camera and screen capture indicators.
    pub activity: crate::activity::ActivityState,
}

impl AppState {
    /// When the microphone timer started, unless only ignored apps use the mic.
    pub fn mic_timer(&self) -> Option<u64> {
        if self.activity.mic_users_all_ignored() { None } else { self.mic_since }
    }

    pub fn apply(&mut self, event: Event) {
        match event {
            Event::Media(m) => self.media = m,
            // Remembered in the config by the shell, see `views::media::merge_seen`.
            Event::MediaApps(_) => {}
            Event::Audio(a) => {
                if !a.mic_in_use {
                    self.mic_since = None;
                } else if self.mic_since.is_none() {
                    self.mic_since = Some(unix_ms());
                }
                self.hud.on_audio(self.audio.as_ref(), &a, unix_ms());
                self.audio = Some(a);
            }
            Event::ClipboardAdded(item) => {
                self.clips.retain(|c| c.id != item.id);
                self.clips.push_front(item);
                // Pinned items are never pushed out by the cap.
                let mut kept = 0;
                self.clips.retain(|c| {
                    kept += 1;
                    c.pinned || kept <= MAX_CLIPS
                });
            }
            Event::Power(p) => {
                self.hud.on_power(self.power, &p, unix_ms());
                self.power = Some(p);
            }
            Event::Radios(r) => self.radios = Some(r),
            Event::Brightness(b) => {
                self.hud.on_brightness(self.brightness, b, unix_ms());
                self.brightness = Some(b);
            }
            Event::Screens(s) => self.screens = s,
            // Handed to the phone link by the island, not kept here.
            Event::YtmRoom(_) | Event::UpdateReady(_) => {}
            Event::Foreground(f) => self.foreground = Some(f),
            Event::Stats(s) => self.stats = Some(s),
            Event::Processes(p) => self.processes = p,
            Event::Notification(n) => {
                self.notifications.push_front(n);
                self.notifications.truncate(MAX_NOTIFICATIONS);
                self.notice_at = unix_ms();
            }
            Event::Agent(a) => match self.agents.iter_mut().find(|x| x.agent == a.agent && x.session == a.session) {
                Some(slot) => *slot = a,
                None => self.agents.push(a),
            },
            Event::Weather(w) => self.weather = Some(w),
            Event::Sonar(s) => self.sonar = s,
            Event::ClaudeUsage(u) => self.claude.on_usage(u),
            Event::ClaudeHook(h) => self.claude.on_hook(h),
            Event::ClaudeLink(l) => self.claude.link = l,
            Event::LockKey(k, on) => self.hud.on_lock(k, on, unix_ms()),
            Event::Download(d) => self.activity.apply_download(d, unix_ms()),
            Event::Capture(c) => self.activity.apply_capture(c),
            Event::Devices(d) => {
                self.hud.on_devices(&d, unix_ms());
                self.batteries.update(d, unix_ms());
            }
        }
    }

    /// The notification the closed island shows at `now_ms`, and when it stops.
    pub fn notice(&self, now_ms: u64) -> Option<(&Notification, u64)> {
        let until = self.notice_at + self.notice_ms;
        if self.notice_ms == 0 || now_ms >= until {
            return None;
        }
        self.notifications.front().map(|n| (n, until))
    }

    /// Extrapolated playback position at `now_ms`.
    pub fn media_position(&self, now_ms: u64) -> u64 {
        match &self.media {
            Some(m) if m.status == PlaybackStatus::Playing => {
                let p = m.position_ms + now_ms.saturating_sub(m.position_sampled_at_ms);
                if m.duration_ms > 0 { p.min(m.duration_ms) } else { p }
            }
            Some(m) => m.position_ms,
            None => 0,
        }
    }
}

pub fn unix_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn audio(mic_in_use: bool) -> Event {
        Event::Audio(AudioState {
            output_volume: 0.5,
            output_muted: false,
            mic_volume: 0.5,
            mic_muted: false,
            mic_in_use,
            output: None,
            input: None,
            outputs: Vec::new(),
            inputs: Vec::new(),
        })
    }

    #[test]
    fn a_new_notification_is_shown_for_the_configured_time_only() {
        let mut st = AppState { notice_ms: 5000, ..Default::default() };
        assert!(st.notice(unix_ms()).is_none());
        st.apply(Event::Notification(Notification { source: "Mail".into(), title: "Hi".into(), body: String::new(), received_at_ms: 0, app_id: "m".into() }));
        let at = st.notice_at;
        let (n, until) = st.notice(at + 100).expect("shown");
        assert_eq!((n.title.as_str(), until), ("Hi", at + 5000));
        assert!(st.notice(at + 5000).is_none(), "gone once the time is up");
        st.notice_ms = 0;
        assert!(st.notice(at).is_none(), "zero shows nothing");
    }

    #[test]
    fn mic_timer_starts_once_and_stops_with_the_mic() {
        let mut st = AppState::default();
        st.apply(audio(false));
        assert_eq!(st.mic_since, None);
        st.apply(audio(true));
        let started = st.mic_since.expect("timer started");
        st.mic_since = Some(started - 5000);
        st.apply(audio(true));
        assert_eq!(st.mic_since, Some(started - 5000), "a volume change must not restart it");
        st.apply(audio(false));
        assert_eq!(st.mic_since, None);
    }
}
