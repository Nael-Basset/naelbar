//! Audio visualizer in the closed pill, next to the artwork.
//!
//! It reads the default output's peak meter (no capture stream) about 30 times
//! per second, and only from a build pass that draws it: while media plays, the
//! island is closed, visible and not a thin bar. Each sample asks for one more
//! frame 33 ms later. A frame that does not draw it releases the meter, so a
//! pause, the open card or the thin bar stop everything.

use std::cell::RefCell;

use nb_media::{MAX_CHANNELS, PeakMeter};

use crate::ui::theme::{TEXT_FAINT, accent};
use crate::ui::{Rect, Ui};

/// Time between two samples, about 30 per second.
pub const SAMPLE_MS: u64 = 33;
pub const BARS: usize = 5;
/// Time constants of the smoothing: quick to rise, slow to fall.
const ATTACK_MS: f32 = 28.0;
const DECAY_MS: f32 = 200.0;
/// The default device can change; a fresh meter follows it.
const REOPEN_MS: u64 = 10_000;
/// After a failure (no output device), wait before trying again.
const RETRY_MS: u64 = 5_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Style {
    Bars,
    Pixels,
}

/// The `visualizer` setting: `bars`, `pixels` or `none`.
pub fn style(setting: &str) -> Option<Style> {
    match setting {
        "none" => None,
        "pixels" => Some(Style::Pixels),
        _ => Some(Style::Bars),
    }
}

/// Width the visualizer takes in the pill, in DIPs.
pub fn width(style: Style) -> f32 {
    match style {
        Style::Bars => BARS as f32 * BAR_W + (BARS - 1) as f32 * BAR_GAP,
        Style::Pixels => COLS as f32 * DOT + (COLS - 1) as f32 * DOT_GAP,
    }
}

/// One smoothing step from `prev` toward `target` after `dt_ms`.
pub fn smooth(prev: f32, target: f32, dt_ms: f32) -> f32 {
    let target = target.clamp(0.0, 1.0);
    if dt_ms <= 0.0 {
        return prev;
    }
    let tau = if target > prev { ATTACK_MS } else { DECAY_MS };
    let k = 1.0 - (-dt_ms / tau).exp();
    (prev + (target - prev) * k).clamp(0.0, 1.0)
}

/// Peaks are linear amplitudes: quiet music would barely move the bars.
fn loudness(peak: f32) -> f32 {
    peak.clamp(0.0, 1.0).powf(0.5)
}

/// A steady small variation per bar and sample, 0.72 to 1.0, so two
/// channels still give five bars that move on their own.
fn wobble(tick: u32, bar: usize) -> f32 {
    let mut h = tick.wrapping_mul(0x9E37_79B9) ^ (bar as u32).wrapping_mul(0x85EB_CA6B);
    h ^= h >> 15;
    h = h.wrapping_mul(0x2C1B_3C6D);
    h ^= h >> 12;
    0.72 + (h & 0xFFFF) as f32 / 65535.0 * 0.28
}

/// Bar targets from the overall peak and the channel peaks: left channel on
/// the left, right channel on the right, the mix in the middle.
pub fn targets(peak: f32, channels: &[f32], tick: u32) -> [f32; BARS] {
    let l = channels.first().copied().unwrap_or(peak);
    let r = channels.get(1).copied().unwrap_or(l);
    let base = [l, (l + peak) * 0.5, peak, (r + peak) * 0.5, r];
    let mut out = [0.0; BARS];
    for (i, b) in base.iter().enumerate() {
        let w = if i == BARS / 2 { 1.0 } else { wobble(tick, i) };
        out[i] = (loudness(*b) * w).clamp(0.0, 1.0);
    }
    out
}

struct Viz {
    meter: Option<PeakMeter>,
    opened_at: u64,
    retry_at: u64,
    last_sample: u64,
    levels: [f32; BARS],
    tick: u32,
    drawn: bool,
}

thread_local! {
    static VIZ: RefCell<Viz> = const { RefCell::new(Viz { meter: None, opened_at: 0, retry_at: 0, last_sample: 0, levels: [0.0; BARS], tick: 0, drawn: false }) };
}

impl Viz {
    fn sample(&mut self, now: u64) {
        if self.meter.as_ref().is_some() && now >= self.opened_at + REOPEN_MS {
            self.meter = None;
        }
        if self.meter.is_none() && now >= self.retry_at {
            match PeakMeter::open() {
                Ok(m) => {
                    self.meter = Some(m);
                    self.opened_at = now;
                }
                Err(_) => self.retry_at = now + RETRY_MS,
            }
        }
        let mut ch = [0.0f32; MAX_CHANNELS];
        let read = self.meter.as_ref().map(|m| m.read(&mut ch));
        let target = match read {
            Some(Ok((peak, n))) => targets(peak, &ch[..n], self.tick),
            Some(Err(_)) => {
                // The device went away: a new one is opened next time.
                self.meter = None;
                [0.0; BARS]
            }
            None => [0.0; BARS],
        };
        let dt = if self.last_sample == 0 { SAMPLE_MS } else { now.saturating_sub(self.last_sample).min(100) };
        for (l, t) in self.levels.iter_mut().zip(target) {
            *l = smooth(*l, t, dt as f32);
        }
        self.tick = self.tick.wrapping_add(1);
        self.last_sample = now;
    }
}

const BAR_W: f32 = 2.0;
const BAR_GAP: f32 = 2.0;
const BAR_MAX_H: f32 = 14.0;
const BAR_MIN_H: f32 = 2.0;
const COLS: usize = 4;
const ROWS: usize = 5;
const DOT: f32 = 2.2;
const DOT_GAP: f32 = 1.2;

/// Snaps a length to a quarter DIP, so a level that barely moves draws
/// exactly the same frame, which the renderer then skips.
fn snap(v: f32) -> f32 {
    (v * 4.0).round() / 4.0
}

/// Draws the visualizer with its left edge at `x`, centred on `cy`, samples
/// the meter when a sample is due and asks for the next frame.
pub fn draw(ui: &mut Ui, x: f32, cy: f32, style: Style) {
    let now = ui.now_ms;
    let levels = VIZ.with(|v| {
        let mut v = v.borrow_mut();
        v.drawn = true;
        if now.saturating_sub(v.last_sample) + 3 >= SAMPLE_MS {
            v.sample(now);
        }
        ui.redraw_at(v.last_sample + SAMPLE_MS);
        v.levels
    });
    let color = accent();
    match style {
        Style::Bars => {
            for (i, l) in levels.iter().enumerate() {
                let h = snap(BAR_MIN_H + (BAR_MAX_H - BAR_MIN_H) * l);
                let bx = x + i as f32 * (BAR_W + BAR_GAP);
                ui.fill(Rect::new(bx, cy - h * 0.5, BAR_W, h), BAR_W * 0.5, color);
            }
        }
        Style::Pixels => {
            let cols = [levels[0], (levels[1] + levels[2]) * 0.5, (levels[2] + levels[3]) * 0.5, levels[4]];
            let grid_h = ROWS as f32 * DOT + (ROWS - 1) as f32 * DOT_GAP;
            let bottom = cy + grid_h * 0.5;
            for (c, l) in cols.iter().enumerate() {
                // The bottom dot is always lit, so silence still shows a line.
                let lit = ((l * ROWS as f32).round() as usize).clamp(1, ROWS);
                for r in 0..ROWS {
                    let dx = x + c as f32 * (DOT + DOT_GAP);
                    let dy = bottom - DOT - r as f32 * (DOT + DOT_GAP);
                    let on = r < lit;
                    ui.fill(Rect::new(dx, dy, DOT, DOT), 0.6, if on { color } else { TEXT_FAINT.alpha(0.45) });
                }
            }
        }
    }
}

/// Called once per build pass, after the pill. If the visualizer was not
/// drawn, the meter is released and the levels fall back to rest.
pub fn end_frame() {
    VIZ.with(|v| {
        let mut v = v.borrow_mut();
        if !v.drawn {
            v.meter = None;
            v.levels = [0.0; BARS];
            v.last_sample = 0;
        }
        v.drawn = false;
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attack_is_faster_than_decay() {
        let up = smooth(0.0, 1.0, 33.0);
        let down = 1.0 - smooth(1.0, 0.0, 33.0);
        assert!(up > 0.6, "{up}");
        assert!(down < 0.2, "{down}");
        assert!(up > down * 3.0);
    }

    #[test]
    fn smoothing_converges_and_stays_in_range() {
        let mut v = 0.0;
        for _ in 0..30 {
            v = smooth(v, 0.8, 33.0);
        }
        assert!((v - 0.8).abs() < 0.001);
        for _ in 0..60 {
            v = smooth(v, 0.0, 33.0);
        }
        assert!(v < 0.001);
        assert_eq!(smooth(0.5, 3.0, 1000.0), 1.0);
        assert_eq!(smooth(0.5, -1.0, 10_000.0), 0.0);
        assert_eq!(smooth(0.3, 1.0, 0.0), 0.3);
    }

    #[test]
    fn smoothing_does_not_depend_on_frame_rate() {
        let step = smooth(smooth(1.0, 0.0, 66.0), 0.0, 66.0);
        assert!((step - smooth(1.0, 0.0, 132.0)).abs() < 1e-5);
    }

    #[test]
    fn targets_follow_channels() {
        let t = targets(0.0, &[0.0, 0.0], 3);
        assert!(t.iter().all(|v| *v == 0.0));
        let t = targets(1.0, &[1.0, 0.0], 3);
        assert!(t[0] > 0.7 && t[4] == 0.0 && t[2] == 1.0);
        let t = targets(0.25, &[], 9);
        assert!(t.iter().all(|v| *v > 0.3 && *v <= 0.5));
    }

    #[test]
    fn settings_map_to_styles() {
        assert_eq!(style("bars"), Some(Style::Bars));
        assert_eq!(style("pixels"), Some(Style::Pixels));
        assert_eq!(style("none"), None);
        assert_eq!(style("whatever"), Some(Style::Bars));
        assert_eq!(width(Style::Bars), 18.0);
    }
}
