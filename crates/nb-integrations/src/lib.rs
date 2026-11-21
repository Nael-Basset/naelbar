//! Integrations: local notify endpoint, agent hooks, toast mirror, weather.
//!
//! Idle rule: the endpoint is a single thread parked in a blocking `accept`.
//! The toast mirror is a WinRT event. Weather runs only when the caller asks.

mod claude;
mod endpoint;
mod hooks;
mod http;
mod toast;
mod weather;
mod steelseries;
mod wpn;

use std::fmt;

pub use claude::statusline_forward;
pub use endpoint::{Config, DEFAULT_PORT, Handle, start};
pub use hooks::{MergeReport, StatusLineOutcome, claude_code_hook_snippet, merge_claude_code_settings};
pub use toast::{ToastMirror, start_toast_mirror};
pub use steelseries::{sonar_refresh, sonar_set_volume};
pub use wpn::{WpnMirror, is_ignored_app, start_wpn_mirror};
pub use weather::{fetch_weather, fetch_weather_blocking, geocode_blocking, wmo_label_fr};

#[derive(Debug)]
pub enum Error {
    Io(std::io::Error),
    /// The OS refused the feature (for example toast access without package identity).
    Unsupported(String),
    Network(String),
    Json(String),
    Os(windows::core::Error),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(e) => write!(f, "io: {e}"),
            Error::Unsupported(s) => write!(f, "unsupported: {s}"),
            Error::Network(s) => write!(f, "network: {s}"),
            Error::Json(s) => write!(f, "json: {s}"),
            Error::Os(e) => write!(f, "os: {e}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}
impl From<windows::core::Error> for Error {
    fn from(e: windows::core::Error) -> Self {
        Error::Os(e)
    }
}

pub(crate) fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
