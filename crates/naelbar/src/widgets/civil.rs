//! Calendar arithmetic on "naive local" milliseconds: the wall clock read as
//! if it were UTC. Pure, so it can be tested without a clock.

pub const MIN_MS: i64 = 60_000;
pub const HOUR_MS: i64 = 3_600_000;
pub const DAY_MS: i64 = 86_400_000;

/// Days since 1970-01-01 of a proleptic Gregorian date.
pub fn days_from_civil(year: i32, month: u32, day: u32) -> i64 {
    let y = if month <= 2 { year as i64 - 1 } else { year as i64 };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (month as i64 + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Inverse of [`days_from_civil`].
pub fn civil_from_days(z: i64) -> (i32, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = (yoe + era * 400 + if month <= 2 { 1 } else { 0 }) as i32;
    (year, month, day)
}

pub fn to_ms(year: i32, month: u32, day: u32, hour: u32, minute: u32, second: u32) -> i64 {
    days_from_civil(year, month, day) * DAY_MS + hour as i64 * HOUR_MS + minute as i64 * MIN_MS + second as i64 * 1000
}

/// Date and time of day (hour, minute) of a naive instant.
pub fn from_ms(ms: i64) -> ((i32, u32, u32), (u32, u32)) {
    let days = ms.div_euclid(DAY_MS);
    let into = ms.rem_euclid(DAY_MS);
    (civil_from_days(days), ((into / HOUR_MS) as u32, (into % HOUR_MS / MIN_MS) as u32))
}

/// Day of the week of a day count, Monday is 0.
pub fn weekday_of_days(days: i64) -> i64 {
    // 1970-01-01 was a Thursday.
    (days + 3).rem_euclid(7)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn epoch_and_round_trip() {
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        for z in [-800_000, -1, 0, 59, 60, 11_016, 19_000, 20_511, 100_000] {
            let (y, m, d) = civil_from_days(z);
            assert_eq!(days_from_civil(y, m, d), z);
        }
    }

    #[test]
    fn leap_days() {
        assert_eq!(days_from_civil(2024, 3, 1) - days_from_civil(2024, 2, 28), 2);
        assert_eq!(days_from_civil(2026, 3, 1) - days_from_civil(2026, 2, 28), 1);
        assert_eq!(days_from_civil(1900, 3, 1) - days_from_civil(1900, 2, 28), 1);
        assert_eq!(days_from_civil(2000, 3, 1) - days_from_civil(2000, 2, 28), 2);
        assert_eq!(civil_from_days(days_from_civil(2024, 2, 29)), (2024, 2, 29));
    }

    #[test]
    fn weekdays_start_on_monday() {
        // 16 November 2026 is a Monday, 1 January 1970 a Thursday.
        assert_eq!(weekday_of_days(days_from_civil(2026, 11, 16)), 0);
        assert_eq!(weekday_of_days(0), 3);
        assert_eq!(weekday_of_days(days_from_civil(2026, 11, 22)), 6);
    }

    #[test]
    fn instants() {
        let ms = to_ms(2026, 11, 16, 14, 35, 59);
        assert_eq!(from_ms(ms), ((2026, 11, 16), (14, 35)));
        assert_eq!(from_ms(-1), ((1969, 12, 31), (23, 59)));
    }
}
