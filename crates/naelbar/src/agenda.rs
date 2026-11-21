//! Local agenda and post-its, stored in `%APPDATA%\naelbar\agenda.json`.
//!
//! Deletes are tombstones and every record carries its last change time, so
//! two copies of the data can be merged by id at any moment.

use std::collections::HashMap;
use std::collections::hash_map::RandomState;
use std::fs;
use std::hash::{BuildHasher, Hasher};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use windows::Win32::UI::WindowsAndMessaging::WM_APP;

/// Posted by the editor process once the file has changed.
pub const WM_AGENDA_CHANGED: u32 = WM_APP + 6;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Event {
    pub id: String,
    pub title: String,
    /// `YYYY-MM-DD`.
    pub date: String,
    /// `HH:MM`, or none for an event without a time.
    pub time: Option<String>,
    /// `HH:MM` end, set from the phone; none for a one-hour block.
    pub end: Option<String>,
    pub updated_at_ms: u64,
    pub deleted: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Note {
    pub id: String,
    pub text: String,
    pub done: bool,
    pub updated_at_ms: u64,
    pub deleted: bool,
}

/// What merging needs to know about a record.
pub trait Stamped {
    fn id(&self) -> &str;
    fn updated_at_ms(&self) -> u64;
    fn deleted(&self) -> bool;
}

impl Stamped for Event {
    fn id(&self) -> &str {
        &self.id
    }
    fn updated_at_ms(&self) -> u64 {
        self.updated_at_ms
    }
    fn deleted(&self) -> bool {
        self.deleted
    }
}

impl Stamped for Note {
    fn id(&self) -> &str {
        &self.id
    }
    fn updated_at_ms(&self) -> u64 {
        self.updated_at_ms
    }
    fn deleted(&self) -> bool {
        self.deleted
    }
}

/// Merges two lists by id, keeping the newest `updated_at_ms`. On equal times
/// a tombstone wins, otherwise the record met first stays. Order is that of
/// `a`, followed by what only `b` has.
pub fn merge_by_id<T: Stamped + Clone>(a: &[T], b: &[T]) -> Vec<T> {
    let mut out: Vec<T> = Vec::with_capacity(a.len() + b.len());
    let mut index: HashMap<&str, usize> = HashMap::with_capacity(a.len() + b.len());
    for item in a.iter().chain(b) {
        match index.get(item.id()) {
            Some(&i) => {
                if (item.updated_at_ms(), item.deleted()) > (out[i].updated_at_ms(), out[i].deleted()) {
                    out[i] = item.clone();
                }
            }
            None => {
                index.insert(item.id(), out.len());
                out.push(item.clone());
            }
        }
    }
    out
}

/// 128 random bits as hex. The standard library seeds `RandomState` from the
/// operating system, which spares a dependency.
pub fn new_id() -> String {
    let word = || RandomState::new().build_hasher().finish();
    format!("{:016x}{:016x}", word(), word())
}

/// A change must sort after what it replaces, even if the clock went back.
fn next_stamp(current: u64, now: u64) -> u64 {
    now.max(current.saturating_add(1))
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Agenda {
    pub events: Vec<Event>,
    pub notes: Vec<Note>,
}

pub fn path() -> Option<PathBuf> {
    std::env::var_os("APPDATA").map(|d| PathBuf::from(d).join("naelbar").join("agenda.json"))
}

impl Agenda {
    pub fn load() -> Self {
        path().map(|p| Self::load_from(&p)).unwrap_or_default()
    }

    /// A missing file is an empty agenda. An unreadable one too, but it is
    /// copied aside first so the next save does not destroy it.
    pub fn load_from(path: &Path) -> Self {
        let Ok(bytes) = fs::read(path) else { return Self::default() };
        match serde_json::from_slice::<Self>(&bytes) {
            Ok(mut agenda) => {
                agenda.events.retain(|e| !e.id.is_empty());
                agenda.notes.retain(|n| !n.id.is_empty());
                agenda
            }
            Err(_) => {
                let _ = fs::copy(path, path.with_extension("json.bad"));
                Self::default()
            }
        }
    }

    pub fn save(&self) -> io::Result<()> {
        let path = path().ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "APPDATA is not set"))?;
        self.save_to(&path)
    }

    /// Replaced in one step: another process may read it at any time.
    pub fn save_to(&self, path: &Path) -> io::Result<()> {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        let json = serde_json::to_vec_pretty(self)?;
        let tmp = path.with_extension(format!("{}.tmp", std::process::id()));
        let written = fs::File::create(&tmp).and_then(|mut f| f.write_all(&json).and_then(|()| f.sync_all()));
        let result = written.and_then(|()| fs::rename(&tmp, path));
        if result.is_err() {
            let _ = fs::remove_file(&tmp);
        }
        result
    }

    pub fn merge(&mut self, other: &Agenda) {
        self.events = merge_by_id(&self.events, &other.events);
        self.notes = merge_by_id(&self.notes, &other.notes);
    }

    pub fn delete_event(&mut self, id: &str, now_ms: u64) {
        if let Some(e) = self.events.iter_mut().find(|e| e.id == id) {
            e.deleted = true;
            e.updated_at_ms = next_stamp(e.updated_at_ms, now_ms);
        }
    }

    pub fn delete_note(&mut self, id: &str, now_ms: u64) {
        if let Some(n) = self.notes.iter_mut().find(|n| n.id == id) {
            n.deleted = true;
            n.updated_at_ms = next_stamp(n.updated_at_ms, now_ms);
        }
    }

    pub fn toggle_note(&mut self, id: &str, now_ms: u64) {
        if let Some(n) = self.notes.iter_mut().find(|n| n.id == id) {
            n.done = !n.done;
            n.updated_at_ms = next_stamp(n.updated_at_ms, now_ms);
        }
    }
}

pub fn is_leap(year: i32) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

pub fn days_in_month(year: i32, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if is_leap(year) => 29,
        2 => 28,
        _ => 0,
    }
}

/// Day of the week, Monday is 0.
pub fn weekday(year: i32, month: u32, day: u32) -> u32 {
    const OFFSET: [i32; 12] = [0, 3, 2, 5, 0, 3, 5, 1, 4, 6, 2, 4];
    let y = if month < 3 { year - 1 } else { year };
    let sunday_first = (y + y / 4 - y / 100 + y / 400 + OFFSET[(month as usize - 1) % 12] + day as i32).rem_euclid(7);
    ((sunday_first + 6) % 7) as u32
}

/// Six Monday-first weeks; the cells outside the month are empty.
pub fn month_grid(year: i32, month: u32) -> [Option<u32>; 42] {
    let mut grid = [None; 42];
    let first = weekday(year, month, 1) as usize;
    for day in 1..=days_in_month(year, month) {
        grid[first + day as usize - 1] = Some(day);
    }
    grid
}

/// The month `delta` months away.
pub fn shift_month(year: i32, month: u32, delta: i32) -> (i32, u32) {
    let total = year * 12 + month as i32 - 1 + delta;
    (total.div_euclid(12), total.rem_euclid(12) as u32 + 1)
}

pub fn date_key(year: i32, month: u32, day: u32) -> String {
    format!("{year:04}-{month:02}-{day:02}")
}

/// Strict `YYYY-MM-DD`, and the day must exist.
pub fn parse_date(s: &str) -> Option<(i32, u32, u32)> {
    let well_formed = s.len() == 10 && s.bytes().enumerate().all(|(i, c)| if i == 4 || i == 7 { c == b'-' } else { c.is_ascii_digit() });
    if !well_formed {
        return None;
    }
    let year: i32 = s[0..4].parse().ok()?;
    let month: u32 = s[5..7].parse().ok()?;
    let day: u32 = s[8..10].parse().ok()?;
    (day >= 1 && day <= days_in_month(year, month)).then_some((year, month, day))
}

/// Visible events of a day: those without a time first, then by time.
pub fn events_of_day<'a>(events: &'a [Event], date: &str) -> Vec<&'a Event> {
    let mut day: Vec<&Event> = events.iter().filter(|e| !e.deleted && e.date == date).collect();
    day.sort_by(|a, b| a.time.cmp(&b.time).then_with(|| a.title.cmp(&b.title)).then_with(|| a.id.cmp(&b.id)));
    day
}

/// Sorted days of a month holding at least one visible event.
pub fn days_with_events(events: &[Event], year: i32, month: u32) -> Vec<u32> {
    let mut days: Vec<u32> = events.iter().filter(|e| !e.deleted).filter_map(|e| parse_date(&e.date)).filter(|&(y, m, _)| y == year && m == month).map(|(_, _, d)| d).collect();
    days.sort_unstable();
    days.dedup();
    days
}

pub fn visible_notes(notes: &[Note]) -> Vec<&Note> {
    notes.iter().filter(|n| !n.deleted).collect()
}

pub fn month_name(month: u32, english: bool) -> &'static str {
    const FR: [&str; 12] = ["Janvier", "Février", "Mars", "Avril", "Mai", "Juin", "Juillet", "Août", "Septembre", "Octobre", "Novembre", "Décembre"];
    const EN: [&str; 12] = ["January", "February", "March", "April", "May", "June", "July", "August", "September", "October", "November", "December"];
    (if english { EN } else { FR })[(month as usize).clamp(1, 12) - 1]
}

/// `mar. 10 novembre`, or `Tue 10 November` in English.
pub fn day_label(year: i32, month: u32, day: u32, english: bool) -> String {
    const FR: [&str; 7] = ["lun.", "mar.", "mer.", "jeu.", "ven.", "sam.", "dim."];
    const EN: [&str; 7] = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];
    let name = (if english { EN } else { FR })[weekday(year, month, day) as usize];
    let month = month_name(month, english);
    format!("{name} {day} {}", if english { month.to_string() } else { month.to_lowercase() })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(id: &str, date: &str, time: Option<&str>, at: u64) -> Event {
        Event { id: id.into(), title: id.to_uppercase(), date: date.into(), time: time.map(Into::into), end: None, updated_at_ms: at, deleted: false }
    }

    fn note(id: &str, text: &str, at: u64) -> Note {
        Note { id: id.into(), text: text.into(), done: false, updated_at_ms: at, deleted: false }
    }

    fn temp_file() -> PathBuf {
        std::env::temp_dir().join(format!("naelbar-agenda-test-{}", new_id())).join("agenda.json")
    }

    #[test]
    fn day_lists_untimed_first_then_by_time() {
        let events = [
            event("late", "2026-11-10", Some("18:00"), 1),
            event("other", "2026-11-11", None, 1),
            event("all", "2026-11-10", None, 1),
            event("early", "2026-11-10", Some("08:30"), 1),
            Event { deleted: true, ..event("gone", "2026-11-10", Some("09:00"), 1) },
        ];
        let ids: Vec<&str> = events_of_day(&events, "2026-11-10").iter().map(|e| e.id.as_str()).collect();
        assert_eq!(ids, ["all", "early", "late"]);
    }

    #[test]
    fn month_dots_ignore_tombstones_and_bad_dates() {
        let events = [
            event("a", "2026-11-20", None, 1),
            event("b", "2026-11-03", None, 1),
            event("c", "2026-11-03", Some("10:00"), 1),
            event("d", "2026-12-03", None, 1),
            event("e", "not a date", None, 1),
            Event { deleted: true, ..event("f", "2026-11-05", None, 1) },
        ];
        assert_eq!(days_with_events(&events, 2026, 11), [3, 20]);
    }

    #[test]
    fn grid_starts_on_monday() {
        // 1 November 2026 is a Sunday, 1 June 2026 a Monday.
        let nov = month_grid(2026, 11);
        assert_eq!(&nov[..7], &[None, None, None, None, None, None, Some(1)]);
        assert_eq!(nov[35], Some(30));
        assert_eq!(nov[36], None);
        assert_eq!(month_grid(2026, 6)[0], Some(1));
    }

    #[test]
    fn leap_years_in_the_grid() {
        assert!(is_leap(2024) && is_leap(2000));
        assert!(!is_leap(2026) && !is_leap(1900));
        // 1 February 2024 is a Thursday.
        let leap = month_grid(2024, 2);
        assert_eq!(leap[3], Some(1));
        assert_eq!(leap[31], Some(29));
        assert_eq!(leap[32], None);
        // 1 February 2023 is a Wednesday.
        let common = month_grid(2023, 2);
        assert_eq!(common[2], Some(1));
        assert_eq!(common[29], Some(28));
        assert_eq!(common[30], None);
    }

    #[test]
    fn longest_month_fits_the_grid() {
        // 31 days starting on a Sunday use the sixth week.
        let may = month_grid(2022, 5);
        assert_eq!(may[6], Some(1));
        assert_eq!(may[36], Some(31));
    }

    #[test]
    fn months_shift_across_years() {
        assert_eq!(shift_month(2026, 1, -1), (2025, 12));
        assert_eq!(shift_month(2026, 12, 1), (2027, 1));
        assert_eq!(shift_month(2026, 6, 1), (2026, 7));
    }

    #[test]
    fn dates_are_strict() {
        assert_eq!(parse_date("2024-02-29"), Some((2024, 2, 29)));
        for bad in ["2023-02-29", "2026-13-01", "2026-00-10", "2026-11-00", "2026-1-10", "+026-11-10", "2026/11/10", "", "2026-11-1é"] {
            assert_eq!(parse_date(bad), None, "{bad}");
        }
        assert_eq!(date_key(2026, 3, 7), "2026-03-07");
    }

    #[test]
    fn labels_follow_the_language() {
        assert_eq!(day_label(2026, 11, 10, false), "mar. 10 novembre");
        assert_eq!(day_label(2026, 11, 10, true), "Tue 10 November");
    }

    #[test]
    fn merge_keeps_the_newest() {
        let a = [note("1", "old", 10), note("2", "only a", 5), note("3", "newer in a", 30)];
        let b = [note("1", "new", 20), note("3", "older in b", 25), note("4", "only b", 1)];
        let merged = merge_by_id(&a, &b);
        let texts: Vec<&str> = merged.iter().map(|n| n.text.as_str()).collect();
        assert_eq!(texts, ["new", "only a", "newer in a", "only b"]);
    }

    #[test]
    fn merge_converges_in_both_directions() {
        let a = [event("x", "2026-01-01", None, 7)];
        let b = [Event { deleted: true, ..event("x", "2026-01-01", None, 7) }];
        // Same time: the delete must win whichever side it came from.
        assert!(merge_by_id(&a, &b)[0].deleted);
        assert!(merge_by_id(&b, &a)[0].deleted);
    }

    #[test]
    fn a_newer_tombstone_hides_and_an_older_one_does_not() {
        let live = note("1", "text", 10);
        let newer = Note { deleted: true, ..note("1", "text", 11) };
        let older = Note { deleted: true, ..note("1", "text", 9) };
        assert!(merge_by_id(std::slice::from_ref(&live), &[newer])[0].deleted);
        assert!(!merge_by_id(&[live], &[older])[0].deleted);
    }

    #[test]
    fn changes_stamp_after_the_previous_one() {
        let mut agenda = Agenda { events: vec![event("e", "2026-01-01", None, 100)], notes: vec![note("n", "t", 100)] };
        // The clock went back: the change must still win a later merge.
        agenda.toggle_note("n", 50);
        assert!(agenda.notes[0].done);
        assert_eq!(agenda.notes[0].updated_at_ms, 101);
        agenda.delete_event("e", 500);
        assert!(agenda.events[0].deleted);
        assert_eq!(agenda.events[0].updated_at_ms, 500);
        agenda.delete_note("n", 600);
        assert!(visible_notes(&agenda.notes).is_empty());
    }

    #[test]
    fn ids_are_random_hex() {
        let (a, b) = (new_id(), new_id());
        assert_eq!(a.len(), 32);
        assert!(a.bytes().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b);
    }

    #[test]
    fn file_round_trip_leaves_no_temp_file() {
        let path = temp_file();
        let agenda = Agenda { events: vec![event("e", "2026-01-01", Some("07:05"), 3)], notes: vec![note("n", "été à noter", 4)] };
        agenda.save_to(&path).unwrap();
        assert_eq!(Agenda::load_from(&path), agenda);
        let dir = path.parent().unwrap();
        assert_eq!(fs::read_dir(dir).unwrap().count(), 1);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn a_bad_file_is_empty_and_kept_aside() {
        let path = temp_file();
        let dir = path.parent().unwrap();
        fs::create_dir_all(dir).unwrap();
        fs::write(&path, b"{ this is not json").unwrap();
        assert_eq!(Agenda::load_from(&path), Agenda::default());
        assert!(path.with_extension("json.bad").exists());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn a_missing_file_is_empty() {
        assert_eq!(Agenda::load_from(&temp_file()), Agenda::default());
    }

    #[test]
    fn records_without_an_id_are_dropped() {
        let path = temp_file();
        let dir = path.parent().unwrap();
        fs::create_dir_all(dir).unwrap();
        fs::write(&path, br#"{"notes":[{"text":"no id"},{"id":"a","text":"ok"}]}"#).unwrap();
        let agenda = Agenda::load_from(&path);
        assert_eq!(agenda.notes.len(), 1);
        assert_eq!(agenda.notes[0].id, "a");
        let _ = fs::remove_dir_all(dir);
    }
}
