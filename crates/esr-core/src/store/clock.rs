//! Local wall-clock time for `saved-at` without a clock in the core.
//!
//! The core only sees the monotonic [`Millis`] clock. The shell supplies a
//! [`WallClock`] anchor ("at monotonic time `at` the local time was `local`")
//! through [`Environment::wall_clock`](crate::engine::Environment), and the
//! store derives the local time of a save from it with exact calendar
//! arithmetic. The UTC offset of the anchor is kept, so a time-zone or DST
//! change is picked up when the shell sends a fresh anchor.

use crate::engine::Millis;
use crate::model::Timestamp;

/// Local time `local` observed at monotonic time `at`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct WallClock {
    pub at: Millis,
    pub local: Timestamp,
}

impl WallClock {
    /// The local time at monotonic time `now` (earlier or later than `at`).
    pub fn local_at(&self, now: Millis) -> Timestamp {
        let delta = i128::from(now.0) - i128::from(self.at.0);
        add_millis(self.local, delta.clamp(i128::from(i64::MIN), i128::from(i64::MAX)) as i64)
    }
}

/// Used for `saved-at` when the shell supplied no anchor.
pub const UNKNOWN_TIME: Timestamp = Timestamp {
    year: 1970,
    month: 1,
    day: 1,
    hour: 0,
    minute: 0,
    second: 0,
    millis: 0,
    offset_minutes: 0,
};

/// The local time at `now`, or [`UNKNOWN_TIME`] without an anchor.
pub fn local_time(wall: Option<WallClock>, now: Millis) -> Timestamp {
    wall.map_or(UNKNOWN_TIME, |w| w.local_at(now))
}

const MS_PER_DAY: i64 = 86_400_000;

/// `ts` shifted by `delta_ms` milliseconds, keeping its UTC offset. The result
/// is clamped to the years 1..=9999 that a [`Timestamp`] can hold.
pub fn add_millis(ts: Timestamp, delta_ms: i64) -> Timestamp {
    let day = days_from_civil(i64::from(ts.year), i64::from(ts.month), i64::from(ts.day));
    let ms_of_day = ((i64::from(ts.hour) * 60 + i64::from(ts.minute)) * 60 + i64::from(ts.second))
        * 1000
        + i64::from(ts.millis);
    let total = i128::from(day) * i128::from(MS_PER_DAY) + i128::from(ms_of_day) + i128::from(delta_ms);
    let min = i128::from(days_from_civil(1, 1, 1)) * i128::from(MS_PER_DAY);
    let max = i128::from(days_from_civil(10_000, 1, 1)) * i128::from(MS_PER_DAY) - 1;
    let total = total.clamp(min, max) as i64;

    let (y, m, d) = civil_from_days(total.div_euclid(MS_PER_DAY));
    let rem = total.rem_euclid(MS_PER_DAY);
    Timestamp {
        year: y as u16,
        month: m as u8,
        day: d as u8,
        hour: (rem / 3_600_000) as u8,
        minute: (rem / 60_000 % 60) as u8,
        second: (rem / 1000 % 60) as u8,
        millis: (rem % 1000) as u16,
        offset_minutes: ts.offset_minutes,
    }
}

/// Days since 1970-01-01 of a proleptic Gregorian date (H. Hinnant's algorithm).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = if m > 2 { m - 3 } else { m + 9 };
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Inverse of [`days_from_civil`].
fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { yoe + era * 400 + 1 } else { yoe + era * 400 }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ts(year: u16, month: u8, day: u8, hour: u8, minute: u8, second: u8, millis: u16) -> Timestamp {
        Timestamp { year, month, day, hour, minute, second, millis, offset_minutes: 60 }
    }

    #[test]
    fn adds_across_day_month_year_and_leap_day() {
        assert_eq!(add_millis(ts(2025, 1, 31, 23, 59, 59, 999), 1), ts(2025, 2, 1, 0, 0, 0, 0));
        assert_eq!(add_millis(ts(2024, 2, 28, 12, 0, 0, 0), MS_PER_DAY), ts(2024, 2, 29, 12, 0, 0, 0));
        assert_eq!(add_millis(ts(2023, 2, 28, 12, 0, 0, 0), MS_PER_DAY), ts(2023, 3, 1, 12, 0, 0, 0));
        assert_eq!(add_millis(ts(2025, 12, 31, 23, 0, 0, 0), 3_600_000), ts(2026, 1, 1, 0, 0, 0, 0));
        assert_eq!(add_millis(ts(2026, 1, 1, 0, 0, 0, 0), -1), ts(2025, 12, 31, 23, 59, 59, 999));
        assert_eq!(add_millis(ts(2025, 6, 15, 9, 15, 2, 123), 0), ts(2025, 6, 15, 9, 15, 2, 123));
    }

    #[test]
    fn clamps_to_the_representable_years() {
        assert_eq!(add_millis(ts(9999, 12, 31, 23, 59, 59, 999), 10), ts(9999, 12, 31, 23, 59, 59, 999));
        assert_eq!(add_millis(ts(1, 1, 1, 0, 0, 0, 0), -10), ts(1, 1, 1, 0, 0, 0, 0));
        assert!(add_millis(ts(2025, 1, 1, 0, 0, 0, 0), i64::MAX).is_valid());
        assert!(add_millis(ts(2025, 1, 1, 0, 0, 0, 0), i64::MIN).is_valid());
    }

    #[test]
    fn day_conversion_round_trips() {
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        for z in (-800_000..3_000_000).step_by(997) {
            let (y, m, d) = civil_from_days(z);
            assert_eq!(days_from_civil(y, m, d), z);
        }
    }

    #[test]
    fn wall_clock_follows_the_monotonic_clock() {
        let w = WallClock { at: Millis(10_000), local: ts(2025, 1, 31, 9, 15, 2, 123) };
        assert_eq!(w.local_at(Millis(10_877)), ts(2025, 1, 31, 9, 15, 3, 0));
        assert_eq!(w.local_at(Millis(9_000)), ts(2025, 1, 31, 9, 15, 1, 123));
        assert_eq!(local_time(None, Millis(5)), UNKNOWN_TIME);
        assert!(UNKNOWN_TIME.is_valid());
    }
}
