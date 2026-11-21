//! `RegisterHotKey` bindings from the config file.

use windows::Win32::Foundation::HWND;
use windows::Win32::UI::Input::KeyboardAndMouse::*;

use crate::config::Hotkeys;

pub const HK_TOGGLE: i32 = 1;
pub const HK_PLAY_PAUSE: i32 = 2;
pub const HK_TIMER: i32 = 3;
pub const HK_HIDE: i32 = 4;
pub const HK_MIC: i32 = 5;
pub const HK_GAME: i32 = 6;

fn parse(s: &str) -> Option<(HOT_KEY_MODIFIERS, u32)> {
    let mut mods = MOD_NOREPEAT;
    let mut key = None;
    for part in s.split('+').map(str::trim) {
        match part.to_ascii_lowercase().as_str() {
            "ctrl" | "control" => mods |= MOD_CONTROL,
            "alt" => mods |= MOD_ALT,
            "shift" => mods |= MOD_SHIFT,
            "win" | "super" => mods |= MOD_WIN,
            "" => return None,
            other => key = Some(vk_for(other)?),
        }
    }
    key.map(|k| (mods, k))
}

fn vk_for(name: &str) -> Option<u32> {
    if name.len() == 1 {
        let c = name.chars().next()?.to_ascii_uppercase();
        return c.is_ascii_alphanumeric().then_some(c as u32);
    }
    if let Some(n) = name.strip_prefix('f').and_then(|n| n.parse::<u32>().ok()) {
        if (1..=24).contains(&n) {
            return Some(VK_F1.0 as u32 + n - 1);
        }
    }
    match name {
        "space" => Some(VK_SPACE.0 as u32),
        "enter" => Some(VK_RETURN.0 as u32),
        _ => None,
    }
}

/// Registers every binding that parses and is free. A failure leaves the rest
/// working; the failed names are returned.
pub fn register_all(hwnd: HWND, keys: &Hotkeys) -> Vec<String> {
    let entries = [
        (HK_TOGGLE, &keys.toggle),
        (HK_PLAY_PAUSE, &keys.play_pause),
        (HK_TIMER, &keys.timer),
        (HK_HIDE, &keys.hide),
        (HK_MIC, &keys.mic),
        (HK_GAME, &keys.game_strip),
    ];
    let mut failed = Vec::new();
    for (id, text) in entries {
        let ok = parse(text).is_some_and(|(mods, vk)| unsafe { RegisterHotKey(Some(hwnd), id, mods, vk).is_ok() });
        if !ok {
            failed.push(text.clone());
        }
    }
    failed
}

pub fn unregister_all(hwnd: HWND) {
    for id in HK_TOGGLE..=HK_GAME {
        unsafe {
            let _ = UnregisterHotKey(Some(hwnd), id);
        }
    }
}
