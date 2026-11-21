//! Message-only window on one parked thread. Blocked in `GetMessageW` unless
//! the clipboard changed or the UI sent a command.

use crate::image::{self, ComGuard, MAX_DIB_BYTES};
use crate::policy::{self, CanInclude, SensitivityFlags, OPEN_BACKOFF_MS};
use crate::{Error, Shared};
use nb_core::{ClipContent, Event, Sink};
use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use windows::Win32::Foundation::*;
use windows::Win32::System::DataExchange::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Memory::*;
use windows::Win32::System::Ole::{CF_DIB, CF_DIBV5, CF_HDROP, CF_UNICODETEXT};
use windows::Win32::UI::Shell::{DragQueryFileW, HDROP};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{HSTRING, PCWSTR, w};

pub(crate) const WM_APP_COPY: u32 = WM_APP + 1;
pub(crate) const WM_APP_PERSIST: u32 = WM_APP + 2;

const MAX_FILES: u32 = 1000;

struct Fmts {
    exclude: u32,
    can_include: u32,
    viewer_ignore: u32,
    png: u32,
}

struct State {
    shared: Arc<Shared>,
    sink: Sink,
    fmts: Fmts,
    /// Clipboard sequence numbers produced by our own write, inclusive.
    own_seq: Option<(u32, u32)>,
}

enum Raw {
    Text(String),
    Files(Vec<PathBuf>),
    Png(Vec<u8>),
    Dib(Vec<u8>),
}

pub(crate) fn spawn(shared: Arc<Shared>, sink: Sink) -> Result<JoinHandle<()>, Error> {
    let (tx, rx) = mpsc::channel();
    let th = thread::Builder::new()
        .name("nb-clipboard".into())
        .spawn(move || run(shared, sink, tx))
        .map_err(|e| Error::Thread(e.to_string()))?;
    match rx.recv() {
        Ok(Ok(())) => Ok(th),
        Ok(Err(e)) => {
            let _ = th.join();
            Err(e)
        }
        Err(_) => Err(Error::Thread("clipboard thread exited during start".into())),
    }
}

fn run(shared: Arc<Shared>, sink: Sink, ready: mpsc::Sender<Result<(), Error>>) {
    let _com = ComGuard::new();
    let hwnd = match create_window() {
        Ok(h) => h,
        Err(e) => {
            let _ = ready.send(Err(e));
            return;
        }
    };
    if let Err(e) = unsafe { AddClipboardFormatListener(hwnd) } {
        unsafe {
            let _ = DestroyWindow(hwnd);
        }
        let _ = ready.send(Err(e.into()));
        return;
    }
    let reg = |name: &str| unsafe { RegisterClipboardFormatW(PCWSTR(HSTRING::from(name).as_ptr())) };
    let state = Box::into_raw(Box::new(State {
        shared: shared.clone(),
        sink,
        fmts: Fmts {
            exclude: reg(policy::FMT_EXCLUDE_MONITOR),
            can_include: reg(policy::FMT_CAN_INCLUDE_HISTORY),
            viewer_ignore: reg(policy::FMT_VIEWER_IGNORE),
            png: reg("PNG"),
        },
        own_seq: None,
    }));
    unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, state as isize) };
    shared.set_hwnd(hwnd.0 as isize);
    let _ = ready.send(Ok(()));

    let mut msg = MSG::default();
    // Blocks in the kernel until a message arrives. Returns 0 on WM_QUIT.
    while unsafe { GetMessageW(&mut msg, None, 0, 0) }.0 > 0 {
        unsafe {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
    shared.set_hwnd(0);
    unsafe {
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
        drop(Box::from_raw(state));
    }
}

fn create_window() -> Result<HWND, Error> {
    unsafe {
        let hinst = GetModuleHandleW(None)?;
        let class = w!("NaelbarClipboardSink");
        let wc = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: hinst.into(),
            lpszClassName: class,
            ..Default::default()
        };
        // Fails harmlessly if the class already exists from a previous start.
        RegisterClassW(&wc);
        Ok(CreateWindowExW(
            WINDOW_EX_STYLE(0),
            class,
            w!(""),
            WINDOW_STYLE(0),
            0,
            0,
            0,
            0,
            Some(HWND_MESSAGE),
            None,
            Some(hinst.into()),
            None,
        )?)
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    let st = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *mut State;
    if !st.is_null() {
        // The pointer is only touched from this thread, and is cleared before free.
        let st = unsafe { &mut *st };
        match msg {
            WM_CLIPBOARDUPDATE => {
                st.on_update(hwnd);
                return LRESULT(0);
            }
            WM_APP_COPY => {
                st.copy_item(hwnd, wp.0 as u64);
                return LRESULT(0);
            }
            WM_APP_PERSIST => {
                st.shared.persist();
                return LRESULT(0);
            }
            _ => {}
        }
    }
    match msg {
        WM_CLOSE => {
            unsafe {
                let _ = DestroyWindow(hwnd);
            }
            LRESULT(0)
        }
        WM_DESTROY => {
            unsafe {
                let _ = RemoveClipboardFormatListener(hwnd);
                PostQuitMessage(0);
            }
            LRESULT(0)
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wp, lp) },
    }
}

/// Closes the clipboard on drop.
struct Open;
impl Open {
    /// Bounded retries: at most `OPEN_BACKOFF_MS.len()` attempts.
    fn acquire(hwnd: HWND) -> Option<Open> {
        policy::retry_bounded(
            &OPEN_BACKOFF_MS,
            || unsafe { OpenClipboard(Some(hwnd)) }.ok().map(|_| Open),
            thread::sleep,
        )
    }
}
impl Drop for Open {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseClipboard();
        }
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Copies the bytes of a clipboard-owned global handle. Clipboard must be open.
unsafe fn read_global(h: HANDLE, max: usize) -> Option<Vec<u8>> {
    unsafe {
        let g = HGLOBAL(h.0);
        let size = GlobalSize(g);
        if size == 0 || size > max {
            return None;
        }
        let p = GlobalLock(g);
        if p.is_null() {
            return None;
        }
        let v = std::slice::from_raw_parts(p as *const u8, size).to_vec();
        let _ = GlobalUnlock(g);
        Some(v)
    }
}

unsafe fn get_bytes(format: u32, max: usize) -> Option<Vec<u8>> {
    unsafe {
        IsClipboardFormatAvailable(format).ok()?;
        let h = GetClipboardData(format).ok()?;
        read_global(h, max)
    }
}

/// Allocates a movable global and hands it to the clipboard on success.
unsafe fn set_bytes(format: u32, data: &[u8]) -> bool {
    unsafe {
        let Ok(g) = GlobalAlloc(GMEM_MOVEABLE, data.len().max(1)) else { return false };
        let p = GlobalLock(g);
        if p.is_null() {
            let _ = GlobalFree(Some(g));
            return false;
        }
        std::ptr::copy_nonoverlapping(data.as_ptr(), p as *mut u8, data.len());
        let _ = GlobalUnlock(g);
        if SetClipboardData(format, Some(HANDLE(g.0))).is_err() {
            let _ = GlobalFree(Some(g));
            return false;
        }
        true
    }
}

impl Fmts {
    unsafe fn flags(&self) -> SensitivityFlags {
        unsafe {
            let avail = |f: u32| IsClipboardFormatAvailable(f).is_ok();
            let can = if avail(self.can_include) {
                match get_bytes(self.can_include, 64) {
                    Some(b) if b.len() >= 4 => {
                        CanInclude::Value(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
                    }
                    _ => CanInclude::Unreadable,
                }
            } else {
                CanInclude::Absent
            };
            SensitivityFlags {
                exclude_from_monitor: avail(self.exclude),
                can_include_in_history: can,
                viewer_ignore: avail(self.viewer_ignore),
            }
        }
    }

    /// Reads the clipboard while open. `None` when sensitive or unsupported.
    unsafe fn capture(&self) -> Option<Raw> {
        unsafe {
            if self.flags().is_sensitive() {
                return None;
            }
            if IsClipboardFormatAvailable(CF_HDROP.0 as u32).is_ok() {
                if let Some(files) = read_hdrop() {
                    return Some(Raw::Files(files));
                }
            }
            if let Some(b) = get_bytes(CF_UNICODETEXT.0 as u32, crate::store::MAX_TEXT_BYTES * 2 + 2) {
                let wide: Vec<u16> = b
                    .chunks_exact(2)
                    .map(|c| u16::from_le_bytes([c[0], c[1]]))
                    .take_while(|&u| u != 0)
                    .collect();
                return Some(Raw::Text(String::from_utf16_lossy(&wide)));
            }
            if let Some(b) = get_bytes(self.png, MAX_DIB_BYTES) {
                return Some(Raw::Png(b));
            }
            let dib = get_bytes(CF_DIBV5.0 as u32, MAX_DIB_BYTES)
                .or_else(|| get_bytes(CF_DIB.0 as u32, MAX_DIB_BYTES))?;
            Some(Raw::Dib(dib))
        }
    }
}

unsafe fn read_hdrop() -> Option<Vec<PathBuf>> {
    use std::os::windows::ffi::OsStringExt;
    unsafe {
        let h = GetClipboardData(CF_HDROP.0 as u32).ok()?;
        let drop = HDROP(h.0);
        let count = DragQueryFileW(drop, u32::MAX, None).min(MAX_FILES);
        let mut out = Vec::new();
        for i in 0..count {
            let len = DragQueryFileW(drop, i, None) as usize;
            if len == 0 {
                continue;
            }
            let mut buf = vec![0u16; len + 1];
            let n = DragQueryFileW(drop, i, Some(&mut buf)) as usize;
            out.push(PathBuf::from(std::ffi::OsString::from_wide(&buf[..n])));
        }
        (!out.is_empty()).then_some(out)
    }
}

impl State {
    fn on_update(&mut self, hwnd: HWND) {
        let seq = unsafe { GetClipboardSequenceNumber() };
        if let Some((a, b)) = self.own_seq {
            if seq.wrapping_sub(a) <= b.wrapping_sub(a) {
                return; // our own write, do not re-capture
            }
        }
        // Fail closed: if the clipboard cannot be opened we capture nothing.
        let raw = {
            let Some(_open) = Open::acquire(hwnd) else { return };
            unsafe { self.fmts.capture() }
        };
        let Some(raw) = raw else { return };
        // Encoding happens after the clipboard is closed again.
        let content = match raw {
            Raw::Text(t) => ClipContent::Text(t),
            Raw::Files(f) => ClipContent::Files(f),
            Raw::Png(p) => match image::png_dimensions(&p) {
                Some((w, h)) => ClipContent::Image { png: p.into(), width: w, height: h },
                None => return,
            },
            Raw::Dib(d) => match image::dib_to_png(&d) {
                Some((p, w, h)) => ClipContent::Image { png: p.into(), width: w, height: h },
                None => return,
            },
        };
        let item = self.shared.store().add(content, now_ms());
        if let Some(item) = item {
            self.shared.persist();
            (self.sink)(Event::ClipboardAdded(item));
        }
    }

    fn copy_item(&mut self, hwnd: HWND, id: u64) {
        let Some(item) = self.shared.store().get(id) else { return };
        let mut sets: Vec<(u32, Vec<u8>)> = Vec::new();
        match &item.content {
            ClipContent::Text(t) => {
                let mut b = Vec::with_capacity(t.len() * 2 + 2);
                for u in t.encode_utf16() {
                    b.extend_from_slice(&u.to_le_bytes());
                }
                b.extend_from_slice(&[0, 0]);
                sets.push((CF_UNICODETEXT.0 as u32, b));
            }
            ClipContent::Image { png, .. } => {
                if let Some(dib) = image::png_to_dib(png) {
                    sets.push((CF_DIB.0 as u32, dib));
                }
                sets.push((self.fmts.png, png.to_vec()));
            }
            ClipContent::Files(f) => sets.push((CF_HDROP.0 as u32, image::build_hdrop(f))),
        }
        let Some(_open) = Open::acquire(hwnd) else { return };
        unsafe {
            let start = GetClipboardSequenceNumber().wrapping_add(1);
            if EmptyClipboard().is_err() {
                return;
            }
            for (fmt, data) in &sets {
                set_bytes(*fmt, data);
            }
            let end = GetClipboardSequenceNumber();
            self.own_seq = Some((start, end));
        }
    }
}

