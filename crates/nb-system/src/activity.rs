//! Live activities for the closed island: browser downloads and the privacy
//! indicators (microphone, camera, screen capture).
//!
//! Both are event driven and idle at zero cost. Downloads watch the Downloads
//! folder with `ReadDirectoryChangesW`; the thread sleeps in
//! `WaitForMultipleObjects` until the folder changes. The indicators watch
//! `HKCU\...\CapabilityAccessManager\ConsentStore` with `RegNotifyChangeKeyValue`
//! and re-read the three capabilities when something under it changes. Neither
//! thread exists unless the shell asks for it, and dropping the guard ends it.

use std::ffi::c_void;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread::JoinHandle;

use nb_core::{CaptureKind, CaptureUse, DownloadEvent, Event, Sink};
use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OVERLAPPED, FILE_LIST_DIRECTORY, FILE_NOTIFY_CHANGE_FILE_NAME,
    FILE_NOTIFY_CHANGE_LAST_WRITE, FILE_NOTIFY_CHANGE_SIZE, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    ReadDirectoryChangesW,
};
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};
use windows::Win32::System::Registry::{
    HKEY, HKEY_CURRENT_USER, KEY_NOTIFY, KEY_READ, REG_NOTIFY_CHANGE_LAST_SET, REG_NOTIFY_CHANGE_NAME, RegCloseKey, RegEnumKeyExW,
    RegNotifyChangeKeyValue, RegOpenKeyExW, RegQueryValueExW,
};
use windows::Win32::System::Threading::{CreateEventW, INFINITE, ResetEvent, SetEvent, WaitForMultipleObjects};
use windows::Win32::UI::Shell::{FOLDERID_Downloads, KF_FLAG_DEFAULT, SHGetKnownFolderPath};
use windows::core::{PCWSTR, PWSTR, w};

// ---------------------------------------------------------------------------
// Downloads: pure logic
// ---------------------------------------------------------------------------

/// Extensions browsers give a file while it is still being written.
const PARTIAL_EXTENSIONS: [&str; 6] = [".crdownload", ".part", ".tmp", ".partial", ".download", ".opdownload"];

/// True for the name of a file a browser has not finished.
pub fn is_partial(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    PARTIAL_EXTENSIONS.iter().any(|e| lower.len() > e.len() && lower.ends_with(e))
}

/// The name the file will have once finished. Empty while the browser has not
/// chosen it yet (Chromium starts with `Unconfirmed 123.crdownload`).
pub fn final_name(partial: &str) -> String {
    let lower = partial.to_ascii_lowercase();
    let Some(ext) = PARTIAL_EXTENSIONS.iter().find(|e| lower.ends_with(**e)) else { return partial.to_string() };
    let stem = &partial[..partial.len() - ext.len()];
    if stem.to_ascii_lowercase().starts_with("unconfirmed ") { String::new() } else { stem.to_string() }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Added,
    Removed,
    Modified,
    RenamedOld,
    RenamedNew,
}

impl Action {
    pub fn from_raw(raw: u32) -> Option<Self> {
        match raw {
            1 => Some(Action::Added),
            2 => Some(Action::Removed),
            3 => Some(Action::Modified),
            4 => Some(Action::RenamedOld),
            5 => Some(Action::RenamedNew),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// The partial file `partial` of download `id` changed.
    Progress { id: u64, partial: String },
    /// The partial file was renamed to its final `name`.
    Done { id: u64, name: String },
    Cancel { id: u64 },
}

impl Step {
    fn id(&self) -> u64 {
        match self {
            Step::Progress { id, .. } | Step::Done { id, .. } | Step::Cancel { id } => *id,
        }
    }
}

/// Follows partial files through their renames.
#[derive(Debug, Default)]
pub struct Tracker {
    next_id: u64,
    active: Vec<(u64, String)>,
    /// Old name of a rename whose new name has not arrived yet.
    old: Option<String>,
}

impl Tracker {
    fn find(&self, name: &str) -> Option<usize> {
        self.active.iter().position(|(_, n)| n.eq_ignore_ascii_case(name))
    }

    fn start(&mut self, name: &str) -> Step {
        self.next_id += 1;
        self.active.push((self.next_id, name.to_string()));
        Step::Progress { id: self.next_id, partial: name.to_string() }
    }

    pub fn feed(&mut self, action: Action, name: &str) -> Option<Step> {
        match action {
            Action::Added | Action::Modified => match self.find(name) {
                Some(i) => Some(Step::Progress { id: self.active[i].0, partial: name.to_string() }),
                None if is_partial(name) => Some(self.start(name)),
                None => None,
            },
            Action::Removed => {
                let i = self.find(name)?;
                Some(Step::Cancel { id: self.active.remove(i).0 })
            }
            Action::RenamedOld => {
                self.old = Some(name.to_string());
                None
            }
            Action::RenamedNew => {
                let old = self.old.take();
                match old.as_deref().and_then(|o| self.find(o)) {
                    Some(i) => {
                        let id = self.active[i].0;
                        if is_partial(name) {
                            self.active[i].1 = name.to_string();
                            Some(Step::Progress { id, partial: name.to_string() })
                        } else {
                            self.active.remove(i);
                            Some(Step::Done { id, name: name.to_string() })
                        }
                    }
                    None if is_partial(name) => Some(self.start(name)),
                    None => None,
                }
            }
        }
    }
}

/// One notification buffer often holds many writes to the same file. Only the
/// last step of each download matters.
pub fn coalesce(steps: Vec<Step>) -> Vec<Step> {
    let mut out = Vec::with_capacity(steps.len());
    for (i, s) in steps.iter().enumerate() {
        let superseded = matches!(s, Step::Progress { .. }) && steps[i + 1..].iter().any(|later| later.id() == s.id());
        if !superseded {
            out.push(s.clone());
        }
    }
    out
}

/// Records of a `FILE_NOTIFY_INFORMATION` buffer: action and file name.
pub fn parse_notifications(buf: &[u8]) -> Vec<(Action, String)> {
    let mut out = Vec::new();
    let mut at = 0usize;
    while at + 12 <= buf.len() {
        let u32_at = |o: usize| u32::from_le_bytes([buf[o], buf[o + 1], buf[o + 2], buf[o + 3]]);
        let (next, action, len) = (u32_at(at) as usize, u32_at(at + 4), u32_at(at + 8) as usize);
        let name_end = at + 12 + len;
        if name_end > buf.len() {
            break;
        }
        let units: Vec<u16> = buf[at + 12..name_end].chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
        if let Some(a) = Action::from_raw(action) {
            out.push((a, String::from_utf16_lossy(&units)));
        }
        if next == 0 {
            break;
        }
        at += next;
    }
    out
}

/// Turns tracker steps into events, reading sizes from `dir`.
fn to_event(step: Step, dir: &Path) -> Event {
    let size = |p: &Path| std::fs::metadata(p).map(|m| m.len()).unwrap_or(0);
    Event::Download(match step {
        Step::Progress { id, partial } => DownloadEvent::Progress { id, bytes: size(&dir.join(&partial)), name: final_name(&partial) },
        Step::Done { id, name } => {
            let path = dir.join(&name);
            DownloadEvent::Done { id, bytes: size(&path), name, path }
        }
        Step::Cancel { id } => DownloadEvent::Cancelled { id },
    })
}

// ---------------------------------------------------------------------------
// Privacy indicators: pure logic
// ---------------------------------------------------------------------------

/// One application entry under a capability key, as stored in the registry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawApp {
    /// Sub key name: a package family name, or a path with `#` for `\`.
    pub key: String,
    pub packaged: bool,
    pub start: u64,
    pub stop: u64,
}

/// An application is using the device from the moment it starts until the
/// system writes the stop time. A zero stop time is "still running".
pub fn in_use(start: u64, stop: u64) -> bool {
    start != 0 && stop == 0
}

/// `(id, readable name)` of a sub key.
pub fn app_identity(key: &str, packaged: bool) -> (String, String) {
    if packaged {
        let family = key.split('_').next().unwrap_or(key);
        let name = family.rsplit('.').next().filter(|s| !s.is_empty()).unwrap_or(family);
        return (key.to_string(), name.to_string());
    }
    let file = key.rsplit('#').next().filter(|s| !s.is_empty()).unwrap_or(key);
    let stem = if file.len() > 4 && file[file.len() - 4..].eq_ignore_ascii_case(".exe") { &file[..file.len() - 4] } else { file };
    (file.to_string(), stem.to_string())
}

/// The applications in use for one capability, sorted and without duplicates.
pub fn collect(kind: CaptureKind, apps: &[RawApp]) -> Vec<CaptureUse> {
    let mut out: Vec<CaptureUse> = apps
        .iter()
        .filter(|a| in_use(a.start, a.stop))
        .map(|a| {
            let (id, name) = app_identity(&a.key, a.packaged);
            CaptureUse { kind, id, name }
        })
        .collect();
    out.sort_by(|a, b| a.id.to_ascii_lowercase().cmp(&b.id.to_ascii_lowercase()));
    out.dedup_by(|a, b| a.id.eq_ignore_ascii_case(&b.id));
    out
}

// ---------------------------------------------------------------------------
// Win32 plumbing
// ---------------------------------------------------------------------------

/// Manual-reset event used to wake a watcher thread for shutdown. Handles are
/// not `Send`, so the raw value travels as an integer.
struct StopEvent(isize);

impl StopEvent {
    fn new() -> windows::core::Result<Self> {
        // SAFETY: plain event creation.
        let h = unsafe { CreateEventW(None, true, false, PCWSTR::null())? };
        Ok(Self(h.0 as isize))
    }

    fn handle(&self) -> HANDLE {
        HANDLE(self.0 as *mut c_void)
    }
}

impl Drop for StopEvent {
    fn drop(&mut self) {
        // SAFETY: the watcher thread was joined before this runs.
        unsafe {
            let _ = CloseHandle(self.handle());
        }
    }
}

struct Watch {
    thread: Option<JoinHandle<()>>,
    stop: StopEvent,
}

impl Watch {
    fn spawn<F>(name: &str, run: F) -> Result<Self, crate::Error>
    where
        F: FnOnce(isize, mpsc::Sender<Result<(), crate::Error>>) + Send + 'static,
    {
        let stop = StopEvent::new()?;
        let raw = stop.0;
        let (tx, rx) = mpsc::channel();
        let thread = std::thread::Builder::new().name(name.into()).spawn(move || run(raw, tx)).map_err(|e| crate::Error::Thread(e.to_string()))?;
        match rx.recv() {
            Ok(Ok(())) => Ok(Self { thread: Some(thread), stop }),
            Ok(Err(e)) => {
                let _ = thread.join();
                Err(e)
            }
            Err(_) => Err(crate::Error::Thread(format!("{name} exited during start"))),
        }
    }
}

impl Drop for Watch {
    fn drop(&mut self) {
        // SAFETY: the event outlives the thread, which is joined below.
        unsafe {
            let _ = SetEvent(self.stop.handle());
        }
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Keeps the Downloads folder watch alive.
pub struct DownloadWatch(#[allow(dead_code)] Watch);

/// Keeps the privacy indicator watch alive.
pub struct CaptureWatch(#[allow(dead_code)] Watch);

fn downloads_dir() -> Option<PathBuf> {
    // SAFETY: the returned string is copied, then freed as documented.
    unsafe {
        let p = SHGetKnownFolderPath(&FOLDERID_Downloads, KF_FLAG_DEFAULT, None).ok()?;
        let s = p.to_string().ok();
        CoTaskMemFree(Some(p.0 as *const c_void));
        s.map(PathBuf::from)
    }
}

/// Emits `Event::Download` while a browser writes a file in the Downloads
/// folder. Works with any browser that uses a partial file name.
pub fn watch_downloads(sink: Sink) -> Result<DownloadWatch, crate::Error> {
    Watch::spawn("nb-downloads", move |stop, tx| run_downloads(sink, stop, tx)).map(DownloadWatch)
}

fn run_downloads(sink: Sink, stop: isize, tx: mpsc::Sender<Result<(), crate::Error>>) {
    let stop = HANDLE(stop as *mut c_void);
    let Some(dir) = downloads_dir() else {
        let _ = tx.send(Err(crate::Error::Thread("no Downloads folder".into())));
        return;
    };
    let wide: Vec<u16> = dir.to_string_lossy().encode_utf16().chain(std::iter::once(0)).collect();
    // SAFETY: Win32 calls on handles owned by this thread, closed before it ends.
    unsafe {
        let handle = match CreateFileW(
            PCWSTR(wide.as_ptr()),
            FILE_LIST_DIRECTORY.0,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            None,
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OVERLAPPED,
            None,
        ) {
            Ok(h) => h,
            Err(e) => {
                let _ = tx.send(Err(e.into()));
                return;
            }
        };
        let event = match CreateEventW(None, true, false, PCWSTR::null()) {
            Ok(e) => e,
            Err(e) => {
                let _ = CloseHandle(handle);
                let _ = tx.send(Err(e.into()));
                return;
            }
        };
        let _ = tx.send(Ok(()));

        let mut tracker = Tracker::default();
        // DWORD aligned, as the API requires.
        let mut buf = vec![0u32; 16 * 1024];
        let mut ov = OVERLAPPED { hEvent: event, ..Default::default() };
        loop {
            let _ = ResetEvent(event);
            let filter = FILE_NOTIFY_CHANGE_FILE_NAME | FILE_NOTIFY_CHANGE_SIZE | FILE_NOTIFY_CHANGE_LAST_WRITE;
            if ReadDirectoryChangesW(handle, buf.as_mut_ptr() as *mut c_void, (buf.len() * 4) as u32, false, filter, None, Some(&mut ov as *mut OVERLAPPED), None).is_err() {
                break;
            }
            let woke = WaitForMultipleObjects(&[event, stop], false, INFINITE);
            if woke != WAIT_OBJECT_0 {
                // Stop requested: the request must finish before the buffer goes.
                let _ = CancelIoEx(handle, Some(&ov as *const OVERLAPPED));
                let mut n = 0u32;
                let _ = GetOverlappedResult(handle, &ov, &mut n, true);
                break;
            }
            let mut n = 0u32;
            if GetOverlappedResult(handle, &ov, &mut n, false).is_err() {
                break;
            }
            // An overflow reports zero bytes: nothing to read, watch again.
            let bytes = std::slice::from_raw_parts(buf.as_ptr() as *const u8, (n as usize).min(buf.len() * 4));
            let steps: Vec<Step> = parse_notifications(bytes).into_iter().filter_map(|(a, name)| tracker.feed(a, &name)).collect();
            for step in coalesce(steps) {
                sink(to_event(step, &dir));
            }
        }
        let _ = CloseHandle(event);
        let _ = CloseHandle(handle);
    }
}

const CONSENT_ROOT: PCWSTR = w!("Software\\Microsoft\\Windows\\CurrentVersion\\CapabilityAccessManager\\ConsentStore");
const CAPABILITIES: [(&str, CaptureKind); 3] = [
    ("microphone", CaptureKind::Microphone),
    ("webcam", CaptureKind::Camera),
    ("graphicsCaptureProgrammatic", CaptureKind::Screen),
];

/// Emits `Event::Capture` whenever the set of applications using the
/// microphone, the camera or screen capture changes.
pub fn watch_capture(sink: Sink) -> Result<CaptureWatch, crate::Error> {
    Watch::spawn("nb-capture", move |stop, tx| run_capture(sink, stop, tx)).map(CaptureWatch)
}

fn run_capture(sink: Sink, stop: isize, tx: mpsc::Sender<Result<(), crate::Error>>) {
    let stop = HANDLE(stop as *mut c_void);
    // SAFETY: Win32 calls on handles owned by this thread, closed before it ends.
    unsafe {
        let mut root = HKEY::default();
        if let Err(e) = RegOpenKeyExW(HKEY_CURRENT_USER, CONSENT_ROOT, None, KEY_NOTIFY, &mut root).ok() {
            let _ = tx.send(Err(e.into()));
            return;
        }
        let event = match CreateEventW(None, false, false, PCWSTR::null()) {
            Ok(e) => e,
            Err(e) => {
                let _ = RegCloseKey(root);
                let _ = tx.send(Err(e.into()));
                return;
            }
        };
        let _ = tx.send(Ok(()));

        let mut last: Vec<CaptureUse> = Vec::new();
        loop {
            // Armed before reading, so a change during the read is not lost.
            let armed = RegNotifyChangeKeyValue(root, true, REG_NOTIFY_CHANGE_LAST_SET | REG_NOTIFY_CHANGE_NAME, Some(event), true);
            let now = read_all();
            if now != last {
                last = now.clone();
                sink(Event::Capture(now));
            }
            if armed.is_err() {
                break;
            }
            if WaitForMultipleObjects(&[event, stop], false, INFINITE) != WAIT_OBJECT_0 {
                break;
            }
        }
        let _ = CloseHandle(event);
        let _ = RegCloseKey(root);
    }
}

fn read_all() -> Vec<CaptureUse> {
    let mut all = Vec::new();
    for (name, kind) in CAPABILITIES {
        all.extend(collect(kind, &read_capability(name)));
    }
    all
}

fn read_capability(capability: &str) -> Vec<RawApp> {
    let path: Vec<u16> = format!("Software\\Microsoft\\Windows\\CurrentVersion\\CapabilityAccessManager\\ConsentStore\\{capability}\0").encode_utf16().collect();
    let mut apps = Vec::new();
    // SAFETY: keys opened here are closed here.
    unsafe {
        let mut key = HKEY::default();
        if RegOpenKeyExW(HKEY_CURRENT_USER, PCWSTR(path.as_ptr()), None, KEY_READ, &mut key).is_err() {
            return apps;
        }
        for name in sub_keys(key) {
            if name == "NonPackaged" {
                let mut inner = HKEY::default();
                if RegOpenKeyExW(key, &windows::core::HSTRING::from(name.as_str()), None, KEY_READ, &mut inner).is_ok() {
                    for app in sub_keys(inner) {
                        apps.extend(read_app(inner, &app, false));
                    }
                    let _ = RegCloseKey(inner);
                }
            } else {
                apps.extend(read_app(key, &name, true));
            }
        }
        let _ = RegCloseKey(key);
    }
    apps
}

fn sub_keys(key: HKEY) -> Vec<String> {
    let mut out = Vec::new();
    for index in 0.. {
        let mut buf = [0u16; 512];
        let mut len = buf.len() as u32;
        // SAFETY: `key` is open and the buffer is local.
        if unsafe { RegEnumKeyExW(key, index, Some(PWSTR(buf.as_mut_ptr())), &mut len, None, None, None, None) }.is_err() {
            break;
        }
        out.push(String::from_utf16_lossy(&buf[..len as usize]));
    }
    out
}

fn read_app(parent: HKEY, name: &str, packaged: bool) -> Option<RawApp> {
    let mut key = HKEY::default();
    // SAFETY: the key opened here is closed here.
    unsafe {
        if RegOpenKeyExW(parent, &windows::core::HSTRING::from(name), None, KEY_READ, &mut key).is_err() {
            return None;
        }
        let app = RawApp { key: name.to_string(), packaged, start: read_qword(key, w!("LastUsedTimeStart")), stop: read_qword(key, w!("LastUsedTimeStop")) };
        let _ = RegCloseKey(key);
        Some(app)
    }
}

/// A missing or malformed value reads as zero.
fn read_qword(key: HKEY, value: PCWSTR) -> u64 {
    let mut data = [0u8; 8];
    let mut len = 8u32;
    // SAFETY: `key` is open and the buffer holds 8 bytes.
    let ok = unsafe { RegQueryValueExW(key, value, None, None, Some(data.as_mut_ptr()), Some(&mut len)) }.is_ok();
    if ok && len == 8 { u64::from_le_bytes(data) } else { 0 }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed_all(t: &mut Tracker, events: &[(Action, &str)]) -> Vec<Step> {
        events.iter().filter_map(|(a, n)| t.feed(*a, n)).collect()
    }

    #[test]
    fn partial_files_are_recognised() {
        for name in ["a.zip.crdownload", "Unconfirmed 123.crdownload", "movie.mp4.part", "x.TMP", "setup.exe.CRDOWNLOAD", "a.opdownload"] {
            assert!(is_partial(name), "{name}");
        }
        for name in ["a.zip", "report.pdf", "tmp", ".tmp", "partial.txt", "notes.parted", ""] {
            assert!(!is_partial(name), "{name}");
        }
    }

    #[test]
    fn the_final_name_drops_the_partial_extension() {
        assert_eq!(final_name("a.zip.crdownload"), "a.zip");
        assert_eq!(final_name("movie.mp4.part"), "movie.mp4");
        assert_eq!(final_name("Unconfirmed 4242.crdownload"), "", "name not chosen yet");
        assert_eq!(final_name("plain.pdf"), "plain.pdf");
    }

    #[test]
    fn a_chromium_download_goes_from_unconfirmed_to_done() {
        let mut t = Tracker::default();
        let steps = feed_all(
            &mut t,
            &[
                (Action::Added, "Unconfirmed 1.crdownload"),
                (Action::Modified, "Unconfirmed 1.crdownload"),
                (Action::RenamedOld, "Unconfirmed 1.crdownload"),
                (Action::RenamedNew, "a.zip.crdownload"),
                (Action::Modified, "a.zip.crdownload"),
                (Action::RenamedOld, "a.zip.crdownload"),
                (Action::RenamedNew, "a.zip"),
            ],
        );
        assert_eq!(
            steps,
            vec![
                Step::Progress { id: 1, partial: "Unconfirmed 1.crdownload".into() },
                Step::Progress { id: 1, partial: "Unconfirmed 1.crdownload".into() },
                Step::Progress { id: 1, partial: "a.zip.crdownload".into() },
                Step::Progress { id: 1, partial: "a.zip.crdownload".into() },
                Step::Done { id: 1, name: "a.zip".into() },
            ]
        );
    }

    #[test]
    fn a_firefox_download_is_done_on_rename() {
        let mut t = Tracker::default();
        let steps = feed_all(&mut t, &[(Action::Added, "iso.part"), (Action::RenamedOld, "iso.part"), (Action::RenamedNew, "iso.iso")]);
        assert_eq!(steps.last(), Some(&Step::Done { id: 1, name: "iso.iso".into() }));
    }

    #[test]
    fn a_cancelled_download_is_dropped() {
        let mut t = Tracker::default();
        t.feed(Action::Added, "a.zip.crdownload");
        assert_eq!(t.feed(Action::Removed, "a.zip.crdownload"), Some(Step::Cancel { id: 1 }));
        assert_eq!(t.feed(Action::Removed, "a.zip.crdownload"), None, "already gone");
    }

    #[test]
    fn ordinary_files_and_renames_are_ignored() {
        let mut t = Tracker::default();
        let steps = feed_all(&mut t, &[(Action::Added, "photo.jpg"), (Action::Modified, "photo.jpg"), (Action::RenamedOld, "a.txt"), (Action::RenamedNew, "b.txt"), (Action::Removed, "photo.jpg")]);
        assert!(steps.is_empty());
    }

    #[test]
    fn a_rename_into_a_partial_name_starts_a_download() {
        let mut t = Tracker::default();
        let steps = feed_all(&mut t, &[(Action::RenamedOld, "tmpfile"), (Action::RenamedNew, "x.bin.crdownload")]);
        assert_eq!(steps, vec![Step::Progress { id: 1, partial: "x.bin.crdownload".into() }]);
    }

    #[test]
    fn two_downloads_keep_their_own_ids() {
        let mut t = Tracker::default();
        let steps = feed_all(&mut t, &[(Action::Added, "a.crdownload"), (Action::Added, "b.crdownload"), (Action::RenamedOld, "a.crdownload"), (Action::RenamedNew, "a.bin")]);
        assert_eq!(steps[1], Step::Progress { id: 2, partial: "b.crdownload".into() });
        assert_eq!(steps[2], Step::Done { id: 1, name: "a.bin".into() });
    }

    #[test]
    fn a_burst_of_writes_collapses_to_one_update() {
        let p = |id| Step::Progress { id, partial: "a.part".into() };
        let out = coalesce(vec![p(1), p(1), p(2), p(1), Step::Done { id: 2, name: "b".into() }]);
        assert_eq!(out, vec![p(1), Step::Done { id: 2, name: "b".into() }]);
    }

    #[test]
    fn notification_records_are_parsed() {
        fn record(next: u32, action: u32, name: &str) -> Vec<u8> {
            let units: Vec<u8> = name.encode_utf16().flat_map(|u| u.to_le_bytes()).collect();
            let mut v = Vec::new();
            v.extend_from_slice(&next.to_le_bytes());
            v.extend_from_slice(&action.to_le_bytes());
            v.extend_from_slice(&(units.len() as u32).to_le_bytes());
            v.extend(units);
            v
        }
        let mut first = record(0, 1, "é.part");
        let next = first.len() as u32;
        first[..4].copy_from_slice(&next.to_le_bytes());
        first.extend(record(0, 3, "é.part"));
        assert_eq!(parse_notifications(&first), vec![(Action::Added, "é.part".into()), (Action::Modified, "é.part".into())]);
        assert!(parse_notifications(&first[..10]).is_empty(), "truncated");
        assert!(parse_notifications(&[]).is_empty());
    }

    #[test]
    fn a_zero_stop_time_means_in_use() {
        assert!(in_use(133_000_000_000_000_000, 0));
        assert!(!in_use(133_000_000_000_000_000, 133_000_000_100_000_000));
        assert!(!in_use(0, 0), "never used");
    }

    #[test]
    fn registry_keys_become_readable_names() {
        assert_eq!(app_identity("C:#Program Files#Zoom#bin#Zoom.exe", false), ("Zoom.exe".into(), "Zoom".into()));
        assert_eq!(app_identity("C:#Windows#System32#SnippingTool.EXE", false).1, "SnippingTool");
        assert_eq!(app_identity("Microsoft.WindowsCamera_8wekyb3d8bbwe", true), ("Microsoft.WindowsCamera_8wekyb3d8bbwe".into(), "WindowsCamera".into()));
        assert_eq!(app_identity("Weird", true).1, "Weird");
    }

    #[test]
    fn only_running_applications_are_collected() {
        let app = |key: &str, packaged, start, stop| RawApp { key: key.into(), packaged, start, stop };
        let apps = [
            app("C:#a#Teams.exe", false, 5, 0),
            app("C:#a#Old.exe", false, 5, 9),
            app("Microsoft.WindowsCamera_x", true, 7, 0),
            app("C:#b#teams.exe", false, 8, 0),
        ];
        let got = collect(CaptureKind::Camera, &apps);
        let ids: Vec<&str> = got.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids, vec!["Microsoft.WindowsCamera_x", "Teams.exe"], "sorted, duplicate exe names merged");
        assert!(got.iter().all(|c| c.kind == CaptureKind::Camera));
    }
}
