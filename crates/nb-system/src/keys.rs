//! Caps Lock, Num Lock and Scroll Lock changes, for the island's HUD.
//!
//! Windows sends no notification when a lock key toggles, so something has to
//! see the keystrokes. The choice is a `WH_KEYBOARD_LL` hook on a dedicated
//! thread rather than Raw Input: Raw Input would deliver a `WM_INPUT` message
//! plus a `GetRawInputData` copy for every key of every keyboard, while the
//! hook procedure only compares one integer and returns at once. Input is never
//! held: the procedure never waits, allocates or reads state, it posts a
//! message to its own thread for a lock key release and always calls the next
//! hook. The thread otherwise sleeps in `GetMessageW`, and nothing is installed
//! unless the user wants lock key HUDs.
//!
//! The toggle is read on release, once the system has applied the press.

use nb_core::{Event, LockKey, Sink};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc;
use std::thread::JoinHandle;
use windows::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Input::KeyboardAndMouse::GetKeyState;
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, GetMessageW, HC_ACTION, KBDLLHOOKSTRUCT, MSG, PM_NOREMOVE, PeekMessageW, PostThreadMessageW,
    SetWindowsHookExW, UnhookWindowsHookEx, WH_KEYBOARD_LL, WM_APP, WM_KEYDOWN, WM_KEYUP, WM_QUIT, WM_SYSKEYDOWN,
    WM_SYSKEYUP,
};

const VK_CAPITAL: u32 = 0x14;
const VK_NUMLOCK: u32 = 0x90;
const VK_SCROLL: u32 = 0x91;
/// wParam: virtual key. lParam: 1 for a press, 0 for a release.
const WM_LOCK_KEY: u32 = WM_APP + 1;

/// Thread that owns the hook. Only one watch exists per process.
static HOOK_THREAD: AtomicU32 = AtomicU32::new(0);

pub fn key_of(vk: u32) -> Option<LockKey> {
    match vk {
        VK_CAPITAL => Some(LockKey::Caps),
        VK_NUMLOCK => Some(LockKey::Num),
        VK_SCROLL => Some(LockKey::Scroll),
        _ => None,
    }
}

fn index(k: LockKey) -> usize {
    match k {
        LockKey::Caps => 0,
        LockKey::Num => 1,
        LockKey::Scroll => 2,
    }
}

/// New toggle state after a release. `read` comes from `GetKeyState`; should
/// it lag behind for a thread that never reads keyboard input, a fresh press
/// still flips the last known state.
pub fn next_state(last: bool, read: bool, pressed: bool) -> bool {
    if read != last { read } else if pressed { !last } else { last }
}

/// Keeps the hook alive. Dropping it removes the hook and ends the thread.
pub struct LockKeyWatch {
    thread: Option<JoinHandle<()>>,
    thread_id: u32,
}

/// Installs the hook. Emits `Event::LockKey` on each change, never the
/// initial state.
pub fn watch_lock_keys(sink: Sink) -> Result<LockKeyWatch, crate::Error> {
    let (tx, rx) = mpsc::channel::<Result<u32, crate::Error>>();
    let thread = std::thread::Builder::new()
        .name("nb-lock-keys".into())
        .spawn(move || run(sink, tx))
        .map_err(|e| crate::Error::Thread(e.to_string()))?;
    match rx.recv() {
        Ok(Ok(thread_id)) => Ok(LockKeyWatch { thread: Some(thread), thread_id }),
        Ok(Err(e)) => {
            let _ = thread.join();
            Err(e)
        }
        Err(_) => Err(crate::Error::Thread("lock key thread exited during start".into())),
    }
}

impl Drop for LockKeyWatch {
    fn drop(&mut self) {
        // SAFETY: posting to a thread id is sound; the thread created its queue
        // before reporting its id.
        unsafe {
            let _ = PostThreadMessageW(self.thread_id, WM_QUIT, WPARAM(0), LPARAM(0));
        }
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn toggled(vk: u32) -> bool {
    // SAFETY: plain query.
    unsafe { GetKeyState(vk as i32) & 1 != 0 }
}

fn run(sink: Sink, tx: mpsc::Sender<Result<u32, crate::Error>>) {
    // SAFETY: Win32 calls on the thread that owns the hook and the queue.
    unsafe {
        let mut msg = MSG::default();
        // Creates this thread's message queue before anyone posts to it.
        let _ = PeekMessageW(&mut msg, None, 0, 0, PM_NOREMOVE);
        let id = GetCurrentThreadId();
        if HOOK_THREAD.compare_exchange(0, id, Ordering::AcqRel, Ordering::Acquire).is_err() {
            let _ = tx.send(Err(crate::Error::Thread("a lock key watch already runs".into())));
            return;
        }
        let hmod = GetModuleHandleW(None).ok();
        let hook = match SetWindowsHookExW(WH_KEYBOARD_LL, Some(hook_proc), hmod.map(|m| m.into()), 0) {
            Ok(h) => h,
            Err(e) => {
                HOOK_THREAD.store(0, Ordering::Release);
                let _ = tx.send(Err(e.into()));
                return;
            }
        };
        let mut last = [toggled(VK_CAPITAL), toggled(VK_NUMLOCK), toggled(VK_SCROLL)];
        let mut pressed = [false; 3];
        let _ = tx.send(Ok(id));
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            if msg.message != WM_LOCK_KEY {
                continue;
            }
            let vk = msg.wParam.0 as u32;
            let Some(key) = key_of(vk) else { continue };
            let i = index(key);
            if msg.lParam.0 == 1 {
                pressed[i] = true;
                continue;
            }
            let now = next_state(last[i], toggled(vk), std::mem::take(&mut pressed[i]));
            if now != last[i] {
                last[i] = now;
                sink(Event::LockKey(key, now));
            }
        }
        let _ = UnhookWindowsHookEx(hook);
        HOOK_THREAD.store(0, Ordering::Release);
    }
}

/// Called by Windows for every keystroke: must return at once.
unsafe extern "system" fn hook_proc(code: i32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 {
        // SAFETY: for HC_ACTION, lParam points at a KBDLLHOOKSTRUCT.
        let vk = unsafe { (*(lp.0 as *const KBDLLHOOKSTRUCT)).vkCode };
        if matches!(vk, VK_CAPITAL | VK_NUMLOCK | VK_SCROLL) {
            let msg = wp.0 as u32;
            let press = msg == WM_KEYDOWN || msg == WM_SYSKEYDOWN;
            if press || msg == WM_KEYUP || msg == WM_SYSKEYUP {
                // SAFETY: posting never blocks; failure only loses one HUD.
                unsafe {
                    let _ = PostThreadMessageW(HOOK_THREAD.load(Ordering::Relaxed), WM_LOCK_KEY, WPARAM(vk as usize), LPARAM(press as isize));
                }
            }
        }
    }
    // SAFETY: passes the event on unchanged.
    unsafe { CallNextHookEx(None, code, wp, lp) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_lock_keys_matter() {
        assert_eq!(key_of(0x14), Some(LockKey::Caps));
        assert_eq!(key_of(0x90), Some(LockKey::Num));
        assert_eq!(key_of(0x91), Some(LockKey::Scroll));
        assert_eq!(key_of(0x41), None);
    }

    #[test]
    fn fresh_reading_wins() {
        assert!(next_state(false, true, true));
        assert!(!next_state(true, false, false));
    }

    #[test]
    fn stale_reading_still_follows_presses() {
        // GetKeyState stuck at "off": every press must still alternate.
        let mut s = false;
        let mut seen = Vec::new();
        for _ in 0..4 {
            s = next_state(s, false, true);
            seen.push(s);
        }
        assert_eq!(seen, [true, false, true, false]);
    }

    #[test]
    fn release_without_press_changes_nothing() {
        assert!(next_state(true, true, false));
        assert!(!next_state(false, false, false));
    }
}
