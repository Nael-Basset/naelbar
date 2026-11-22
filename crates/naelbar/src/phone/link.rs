//! The PC side of the phone link (`--phone-link`): a hidden window holding a
//! WebView2 on the hosted site in its "PC" role. The page answers the phone's
//! WebRTC offers and relays what arrives on the data channels here, where it
//! is validated, turned into input, or written to `Downloads\naelbar`.
//!
//! Shares the agenda's WebView2 profile, so it is signed in once the agenda
//! is. Nothing but input messages, file chunks and acknowledgements crosses
//! the bridge; no identity or token reaches this process.

use std::cell::RefCell;
use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};
use webview2_com::Microsoft::Web::WebView2::Win32::{
    CreateCoreWebView2EnvironmentWithOptions, ICoreWebView2, ICoreWebView2Controller, ICoreWebView2Environment,
    ICoreWebView2EnvironmentOptions, ICoreWebView2WebMessageReceivedEventArgs,
};
use webview2_com::{
    CreateCoreWebView2ControllerCompletedHandler, CreateCoreWebView2EnvironmentCompletedHandler,
    NavigationCompletedEventHandler, NewWindowRequestedEventHandler, WebMessageReceivedEventHandler, take_pwstr,
};
use windows::Win32::Foundation::{CloseHandle, E_FAIL, E_POINTER, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx, CoTaskMemFree};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::{CreateMutexW, INFINITE, OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject};
use windows::Win32::UI::Shell::{FOLDERID_Downloads, KF_FLAG_DEFAULT, SHGetKnownFolderPath};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetMessageW, KillTimer, MSG, PostMessageW,
    PostQuitMessage, RegisterClassW, SetTimer, TranslateMessage, WINDOW_EX_STYLE, WM_CLOSE, WM_TIMER, WNDCLASSW,
    WS_OVERLAPPEDWINDOW,
};
use windows::core::{BOOL, Error, HSTRING, PCWSTR, PWSTR, Result, w};

use super::input::{self, Injector};

const PAGE_URL: PCWSTR = w!("https://naelbar-agenda-eadafc.web.app/?pc=1&v=10");
/// Retries a page that failed to load (no network at boot), and only then.
const RETRY_TIMER: usize = 1;
const RETRY_MS: u32 = 60_000;
/// Largest file accepted from the phone.
const MAX_FILE: u64 = 4 << 30;
const MAX_NAME: usize = 120;

struct Incoming {
    id: String,
    part: PathBuf,
    dest: PathBuf,
    file: Option<File>,
    size: u64,
    written: u64,
}

impl Incoming {
    fn discard(mut self) {
        self.file = None;
        let _ = std::fs::remove_file(&self.part);
    }
}

thread_local! {
    static CONTROLLER: RefCell<Option<ICoreWebView2Controller>> = const { RefCell::new(None) };
    static INJECTOR: RefCell<Injector> = RefCell::new(Injector::default());
    static INCOMING: RefCell<Option<Incoming>> = const { RefCell::new(None) };
}

// ---------------------------------------------------------------- files

/// Where files from the phone land.
fn inbox() -> Option<PathBuf> {
    let downloads = unsafe {
        let p = SHGetKnownFolderPath(&FOLDERID_Downloads, KF_FLAG_DEFAULT, None).ok()?;
        let s = p.to_string().ok();
        CoTaskMemFree(Some(p.0 as *const _));
        PathBuf::from(s?)
    };
    let dir = downloads.join("naelbar");
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir)
}

/// A plain file name: no folders, no characters Windows refuses, no device
/// names, at most `MAX_NAME` characters with the extension kept.
pub fn safe_name(raw: &str) -> String {
    let last = raw.rsplit(['/', '\\']).next().unwrap_or("");
    let clean: String = last.chars().map(|c| if c.is_control() || "<>:\"/\\|?*".contains(c) { '_' } else { c }).collect();
    let clean = clean.trim().trim_end_matches(['.', ' ']).trim_start_matches('.').to_string();
    let (stem, ext) = match clean.rfind('.') {
        Some(i) if i > 0 && clean.len() - i <= 10 => (&clean[..i], &clean[i..]),
        _ => (clean.as_str(), ""),
    };
    let reserved = ["con", "prn", "aux", "nul", "com1", "com2", "com3", "com4", "lpt1", "lpt2", "lpt3"];
    let stem = if stem.is_empty() || reserved.contains(&stem.to_ascii_lowercase().as_str()) { format!("fichier{stem}") } else { stem.to_string() };
    let keep = MAX_NAME.saturating_sub(ext.chars().count()).max(1);
    format!("{}{}", stem.chars().take(keep).collect::<String>(), ext)
}

/// `name`, or `name (2)`, `name (3)`… when it is taken.
fn unique(dir: &Path, name: &str) -> PathBuf {
    let first = dir.join(name);
    if !first.exists() {
        return first;
    }
    let (stem, ext) = match name.rfind('.') {
        Some(i) if i > 0 => (&name[..i], &name[i..]),
        _ => (name, ""),
    };
    (2..10_000).map(|n| dir.join(format!("{stem} ({n}){ext}"))).find(|p| !p.exists()).unwrap_or(first)
}

/// Standard base64, as `btoa` writes it.
pub fn base64_decode(s: &str) -> Option<Vec<u8>> {
    let val = |c: u8| -> Option<u32> {
        Some(match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        } as u32)
    };
    let b = s.trim_end_matches('=').as_bytes();
    if b.len() % 4 == 1 {
        return None;
    }
    let mut out = Vec::with_capacity(b.len() * 3 / 4);
    for chunk in b.chunks(4) {
        let mut n = 0u32;
        for (i, &c) in chunk.iter().enumerate() {
            n |= val(c)? << (18 - 6 * i);
        }
        out.push((n >> 16) as u8);
        if chunk.len() > 2 {
            out.push((n >> 8) as u8);
        }
        if chunk.len() > 3 {
            out.push(n as u8);
        }
    }
    Some(out)
}

/// The folder scans are filed under: the lessons folder from the settings,
/// or the inbox.
fn courses_root() -> Option<PathBuf> {
    let root = crate::config::Config::load().courses_root;
    let root = PathBuf::from(root.trim());
    if root.is_absolute() && root.is_dir() { Some(root) } else { inbox() }
}

/// `a/b/c` under `root`, each part a plain existing folder name. Refuses
/// `..`, drive letters, and links leading out of `root`.
pub fn resolve(root: &Path, rel: &str) -> Option<PathBuf> {
    let mut p = root.to_path_buf();
    for part in rel.split('/').filter(|c| !c.is_empty()) {
        if part == "." || part == ".." || safe_name(part) != part || part.chars().count() > MAX_NAME {
            return None;
        }
        p.push(part);
    }
    let (canon, base) = (p.canonicalize().ok()?, root.canonicalize().ok()?);
    (canon.starts_with(&base) && canon.is_dir()).then_some(p)
}

/// Folder browsing for the phone's scan page.
fn on_dirs(webview: &ICoreWebView2, d: &Value) {
    let q = d.get("q").and_then(Value::as_u64).unwrap_or(0);
    let rel = d.get("path").and_then(Value::as_str).unwrap_or("");
    let fail = |webview: &ICoreWebView2| reply(webview, json!({"dirsReply": {"q": q, "ok": false}}));
    let Some(root) = courses_root() else { return fail(webview) };
    let Some(dir) = resolve(&root, rel) else { return fail(webview) };
    let root_name = root.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| root.display().to_string());
    match d.get("op").and_then(Value::as_str) {
        Some("list") => {
            let mut dirs: Vec<String> = std::fs::read_dir(&dir)
                .map(|it| {
                    it.flatten()
                        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
                        .map(|e| e.file_name().to_string_lossy().into_owned())
                        .filter(|n| !n.starts_with(['.', '$']))
                        .take(500)
                        .collect()
                })
                .unwrap_or_default();
            dirs.sort_by_key(|n| n.to_lowercase());
            reply(webview, json!({"dirsReply": {"q": q, "ok": true, "dirs": dirs, "root": root_name}}));
        }
        Some("mkdir") => {
            let raw = d.get("name").and_then(Value::as_str).unwrap_or("").trim();
            let name = safe_name(raw);
            if !raw.is_empty() && std::fs::create_dir_all(dir.join(&name)).is_ok() {
                reply(webview, json!({"dirsReply": {"q": q, "ok": true, "name": name}}));
            } else {
                fail(webview);
            }
        }
        _ => fail(webview),
    }
}

fn valid_id(s: &str) -> bool {
    !s.is_empty() && s.len() <= 64 && s.bytes().all(|c| c.is_ascii_hexdigit())
}

fn reply(webview: &ICoreWebView2, v: Value) {
    let s = HSTRING::from(v.to_string());
    let _ = unsafe { webview.PostWebMessageAsJson(PCWSTR(s.as_ptr())) };
}

fn on_file(webview: &ICoreWebView2, f: &Value) {
    match f.get("op").and_then(Value::as_str) {
        Some("start") => {
            if let Some(old) = INCOMING.take() {
                old.discard();
            }
            let id = f.get("id").and_then(Value::as_str).unwrap_or("");
            let size = f.get("size").and_then(Value::as_f64).unwrap_or(-1.0);
            if !valid_id(id) || !(0.0..=MAX_FILE as f64).contains(&size) {
                return;
            }
            // A scan names its folder under the lessons folder; anything else
            // lands in the inbox.
            let dir = match f.get("dir").and_then(Value::as_str) {
                Some(rel) => courses_root().and_then(|root| resolve(&root, rel)),
                None => inbox(),
            };
            let Some(dir) = dir else {
                INCOMING.set(Some(Incoming { id: id.to_string(), part: PathBuf::new(), dest: PathBuf::new(), file: None, size: size as u64, written: 0 }));
                return;
            };
            let dest = unique(&dir, &safe_name(f.get("name").and_then(Value::as_str).unwrap_or("")));
            let part = PathBuf::from(format!("{}.part", dest.display()));
            let file = File::create(&part).ok();
            INCOMING.set(Some(Incoming { id: id.to_string(), part, dest, file, size: size as u64, written: 0 }));
        }
        Some("data") => INCOMING.with_borrow_mut(|inc| {
            let Some(inc) = inc else { return };
            let ok = f.get("b64").and_then(Value::as_str).and_then(base64_decode).is_some_and(|bytes| {
                inc.written += bytes.len() as u64;
                inc.written <= inc.size && inc.file.as_mut().is_some_and(|file| file.write_all(&bytes).is_ok())
            });
            if !ok {
                // Kept as a failed transfer until its end, then refused.
                inc.file = None;
            }
        }),
        Some("end") => {
            let id = f.get("id").and_then(Value::as_str).unwrap_or("");
            let Some(mut inc) = INCOMING.take() else { return reply(webview, json!({"fileDone": id, "ok": false})) };
            if inc.id != id {
                inc.discard();
                return reply(webview, json!({"fileDone": id, "ok": false}));
            }
            let complete = inc.written == inc.size && inc.file.take().is_some_and(|mut f| f.flush().is_ok() && f.sync_all().is_ok());
            let saved = complete && std::fs::rename(&inc.part, &inc.dest).is_ok();
            if !saved {
                inc.discard();
            }
            reply(webview, json!({"fileDone": id, "ok": saved}));
        }
        _ => {}
    }
}

// ---------------------------------------------------------------- page bridge

fn peer_closed() {
    INJECTOR.with_borrow_mut(|inj| input::send(&inj.release_all()));
    if let Some(inc) = INCOMING.take() {
        inc.discard();
    }
}

unsafe fn on_web_message(webview: &ICoreWebView2, args: &ICoreWebView2WebMessageReceivedEventArgs) -> Result<()> {
    let mut source = PWSTR::null();
    let mut message = PWSTR::null();
    unsafe {
        args.Source(&mut source)?;
        if !crate::agendasync::is_host_origin(&take_pwstr(source)) {
            return Ok(());
        }
        args.WebMessageAsJson(&mut message)?;
    }
    let Ok(v) = serde_json::from_str::<Value>(&take_pwstr(message)) else { return Ok(()) };
    if let Some(text) = v.get("naelbarInput").and_then(Value::as_str) {
        if let Some(m) = input::parse(text) {
            INJECTOR.with_borrow_mut(|inj| input::send(&inj.plan(&m)));
        }
    } else if let Some(f) = v.get("naelbarFile") {
        on_file(webview, f);
    } else if let Some(d) = v.get("naelbarDirs") {
        on_dirs(webview, d);
    } else if v.get("naelbarPeer").and_then(Value::as_str) == Some("closed") {
        peer_closed();
    }
    Ok(())
}

// ---------------------------------------------------------------- window and webview

/// Hands the last room the extension reported to the page, which files it
/// in the account.
fn send_ytm_room() {
    let Some(room) = super::ytm_room_path().and_then(|p| std::fs::read_to_string(p).ok()) else { return };
    let room = room.trim();
    if !nb_integrations::valid_ytm_room(room) {
        return;
    }
    if let Some(wv) = controller().and_then(|c| unsafe { c.CoreWebView2() }.ok()) {
        reply(&wv, json!({"ytmRoom": room}));
    }
}

fn controller() -> Option<ICoreWebView2Controller> {
    CONTROLLER.with_borrow(Clone::clone)
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_TIMER if wparam.0 == RETRY_TIMER => {
            if let Some(c) = controller() {
                unsafe {
                    if let Ok(wv) = c.CoreWebView2() {
                        let _ = wv.Navigate(PAGE_URL);
                    }
                }
            }
            LRESULT(0)
        }
        super::WM_YTM_ROOM => {
            send_ytm_room();
            LRESULT(0)
        }
        WM_CLOSE => {
            unsafe { PostQuitMessage(0) };
            LRESULT(0)
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

unsafe fn on_controller(hwnd: HWND, controller: ICoreWebView2Controller) -> Result<()> {
    let mut token = 0i64;
    unsafe {
        crate::webusage::fit_to_client(hwnd, &controller)?;
        let webview = controller.CoreWebView2()?;
        let settings = webview.Settings()?;
        settings.SetAreDevToolsEnabled(false)?;
        settings.SetAreDefaultContextMenusEnabled(false)?;
        settings.SetAreHostObjectsAllowed(false)?;
        settings.SetIsStatusBarEnabled(false)?;

        let completed = NavigationCompletedEventHandler::create(Box::new(move |_, args| {
            if let Some(args) = args {
                let mut ok = BOOL(0);
                let _ = args.IsSuccess(&mut ok);
                if ok.as_bool() {
                    let _ = KillTimer(Some(hwnd), RETRY_TIMER);
                    send_ytm_room();
                } else {
                    SetTimer(Some(hwnd), RETRY_TIMER, RETRY_MS, None);
                }
            }
            Ok(())
        }));
        webview.add_NavigationCompleted(&completed, &mut token)?;

        let received = WebMessageReceivedEventHandler::create(Box::new(|sender, args| {
            if let (Some(sender), Some(args)) = (sender, args) {
                let _ = on_web_message(&sender, &args);
            }
            Ok(())
        }));
        webview.add_WebMessageReceived(&received, &mut token)?;

        // Nothing opens new windows from here.
        let new_window = NewWindowRequestedEventHandler::create(Box::new(|_, args| {
            if let Some(args) = args {
                let _ = args.SetHandled(true);
            }
            Ok(())
        }));
        webview.add_NewWindowRequested(&new_window, &mut token)?;

        CONTROLLER.set(Some(controller));
        webview.Navigate(PAGE_URL)
    }
}

unsafe fn on_environment(hwnd: HWND, environment: ICoreWebView2Environment) -> Result<()> {
    let done = CreateCoreWebView2ControllerCompletedHandler::create(Box::new(move |result, controller| {
        let started = result.and_then(|()| controller.ok_or_else(|| Error::from(E_POINTER))).and_then(|c| unsafe { on_controller(hwnd, c) });
        if started.is_err() {
            unsafe { PostQuitMessage(1) };
        }
        Ok(())
    }));
    unsafe { environment.CreateCoreWebView2Controller(hwnd, &done) }
}

unsafe fn start_webview(hwnd: HWND) -> Result<()> {
    let folder = crate::agendasync::user_data_folder().ok_or_else(|| Error::from(E_FAIL))?;
    let folder = HSTRING::from(folder.as_os_str());
    let done = CreateCoreWebView2EnvironmentCompletedHandler::create(Box::new(move |result, environment| {
        let started = result.and_then(|()| environment.ok_or_else(|| Error::from(E_POINTER))).and_then(|e| unsafe { on_environment(hwnd, e) });
        if started.is_err() {
            unsafe { PostQuitMessage(1) };
        }
        Ok(())
    }));
    unsafe { CreateCoreWebView2EnvironmentWithOptions(PCWSTR::null(), PCWSTR(folder.as_ptr()), None::<&ICoreWebView2EnvironmentOptions>, &done) }
}

/// Closes this helper when the island that started it ends, without polling:
/// a thread waits on the parent's process handle.
fn follow_parent(hwnd: HWND) {
    let Some(pid) = std::env::args().find_map(|a| a.strip_prefix("--parent=").and_then(|v| v.parse::<u32>().ok())) else { return };
    let Ok(h) = (unsafe { OpenProcess(PROCESS_SYNCHRONIZE, false, pid) }) else {
        unsafe {
            let _ = PostMessageW(Some(hwnd), WM_CLOSE, WPARAM(0), LPARAM(0));
        }
        return;
    };
    let (h, hwnd) = (h.0 as isize, hwnd.0 as isize);
    let _ = std::thread::Builder::new().name("phone-link-parent".into()).spawn(move || unsafe {
        let h = windows::Win32::Foundation::HANDLE(h as *mut _);
        WaitForSingleObject(h, INFINITE);
        let _ = CloseHandle(h);
        let _ = PostMessageW(Some(HWND(hwnd as *mut _)), WM_CLOSE, WPARAM(0), LPARAM(0));
    });
}

unsafe fn host() -> Result<()> {
    unsafe {
        // One helper at a time.
        let _mutex = CreateMutexW(None, true, w!("Local\\naelbar.phone-link"))?;
        if windows::Win32::Foundation::GetLastError() == windows::Win32::Foundation::ERROR_ALREADY_EXISTS {
            return Ok(());
        }
        CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok()?;
        // Same awareness as the agenda sync and sign-in processes: WebView2
        // refuses to share one profile between hosts that differ here
        // (0x8007139F when the second one creates its view).
        let _ = windows::Win32::UI::HiDpi::SetProcessDpiAwarenessContext(windows::Win32::UI::HiDpi::DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        let instance = GetModuleHandleW(None)?.into();
        let class_name = w!("naelbar.phonelink");
        let class = WNDCLASSW { lpfnWndProc: Some(wndproc), hInstance: instance, lpszClassName: class_name, ..Default::default() };
        if RegisterClassW(&class) == 0 {
            return Err(Error::from_thread());
        }
        // Never shown.
        let hwnd = CreateWindowExW(WINDOW_EX_STYLE(0), class_name, w!("naelbar phone link"), WS_OVERLAPPEDWINDOW, 0, 0, 800, 600, None, None, Some(instance), None)?;
        follow_parent(hwnd);
        start_webview(hwnd)?;
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).0 > 0 {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        peer_closed();
        if let Some(c) = CONTROLLER.take() {
            let _ = c.Close();
        }
        let _ = DestroyWindow(hwnd);
    }
    Ok(())
}

pub fn run() -> i32 {
    match unsafe { host() } {
        Ok(()) => 0,
        Err(_) => 3,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_round_trip() {
        assert_eq!(base64_decode("aGVsbG8=").unwrap(), b"hello");
        assert_eq!(base64_decode("aGVsbG8hIQ==").unwrap(), b"hello!!");
        assert_eq!(base64_decode("").unwrap(), b"");
        assert!(base64_decode("a").is_none());
        assert!(base64_decode("a$==").is_none());
    }

    #[test]
    fn file_names_stay_in_the_inbox() {
        assert_eq!(safe_name("photo.jpg"), "photo.jpg");
        assert_eq!(safe_name("..\\..\\Windows\\evil.exe"), "evil.exe");
        assert_eq!(safe_name("a/b/c:d?.pdf"), "c_d_.pdf");
        assert_eq!(safe_name("CON.txt"), "fichierCON.txt");
        assert_eq!(safe_name(""), "fichier");
        assert_eq!(safe_name("..."), "fichier");
        assert_eq!(safe_name(&format!("{}.png", "x".repeat(300))).chars().count(), MAX_NAME);
    }

    #[test]
    fn scan_folders_stay_under_the_root() {
        let root = std::env::temp_dir().join(format!("naelbar-root-test-{}", std::process::id()));
        std::fs::create_dir_all(root.join("Maths").join("TD")).unwrap();
        assert_eq!(resolve(&root, ""), Some(root.clone()));
        assert_eq!(resolve(&root, "Maths/TD"), Some(root.join("Maths").join("TD")));
        for bad in ["..", "Maths/..", "Maths/../..", "C:", "Maths\\TD", "Absent", "./Maths"] {
            assert_eq!(resolve(&root, bad), None, "{bad}");
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn taken_names_get_a_number() {
        let dir = std::env::temp_dir().join(format!("naelbar-link-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(unique(&dir, "a.txt"), dir.join("a.txt"));
        std::fs::write(dir.join("a.txt"), "x").unwrap();
        assert_eq!(unique(&dir, "a.txt"), dir.join("a (2).txt"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
