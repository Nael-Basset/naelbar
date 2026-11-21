//! naelbar palette: neutral black and white, one configurable accent.

use std::sync::atomic::{AtomicU32, Ordering};


use super::Color;

pub const SURFACE: Color = Color::rgb(0x0E, 0x0E, 0x0F);
pub const TILE: Color = Color::rgb(0x1C, 0x1C, 0x1E);
pub const TILE_HOVER: Color = Color::rgb(0x29, 0x29, 0x2C);
pub const HAIRLINE: Color = Color::rgba(0xFF, 0xFF, 0xFF, 0x18);
pub const TEXT: Color = Color::rgb(0xF5, 0xF5, 0xF5);
pub const TEXT_DIM: Color = Color::rgb(0xA3, 0xA3, 0xA8);
pub const TEXT_FAINT: Color = Color::rgb(0x6C, 0x6C, 0x72);

pub const DEFAULT_ACCENT: u32 = 0xF5F5F5;
static ACCENT_RGB: AtomicU32 = AtomicU32::new(DEFAULT_ACCENT);

/// Parses `#RRGGBB`.
pub fn parse_hex(s: &str) -> Option<u32> {
    let h = s.strip_prefix('#')?;
    if h.len() != 6 {
        return None;
    }
    u32::from_str_radix(h, 16).ok()
}

pub fn set_accent(rgb: u32) {
    ACCENT_RGB.store(rgb & 0xFF_FFFF, Ordering::Relaxed);
}

fn channels() -> (u8, u8, u8) {
    let v = ACCENT_RGB.load(Ordering::Relaxed);
    ((v >> 16) as u8, (v >> 8) as u8, v as u8)
}

pub fn accent() -> Color {
    let (r, g, b) = channels();
    Color::rgb(r, g, b)
}

pub fn accent_soft() -> Color {
    let (r, g, b) = channels();
    Color::rgba(r, g, b, 0x2E)
}

/// Lowest glass opacity, so text on the island stays readable on light wallpapers.
pub const GLASS_MIN: f32 = 0.6;
/// Glass opacity in percent; 0 means the opaque surface is used.
static GLASS_PCT: AtomicU32 = AtomicU32::new(0);

/// Turns the glass surface on (`opacity` clamped to 0.6..=1.0) or off.
pub fn set_glass(on: bool, opacity: f32) {
    let pct = if on { (opacity.clamp(GLASS_MIN, 1.0) * 100.0).round() as u32 } else { 0 };
    GLASS_PCT.store(pct, Ordering::Relaxed);
}

fn glass_pct() -> Option<f32> {
    match GLASS_PCT.load(Ordering::Relaxed) {
        0 => None,
        p => Some(p as f32 / 100.0),
    }
}

/// Fill of the island body: opaque, or a slightly lighter translucent tint.
pub fn island_fill() -> Color {
    match glass_pct() {
        Some(a) => Color::rgb(0x17, 0x17, 0x19).alpha(a),
        None => SURFACE,
    }
}

/// Outline of the island body; glass adds a brighter hairline highlight.
pub fn island_edge() -> Color {
    if glass_pct().is_some() { Color::rgba(0xFF, 0xFF, 0xFF, 0x36) } else { HAIRLINE }
}

pub const R_TILE: f32 = 14.0;
pub const R_SMALL: f32 = 9.0;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_colors() {
        assert_eq!(parse_hex("#F2A93B"), Some(0xF2A93B));
        assert_eq!(parse_hex("F2A93B"), None);
        assert_eq!(parse_hex("#F2A"), None);
        assert_eq!(parse_hex("#GGGGGG"), None);
    }

    #[test]
    fn glass_opacity_is_clamped_and_off_is_opaque() {
        set_glass(true, 0.1);
        assert!((island_fill().a - GLASS_MIN).abs() < 0.01);
        set_glass(true, 5.0);
        assert!((island_fill().a - 1.0).abs() < 0.01);
        set_glass(false, 0.7);
        assert_eq!(island_fill(), SURFACE);
        assert_eq!(island_edge(), HAIRLINE);
    }
}
