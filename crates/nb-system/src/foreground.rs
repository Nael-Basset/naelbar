//! Foreground window description and the fullscreen decision.

use nb_core::ForegroundState;
use windows::Win32::Foundation::{CloseHandle, HWND, RECT};
use windows::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromWindow,
};
use windows::Win32::System::Threading::{
    OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
};
use windows::Win32::UI::Shell::{
    QUNS_PRESENTATION_MODE, QUNS_RUNNING_D3D_FULL_SCREEN, SHQueryUserNotificationState,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GWL_STYLE, GetClassNameW, GetWindowLongW, GetWindowRect, GetWindowThreadProcessId, WS_CAPTION,
};
use windows::core::PWSTR;

/// Pure fullscreen decision. A window is fullscreen when its rectangle covers
/// its monitor, it has no caption (a maximised normal window has one and
/// leaves room for the taskbar), and it is not the shell.
pub fn is_fullscreen(win: RECT, monitor: RECT, has_caption: bool, is_shell: bool) -> bool {
    if is_shell || has_caption {
        return false;
    }
    win.left <= monitor.left
        && win.top <= monitor.top
        && win.right >= monitor.right
        && win.bottom >= monitor.bottom
}

pub fn is_shell_class(class: &str) -> bool {
    matches!(class, "Progman" | "WorkerW" | "Shell_TrayWnd" | "Shell_SecondaryTrayWnd")
}

pub fn file_name(path: &str) -> &str {
    path.rsplit(['\\', '/']).next().unwrap_or(path)
}

/// (thread id, process id) owning the window.
pub fn thread_and_pid(hwnd: HWND) -> (u32, u32) {
    let mut pid = 0u32;
    // SAFETY: valid out pointer.
    let tid = unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
    (tid, pid)
}

fn exe_of(pid: u32) -> String {
    // SAFETY: the handle is closed before returning; the buffer length is passed in size.
    unsafe {
        let Ok(h) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else {
            return String::new();
        };
        let mut buf = [0u16; 1024];
        let mut size = buf.len() as u32;
        let r = QueryFullProcessImageNameW(h, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut size);
        let _ = CloseHandle(h);
        if r.is_err() {
            return String::new();
        }
        file_name(&String::from_utf16_lossy(&buf[..size as usize])).to_string()
    }
}

/// Describes the window. `None` when there is no usable window.
pub fn describe(hwnd: HWND) -> Option<ForegroundState> {
    if hwnd.0.is_null() {
        return None;
    }
    let (_, pid) = thread_and_pid(hwnd);
    if pid == 0 {
        return None;
    }
    // SAFETY: all out pointers are valid locals.
    unsafe {
        let mut cls = [0u16; 64];
        let n = GetClassNameW(hwnd, &mut cls);
        let class = String::from_utf16_lossy(&cls[..n.max(0) as usize]);
        let shell = is_shell_class(&class);
        let mut wr = RECT::default();
        let mut fs = false;
        if GetWindowRect(hwnd, &mut wr).is_ok() {
            let mon = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
            let mut mi = MONITORINFO { cbSize: size_of::<MONITORINFO>() as u32, ..Default::default() };
            if GetMonitorInfoW(mon, &mut mi).as_bool() {
                let style = GetWindowLongW(hwnd, GWL_STYLE) as u32;
                let caption = style & WS_CAPTION.0 == WS_CAPTION.0;
                fs = is_fullscreen(wr, mi.rcMonitor, caption, shell);
            }
        }
        if !fs && !shell {
            if let Ok(state) = SHQueryUserNotificationState() {
                fs = state == QUNS_RUNNING_D3D_FULL_SCREEN || state == QUNS_PRESENTATION_MODE;
            }
        }
        Some(ForegroundState { exe: exe_of(pid), fullscreen: fs, desktop: shell })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(l: i32, t: i32, rr: i32, b: i32) -> RECT {
        RECT { left: l, top: t, right: rr, bottom: b }
    }

    #[test]
    fn borderless_covering_monitor_is_fullscreen() {
        assert!(is_fullscreen(r(0, 0, 1920, 1080), r(0, 0, 1920, 1080), false, false));
        assert!(is_fullscreen(r(-8, -8, 1928, 1088), r(0, 0, 1920, 1080), false, false));
    }

    #[test]
    fn maximised_with_taskbar_is_not() {
        assert!(!is_fullscreen(r(-8, -8, 1928, 1032), r(0, 0, 1920, 1080), true, false));
        assert!(!is_fullscreen(r(0, 0, 1920, 1040), r(0, 0, 1920, 1080), false, false));
    }

    #[test]
    fn caption_and_shell_excluded() {
        assert!(!is_fullscreen(r(0, 0, 1920, 1080), r(0, 0, 1920, 1080), true, false));
        assert!(!is_fullscreen(r(0, 0, 1920, 1080), r(0, 0, 1920, 1080), false, true));
        assert!(is_shell_class("Progman"));
        assert!(!is_shell_class("Chrome_WidgetWin_1"));
    }

    #[test]
    fn second_monitor_offsets() {
        let m = r(1920, 0, 3840, 1080);
        assert!(is_fullscreen(r(1920, 0, 3840, 1080), m, false, false));
        assert!(!is_fullscreen(r(0, 0, 1920, 1080), m, false, false));
    }

    #[test]
    fn file_names() {
        assert_eq!(file_name(r"C:\Windows\explorer.exe"), "explorer.exe");
        assert_eq!(file_name("a.exe"), "a.exe");
    }
}
