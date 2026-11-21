//! Reads the subscription limits through a private WebView2 profile.
//!
//! Runs in its own short-lived process (`--claude-usage`, `--claude-login`), so
//! the island carries none of this. The page script posts sanitised numbers
//! only: no names, ids, emails or cookies ever reach this process.

use std::cell::{Cell, RefCell};
use std::io::Write;
use std::path::PathBuf;

use serde_json::{Map, Value, json};
use webview2_com::Microsoft::Web::WebView2::Win32::{
    COREWEBVIEW2_MOVE_FOCUS_REASON_PROGRAMMATIC, COREWEBVIEW2_WEB_ERROR_STATUS,
    COREWEBVIEW2_WEB_ERROR_STATUS_UNKNOWN, CreateCoreWebView2EnvironmentWithOptions, ICoreWebView2,
    ICoreWebView2Controller, ICoreWebView2Environment, ICoreWebView2EnvironmentOptions,
    ICoreWebView2NavigationCompletedEventArgs, ICoreWebView2NewWindowRequestedEventArgs,
    ICoreWebView2WebMessageReceivedEventArgs,
};
use webview2_com::{
    CreateCoreWebView2ControllerCompletedHandler, CreateCoreWebView2EnvironmentCompletedHandler,
    ExecuteScriptCompletedHandler, NavigationCompletedEventHandler, NewWindowRequestedEventHandler,
    WebMessageReceivedEventHandler, take_pwstr,
};
use windows::Win32::Foundation::{E_FAIL, E_POINTER, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{GetStockObject, HBRUSH, WHITE_BRUSH};
use windows::Win32::System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, GetDpiForSystem, SetProcessDpiAwarenessContext,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CW_USEDEFAULT, CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetClientRect,
    GetMessageW, IDC_ARROW, KillTimer, LoadCursorW, MSG, PostQuitMessage, RegisterClassW,
    SPI_GETWORKAREA, SW_SHOW, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS, SetTimer, ShowWindow,
    SystemParametersInfoW, TranslateMessage, WINDOW_EX_STYLE, WM_CLOSE, WM_MOVE, WM_SETFOCUS,
    WM_SIZE, WM_TIMER, WNDCLASSW, WS_OVERLAPPEDWINDOW,
};
use windows::core::{BOOL, Error, HSTRING, PCWSTR, PWSTR, Result, w};

pub(crate) const EXIT_OK: i32 = 0;
pub(crate) const EXIT_SIGN_IN: i32 = 2;
pub(crate) const EXIT_FAILED: i32 = 3;

const FETCH_URL: PCWSTR = w!("https://claude.ai/robots.txt");
const LOGIN_URL: PCWSTR = w!("https://claude.ai/login");
const ORIGIN: &str = "https://claude.ai";

const DEADLINE_TIMER: usize = 1;
const DEADLINE_MS: u32 = 30_000;
const PROBE_TIMER: usize = 2;
const PROBE_MS: u32 = 5_000;
const LOGIN_WIDTH_DIP: i32 = 480;
const LOGIN_HEIGHT_DIP: i32 = 720;

const WINDOWS_KEYS: [&str; 2] = ["five_hour", "seven_day"];

/// `ExecuteScript` does not await promises, so the result goes through
/// `postMessage`. Only numbers and top-level key names leave the page.
const SCRIPT: &str = r#"(async () => {
  const post = m => window.chrome.webview.postMessage(m);
  try {
    if (location.hostname !== 'claude.ai') return;
    const isJson = r => (r.headers.get('content-type') || '').toLowerCase().includes('json');
    const orgs = await fetch('/api/organizations', {credentials: 'include'});
    if (!isJson(orgs)) { post({error: 'blocked'}); return; }
    if (orgs.status === 401 || orgs.status === 403) { post({auth: false}); return; }
    if (!orgs.ok) { post({error: 'status'}); return; }
    const list = await orgs.json();
    if (!Array.isArray(list)) { post({error: 'no_usage'}); return; }
    const pick = w => ({p: w.utilization, r: Date.parse(w.resets_at) / 1000});
    for (const org of list) {
      if (!org || typeof org.uuid !== 'string') continue;
      const res = await fetch('/api/organizations/' + encodeURIComponent(org.uuid) + '/usage',
        {credentials: 'include'});
      if (!res.ok || !isJson(res)) continue;
      const data = await res.json();
      if (!data || !(data.five_hour || data.seven_day)) continue;
      const out = {keys: Object.keys(data)};
      if (data.five_hour) out.five_hour = pick(data.five_hour);
      if (data.seven_day) out.seven_day = pick(data.seven_day);
      post(out);
      return;
    }
    post({error: 'no_usage'});
  } catch (e) {
    post({error: 'exception'});
  }
})();"#;

thread_local! {
    static LOGIN: Cell<bool> = const { Cell::new(false) };
    static EXIT: Cell<Option<i32>> = const { Cell::new(None) };
    static CONTROLLER: RefCell<Option<ICoreWebView2Controller>> = const { RefCell::new(None) };
}

/// What one message from the page means.
#[derive(Debug, PartialEq)]
enum Outcome {
    /// Status line JSON, ready to forward.
    Usable(String),
    SignedOut,
    /// Fixed code, safe to print.
    Failed(&'static str),
}

/// One validated window, `None` for null, missing or out of range values.
fn convert_window(window: Option<&Value>) -> Option<Value> {
    let w = window?;
    let percent = w.get("p")?.as_f64()?;
    let resets = w.get("r")?.as_f64()?;
    if !percent.is_finite() || !(0.0..=100.0).contains(&percent) {
        return None;
    }
    if !resets.is_finite() || resets <= 0.0 {
        return None;
    }
    Some(json!({"used_percentage": percent, "resets_at": resets.round() as u64}))
}

/// Converts a page message to the status line JSON shape.
fn convert(message: &str) -> Outcome {
    let Ok(v) = serde_json::from_str::<Value>(message) else {
        return Outcome::Failed("invalid");
    };
    if v.get("auth") == Some(&Value::Bool(false)) {
        return Outcome::SignedOut;
    }
    if let Some(code) = v.get("error") {
        return Outcome::Failed(match code.as_str() {
            Some("blocked") => "blocked",
            Some("status") => "status",
            Some("no_usage") => "no_usage",
            Some("exception") => "exception",
            _ => "error",
        });
    }
    let mut limits = Map::new();
    for key in WINDOWS_KEYS {
        if let Some(window) = convert_window(v.get(key)) {
            limits.insert(key.to_string(), window);
        }
    }
    if limits.is_empty() {
        return Outcome::Failed("no_windows");
    }
    Outcome::Usable(json!({"rate_limits": limits}).to_string())
}

/// True when `uri` is a document of the claude.ai origin. The trailing slash
/// rejects look-alikes such as `https://claude.ai.example.com`.
fn is_claude_origin(uri: &str) -> bool {
    uri.strip_prefix(ORIGIN).is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
}

/// Prints the key names and the outcome, never a value. Never panics: stderr
/// may be closed in this windowless process.
fn debug_report(message: &str, outcome: &Outcome) {
    if std::env::var_os("NAELBAR_WEBUSAGE_DEBUG").is_none() {
        return;
    }
    let parsed = serde_json::from_str::<Value>(message).unwrap_or(Value::Null);
    let keys: Vec<&str> = parsed
        .get("keys")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let label = match outcome {
        Outcome::Usable(_) => "ok",
        Outcome::SignedOut => "signed_out",
        Outcome::Failed(code) => code,
    };
    let _ = writeln!(std::io::stderr(), "webusage: keys={keys:?} outcome={label}");
}

/// Records the exit code once and leaves the message loop.
fn finish(code: i32) {
    if EXIT.get().is_none() {
        EXIT.set(Some(code));
        unsafe { PostQuitMessage(0) };
    }
}

fn on_message(message: &str) {
    if EXIT.get().is_some() {
        return;
    }
    let outcome = convert(message);
    debug_report(message, &outcome);
    let login = LOGIN.get();
    match outcome {
        Outcome::Usable(line) => {
            nb_integrations::statusline_forward(&line, nb_integrations::DEFAULT_PORT);
            finish(EXIT_OK);
        }
        // In login mode the user is still signing in: keep waiting.
        Outcome::SignedOut if !login => finish(EXIT_SIGN_IN),
        Outcome::Failed(_) if !login => finish(EXIT_FAILED),
        _ => {}
    }
}

fn user_data_folder() -> Option<PathBuf> {
    let base = std::env::var_os("LOCALAPPDATA")?;
    let path = PathBuf::from(base).join("naelbar").join("webview");
    std::fs::create_dir_all(&path).ok()?;
    Some(path)
}

fn controller() -> Option<ICoreWebView2Controller> {
    CONTROLLER.with_borrow(Clone::clone)
}

pub(crate) unsafe fn fit_to_client(hwnd: HWND, controller: &ICoreWebView2Controller) -> Result<()> {
    let mut rect = RECT::default();
    unsafe {
        GetClientRect(hwnd, &mut rect)?;
        controller.SetBounds(rect)
    }
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

/// Size and position of the login window: centred in the primary work area.
pub(crate) unsafe fn login_rect() -> (i32, i32, i32, i32) {
    let dpi = unsafe { GetDpiForSystem() } as i32;
    let mut width = LOGIN_WIDTH_DIP * dpi / 96;
    let mut height = LOGIN_HEIGHT_DIP * dpi / 96;
    let mut area = RECT::default();
    let found = unsafe {
        SystemParametersInfoW(
            SPI_GETWORKAREA,
            0,
            Some(&mut area as *mut RECT as *mut _),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        )
    };
    if found.is_err() {
        return (CW_USEDEFAULT, CW_USEDEFAULT, width, height);
    }
    width = width.min(area.right - area.left);
    height = height.min(area.bottom - area.top);
    let x = area.left + (area.right - area.left - width) / 2;
    let y = area.top + (area.bottom - area.top - height) / 2;
    (x, y, width, height)
}

unsafe fn create_window(login: bool) -> Result<HWND> {
    unsafe {
        let instance = GetModuleHandleW(None)?.into();
        let class_name = w!("naelbar.webusage");
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
            if login { w!("Connexion à Claude") } else { w!("naelbar") },
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

unsafe fn on_navigation(
    webview: &ICoreWebView2,
    args: &ICoreWebView2NavigationCompletedEventArgs,
) -> Result<()> {
    if EXIT.get().is_some() {
        return Ok(());
    }
    let login = LOGIN.get();
    let mut success = BOOL(0);
    let mut status = COREWEBVIEW2_WEB_ERROR_STATUS::default();
    let mut source = PWSTR::null();
    unsafe {
        args.IsSuccess(&mut success)?;
        args.WebErrorStatus(&mut status)?;
        // HTTP error pages report status UNKNOWN and still run the script,
        // which classifies them. The rest are network failures.
        if !success.as_bool() && status != COREWEBVIEW2_WEB_ERROR_STATUS_UNKNOWN {
            return if login { Ok(()) } else { Err(E_FAIL.into()) };
        }
        webview.Source(&mut source)?;
        if !is_claude_origin(&take_pwstr(source)) {
            return if login { Ok(()) } else { Err(E_FAIL.into()) };
        }
        run_script(webview, login)
    }
}

unsafe fn run_script(webview: &ICoreWebView2, login: bool) -> Result<()> {
    unsafe {
        let done = ExecuteScriptCompletedHandler::create(Box::new(move |result, _| {
            if result.is_err() && !login {
                finish(EXIT_FAILED);
            }
            Ok(())
        }));
        let script = HSTRING::from(SCRIPT);
        webview.ExecuteScript(PCWSTR(script.as_ptr()), &done)
    }
}

/// claude.ai can finish signing in without a page load, so the login window
/// also checks on a timer, for as long as it is open.
unsafe fn probe_login() -> Result<()> {
    let Some(controller) = controller() else { return Ok(()) };
    unsafe {
        let webview = controller.CoreWebView2()?;
        let mut source = PWSTR::null();
        webview.Source(&mut source)?;
        if is_claude_origin(&take_pwstr(source)) { run_script(&webview, true) } else { Ok(()) }
    }
}

unsafe fn on_web_message(args: &ICoreWebView2WebMessageReceivedEventArgs) -> Result<()> {
    let mut source = PWSTR::null();
    let mut message = PWSTR::null();
    unsafe {
        args.Source(&mut source)?;
        if !is_claude_origin(&take_pwstr(source)) {
            return Ok(());
        }
        args.WebMessageAsJson(&mut message)?;
        on_message(&take_pwstr(message));
    }
    Ok(())
}

/// OAuth popups stay in this webview instead of the system browser.
pub(crate) unsafe fn on_new_window(
    webview: &ICoreWebView2,
    args: &ICoreWebView2NewWindowRequestedEventArgs,
) -> Result<()> {
    let mut uri = PWSTR::null();
    unsafe {
        args.SetHandled(true)?;
        args.Uri(&mut uri)?;
        let uri = take_pwstr(uri);
        if uri.starts_with("https://") || uri.starts_with("http://") {
            let uri = HSTRING::from(uri);
            webview.Navigate(PCWSTR(uri.as_ptr()))?;
        }
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

        // Handlers use the sender they receive and hold no webview reference.
        let completed = NavigationCompletedEventHandler::create(Box::new(move |sender, args| {
            if let (Some(sender), Some(args)) = (sender, args) {
                if on_navigation(&sender, &args).is_err() && !login {
                    finish(EXIT_FAILED);
                }
            }
            Ok(())
        }));
        webview.add_NavigationCompleted(&completed, &mut token)?;

        let received = WebMessageReceivedEventHandler::create(Box::new(move |_, args| {
            if let Some(args) = args {
                if on_web_message(&args).is_err() && !login {
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
        webview.Navigate(if login { LOGIN_URL } else { FETCH_URL })
    }
}

unsafe fn on_environment(hwnd: HWND, environment: ICoreWebView2Environment) -> Result<()> {
    let done = CreateCoreWebView2ControllerCompletedHandler::create(Box::new(move |result, controller| {
        let started = result
            .and_then(|()| controller.ok_or_else(|| Error::from(E_POINTER)))
            .and_then(|c| unsafe { on_controller(hwnd, c) });
        if started.is_err() {
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
        if started.is_err() {
            finish(EXIT_FAILED);
        }
        Ok(())
    }));
    // Fails here when the WebView2 runtime is missing.
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
        // No CoUninitialize: the process exits right after this returns.
    }
    Ok(EXIT.get().unwrap_or(EXIT_FAILED))
}

/// Exit code: 0 read and forwarded, 2 sign-in needed, 3 failure.
pub fn run(login: bool) -> i32 {
    unsafe { host(login) }.unwrap_or(EXIT_FAILED)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn usable(message: &str) -> Value {
        match convert(message) {
            Outcome::Usable(line) => serde_json::from_str(&line).unwrap(),
            other => panic!("expected usable, got {other:?}"),
        }
    }

    #[test]
    fn both_windows_convert() {
        let v = usable(r#"{"five_hour":{"p":34.5,"r":1700000000},"seven_day":{"p":12,"r":1700500000.4},"keys":["five_hour","seven_day"]}"#);
        assert_eq!(v["rate_limits"]["five_hour"]["used_percentage"], 34.5);
        assert_eq!(v["rate_limits"]["five_hour"]["resets_at"], 1_700_000_000u64);
        assert_eq!(v["rate_limits"]["seven_day"]["used_percentage"], 12.0);
        assert_eq!(v["rate_limits"]["seven_day"]["resets_at"], 1_700_500_000u64);
    }

    #[test]
    fn output_is_what_the_status_line_reads() {
        let Outcome::Usable(line) = convert(r#"{"five_hour":{"p":34,"r":1700000000},"seven_day":{"p":12,"r":1700500000}}"#) else {
            panic!("not usable");
        };
        assert_eq!(nb_integrations::statusline_forward(&line, 1), "5 h 34 % \u{b7} sem. 12 %");
    }

    #[test]
    fn null_or_missing_window_is_omitted() {
        let v = usable(r#"{"five_hour":null,"seven_day":{"p":1,"r":2}}"#);
        assert!(v["rate_limits"].get("five_hour").is_none());
        assert!(v["rate_limits"].get("seven_day").is_some());
        let v = usable(r#"{"five_hour":{"p":1,"r":2}}"#);
        assert!(v["rate_limits"].get("seven_day").is_none());
    }

    #[test]
    fn bad_numbers_drop_the_window() {
        for bad in [
            r#"{"p":-1,"r":2}"#,
            r#"{"p":100.5,"r":2}"#,
            r#"{"p":"12","r":2}"#,
            r#"{"p":null,"r":2}"#,
            r#"{"p":12,"r":null}"#,
            r#"{"p":12,"r":0}"#,
            r#"{"p":12,"r":-5}"#,
            r#"{"r":2}"#,
        ] {
            let alone = format!(r#"{{"five_hour":{bad}}}"#);
            assert_eq!(convert(&alone), Outcome::Failed("no_windows"), "{bad}");
            let v = usable(&format!(r#"{{"five_hour":{bad},"seven_day":{{"p":100,"r":2}}}}"#));
            assert!(v["rate_limits"].get("five_hour").is_none(), "{bad}");
        }
    }

    #[test]
    fn percent_bounds_are_inclusive() {
        let v = usable(r#"{"five_hour":{"p":0,"r":1},"seven_day":{"p":100,"r":1}}"#);
        assert_eq!(v["rate_limits"]["five_hour"]["used_percentage"], 0.0);
        assert_eq!(v["rate_limits"]["seven_day"]["used_percentage"], 100.0);
    }

    #[test]
    fn auth_and_errors() {
        assert_eq!(convert(r#"{"auth":false}"#), Outcome::SignedOut);
        assert_eq!(convert(r#"{"error":"blocked"}"#), Outcome::Failed("blocked"));
        assert_eq!(convert(r#"{"error":"exception"}"#), Outcome::Failed("exception"));
        assert_eq!(convert(r#"{"error":"anything else"}"#), Outcome::Failed("error"));
    }

    #[test]
    fn garbage_is_not_usable() {
        assert_eq!(convert("not json"), Outcome::Failed("invalid"));
        assert_eq!(convert("null"), Outcome::Failed("no_windows"));
        assert_eq!(convert(r#"{"keys":[]}"#), Outcome::Failed("no_windows"));
        assert_eq!(convert(r#"{"auth":true}"#), Outcome::Failed("no_windows"));
    }

    #[test]
    fn output_carries_only_the_windows() {
        let v = usable(r#"{"five_hour":{"p":1,"r":2,"extra":"secret"},"email":"a@b","keys":["x"]}"#);
        assert_eq!(v, json!({"rate_limits":{"five_hour":{"used_percentage":1.0,"resets_at":2}}}));
    }

    #[test]
    fn origin_check() {
        assert!(is_claude_origin("https://claude.ai"));
        assert!(is_claude_origin("https://claude.ai/robots.txt"));
        assert!(!is_claude_origin("https://claude.ai.example.com/"));
        assert!(!is_claude_origin("https://claude.ai@example.com/"));
        assert!(!is_claude_origin("http://claude.ai/"));
        assert!(!is_claude_origin("https://accounts.google.com/claude.ai/"));
        assert!(!is_claude_origin(""));
    }
}
