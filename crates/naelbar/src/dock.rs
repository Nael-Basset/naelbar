//! Dock data: Start menu and Store apps, pinned set, icons and Brave bookmarks.
//!
//! Nothing here runs until the dock tab or its settings page opens. Scanning
//! happens on a short-lived thread, and the result is cached in memory.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use windows::Win32::Foundation::SIZE;
use windows::Win32::Graphics::Gdi::{DeleteObject, HGDIOBJ};
use windows::Win32::Graphics::Imaging::*;
use windows::Win32::System::Com::StructuredStorage::CreateStreamOnHGlobal;
use windows::Win32::System::Com::*;
use windows::Win32::UI::Shell::*;
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
use windows::core::{HSTRING, PCWSTR, w};

/// Names pinned the first time, matched against the apps found.
pub const DEFAULT_PINS: [&str; 7] = ["Brave", "YouTube Music", "Claude", "Discord", "Steam", "Bitwarden", "SteelSeries GG"];

/// Pins and bookmark source, stored in `%APPDATA%\naelbar\dock.json`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Store {
    pub pinned: Vec<String>,
    /// `brave` or `none`.
    pub bookmarks: String,
}

impl Default for Store {
    fn default() -> Self {
        Self { pinned: DEFAULT_PINS.iter().map(|s| s.to_string()).collect(), bookmarks: "brave".into() }
    }
}

impl Store {
    pub fn path() -> Option<PathBuf> {
        std::env::var_os("APPDATA").map(|d| PathBuf::from(d).join("naelbar").join("dock.json"))
    }

    pub fn load() -> Self {
        Self::path().map(|p| Self::load_from(&p)).unwrap_or_default()
    }

    pub fn load_from(p: &Path) -> Self {
        match std::fs::read(p) {
            Ok(b) => serde_json::from_slice::<Self>(&b).map(Self::sanitized).unwrap_or_default(),
            Err(_) => Self::default(),
        }
    }

    /// The file can be edited by hand, so no value is trusted.
    fn sanitized(mut self) -> Self {
        let mut seen = Vec::new();
        self.pinned.retain(|n| {
            let f = fold(n);
            let ok = !f.is_empty() && n.len() <= 200 && !seen.contains(&f);
            if ok {
                seen.push(f);
            }
            ok
        });
        self.pinned.truncate(60);
        if self.bookmarks != "none" {
            self.bookmarks = "brave".into();
        }
        self
    }

    pub fn save(&self) {
        if let Some(p) = Self::path() {
            self.save_to(&p);
        }
    }

    /// Replaced in one step, so a reader never sees half a file.
    pub fn save_to(&self, p: &Path) {
        if let Some(dir) = p.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Ok(json) = serde_json::to_vec_pretty(self) {
            let tmp = p.with_extension(format!("{}.tmp", std::process::id()));
            if std::fs::write(&tmp, json).is_ok() && std::fs::rename(&tmp, p).is_err() {
                let _ = std::fs::remove_file(&tmp);
            }
        }
    }

    /// Replaces each pin by the exact name of the app it matches and drops
    /// the ones that match nothing.
    pub fn normalize(&mut self, names: &[&str]) {
        if names.is_empty() {
            return;
        }
        let mut out: Vec<String> = Vec::new();
        for p in &self.pinned {
            if let Some(i) = resolve(p, names) {
                if !out.iter().any(|o| o == names[i]) {
                    out.push(names[i].to_string());
                }
            }
        }
        self.pinned = out;
    }

    pub fn is_pinned(&self, names: &[&str], index: usize) -> bool {
        self.pinned.iter().any(|p| resolve(p, names) == Some(index))
    }

    /// Pins or unpins the app at `index`. A new pin goes to the end.
    pub fn toggle(&mut self, names: &[&str], index: usize) {
        if self.is_pinned(names, index) {
            self.pinned.retain(|p| resolve(p, names) != Some(index));
        } else if let Some(n) = names.get(index) {
            self.pinned.push(n.to_string());
        }
    }
}

/// Lowercases and removes French accents.
pub fn fold(s: &str) -> String {
    s.trim()
        .chars()
        .flat_map(char::to_lowercase)
        .map(|c| match c {
            'à' | 'â' | 'ä' => 'a',
            'é' | 'è' | 'ê' | 'ë' => 'e',
            'î' | 'ï' => 'i',
            'ô' | 'ö' => 'o',
            'ù' | 'û' | 'ü' => 'u',
            'ç' => 'c',
            c => c,
        })
        .collect()
}

/// Finds the app a pinned name refers to: exact name, then names starting
/// with it, then names containing it. The shortest wins within a rank.
pub fn resolve(pin: &str, names: &[&str]) -> Option<usize> {
    let p = fold(pin);
    if p.is_empty() {
        return None;
    }
    let folded: Vec<String> = names.iter().map(|n| fold(n)).collect();
    if let Some(i) = folded.iter().position(|n| *n == p) {
        return Some(i);
    }
    let best = |f: &dyn Fn(&str) -> bool| folded.iter().enumerate().filter(|(_, n)| f(n)).min_by_key(|(_, n)| n.len()).map(|(i, _)| i);
    best(&|n| n.starts_with(&p)).or_else(|| best(&|n| n.contains(&p)))
}

/// First letter in capitals, for placeholders and chips.
pub fn monogram(name: &str) -> String {
    name.chars().find(|c| c.is_alphanumeric()).map(|c| c.to_uppercase().collect()).unwrap_or_else(|| "?".into())
}

#[derive(Debug, Clone, PartialEq)]
pub struct Bookmark {
    pub name: String,
    pub url: String,
}

/// Direct children of the bookmark bar that are links. Folders are skipped.
pub fn parse_bookmarks(json: &str) -> Vec<Bookmark> {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(json) else { return Vec::new() };
    let Some(children) = v.pointer("/roots/bookmark_bar/children").and_then(|c| c.as_array()) else { return Vec::new() };
    children
        .iter()
        .filter(|c| c.get("type").and_then(|t| t.as_str()) == Some("url"))
        .filter_map(|c| {
            let url = c.get("url")?.as_str()?.trim();
            if !(url.starts_with("https://") || url.starts_with("http://")) {
                return None;
            }
            let name = c.get("name").and_then(|n| n.as_str()).map(str::trim).filter(|n| !n.is_empty()).unwrap_or(url);
            Some(Bookmark { name: name.to_string(), url: url.to_string() })
        })
        .collect()
}

#[derive(Debug, Clone, PartialEq)]
pub enum Launch {
    /// A `.lnk` or `.url` file.
    Path(String),
    /// `shell:AppsFolder\<name>`.
    Shell(String),
}

#[derive(Debug, Clone)]
pub struct AppEntry {
    pub name: String,
    pub launch: Launch,
    /// Shell parsing name used to fetch the icon.
    pub icon_src: String,
}

fn skipped(name: &str) -> bool {
    let f = fold(name);
    ["uninstall", "desinstall", "readme", "release notes", "documentation", "manual"].iter().any(|k| f.contains(k))
}

/// Merges Start menu shortcuts with the Apps folder. A shortcut wins over an
/// Apps folder entry of the same name, but borrows its cleaner icon.
pub fn merge(shortcuts: Vec<(String, String)>, shell: Vec<(String, String)>) -> Vec<AppEntry> {
    let mut out: Vec<AppEntry> = Vec::new();
    let mut index: HashMap<String, usize> = HashMap::new();
    for (name, path) in shortcuts {
        if skipped(&name) || index.contains_key(&fold(&name)) {
            continue;
        }
        index.insert(fold(&name), out.len());
        out.push(AppEntry { name, launch: Launch::Path(path.clone()), icon_src: path });
    }
    for (name, parsing) in shell {
        if skipped(&name) {
            continue;
        }
        let src = format!("shell:AppsFolder\\{parsing}");
        match index.get(&fold(&name)) {
            Some(&i) => out[i].icon_src = src,
            None => {
                index.insert(fold(&name), out.len());
                out.push(AppEntry { name, launch: Launch::Shell(src.clone()), icon_src: src });
            }
        }
    }
    out.sort_by_key(|a| fold(&a.name));
    out
}

fn walk(dir: &Path, depth: u32, out: &mut Vec<(String, String)>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            if depth < 4 {
                walk(&p, depth + 1, out);
            }
        } else if let Some(ext) = p.extension().and_then(|x| x.to_str()) {
            if (ext.eq_ignore_ascii_case("lnk") || ext.eq_ignore_ascii_case("url")) && let Some(stem) = p.file_stem().and_then(|s| s.to_str()) {
                out.push((stem.to_string(), p.to_string_lossy().into_owned()));
            }
        }
    }
}

fn scan_start_menu() -> Vec<(String, String)> {
    let mut out = Vec::new();
    for var in ["ProgramData", "APPDATA"] {
        if let Some(base) = std::env::var_os(var) {
            walk(&PathBuf::from(base).join("Microsoft\\Windows\\Start Menu\\Programs"), 0, &mut out);
        }
    }
    out
}

unsafe fn take_string(p: windows::core::PWSTR) -> String {
    let s = unsafe { p.to_string() }.unwrap_or_default();
    unsafe { CoTaskMemFree(Some(p.0 as *const _)) };
    s
}

/// Names and parsing names of everything in `shell:AppsFolder`.
fn scan_apps_folder() -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut run = || -> windows::core::Result<()> {
        unsafe {
            let folder: IShellItem = SHGetKnownFolderItem(&FOLDERID_AppsFolder, KF_FLAG_DEFAULT, None)?;
            let en: IEnumShellItems = folder.BindToHandler(None, &BHID_EnumItems)?;
            loop {
                let mut items: [Option<IShellItem>; 1] = [None];
                let mut got = 0u32;
                let _ = en.Next(&mut items, Some(&mut got));
                let Some(item) = items[0].take() else { break };
                if got == 0 {
                    break;
                }
                let (Ok(name), Ok(parsing)) = (item.GetDisplayName(SIGDN_NORMALDISPLAY), item.GetDisplayName(SIGDN_PARENTRELATIVEPARSING)) else { continue };
                let (name, parsing) = (take_string(name), take_string(parsing));
                if !name.is_empty() && !parsing.is_empty() {
                    out.push((name, parsing));
                }
            }
        }
        Ok(())
    };
    let _ = run();
    out
}

/// Blocking scan of every launchable app. Call from a thread with COM ready.
pub fn scan_apps() -> Vec<AppEntry> {
    merge(scan_start_menu(), scan_apps_folder())
}

/// Runs `f` on a thread with COM initialised, then uninitialises it.
pub fn with_com<R>(f: impl FnOnce() -> R) -> R {
    let ok = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) }.is_ok();
    let r = f();
    if ok {
        unsafe { CoUninitialize() };
    }
    r
}

/// The icon of `parsing` (a path or `shell:AppsFolder\...`) as PNG bytes.
pub fn icon_png(parsing: &str) -> Option<Vec<u8>> {
    unsafe {
        let item: IShellItemImageFactory = SHCreateItemFromParsingName(&HSTRING::from(parsing), None).ok()?;
        let hbm = item.GetImage(SIZE { cx: 64, cy: 64 }, SIIGBF_ICONONLY | SIIGBF_BIGGERSIZEOK).ok()?;
        let png = encode_png(hbm);
        let _ = DeleteObject(HGDIOBJ(hbm.0));
        png.ok()
    }
}

unsafe fn encode_png(hbm: windows::Win32::Graphics::Gdi::HBITMAP) -> windows::core::Result<Vec<u8>> {
    unsafe {
        let f: IWICImagingFactory = CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER)?;
        let bmp = f.CreateBitmapFromHBITMAP(hbm, Default::default(), WICBitmapUseAlpha)?;
        let conv = f.CreateFormatConverter()?;
        conv.Initialize(&bmp, &GUID_WICPixelFormat32bppBGRA, WICBitmapDitherTypeNone, None, 0.0, WICBitmapPaletteTypeCustom)?;
        let (mut w, mut h) = (0, 0);
        conv.GetSize(&mut w, &mut h)?;
        let mem = CreateStreamOnHGlobal(Default::default(), true)?;
        let enc = f.CreateEncoder(&GUID_ContainerFormatPng, std::ptr::null())?;
        enc.Initialize(&mem, WICBitmapEncoderNoCache)?;
        let mut frame = None;
        enc.CreateNewFrame(&mut frame, std::ptr::null_mut())?;
        let frame = frame.ok_or_else(windows::core::Error::empty)?;
        frame.Initialize(None)?;
        frame.SetSize(w, h)?;
        let mut fmt = GUID_WICPixelFormat32bppBGRA;
        frame.SetPixelFormat(&mut fmt)?;
        frame.WriteSource(&conv, std::ptr::null())?;
        frame.Commit()?;
        enc.Commit()?;
        let mut stat = STATSTG::default();
        mem.Stat(&mut stat, STATFLAG_NONAME)?;
        let len = stat.cbSize as usize;
        let mut buf = vec![0u8; len];
        mem.Seek(0, STREAM_SEEK_SET, None)?;
        let mut read = 0u32;
        mem.Read(buf.as_mut_ptr().cast(), len as u32, Some(&mut read)).ok()?;
        buf.truncate(read as usize);
        Ok(buf)
    }
}

fn shell_open(file: &str, params: Option<&str>) {
    unsafe {
        let params = params.map(HSTRING::from);
        let p = params.as_ref().map_or(PCWSTR::null(), |h| PCWSTR(h.as_ptr()));
        let _ = ShellExecuteW(None, w!("open"), &HSTRING::from(file), p, PCWSTR::null(), SW_SHOWNORMAL);
    }
}

pub fn launch(l: &Launch) {
    let target = match l {
        Launch::Path(p) | Launch::Shell(p) => p.clone(),
    };
    // Off the UI thread: the shell may take a moment to start the app.
    std::thread::spawn(move || shell_open(&target, None));
}

fn brave_exe() -> Option<PathBuf> {
    let tail = "BraveSoftware\\Brave-Browser\\Application\\brave.exe";
    ["ProgramFiles", "ProgramFiles(x86)", "LOCALAPPDATA"].iter().filter_map(|v| std::env::var_os(v)).map(|b| PathBuf::from(b).join(tail)).find(|p| p.exists())
}

/// Opens a web link in Brave when asked and installed, else in the default browser.
pub fn open_url(url: &str, prefer_brave: bool) {
    if !(url.starts_with("https://") || url.starts_with("http://")) {
        return;
    }
    let url = url.to_string();
    std::thread::spawn(move || match prefer_brave.then(brave_exe).flatten() {
        Some(exe) => shell_open(&exe.to_string_lossy(), Some(&format!("\"{}\"", url.replace('"', "%22")))),
        None => shell_open(&url, None),
    });
}

fn brave_bookmarks() -> Vec<Bookmark> {
    let Some(base) = std::env::var_os("LOCALAPPDATA") else { return Vec::new() };
    let p = PathBuf::from(base).join("BraveSoftware\\Brave-Browser\\User Data\\Default\\Bookmarks");
    std::fs::read_to_string(p).map(|s| parse_bookmarks(&s)).unwrap_or_default()
}

// ---------------------------------------------------------------- cache

#[derive(Clone)]
pub struct Tile {
    pub name: String,
    pub launch: Launch,
    pub icon: Option<Arc<[u8]>>,
}

#[derive(Clone, Default)]
pub struct Snapshot {
    pub tiles: Vec<Tile>,
    pub bookmarks: Vec<Bookmark>,
    pub brave: bool,
    pub loading: bool,
}

#[derive(Default)]
struct Cache {
    apps: Option<Arc<Vec<AppEntry>>>,
    apps_at: Option<Instant>,
    icons: HashMap<String, Arc<[u8]>>,
    snap: Snapshot,
}

static CACHE: Mutex<Option<Cache>> = Mutex::new(None);
static LOADING: AtomicBool = AtomicBool::new(false);
/// Posted to the island control window when [`refresh`] has finished.
pub const WM_DOCK_READY: u32 = windows::Win32::UI::WindowsAndMessaging::WM_APP + 62;
const APPS_TTL: Duration = Duration::from_secs(600);

fn with_cache<R>(f: impl FnOnce(&mut Cache) -> R) -> R {
    let mut g = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    f(g.get_or_insert_with(Cache::default))
}

pub fn snapshot() -> Snapshot {
    let mut s = with_cache(|c| c.snap.clone());
    s.loading = LOADING.load(Ordering::Acquire);
    s
}

/// Reads pins, bookmarks and (when stale) the app list on a short-lived thread.
pub fn refresh() {
    if LOADING.swap(true, Ordering::AcqRel) {
        return;
    }
    std::thread::spawn(|| {
        with_com(load);
        LOADING.store(false, Ordering::Release);
        // The island repaints on this message; nothing polls while loading.
        crate::widgets::notify_island(WM_DOCK_READY);
    });
}

fn load() {
    let store = Store::load();
    let stale = with_cache(|c| c.apps.is_none() || c.apps_at.is_none_or(|t| t.elapsed() > APPS_TTL));
    if stale {
        let apps = Arc::new(scan_apps());
        with_cache(|c| {
            c.apps = Some(apps);
            c.apps_at = Some(Instant::now());
        });
    }
    let Some(apps) = with_cache(|c| c.apps.clone()) else { return };
    let names: Vec<&str> = apps.iter().map(|a| a.name.as_str()).collect();
    let mut picked: Vec<usize> = Vec::new();
    for p in &store.pinned {
        if let Some(i) = resolve(p, &names) {
            if !picked.contains(&i) {
                picked.push(i);
            }
        }
    }
    let bookmarks = if store.bookmarks == "brave" { brave_bookmarks() } else { Vec::new() };
    let brave = store.bookmarks == "brave";
    let make = |c: &Cache| -> Vec<Tile> {
        picked.iter().map(|&i| Tile { name: apps[i].name.clone(), launch: apps[i].launch.clone(), icon: c.icons.get(&apps[i].icon_src).cloned() }).collect()
    };
    with_cache(|c| {
        c.snap.tiles = make(c);
        c.snap.bookmarks = bookmarks;
        c.snap.brave = brave;
    });
    let missing: Vec<String> = with_cache(|c| picked.iter().map(|&i| apps[i].icon_src.clone()).filter(|s| !c.icons.contains_key(s)).collect());
    if missing.is_empty() {
        return;
    }
    for src in missing {
        if let Some(png) = icon_png(&src) {
            let arc: Arc<[u8]> = Arc::from(png);
            with_cache(|c| {
                c.icons.insert(src, arc);
                c.snap.tiles = make(c);
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NAMES: [&str; 6] = ["Brave", "Claude Code", "Discord", "SteelSeries GG", "Steam", "Éditeur de texte"];

    #[test]
    fn names_match_exactly_then_by_prefix() {
        assert_eq!(resolve("brave", &NAMES), Some(0));
        assert_eq!(resolve("Steam", &NAMES), Some(4), "exact beats prefix of SteelSeries");
        assert_eq!(resolve("steels", &NAMES), Some(3));
        assert_eq!(resolve("claude", &NAMES), Some(1));
        assert_eq!(resolve("editeur", &NAMES), Some(5), "accents are folded");
        assert_eq!(resolve("texte", &NAMES), Some(5), "contains");
        assert_eq!(resolve("bitwarden", &NAMES), None);
        assert_eq!(resolve("  ", &NAMES), None);
    }

    #[test]
    fn bookmark_bar_only_links() {
        let json = r#"{"roots":{
          "bookmark_bar":{"children":[
            {"type":"url","name":"GitHub","url":"https://github.com/"},
            {"type":"folder","name":"Work","children":[{"type":"url","name":"Hidden","url":"https://x.test/"}]},
            {"type":"url","name":"","url":"http://example.com"},
            {"type":"url","name":"Bad","url":"javascript:alert(1)"}
          ]},
          "other":{"children":[{"type":"url","name":"Other","url":"https://o.test/"}]}}}"#;
        let b = parse_bookmarks(json);
        assert_eq!(b, vec![Bookmark { name: "GitHub".into(), url: "https://github.com/".into() }, Bookmark { name: "http://example.com".into(), url: "http://example.com".into() }]);
        assert!(parse_bookmarks("not json").is_empty());
        assert_eq!(monogram("  git"), "G");
        assert_eq!(monogram("école"), "É");
    }

    #[test]
    fn pins_toggle_normalize_and_persist() {
        let mut s = Store::default();
        s.normalize(&NAMES);
        assert_eq!(s.pinned, ["Brave", "Claude Code", "Discord", "Steam", "SteelSeries GG"]);
        s.toggle(&NAMES, 5);
        assert!(s.is_pinned(&NAMES, 5));
        s.toggle(&NAMES, 0);
        assert!(!s.is_pinned(&NAMES, 0));
        assert_eq!(s.pinned.last().map(String::as_str), Some("Éditeur de texte"), "new pins go last");

        let dir = std::env::temp_dir().join(format!("nb-dock-test-{}", std::process::id()));
        let file = dir.join("dock.json");
        s.bookmarks = "none".into();
        s.save_to(&file);
        assert_eq!(Store::load_from(&file), s);
        std::fs::write(&file, r#"{"pinned":["A","a","",  "B"],"bookmarks":"weird"}"#).unwrap();
        let loaded = Store::load_from(&file);
        assert_eq!(loaded.pinned, vec!["A".to_string(), "B".to_string()]);
        assert_eq!(loaded.bookmarks, "brave");
        assert_eq!(Store::load_from(&dir.join("missing.json")), Store::default());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn shortcuts_win_and_skip_uninstallers() {
        let apps = merge(
            vec![("Discord".into(), "C:\\a\\Discord.lnk".into()), ("Uninstall Foo".into(), "C:\\a\\u.lnk".into())],
            vec![("discord".into(), "id1".into()), ("Calculatrice".into(), "calc!App".into())],
        );
        assert_eq!(apps.len(), 2);
        let d = apps.iter().find(|a| a.name == "Discord").unwrap();
        assert_eq!(d.launch, Launch::Path("C:\\a\\Discord.lnk".into()));
        assert_eq!(d.icon_src, "shell:AppsFolder\\id1");
        let c = apps.iter().find(|a| a.name == "Calculatrice").unwrap();
        assert_eq!(c.launch, Launch::Shell("shell:AppsFolder\\calc!App".into()));
    }
}
