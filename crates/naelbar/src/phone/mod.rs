//! Phone link. The phone always uses the hosted site, one QR code in the
//! settings. When the switch is on, the island keeps a hidden helper process
//! (`--phone-link`, see `link`) that opens the same site as the "PC" side of a
//! WebRTC link: when the phone is on the same network, a direct, encrypted
//! data channel opens between the two, and the helper turns its messages into
//! mouse and keyboard input and saves the files sent.
//!
//! Only local addresses are offered (no STUN or TURN server), so nothing is
//! relayed over the internet. The meeting happens through two private
//! Firestore documents that only the signed-in account can read or write.
//!
//! Off by default. While off, nothing runs. While on, the helper sleeps in a
//! WebView2 waiting on its Firestore listener, and exits with the island.

pub mod input;
pub mod link;
pub mod page;

use std::process::Child;
use std::sync::Mutex;

use crate::config::Config;

/// Port of the former LAN remote, still a config field so old files load.
pub const DEFAULT_PORT: u16 = 47912;

static CHILD: Mutex<Option<Child>> = Mutex::new(None);

/// Starts or stops the helper to match the config. Called at startup and
/// whenever the config file changes.
pub fn sync(cfg: &Config) {
    let mut g = CHILD.lock().unwrap_or_else(|e| e.into_inner());
    let running = g.as_mut().is_some_and(|c| matches!(c.try_wait(), Ok(None)));
    if cfg.phone_remote {
        if running {
            return;
        }
        let Ok(exe) = std::env::current_exe() else { return };
        *g = std::process::Command::new(exe).arg("--phone-link").arg(format!("--parent={}", std::process::id())).spawn().ok();
    } else if let Some(mut c) = g.take() {
        if running {
            let _ = c.kill();
        }
        let _ = c.wait();
    }
}
