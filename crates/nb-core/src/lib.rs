//! Shared contract between the naelbar shell and its background services.
//!
//! Services never touch the UI. They push [`Event`]s through a [`Sink`], which
//! the shell implements by queueing the event and waking its message loop.

use std::path::PathBuf;
use std::sync::Arc;

/// Delivers an event to the shell. Callable from any thread, never blocks.
pub type Sink = Arc<dyn Fn(Event) + Send + Sync + 'static>;

#[derive(Debug, Clone)]
pub enum Event {
    /// Room of the YouTube Music extension's remote, reported by the
    /// extension when its tab opens one (`ytm-` and 24 hex digits).
    YtmRoom(String),
    /// A newer version replaced the executable: the island restarts.
    UpdateReady(String),
    /// `None` when no media session exists any more.
    Media(Option<MediaState>),
    /// App ids (AUMID) of the media sessions that exist now, sorted.
    MediaApps(Vec<String>),
    Audio(AudioState),
    ClipboardAdded(ClipItem),
    Power(PowerState),
    Radios(RadioState),
    /// Brightness of the first screen in [`Event::Screens`], for the HUD.
    Brightness(u8),
    /// Every screen whose brightness can be set, the built-in panel first.
    Screens(Vec<ScreenBrightness>),
    Foreground(ForegroundState),
    /// Only emitted while a [`nb-system`] stats subscription is active.
    Stats(SystemStats),
    /// Only emitted while the stats detail panel is open.
    Processes(Vec<ProcessUse>),
    Notification(Notification),
    Agent(AgentUpdate),
    Weather(Weather),
    ClaudeUsage(ClaudeUsage),
    ClaudeHook(ClaudeHook),
    ClaudeLink(ClaudeLink),
    /// SteelSeries Sonar channels; empty when Sonar is not reachable.
    Sonar(Vec<SonarChannel>),
    /// A lock key changed state. Only emitted while a lock key watch is alive.
    LockKey(LockKey, bool),
    /// A browser download in the Downloads folder changed.
    Download(DownloadEvent),
    /// Every application using the microphone, the camera or screen capture
    /// right now. Replaces the previous list.
    Capture(Vec<CaptureUse>),
    /// Battery levels of Bluetooth and USB peripherals. Replaces the list.
    Devices(Vec<DeviceBattery>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceKind {
    Headset,
    Mouse,
    Keyboard,
    Controller,
    Other,
}

/// One peripheral that reports its battery level to Windows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceBattery {
    pub name: String,
    pub kind: DeviceKind,
    pub percent: u8,
}

#[derive(Debug, Clone, PartialEq)]
pub enum DownloadEvent {
    /// `name` is empty while the browser has not chosen the file name yet.
    Progress { id: u64, name: String, bytes: u64 },
    Done { id: u64, name: String, path: PathBuf, bytes: u64 },
    Cancelled { id: u64 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureKind {
    Microphone,
    Camera,
    Screen,
}

/// One application using a capture device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaptureUse {
    pub kind: CaptureKind,
    /// Executable file name, or package family name for a Store application.
    pub id: String,
    /// Readable name derived from `id`.
    pub name: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockKey {
    Caps,
    Num,
    Scroll,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ScreenBrightness {
    /// 0 for the built-in panel, then 1 + the position of the external monitor.
    pub id: u32,
    pub name: String,
    /// 0 to 100.
    pub percent: u8,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SonarChannel {
    pub id: String,
    /// 0 to 1.
    pub volume: f32,
    pub muted: bool,
}

/// Whether the island can read the subscription limits by itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ClaudeLink {
    #[default]
    Unknown,
    Connected,
    NeedsLogin,
    Failed,
}

/// One Claude subscription limit.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct UsageWindow {
    /// 0 to 100.
    pub used_percent: f32,
    /// Unix milliseconds at which the window resets.
    pub resets_at_ms: u64,
}

/// Limits reported by Claude Code. A window can be missing on its own.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ClaudeUsage {
    pub five_hour: Option<UsageWindow>,
    pub seven_day: Option<UsageWindow>,
    pub received_at_ms: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ClaudeHookKind {
    SessionStart,
    SessionEnd,
    /// A prompt or a tool ran, so nothing is waiting any more.
    Progress,
    PermissionRequest { tool: String },
    Stop,
    SubagentStart { agent_type: String },
    SubagentStop,
}

/// A Claude Code hook, reduced to what the island shows.
#[derive(Debug, Clone, PartialEq)]
pub struct ClaudeHook {
    pub session: String,
    /// Last folder of the working directory.
    pub project: String,
    /// Set when the hook came from inside a subagent.
    pub agent: Option<String>,
    pub kind: ClaudeHookKind,
    pub at_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlaybackStatus {
    Playing,
    Paused,
    Stopped,
}

#[derive(Debug, Clone)]
pub struct MediaState {
    pub app_id: String,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub status: PlaybackStatus,
    pub position_ms: u64,
    pub duration_ms: u64,
    /// Unix milliseconds at which `position_ms` was sampled, so the shell can
    /// extrapolate the position only while it is actually drawing it.
    pub position_sampled_at_ms: u64,
    /// Encoded image bytes (PNG or JPEG), decoded by the shell.
    pub artwork: Option<Arc<[u8]>>,
    pub can_previous: bool,
    pub can_next: bool,
    pub can_toggle: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioDevice {
    pub id: String,
    pub name: String,
}

#[derive(Debug, Clone)]
pub struct AudioState {
    /// 0.0 to 1.0.
    pub output_volume: f32,
    pub output_muted: bool,
    /// 0.0 to 1.0.
    pub mic_volume: f32,
    pub mic_muted: bool,
    /// True while any application holds a capture stream open.
    pub mic_in_use: bool,
    pub output: Option<AudioDevice>,
    pub input: Option<AudioDevice>,
    pub outputs: Vec<AudioDevice>,
    pub inputs: Vec<AudioDevice>,
}

#[derive(Debug, Clone)]
pub enum ClipContent {
    Text(String),
    Image { png: Arc<[u8]>, width: u32, height: u32 },
    Files(Vec<PathBuf>),
}

#[derive(Debug, Clone)]
pub struct ClipItem {
    pub id: u64,
    pub content: ClipContent,
    pub captured_at_ms: u64,
    pub pinned: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PowerMode {
    Efficiency,
    Balanced,
    Performance,
}

#[derive(Debug, Clone, Copy)]
pub struct PowerState {
    /// `None` on machines without a battery.
    pub battery_percent: Option<u8>,
    pub charging: bool,
    pub mode: PowerMode,
    /// Estimated time left on battery, when Windows knows it.
    pub seconds_left: Option<u32>,
}

#[derive(Debug, Clone, Copy)]
pub struct RadioState {
    pub wifi: Option<bool>,
    pub bluetooth: Option<bool>,
}

#[derive(Debug, Clone)]
pub struct ForegroundState {
    pub exe: String,
    pub fullscreen: bool,
    /// True when the desktop or the taskbar is in front, no application.
    pub desktop: bool,
}

#[derive(Debug, Clone, Copy)]
pub struct SystemStats {
    pub cpu_percent: f32,
    pub gpu_percent: Option<f32>,
    pub gpu_temp_c: Option<f32>,
    /// Filled only while the usage panel is open.
    pub gpu_mhz: Option<u32>,
    pub cpu_mhz: Option<u32>,
    pub cpu_temp_c: Option<f32>,
    pub ram_percent: f32,
    pub ram_used_bytes: u64,
    pub ram_total_bytes: u64,
    pub net_down_bps: u64,
    pub net_up_bps: u64,
}

/// One application, all its processes added together.
#[derive(Debug, Clone, PartialEq)]
pub struct ProcessUse {
    pub name: String,
    pub cpu_percent: f32,
    pub ram_bytes: u64,
}

#[derive(Debug, Clone)]
pub struct Notification {
    pub source: String,
    pub title: String,
    pub body: String,
    pub received_at_ms: u64,
    /// Application user model id of the sender, empty when unknown.
    pub app_id: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentStatus {
    Working,
    NeedsInput,
    Finished,
}

#[derive(Debug, Clone)]
pub struct AgentUpdate {
    pub agent: String,
    pub session: String,
    pub status: AgentStatus,
    pub detail: String,
}

#[derive(Debug, Clone)]
pub struct Weather {
    pub temperature_c: f32,
    pub min_c: f32,
    pub max_c: f32,
    pub condition: String,
}
