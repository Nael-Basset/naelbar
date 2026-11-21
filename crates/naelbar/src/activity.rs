//! Live activities in the closed island: browser downloads and the privacy
//! indicators (microphone, camera, screen capture).
//!
//! The data comes from two event driven watchers in `nb-system`. Nothing here
//! samples anything: a download shows the size of its last notification, and a
//! redraw is only requested for the instant "Téléchargé" ends.

use std::path::{Path, PathBuf};

use nb_core::{CaptureKind, CaptureUse, DownloadEvent};

use crate::ui::theme::*;
use crate::ui::{Align, Color, Icon, Rect, TextStyle, Ui, tr};

/// How long the closed island says "Téléchargé".
pub const DONE_MS: u64 = 3000;
/// How long the Quick tab keeps offering the last finished download.
pub const LAST_DONE_MS: u64 = 10 * 60_000;

/// The only colours allowed beyond the theme, like the iOS indicators.
pub const MIC_GREEN: Color = Color::rgb(74, 222, 128);
pub const CAMERA_ORANGE: Color = Color::rgb(255, 159, 10);
pub const SCREEN_BLUE: Color = Color::rgb(10, 132, 255);

pub fn kind_color(kind: CaptureKind) -> Color {
    match kind {
        CaptureKind::Microphone => MIC_GREEN,
        CaptureKind::Camera => CAMERA_ORANGE,
        CaptureKind::Screen => SCREEN_BLUE,
    }
}

pub fn kind_label(kind: CaptureKind) -> &'static str {
    match kind {
        CaptureKind::Microphone => tr("Micro", "Mic"),
        CaptureKind::Camera => tr("Caméra", "Camera"),
        CaptureKind::Screen => tr("Écran", "Screen"),
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Download {
    pub id: u64,
    /// Empty while the browser has not chosen the file name.
    pub name: String,
    pub bytes: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Finished {
    pub name: String,
    pub path: PathBuf,
    pub at_ms: u64,
}

/// What the closed island shows for downloads.
#[derive(Debug, Clone, PartialEq)]
pub enum DownloadLine<'a> {
    Active { download: &'a Download, others: usize },
    Done { finished: &'a Finished, until_ms: u64 },
}

#[derive(Debug, Default)]
pub struct ActivityState {
    /// Executable names or AUMIDs the user does not want indicators for.
    pub ignore: Vec<String>,
    /// Everything the registry reports, before the ignore list is applied.
    raw_captures: Vec<CaptureUse>,
    /// Newest update last.
    active: Vec<Download>,
    done: Option<Finished>,
}

impl ActivityState {
    pub fn apply_download(&mut self, ev: DownloadEvent, now_ms: u64) {
        match ev {
            DownloadEvent::Progress { id, name, bytes } => match self.active.iter().position(|d| d.id == id) {
                Some(i) => {
                    let mut d = self.active.remove(i);
                    d.bytes = bytes;
                    if !name.is_empty() {
                        d.name = name;
                    }
                    self.active.push(d);
                }
                None => self.active.push(Download { id, name, bytes }),
            },
            DownloadEvent::Done { id, name, path, .. } => {
                self.active.retain(|d| d.id != id);
                self.done = Some(Finished { name, path, at_ms: now_ms });
            }
            DownloadEvent::Cancelled { id } => self.active.retain(|d| d.id != id),
        }
    }

    pub fn apply_capture(&mut self, list: Vec<CaptureUse>) {
        self.raw_captures = list;
    }

    /// Forgets the downloads, when their watcher is switched off. The
    /// indicators are cleared by the empty list the service sends.
    pub fn clear_downloads(&mut self) {
        self.active.clear();
        self.done = None;
    }

    /// Applications using a device, without the ignored ones.
    pub fn captures(&self) -> impl Iterator<Item = &CaptureUse> {
        self.raw_captures.iter().filter(|c| !is_ignored(c, &self.ignore))
    }

    pub fn capture_active(&self, kind: CaptureKind) -> bool {
        self.captures().any(|c| c.kind == kind)
    }

    /// True when every application using the microphone is on the ignore list.
    /// False when Windows has not named any user yet.
    pub fn mic_users_all_ignored(&self) -> bool {
        let mut users = self.raw_captures.iter().filter(|c| c.kind == CaptureKind::Microphone).peekable();
        users.peek().is_some() && users.all(|c| is_ignored(c, &self.ignore))
    }

    /// The first application using the microphone that is not ignored.
    pub fn mic_app(&self) -> Option<&CaptureUse> {
        self.captures().find(|c| c.kind == CaptureKind::Microphone)
    }

    pub fn any_capture(&self) -> bool {
        self.captures().next().is_some()
    }

    /// The download line of the closed island at `now_ms`.
    pub fn line(&self, now_ms: u64) -> Option<DownloadLine<'_>> {
        if let Some(d) = self.active.last() {
            return Some(DownloadLine::Active { download: d, others: self.active.len() - 1 });
        }
        self.done.as_ref().filter(|f| now_ms < f.at_ms + DONE_MS).map(|f| DownloadLine::Done { finished: f, until_ms: f.at_ms + DONE_MS })
    }

    /// The last finished download, as long as the Quick tab still offers it.
    pub fn last_done(&self, now_ms: u64) -> Option<&Finished> {
        self.done.as_ref().filter(|f| now_ms < f.at_ms + LAST_DONE_MS)
    }
}

/// True when the application is in the user's ignore list. Entries may be an
/// executable name (with a path or not), a package family name or an AUMID.
pub fn is_ignored(app: &CaptureUse, ignore: &[String]) -> bool {
    ignore.iter().any(|entry| {
        let e = entry.trim();
        if e.is_empty() {
            return false;
        }
        let file = e.rsplit(['\\', '/']).next().unwrap_or(e);
        let package = e.split('!').next().unwrap_or(e);
        [e, file, package].iter().any(|c| c.eq_ignore_ascii_case(&app.id)) || e.eq_ignore_ascii_case(&app.name)
    })
}

/// Adds `entry` to the list unless it is empty or already there (any case).
/// Returns whether the list changed.
pub fn add_ignored(list: &mut Vec<String>, entry: &str) -> bool {
    let entry = entry.trim().trim_matches('"');
    if entry.is_empty() || list.iter().any(|e| e.eq_ignore_ascii_case(entry)) {
        return false;
    }
    list.push(entry.to_string());
    true
}

pub fn remove_ignored(list: &mut Vec<String>, index: usize) -> bool {
    if index < list.len() {
        list.remove(index);
        true
    } else {
        false
    }
}

/// "12,4 Mo" in French, "12.4 MB" in English. Whole numbers below 10 KB.
pub fn format_size(bytes: u64, english: bool) -> String {
    const UNITS_EN: [&str; 4] = ["B", "KB", "MB", "GB"];
    const UNITS_FR: [&str; 4] = ["o", "Ko", "Mo", "Go"];
    let units = if english { UNITS_EN } else { UNITS_FR };
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < units.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    let text = if unit == 0 || value >= 100.0 { format!("{value:.0}") } else { format!("{value:.1}") };
    let text = if english { text } else { text.replace('.', ",") };
    format!("{text} {}", units[unit])
}

/// Keeps the start and the extension of a long file name: `big_arch….zip`.
pub fn shorten(name: &str, max: usize) -> String {
    let chars: Vec<char> = name.chars().collect();
    if chars.len() <= max || max < 6 {
        return name.to_string();
    }
    let ext_len = name.rfind('.').map(|i| name[i..].chars().count()).filter(|&n| n <= 6).unwrap_or(0);
    let tail: String = chars[chars.len() - ext_len..].iter().collect();
    let head: String = chars[..max - ext_len - 1].iter().collect();
    format!("{head}…{tail}")
}

/// Selects the file in a new Explorer window.
pub fn reveal(path: &Path) {
    use windows::Win32::UI::Shell::Common::ITEMIDLIST;
    use windows::Win32::UI::Shell::{ILFree, SHOpenFolderAndSelectItems, SHParseDisplayName};
    use windows::core::HSTRING;
    // SAFETY: the item list returned by the shell is freed once used.
    unsafe {
        let mut item: *mut ITEMIDLIST = std::ptr::null_mut();
        if SHParseDisplayName(&HSTRING::from(path.as_os_str()), None, &mut item, 0, None).is_ok() && !item.is_null() {
            let _ = SHOpenFolderAndSelectItems(item, None, 0);
            ILFree(Some(item));
        } else if let Some(dir) = path.parent() {
            let _ = std::process::Command::new("explorer.exe").arg(dir).spawn();
        }
    }
}

/// The closed island while a download runs or has just finished.
pub fn draw_download(ui: &mut Ui, line: &DownloadLine, island: Rect) {
    let icon_r = Rect::new(island.x + 12.0, island.y + (island.h - 16.0) * 0.5, 16.0, 16.0);
    let text = Rect::new(island.x + 34.0, island.y, island.w - 46.0, island.h);
    let english = crate::ui::is_english();
    match line {
        DownloadLine::Active { download, others } => {
            ui.icon(Icon::Download, icon_r, accent());
            let size = format_size(download.bytes, english);
            let size_w = size.chars().count() as f32 * 6.4 + 4.0;
            let mut left = text;
            let right = left.take_right(size_w, 6.0);
            let name = if download.name.is_empty() { tr("Téléchargement", "Downloading").to_string() } else { shorten(&download.name, 20) };
            let name = if *others > 0 { format!("{name} +{others}") } else { name };
            ui.text(left, &name, TextStyle::new(12.0, 500, TEXT));
            ui.text(right, &size, TextStyle::new(11.5, 500, TEXT_DIM).aligned(Align::End));
        }
        DownloadLine::Done { finished, until_ms } => {
            ui.redraw_at(*until_ms);
            ui.icon(Icon::Check, icon_r, accent());
            let mut left = text;
            let right = left.take_right(104.0, 6.0);
            ui.text(left, tr("Téléchargé", "Downloaded"), TextStyle::new(12.5, 500, TEXT));
            ui.text(right, &shorten(&finished.name, 16), TextStyle::new(11.0, 400, TEXT_DIM).aligned(Align::End));
        }
    }
}

/// Small coloured dots at the right end of the pill, stacked when several
/// devices are in use. The microphone dot is left out while the microphone
/// timer already fills the pill in green.
pub fn draw_dots(ui: &mut Ui, st: &ActivityState, island: Rect, mic_timer_shown: bool) {
    let kinds: Vec<CaptureKind> = [CaptureKind::Microphone, CaptureKind::Camera, CaptureKind::Screen]
        .into_iter()
        .filter(|&k| st.capture_active(k) && !(k == CaptureKind::Microphone && mic_timer_shown))
        .collect();
    if kinds.is_empty() {
        return;
    }
    let (r, gap) = if kinds.len() == 1 { (3.0, 0.0) } else { (2.5, 3.0) };
    let step = r * 2.0 + gap;
    let total = step * kinds.len() as f32 - gap;
    let cx = island.right() - 6.0;
    let mut y = island.center().y - total * 0.5;
    for k in kinds {
        ui.fill(Rect::new(cx - r, y, r * 2.0, r * 2.0), r, kind_color(k));
        y += step;
    }
}

/// The line the Quick tab shows under the controls, if there is something to
/// say. Names the applications using a device, or offers the last download.
pub fn quick_text(st: &ActivityState) -> Option<String> {
    let mut parts = Vec::new();
    for kind in [CaptureKind::Microphone, CaptureKind::Camera, CaptureKind::Screen] {
        let names: Vec<&str> = st.captures().filter(|c| c.kind == kind).map(|c| c.name.as_str()).collect();
        if !names.is_empty() {
            parts.push(format!("{} : {}", kind_label(kind), names.join(", ")));
        }
    }
    if parts.is_empty() { None } else { Some(shorten(&parts.join("  ·  "), 52)) }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cap(kind: CaptureKind, id: &str, name: &str) -> CaptureUse {
        CaptureUse { kind, id: id.into(), name: name.into() }
    }

    fn list(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn ignore_matches_exe_names_paths_and_aumids() {
        let zoom = cap(CaptureKind::Camera, "Zoom.exe", "Zoom");
        assert!(is_ignored(&zoom, &list(&["zoom.exe"])));
        assert!(is_ignored(&zoom, &list(&["C:\\Apps\\Zoom\\Zoom.exe"])));
        assert!(is_ignored(&zoom, &list(&["Zoom"])));
        assert!(!is_ignored(&zoom, &list(&["Teams.exe", ""])));
        let cam = cap(CaptureKind::Camera, "Microsoft.WindowsCamera_8wekyb3d8bbwe", "WindowsCamera");
        assert!(is_ignored(&cam, &list(&["Microsoft.WindowsCamera_8wekyb3d8bbwe!App"])));
        assert!(!is_ignored(&cam, &list(&["Microsoft.Other_8wekyb3d8bbwe!App"])));
    }

    #[test]
    fn ignored_apps_leave_the_indicators() {
        let mut st = ActivityState::default();
        st.apply_capture(vec![cap(CaptureKind::Camera, "Zoom.exe", "Zoom"), cap(CaptureKind::Screen, "Obs.exe", "Obs")]);
        assert!(st.capture_active(CaptureKind::Camera));
        st.ignore = list(&["zoom.exe"]);
        assert!(!st.capture_active(CaptureKind::Camera), "the list applies at once, without a new event");
        assert!(st.capture_active(CaptureKind::Screen));
    }

    #[test]
    fn the_ignore_list_has_no_duplicates() {
        let mut l = Vec::new();
        assert!(add_ignored(&mut l, "  Zoom.exe "));
        assert!(!add_ignored(&mut l, "zoom.EXE"));
        assert!(!add_ignored(&mut l, "   "));
        assert!(add_ignored(&mut l, "\"Teams.exe\""));
        assert_eq!(l, list(&["Zoom.exe", "Teams.exe"]));
        assert!(remove_ignored(&mut l, 0));
        assert!(!remove_ignored(&mut l, 5));
        assert_eq!(l, list(&["Teams.exe"]));
    }

    #[test]
    fn sizes_use_the_language_units() {
        assert_eq!(format_size(0, true), "0 B");
        assert_eq!(format_size(900, false), "900 o");
        assert_eq!(format_size(1536, true), "1.5 KB");
        assert_eq!(format_size(12_400_000, false), "11,8 Mo");
        assert_eq!(format_size(250 * 1024 * 1024, true), "250 MB");
        assert_eq!(format_size(3 * 1024 * 1024 * 1024, true), "3.0 GB");
    }

    #[test]
    fn long_names_keep_their_extension() {
        assert_eq!(shorten("a.zip", 20), "a.zip");
        assert_eq!(shorten("a_very_long_archive_name.zip", 12), "a_very_….zip");
        let s = shorten("a_very_long_archive_name.zip", 12);
        assert_eq!(s.chars().count(), 12);
        assert!(s.ends_with(".zip") && s.contains('…'));
        assert_eq!(shorten("noextensionatallinthisname", 10).chars().count(), 10);
    }

    #[test]
    fn a_download_shows_then_finishes_for_three_seconds() {
        let mut st = ActivityState::default();
        st.apply_download(DownloadEvent::Progress { id: 1, name: String::new(), bytes: 10 }, 0);
        assert!(matches!(st.line(0), Some(DownloadLine::Active { others: 0, .. })));
        st.apply_download(DownloadEvent::Progress { id: 1, name: "a.zip".into(), bytes: 99 }, 5);
        match st.line(5) {
            Some(DownloadLine::Active { download, .. }) => assert_eq!((download.name.as_str(), download.bytes), ("a.zip", 99)),
            other => panic!("{other:?}"),
        }
        st.apply_download(DownloadEvent::Done { id: 1, name: "a.zip".into(), path: "C:\\d\\a.zip".into(), bytes: 100 }, 1000);
        assert!(matches!(st.line(1000), Some(DownloadLine::Done { until_ms: 4000, .. })));
        assert_eq!(st.line(4000), None, "gone after 3 s");
        assert!(st.last_done(4000).is_some(), "still offered in the Quick tab");
        assert!(st.last_done(1000 + LAST_DONE_MS).is_none());
    }

    #[test]
    fn a_name_arriving_late_is_kept_and_cancel_clears() {
        let mut st = ActivityState::default();
        st.apply_download(DownloadEvent::Progress { id: 1, name: "a.zip".into(), bytes: 1 }, 0);
        st.apply_download(DownloadEvent::Progress { id: 1, name: String::new(), bytes: 2 }, 0);
        match st.line(0) {
            Some(DownloadLine::Active { download, .. }) => assert_eq!(download.name, "a.zip"),
            other => panic!("{other:?}"),
        }
        st.apply_download(DownloadEvent::Progress { id: 2, name: "b".into(), bytes: 1 }, 0);
        assert!(matches!(st.line(0), Some(DownloadLine::Active { others: 1, .. })));
        st.apply_download(DownloadEvent::Cancelled { id: 2 }, 0);
        st.apply_download(DownloadEvent::Cancelled { id: 1 }, 0);
        assert_eq!(st.line(0), None);
    }

    #[test]
    fn the_quick_line_names_the_apps() {
        let mut st = ActivityState::default();
        assert_eq!(quick_text(&st), None);
        st.apply_capture(vec![cap(CaptureKind::Camera, "Zoom.exe", "Zoom"), cap(CaptureKind::Camera, "Teams.exe", "Teams")]);
        let t = quick_text(&st).unwrap();
        assert!(t.contains("Zoom, Teams"), "{t}");
    }
}

#[cfg(test)]
mod mic_ignore_tests {
    use super::*;

    fn mic(id: &str) -> CaptureUse {
        CaptureUse { kind: CaptureKind::Microphone, id: id.into(), name: id.trim_end_matches(".exe").into() }
    }

    #[test]
    fn the_mic_timer_is_hidden_only_when_every_user_is_ignored() {
        let mut st = ActivityState { ignore: vec!["Discord".into()], ..Default::default() };
        assert!(!st.mic_users_all_ignored(), "nobody named yet");
        st.apply_capture(vec![mic("Discord.exe")]);
        assert!(st.mic_users_all_ignored());
        st.apply_capture(vec![mic("Discord.exe"), mic("Teams.exe")]);
        assert!(!st.mic_users_all_ignored(), "another app is also recording");
    }
}
