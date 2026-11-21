//! Clipboard history service.
//!
//! One thread owns a message-only window registered with
//! `AddClipboardFormatListener`. It sleeps in `GetMessageW` and wakes only on
//! `WM_CLIPBOARDUPDATE` or a command from [`Handle`]. No polling, no timers.
//!
//! Content flagged as sensitive by the clipboard-history conventions
//! (see [`policy`]) is never read, stored or emitted.

mod crypt;
mod image;
pub mod policy;
mod store;
mod win;

pub use nb_core::{ClipContent, ClipItem};
pub use store::Config;

use nb_core::Sink;
use std::sync::atomic::{AtomicIsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use store::Store;

#[derive(Debug)]
pub enum Error {
    Windows(windows::core::Error),
    Thread(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Windows(e) => write!(f, "windows error: {e}"),
            Error::Thread(s) => write!(f, "clipboard thread: {s}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<windows::core::Error> for Error {
    fn from(e: windows::core::Error) -> Self {
        Error::Windows(e)
    }
}

/// State shared between the UI-facing [`Handle`] and the window thread.
pub(crate) struct Shared {
    store: Mutex<Store>,
    hwnd: AtomicIsize,
}

impl Shared {
    pub(crate) fn store(&self) -> MutexGuard<'_, Store> {
        self.store.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub(crate) fn set_hwnd(&self, h: isize) {
        self.hwnd.store(h, Ordering::Release);
    }

    /// Writes history to disk if (and only if) it changed. I/O runs without
    /// holding the store lock.
    pub(crate) fn persist(&self) {
        let job = self.store().prepare_save();
        if let Some(job) = job {
            if job.write().is_ok() {
                self.store().mark_saved(job.rev);
            }
        }
    }

    /// Wakes the window thread. Never blocks.
    fn post(&self, msg: u32, wparam: usize) {
        use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
        use windows::Win32::UI::WindowsAndMessaging::PostMessageW;
        let h = self.hwnd.load(Ordering::Acquire);
        if h != 0 {
            unsafe {
                let _ = PostMessageW(Some(HWND(h as *mut _)), msg, WPARAM(wparam), LPARAM(0));
            }
        }
    }
}

/// Dropping the handle stops the service and joins its thread.
pub struct Handle {
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
}

/// Starts the clipboard service. Loads persisted history first. Existing
/// history is available through [`Handle::list`]; only new captures are
/// emitted as `Event::ClipboardAdded`.
pub fn start(sink: Sink, config: Config) -> Result<Handle, Error> {
    let shared = Arc::new(Shared {
        store: Mutex::new(Store::load(&config)),
        hwnd: AtomicIsize::new(0),
    });
    let thread = win::spawn(shared.clone(), sink)?;
    // Persist anything changed while loading (caps that shrank).
    shared.post(win::WM_APP_PERSIST, 0);
    Ok(Handle { shared, thread: Some(thread) })
}

impl Handle {
    /// Current history, newest first. Pinned items keep their position.
    pub fn list(&self) -> Vec<ClipItem> {
        self.shared.store().list()
    }

    /// Asks the service to put item `id` on the system clipboard. The write
    /// is not re-captured. Returns false if the id is unknown.
    pub fn copy_item(&self, id: u64) -> bool {
        if self.shared.store().get(id).is_none() {
            return false;
        }
        self.shared.post(win::WM_APP_COPY, id as usize);
        true
    }

    pub fn delete(&self, id: u64) -> bool {
        let changed = self.shared.store().delete(id);
        if changed {
            self.shared.post(win::WM_APP_PERSIST, 0);
        }
        changed
    }

    pub fn pin(&self, id: u64, pinned: bool) -> bool {
        let changed = self.shared.store().pin(id, pinned);
        if changed {
            self.shared.post(win::WM_APP_PERSIST, 0);
        }
        changed
    }

    /// Removes every unpinned item.
    pub fn clear(&self) {
        if self.shared.store().clear() {
            self.shared.post(win::WM_APP_PERSIST, 0);
        }
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
        use windows::Win32::UI::WindowsAndMessaging::{PostMessageW, WM_CLOSE};
        let h = self.shared.hwnd.load(Ordering::Acquire);
        if h != 0 {
            unsafe {
                let _ = PostMessageW(Some(HWND(h as *mut _)), WM_CLOSE, WPARAM(0), LPARAM(0));
            }
        }
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        // Flush anything still pending (commands posted just before drop).
        self.shared.persist();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_cfg(tag: &str) -> (Config, std::path::PathBuf) {
        let d = std::env::temp_dir().join(format!("nb-clipboard-lib-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        (Config { dir: Some(d.clone()), ..Config::default() }, d)
    }

    /// Starts and stops the real window thread. Never touches clipboard data.
    #[test]
    fn start_and_drop_cleanly_twice() {
        let (cfg, dir) = temp_cfg("life");
        for _ in 0..2 {
            let sink: Sink = Arc::new(|_| {});
            let h = start(sink, cfg.clone()).expect("start");
            assert!(h.list().is_empty());
            assert!(!h.copy_item(12345), "unknown id is refused, clipboard untouched");
            assert!(!h.delete(12345));
            h.clear();
            drop(h);
        }
        assert!(!dir.exists(), "nothing written when nothing changed");
    }
}
