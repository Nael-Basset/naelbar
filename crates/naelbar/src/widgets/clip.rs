//! Plain text in and out of the clipboard, for the calculator and the
//! settings address field.

use windows::Win32::Foundation::{HANDLE, HGLOBAL, HWND};
use windows::Win32::System::DataExchange::{CloseClipboard, EmptyClipboard, GetClipboardData, OpenClipboard, SetClipboardData};
use windows::Win32::System::Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock};
use windows::Win32::System::Ole::CF_UNICODETEXT;

/// Puts `text` on the clipboard. Returns false if another program holds it.
pub fn set_text(owner: HWND, text: &str) -> bool {
    let units: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        if OpenClipboard(Some(owner)).is_err() {
            return false;
        }
        let done = (|| {
            EmptyClipboard().ok()?;
            let mem = GlobalAlloc(GMEM_MOVEABLE, units.len() * 2).ok()?;
            let ptr = GlobalLock(mem) as *mut u16;
            if ptr.is_null() {
                return None;
            }
            std::ptr::copy_nonoverlapping(units.as_ptr(), ptr, units.len());
            let _ = GlobalUnlock(mem);
            // The clipboard owns the memory once this succeeds.
            SetClipboardData(CF_UNICODETEXT.0 as u32, Some(HANDLE(mem.0))).ok().map(|_| ())
        })();
        let _ = CloseClipboard();
        done.is_some()
    }
}

/// The text on the clipboard, at most `max_chars` characters.
pub fn get_text(owner: HWND, max_chars: usize) -> Option<String> {
    unsafe {
        OpenClipboard(Some(owner)).ok()?;
        let text = (|| {
            let handle = GetClipboardData(CF_UNICODETEXT.0 as u32).ok()?;
            let mem = HGLOBAL(handle.0);
            let ptr = GlobalLock(mem) as *const u16;
            if ptr.is_null() {
                return None;
            }
            let mut len = 0;
            while len < max_chars * 2 && *ptr.add(len) != 0 {
                len += 1;
            }
            let text = String::from_utf16_lossy(std::slice::from_raw_parts(ptr, len));
            let _ = GlobalUnlock(mem);
            Some(text)
        })();
        let _ = CloseClipboard();
        text
    }
}
