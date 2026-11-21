//! Syncs the local agenda with the hosted one through a private WebView2 profile.
//!
//! Runs in its own short-lived process (`--agenda-sync`, `--agenda-login`), only
//! on an event. The page bridge `window.naelbarSync` exchanges events and notes
//! only; no identity, cookie or token ever reaches this process.

use std::cell::{Cell, RefCell};
use std::path::PathBuf;

use serde_json::{Value, json};
use webview2_com::Microsoft::Web::WebView2::Win32::{
    COREWEBVIEW2_MOVE_FOCUS_REASON_PROGRAMMATIC, COREWEBVIEW2_WEB_ERROR_STATUS,
    COREWEBVIEW2_WEB_ERROR_STATUS_UNKNOWN, CreateCoreWebView2EnvironmentWithOptions, ICoreWebView2,
    ICoreWebView2Controller, ICoreWebView2Environment, ICoreWebView2EnvironmentOptions,
    ICoreWebView2NavigationCompletedEventArgs, ICoreWebView2WebMessageReceivedEventArgs,
};
use webview2_com::{
    CreateCoreWebView2ControllerCompletedHandler, CreateCoreWebView2EnvironmentCompletedHandler,
    ExecuteScriptCompletedHandler, NavigationCompletedEventHandler, NewWindowRequestedEventHandler,
    WebMessageReceivedEventHandler, take_pwstr,
};
use windows::Win32::Foundation::{E_FAIL, E_POINTER, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::Graphics::Gdi::{GetStockObject, HBRUSH, WHITE_BRUSH};
use windows::Win32::System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::{DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, FindWindowW, GetMessageW, IDC_ARROW, KillTimer,
    LoadCursorW, MSG, PostMessageW, PostQuitMessage, RegisterClassW, SW_SHOW, SetTimer, ShowWindow, TranslateMessage,
    WINDOW_EX_STYLE, WM_CLOSE, WM_MOVE, WM_SETFOCUS, WM_SIZE, WM_TIMER, WNDCLASSW, WS_OVERLAPPEDWINDOW,
};
use windows::core::{BOOL, Error, HSTRING, PCWSTR, PWSTR, Result, w};

use crate::agenda::{self, Agenda, Event, Note, WM_AGENDA_CHANGED};
use crate::config::CourseStyle;
use crate::webusage::{EXIT_FAILED, EXIT_OK, EXIT_SIGN_IN, fit_to_client, login_rect, on_new_window};

const HOST: &str = "naelbar-agenda-eadafc.web.app";
/// `desktop=1` makes the page sign in by redirect, as an embedded view has no popup.
const PAGE_URL: PCWSTR = w!("https://naelbar-agenda-eadafc.web.app/?desktop=1&v=8");

const DEADLINE_TIMER: usize = 1;
const DEADLINE_MS: u32 = 30_000;
const PROBE_TIMER: usize = 2;
const PROBE_MS: u32 = 5_000;

const MAX_RECORDS: usize = 5_000;
const MAX_TITLE: usize = 200;
const MAX_TEXT: usize = 500;
const MAX_ID: usize = 64;
/// Year 9999, the latest instant a sane record can carry.
const MAX_STAMP: f64 = 253_402_300_799_999.0;

/// Waits for the page, reads the hosted data. Only events and notes leave the page.
const PULL_SCRIPT: &str = r#"(async () => {
  const post = m => window.chrome.webview.postMessage(m);
  try {
    if (location.hostname !== 'naelbar-agenda-eadafc.web.app') return;
    const s = window.naelbarSync;
    if (!s) { post({error: 'no_bridge'}); return; }
    if (s.ready) await s.ready();
    const r = await s.pull();
    if (!r || !r.signedIn) { post({auth: false}); return; }
    post({events: r.events, notes: r.notes, styles: r.styles || null});
  } catch (e) {
    post({error: 'exception'});
  }
})();"#;

/// Signs the page out; the Google account is forgotten by this PC.
const LOGOUT_SCRIPT: &str = r#"(async () => {
  const post = m => window.chrome.webview.postMessage(m);
  try {
    if (location.hostname !== 'naelbar-agenda-eadafc.web.app') return;
    const s = window.naelbarSync;
    if (s.ready) await s.ready();
    await s.signOut();
    post({loggedOut: true});
  } catch (e) {
    post({error: 'logout'});
  }
})();"#;

fn push_script(data: &str) -> String {
    format!(
        r#"(async () => {{
  const post = m => window.chrome.webview.postMessage(m);
  try {{
    await window.naelbarSync.push({data});
    post({{pushed: true}});
  }} catch (e) {{
    post({{error: 'push'}});
  }}
}})();"#
    )
}

#[derive(Clone, Copy, PartialEq)]
enum Stage {
    Idle,
    Pulling,
    Pushing,
}

thread_local! {
    static LOGIN: Cell<bool> = const { Cell::new(false) };
    static LOGOUT: Cell<bool> = const { Cell::new(false) };
    static EXIT: Cell<Option<i32>> = const { Cell::new(None) };
    static STAGE: Cell<Stage> = const { Cell::new(Stage::Idle) };
    static CONTROLLER: RefCell<Option<ICoreWebView2Controller>> = const { RefCell::new(None) };
}

fn valid_id(id: &Value) -> Option<String> {
    let id = id.as_str()?;
    (!id.is_empty() && id.len() <= MAX_ID && id.bytes().all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c)))
        .then(|| id.to_string())
}

fn valid_text(value: &Value, max: usize) -> Option<String> {
    let s = value.as_str()?;
    (s.chars().count() <= max).then(|| s.to_string())
}

fn valid_stamp(value: &Value) -> Option<u64> {
    let n = value.as_f64()?;
    (n.is_finite() && (0.0..=MAX_STAMP).contains(&n)).then_some(n as u64)
}

fn valid_time(value: &Value) -> Option<Option<String>> {
    if value.is_null() {
        return Some(None);
    }
    let s = value.as_str()?;
    let b = s.as_bytes();
    let well_formed = b.len() == 5 && b[2] == b':' && [0, 1, 3, 4].iter().all(|&i| b[i].is_ascii_digit());
    (well_formed && s[0..2].parse::<u32>().ok()? < 24 && s[3..5].parse::<u32>().ok()? < 60).then(|| Some(s.to_string()))
}

/// `deleted` and `done` may be absent (false) but not of another type.
fn flag(record: &Value, key: &str) -> Option<bool> {
    match record.get(key) {
        None | Some(Value::Null) => Some(false),
        Some(v) => v.as_bool(),
    }
}

fn event_of(record: &Value) -> Option<Event> {
    let date = record.get("date")?.as_str()?;
    agenda::parse_date(date)?;
    Some(Event {
        id: valid_id(record.get("id")?)?,
        title: valid_text(record.get("title")?, MAX_TITLE)?,
        date: date.to_string(),
        time: valid_time(record.get("time").unwrap_or(&Value::Null))?,
        end: valid_time(record.get("end").unwrap_or(&Value::Null))?.filter(|_| record.get("time").is_some_and(Value::is_string)),
        updated_at_ms: valid_stamp(record.get("updated_at_ms")?)?,
        deleted: flag(record, "deleted")?,
    })
}

fn note_of(record: &Value) -> Option<Note> {
    Some(Note {
        id: valid_id(record.get("id")?)?,
        text: valid_text(record.get("text")?, MAX_TEXT)?,
        done: flag(record, "done")?,
        updated_at_ms: valid_stamp(record.get("updated_at_ms")?)?,
        deleted: flag(record, "deleted")?,
    })
}

/// Keeps the valid records of what the page sent and drops the others.
fn validate(events: Option<&Value>, notes: Option<&Value>) -> Agenda {
    let list = |v: Option<&Value>| v.and_then(Value::as_array).map(|a| a.iter().take(MAX_RECORDS).cloned().collect::<Vec<_>>()).unwrap_or_default();
    Agenda {
        events: list(events).iter().filter_map(event_of).collect(),
        notes: list(notes).iter().filter_map(note_of).collect(),
    }
}

/// Course colours as the phone last saved them.
#[derive(Debug, PartialEq)]
struct RemoteStyles {
    items: Vec<CourseStyle>,
    at: u64,
}

const MAX_STYLES: usize = 2000;

fn styles_of(v: &Value) -> Option<RemoteStyles> {
    let at = valid_stamp(v.get("updated_at_ms")?)?;
    let items = v
        .get("items")?
        .as_array()?
        .iter()
        .take(MAX_STYLES)
        .filter_map(|s| {
            let text = |k: &str, max: usize| s.get(k).and_then(Value::as_str).filter(|t| t.chars().count() <= max).map(str::to_string);
            let style = CourseStyle { title: text("title", 120)?, date: text("date", 10)?, time: text("time", 5)?, color: text("color", 7)?, hidden: s.get("hidden")?.as_bool()? };
            let color_ok = style.color.is_empty() || (style.color.len() == 7 && style.color.starts_with('#') && style.color[1..].bytes().all(|c| c.is_ascii_hexdigit()));
            (!style.title.is_empty() && color_ok && (style.hidden || !style.color.is_empty())).then_some(style)
        })
        .collect();
    Some(RemoteStyles { items, at })
}

/// What one message from the page means.
#[derive(Debug, PartialEq)]
enum Message {
    Pulled(Agenda, Option<RemoteStyles>),
    SignedOut,
    LoggedOut,
    Pushed,
    Failed,
}

fn parse(message: &str) -> Message {
    let Ok(v) = serde_json::from_str::<Value>(message) else { return Message::Failed };
    if v.get("auth") == Some(&Value::Bool(false)) {
        return Message::SignedOut;
    }
    if v.get("loggedOut") == Some(&Value::Bool(true)) {
        return Message::LoggedOut;
    }
    if v.get("pushed") == Some(&Value::Bool(true)) {
        return Message::Pushed;
    }
    if v.get("error").is_some() || (v.get("events").is_none() && v.get("notes").is_none()) {
        return Message::Failed;
    }
    Message::Pulled(validate(v.get("events"), v.get("notes")), v.get("styles").and_then(styles_of))
}

/// Debug log of the sign-in window: shown in a console behind it and kept in
/// `%LOCALAPPDATA%\naelbar\agenda-login.log`. Never holds tokens, query
/// strings or agenda content.
fn log(msg: &str) {
    use std::io::Write;
    if !LOGIN.get() {
        return;
    }
    let line = format!("[{}] {msg}\n", log_time());
    if let Ok(mut c) = std::fs::OpenOptions::new().write(true).open("CONOUT$") {
        let _ = c.write_all(line.as_bytes());
    }
    if let Some(p) = log_path() {
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(p) {
            let _ = f.write_all(line.as_bytes());
        }
    }
}

fn log_path() -> Option<std::path::PathBuf> {
    std::env::var_os("LOCALAPPDATA").map(|d| std::path::PathBuf::from(d).join("naelbar").join("agenda-login.log"))
}

fn log_time() -> String {
    let t = unsafe { windows::Win32::System::SystemInformation::GetLocalTime() };
    format!("{:02}:{:02}:{:02}.{:03}", t.wHour, t.wMinute, t.wSecond, t.wMilliseconds)
}

/// A URL without its query string or fragment, which can carry codes.
fn bare_url(url: &str) -> &str {
    url.split(['?', '#']).next().unwrap_or(url)
}

fn finish(code: i32) {
    log(&format!("fin demandée, code {code} (0 connecté et synchronisé, 2 connexion nécessaire, 3 échec)"));
    if EXIT.get().is_none() {
        EXIT.set(Some(code));
        match code {
            EXIT_OK => remember_account(true),
            EXIT_SIGN_IN => remember_account(false),
            _ => {}
        }
        unsafe { PostQuitMessage(0) };
    }
}

/// Whether the agenda's Google account was signed in at the last sync,
/// for the settings page. Holds one character, never an identity.
fn account_path() -> Option<PathBuf> {
    std::env::var_os("LOCALAPPDATA").map(|d| PathBuf::from(d).join("naelbar").join("agenda-account"))
}

fn remember_account(signed_in: bool) {
    if let Some(p) = account_path() {
        let _ = std::fs::create_dir_all(p.parent().unwrap_or(&p));
        let _ = std::fs::write(p, if signed_in { "1" } else { "0" });
    }
}

/// None before the first sync.
pub fn signed_in() -> Option<bool> {
    match std::fs::read_to_string(account_path()?).ok()?.trim() {
        "1" => Some(true),
        "0" => Some(false),
        _ => None,
    }
}

/// Merges `remote` into the stored agenda. Returns the result and whether the
/// file changed.
fn merge_into_local(remote: &Agenda) -> std::io::Result<(Agenda, bool)> {
    let mut local = Agenda::load();
    let before = local.clone();
    local.merge(remote);
    let changed = local != before;
    if changed {
        local.save()?;
    }
    Ok((local, changed))
}

/// Lessons kept for the phone: the last half year and what is ahead, at most
/// this many (one Firestore document holds them all).
const MAX_COURSES: usize = 1500;
const PHONE_HISTORY_DAYS: i64 = 183;

/// The .ics timetable, read now, for the phone to show while this PC is off.
/// Fetched only during a sync, so it is never newer than the PC's last one.
fn courses_for_phone() -> Option<Value> {
    let cfg = crate::config::Config::load();
    if cfg.ics_url.trim().is_empty() {
        return None;
    }
    crate::widgets::ics::set_styles(&cfg.course_styles);
    let events = crate::widgets::ics::fetch_now(&cfg.ics_url)?;
    let from = {
        use crate::widgets::civil;
        let t = unsafe { windows::Win32::System::SystemInformation::GetLocalTime() };
        let today = civil::to_ms(t.wYear as i32, t.wMonth as u32, t.wDay as u32, 0, 0, 0);
        let ((y, m, d), _) = civil::from_ms(today - PHONE_HISTORY_DAYS * civil::DAY_MS);
        agenda::date_key(y, m, d)
    };
    let cut = |s: &str, n: usize| s.chars().take(n).collect::<String>();
    let items: Vec<Value> = events
        .iter()
        .filter(|e| e.date >= from)
        .take(MAX_COURSES)
        .map(|e| json!({"d": e.date, "t": e.time, "e": e.end, "ti": cut(&e.title, 120), "k": e.kind, "p": cut(&e.place, 40)}))
        .collect();
    Some(json!({"items": items}))
}

/// Takes the phone's course colours when they are newer. Returns the local
/// ones to send when they are the newer side.
fn reconcile_styles(remote: Option<RemoteStyles>) -> Option<Value> {
    let mut cfg = crate::config::Config::load();
    // Colours set before the sync existed carry no stamp: on first contact
    // they count as new, so the phone never replaces them with its few.
    if cfg.course_styles_at == 0 && !cfg.course_styles.is_empty() {
        cfg.course_styles_at = crate::state::unix_ms();
        cfg.save();
        notify_config();
    }
    let remote_at = remote.as_ref().map_or(0, |r| r.at);
    if let Some(r) = remote.filter(|r| r.at > cfg.course_styles_at) {
        cfg.course_styles = r.items;
        cfg.course_styles_at = r.at;
        cfg.save();
        notify_config();
        return None;
    }
    (cfg.course_styles_at > remote_at).then(|| json!({"items": cfg.course_styles, "updated_at_ms": cfg.course_styles_at}))
}

/// The island reloads its config, so it never saves an older copy over this one.
fn notify_config() {
    unsafe {
        if let Ok(island) = FindWindowW(w!("naelbar.window"), w!("naelbar control")) {
            let _ = PostMessageW(Some(island), crate::config::WM_CONFIG_CHANGED, WPARAM(0), LPARAM(0));
        }
    }
}

/// Tells the island, with a wParam that says the change came from here.
fn notify_island() {
    unsafe {
        if let Ok(island) = FindWindowW(w!("naelbar.window"), w!("naelbar control")) {
            let _ = PostMessageW(Some(island), WM_AGENDA_CHANGED, WPARAM(1), LPARAM(0));
        }
    }
}

fn on_message(webview: &ICoreWebView2, message: &str) {
    if EXIT.get().is_some() {
        return;
    }
    let login = LOGIN.get();
    match (STAGE.get(), parse(message)) {
        (Stage::Pulling, Message::Pulled(remote, styles)) => {
            let Ok((merged, changed)) = merge_into_local(&remote) else { return finish(EXIT_FAILED) };
            if changed {
                notify_island();
            }
            let mut data = json!({"events": merged.events, "notes": merged.notes});
            if let Some(local) = reconcile_styles(styles) {
                data["styles"] = local;
            }
            if let Some(courses) = courses_for_phone() {
                data["courses"] = courses;
            }
            let data = data.to_string();
            STAGE.set(Stage::Pushing);
            let script = HSTRING::from(push_script(&data));
            let done = ExecuteScriptCompletedHandler::create(Box::new(|result, _| {
                if result.is_err() {
                    finish(EXIT_FAILED);
                }
                Ok(())
            }));
            if unsafe { webview.ExecuteScript(PCWSTR(script.as_ptr()), &done) }.is_err() {
                finish(EXIT_FAILED);
            }
        }
        (Stage::Pulling, Message::LoggedOut) => finish(EXIT_SIGN_IN),
        (Stage::Pulling, Message::SignedOut) => {
            // In login mode the user is still signing in: keep waiting.
            STAGE.set(Stage::Idle);
            if !login {
                finish(EXIT_SIGN_IN);
            }
        }
        (Stage::Pulling, _) => {
            STAGE.set(Stage::Idle);
            if !login {
                finish(EXIT_FAILED);
            }
        }
        (Stage::Pushing, Message::Pushed) => finish(EXIT_OK),
        (Stage::Pushing, _) => finish(EXIT_FAILED),
        _ => {}
    }
}

/// True when `uri` is a document of the hosted app.
pub(crate) fn is_host_origin(uri: &str) -> bool {
    uri.strip_prefix("https://").and_then(|r| r.strip_prefix(HOST)).is_some_and(|rest| rest.is_empty() || rest.starts_with(['/', '?', '#']))
}

pub(crate) fn user_data_folder() -> Option<PathBuf> {
    let base = std::env::var_os("LOCALAPPDATA")?;
    let path = PathBuf::from(base).join("naelbar").join("webview-agenda");
    std::fs::create_dir_all(&path).ok()?;
    Some(path)
}

fn controller() -> Option<ICoreWebView2Controller> {
    CONTROLLER.with_borrow(Clone::clone)
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_SIZE => {
            if let Some(c) = controller() {
                let _ = unsafe { fit_to_client(hwnd, &c) };
            }
            LRESULT(0)
        }
        WM_MOVE => {
            if let Some(c) = controller() {
                let _ = unsafe { c.NotifyParentWindowPositionChanged() };
            }
            LRESULT(0)
        }
        WM_SETFOCUS => {
            if let Some(c) = controller() {
                let _ = unsafe { c.MoveFocus(COREWEBVIEW2_MOVE_FOCUS_REASON_PROGRAMMATIC) };
            }
            LRESULT(0)
        }
        WM_TIMER if wparam.0 == DEADLINE_TIMER => {
            finish(EXIT_FAILED);
            LRESULT(0)
        }
        WM_TIMER if wparam.0 == PROBE_TIMER => {
            let _ = unsafe { probe_login() };
            LRESULT(0)
        }
        WM_CLOSE => {
            finish(if LOGIN.get() { EXIT_SIGN_IN } else { EXIT_FAILED });
            LRESULT(0)
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

unsafe fn create_window(login: bool) -> Result<HWND> {
    unsafe {
        let instance = GetModuleHandleW(None)?.into();
        let class_name = w!("naelbar.agendasync");
        let class = WNDCLASSW {
            hIcon: crate::app_icon(),
            lpfnWndProc: Some(wndproc),
            hInstance: instance,
            lpszClassName: class_name,
            hCursor: LoadCursorW(None, IDC_ARROW)?,
            hbrBackground: HBRUSH(GetStockObject(WHITE_BRUSH).0),
            ..Default::default()
        };
        if RegisterClassW(&class) == 0 {
            return Err(Error::from_thread());
        }
        let (x, y, width, height) = if login { login_rect() } else { (0, 0, 800, 600) };
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            class_name,
            if login { w!("Connexion à l'agenda") } else { w!("naelbar") },
            WS_OVERLAPPEDWINDOW,
            x,
            y,
            width,
            height,
            None,
            None,
            Some(instance),
            None,
        )
    }
}

unsafe fn run_pull(webview: &ICoreWebView2) -> Result<()> {
    if STAGE.get() != Stage::Idle {
        return Ok(());
    }
    STAGE.set(Stage::Pulling);
    let login = LOGIN.get();
    unsafe {
        let done = ExecuteScriptCompletedHandler::create(Box::new(move |result, _| {
            if result.is_err() && !login {
                finish(EXIT_FAILED);
            }
            Ok(())
        }));
        let script = HSTRING::from(if LOGOUT.get() { LOGOUT_SCRIPT } else { PULL_SCRIPT });
        webview.ExecuteScript(PCWSTR(script.as_ptr()), &done)
    }
}

unsafe fn current_is_host(webview: &ICoreWebView2) -> Result<bool> {
    let mut source = PWSTR::null();
    unsafe { webview.Source(&mut source)? };
    Ok(is_host_origin(&take_pwstr(source)))
}

unsafe fn on_navigation(webview: &ICoreWebView2, args: &ICoreWebView2NavigationCompletedEventArgs) -> Result<()> {
    if EXIT.get().is_some() {
        return Ok(());
    }
    let login = LOGIN.get();
    let mut success = BOOL(0);
    let mut status = COREWEBVIEW2_WEB_ERROR_STATUS::default();
    unsafe {
        args.IsSuccess(&mut success)?;
        args.WebErrorStatus(&mut status)?;
        if !success.as_bool() && status != COREWEBVIEW2_WEB_ERROR_STATUS_UNKNOWN {
            return if login { Ok(()) } else { Err(E_FAIL.into()) };
        }
        let mut src = PWSTR::null();
        webview.Source(&mut src)?;
        log(&format!("page chargée: {} (succès {}, statut {})", bare_url(&take_pwstr(src)), success.as_bool(), status.0));
        if !current_is_host(webview)? {
            // Sign-in pages of the identity provider: only the user can pass them.
            return if login { Ok(()) } else { Err(E_FAIL.into()) };
        }
        // A new document replaces any script still running in the old one.
        if STAGE.get() == Stage::Pulling {
            STAGE.set(Stage::Idle);
        }
        run_pull(webview)
    }
}

/// The sign-in can finish without a page load, so the login window also asks
/// on a timer, for as long as it is open.
unsafe fn probe_login() -> Result<()> {
    let Some(controller) = controller() else { return Ok(()) };
    unsafe {
        let webview = controller.CoreWebView2()?;
        let host = current_is_host(&webview)?;
        log(&format!("vérification périodique (sur la page de l'agenda: {host})"));
        if host { run_pull(&webview) } else { Ok(()) }
    }
}

unsafe fn on_web_message(webview: &ICoreWebView2, args: &ICoreWebView2WebMessageReceivedEventArgs) -> Result<()> {
    let mut source = PWSTR::null();
    let mut message = PWSTR::null();
    unsafe {
        args.Source(&mut source)?;
        if !is_host_origin(&take_pwstr(source)) {
            return Ok(());
        }
        args.WebMessageAsJson(&mut message)?;
        let text = take_pwstr(message);
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) {
            if let Some(l) = v.get("naelbarLog").and_then(|l| l.as_str()) {
                log(&format!("page: {}", l.chars().take(200).collect::<String>()));
                return Ok(());
            }
            let keys: Vec<&str> = v.as_object().map(|o| o.keys().map(String::as_str).collect()).unwrap_or_default();
            log(&format!("message de la page: clés {keys:?}, signedIn {:?}, erreur {:?}", v.get("signedIn"), v.get("error")));
        }
        on_message(webview, &text);
    }
    Ok(())
}

unsafe fn on_controller(hwnd: HWND, controller: ICoreWebView2Controller) -> Result<()> {
    let login = LOGIN.get();
    let mut token = 0i64;
    unsafe {
        fit_to_client(hwnd, &controller)?;
        let webview = controller.CoreWebView2()?;

        let settings = webview.Settings()?;
        settings.SetAreDevToolsEnabled(false)?;
        settings.SetAreDefaultContextMenusEnabled(false)?;
        settings.SetAreHostObjectsAllowed(false)?;
        settings.SetIsStatusBarEnabled(false)?;

        let completed = NavigationCompletedEventHandler::create(Box::new(move |sender, args| {
            if let (Some(sender), Some(args)) = (sender, args) {
                if on_navigation(&sender, &args).is_err() && !login {
                    finish(EXIT_FAILED);
                }
            }
            Ok(())
        }));
        webview.add_NavigationCompleted(&completed, &mut token)?;

        let received = WebMessageReceivedEventHandler::create(Box::new(move |sender, args| {
            if let (Some(sender), Some(args)) = (sender, args) {
                if on_web_message(&sender, &args).is_err() && !login {
                    finish(EXIT_FAILED);
                }
            }
            Ok(())
        }));
        webview.add_WebMessageReceived(&received, &mut token)?;

        let new_window = NewWindowRequestedEventHandler::create(Box::new(|sender, args| {
            if let (Some(sender), Some(args)) = (sender, args) {
                let _ = on_new_window(&sender, &args);
            }
            Ok(())
        }));
        webview.add_NewWindowRequested(&new_window, &mut token)?;

        if login {
            let _ = controller.MoveFocus(COREWEBVIEW2_MOVE_FOCUS_REASON_PROGRAMMATIC);
        }
        CONTROLLER.set(Some(controller));
        webview.Navigate(PAGE_URL)
    }
}

unsafe fn on_environment(hwnd: HWND, environment: ICoreWebView2Environment) -> Result<()> {
    let done = CreateCoreWebView2ControllerCompletedHandler::create(Box::new(move |result, controller| {
        let started = result
            .and_then(|()| controller.ok_or_else(|| Error::from(E_POINTER)))
            .and_then(|c| unsafe { on_controller(hwnd, c) });
        if let Err(e) = &started {
            log(&format!("création de la vue impossible: {e}"));
            finish(EXIT_FAILED);
        }
        Ok(())
    }));
    unsafe { environment.CreateCoreWebView2Controller(hwnd, &done) }
}

unsafe fn start_webview(hwnd: HWND) -> Result<()> {
    let folder = user_data_folder().ok_or_else(|| Error::from(E_FAIL))?;
    let folder = HSTRING::from(folder.as_os_str());
    let done = CreateCoreWebView2EnvironmentCompletedHandler::create(Box::new(move |result, environment| {
        let started = result
            .and_then(|()| environment.ok_or_else(|| Error::from(E_POINTER)))
            .and_then(|e| unsafe { on_environment(hwnd, e) });
        if let Err(e) = &started {
            log(&format!("environnement WebView2 impossible: {e}"));
            finish(EXIT_FAILED);
        }
        Ok(())
    }));
    unsafe {
        CreateCoreWebView2EnvironmentWithOptions(
            PCWSTR::null(),
            PCWSTR(folder.as_ptr()),
            None::<&ICoreWebView2EnvironmentOptions>,
            &done,
        )
    }
}

unsafe fn host(login: bool) -> Result<i32> {
    LOGIN.set(login);
    unsafe {
        CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok()?;
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);

        let hwnd = create_window(login)?;
        if login {
            let _ = ShowWindow(hwnd, SW_SHOW);
            SetTimer(Some(hwnd), PROBE_TIMER, PROBE_MS, None);
        } else {
            SetTimer(Some(hwnd), DEADLINE_TIMER, DEADLINE_MS, None);
        }
        start_webview(hwnd)?;

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).0 > 0 {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }

        let _ = KillTimer(Some(hwnd), if login { PROBE_TIMER } else { DEADLINE_TIMER });
        if let Some(c) = CONTROLLER.take() {
            let _ = c.Close();
        }
        let _ = DestroyWindow(hwnd);
    }
    Ok(EXIT.get().unwrap_or(EXIT_FAILED))
}

/// Signs this PC out of the agenda's Google account (`--agenda-logout`).
pub fn run_logout() -> i32 {
    LOGOUT.set(true);
    run(false)
}

/// Exit code: 0 synced, 2 sign-in needed, 3 failure.
pub fn run(login: bool) -> i32 {
    if login {
        LOGIN.set(true);
        unsafe {
            let _ = windows::Win32::System::Console::AllocConsole();
            let _ = windows::Win32::System::Console::SetConsoleOutputCP(65001);
        }
        if let Some(p) = log_path() {
            let _ = std::fs::create_dir_all(p.parent().unwrap_or(&p));
            let _ = std::fs::write(&p, "");
            log(&format!("journal: {}", p.display()));
        }
        log("connexion à l'agenda: démarrage");
    }
    let code = unsafe { host(login) }.unwrap_or_else(|e| {
        log(&format!("erreur WebView2: {e}"));
        EXIT_FAILED
    });
    if login {
        log(&format!("terminé, code {code}. La console se ferme dans 15 s."));
        std::thread::sleep(std::time::Duration::from_secs(15));
    }
    code
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agenda_of(v: Value) -> Agenda {
        validate(v.get("events"), v.get("notes"))
    }

    #[test]
    fn valid_records_pass() {
        let a = agenda_of(json!({
            "events": [
                {"id": "ab12", "title": "Dentiste", "date": "2026-02-28", "time": "09:30", "updated_at_ms": 1700000000000u64, "deleted": false},
                {"id": "cd34", "title": "Jour", "date": "2024-02-29", "time": null, "updated_at_ms": 5, "deleted": true}
            ],
            "notes": [{"id": "ef56", "text": "Lait", "done": true, "updated_at_ms": 7.0, "deleted": false}]
        }));
        assert_eq!(a.events.len(), 2);
        assert_eq!(a.events[0].time.as_deref(), Some("09:30"));
        assert!(a.events[1].deleted && a.events[1].time.is_none());
        assert_eq!(a.notes.len(), 1);
        assert!(a.notes[0].done);
    }

    #[test]
    fn bad_events_are_dropped_not_fatal() {
        let a = agenda_of(json!({"events": [
            {"id": "a1", "title": "ok", "date": "2026-13-01", "updated_at_ms": 1},
            {"id": "a2", "title": "ok", "date": "2026-02-30", "updated_at_ms": 1},
            {"id": "a3", "title": "ok", "date": "26-02-01", "updated_at_ms": 1},
            {"id": "a4", "title": "ok", "date": "2026-02-01", "time": "24:00", "updated_at_ms": 1},
            {"id": "a5", "title": "ok", "date": "2026-02-01", "time": "9:30", "updated_at_ms": 1},
            {"id": "a6", "title": "ok", "date": "2026-02-01", "time": 930, "updated_at_ms": 1},
            {"id": "a7", "title": "ok", "date": "2026-02-01", "updated_at_ms": -1},
            {"id": "a8", "title": "ok", "date": "2026-02-01", "updated_at_ms": "1"},
            {"id": "a9", "title": "ok", "date": "2026-02-01", "updated_at_ms": 1e300},
            {"id": "", "title": "ok", "date": "2026-02-01", "updated_at_ms": 1},
            {"id": "ZZ", "title": "ok", "date": "2026-02-01", "updated_at_ms": 1},
            {"id": "b1", "title": 5, "date": "2026-02-01", "updated_at_ms": 1},
            {"id": "b2", "title": "ok", "date": "2026-02-01", "updated_at_ms": 1, "deleted": "yes"},
            "junk", null, 3
        ]}));
        assert_eq!(a.events.len(), 0);
        let a = agenda_of(json!({"events": [
            {"id": "a0", "title": "ok", "date": "2026-02-01", "updated_at_ms": 1},
            {"id": "a1", "title": "ok", "date": "2026-02-01", "time": "23:59", "updated_at_ms": 1}
        ]}));
        assert_eq!(a.events.len(), 2);
    }

    #[test]
    fn lengths_are_capped() {
        let long_title = "x".repeat(MAX_TITLE + 1);
        let max_title = "é".repeat(MAX_TITLE);
        let long_text = "x".repeat(MAX_TEXT + 1);
        let long_id = "a".repeat(MAX_ID + 1);
        let a = agenda_of(json!({
            "events": [
                {"id": "a1", "title": long_title, "date": "2026-02-01", "updated_at_ms": 1},
                {"id": "a2", "title": max_title, "date": "2026-02-01", "updated_at_ms": 1},
                {"id": long_id, "title": "t", "date": "2026-02-01", "updated_at_ms": 1}
            ],
            "notes": [
                {"id": "a3", "text": long_text, "updated_at_ms": 1},
                {"id": "a4", "text": "ok", "updated_at_ms": 1}
            ]
        }));
        assert_eq!(a.events.len(), 1);
        assert_eq!(a.events[0].id, "a2");
        assert_eq!(a.notes.len(), 1);
        assert_eq!(a.notes[0].id, "a4");
    }

    #[test]
    fn missing_flags_default_to_false_and_garbage_lists_are_empty() {
        let a = agenda_of(json!({"notes": [{"id": "a1", "text": "t", "updated_at_ms": 1}]}));
        assert!(!a.notes[0].done && !a.notes[0].deleted);
        assert_eq!(agenda_of(json!({"events": "x", "notes": 4})), Agenda::default());
        assert_eq!(validate(None, None), Agenda::default());
    }

    #[test]
    fn messages() {
        assert_eq!(parse(r#"{"auth":false}"#), Message::SignedOut);
        assert_eq!(parse(r#"{"pushed":true}"#), Message::Pushed);
        assert_eq!(parse(r#"{"error":"push"}"#), Message::Failed);
        assert_eq!(parse("nope"), Message::Failed);
        assert_eq!(parse("{}"), Message::Failed);
        assert_eq!(parse(r#"{"events":[],"notes":[]}"#), Message::Pulled(Agenda::default(), None));
    }

    #[test]
    fn end_times_and_phone_styles() {
        let a = agenda_of(json!({"events": [
            {"id": "a1", "title": "Projet", "date": "2026-11-20", "time": "13:00", "end": "16:30", "updated_at_ms": 1},
            {"id": "a2", "title": "Sans heure", "date": "2026-11-20", "time": null, "end": "16:30", "updated_at_ms": 1},
            {"id": "a3", "title": "Mauvaise fin", "date": "2026-11-20", "time": "13:00", "end": "25:00", "updated_at_ms": 1}
        ]}));
        assert_eq!(a.events.len(), 2);
        assert_eq!(a.events[0].end.as_deref(), Some("16:30"));
        assert_eq!(a.events[1].end, None);
        let st = styles_of(&json!({"updated_at_ms": 5, "items": [
            {"title": "Anglais", "date": "", "time": "", "color": "#4C9BFF", "hidden": false},
            {"title": "Maths", "date": "2026-11-20", "time": "08:00", "color": "", "hidden": true},
            {"title": "Rien", "date": "", "time": "", "color": "", "hidden": false},
            {"title": "Mauvais", "date": "", "time": "", "color": "red", "hidden": false}
        ]})).unwrap();
        assert_eq!(st.at, 5);
        assert_eq!(st.items.iter().map(|s| s.title.as_str()).collect::<Vec<_>>(), ["Anglais", "Maths"]);
        assert!(styles_of(&json!({"items": []})).is_none());
    }

    #[test]
    fn origin_check() {
        assert!(is_host_origin("https://naelbar-agenda-eadafc.web.app"));
        assert!(is_host_origin("https://naelbar-agenda-eadafc.web.app/?desktop=1"));
        assert!(is_host_origin("https://naelbar-agenda-eadafc.web.app?x"));
        assert!(!is_host_origin("https://naelbar-agenda-eadafc.web.app.evil.com/"));
        assert!(!is_host_origin("https://naelbar-agenda-eadafc.web.app@evil.com/"));
        assert!(!is_host_origin("http://naelbar-agenda-eadafc.web.app/"));
        assert!(!is_host_origin("https://naelbar-agenda-eadafc.firebaseapp.com/__/auth/handler"));
        assert!(!is_host_origin(""));
    }
}
