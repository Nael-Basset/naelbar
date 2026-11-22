//! naelbar: a floating island for Windows 11.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
#![allow(unsafe_op_in_unsafe_fn)]

mod activity;
mod agenda;
mod agendawin;
mod agendasync;
mod anim;
mod app;
mod claude;
mod config;
mod dock;
mod desktop;
mod dragdrop;
mod editor;
mod gamestrip;
mod hotkeys;
mod launcher;
mod hud;
mod monitors;
mod phone;
mod preview;
mod render;
mod services;
mod share;
mod shelf_store;
mod settings;
mod state;
mod tray;
mod updater;
mod ytmext;
mod ui;
mod views;
mod visualizer;
mod webusage;
mod widgets;

use windows::Win32::Foundation::{ERROR_ALREADY_EXISTS, GetLastError};
use windows::Win32::System::Ole::OleInitialize;
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::UI::HiDpi::{DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext};
use windows::core::w;

/// Run by Claude Code as its status line: hands the limits to the island, then
/// prints the short text Claude Code displays.
fn run_statusline() {
    use std::io::{Read, Write};
    let mut input = String::new();
    if std::io::stdin().read_to_string(&mut input).is_ok() {
        let text = nb_integrations::statusline_forward(&input, nb_integrations::DEFAULT_PORT);
        let _ = std::io::stdout().write_all(text.as_bytes());
    }
}

/// Adds naelbar's hooks and status line to the user's Claude Code settings.
fn setup_claude() -> Result<String, String> {
    let home = std::env::var_os("USERPROFILE").ok_or("USERPROFILE is not set")?;
    let settings = std::path::Path::new(&home).join(".claude").join("settings.json");
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let report = nb_integrations::merge_claude_code_settings(&settings, nb_integrations::DEFAULT_PORT, &exe).map_err(|e| e.to_string())?;
    Ok(format!("{report:?}"))
}

/// The icon embedded in the executable (resource 1), for window classes.
pub(crate) fn app_icon() -> windows::Win32::UI::WindowsAndMessaging::HICON {
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::UI::WindowsAndMessaging::LoadIconW;
    unsafe {
        let instance = GetModuleHandleW(None).ok().map(|m| m.into());
        LoadIconW(instance, windows::core::PCWSTR(1 as *const u16)).unwrap_or_default()
    }
}

fn main() {
    if let Some(spec) = std::env::args().find_map(|a| a.strip_prefix("--edit=").map(str::to_string)) {
        unsafe {
            let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
            if OleInitialize(None).is_ok() {
                if let Err(e) = editor::run(&spec) {
                    eprintln!("naelbar editor: {e}");
                }
            }
        }
        return;
    }
    if std::env::args().any(|a| a == "--statusline") {
        run_statusline();
        return;
    }
    if std::env::args().any(|a| a == "--claude-usage") {
        std::process::exit(webusage::run(false));
    }
    if std::env::args().any(|a| a == "--claude-login") {
        std::process::exit(webusage::run(true));
    }
    if std::env::args().any(|a| a == "--agenda-sync") {
        std::process::exit(agendasync::run(false));
    }
    if std::env::args().any(|a| a == "--agenda-logout") {
        std::process::exit(agendasync::run_logout());
    }
    if std::env::args().any(|a| a == "--agenda-login") {
        std::process::exit(agendasync::run(true));
    }
    if std::env::args().any(|a| a == "--phone-link") {
        std::process::exit(phone::link::run());
    }
    if std::env::args().any(|a| a == "--setup-claude") {
        match setup_claude() {
            Ok(report) => println!("{report}"),
            Err(e) => {
                eprintln!("naelbar: {e}");
                std::process::exit(1);
            }
        }
        return;
    }
    let mutex = unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        if let Some(name) = std::env::args().find_map(|a| a.strip_prefix("--desktop-widget=").map(str::to_string)) {
            if OleInitialize(None).is_ok() {
                if let Err(e) = desktop::run(&name) {
                    eprintln!("naelbar desktop widget: {e}");
                }
            }
            return;
        }
        if std::env::args().any(|a| a == "--agenda-window") {
            if OleInitialize(None).is_ok() {
                if let Err(e) = agendawin::run() {
                    eprintln!("naelbar agenda window: {e}");
                }
            }
            return;
        }
        if std::env::args().any(|a| a == "--settings") {
            if OleInitialize(None).is_ok() {
                let section = std::env::args().find_map(|a| a.strip_prefix("--section=").and_then(|v| v.parse().ok())).unwrap_or(0);
                if let Err(e) = settings::run(section) {
                    eprintln!("naelbar settings: {e}");
                }
            }
            return;
        }
        // Held for the life of the process; a second instance exits at once.
        let mutex = CreateMutexW(None, true, w!("Local\\naelbar.single-instance"));
        if mutex.is_err() || GetLastError() == ERROR_ALREADY_EXISTS {
            return;
        }
        if OleInitialize(None).is_err() {
            return;
        }
        mutex
    };
    let demo = std::env::args().any(|a| a == "--demo");
    let start_open = std::env::args().any(|a| a == "--open");
    let start_tab = std::env::args().find_map(|a| a.strip_prefix("--tab=").map(str::to_string));
    if let Err(e) = app::run(demo, start_open, start_tab) {
        eprintln!("naelbar: {e}");
    }
    // The new version takes the single-instance lock once this one let go.
    if updater::RESTART.load(std::sync::atomic::Ordering::SeqCst) {
        if let Ok(m) = mutex {
            unsafe {
                let _ = windows::Win32::System::Threading::ReleaseMutex(m);
                let _ = windows::Win32::Foundation::CloseHandle(m);
            }
        }
        updater::relaunch_if_updated();
    }
}
