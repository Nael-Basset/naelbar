//! Page two of the widgets tab and what it needs: countdowns, elapsed time,
//! a calculator with unit conversions, and a read-only .ics agenda.

pub mod batteries;
pub mod calc;
pub mod civil;
pub mod clip;
pub mod countdown;
pub mod elapsed;
pub mod ics;
pub mod page2;
pub mod store;

use windows::Win32::Foundation::{LPARAM, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{FindWindowW, PostMessageW, WM_APP};
use windows::core::w;

/// Posted to the island by the editor once a widgets file has changed.
pub const WM_WIDGETS_CHANGED: u32 = WM_APP + 60;
/// Posted by the download thread once the .ics events were refreshed.
pub const WM_ICS_READY: u32 = WM_APP + 61;

/// Tells the island window something changed.
pub fn notify_island(msg: u32) {
    unsafe {
        if let Ok(island) = FindWindowW(w!("naelbar.window"), w!("naelbar control")) {
            let _ = PostMessageW(Some(island), msg, WPARAM(0), LPARAM(0));
        }
    }
}

/// Applies the settings this module cares about.
pub fn on_config(c: &crate::config::Config) {
    ics::set_source(&c.ics_url);
    ics::set_styles(&c.course_styles);
}

/// The widgets files were changed by another process.
pub fn on_files_changed() {
    store::invalidate();
}
