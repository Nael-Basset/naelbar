//! How much of the day, week, month, year and semester has gone by. All
//! instants are naive local milliseconds.

use super::civil::{self, DAY_MS};
use crate::agenda;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Span {
    pub start: i64,
    pub end: i64,
}

impl Span {
    fn len(&self) -> i64 {
        (self.end - self.start).max(1)
    }

    /// Part of the span that has gone by, 0.0 to 1.0.
    #[cfg(test)]
    pub fn fraction(&self, now: i64) -> f64 {
        ((now - self.start) as f64 / self.len() as f64).clamp(0.0, 1.0)
    }

    /// Whole percent shown, 0 to 100.
    pub fn percent(&self, now: i64) -> u32 {
        // Integer arithmetic: a float could round up a hair early.
        (((now - self.start).clamp(0, self.len()) as i128 * 100) / self.len() as i128) as u32
    }

    /// Milliseconds until the whole percent changes, none once at 100.
    pub fn next_percent_in(&self, now: i64) -> Option<u64> {
        let pct = self.percent(now) as i128;
        if pct >= 100 {
            return None;
        }
        let len = self.len() as i128;
        // Smallest offset with 100 * offset >= (pct + 1) * len.
        let at = self.start as i128 + ((pct + 1) * len + 99) / 100;
        Some((at - now as i128).max(1) as u64)
    }
}

fn span(from: (i32, u32, u32), to: (i32, u32, u32)) -> Span {
    Span { start: civil::days_from_civil(from.0, from.1, from.2) * DAY_MS, end: civil::days_from_civil(to.0, to.1, to.2) * DAY_MS }
}

pub fn day(now: i64) -> Span {
    let start = now.div_euclid(DAY_MS) * DAY_MS;
    Span { start, end: start + DAY_MS }
}

/// Monday to the next Monday.
pub fn week(now: i64) -> Span {
    let days = now.div_euclid(DAY_MS);
    let start = (days - civil::weekday_of_days(days)) * DAY_MS;
    Span { start, end: start + 7 * DAY_MS }
}

pub fn month(now: i64) -> Span {
    let (y, m, _) = civil::from_ms(now).0;
    let (ny, nm) = agenda::shift_month(y, m, 1);
    span((y, m, 1), (ny, nm, 1))
}

pub fn year(now: i64) -> Span {
    let (y, _, _) = civil::from_ms(now).0;
    span((y, 1, 1), (y + 1, 1, 1))
}

/// School semesters: September to January, then February to August.
pub fn semester(now: i64) -> Span {
    let (y, m, _) = civil::from_ms(now).0;
    match m {
        9..=12 => span((y, 9, 1), (y + 1, 2, 1)),
        1 => span((y - 1, 9, 1), (y, 2, 1)),
        _ => span((y, 2, 1), (y, 9, 1)),
    }
}

/// Spans in display order, with their French and English names.
pub fn all(now: i64) -> [(&'static str, &'static str, Span); 5] {
    [("Jour", "Day", day(now)), ("Semaine", "Week", week(now)), ("Mois", "Month", month(now)), ("Semestre", "Semester", semester(now)), ("Année", "Year", year(now))]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(y: i32, m: u32, d: u32, h: u32, mi: u32) -> i64 {
        civil::to_ms(y, m, d, h, mi, 0)
    }

    #[test]
    fn the_day() {
        let s = day(at(2026, 11, 16, 6, 0));
        assert_eq!(s.fraction(at(2026, 11, 16, 6, 0)), 0.25);
        assert_eq!(s.percent(at(2026, 11, 16, 0, 0)), 0);
        assert_eq!(s.percent(at(2026, 11, 16, 12, 0)), 50);
        assert_eq!(s.percent(at(2026, 11, 16, 23, 59)), 99);
    }

    #[test]
    fn the_week_starts_on_monday() {
        // Monday 16 November 2026.
        let monday = at(2026, 11, 16, 0, 0);
        assert_eq!(week(monday).start, monday);
        assert_eq!(week(monday).percent(monday), 0);
        let sunday = at(2026, 11, 22, 23, 0);
        assert_eq!(week(sunday).start, monday);
        assert_eq!(week(sunday).end, at(2026, 11, 23, 0, 0));
        assert_eq!(week(sunday).percent(sunday), 99);
        // Thursday noon is 3.5 days of 7.
        assert_eq!(week(at(2026, 11, 19, 12, 0)).percent(at(2026, 11, 19, 12, 0)), 50);
        // A week crossing a month and a year.
        assert_eq!(week(at(2027, 1, 1, 12, 0)).start, at(2026, 12, 28, 0, 0));
    }

    #[test]
    fn months_have_their_real_length() {
        for (y, m, days) in [(2026, 2, 28), (2028, 2, 29), (1900, 2, 28), (2000, 2, 29), (2026, 11, 30), (2026, 12, 31)] {
            let s = month(at(y, m, 10, 0, 0));
            assert_eq!((s.end - s.start) / DAY_MS, days, "{y}-{m}");
        }
        // 15 February of a leap year: 14.0 of 29 days.
        let now = at(2028, 2, 15, 0, 0);
        assert_eq!(month(now).percent(now), 48);
    }

    #[test]
    fn years_have_their_real_length() {
        for (y, days) in [(2026, 365), (2028, 366), (1900, 365), (2000, 366)] {
            let s = year(at(y, 6, 1, 0, 0));
            assert_eq!((s.end - s.start) / DAY_MS, days, "{y}");
        }
        // 1 July 2026 is day 181 of 365, 1 July 2028 day 182 of 366.
        let now = at(2026, 7, 1, 0, 0);
        assert_eq!(year(now).percent(now), 49);
        let now = at(2028, 7, 1, 0, 0);
        assert_eq!(year(now).percent(now), 49);
        assert_eq!(year(now).fraction(at(2028, 1, 1, 0, 0)), 0.0);
    }

    #[test]
    fn semesters_follow_the_school_year() {
        let s = semester(at(2026, 11, 16, 0, 0));
        assert_eq!((s.start, s.end), (at(2026, 9, 1, 0, 0), at(2027, 2, 1, 0, 0)));
        let s = semester(at(2027, 1, 20, 0, 0));
        assert_eq!((s.start, s.end), (at(2026, 9, 1, 0, 0), at(2027, 2, 1, 0, 0)));
        let s = semester(at(2027, 2, 1, 0, 0));
        assert_eq!((s.start, s.end), (at(2027, 2, 1, 0, 0), at(2027, 9, 1, 0, 0)));
        let s = semester(at(2027, 8, 31, 23, 59));
        assert_eq!(s.end, at(2027, 9, 1, 0, 0));
        assert_eq!(s.percent(at(2027, 8, 31, 23, 59)), 99);
    }

    #[test]
    fn the_next_change_is_the_next_whole_percent() {
        let s = day(at(2026, 11, 16, 0, 0));
        // 1 % of a day is 14.4 minutes: 864 000 ms.
        let start = at(2026, 11, 16, 0, 0);
        assert_eq!(s.next_percent_in(start), Some(864_000));
        assert_eq!(s.next_percent_in(start + 863_999), Some(1));
        assert_eq!(s.next_percent_in(start + 864_000), Some(864_000));
        assert_eq!(s.next_percent_in(s.end), None);
        assert_eq!(s.percent(s.end + 5), 100);
    }

    #[test]
    fn the_announced_instant_is_exact() {
        let s = year(at(2028, 5, 5, 0, 0));
        let mut now = at(2028, 5, 5, 13, 17);
        for _ in 0..200 {
            let Some(wait) = s.next_percent_in(now) else { break };
            let p = s.percent(now);
            assert_eq!(s.percent(now + wait as i64 - 1), p);
            assert_eq!(s.percent(now + wait as i64), p + 1);
            now += wait as i64;
        }
    }
}
