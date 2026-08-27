//! Alarm ordering/recurrence math and ID allocation, moved verbatim out of
//! `rust-firmware/src/alarms.rs` (which re-exports everything here so
//! nothing else has to change). The NVS-backed `AlarmStore`, hardware ring
//! screen, and PCF8563 programming stay in that file - only the pure
//! date/schedule arithmetic and the `StoredAlarm`/`Repeat` data shapes live
//! here.
//!
//! Day-number/calendar-date conversion itself is not reimplemented here -
//! it's `datetime::days_since_epoch`/`datetime::date_from_days`, re-exported
//! below so existing call sites are unaffected.

use serde::{Deserialize, Serialize};

use crate::datetime::weekday_from_days;
use crate::datetime::DateTime;

// Re-exported so `rust-firmware/src/alarms.rs`'s existing
// `alarms::{days_since_epoch, date_from_days}` call sites keep working
// unchanged - `datetime` is the single canonical implementation now (see
// its module doc comment).
pub use crate::datetime::{date_from_days, days_since_epoch};

/// Recurrence schedule, wire-compatible with the server's
/// `models::Repeat`. Externally tagged by serde: `"Daily"`, `{"Weekly":
/// {"days": [0, 2, 4]}}`, `{"Monthly": {"days": [1, 15]}}`, or `{"Once":
/// {"year": 2026, "month": 8, "day": 19}}`. Weekdays are 0=Sunday ..
/// 6=Saturday (matching the RTC's `DateTime.weekday` and JS
/// `Date.getDay()`); month days are 1..=31.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum Repeat {
    /// Every day at the alarm's time.
    Daily,
    /// Days of the week at the alarm's time. Never empty once created.
    Weekly { days: Vec<u8> },
    /// Days of the month at the alarm's time. Never empty once created.
    Monthly { days: Vec<u8> },
    /// Fires once on this calendar date, then the caller is expected to
    /// drop it from the store (see `main.rs`'s ack-and-rearm flow).
    Once { year: u16, month: u8, day: u8 },
}

impl Repeat {
    /// Stable discriminator string - the server uses this as its SQLite
    /// `repeat_kind` column value, so it must not change without a
    /// migration on that side.
    pub const fn kind(&self) -> &'static str {
        match self {
            Repeat::Daily => "daily",
            Repeat::Weekly { .. } => "weekly",
            Repeat::Monthly { .. } => "monthly",
            Repeat::Once { .. } => "once",
        }
    }

    /// Whether this schedule covers the given calendar date. `weekday`
    /// is 0=Sunday..6=Saturday.
    pub fn fires_on(&self, year: u16, month: u8, day: u8, weekday: u8) -> bool {
        match self {
            Repeat::Daily => true,
            Repeat::Weekly { days } => days.contains(&weekday),
            Repeat::Monthly { days } => days.contains(&day),
            Repeat::Once {
                year: y,
                month: m,
                day: d,
            } => *y == year && *m == month && *d == day,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StoredAlarm {
    pub id: u8,
    pub hour: u8,
    pub minute: u8,
    pub repeat: Repeat,
    pub enabled: bool,
    pub label: String,
}

/// A calendar date: year, month, day and its computed weekday
/// (0=Sunday..6=Saturday). Returned by `next_matching_day`; callers that
/// need an occurrence's time-of-day pair it with their own hour/minute.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CalDate {
    pub year: u16,
    pub month: u8,
    pub day: u8,
    pub weekday: u8,
}

/// The next calendar date at or after `from` whose schedule matches
/// `repeat`, found by scanning day-by-day (the shared implementation behind
/// `next_occurrence_date` and `minutes_until`'s Weekly/Monthly branches -
/// before this function existed each caller carried its own copy of this
/// scan, and a duplicated date-math bug in one copy is exactly the class of
/// error this refactor prevents.
///
/// The scan runs for `max_days` consecutive days starting at `from`
/// (inclusive). `None` means no matching day within the window; non-empty
/// schedules always match within ~62 days, so with the default `370` the
/// fallback is unreachable in practice for a validated schedule. Whether a
/// match on the very first day is acceptable is the caller's decision: a
/// caller whose occurrence time for today has already passed should start
/// `from` at tomorrow rather than post-filtering the result.
pub fn next_matching_day(repeat: &Repeat, from: CalDate, max_days: u32) -> Option<CalDate> {
    let from_days = days_since_epoch(from.year, from.month, from.day);
    for offset in 0..max_days {
        let days = from_days + offset as i64;
        let (year, month, day) = date_from_days(days);
        let weekday = weekday_from_days(days);
        if repeat.fires_on(year, month, day, weekday) {
            return Some(CalDate {
                year,
                month,
                day,
                weekday,
            });
        }
    }
    None
}

/// The next calendar date (year, month, day, weekday) that `repeat` covers,
/// at or after `now`'s date. Non-empty schedules always match within ~62
/// days, so the scan is bounded and the fallback is unreachable in practice.
pub fn next_occurrence_date(
    repeat: &Repeat,
    hour: u8,
    minute: u8,
    now: &DateTime,
) -> (u16, u8, u8, u8) {
    let occurrence_minutes = hour as i64 * 60 + minute as i64;
    let now_minutes = now.hour as i64 * 60 + now.minute as i64;
    let from_days = days_since_epoch(now.year, now.month, now.day)
        + if occurrence_minutes > now_minutes {
            0
        } else {
            1
        };
    let (year, month, day) = date_from_days(from_days);
    let from = CalDate {
        year,
        month,
        day,
        weekday: weekday_from_days(from_days),
    };
    match next_matching_day(repeat, from, 370) {
        Some(d) => (d.year, d.month, d.day, d.weekday),
        None => (now.year, now.month, now.day, now.weekday),
    }
}

/// Whole days from `now` until the given calendar date (0 = today,
/// negative = already passed). Used by the Home screen for the
/// "in N days" alarm countdown.
pub fn days_until(year: u16, month: u8, day: u8, now: &DateTime) -> i64 {
    days_since_epoch(year, month, day) - days_since_epoch(now.year, now.month, now.day)
}

/// Minutes from `now` until `alarm` next fires, or `i64::MAX` if it's a
/// `Once` alarm whose date has already passed (a live store shouldn't have
/// these - `main.rs` drops fired one-shots - but `next_due` stays correct
/// even if one lingers).
fn minutes_until(alarm: &StoredAlarm, now: &DateTime) -> i64 {
    let now_minutes = now.hour as i64 * 60 + now.minute as i64;
    let alarm_minutes = alarm.hour as i64 * 60 + alarm.minute as i64;
    match &alarm.repeat {
        Repeat::Daily => {
            let mut delta = alarm_minutes - now_minutes;
            if delta < 0 {
                delta += 24 * 60;
            }
            delta
        }
        Repeat::Weekly { .. } | Repeat::Monthly { .. } => {
            let from_days = days_since_epoch(now.year, now.month, now.day)
                + if alarm_minutes > now_minutes { 0 } else { 1 };
            let (year, month, day) = date_from_days(from_days);
            let from = CalDate {
                year,
                month,
                day,
                weekday: weekday_from_days(from_days),
            };
            let Some(next) = next_matching_day(&alarm.repeat, from, 370) else {
                return i64::MAX;
            };
            let next_days = days_since_epoch(next.year, next.month, next.day);
            let offset = next_days - days_since_epoch(now.year, now.month, now.day);
            offset * 24 * 60 + (alarm_minutes - now_minutes)
        }
        Repeat::Once { year, month, day } => {
            let now_days = days_since_epoch(now.year, now.month, now.day);
            let alarm_days = days_since_epoch(*year, *month, *day);
            let delta = (alarm_days - now_days) * 24 * 60 + (alarm_minutes - now_minutes);
            if delta < 0 {
                i64::MAX
            } else {
                delta
            }
        }
    }
}

/// First unused wire-compatible id. Searching gaps avoids overflow at 255 and
/// prevents the duplicate id that a wrapping `max + 1` would create.
pub fn next_id(alarms: &[StoredAlarm]) -> Option<u8> {
    (u8::MIN..=u8::MAX).find(|candidate| !alarms.iter().any(|a| a.id == *candidate))
}

/// True for a `Once` alarm whose date has already passed relative to `now`
/// - i.e. it fired (or was skipped over) and should be dropped from the
///   store so it doesn't linger and confuse `next_due` forever. Daily alarms
///   recur on their own and are never "expired".
pub fn is_expired_once(alarm: &StoredAlarm, now: &DateTime) -> bool {
    matches!(alarm.repeat, Repeat::Once { .. })
        && matches!(minutes_until(alarm, now), i64::MAX | ..=0)
}

/// Picks the chronologically nearest enabled alarm relative to `now`.
pub fn next_due<'a>(alarms: &'a [StoredAlarm], now: &DateTime) -> Option<&'a StoredAlarm> {
    alarms
        .iter()
        .filter(|a| a.enabled)
        .min_by_key(|a| minutes_until(a, now))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dt(year: u16, month: u8, day: u8, hour: u8, minute: u8) -> DateTime {
        let mut d = DateTime {
            year,
            month,
            day,
            hour,
            minute,
            ..Default::default()
        };
        d.weekday = weekday_from_days(days_since_epoch(year, month, day));
        d
    }

    fn alarm(id: u8, hour: u8, minute: u8, repeat: Repeat, enabled: bool) -> StoredAlarm {
        StoredAlarm {
            id,
            hour,
            minute,
            repeat,
            enabled,
            label: String::new(),
        }
    }

    #[test]
    fn next_matching_day_daily_matches_the_start_date_itself() {
        let from = CalDate {
            year: 2026,
            month: 8,
            day: 22,
            weekday: 6,
        };
        assert_eq!(
            next_matching_day(&Repeat::Daily, from, 370),
            Some(from),
            "Daily covers every day, so the scan must hit `from` itself"
        );
    }

    #[test]
    fn next_matching_day_weekly_skips_non_matching_weekdays() {
        // 2026-08-31 is a Monday; the next Tuesday (weekday 2) is 2026-09-01.
        let from = CalDate {
            year: 2026,
            month: 8,
            day: 31,
            weekday: 1,
        };
        let repeat = Repeat::Weekly { days: vec![2] };
        let next = next_matching_day(&repeat, from, 370).expect("a Tuesday exists");
        assert_eq!((next.year, next.month, next.day), (2026, 9, 1));
        assert_eq!(next.weekday, 2);
    }

    #[test]
    fn next_matching_day_monthly_wraps_across_year_boundary() {
        // Only the 5th; from December 10th the next hit is January 5th of
        // next year.
        let from = CalDate {
            year: 2026,
            month: 12,
            day: 10,
            weekday: 4,
        };
        let repeat = Repeat::Monthly { days: vec![5] };
        let next = next_matching_day(&repeat, from, 370).expect("a 5th exists");
        assert_eq!((next.year, next.month, next.day), (2027, 1, 5));
    }

    #[test]
    fn next_matching_day_monthly_never_clamps_to_a_nonexistent_day() {
        // Day 29 in a non-leap February doesn't exist; the scan must skip
        // the whole month and land on the next real 29th (March), never a
        // clamped 28th.
        let from = CalDate {
            year: 2026,
            month: 2,
            day: 1,
            weekday: 0,
        };
        let repeat = Repeat::Monthly { days: vec![29] };
        let next = next_matching_day(&repeat, from, 370).expect("a 29th exists");
        assert_eq!((next.year, next.month, next.day), (2026, 3, 29));
    }

    #[test]
    fn next_matching_day_once_matches_only_its_exact_date() {
        let from = CalDate {
            year: 2026,
            month: 8,
            day: 22,
            weekday: 6,
        };
        let today = Repeat::Once {
            year: 2026,
            month: 8,
            day: 22,
        };
        assert_eq!(
            next_matching_day(&today, from, 370),
            Some(from),
            "a One-shot on the start date matches the start date itself"
        );
        let tomorrow = Repeat::Once {
            year: 2026,
            month: 8,
            day: 23,
        };
        let next = next_matching_day(&tomorrow, from, 370).expect("tomorrow is within the window");
        assert_eq!((next.year, next.month, next.day), (2026, 8, 23));
        let yesterday = Repeat::Once {
            year: 2026,
            month: 8,
            day: 21,
        };
        assert_eq!(
            next_matching_day(&yesterday, from, 370),
            None,
            "a One-shot behind the start date must not be found"
        );
    }

    #[test]
    fn next_matching_day_respects_a_short_max_days_window() {
        let from = CalDate {
            year: 2026,
            month: 8,
            day: 22,
            weekday: 6,
        };
        let repeat = Repeat::Weekly { days: vec![2] }; // next Tuesday: Aug 25
        assert_eq!(
            next_matching_day(&repeat, from, 3),
            None,
            "3 days from Saturday reaches only Sun/Mon - no Tuesday"
        );
        assert_eq!(
            next_matching_day(&repeat, from, 4),
            Some(CalDate {
                year: 2026,
                month: 8,
                day: 25,
                weekday: 2,
            }),
            "4 days from Saturday reaches Tuesday exactly"
        );
    }

    #[test]
    fn days_since_epoch_roundtrips_through_date_from_days() {
        for (y, m, d) in [(1970, 1, 1), (2000, 2, 29), (2024, 12, 31), (2099, 1, 1)] {
            let days = days_since_epoch(y, m, d);
            assert_eq!(date_from_days(days), (y, m, d));
        }
    }

    #[test]
    fn repeat_fires_on_matches_each_variant() {
        assert!(Repeat::Daily.fires_on(2026, 8, 22, 6));
        let weekly = Repeat::Weekly { days: vec![0, 6] };
        assert!(weekly.fires_on(2026, 8, 22, 6)); // Saturday
        assert!(!weekly.fires_on(2026, 8, 24, 1)); // Monday
        let monthly = Repeat::Monthly { days: vec![1, 15] };
        assert!(monthly.fires_on(2026, 8, 15, 0));
        assert!(!monthly.fires_on(2026, 8, 16, 0));
        let once = Repeat::Once {
            year: 2026,
            month: 8,
            day: 22,
        };
        assert!(once.fires_on(2026, 8, 22, 6));
        assert!(!once.fires_on(2026, 8, 23, 0));
    }

    #[test]
    fn next_occurrence_date_daily_skips_to_tomorrow_once_time_passed() {
        let now = dt(2026, 8, 22, 10, 0);
        let (y, m, d, _) = next_occurrence_date(&Repeat::Daily, 9, 0, &now);
        assert_eq!((y, m, d), (2026, 8, 23));
    }

    #[test]
    fn next_occurrence_date_daily_same_day_when_time_not_yet_passed() {
        let now = dt(2026, 8, 22, 10, 0);
        let (y, m, d, _) = next_occurrence_date(&Repeat::Daily, 11, 0, &now);
        assert_eq!((y, m, d), (2026, 8, 22));
    }

    #[test]
    fn next_occurrence_date_weekly_wraps_across_month_boundary() {
        // 2026-08-31 is a Monday; asking for the next Tuesday (weekday 2)
        // must cross into September.
        let now = dt(2026, 8, 31, 0, 0);
        let repeat = Repeat::Weekly { days: vec![2] };
        let (y, m, d, weekday) = next_occurrence_date(&repeat, 8, 0, &now);
        assert_eq!((y, m, d), (2026, 9, 1));
        assert_eq!(weekday, 2);
    }

    #[test]
    fn next_occurrence_date_monthly_wraps_across_year_boundary() {
        // Only the 5th; from December 10th the next hit is January 5th of
        // next year.
        let now = dt(2026, 12, 10, 0, 0);
        let repeat = Repeat::Monthly { days: vec![5] };
        let (y, m, d, _) = next_occurrence_date(&repeat, 8, 0, &now);
        assert_eq!((y, m, d), (2027, 1, 5));
    }

    #[test]
    fn next_occurrence_date_monthly_skips_feb_29_on_non_leap_years() {
        // Day 29 only exists in February on a leap year; from 2026 (not
        // leap) this must land on the leap year 2028, not silently clamp.
        let now = dt(2026, 2, 1, 0, 0);
        let repeat = Repeat::Monthly { days: vec![29] };
        let (y, m, d, _) = next_occurrence_date(&repeat, 0, 0, &now);
        assert_eq!((y, m, d), (2026, 3, 29));
        // A year that *does* have a Feb 29 still fires on day 29 of every
        // other month first, in date order - March 29 comes before the
        // "day 29" that happens to be Feb 29 the following February.
        let now_leap = dt(2028, 3, 1, 0, 0);
        let (y2, m2, d2, _) = next_occurrence_date(&repeat, 0, 0, &now_leap);
        assert_eq!((y2, m2, d2), (2028, 3, 29));
    }

    #[test]
    fn days_until_is_zero_for_today_and_negative_for_the_past() {
        let now = dt(2026, 8, 22, 0, 0);
        assert_eq!(days_until(2026, 8, 22, &now), 0);
        assert_eq!(days_until(2026, 8, 21, &now), -1);
        assert_eq!(days_until(2026, 8, 23, &now), 1);
    }

    #[test]
    fn next_id_fills_the_first_gap_not_max_plus_one() {
        let alarms = vec![
            alarm(0, 0, 0, Repeat::Daily, true),
            alarm(2, 0, 0, Repeat::Daily, true),
        ];
        assert_eq!(next_id(&alarms), Some(1));
    }

    #[test]
    fn next_id_none_when_all_256_ids_are_taken() {
        let alarms: Vec<StoredAlarm> = (0..=255u8)
            .map(|id| alarm(id, 0, 0, Repeat::Daily, true))
            .collect();
        assert_eq!(next_id(&alarms), None);
    }

    #[test]
    fn is_expired_once_true_only_for_a_past_one_shot() {
        let now = dt(2026, 8, 22, 12, 0);
        let past = alarm(
            0,
            9,
            0,
            Repeat::Once {
                year: 2026,
                month: 8,
                day: 22,
            },
            true,
        );
        assert!(is_expired_once(&past, &now));
        let future = alarm(
            1,
            13,
            0,
            Repeat::Once {
                year: 2026,
                month: 8,
                day: 22,
            },
            true,
        );
        assert!(!is_expired_once(&future, &now));
        let daily = alarm(2, 9, 0, Repeat::Daily, true);
        assert!(!is_expired_once(&daily, &now));
    }

    #[test]
    fn next_due_ignores_disabled_and_picks_soonest() {
        let now = dt(2026, 8, 22, 10, 0);
        let alarms = vec![
            alarm(0, 23, 0, Repeat::Daily, true),  // 13h away
            alarm(1, 10, 30, Repeat::Daily, true), // 30m away - soonest
            alarm(2, 10, 5, Repeat::Daily, false), // sooner but disabled
        ];
        let due = next_due(&alarms, &now).expect("an enabled alarm exists");
        assert_eq!(due.id, 1);
    }

    #[test]
    fn next_due_none_when_nothing_enabled() {
        let now = dt(2026, 8, 22, 10, 0);
        let alarms = vec![alarm(0, 10, 30, Repeat::Daily, false)];
        assert!(next_due(&alarms, &now).is_none());
    }
}
