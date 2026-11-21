//! Messages from the phone, their validation, and the mouse and keyboard
//! input they turn into. Every number is checked and clamped before it
//! reaches `SendInput`.

use serde::Deserialize;
use windows::Win32::UI::Input::KeyboardAndMouse::*;

/// Largest message the page may send.
pub const MAX_PAYLOAD: usize = 1024;
/// Largest pointer step in one message, in pixels. The page sends about 60
/// messages a second, so this is far above a real swipe.
pub const MAX_STEP: f32 = 400.0;
/// Largest scroll step in one message, in phone pixels.
pub const MAX_SCROLL: f32 = 300.0;
/// Wheel units per phone pixel: 30 pixels make one notch of 120.
const WHEEL_PER_PX: f32 = 4.0;
/// Characters typed by one message.
pub const MAX_TEXT: usize = 200;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Button {
    Left,
    Right,
    Middle,
}

/// The keys the page offers; nothing else can be pressed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Key {
    Esc,
    Tab,
    Back,
    Del,
    Enter,
    Left,
    Up,
    Down,
    Right,
    Win,
    Play,
    Next,
    Prev,
    Mute,
    VolUp,
    VolDown,
}

impl Key {
    fn vk(self) -> (VIRTUAL_KEY, bool) {
        // The second value marks extended keys, which need the flag to be
        // told apart from the numeric keypad.
        match self {
            Key::Esc => (VK_ESCAPE, false),
            Key::Tab => (VK_TAB, false),
            Key::Back => (VK_BACK, false),
            Key::Del => (VK_DELETE, true),
            Key::Enter => (VK_RETURN, false),
            Key::Left => (VK_LEFT, true),
            Key::Up => (VK_UP, true),
            Key::Down => (VK_DOWN, true),
            Key::Right => (VK_RIGHT, true),
            Key::Win => (VK_LWIN, true),
            Key::Play => (VK_MEDIA_PLAY_PAUSE, true),
            Key::Next => (VK_MEDIA_NEXT_TRACK, true),
            Key::Prev => (VK_MEDIA_PREV_TRACK, true),
            Key::Mute => (VK_VOLUME_MUTE, true),
            Key::VolUp => (VK_VOLUME_UP, true),
            Key::VolDown => (VK_VOLUME_DOWN, true),
        }
    }
}

/// What the page may send. Unknown types or fields are refused.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(tag = "t", rename_all = "lowercase", deny_unknown_fields)]
pub enum Msg {
    Move { dx: f32, dy: f32 },
    Scroll { dx: f32, dy: f32 },
    Click { b: Button },
    Btn { b: Button, down: bool },
    Key { k: Key },
    Text { s: String },
}

/// Parses and validates one message. Non-finite numbers and long texts are
/// refused; steps are clamped.
pub fn parse(text: &str) -> Option<Msg> {
    if text.len() > MAX_PAYLOAD {
        return None;
    }
    let m: Msg = serde_json::from_str(text).ok()?;
    let finite = |a: f32, b: f32| a.is_finite() && b.is_finite();
    Some(match m {
        Msg::Move { dx, dy } if finite(dx, dy) => Msg::Move { dx: dx.clamp(-MAX_STEP, MAX_STEP), dy: dy.clamp(-MAX_STEP, MAX_STEP) },
        Msg::Scroll { dx, dy } if finite(dx, dy) => Msg::Scroll { dx: dx.clamp(-MAX_SCROLL, MAX_SCROLL), dy: dy.clamp(-MAX_SCROLL, MAX_SCROLL) },
        Msg::Text { ref s } if !s.is_empty() && s.chars().count() <= MAX_TEXT => m,
        Msg::Move { .. } | Msg::Scroll { .. } | Msg::Text { .. } => return None,
        other => other,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Inject {
    Move(i32, i32),
    Wheel(i32),
    HWheel(i32),
    Button(Button, bool),
    Key(Key),
    /// One UTF-16 unit, typed as is.
    Unit(u16),
}

/// Per connection: sub-pixel remainders and the buttons held down, so a
/// dropped connection never leaves a button pressed.
#[derive(Default)]
pub struct Injector {
    rem: (f32, f32),
    wheel: (f32, f32),
    held: [bool; 3],
}

fn index(b: Button) -> usize {
    match b {
        Button::Left => 0,
        Button::Right => 1,
        Button::Middle => 2,
    }
}

impl Injector {
    /// The input events for one message.
    pub fn plan(&mut self, m: &Msg) -> Vec<Inject> {
        match m {
            Msg::Move { dx, dy } => {
                let (x, y) = (self.rem.0 + dx, self.rem.1 + dy);
                let (ix, iy) = (x.trunc(), y.trunc());
                self.rem = (x - ix, y - iy);
                if ix == 0.0 && iy == 0.0 { Vec::new() } else { vec![Inject::Move(ix as i32, iy as i32)] }
            }
            Msg::Scroll { dx, dy } => {
                // Natural scrolling: the content follows the fingers.
                let (h, v) = (self.wheel.0 - dx * WHEEL_PER_PX, self.wheel.1 + dy * WHEEL_PER_PX);
                let (ih, iv) = (h.trunc(), v.trunc());
                self.wheel = (h - ih, v - iv);
                let mut out = Vec::new();
                if iv != 0.0 {
                    out.push(Inject::Wheel(iv as i32));
                }
                if ih != 0.0 {
                    out.push(Inject::HWheel(ih as i32));
                }
                out
            }
            Msg::Click { b } => {
                if self.held[index(*b)] {
                    return Vec::new();
                }
                vec![Inject::Button(*b, true), Inject::Button(*b, false)]
            }
            Msg::Btn { b, down } => {
                let i = index(*b);
                if self.held[i] == *down {
                    return Vec::new();
                }
                self.held[i] = *down;
                vec![Inject::Button(*b, *down)]
            }
            Msg::Key { k } => vec![Inject::Key(*k)],
            Msg::Text { s } => s
                .chars()
                .filter(|c| !c.is_control() || *c == '\n' || *c == '\t')
                .flat_map(|c| match c {
                    '\n' => vec![Inject::Key(Key::Enter)],
                    '\t' => vec![Inject::Key(Key::Tab)],
                    c => {
                        let mut buf = [0u16; 2];
                        c.encode_utf16(&mut buf).iter().map(|&u| Inject::Unit(u)).collect()
                    }
                })
                .collect(),
        }
    }

    /// Releases whatever is still held.
    pub fn release_all(&mut self) -> Vec<Inject> {
        let mut out = Vec::new();
        for (i, b) in [Button::Left, Button::Right, Button::Middle].into_iter().enumerate() {
            if std::mem::take(&mut self.held[i]) {
                out.push(Inject::Button(b, false));
            }
        }
        out
    }
}

/// Sends the planned events in one `SendInput` call.
pub fn send(events: &[Inject]) {
    if events.is_empty() {
        return;
    }
    let mouse = |dx: i32, dy: i32, data: i32, flags: MOUSE_EVENT_FLAGS| INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 { mi: MOUSEINPUT { dx, dy, mouseData: data as u32, dwFlags: flags, time: 0, dwExtraInfo: 0 } },
    };
    let key = |vk: VIRTUAL_KEY, scan: u16, flags: KEYBD_EVENT_FLAGS| INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 { ki: KEYBDINPUT { wVk: vk, wScan: scan, dwFlags: flags, time: 0, dwExtraInfo: 0 } },
    };
    let mut inputs: Vec<INPUT> = Vec::with_capacity(events.len() * 2);
    for e in events {
        match *e {
            Inject::Move(x, y) => inputs.push(mouse(x, y, 0, MOUSEEVENTF_MOVE)),
            Inject::Wheel(d) => inputs.push(mouse(0, 0, d, MOUSEEVENTF_WHEEL)),
            Inject::HWheel(d) => inputs.push(mouse(0, 0, d, MOUSEEVENTF_HWHEEL)),
            Inject::Button(b, down) => {
                let f = match (b, down) {
                    (Button::Left, true) => MOUSEEVENTF_LEFTDOWN,
                    (Button::Left, false) => MOUSEEVENTF_LEFTUP,
                    (Button::Right, true) => MOUSEEVENTF_RIGHTDOWN,
                    (Button::Right, false) => MOUSEEVENTF_RIGHTUP,
                    (Button::Middle, true) => MOUSEEVENTF_MIDDLEDOWN,
                    (Button::Middle, false) => MOUSEEVENTF_MIDDLEUP,
                };
                inputs.push(mouse(0, 0, 0, f));
            }
            Inject::Key(k) => {
                let (vk, ext) = k.vk();
                let ext = if ext { KEYEVENTF_EXTENDEDKEY } else { KEYBD_EVENT_FLAGS(0) };
                inputs.push(key(vk, 0, ext));
                inputs.push(key(vk, 0, ext | KEYEVENTF_KEYUP));
            }
            Inject::Unit(u) => {
                inputs.push(key(VIRTUAL_KEY(0), u, KEYEVENTF_UNICODE));
                inputs.push(key(VIRTUAL_KEY(0), u, KEYEVENTF_UNICODE | KEYEVENTF_KEYUP));
            }
        }
    }
    unsafe {
        SendInput(&inputs, size_of::<INPUT>() as i32);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_parse() {
        assert_eq!(parse(r#"{"t":"move","dx":1.5,"dy":-2}"#), Some(Msg::Move { dx: 1.5, dy: -2.0 }));
        assert_eq!(parse(r#"{"t":"click","b":"right"}"#), Some(Msg::Click { b: Button::Right }));
        assert_eq!(parse(r#"{"t":"btn","b":"left","down":true}"#), Some(Msg::Btn { b: Button::Left, down: true }));
        assert_eq!(parse(r#"{"t":"key","k":"volup"}"#), Some(Msg::Key { k: Key::VolUp }));
        assert_eq!(parse(r#"{"t":"text","s":"été"}"#), Some(Msg::Text { s: "été".into() }));
    }

    #[test]
    fn invalid_messages_are_refused() {
        for bad in [
            "",
            "null",
            "[]",
            r#"{"t":"exec","cmd":"calc"}"#,
            r#"{"t":"move","dx":1}"#,
            r#"{"t":"move","dx":"1","dy":0}"#,
            r#"{"t":"move","dx":1,"dy":0,"extra":1}"#,
            r#"{"t":"click","b":"x1"}"#,
            r#"{"t":"key","k":"f4"}"#,
            r#"{"t":"text","s":""}"#,
            r#"{"t":"move","dx":1e999,"dy":0}"#,
        ] {
            assert_eq!(parse(bad), None, "{bad}");
        }
        let long = format!(r#"{{"t":"text","s":"{}"}}"#, "a".repeat(MAX_TEXT + 1));
        assert_eq!(parse(&long), None);
    }

    #[test]
    fn steps_are_clamped() {
        assert_eq!(parse(r#"{"t":"move","dx":99999,"dy":-99999}"#), Some(Msg::Move { dx: MAX_STEP, dy: -MAX_STEP }));
        assert_eq!(parse(r#"{"t":"scroll","dx":0,"dy":5000}"#), Some(Msg::Scroll { dx: 0.0, dy: MAX_SCROLL }));
    }

    #[test]
    fn sub_pixel_moves_accumulate() {
        let mut inj = Injector::default();
        assert!(inj.plan(&Msg::Move { dx: 0.6, dy: 0.0 }).is_empty());
        assert_eq!(inj.plan(&Msg::Move { dx: 0.6, dy: -1.5 }), vec![Inject::Move(1, -1)]);
        assert_eq!(inj.plan(&Msg::Move { dx: 0.0, dy: -0.5 }), vec![Inject::Move(0, -1)]);
    }

    #[test]
    fn scrolling_follows_the_fingers() {
        let mut inj = Injector::default();
        assert_eq!(inj.plan(&Msg::Scroll { dx: 0.0, dy: 30.0 }), vec![Inject::Wheel(120)]);
        assert_eq!(inj.plan(&Msg::Scroll { dx: -30.0, dy: 0.0 }), vec![Inject::HWheel(120)]);
    }

    #[test]
    fn text_becomes_units_and_keys() {
        let mut inj = Injector::default();
        assert_eq!(inj.plan(&Msg::Text { s: "é\n😀\u{7}".into() }), vec![Inject::Unit(0xE9), Inject::Key(Key::Enter), Inject::Unit(0xD83D), Inject::Unit(0xDE00)]);
    }

    #[test]
    fn buttons_never_stay_down() {
        let mut inj = Injector::default();
        assert_eq!(inj.plan(&Msg::Btn { b: Button::Left, down: true }), vec![Inject::Button(Button::Left, true)]);
        assert!(inj.plan(&Msg::Btn { b: Button::Left, down: true }).is_empty());
        assert!(inj.plan(&Msg::Click { b: Button::Left }).is_empty());
        assert_eq!(inj.release_all(), vec![Inject::Button(Button::Left, false)]);
        assert!(inj.release_all().is_empty());
        assert_eq!(inj.plan(&Msg::Click { b: Button::Right }).len(), 2);
    }
}
