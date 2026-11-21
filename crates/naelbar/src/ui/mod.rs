//! Small immediate-mode UI. A build pass records a display list ([`Cmd`]) and
//! hit-tests at the same time. The display list is compared with the previous
//! one, so anything that changes nothing visible never reaches the GPU.

pub mod icons;
pub mod theme;
pub mod widgets;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::state::Command;
pub use icons::Icon;

static ENGLISH: AtomicBool = AtomicBool::new(false);

pub fn set_english(on: bool) {
    ENGLISH.store(on, Ordering::Relaxed);
}

pub fn is_english() -> bool {
    ENGLISH.load(Ordering::Relaxed)
}

/// Picks the label for the current language.
pub fn tr(fr: &'static str, en: &'static str) -> &'static str {
    if is_english() { en } else { fr }
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Pt {
    pub x: f32,
    pub y: f32,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Rect {
    pub const fn new(x: f32, y: f32, w: f32, h: f32) -> Self {
        Self { x, y, w, h }
    }
    pub fn right(&self) -> f32 {
        self.x + self.w
    }
    pub fn bottom(&self) -> f32 {
        self.y + self.h
    }
    pub fn center(&self) -> Pt {
        Pt { x: self.x + self.w * 0.5, y: self.y + self.h * 0.5 }
    }
    pub fn contains(&self, p: Pt) -> bool {
        p.x >= self.x && p.x < self.right() && p.y >= self.y && p.y < self.bottom()
    }
    pub fn inset(&self, d: f32) -> Rect {
        self.inset_xy(d, d)
    }
    pub fn inset_xy(&self, dx: f32, dy: f32) -> Rect {
        Rect::new(self.x + dx, self.y + dy, (self.w - 2.0 * dx).max(0.0), (self.h - 2.0 * dy).max(0.0))
    }
    /// Rect of the given size centred inside this one.
    pub fn centered(&self, w: f32, h: f32) -> Rect {
        Rect::new(self.x + (self.w - w) * 0.5, self.y + (self.h - h) * 0.5, w, h)
    }
    /// Cuts a strip off the left and returns it; `gap` is removed as well.
    pub fn take_left(&mut self, w: f32, gap: f32) -> Rect {
        let out = Rect::new(self.x, self.y, w, self.h);
        self.x += w + gap;
        self.w = (self.w - w - gap).max(0.0);
        out
    }
    pub fn take_right(&mut self, w: f32, gap: f32) -> Rect {
        let out = Rect::new(self.right() - w, self.y, w, self.h);
        self.w = (self.w - w - gap).max(0.0);
        out
    }
    pub fn take_top(&mut self, h: f32, gap: f32) -> Rect {
        let out = Rect::new(self.x, self.y, self.w, h);
        self.y += h + gap;
        self.h = (self.h - h - gap).max(0.0);
        out
    }
    /// Splits into `n` equal columns separated by `gap`.
    pub fn columns(&self, n: usize, gap: f32) -> Vec<Rect> {
        let w = (self.w - gap * (n.saturating_sub(1)) as f32) / n.max(1) as f32;
        (0..n).map(|i| Rect::new(self.x + i as f32 * (w + gap), self.y, w, self.h)).collect()
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Color {
    pub r: f32,
    pub g: f32,
    pub b: f32,
    pub a: f32,
}

impl Color {
    pub const fn rgb(r: u8, g: u8, b: u8) -> Self {
        Self::rgba(r, g, b, 255)
    }
    pub const fn rgba(r: u8, g: u8, b: u8, a: u8) -> Self {
        Self { r: r as f32 / 255.0, g: g as f32 / 255.0, b: b as f32 / 255.0, a: a as f32 / 255.0 }
    }
    pub fn alpha(self, a: f32) -> Self {
        Self { a: self.a * a, ..self }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Align {
    Start,
    Center,
    End,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TextStyle {
    pub size: f32,
    pub weight: u16,
    pub color: Color,
    pub align: Align,
    pub valign: Align,
    pub wrap: bool,
}

impl TextStyle {
    pub const fn new(size: f32, weight: u16, color: Color) -> Self {
        Self { size, weight, color, align: Align::Start, valign: Align::Center, wrap: false }
    }
    pub const fn aligned(mut self, align: Align) -> Self {
        self.align = align;
        self
    }
    pub const fn top(mut self) -> Self {
        self.valign = Align::Start;
        self
    }
    pub const fn wrapped(mut self) -> Self {
        self.wrap = true;
        self
    }
}

/// Encoded image bytes, identified by allocation so comparison is cheap.
#[derive(Clone, Debug)]
pub struct ImageRef(pub Arc<[u8]>);

impl PartialEq for ImageRef {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Cmd {
    FillRR { r: Rect, radius: f32, c: Color },
    StrokeRR { r: Rect, radius: f32, width: f32, c: Color },
    Text { r: Rect, s: Box<str>, style: TextStyle },
    Icon { icon: Icon, r: Rect, c: Color, width: f32 },
    Image { r: Rect, radius: f32, image: ImageRef, opacity: f32, contain: bool },
    PushClip { r: Rect, radius: f32 },
    PopClip,
}

#[derive(Default, Clone, Debug)]
pub struct Input {
    /// Pointer position in visual-window DIPs, `None` when outside.
    pub mouse: Option<Pt>,
    pub down: bool,
    /// Edge flags, cleared after each build pass.
    pub pressed: bool,
    pub released: bool,
    pub wheel: f32,
    pub press_pos: Pt,
}

pub type Id = u64;

pub fn id(name: &str, index: usize) -> Id {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in name.bytes().chain(index.to_le_bytes()) {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// Widget interaction state that survives between build passes.
#[derive(Default, Clone, Debug)]
pub struct WidgetState {
    pub active: Option<Id>,
    pub drag_value: f32,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Resp {
    pub hovered: bool,
    pub held: bool,
    pub clicked: bool,
}

pub struct Ui<'a> {
    pub cmds: Vec<Cmd>,
    pub input: &'a Input,
    pub widgets: &'a mut WidgetState,
    pub out: &'a mut Vec<Command>,
    pub scale: f32,
    pub now_ms: u64,
    /// True while files are being dragged over the island.
    pub drag_hover: bool,
    /// Earliest Unix-ms instant at which this frame would look different.
    pub deadline: Option<u64>,
    alpha: f32,
    origin: Pt,
    zoom: f32,
}

impl<'a> Ui<'a> {
    pub fn new(input: &'a Input, widgets: &'a mut WidgetState, out: &'a mut Vec<Command>, scale: f32, now_ms: u64) -> Self {
        Self { cmds: Vec::with_capacity(128), input, widgets, out, scale, now_ms, drag_hover: false, deadline: None, alpha: 1.0, origin: Pt::default(), zoom: 1.0 }
    }

    pub fn set_alpha(&mut self, a: f32) {
        self.alpha = a;
    }

    /// Scales everything drawn from now on around `origin`. Hit-testing works
    /// in unscaled coordinates, so the caller also swaps `input`.
    pub fn set_zoom(&mut self, origin: Pt, zoom: f32) {
        self.origin = origin;
        self.zoom = zoom;
    }

    pub fn clear_zoom(&mut self) {
        self.zoom = 1.0;
    }

    fn mapped(&self, r: Rect) -> Rect {
        let k = self.zoom;
        Rect::new(self.origin.x + (r.x - self.origin.x) * k, self.origin.y + (r.y - self.origin.y) * k, r.w * k, r.h * k)
    }

    /// Requests a repaint at `at_ms` (Unix ms). Earliest request wins.
    pub fn redraw_at(&mut self, at_ms: u64) {
        self.deadline = Some(self.deadline.map_or(at_ms, |d| d.min(at_ms)));
    }

    pub fn fill(&mut self, r: Rect, radius: f32, c: Color) {
        self.cmds.push(Cmd::FillRR { r: self.mapped(r), radius: radius * self.zoom, c: c.alpha(self.alpha) });
    }

    pub fn stroke(&mut self, r: Rect, radius: f32, width: f32, c: Color) {
        self.cmds.push(Cmd::StrokeRR { r: self.mapped(r), radius: radius * self.zoom, width: width * self.zoom, c: c.alpha(self.alpha) });
    }

    pub fn text(&mut self, r: Rect, s: &str, style: TextStyle) {
        let style = TextStyle { color: style.color.alpha(self.alpha), size: style.size * self.zoom, ..style };
        self.cmds.push(Cmd::Text { r: self.mapped(r), s: s.into(), style });
    }

    pub fn icon(&mut self, icon: Icon, r: Rect, c: Color) {
        self.cmds.push(Cmd::Icon { icon, r: self.mapped(r), c: c.alpha(self.alpha), width: 1.7 * self.zoom });
    }

    pub fn image(&mut self, r: Rect, radius: f32, image: &Arc<[u8]>) {
        self.cmds.push(Cmd::Image { r: self.mapped(r), radius: radius * self.zoom, image: ImageRef(image.clone()), opacity: self.alpha, contain: false });
    }

    /// Like [`Ui::image`], but shows the whole picture instead of filling `r`.
    pub fn image_contain(&mut self, r: Rect, radius: f32, image: &Arc<[u8]>) {
        self.cmds.push(Cmd::Image { r: self.mapped(r), radius: radius * self.zoom, image: ImageRef(image.clone()), opacity: self.alpha, contain: true });
    }

    pub fn push_clip(&mut self, r: Rect, radius: f32) {
        self.cmds.push(Cmd::PushClip { r: self.mapped(r), radius: radius * self.zoom });
    }

    pub fn pop_clip(&mut self) {
        self.cmds.push(Cmd::PopClip);
    }

    pub fn hovered(&self, r: Rect) -> bool {
        self.input.mouse.is_some_and(|m| r.contains(m))
    }

    /// Button-like interaction for the widget `w` occupying `r`.
    pub fn interact(&mut self, w: Id, r: Rect) -> Resp {
        let hovered = self.hovered(r);
        let mut resp = Resp { hovered, ..Resp::default() };
        if self.widgets.active == Some(w) {
            resp.held = self.input.down;
            if self.input.released || !self.input.down {
                self.widgets.active = None;
                resp.clicked = hovered && self.input.released;
                resp.held = false;
            }
        } else if hovered && self.input.pressed && self.widgets.active.is_none() {
            self.widgets.active = Some(w);
            resp.held = true;
        }
        resp
    }
}

/// `m:ss`, or `h:mm:ss` from one hour.
pub fn format_time(ms: u64) -> String {
    let s = ms / 1000;
    if s >= 3600 {
        format!("{}:{:02}:{:02}", s / 3600, (s / 60) % 60, s % 60)
    } else {
        format!("{}:{:02}", s / 60, s % 60)
    }
}
