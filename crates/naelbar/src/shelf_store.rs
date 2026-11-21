//! What the shelf remembers: the list of files (`%APPDATA%\naelbar\shelf.json`),
//! a few facts about each file read once when it is added, and the watcher that
//! drops new screenshots onto the shelf.
//!
//! Nothing here polls. The watcher thread sleeps inside the kernel until the
//! Screenshots folder reports a new file, and the info readers are short-lived
//! threads that end as soon as they have read their files.

use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::os::windows::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicIsize, Ordering};
use std::sync::{Arc, Mutex};

use windows::Win32::Foundation::{CloseHandle, HANDLE, HWND, LPARAM, WAIT_OBJECT_0, WPARAM};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OVERLAPPED, FILE_NOTIFY_CHANGE_FILE_NAME, FILE_SHARE_DELETE, FILE_SHARE_READ,
    FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows::Win32::System::Com::StructuredStorage::PropVariantToUInt64;
use windows::Win32::Storage::EnhancedStorage::PKEY_Media_Duration;
use windows::Win32::System::Com::{CoInitializeEx, CoTaskMemFree, CoUninitialize, COINIT_MULTITHREADED};
use windows::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};
use windows::Win32::System::Threading::{CreateEventW, INFINITE, ResetEvent, SetEvent, WaitForMultipleObjects};
use windows::Win32::UI::Shell::PropertiesSystem::{GPS_FASTPROPERTIESONLY, SHGetPropertyStoreFromParsingName};
use windows::Win32::UI::Shell::{FOLDERID_Pictures, FOLDERID_Screenshots, KF_FLAG_DEFAULT, SHGetKnownFolderPath};
use windows::Win32::UI::WindowsAndMessaging::{PostMessageW, WM_APP};
use windows::core::{HSTRING, PCWSTR};

/// Posted to the control window when something about the shelf changed outside
/// the UI thread. wParam 1: files waiting in the inbox or facts ready; 2: the
/// phone transfer changed state.
pub const WM_SHELF_CHANGED: u32 = WM_APP + 21;

pub fn notify(ctrl: isize, what: usize) {
    unsafe {
        let _ = PostMessageW(Some(HWND(ctrl as *mut _)), WM_SHELF_CHANGED, WPARAM(what), LPARAM(0));
    }
}

// ---------------------------------------------------------------------------
// The list of files

fn file_path() -> Option<PathBuf> {
    std::env::var_os("APPDATA").map(|d| PathBuf::from(d).join("naelbar").join("shelf.json"))
}

fn encode(files: &[PathBuf]) -> Vec<u8> {
    let list: Vec<&str> = files.iter().filter_map(|p| p.to_str()).collect();
    serde_json::to_vec_pretty(&list).unwrap_or_default()
}

/// Entries whose file is gone are dropped, and so are repeated ones.
fn parse(bytes: &[u8], exists: impl Fn(&Path) -> bool) -> Vec<PathBuf> {
    let list: Vec<String> = serde_json::from_slice(bytes).unwrap_or_default();
    let mut out: Vec<PathBuf> = Vec::new();
    for s in list {
        let p = PathBuf::from(s);
        if exists(&p) && !out.contains(&p) {
            out.push(p);
        }
    }
    out
}

fn load_from(path: &Path) -> Vec<PathBuf> {
    std::fs::read(path).map(|b| parse(&b, |p| p.exists())).unwrap_or_default()
}

fn save_to(path: &Path, files: &[PathBuf]) {
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    // Replaced in one step so a crash never leaves half a file.
    let tmp = path.with_extension(format!("{}.tmp", std::process::id()));
    if std::fs::write(&tmp, encode(files)).is_ok() && std::fs::rename(&tmp, path).is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
}

pub fn load() -> Vec<PathBuf> {
    file_path().map(|p| load_from(&p)).unwrap_or_default()
}

pub fn save(files: &[PathBuf]) {
    if let Some(p) = file_path() {
        save_to(&p, files);
    }
}

/// Adds files that are not on the shelf yet. Returns true when the list changed.
pub fn add(shelf: &mut Vec<PathBuf>, files: impl IntoIterator<Item = PathBuf>, ctrl: isize) -> bool {
    let fresh: Vec<PathBuf> = files.into_iter().filter(|f| !shelf.contains(f)).collect();
    if fresh.is_empty() {
        return false;
    }
    for f in &fresh {
        if !shelf.contains(f) {
            shelf.push(f.clone());
        }
    }
    save(shelf);
    read_info(fresh, ctrl);
    true
}

pub fn remove(shelf: &mut Vec<PathBuf>, i: usize) -> bool {
    if i >= shelf.len() {
        return false;
    }
    let gone = shelf.remove(i);
    infos().remove(&gone);
    save(shelf);
    true
}

/// Moves the screenshots found by the watcher onto the shelf.
pub fn absorb(shelf: &mut Vec<PathBuf>, ctrl: isize) -> bool {
    let fresh: Vec<PathBuf> = std::mem::take(&mut *INBOX.lock().unwrap());
    add(shelf, fresh, ctrl)
}

static INBOX: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());

// ---------------------------------------------------------------------------
// Facts about a file, read once when it joins the shelf

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Info {
    pub size: u64,
    pub dims: Option<(u32, u32)>,
    pub duration_ms: Option<u64>,
}

fn infos() -> std::sync::MutexGuard<'static, HashMap<PathBuf, Info>> {
    static INFOS: std::sync::OnceLock<Mutex<HashMap<PathBuf, Info>>> = std::sync::OnceLock::new();
    INFOS.get_or_init(|| Mutex::new(HashMap::new())).lock().unwrap_or_else(|e| e.into_inner())
}

/// What is known about `path` so far. Cheap: a map lookup.
pub fn info(path: &Path) -> Option<Info> {
    infos().get(path).copied()
}

/// Reads the facts of each file on a short-lived thread, then wakes the shell.
pub fn read_info(files: Vec<PathBuf>, ctrl: isize) {
    if files.is_empty() {
        return;
    }
    let _ = std::thread::Builder::new().name("shelf-info".into()).spawn(move || {
        let com = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED).is_ok() };
        for f in files {
            let i = read_one(&f);
            infos().insert(f, i);
        }
        if com {
            unsafe { CoUninitialize() };
        }
        notify(ctrl, 1);
    });
}

fn read_one(path: &Path) -> Info {
    let mut info = Info { size: std::fs::metadata(path).map(|m| m.len()).unwrap_or(0), ..Info::default() };
    match kind(path) {
        Kind::Image => {
            if let Ok(mut f) = std::fs::File::open(path) {
                let mut head = Vec::new();
                let _ = f.by_ref().take(256 * 1024).read_to_end(&mut head);
                info.dims = image_dims(&head);
            }
        }
        Kind::Media => info.duration_ms = media_duration_ms(path),
        Kind::Other => {}
    }
    info
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    Image,
    Media,
    Other,
}

fn kind(path: &Path) -> Kind {
    let ext = path.extension().map(|e| e.to_string_lossy().to_ascii_lowercase()).unwrap_or_default();
    match ext.as_str() {
        "png" | "jpg" | "jpeg" | "gif" | "bmp" | "webp" => Kind::Image,
        "mp3" | "m4a" | "aac" | "wav" | "flac" | "ogg" | "opus" | "wma" | "mp4" | "m4v" | "mkv" | "mov" | "avi" | "wmv" | "webm" => Kind::Media,
        _ => Kind::Other,
    }
}

/// Duration from the Windows property system, which answers from the file's
/// own metadata without decoding anything.
fn media_duration_ms(path: &Path) -> Option<u64> {
    unsafe {
        let store: windows::Win32::UI::Shell::PropertiesSystem::IPropertyStore =
            SHGetPropertyStoreFromParsingName(&HSTRING::from(path.as_os_str()), None, GPS_FASTPROPERTIESONLY).ok()?;
        let v = store.GetValue(&PKEY_Media_Duration).ok()?;
        let hundred_ns = PropVariantToUInt64(&v).ok()?;
        (hundred_ns > 0).then_some(hundred_ns / 10_000)
    }
}

/// Pixel size from the first bytes of PNG, GIF, BMP, JPEG and WebP files.
pub fn image_dims(b: &[u8]) -> Option<(u32, u32)> {
    let be32 = |o: usize| b.get(o..o + 4).map(|s| u32::from_be_bytes([s[0], s[1], s[2], s[3]]));
    let le32 = |o: usize| b.get(o..o + 4).map(|s| u32::from_le_bytes([s[0], s[1], s[2], s[3]]));
    let le16 = |o: usize| b.get(o..o + 2).map(|s| u16::from_le_bytes([s[0], s[1]]) as u32);
    let ok = |d: (u32, u32)| (d.0 > 0 && d.1 > 0).then_some(d);
    if b.starts_with(b"\x89PNG\r\n\x1a\n") && b.get(12..16) == Some(b"IHDR") {
        return ok((be32(16)?, be32(20)?));
    }
    if b.starts_with(b"GIF87a") || b.starts_with(b"GIF89a") {
        return ok((le16(6)?, le16(8)?));
    }
    if b.starts_with(b"BM") {
        let w = le32(18)? as i32;
        let h = le32(22)? as i32;
        return ok((w.unsigned_abs(), h.unsigned_abs()));
    }
    if b.starts_with(b"RIFF") && b.get(8..12) == Some(b"WEBP") {
        return match b.get(12..16)? {
            b"VP8X" => {
                let w = 1 + *b.get(24)? as u32 + ((*b.get(25)? as u32) << 8) + ((*b.get(26)? as u32) << 16);
                let h = 1 + *b.get(27)? as u32 + ((*b.get(28)? as u32) << 8) + ((*b.get(29)? as u32) << 16);
                ok((w, h))
            }
            b"VP8L" => {
                let bits = le32(21)?;
                ok(((bits & 0x3FFF) + 1, ((bits >> 14) & 0x3FFF) + 1))
            }
            b"VP8 " => ok((le16(26)? & 0x3FFF, le16(28)? & 0x3FFF)),
            _ => None,
        };
    }
    if b.starts_with(&[0xFF, 0xD8]) {
        let mut i = 2;
        while i + 4 <= b.len() {
            if b[i] != 0xFF {
                i += 1;
                continue;
            }
            let m = b[i + 1];
            if m == 0xFF {
                i += 1;
                continue;
            }
            if m == 0xD8 || m == 0x01 || (0xD0..=0xD7).contains(&m) {
                i += 2;
                continue;
            }
            let len = u16::from_be_bytes([b[i + 2], b[i + 3]]) as usize;
            // SOF0..SOF15 except DHT, JPG and DAC carry the frame size.
            if (0xC0..=0xCF).contains(&m) && ![0xC4, 0xC8, 0xCC].contains(&m) {
                let h = u16::from_be_bytes([*b.get(i + 5)?, *b.get(i + 6)?]) as u32;
                let w = u16::from_be_bytes([*b.get(i + 7)?, *b.get(i + 8)?]) as u32;
                return ok((w, h));
            }
            i += 2 + len;
        }
    }
    None
}

/// One line for the bar under the tiles, such as `1920 × 1080 · 2.3 MB`.
pub fn describe(info: &Info, english: bool) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some((w, h)) = info.dims {
        parts.push(format!("{w} × {h}"));
    }
    if let Some(ms) = info.duration_ms {
        parts.push(crate::ui::format_time(ms));
    }
    parts.push(format_size(info.size, english));
    parts.join(" · ")
}

pub fn format_size(bytes: u64, english: bool) -> String {
    let (kb, mb, gb, b) = if english { ("KB", "MB", "GB", "B") } else { ("Ko", "Mo", "Go", "o") };
    let f = bytes as f64;
    let (v, unit) = if bytes >= 1 << 30 {
        (f / (1u64 << 30) as f64, gb)
    } else if bytes >= 1 << 20 {
        (f / (1u64 << 20) as f64, mb)
    } else if bytes >= 1 << 10 {
        (f / 1024.0, kb)
    } else {
        return format!("{bytes} {b}");
    };
    let s = if v >= 100.0 { format!("{v:.0}") } else { format!("{v:.1}") };
    let s = if english { s } else { s.replace('.', ",") };
    format!("{s} {unit}")
}

// ---------------------------------------------------------------------------
// New screenshots

/// The Screenshots known folder, or Pictures\Screenshots when Windows has none.
fn screenshots_dir() -> Option<PathBuf> {
    unsafe fn known(id: &windows::core::GUID) -> Option<PathBuf> {
        unsafe {
            let p = SHGetKnownFolderPath(id, KF_FLAG_DEFAULT, None).ok()?;
            let s = p.to_string().ok();
            CoTaskMemFree(Some(p.0 as *const _));
            s.map(PathBuf::from)
        }
    }
    let dir = unsafe { known(&FOLDERID_Screenshots) }.or_else(|| unsafe { known(&FOLDERID_Pictures) }.map(|p| p.join("Screenshots")))?;
    if !dir.is_dir() {
        std::fs::create_dir_all(&dir).ok()?;
    }
    Some(dir)
}

/// Manual-reset event shared by the watcher and its thread.
struct StopEvent(AtomicIsize);

impl StopEvent {
    fn new() -> Option<Arc<Self>> {
        let h = unsafe { CreateEventW(None, true, false, None) }.ok()?;
        Some(Arc::new(Self(AtomicIsize::new(h.0 as isize))))
    }
    fn handle(&self) -> HANDLE {
        HANDLE(self.0.load(Ordering::Relaxed) as *mut _)
    }
    fn signal(&self) {
        unsafe {
            let _ = SetEvent(self.handle());
        }
    }
}

impl Drop for StopEvent {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.handle());
        }
    }
}

pub struct Watcher {
    stop: Arc<StopEvent>,
}

impl Drop for Watcher {
    fn drop(&mut self) {
        self.stop.signal();
    }
}

static WATCHER: Mutex<Option<Watcher>> = Mutex::new(None);

/// Starts or stops the screenshot watcher. Calling it again with the same
/// value does nothing.
pub fn set_captures(on: bool, ctrl: isize) {
    let mut w = WATCHER.lock().unwrap();
    if on && w.is_none() {
        *w = Watcher::start(ctrl);
    } else if !on {
        *w = None;
    }
}

impl Watcher {
    fn start(ctrl: isize) -> Option<Watcher> {
        let dir = screenshots_dir()?;
        let stop = StopEvent::new()?;
        let theirs = stop.clone();
        std::thread::Builder::new().name("shelf-captures".into()).spawn(move || watch(&dir, &theirs, ctrl)).ok()?;
        Some(Watcher { stop })
    }
}

const ACTION_ADDED: u32 = 1;
const ACTION_RENAMED_NEW: u32 = 5;

/// Walks a `FILE_NOTIFY_INFORMATION` chain: (action, file name).
fn parse_notifications(buf: &[u8]) -> Vec<(u32, String)> {
    let mut out = Vec::new();
    let mut at = 0usize;
    while at + 12 <= buf.len() {
        let rd = |o: usize| u32::from_le_bytes([buf[at + o], buf[at + o + 1], buf[at + o + 2], buf[at + o + 3]]);
        let (next, action, len) = (rd(0) as usize, rd(4), rd(8) as usize);
        let Some(name) = buf.get(at + 12..at + 12 + len) else { break };
        let wide: Vec<u16> = name.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
        out.push((action, String::from_utf16_lossy(&wide)));
        if next == 0 {
            break;
        }
        at += next;
    }
    out
}

fn is_png_name(name: &str) -> bool {
    !name.contains(['\\', '/']) && Path::new(name).extension().is_some_and(|e| e.eq_ignore_ascii_case("png"))
}

/// A finished PNG ends with an empty IEND chunk.
fn png_is_complete(tail: &[u8]) -> bool {
    tail.ends_with(&[0, 0, 0, 0, b'I', b'E', b'N', b'D', 0xAE, 0x42, 0x60, 0x82])
}

/// The screenshot tool may still be writing. Opening without sharing fails
/// while it holds the file, so retry for a moment.
fn wait_until_written(path: &Path, stop: &StopEvent) -> bool {
    for _ in 0..40 {
        let opened = std::fs::OpenOptions::new().read(true).share_mode(0).open(path);
        if let Ok(mut f) = opened {
            let len = f.metadata().map(|m| m.len()).unwrap_or(0);
            if len >= 12 && f.seek(SeekFrom::End(-12)).is_ok() {
                let mut tail = [0u8; 12];
                if f.read_exact(&mut tail).is_ok() && png_is_complete(&tail) {
                    return true;
                }
            }
        }
        // A kernel wait on the stop event doubles as the delay.
        if unsafe { WaitForMultipleObjects(&[stop.handle()], false, 150) } == WAIT_OBJECT_0 {
            return false;
        }
    }
    false
}

fn watch(dir: &Path, stop: &StopEvent, ctrl: isize) {
    unsafe {
        let wide = HSTRING::from(dir.as_os_str());
        let Ok(h) = CreateFileW(
            PCWSTR(wide.as_ptr()),
            1, // FILE_LIST_DIRECTORY
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            None,
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OVERLAPPED,
            None,
        ) else {
            return;
        };
        let Ok(event) = CreateEventW(None, true, false, None) else {
            let _ = CloseHandle(h);
            return;
        };
        // DWORD aligned, as the API requires.
        let mut buf = vec![0u32; 4096];
        loop {
            let _ = ResetEvent(event);
            let mut ov = OVERLAPPED { hEvent: event, ..Default::default() };
            let queued = windows::Win32::Storage::FileSystem::ReadDirectoryChangesW(
                h,
                buf.as_mut_ptr().cast(),
                (buf.len() * 4) as u32,
                false,
                FILE_NOTIFY_CHANGE_FILE_NAME,
                None,
                Some(&mut ov),
                None,
            );
            if queued.is_err() {
                break;
            }
            let woke = WaitForMultipleObjects(&[event, stop.handle()], false, INFINITE);
            if woke != WAIT_OBJECT_0 {
                // Stop (or a failed wait): cancel and wait for the kernel to let go of the buffer.
                let _ = CancelIoEx(h, Some(&ov));
                let mut n = 0u32;
                let _ = GetOverlappedResult(h, &ov, &mut n, true);
                break;
            }
            let mut n = 0u32;
            if GetOverlappedResult(h, &ov, &mut n, false).is_err() {
                break;
            }
            let bytes = std::slice::from_raw_parts(buf.as_ptr().cast::<u8>(), n as usize).to_vec();
            let mut found = false;
            for (action, name) in parse_notifications(&bytes) {
                if (action == ACTION_ADDED || action == ACTION_RENAMED_NEW) && is_png_name(&name) {
                    let path = dir.join(&name);
                    if wait_until_written(&path, stop) {
                        INBOX.lock().unwrap().push(path);
                        found = true;
                    }
                }
            }
            if found {
                notify(ctrl, 1);
            }
        }
        let _ = CloseHandle(event);
        let _ = CloseHandle(h);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("naelbar-shelf-test-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn list_survives_a_round_trip_and_drops_missing_files() {
        let d = temp("roundtrip");
        let real = d.join("a.txt");
        std::fs::write(&real, b"x").unwrap();
        let gone = d.join("gone.txt");
        let file = d.join("sub").join("shelf.json");
        save_to(&file, &[real.clone(), gone, real.clone()]);
        assert_eq!(load_from(&file), vec![real]);
        assert!(std::fs::read_dir(file.parent().unwrap()).unwrap().all(|e| !e.unwrap().file_name().to_string_lossy().ends_with(".tmp")));
        assert!(load_from(&d.join("none.json")).is_empty());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn garbage_file_gives_an_empty_shelf() {
        assert!(parse(b"{ not json", |_| true).is_empty());
        assert!(parse(b"{\"a\":1}", |_| true).is_empty());
    }

    #[test]
    fn png_gif_bmp_dimensions() {
        let mut png = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec();
        png.extend_from_slice(&1920u32.to_be_bytes());
        png.extend_from_slice(&1080u32.to_be_bytes());
        assert_eq!(image_dims(&png), Some((1920, 1080)));
        let gif = [b"GIF89a".as_slice(), &[0x40, 0x01, 0xC8, 0x00]].concat();
        assert_eq!(image_dims(&gif), Some((320, 200)));
        let mut bmp = vec![0u8; 26];
        bmp[0] = b'B';
        bmp[1] = b'M';
        bmp[18..22].copy_from_slice(&640i32.to_le_bytes());
        bmp[22..26].copy_from_slice(&(-480i32).to_le_bytes());
        assert_eq!(image_dims(&bmp), Some((640, 480)));
    }

    #[test]
    fn jpeg_dimensions_skip_other_segments() {
        let mut j = vec![0xFF, 0xD8, 0xFF, 0xE1, 0x00, 0x06, 1, 2, 3, 4];
        j.extend_from_slice(&[0xFF, 0xC0, 0x00, 0x11, 8, 0x02, 0x00, 0x03, 0x00]);
        assert_eq!(image_dims(&j), Some((768, 512)));
        assert_eq!(image_dims(&j[..8]), None);
        assert_eq!(image_dims(b"hello world, not an image"), None);
    }

    #[test]
    fn webp_lossless_dimensions() {
        let mut w = b"RIFF\0\0\0\0WEBPVP8L\0\0\0\0".to_vec();
        w.push(0x2F);
        let bits: u32 = 99 | (49 << 14);
        w.extend_from_slice(&bits.to_le_bytes());
        assert_eq!(image_dims(&w), Some((100, 50)));
    }

    #[test]
    fn sizes_are_readable_in_both_languages() {
        assert_eq!(format_size(512, true), "512 B");
        assert_eq!(format_size(512, false), "512 o");
        assert_eq!(format_size(1536, true), "1.5 KB");
        assert_eq!(format_size(2_411_724, false), "2,3 Mo");
        assert_eq!(format_size(300 << 20, true), "300 MB");
        let i = Info { size: 2048, dims: Some((10, 20)), duration_ms: Some(65_000) };
        assert_eq!(describe(&i, true), "10 × 20 · 1:05 · 2.0 KB");
        assert_eq!(describe(&Info { size: 5, ..Info::default() }, true), "5 B");
    }

    #[test]
    fn kinds_follow_the_extension() {
        assert_eq!(kind(Path::new("a/B.PNG")), Kind::Image);
        assert_eq!(kind(Path::new("a.mp3")), Kind::Media);
        assert_eq!(kind(Path::new("a.docx")), Kind::Other);
        assert_eq!(kind(Path::new("noext")), Kind::Other);
    }

    fn entry(next: u32, action: u32, name: &str) -> Vec<u8> {
        let w: Vec<u8> = name.encode_utf16().flat_map(|c| c.to_le_bytes()).collect();
        let mut v = Vec::new();
        v.extend_from_slice(&next.to_le_bytes());
        v.extend_from_slice(&action.to_le_bytes());
        v.extend_from_slice(&(w.len() as u32).to_le_bytes());
        v.extend_from_slice(&w);
        v
    }

    #[test]
    fn notification_chain_is_walked() {
        let first = entry(0, 1, "Capture d'écran.png");
        let next = first.len() as u32;
        let mut buf = entry(next, 1, "Capture d'écran.png");
        buf.extend(entry(0, 5, "b.png"));
        assert_eq!(parse_notifications(&buf), vec![(1, "Capture d'écran.png".to_string()), (5, "b.png".to_string())]);
        assert!(parse_notifications(&[1, 2, 3]).is_empty());
        // A length that points past the buffer ends the walk instead of panicking.
        let mut bad = entry(0, 1, "x.png");
        bad[8] = 200;
        assert!(parse_notifications(&bad).is_empty());
    }

    #[test]
    fn only_png_files_in_the_folder_itself_count() {
        assert!(is_png_name("a.png"));
        assert!(is_png_name("A.PNG"));
        assert!(!is_png_name("a.png.tmp"));
        assert!(!is_png_name("a.jpg"));
        assert!(!is_png_name("sub\\a.png"));
    }

    #[test]
    fn png_completeness_needs_the_iend_chunk() {
        assert!(png_is_complete(&[0, 0, 0, 0, b'I', b'E', b'N', b'D', 0xAE, 0x42, 0x60, 0x82]));
        assert!(!png_is_complete(&[0; 12]));
        assert!(!png_is_complete(&[]));
    }

    #[test]
    fn a_half_written_png_is_waited_for_then_accepted() {
        let d = temp("png");
        let p = d.join("shot.png");
        let done = [b"\x89PNG\r\n\x1a\n".as_slice(), &[0, 0, 0, 0, b'I', b'E', b'N', b'D', 0xAE, 0x42, 0x60, 0x82]].concat();
        std::fs::write(&p, &done[..10]).unwrap();
        let stop = StopEvent::new().unwrap();
        let (p2, done2) = (p.clone(), done.clone());
        let writer = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(400));
            std::fs::write(p2, done2).unwrap();
        });
        assert!(wait_until_written(&p, &stop));
        writer.join().unwrap();
        // A stop request ends the wait early.
        std::fs::write(&p, &done[..10]).unwrap();
        stop.signal();
        assert!(!wait_until_written(&p, &stop));
        let _ = std::fs::remove_dir_all(&d);
    }
}
