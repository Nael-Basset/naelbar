//! Named countdowns to a date and time, stored in `countdowns.json`.

use serde::{Deserialize, Serialize};

use super::civil::{self, DAY_MS, HOUR_MS, MIN_MS};
use crate::agenda;

pub const MAX: usize = 3;
pub const MAX_NAME_CHARS: usize = 40;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Countdown {
    pub name: String,
    /// Local wall-clock time, `YYYY-MM-DDTHH:MM`.
    pub target: String,
}

pub fn path() -> Option<std::path::PathBuf> {
    super::store::dir().map(|d| d.join("countdowns.json"))
}

pub fn load() -> Vec<Countdown> {
    let Some(bytes) = path().and_then(|p| std::fs::read(p).ok()) else { return Vec::new() };
    let mut list: Vec<Countdown> = serde_json::from_slice(&bytes).unwrap_or_default();
    list.retain(|c| target_ms(&c.target).is_some());
    list.truncate(MAX);
    list
}

pub fn save(list: &[Countdown]) -> std::io::Result<()> {
    let path = path().ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "APPDATA is not set"))?;
    super::store::write_atomic(&path, &serde_json::to_vec_pretty(list)?)
}

/// Naive local milliseconds of a stored `YYYY-MM-DDTHH:MM`.
pub fn target_ms(target: &str) -> Option<i64> {
    let (date, time) = target.split_once('T')?;
    let (y, m, d) = agenda::parse_date(date)?;
    let (h, mi) = parse_time(time)?;
    Some(civil::to_ms(y, m, d, h, mi, 0))
}

/// `18:30`, `18h30`, `18h` or `18`. Nothing at all means midnight.
pub fn parse_time(s: &str) -> Option<(u32, u32)> {
    let s = s.trim();
    if s.is_empty() {
        return Some((0, 0));
    }
    let (h, m) = match s.split_once([':', 'h', 'H']) {
        Some((h, m)) => (h, m.trim()),
        None => (s, ""),
    };
    let digits = |t: &str| !t.is_empty() && t.len() <= 2 && t.bytes().all(|c| c.is_ascii_digit());
    if !digits(h) || !(m.is_empty() || (m.len() == 2 && digits(m))) {
        return None;
    }
    let (h, m): (u32, u32) = (h.parse().ok()?, if m.is_empty() { 0 } else { m.parse().ok()? });
    (h < 24 && m < 60).then_some((h, m))
}

/// `2026-12-25`, `25/12/2026`, `25.12.2026`, `25-12-2026`, or `25/12` for the
/// next 25 December (today included).
pub fn parse_date(s: &str, today: (i32, u32, u32)) -> Option<(i32, u32, u32)> {
    let s = s.trim();
    if let Some(d) = agenda::parse_date(s) {
        return Some(d);
    }
    let parts: Vec<&str> = s.split(['/', '.', '-']).collect();
    let number = |t: &str| (!t.is_empty() && t.len() <= 4 && t.bytes().all(|c| c.is_ascii_digit())).then(|| t.parse::<u32>().ok()).flatten();
    let valid = |y: i32, m: u32, d: u32| (d >= 1 && d <= agenda::days_in_month(y, m)).then_some((y, m, d));
    match parts.as_slice() {
        [d, m, y] if y.len() == 4 => valid(number(y)? as i32, number(m)?, number(d)?),
        [d, m] => {
            let (m, d) = (number(m)?, number(d)?);
            let this = valid(today.0, m, d);
            match this {
                Some(date) if (date.1, date.2) >= (today.1, today.2) => Some(date),
                // 29 February falls back to the next leap year.
                _ => (today.0 + 1..today.0 + 9).find_map(|y| valid(y, m, d)),
            }
        }
        _ => None,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Problem {
    Name,
    Date,
    Time,
    Past,
    Full,
}

/// The list with one more countdown, or why it cannot be added. `now_ms` is
/// the naive local time, which also gives today's date.
pub fn with_added(mut list: Vec<Countdown>, name: &str, date: &str, time: &str, now_ms: i64) -> Result<Vec<Countdown>, Problem> {
    let name: String = name.trim().chars().take(MAX_NAME_CHARS).collect();
    if name.is_empty() {
        return Err(Problem::Name);
    }
    if list.len() >= MAX {
        return Err(Problem::Full);
    }
    let today = civil::from_ms(now_ms).0;
    let (y, m, d) = parse_date(date, today).ok_or(Problem::Date)?;
    let (h, mi) = parse_time(time).ok_or(Problem::Time)?;
    if civil::to_ms(y, m, d, h, mi, 0) <= now_ms {
        return Err(Problem::Past);
    }
    list.push(Countdown { name, target: format!("{y:04}-{m:02}-{d:02}T{h:02}:{mi:02}") });
    Ok(list)
}

/// What a countdown shows, and when that changes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Shown {
    pub text: String,
    /// Milliseconds until the text differs, none once it is final.
    pub wait_ms: Option<u64>,
    pub done: bool,
}

/// `J-12` from one day, `3 h 20` from one hour, `12 min` from one minute.
/// Both instants are naive local milliseconds.
pub fn show(target_ms: i64, now_ms: i64, english: bool) -> Shown {
    let rem = target_ms - now_ms;
    let shown = |text: String, wait: i64| Shown { text, wait_ms: Some(wait.max(1) as u64), done: false };
    if rem <= 0 {
        return Shown { text: if english { "Done" } else { "Terminé" }.to_string(), wait_ms: None, done: true };
    }
    if rem >= DAY_MS {
        let days = target_ms.div_euclid(DAY_MS) - now_ms.div_euclid(DAY_MS);
        // The day count moves at midnight, and the unit changes below 24 h.
        let midnight = DAY_MS - now_ms.rem_euclid(DAY_MS);
        return shown(format!("{}-{days}", if english { "D" } else { "J" }), midnight.min(rem - DAY_MS + 1));
    }
    let wait = rem % MIN_MS + 1;
    if rem >= HOUR_MS {
        let minutes = rem / MIN_MS;
        return shown(format!("{} h {:02}", minutes / 60, minutes % 60), wait);
    }
    if rem >= MIN_MS {
        return shown(format!("{} min", rem / MIN_MS), wait);
    }
    shown("< 1 min".to_string(), rem)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(y: i32, m: u32, d: u32, h: u32, mi: u32, s: u32) -> i64 {
        civil::to_ms(y, m, d, h, mi, s)
    }

    #[test]
    fn times_are_read_in_several_spellings() {
        assert_eq!(parse_time("18:30"), Some((18, 30)));
        assert_eq!(parse_time(" 9h05 "), Some((9, 5)));
        assert_eq!(parse_time("9h"), Some((9, 0)));
        assert_eq!(parse_time("7"), Some((7, 0)));
        assert_eq!(parse_time(""), Some((0, 0)));
        for bad in ["24:00", "12:60", "12:5", "ab", "1:2:3", "-1", "123", "12:345"] {
            assert_eq!(parse_time(bad), None, "{bad}");
        }
    }

    #[test]
    fn dates_are_read_in_several_spellings() {
        let today = (2026, 11, 16);
        assert_eq!(parse_date("2026-12-25", today), Some((2026, 12, 25)));
        assert_eq!(parse_date("25/12/2026", today), Some((2026, 12, 25)));
        assert_eq!(parse_date("5.1.2027", today), Some((2027, 1, 5)));
        assert_eq!(parse_date("25-12-2026", today), Some((2026, 12, 25)));
        assert_eq!(parse_date("25/12", today), Some((2026, 12, 25)));
        assert_eq!(parse_date("16/11", today), Some((2026, 11, 16)));
        assert_eq!(parse_date("15/11", today), Some((2027, 11, 15)));
        assert_eq!(parse_date("29/02", today), Some((2028, 2, 29)));
        for bad in ["", "31/02/2026", "2026-02-30", "32/12", "a/b", "1/2/3", "12/13", "25/12/26"] {
            assert_eq!(parse_date(bad, today), None, "{bad}");
        }
    }

    #[test]
    fn stored_targets() {
        assert_eq!(target_ms("2026-12-25T18:30"), Some(at(2026, 12, 25, 18, 30, 0)));
        assert_eq!(target_ms("2026-12-25"), None);
        assert_eq!(target_ms("2026-12-25T25:00"), None);
    }

    #[test]
    fn days_use_the_calendar() {
        let now = at(2026, 11, 16, 10, 0, 0);
        assert_eq!(show(at(2026, 11, 28, 0, 0, 0), now, false).text, "J-12");
        assert_eq!(show(at(2026, 11, 28, 0, 0, 0), now, true).text, "D-12");
        assert_eq!(show(at(2026, 11, 17, 23, 0, 0), now, false).text, "J-1");
        // 24 h exactly is still counted in days.
        assert_eq!(show(at(2026, 11, 17, 10, 0, 0), now, false).text, "J-1");
        assert_eq!(show(at(2028, 2, 29, 12, 0, 0), at(2028, 2, 28, 11, 0, 0), false).text, "J-1");
    }

    #[test]
    fn days_across_a_leap_day() {
        let now = at(2028, 2, 27, 8, 0, 0);
        assert_eq!(show(at(2028, 3, 1, 8, 0, 0), now, false).text, "J-3");
        let now = at(2027, 2, 27, 8, 0, 0);
        assert_eq!(show(at(2027, 3, 1, 8, 0, 0), now, false).text, "J-2");
    }

    #[test]
    fn hours_and_minutes() {
        let now = at(2026, 11, 16, 10, 0, 0);
        assert_eq!(show(at(2026, 11, 16, 13, 20, 0), now, false).text, "3 h 20");
        assert_eq!(show(at(2026, 11, 16, 11, 5, 30), now, false).text, "1 h 05");
        assert_eq!(show(at(2026, 11, 16, 10, 12, 0), now, false).text, "12 min");
        assert_eq!(show(at(2026, 11, 16, 10, 59, 59), now, false).text, "59 min");
        assert_eq!(show(at(2026, 11, 16, 10, 0, 30), now, false).text, "< 1 min");
        assert_eq!(show(at(2026, 11, 17, 9, 59, 59), now, false).text, "23 h 59");
    }

    #[test]
    fn finished_countdowns_stop_repainting() {
        let now = at(2026, 11, 16, 10, 0, 0);
        let s = show(now, now, false);
        assert!(s.done && s.wait_ms.is_none());
        assert_eq!(s.text, "Terminé");
        assert_eq!(show(now - 1, now, true).text, "Done");
    }

    #[test]
    fn the_repaint_comes_when_the_text_changes() {
        let target = at(2026, 11, 16, 13, 20, 0);
        // 3 h 19 min 30 s left: "3 h 19" until 30 s and 1 ms later.
        let now = at(2026, 11, 16, 10, 0, 30);
        let s = show(target, now, false);
        assert_eq!(s.text, "3 h 19");
        assert_eq!(s.wait_ms, Some(30_001));
        let later = show(target, now + 30_001, false);
        assert_eq!(later.text, "3 h 18");
        let after = show(target, now + 30_001 + 60_000, false);
        assert_eq!(after.text, "3 h 17");
    }

    #[test]
    fn the_text_never_changes_before_the_announced_wait() {
        let target = at(2026, 11, 20, 8, 0, 0);
        let mut now = at(2026, 11, 18, 22, 7, 13);
        let mut last = show(target, now, false);
        let mut changes = 0;
        while let Some(wait) = last.wait_ms {
            // Just before the announced instant nothing differs.
            let before = show(target, now + wait as i64 - 1, false);
            assert_eq!(before.text, last.text);
            now += wait as i64;
            let next = show(target, now, false);
            if next.text != last.text {
                changes += 1;
            }
            last = next;
            if changes > 5000 {
                break;
            }
        }
        assert!(last.done);
        // Two day counts, then a text per minute for the last 32 hours.
        assert!(changes > 1000);
    }

    #[test]
    fn day_text_changes_at_midnight() {
        let target = at(2026, 11, 28, 12, 0, 0);
        let now = at(2026, 11, 16, 23, 0, 0);
        let s = show(target, now, false);
        assert_eq!(s.text, "J-12");
        assert_eq!(s.wait_ms, Some(HOUR_MS as u64));
        assert_eq!(show(target, now + HOUR_MS, false).text, "J-11");
    }

    #[test]
    fn the_switch_to_hours_is_announced() {
        let target = at(2026, 11, 17, 12, 0, 0);
        let now = at(2026, 11, 16, 9, 0, 0);
        let s = show(target, now, false);
        assert_eq!(s.text, "J-1");
        // 27 h left: the day count would move at midnight (15 h away), but
        // the unit changes first at 24 h, three hours away.
        assert_eq!(s.wait_ms, Some(3 * HOUR_MS as u64 + 1));
        assert_eq!(show(target, now + 3 * HOUR_MS + 1, false).text, "23 h 59");
    }

    #[test]
    fn adding_a_countdown() {
        let now = at(2026, 11, 16, 10, 0, 0);
        let list = with_added(Vec::new(), "  Partiels ", "15/12", "8h30", now).unwrap();
        assert_eq!(list, [Countdown { name: "Partiels".into(), target: "2026-12-15T08:30".into() }]);
        assert_eq!(target_ms(&list[0].target), Some(at(2026, 12, 15, 8, 30, 0)));
        // A date alone means midnight.
        let list = with_added(list, "Noël", "2026-12-25", "", now).unwrap();
        assert_eq!(list[1].target, "2026-12-25T00:00");
        let list = with_added(list, "Vacances", "20/12/2026", "18:00", now).unwrap();
        assert_eq!(list.len(), 3);
        assert_eq!(with_added(list.clone(), "Quatre", "2026-12-30", "", now), Err(Problem::Full));
        assert_eq!(list.len(), 3);
    }

    #[test]
    fn refused_countdowns() {
        let now = at(2026, 11, 16, 10, 0, 0);
        assert_eq!(with_added(Vec::new(), "  ", "2026-12-25", "", now), Err(Problem::Name));
        assert_eq!(with_added(Vec::new(), "A", "bientôt", "", now), Err(Problem::Date));
        assert_eq!(with_added(Vec::new(), "A", "2026-12-25", "25:00", now), Err(Problem::Time));
        assert_eq!(with_added(Vec::new(), "A", "2026-11-16", "09:59", now), Err(Problem::Past));
        assert_eq!(with_added(Vec::new(), "A", "2026-11-16", "10:00", now), Err(Problem::Past));
        assert!(with_added(Vec::new(), "A", "2026-11-16", "10:01", now).is_ok());
        let long = "x".repeat(100);
        assert_eq!(with_added(Vec::new(), &long, "2026-12-25", "", now).unwrap()[0].name.chars().count(), MAX_NAME_CHARS);
    }
}
