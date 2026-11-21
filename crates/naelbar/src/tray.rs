//! Notification-area icon and its right-click menu.

use windows::Win32::Foundation::{HWND, LPARAM, POINT, WPARAM};
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::UI::Shell::*;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{HSTRING, PCWSTR};

use crate::ui::tr;

pub const WM_TRAY: u32 = WM_APP + 2;
pub const MENU_SETTINGS: usize = 1;
pub const MENU_QUIT: usize = 2;

pub struct Tray {
    data: NOTIFYICONDATAW,
}

impl Tray {
    pub fn add(hwnd: HWND) -> Self {
        unsafe {
            let mut data = NOTIFYICONDATAW {
                cbSize: size_of::<NOTIFYICONDATAW>() as u32,
                hWnd: hwnd,
                uID: 1,
                uFlags: NIF_ICON | NIF_MESSAGE | NIF_TIP,
                uCallbackMessage: WM_TRAY,
                hIcon: make_icon().unwrap_or_default(),
                ..Default::default()
            };
            let tip: Vec<u16> = "naelbar".encode_utf16().collect();
            data.szTip[..tip.len()].copy_from_slice(&tip);
            let _ = Shell_NotifyIconW(NIM_ADD, &data);
            Self { data }
        }
    }

    /// The taskbar restarted and forgot every icon.
    pub fn restore(&self) {
        unsafe {
            let _ = Shell_NotifyIconW(NIM_ADD, &self.data);
        }
    }

    pub fn remove(&self) {
        unsafe {
            let _ = Shell_NotifyIconW(NIM_DELETE, &self.data);
        }
    }
}

/// A small white pill on transparency, drawn in code.
unsafe fn make_icon() -> Option<HICON> {
    const SIZE: i32 = 32;
    let info = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: SIZE,
            biHeight: -SIZE,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut bits = std::ptr::null_mut();
    let color = CreateDIBSection(None, &info, DIB_RGB_COLORS, &mut bits, None, 0).ok()?;
    let pixels = std::slice::from_raw_parts_mut(bits as *mut u32, (SIZE * SIZE) as usize);
    let (cx, cy, half_w, half_h) = (15.5f32, 15.5f32, 14.0f32, 7.0f32);
    for y in 0..SIZE {
        for x in 0..SIZE {
            let dx = ((x as f32 - cx).abs() - (half_w - half_h)).max(0.0);
            let dy = (y as f32 - cy).abs();
            let d = (dx * dx + dy * dy).sqrt() - half_h;
            let a = (0.5 - d).clamp(0.0, 1.0);
            let (r, g, b) = (0xF5 as f32 * a, 0xF5 as f32 * a, 0xF5 as f32 * a);
            pixels[(y * SIZE + x) as usize] = ((a * 255.0) as u32) << 24 | (r as u32) << 16 | (g as u32) << 8 | b as u32;
        }
    }
    let mask = CreateBitmap(SIZE, SIZE, 1, 1, None);
    let icon = ICONINFO { fIcon: true.into(), hbmColor: color, hbmMask: mask, ..Default::default() };
    let handle = CreateIconIndirect(&icon).ok();
    let _ = DeleteObject(color.into());
    let _ = DeleteObject(mask.into());
    handle
}

/// Blocks in the menu's modal loop. Returns the chosen item id, 0 when dismissed.
pub fn show_menu(hwnd: HWND) -> usize {
    unsafe {
        let Ok(menu) = CreatePopupMenu() else { return 0 };
        let _ = AppendMenuW(menu, MF_STRING, MENU_SETTINGS, &HSTRING::from(tr("Paramètres", "Settings")));
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
        let _ = AppendMenuW(menu, MF_STRING, MENU_QUIT, &HSTRING::from(tr("Quitter", "Quit")));
        let mut pt = POINT::default();
        let _ = GetCursorPos(&mut pt);
        // Without this the menu does not dismiss on an outside click.
        let _ = SetForegroundWindow(hwnd);
        let chosen = TrackPopupMenu(menu, TPM_RETURNCMD | TPM_RIGHTBUTTON | TPM_BOTTOMALIGN, pt.x, pt.y, Some(0), hwnd, None);
        let _ = PostMessageW(Some(hwnd), WM_NULL, WPARAM(0), LPARAM(0));
        let _ = DestroyMenu(menu);
        chosen.0 as usize
    }
}
