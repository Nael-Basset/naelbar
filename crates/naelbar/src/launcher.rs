//! The six launcher slots on top of the Dock tab: capture, region capture,
//! recording and three free shortcuts. Everything is opened through
//! `ShellExecuteW` with the file and the parameters kept apart, never through
//! a shell command line.

use serde::{Deserialize, Serialize};
use std::sync::Mutex;
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::Shell::{FOLDERID_Pictures, FOLDERID_Screenshots, KF_FLAG_DEFAULT, SHGetKnownFolderPath, ShellExecuteW};
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
use windows::core::{HSTRING, PCWSTR, w};

use crate::ui::{Icon, tr};

pub const SLOT_COUNT: usize = 6;
/// Longest target text kept, in characters.
pub const MAX_TARGET: usize = 260;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Screenshot,
    Region,
    Recording,
    App,
    Link,
    Folder,
    Command,
}

pub const KINDS: [Kind; 7] = [Kind::Screenshot, Kind::Region, Kind::Recording, Kind::App, Kind::Link, Kind::Folder, Kind::Command];

impl Kind {
    pub fn id(self) -> &'static str {
        match self {
            Kind::Screenshot => "screenshot",
            Kind::Region => "region",
            Kind::Recording => "recording",
            Kind::App => "app",
            Kind::Link => "link",
            Kind::Folder => "folder",
            Kind::Command => "command",
        }
    }

    pub fn parse(s: &str) -> Option<Kind> {
        KINDS.iter().copied().find(|k| k.id() == s)
    }

    pub fn label(self) -> &'static str {
        match self {
            Kind::Screenshot => tr("Capture écran", "Screenshot"),
            Kind::Region => tr("Capture zone", "Region capture"),
            Kind::Recording => tr("Enregistrement", "Recording"),
            Kind::App => tr("Appli", "App"),
            Kind::Link => tr("Lien", "Link"),
            Kind::Folder => tr("Dossier", "Folder"),
            Kind::Command => tr("Commande", "Command"),
        }
    }

    pub fn icon(self) -> Icon {
        match self {
            Kind::Screenshot => Icon::Camera,
            Kind::Region => Icon::Crop,
            Kind::Recording => Icon::Record,
            Kind::App => Icon::AppWindow,
            Kind::Link => Icon::Link,
            Kind::Folder => Icon::Folder,
            Kind::Command => Icon::Terminal,
        }
    }

    pub fn next(self) -> Kind {
        let i = KINDS.iter().position(|k| *k == self).unwrap_or(0);
        KINDS[(i + 1) % KINDS.len()]
    }

    /// Whether the target text means anything for this kind.
    pub fn uses_target(self) -> bool {
        !matches!(self, Kind::Screenshot | Kind::Region | Kind::Recording)
    }

    pub fn hint(self) -> &'static str {
        match self {
            Kind::App => tr("Application ou chemin du .exe", "App name or .exe path"),
            Kind::Link => tr("Lien, par exemple https://…", "Link, for example https://…"),
            Kind::Folder => tr("Dossier, vide pour Captures d'écran", "Folder, empty for Screenshots"),
            Kind::Command => tr("Programme puis arguments", "Program, then arguments"),
            _ => tr("Aucune cible", "No target"),
        }
    }
}

/// Stored as plain text so a typo in the file only spoils one slot.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Slot {
    pub kind: String,
    pub target: String,
}

impl Default for Slot {
    fn default() -> Self {
        Slot { kind: Kind::App.id().into(), target: String::new() }
    }
}

impl Slot {
    pub fn new(kind: Kind, target: &str) -> Slot {
        Slot { kind: kind.id().into(), target: target.into() }
    }
    pub fn kind(&self) -> Kind {
        Kind::parse(&self.kind).unwrap_or(Kind::App)
    }
}

pub fn default_slots() -> Vec<Slot> {
    vec![
        Slot::new(Kind::Screenshot, ""),
        Slot::new(Kind::Region, ""),
        Slot::new(Kind::Recording, ""),
        Slot::new(Kind::Folder, ""),
        Slot::new(Kind::App, "calc.exe"),
        Slot::new(Kind::Link, "ms-settings:"),
    ]
}

/// Exactly six slots, known kinds, clean targets.
pub fn sanitize(mut slots: Vec<Slot>) -> Vec<Slot> {
    let defaults = default_slots();
    slots.truncate(SLOT_COUNT);
    for (i, d) in defaults.iter().enumerate() {
        match slots.get_mut(i) {
            None => slots.push(d.clone()),
            Some(s) => {
                if Kind::parse(&s.kind).is_none() {
                    *s = d.clone();
                    continue;
                }
                let clean: String = s.target.chars().filter(|c| !c.is_control()).take(MAX_TARGET).collect();
                s.target = clean.trim().to_string();
            }
        }
    }
    slots
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Invalid {
    Empty,
    Control,
    Link,
    Command,
}

impl Invalid {
    pub fn message(self) -> &'static str {
        match self {
            Invalid::Empty => tr("Écris d'abord une cible.", "Type a target first."),
            Invalid::Control => tr("Caractères interdits.", "Forbidden characters."),
            Invalid::Link => tr("Lien invalide, par exemple https://…", "Invalid link, for example https://…"),
            Invalid::Command => tr("Commande invalide.", "Invalid command."),
        }
    }
}

fn is_scheme(s: &str) -> bool {
    let mut chars = s.chars();
    // Two letters at least: `C:` is a drive, not a scheme.
    s.len() >= 2 && chars.next().is_some_and(|c| c.is_ascii_alphabetic()) && chars.all(|c| c.is_ascii_alphanumeric() || "+.-".contains(c))
}

/// Checks a target for its kind. The text is trimmed by the caller.
pub fn validate(kind: Kind, target: &str) -> Result<(), Invalid> {
    if target.chars().any(char::is_control) || target.chars().count() > MAX_TARGET {
        return Err(Invalid::Control);
    }
    match kind {
        Kind::Screenshot | Kind::Region | Kind::Recording | Kind::Folder => Ok(()),
        Kind::App => {
            if target.is_empty() {
                Err(Invalid::Empty)
            } else {
                Ok(())
            }
        }
        Kind::Link => {
            if target.is_empty() {
                return Err(Invalid::Empty);
            }
            let scheme = target.split_once(':').map(|(s, _)| s.to_ascii_lowercase()).ok_or(Invalid::Link)?;
            if !is_scheme(&scheme) || ["javascript", "vbscript", "data"].contains(&scheme.as_str()) {
                return Err(Invalid::Link);
            }
            Ok(())
        }
        Kind::Command => {
            if target.is_empty() {
                return Err(Invalid::Empty);
            }
            split_command(target).map(|_| ()).ok_or(Invalid::Command)
        }
    }
}

/// `"C:\Program Files\x.exe" --flag` or `notepad file.txt` into file and parameters.
pub fn split_command(s: &str) -> Option<(String, String)> {
    let s = s.trim();
    if let Some(rest) = s.strip_prefix('"') {
        let end = rest.find('"')?;
        let file = &rest[..end];
        return (!file.is_empty()).then(|| (file.to_string(), rest[end + 1..].trim().to_string()));
    }
    let (file, params) = match s.split_once(char::is_whitespace) {
        Some((f, p)) => (f, p.trim()),
        None => (s, ""),
    };
    (!file.is_empty()).then(|| (file.to_string(), params.to_string()))
}

static SLOTS: Mutex<Option<Vec<Slot>>> = Mutex::new(None);

/// The slots in use, read from the config the first time.
pub fn slots() -> Vec<Slot> {
    let mut g = SLOTS.lock().unwrap_or_else(|e| e.into_inner());
    g.get_or_insert_with(|| crate::config::Config::load().slots).clone()
}

/// Called when the config file changes.
pub fn set_slots(v: Vec<Slot>) {
    *SLOTS.lock().unwrap_or_else(|e| e.into_inner()) = Some(sanitize(v));
}

fn known_folder(id: &windows::core::GUID) -> Option<String> {
    unsafe {
        let p = SHGetKnownFolderPath(id, KF_FLAG_DEFAULT, None).ok()?;
        let s = p.to_string().ok();
        CoTaskMemFree(Some(p.0 as *const _));
        s
    }
}

fn screenshots_dir() -> Option<String> {
    known_folder(&FOLDERID_Screenshots).or_else(|| known_folder(&FOLDERID_Pictures).map(|p| format!("{p}\\Screenshots")))
}

fn shell_open(file: &str, params: Option<&str>) -> bool {
    unsafe {
        let params = params.filter(|p| !p.is_empty()).map(HSTRING::from);
        let p = params.as_ref().map_or(PCWSTR::null(), |h| PCWSTR(h.as_ptr()));
        ShellExecuteW(None, w!("open"), &HSTRING::from(file), p, PCWSTR::null(), SW_SHOWNORMAL).0 as isize > 32
    }
}

/// Win+PrintScreen: Windows saves the whole screen in the Screenshots folder.
fn press_screenshot() {
    fn key(vk: VIRTUAL_KEY, up: bool) -> INPUT {
        INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 { ki: KEYBDINPUT { wVk: vk, wScan: 0, dwFlags: if up { KEYEVENTF_KEYUP } else { KEYBD_EVENT_FLAGS(0) }, time: 0, dwExtraInfo: 0 } },
        }
    }
    let keys = [key(VK_LWIN, false), key(VK_SNAPSHOT, false), key(VK_SNAPSHOT, true), key(VK_LWIN, true)];
    unsafe {
        SendInput(&keys, size_of::<INPUT>() as i32);
    }
}

fn execute(slot: &Slot) {
    let target = slot.target.trim();
    let kind = slot.kind();
    if validate(kind, target).is_err() {
        return;
    }
    match kind {
        Kind::Screenshot => {
            // Lets the card get out of the picture.
            std::thread::sleep(std::time::Duration::from_millis(350));
            press_screenshot();
        }
        Kind::Region => {
            shell_open("ms-screenclip:", None);
        }
        Kind::Recording => {
            // Snipping Tool's video mode, which records the mic and the PC
            // sound by itself. Older builds ignore the path and open the app.
            if !shell_open("ms-screenclip://capture/video", None) {
                shell_open("snippingtool.exe", None);
            }
        }
        Kind::App | Kind::Link => {
            shell_open(target, None);
        }
        Kind::Folder => {
            let dir = if target.is_empty() { screenshots_dir() } else { Some(target.to_string()) };
            if let Some(d) = dir {
                shell_open(&d, None);
            }
        }
        Kind::Command => {
            if let Some((file, params)) = split_command(target) {
                shell_open(&file, Some(&params));
            }
        }
    }
}

/// Runs a slot off the UI thread: the shell may take a moment.
pub fn run(slot: &Slot) {
    let slot = slot.clone();
    std::thread::spawn(move || execute(&slot));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn six_default_slots_in_order() {
        let d = default_slots();
        assert_eq!(d.len(), SLOT_COUNT);
        let kinds: Vec<Kind> = d.iter().map(Slot::kind).collect();
        assert_eq!(kinds, [Kind::Screenshot, Kind::Region, Kind::Recording, Kind::Folder, Kind::App, Kind::Link]);
        assert_eq!(d[5].target, "ms-settings:");
        assert!(d.iter().all(|s| validate(s.kind(), &s.target).is_ok()));
    }

    #[test]
    fn config_without_slots_gets_the_defaults() {
        let c: crate::config::Config = serde_json::from_str("{}").unwrap();
        assert_eq!(c.slots, default_slots());
    }

    #[test]
    fn kinds_round_trip_and_cycle() {
        for k in KINDS {
            assert_eq!(Kind::parse(k.id()), Some(k));
        }
        assert_eq!(Kind::parse("rm"), None);
        let mut k = Kind::Screenshot;
        for _ in 0..KINDS.len() {
            k = k.next();
        }
        assert_eq!(k, Kind::Screenshot);
    }

    #[test]
    fn sanitize_pads_truncates_and_fixes_kinds() {
        assert_eq!(sanitize(Vec::new()), default_slots());
        let long = vec![Slot::new(Kind::App, "x"); 9];
        assert_eq!(sanitize(long).len(), SLOT_COUNT);
        let bad = vec![Slot { kind: "shell".into(), target: "format c:".into() }];
        assert_eq!(sanitize(bad)[0], default_slots()[0]);
        let messy = vec![Slot { kind: "app".into(), target: "  a\u{7}b  ".into() }];
        assert_eq!(sanitize(messy)[0].target, "ab");
        let huge = vec![Slot { kind: "app".into(), target: "a".repeat(1000) }];
        assert_eq!(sanitize(huge)[0].target.chars().count(), MAX_TARGET);
    }

    #[test]
    fn targets_are_validated_per_kind() {
        assert_eq!(validate(Kind::App, ""), Err(Invalid::Empty));
        assert!(validate(Kind::App, "calc.exe").is_ok());
        assert!(validate(Kind::Folder, "").is_ok());
        assert_eq!(validate(Kind::Link, "example.com"), Err(Invalid::Link));
        assert_eq!(validate(Kind::Link, "C:\\x"), Err(Invalid::Link));
        assert_eq!(validate(Kind::Link, "javascript:alert(1)"), Err(Invalid::Link));
        assert!(validate(Kind::Link, "https://example.com").is_ok());
        assert!(validate(Kind::Link, "ms-settings:").is_ok());
        assert_eq!(validate(Kind::Command, "\"\""), Err(Invalid::Command));
        assert_eq!(validate(Kind::App, "a\nb"), Err(Invalid::Control));
    }

    #[test]
    fn commands_split_into_file_and_parameters() {
        assert_eq!(split_command("notepad a.txt b.txt"), Some(("notepad".into(), "a.txt b.txt".into())));
        assert_eq!(split_command("\"C:\\Program Files\\x.exe\" -v"), Some(("C:\\Program Files\\x.exe".into(), "-v".into())));
        assert_eq!(split_command("calc"), Some(("calc".into(), String::new())));
        assert_eq!(split_command("\"open"), None);
        assert_eq!(split_command("   "), None);
    }
}
