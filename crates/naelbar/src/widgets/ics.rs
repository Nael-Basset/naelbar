//! Read-only agenda from an .ics URL or file. Downloaded in a short-lived
//! thread, only while something that shows it is on screen, at most once per
//! [`MIN_AGE`]. The address is never logged: some calendars put a secret in it.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use super::civil::{self, DAY_MS, MIN_MS};

/// Least time between two downloads.
pub const MIN_AGE: Duration = Duration::from_secs(30 * 60);
const MAX_BYTES: u64 = 4_000_000;
const MAX_EVENTS: usize = 6000;
/// Occurrences kept for one repeating event.
const MAX_REPEATS: usize = 366;

#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct IcsEvent {
    /// `YYYY-MM-DD`.
    pub date: String,
    /// `HH:MM`, none for an all-day event.
    pub time: Option<String>,
    /// `HH:MM` when the event ends the same day.
    pub end: Option<String>,
    pub title: String,
    /// Room or place, shortened.
    pub place: String,
    /// Kind of lesson from a university timetable: CM, TD, TP, DS or RN.
    pub kind: Option<String>,
}

impl IcsEvent {
    /// "RN" slots are project hours the student places freely, not lessons.
    pub fn is_project(&self) -> bool {
        self.kind.as_deref() == Some("RN")
    }

    /// Title with its kind, as lists show it: "Anglais 5 · TP".
    pub fn label(&self) -> String {
        match &self.kind {
            Some(_) if self.is_project() => format!("{} · {}", self.title, crate::ui::tr("heures projet", "project hours")),
            Some(k) => format!("{} · {k}", self.title),
            None => self.title.clone(),
        }
    }
}

/// Kinds of lesson that university timetables (ADE) put near the end of a title.
const KINDS: [&str; 6] = ["CM", "TD", "TP", "DS", "RN", "CC"];

/// Splits an ADE title such as "R501 Anglais 5 TP CD" into "Anglais 5" and
/// "TP". The course code comes first, the groups last. Other titles are kept.
pub fn ade_title(summary: &str) -> (String, Option<String>) {
    let words: Vec<&str> = summary.split_whitespace().collect();
    let Some(k) = words.iter().rposition(|w| KINDS.contains(w)) else { return (summary.trim().to_string(), None) };
    // After the kind only group letters may follow: "CD", "ABCD", "C".
    let groups_only = words[k + 1..].iter().all(|w| w.len() <= 6 && w.chars().all(|c| c.is_ascii_uppercase()));
    if !groups_only || k == 0 {
        return (summary.trim().to_string(), None);
    }
    let code_like = |w: &str| w.chars().any(|c| c.is_ascii_digit()) || w.starts_with("SAE");
    let start = usize::from(code_like(words[0]) && k > 1);
    (words[start..k].join(" "), Some(words[k].to_string()))
}

/// "BL CH226 TP physique électricité" becomes "BL CH226".
fn short_place(location: &str) -> String {
    let words: Vec<&str> = location.split_whitespace().collect();
    let room = words.iter().position(|w| w.chars().any(|c| c.is_ascii_digit()));
    match room {
        Some(i) if i < 3 => words[..=i].join(" "),
        _ => location.trim().chars().take(24).collect(),
    }
}

/// Unfolds continuation lines and removes the line ends.
fn unfold(text: &str) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    for raw in text.split('\n') {
        let raw = raw.strip_suffix('\r').unwrap_or(raw);
        match (raw.strip_prefix([' ', '\t']), lines.last_mut()) {
            (Some(rest), Some(last)) => last.push_str(rest),
            _ => lines.push(raw.to_string()),
        }
    }
    lines
}

/// Upper-case property name and value; the parameters are returned apart.
fn split_property(line: &str) -> Option<(String, String, &str)> {
    let mut quoted = false;
    let colon = line.char_indices().find(|&(_, c)| {
        if c == '"' {
            quoted = !quoted;
        }
        c == ':' && !quoted
    })?;
    let (head, value) = (&line[..colon.0], &line[colon.0 + 1..]);
    let (name, params) = head.split_once(';').unwrap_or((head, ""));
    Some((name.to_ascii_uppercase(), params.to_string(), value))
}

fn unescape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut chars = value.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n' | 'N') => out.push(' '),
            Some(other) => out.push(other),
            None => {}
        }
    }
    out
}

/// A `DTSTART` or `UNTIL` value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Stamp {
    date: (i32, u32, u32),
    time: Option<(u32, u32, u32)>,
    utc: bool,
}

fn parse_stamp(value: &str) -> Option<Stamp> {
    let value = value.trim();
    let (day, rest) = match value.split_once('T') {
        Some((d, t)) => (d, Some(t)),
        None => (value, None),
    };
    if day.len() != 8 || !day.bytes().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let (y, m, d): (i32, u32, u32) = (day[..4].parse().ok()?, day[4..6].parse().ok()?, day[6..8].parse().ok()?);
    if !(1..=12).contains(&m) || d == 0 || d > crate::agenda::days_in_month(y, m) {
        return None;
    }
    let Some(rest) = rest else { return Some(Stamp { date: (y, m, d), time: None, utc: false }) };
    let (clock, utc) = match rest.strip_suffix(['Z', 'z']) {
        Some(c) => (c, true),
        None => (rest, false),
    };
    if clock.len() != 6 || !clock.bytes().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let (h, mi, s): (u32, u32, u32) = (clock[..2].parse().ok()?, clock[2..4].parse().ok()?, clock[4..6].parse().ok()?);
    (h < 24 && mi < 60 && s < 61).then_some(Stamp { date: (y, m, d), time: Some((h, mi, s)), utc })
}

/// Local naive milliseconds. A time in UTC goes through `to_local`; one with
/// a TZID or none is taken as local time, which is exact in the user's zone.
fn local_ms(s: &Stamp, to_local: &dyn Fn(i64) -> i64) -> i64 {
    let (h, mi, sec) = s.time.unwrap_or((0, 0, 0));
    let ms = civil::to_ms(s.date.0, s.date.1, s.date.2, h, mi, sec);
    if s.utc { to_local(ms) } else { ms }
}

#[derive(Default)]
struct Rule {
    /// A repeat the parser understands: weekly with a count or an end.
    weekly: bool,
    interval: i64,
    count: Option<usize>,
    until: Option<Stamp>,
}

fn parse_rule(value: &str) -> Rule {
    let mut rule = Rule { interval: 1, ..Rule::default() };
    let mut weekly = false;
    for part in value.split(';') {
        let Some((key, val)) = part.split_once('=') else { continue };
        match key.to_ascii_uppercase().as_str() {
            "FREQ" => weekly = val.eq_ignore_ascii_case("WEEKLY"),
            "COUNT" => rule.count = val.parse().ok(),
            "UNTIL" => rule.until = parse_stamp(val),
            "INTERVAL" => rule.interval = val.parse().ok().filter(|n| (1..=52).contains(n)).unwrap_or(1),
            "WKST" => {}
            // BYDAY and the others change which days repeat: not simple.
            _ => return Rule::default(),
        }
    }
    rule.weekly = weekly && (rule.count.is_some() || rule.until.is_some());
    rule
}

fn push_event(out: &mut Vec<IcsEvent>, ms: i64, all_day: bool, length: Option<i64>, model: &IcsEvent) {
    let ((y, m, d), (h, mi)) = civil::from_ms(ms);
    let end = length.filter(|_| !all_day).map(|l| civil::from_ms(ms + l)).filter(|(day, _)| *day == (y, m, d)).map(|(_, (eh, em))| format!("{eh:02}:{em:02}"));
    out.push(IcsEvent { date: crate::agenda::date_key(y, m, d), time: (!all_day).then(|| format!("{h:02}:{mi:02}")), end, ..model.clone() });
}

#[derive(Default)]
struct Draft {
    summary: Option<String>,
    location: Option<String>,
    start: Option<Stamp>,
    end: Option<Stamp>,
    rule: Option<Rule>,
    cancelled: bool,
}

fn finish(draft: Draft, to_local: &dyn Fn(i64) -> i64, out: &mut Vec<IcsEvent>) {
    let (Some(summary), Some(start)) = (draft.summary.filter(|t| !t.trim().is_empty()), draft.start) else { return };
    if draft.cancelled {
        return;
    }
    let (title, kind) = ade_title(&summary);
    let model = IcsEvent { title, kind, place: draft.location.as_deref().map(short_place).unwrap_or_default(), ..IcsEvent::default() };
    let first = local_ms(&start, to_local);
    let all_day = start.time.is_none();
    let length = draft.end.map(|e| local_ms(&e, to_local) - first).filter(|l| (1..DAY_MS).contains(l));
    let rule = draft.rule.unwrap_or_default();
    if !rule.weekly {
        push_event(out, first, all_day, length, &model);
        return;
    }
    let until = rule.until.map(|u| if u.time.is_none() { local_ms(&u, to_local) + DAY_MS - 1 } else { local_ms(&u, to_local) });
    for k in 0..rule.count.unwrap_or(MAX_REPEATS).min(MAX_REPEATS) {
        let at = first + k as i64 * rule.interval * 7 * DAY_MS;
        if until.is_some_and(|u| at > u) {
            break;
        }
        push_event(out, at, all_day, length, &model);
    }
}

/// Events of an .ics text, sorted by day then time. `utc_offset_min` is the
/// local offset from UTC, applied to times written in UTC.
#[cfg(test)]
pub fn parse(text: &str, utc_offset_min: i64) -> Vec<IcsEvent> {
    parse_with(text, &|ms| ms + utc_offset_min * MIN_MS)
}

/// Same, with UTC times moved to local time by `to_local`, date by date.
pub fn parse_with(text: &str, to_local: &dyn Fn(i64) -> i64) -> Vec<IcsEvent> {
    let mut out = Vec::new();
    let mut draft: Option<Draft> = None;
    for line in unfold(text) {
        let Some((name, _params, value)) = split_property(&line) else { continue };
        match (name.as_str(), draft.as_mut()) {
            ("BEGIN", _) if value.eq_ignore_ascii_case("VEVENT") => draft = Some(Draft::default()),
            ("END", Some(_)) if value.eq_ignore_ascii_case("VEVENT") => {
                if let Some(d) = draft.take() {
                    finish(d, to_local, &mut out);
                }
                if out.len() >= MAX_EVENTS {
                    break;
                }
            }
            ("SUMMARY", Some(d)) => d.summary = Some(unescape(value)),
            ("DTSTART", Some(d)) => d.start = parse_stamp(value),
            ("DTEND", Some(d)) => d.end = parse_stamp(value),
            ("LOCATION", Some(d)) => d.location = Some(unescape(value)),
            ("RRULE", Some(d)) => d.rule = Some(parse_rule(value)),
            ("STATUS", Some(d)) => d.cancelled = value.trim().eq_ignore_ascii_case("CANCELLED"),
            _ => {}
        }
    }
    out.sort_by(|a, b| a.date.cmp(&b.date).then_with(|| a.time.cmp(&b.time)).then_with(|| a.title.cmp(&b.title)));
    out.truncate(MAX_EVENTS);
    out
}

/// A UTC time in the user's zone, with the summer time rule of that date.
fn to_local_time(utc_ms: i64) -> i64 {
    use windows::Win32::Foundation::SYSTEMTIME;
    use windows::Win32::System::Time::SystemTimeToTzSpecificLocalTime;
    let ((y, m, d), (h, mi)) = civil::from_ms(utc_ms);
    let utc = SYSTEMTIME { wYear: y as u16, wMonth: m as u16, wDay: d as u16, wHour: h as u16, wMinute: mi as u16, ..Default::default() };
    let mut local = SYSTEMTIME::default();
    if unsafe { SystemTimeToTzSpecificLocalTime(None, &utc, &mut local) }.is_err() {
        return utc_ms;
    }
    civil::to_ms(local.wYear as i32, local.wMonth as u32, local.wDay as u32, local.wHour as u32, local.wMinute as u32, 0)
}

fn read_limited(path: &std::path::Path) -> Option<String> {
    if std::fs::metadata(path).ok()?.len() > MAX_BYTES {
        return None;
    }
    Some(String::from_utf8_lossy(&std::fs::read(path).ok()?).into_owned())
}

/// Downloads with the `curl.exe` that ships with Windows: no extra dependency,
/// and the process ends with the transfer.
fn download(url: &str) -> Option<String> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let tmp = std::env::temp_dir().join(format!("naelbar-ics-{}.tmp", std::process::id()));
    let status = std::process::Command::new("curl.exe")
        .args(["-sSL", "--max-time", "20", "--max-filesize", "4000000", "--proto", "=http,https", "--proto-redir", "=http,https", "-o"])
        .arg(&tmp)
        .arg("--")
        .arg(url)
        .creation_flags(CREATE_NO_WINDOW)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .ok()?;
    let text = if status.success() { read_limited(&tmp) } else { None };
    let _ = std::fs::remove_file(&tmp);
    text
}

/// Where the text comes from: a web address or a file.
fn fetch(source: &str) -> Option<String> {
    let lower = source.to_ascii_lowercase();
    let text = if lower.starts_with("https://") || lower.starts_with("http://") {
        download(source)?
    } else if lower.starts_with("webcal://") {
        download(&format!("https://{}", &source[9..]))?
    } else {
        let path = source.strip_prefix("file:///").unwrap_or(source).trim_matches('"');
        read_limited(std::path::Path::new(path))?
    };
    text.contains("BEGIN:VCALENDAR").then_some(text)
}

struct State {
    source: String,
    events: Vec<IcsEvent>,
    fetched: Option<Instant>,
    running: bool,
    /// Bumped when the source changes, so a late answer for the old one is dropped.
    generation: u64,
}

static STATE: Mutex<State> = Mutex::new(State { source: String::new(), events: Vec::new(), fetched: None, running: false, generation: 0 });

fn state() -> std::sync::MutexGuard<'static, State> {
    STATE.lock().unwrap_or_else(|e| e.into_inner())
}

/// Window of another process-local owner (the agenda window) that wants a
/// message once a download ends. 0 when nobody asked.
static WATCHER: std::sync::atomic::AtomicIsize = std::sync::atomic::AtomicIsize::new(0);
static WATCHER_MSG: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// Posts `msg` to `hwnd` each time a download ends.
pub fn watch(hwnd: isize, msg: u32) {
    use std::sync::atomic::Ordering::Relaxed;
    WATCHER_MSG.store(msg, Relaxed);
    WATCHER.store(hwnd, Relaxed);
}

fn notify_watcher() {
    use std::sync::atomic::Ordering::Relaxed;
    use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
    let hwnd = WATCHER.load(Relaxed);
    if hwnd != 0 {
        unsafe {
            let _ = windows::Win32::UI::WindowsAndMessaging::PostMessageW(Some(HWND(hwnd as *mut _)), WATCHER_MSG.load(Relaxed), WPARAM(0), LPARAM(0));
        }
    }
}

// ---------------------------------------------------------------- History
//
// Timetables such as ADE only list today and the days ahead. What was seen
// before is kept in `%APPDATA%\naelbar\courses.json`, tied to a hash of the
// address (never the address itself, which may hold a secret).

/// Past lessons kept, in days.
const HISTORY_DAYS: i64 = 400;

#[derive(Default, serde::Serialize, serde::Deserialize)]
#[serde(default)]
struct History {
    source: u64,
    events: Vec<IcsEvent>,
}

fn history_path() -> Option<std::path::PathBuf> {
    std::env::var_os("APPDATA").map(|d| std::path::PathBuf::from(d).join("naelbar").join("courses.json"))
}

/// FNV-1a: stable across builds, unlike the standard hasher.
fn source_hash(source: &str) -> u64 {
    source.trim().bytes().fold(0xcbf2_9ce4_8422_2325, |h, b| (h ^ b as u64).wrapping_mul(0x0100_0000_01b3))
}

fn load_history(source: &str) -> Vec<IcsEvent> {
    let Some(bytes) = history_path().and_then(|p| std::fs::read(p).ok()) else { return Vec::new() };
    match serde_json::from_slice::<History>(&bytes) {
        Ok(h) if h.source == source_hash(source) => h.events,
        _ => Vec::new(),
    }
}

fn save_history(source: &str, events: &[IcsEvent]) {
    let Some(path) = history_path() else { return };
    let Ok(json) = serde_json::to_vec(&History { source: source_hash(source), events: events.to_vec() }) else { return };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    // Replaced in one step: the helper processes read it too.
    let tmp = path.with_extension(format!("{}.tmp", std::process::id()));
    if std::fs::write(&tmp, json).is_ok() && std::fs::rename(&tmp, &path).is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
}

fn today_key() -> String {
    let t = unsafe { windows::Win32::System::SystemInformation::GetLocalTime() };
    crate::agenda::date_key(t.wYear as i32, t.wMonth as u32, t.wDay as u32)
}

/// Days before `today` come from what was kept, today and later from the
/// fresh download. A past day the history lacks is taken from the download.
pub fn merge_history(kept: &[IcsEvent], fresh: Vec<IcsEvent>, today: &str) -> Vec<IcsEvent> {
    let oldest = crate::agenda::parse_date(today).map(|(y, m, d)| {
        let ((y, m, d), _) = civil::from_ms(civil::to_ms(y, m, d, 0, 0, 0) - HISTORY_DAYS * DAY_MS);
        crate::agenda::date_key(y, m, d)
    });
    let past_days: std::collections::HashSet<&str> = kept.iter().filter(|e| e.date.as_str() < today).map(|e| e.date.as_str()).collect();
    let mut out: Vec<IcsEvent> = kept.iter().filter(|e| e.date.as_str() < today).cloned().collect();
    out.extend(fresh.into_iter().filter(|e| e.date.as_str() >= today || !past_days.contains(e.date.as_str())));
    if let Some(oldest) = oldest {
        out.retain(|e| e.date >= oldest);
    }
    out.sort_by(|a, b| a.date.cmp(&b.date).then_with(|| a.time.cmp(&b.time)).then_with(|| a.title.cmp(&b.title)));
    // Keep the most recent ones when there are too many.
    if out.len() > MAX_EVENTS {
        out.drain(..out.len() - MAX_EVENTS);
    }
    out
}

/// Downloads, merges with the history and saves it. None when the download failed.
fn refresh(source: &str) -> Option<Vec<IcsEvent>> {
    let fresh = fetch(source).map(|text| parse_with(&text, &to_local_time))?;
    let merged = merge_history(&load_history(source), fresh, &today_key());
    save_history(source, &merged);
    Some(merged)
}

// ---------------------------------------------------------------- Subjects

/// Colour and visibility of each subject, by its cleaned title.
static STYLES: Mutex<Vec<crate::config::CourseStyle>> = Mutex::new(Vec::new());

fn styles() -> std::sync::MutexGuard<'static, Vec<crate::config::CourseStyle>> {
    STYLES.lock().unwrap_or_else(|e| e.into_inner())
}

/// The style of one lesson: its own if it has one, else its subject's.
pub fn style_in(list: &[crate::config::CourseStyle], e: &IcsEvent) -> Option<crate::config::CourseStyle> {
    let time = e.time.as_deref().unwrap_or("");
    let own = list.iter().find(|s| s.title == e.title && s.date == e.date && s.time == time);
    own.or_else(|| list.iter().find(|s| s.title == e.title && s.date.is_empty())).cloned()
}

/// Whether the user hid this lesson or its subject.
pub fn is_hidden(e: &IcsEvent) -> bool {
    style_in(&styles(), e).is_some_and(|s| s.hidden)
}

/// The lesson's colour as `#RRGGBB`, if one was chosen.
pub fn color_of(e: &IcsEvent) -> Option<String> {
    style_in(&styles(), e).map(|s| s.color).filter(|c| !c.is_empty())
}

/// Same colour, ready to draw.
pub fn ui_color(e: &IcsEvent) -> Option<crate::ui::Color> {
    let rgb = crate::ui::theme::parse_hex(&color_of(e)?)?;
    Some(crate::ui::Color::rgb((rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8))
}

/// Records a choice for one lesson, or for every lesson of its subject. A
/// subject-wide choice drops the lesson-level ones it replaces.
pub fn apply_style(list: &mut Vec<crate::config::CourseStyle>, e: &IcsEvent, all: bool, color: Option<String>, hidden: Option<bool>) {
    let time = e.time.clone().unwrap_or_default();
    let mut next = style_in(list, e).unwrap_or_default();
    next.title = e.title.clone();
    if let Some(c) = color {
        next.color = c;
    }
    if let Some(h) = hidden {
        next.hidden = h;
    }
    if all {
        // Replaces the subject's style and every lesson-level one.
        list.retain(|s| s.title != e.title);
        next.date.clear();
        next.time.clear();
        if next.hidden || !next.color.is_empty() {
            list.push(next);
        }
    } else {
        list.retain(|s| !(s.title == e.title && s.date == e.date && s.time == time));
        let subject = list.iter().find(|s| s.title == e.title && s.date.is_empty()).cloned().unwrap_or_default();
        // A lesson that matches its subject again needs no entry of its own.
        if next.color != subject.color || next.hidden != subject.hidden {
            next.date = e.date.clone();
            next.time = time;
            list.push(next);
        }
    }
}

/// Called when the configuration is read or changes.
pub fn set_source(source: &str) {
    let source = source.trim();
    let mut s = state();
    if s.source != source {
        s.source = source.to_string();
        // What was kept shows at once; the download refreshes it.
        s.events = if source.is_empty() { Vec::new() } else { load_history(source) };
        s.fetched = None;
        s.generation += 1;
    }
}

/// Called with the configuration, next to [`set_source`].
pub fn set_styles(list: &[crate::config::CourseStyle]) {
    *styles() = list.to_vec();
}

pub fn is_configured() -> bool {
    !state().source.is_empty()
}

/// Starts a download unless one ran in the last [`MIN_AGE`]. Cheap to call on
/// every frame of a view that shows the events.
pub fn request() {
    let (source, generation) = {
        let mut s = state();
        if s.source.is_empty() || s.running || s.fetched.is_some_and(|t| t.elapsed() < MIN_AGE) {
            return;
        }
        s.running = true;
        s.fetched = Some(Instant::now());
        (s.source.clone(), s.generation)
    };
    let spawned = std::thread::Builder::new().name("naelbar-ics".into()).spawn(move || {
        let events = refresh(&source);
        {
            let mut s = state();
            s.running = false;
            if s.generation == generation {
                if let Some(events) = events {
                    s.events = events;
                }
            }
        }
        super::notify_island(super::WM_ICS_READY);
        notify_watcher();
    });
    if spawned.is_err() {
        state().running = false;
    }
}

/// Downloads, merges with the history and returns every kept event, now, in
/// the calling thread. For the short-lived helper processes.
pub fn fetch_now(source: &str) -> Option<Vec<IcsEvent>> {
    refresh(source.trim())
}

/// Events of one `YYYY-MM-DD` day, hidden ones left out.
pub fn of_day(date: &str) -> Vec<IcsEvent> {
    of_day_all(date).into_iter().filter(|e| !is_hidden(e)).collect()
}

/// Same, hidden ones included, so they can be shown again.
pub fn of_day_all(date: &str) -> Vec<IcsEvent> {
    let s = state();
    let from = s.events.partition_point(|e| e.date.as_str() < date);
    s.events[from..].iter().take_while(|e| e.date == date).cloned().collect()
}

/// Sorted days of a month that hold at least one event.
pub fn days_of_month(year: i32, month: u32) -> Vec<u32> {
    let prefix = format!("{year:04}-{month:02}-");
    let s = state();
    let from = s.events.partition_point(|e| e.date < prefix);
    let mut days: Vec<u32> = s.events[from..].iter().take_while(|e| e.date.starts_with(&prefix)).filter(|e| !is_hidden(e)).filter_map(|e| e.date[8..].parse().ok()).collect();
    days.dedup();
    days
}

/// The event the tile announces, and when that stops being true.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Next {
    pub event: IcsEvent,
    pub change_in_ms: u64,
}

/// First event not over at `now` (naive local ms): a timed event counts until
/// its minute has passed, an all-day one until the end of its day.
fn next_in(events: &[IcsEvent], now: i64) -> Option<Next> {
    let ((y, m, d), (h, mi)) = civil::from_ms(now);
    let today = crate::agenda::date_key(y, m, d);
    let clock = format!("{h:02}:{mi:02}");
    let from = events.partition_point(|e| e.date < today);
    let event = events[from..].iter().filter(|e| !e.is_project() && !is_hidden(e)).find(|e| e.date != today || e.time.as_deref().is_none_or(|t| t >= clock.as_str()))?;
    let (ey, em, ed) = crate::agenda::parse_date(&event.date)?;
    let over = match event.time.as_deref().and_then(super::countdown::parse_time) {
        Some((th, tm)) => civil::to_ms(ey, em, ed, th, tm, 0) + MIN_MS,
        _ => civil::to_ms(ey, em, ed, 0, 0, 0) + DAY_MS,
    };
    Some(Next { event: event.clone(), change_in_ms: (over - now).max(1) as u64 })
}

pub fn next_after(now: i64) -> Option<Next> {
    next_in(&state().events, now)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//test//EN\r\n\
BEGIN:VEVENT\r\nUID:1\r\nSUMMARY:Cours d'électronique\r\nDTSTART;TZID=Europe/Paris:20261116T083000\r\nDTEND;TZID=Europe/Paris:20261116T103000\r\nEND:VEVENT\r\n\
BEGIN:VEVENT\r\nUID:2\r\nSUMMARY:Réunion\\, projet\\nsalle 4\r\nDTSTART:20261116T130000Z\r\nEND:VEVENT\r\n\
BEGIN:VEVENT\r\nUID:3\r\nSUMMARY:Férié\r\nDTSTART;VALUE=DATE:20261111\r\nDTEND;VALUE=DATE:20261112\r\nEND:VEVENT\r\n\
BEGIN:VEVENT\r\nUID:4\r\nSUMMARY:TP\r\nDTSTART;TZID=Europe/Paris:20261117T140000\r\nRRULE:FREQ=WEEKLY;COUNT=3\r\nEND:VEVENT\r\n\
BEGIN:VEVENT\r\nUID:5\r\nSUMMARY:Séminaire\r\nDTSTART;TZID=Europe/Paris:20261118T100000\r\nRRULE:FREQ=WEEKLY;UNTIL=20261209T120000Z\r\nEND:VEVENT\r\n\
BEGIN:VEVENT\r\nUID:6\r\nSUMMARY:Sans fin\r\nDTSTART;VALUE=DATE:20261120\r\nRRULE:FREQ=WEEKLY\r\nEND:VEVENT\r\n\
BEGIN:VEVENT\r\nUID:7\r\nSUMMARY:Deux jours\r\nDTSTART;TZID=Europe/Paris:20261119T090000\r\nRRULE:FREQ=WEEKLY;COUNT=4;BYDAY=TH,FR\r\nEND:VEVENT\r\n\
BEGIN:VEVENT\r\nUID:8\r\nSUMMARY:Annulé\r\nDTSTART:20261121T090000\r\nSTATUS:CANCELLED\r\nEND:VEVENT\r\n\
BEGIN:VEVENT\r\nUID:9\r\nSUMMARY:Une très longue description qui est repliée sur\r\n  plusieurs lignes\r\nDTSTART:20261122T090000\r\nEND:VEVENT\r\n\
BEGIN:VEVENT\r\nUID:10\r\nDTSTART:20261123T090000\r\nEND:VEVENT\r\n\
END:VCALENDAR\r\n";

    fn titles_on(events: &[IcsEvent], date: &str) -> Vec<(Option<String>, String)> {
        events.iter().filter(|e| e.date == date).map(|e| (e.time.clone(), e.title.clone())).collect()
    }

    #[test]
    fn dates_and_times() {
        let events = parse(SAMPLE, 60);
        assert_eq!(titles_on(&events, "2026-11-11"), [(None, "Férié".to_string())]);
        // TZID times are taken as local.
        assert_eq!(titles_on(&events, "2026-11-16")[0], (Some("08:30".into()), "Cours d'électronique".into()));
    }

    #[test]
    fn utc_times_move_by_the_local_offset() {
        let events = parse(SAMPLE, 60);
        assert_eq!(titles_on(&events, "2026-11-16")[1], (Some("14:00".into()), "Réunion, projet salle 4".into()));
        let summer = parse(SAMPLE, 120);
        assert_eq!(titles_on(&summer, "2026-11-16")[1].0.as_deref(), Some("15:00"));
        // The offset can push an event onto another day.
        let late = parse("BEGIN:VEVENT\nSUMMARY:Nuit\nDTSTART:20261116T233000Z\nEND:VEVENT\n", 120);
        assert_eq!((late[0].date.as_str(), late[0].time.as_deref()), ("2026-11-17", Some("01:30")));
        let early = parse("BEGIN:VEVENT\nSUMMARY:Tôt\nDTSTART:20261116T003000Z\nEND:VEVENT\n", -300);
        assert_eq!((early[0].date.as_str(), early[0].time.as_deref()), ("2026-11-15", Some("19:30")));
    }

    #[test]
    fn weekly_with_a_count() {
        let events = parse(SAMPLE, 60);
        let dates: Vec<&str> = events.iter().filter(|e| e.title == "TP").map(|e| e.date.as_str()).collect();
        assert_eq!(dates, ["2026-11-17", "2026-11-24", "2026-12-01"]);
    }

    #[test]
    fn weekly_with_an_end() {
        let events = parse(SAMPLE, 60);
        let dates: Vec<&str> = events.iter().filter(|e| e.title == "Séminaire").map(|e| e.date.as_str()).collect();
        assert_eq!(dates, ["2026-11-18", "2026-11-25", "2026-12-02", "2026-12-09"]);
    }

    #[test]
    fn other_repeats_are_ignored() {
        let events = parse(SAMPLE, 60);
        // No end, or a BYDAY list: only the first day.
        assert_eq!(events.iter().filter(|e| e.title == "Sans fin").count(), 1);
        assert_eq!(events.iter().filter(|e| e.title == "Deux jours").count(), 1);
    }

    #[test]
    fn an_all_day_end_date_includes_that_day() {
        let text = "BEGIN:VEVENT\nSUMMARY:A\nDTSTART;VALUE=DATE:20261102\nRRULE:FREQ=WEEKLY;UNTIL=20261116\nEND:VEVENT\n";
        let dates: Vec<String> = parse(text, 0).into_iter().map(|e| e.date).collect();
        assert_eq!(dates, ["2026-11-02", "2026-11-09", "2026-11-16"]);
    }

    #[test]
    fn interval_and_year_end() {
        let text = "BEGIN:VEVENT\nSUMMARY:A\nDTSTART:20261221T100000\nRRULE:FREQ=WEEKLY;INTERVAL=2;COUNT=3\nEND:VEVENT\n";
        let dates: Vec<String> = parse(text, 0).into_iter().map(|e| e.date).collect();
        assert_eq!(dates, ["2026-12-21", "2027-01-04", "2027-01-18"]);
    }

    #[test]
    fn cancelled_untitled_and_folded_lines() {
        let events = parse(SAMPLE, 60);
        assert!(events.iter().all(|e| e.title != "Annulé"));
        assert!(events.iter().all(|e| !e.title.is_empty()));
        let folded = events.iter().find(|e| e.date == "2026-11-22").expect("folded event");
        assert_eq!(folded.title, "Une très longue description qui est repliée sur plusieurs lignes");
        assert!(events.iter().all(|e| e.date != "2026-11-23"), "no summary, no event");
    }

    #[test]
    fn the_list_is_sorted_like_the_agenda() {
        let events = parse(SAMPLE, 60);
        let keys: Vec<(&str, Option<&str>)> = events.iter().map(|e| (e.date.as_str(), e.time.as_deref())).collect();
        let mut sorted = keys.clone();
        sorted.sort();
        assert_eq!(keys, sorted);
    }

    #[test]
    fn malformed_input_does_not_panic() {
        for text in ["", "garbage", "BEGIN:VEVENT", "BEGIN:VEVENT\nDTSTART:2026\nSUMMARY:x\nEND:VEVENT", "BEGIN:VEVENT\nDTSTART:20261340T250000\nSUMMARY:x\nEND:VEVENT", "END:VEVENT\nSUMMARY:x"] {
            assert!(parse(text, 60).is_empty(), "{text:?}");
        }
    }

    #[test]
    fn university_titles() {
        assert_eq!(ade_title("R501 Anglais 5 TP CD"), ("Anglais 5".into(), Some("TP".into())));
        assert_eq!(ade_title("SAE5TI02 Construire un projet complexe RN D"), ("Construire un projet complexe".into(), Some("RN".into())));
        assert_eq!(ade_title("R506 Métrologie et qualité 1 CM ABCD"), ("Métrologie et qualité 1".into(), Some("CM".into())));
        assert_eq!(ade_title("R603 Organisation et gestion d’équipe  TD CD"), ("Organisation et gestion d’équipe".into(), Some("TD".into())));
        assert_eq!(ade_title("SAEportfolio SAE PORTFOLIO TP BD"), ("SAE PORTFOLIO".into(), Some("TP".into())));
        assert_eq!(ade_title("R5TI08 Electrotechnique appliquée aux énergies renouvelables TD C"), ("Electrotechnique appliquée aux énergies renouvelables".into(), Some("TD".into())));
        // Titles without a kind stay as they are.
        assert_eq!(ade_title("TOEIC"), ("TOEIC".into(), None));
        assert_eq!(ade_title("VIE ETUDIANTE"), ("VIE ETUDIANTE".into(), None));
        assert_eq!(ade_title("Réunion TP du club de robotique"), ("Réunion TP du club de robotique".into(), None));
        assert_eq!(short_place("BL CH226 TP physique électricité"), "BL CH226");
    }

    #[test]
    fn a_lesson_or_its_whole_subject() {
        let ev = |date: &str| IcsEvent { date: date.into(), time: Some("08:00".into()), title: "Anglais 5".into(), ..IcsEvent::default() };
        let (mon, tue) = (ev("2026-11-23"), ev("2026-11-24"));
        let mut list = Vec::new();
        apply_style(&mut list, &mon, true, Some("#4C9BFF".into()), None);
        assert_eq!(style_in(&list, &tue).unwrap().color, "#4C9BFF");
        // One lesson only: the other keeps the subject's colour.
        apply_style(&mut list, &mon, false, Some("#E5484D".into()), None);
        assert_eq!(style_in(&list, &mon).unwrap().color, "#E5484D");
        assert_eq!(style_in(&list, &tue).unwrap().color, "#4C9BFF");
        apply_style(&mut list, &tue, false, None, Some(true));
        assert!(style_in(&list, &tue).unwrap().hidden);
        assert!(!style_in(&list, &mon).unwrap().hidden);
        // The whole subject again replaces the lesson-level choices.
        apply_style(&mut list, &tue, true, Some(String::new()), Some(false));
        assert!(list.is_empty(), "{list:?}");
    }

    #[test]
    fn past_days_come_from_the_history() {
        let ev = |date: &str, title: &str| IcsEvent { date: date.into(), time: Some("08:00".into()), title: title.into(), ..IcsEvent::default() };
        let kept = vec![ev("2026-11-18", "Ancien"), ev("2026-11-20", "Annulé depuis"), ev("2026-11-21", "Prévu")];
        // The feed only lists today and after; today's lesson moved.
        let fresh = vec![ev("2026-11-20", "Nouveau"), ev("2026-11-23", "Lundi"), ev("2026-11-19", "Oublié")];
        let merged = merge_history(&kept, fresh, "2026-11-20");
        let titles: Vec<&str> = merged.iter().map(|e| e.title.as_str()).collect();
        assert_eq!(titles, ["Ancien", "Oublié", "Nouveau", "Lundi"]);
        // Very old days are dropped.
        let merged = merge_history(&[ev("2024-01-01", "Vieux")], Vec::new(), "2026-11-20");
        assert!(merged.is_empty());
        assert_eq!(source_hash(" a "), source_hash("a"));
        assert_ne!(source_hash("a"), source_hash("b"));
    }

    #[test]
    fn ends_places_and_project_hours() {
        let text = "BEGIN:VEVENT\nSUMMARY:SAE5TI02 Construire un projet complexe RN D\nLOCATION:BL CH226 TP physique\nDTSTART:20261126T120000Z\nDTEND:20261126T153000Z\nEND:VEVENT\n";
        let e = &parse(text, 60)[0];
        assert_eq!((e.time.as_deref(), e.end.as_deref(), e.place.as_str()), (Some("13:00"), Some("16:30"), "BL CH226"));
        assert!(e.is_project());
        assert!(next_in(&parse(text, 60), civil::to_ms(2026, 11, 26, 8, 0, 0)).is_none(), "project hours are not announced");
    }

    #[test]
    fn colons_inside_parameters_do_not_split_the_line() {
        let text = "BEGIN:VEVENT\nSUMMARY:x\nDTSTART;TZID=\"UTC+01:00 (a:b)\":20261116T080000\nEND:VEVENT\n";
        let events = parse(text, 0);
        assert_eq!(events[0].time.as_deref(), Some("08:00"));
    }

    #[test]
    fn days_of_a_month_and_of_a_day() {
        {
            let mut s = state();
            s.events = parse(SAMPLE, 60);
        }
        assert_eq!(of_day("2026-11-16").len(), 2);
        assert!(of_day("2026-11-10").is_empty());
        let days = days_of_month(2026, 11);
        assert!(days.windows(2).all(|w| w[0] < w[1]));
        assert!(days.contains(&11) && days.contains(&16) && days.contains(&25));
        assert!(!days.contains(&12));
        assert!(days_of_month(2025, 11).is_empty());
        state().events.clear();
    }

    #[test]
    fn files_are_read_and_checked() {
        let path = std::env::temp_dir().join(format!("naelbar-ics-test-{}.ics", std::process::id()));
        std::fs::write(&path, SAMPLE).unwrap();
        let text = fetch(path.to_str().unwrap()).expect("read");
        assert_eq!(parse(&text, 60).len(), parse(SAMPLE, 60).len());
        std::fs::write(&path, "not a calendar").unwrap();
        assert!(fetch(path.to_str().unwrap()).is_none());
        let _ = std::fs::remove_file(&path);
        assert!(fetch("C:\\does\\not\\exist.ics").is_none());
    }
    #[test]
    fn the_tile_announces_the_next_event() {
        let events = parse(SAMPLE, 60);
        let at = |d: u32, h: u32, m: u32| civil::to_ms(2026, 11, d, h, m, 0);
        // Before the first lesson of the 16th.
        let n = next_in(&events, at(16, 7, 0)).unwrap();
        assert_eq!((n.event.title.as_str(), n.event.time.as_deref()), ("Cours d'électronique", Some("08:30")));
        assert_eq!(n.change_in_ms, (90 * 60_000 + 60_000) as u64);
        // During the minute of the lesson it still counts, one minute later it does not.
        assert_eq!(next_in(&events, at(16, 8, 30)).unwrap().event.title, "Cours d'électronique");
        assert_eq!(next_in(&events, at(16, 8, 31)).unwrap().event.title, "Réunion, projet salle 4");
        // An all-day event stays until midnight.
        let n = next_in(&events, at(11, 15, 0)).unwrap();
        assert_eq!((n.event.title.as_str(), n.event.time), ("Férié", None));
        assert_eq!(n.change_in_ms, 9 * 3_600_000);
        // Nothing left after the last one.
        assert!(next_in(&events, civil::to_ms(2030, 1, 1, 0, 0, 0)).is_none());
        assert!(next_in(&[], at(16, 7, 0)).is_none());
    }
}
