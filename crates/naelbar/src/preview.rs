//! Full-size look at a copied image. The window exists only while the button
//! is held: it is created on demand and destroyed on release, so it costs
//! nothing the rest of the time.

use std::sync::{Arc, Once};

use windows::Win32::Foundation::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::w;

use crate::render::Renderer;
use crate::ui::theme::{HAIRLINE, SURFACE};
use crate::ui::{Cmd, Color, ImageRef, Rect};

/// Share of the screen the preview may take.
const SCREEN_SHARE: f32 = 0.86;
/// Room around the card for its shadow, in DIPs.
const MARGIN: f32 = 20.0;
const PADDING: f32 = 8.0;
const RADIUS: f32 = 16.0;

/// Size of an image shown whole inside `max`, never enlarged.
fn fit(image: (u32, u32), max: (u32, u32)) -> (u32, u32) {
    let (iw, ih) = (image.0.max(1) as f32, image.1.max(1) as f32);
    let k = (max.0 as f32 / iw).min(max.1 as f32 / ih).min(1.0);
    (((iw * k).round() as u32).max(1), ((ih * k).round() as u32).max(1))
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe { DefWindowProcW(hwnd, msg, wp, lp) }
}

pub struct Preview {
    hwnd: HWND,
    renderer: Option<Renderer>,
}

impl Drop for Preview {
    fn drop(&mut self) {
        self.renderer = None;
        unsafe {
            let _ = DestroyWindow(self.hwnd);
        }
    }
}

/// Opens the preview centred on `screen`, drawn before it becomes visible.
pub fn show(screen: RECT, scale: f32, png: &Arc<[u8]>, width: u32, height: u32) -> Option<Preview> {
    static REGISTER: Once = Once::new();
    let class = w!("naelbar.preview");
    unsafe {
        let instance = GetModuleHandleW(None).ok()?;
        REGISTER.call_once(|| {
            let wc = WNDCLASSEXW { cbSize: size_of::<WNDCLASSEXW>() as u32, lpfnWndProc: Some(wndproc), hInstance: instance.into(), lpszClassName: class, ..Default::default() };
            RegisterClassExW(&wc);
        });

        let (sw, sh) = (screen.right - screen.left, screen.bottom - screen.top);
        let around = ((MARGIN + PADDING) * scale).round() as i32;
        let max_w = ((sw as f32 * SCREEN_SHARE) as i32 - 2 * around).max(1) as u32;
        let max_h = ((sh as f32 * SCREEN_SHARE) as i32 - 2 * around).max(1) as u32;
        let (iw, ih) = fit((width, height), (max_w, max_h));
        let (ww, wh) = (iw as i32 + 2 * around, ih as i32 + 2 * around);

        let ex = WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_NOREDIRECTIONBITMAP;
        let hwnd = CreateWindowExW(ex, class, w!("naelbar preview"), WS_POPUP, screen.left + (sw - ww) / 2, screen.top + (sh - wh) / 2, ww, wh, None, None, Some(instance.into()), None).ok()?;
        let _ = SetLayeredWindowAttributes(hwnd, COLORREF(0), 255, LWA_ALPHA);

        let mut renderer = Renderer::new(hwnd);
        renderer.set_size(ww as u32, wh as u32, scale);
        renderer.set_max_image_edge(iw.max(ih));
        let mut preview = Preview { hwnd, renderer: None };

        let window = Rect::new(0.0, 0.0, ww as f32 / scale, wh as f32 / scale);
        let body = window.inset(MARGIN);
        let mut cmds = Vec::with_capacity(7);
        for i in 1..=4 {
            let spread = 4.0 * i as f32;
            cmds.push(Cmd::FillRR { r: body.inset(-spread), radius: RADIUS + spread, c: Color::rgb(0, 0, 0).alpha(0.16 - 0.03 * i as f32) });
        }
        cmds.push(Cmd::FillRR { r: body, radius: RADIUS, c: SURFACE });
        cmds.push(Cmd::Image { r: body.inset(PADDING), radius: RADIUS - PADDING, image: ImageRef(png.clone()), opacity: 1.0, contain: true });
        cmds.push(Cmd::StrokeRR { r: body.inset(0.5), radius: RADIUS, width: 1.0, c: HAIRLINE });
        renderer.render(&cmds).ok()?;

        let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        preview.renderer = Some(renderer);
        Some(preview)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_large_image_shrinks_and_keeps_its_shape() {
        assert_eq!(fit((3840, 2160), (1600, 900)), (1600, 900));
        assert_eq!(fit((1000, 2000), (800, 800)), (400, 800));
    }

    #[test]
    fn a_small_image_is_not_enlarged() {
        assert_eq!(fit((200, 100), (1600, 900)), (200, 100));
    }
}
