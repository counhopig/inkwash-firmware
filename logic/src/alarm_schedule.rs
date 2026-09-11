use serde::{Deserialize, Serialize};
use std::time::Duration;

use crate::alarm_regs::AlarmRegs;
use crate::datetime::{weekday_from_days, DateTime};

pub use crate::datetime::{date_from_days, days_since_epoch};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum Repeat {
    Daily,

    Weekly { days: Vec<u8> },

    Monthly { days: Vec<u8> },

    Once { year: u16, month: u8, day: u8 },
}

impl Repeat {
    pub const fn kind(&self) -> &'static str {
        match self {
            Repeat::Daily => "daily",
            Repeat::Weekly { .. } => "weekly",
            Repeat::Monthly { .. } => "monthly",
            Repeat::Once { .. } => "once",
        }
    }

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

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredAlarm {
    pub id: u8,
    pub hour: u8,
    pub minute: u8,
    pub repeat: Repeat,
    pub enabled: bool,
    pub label: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CalDate {
    pub year: u16,
    pub month: u8,
    pub day: u8,
    pub weekday: u8,
}

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

pub fn days_until(year: u16, month: u8, day: u8, now: &DateTime) -> i64 {
    days_since_epoch(year, month, day) - days_since_epoch(now.year, now.month, now.day)
}

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

pub fn next_id(alarms: &[StoredAlarm]) -> Option<u8> {
    (u8::MIN..=u8::MAX).find(|candidate| !alarms.iter().any(|a| a.id == *candidate))
}

pub fn is_expired_once(alarm: &StoredAlarm, now: &DateTime) -> bool {
    matches!(alarm.repeat, Repeat::Once { .. })
        && matches!(minutes_until(alarm, now), i64::MAX | ..=0)
}

pub fn next_due<'a>(alarms: &'a [StoredAlarm], now: &DateTime) -> Option<&'a StoredAlarm> {
    alarms
        .iter()
        .filter(|a| a.enabled && minutes_until(a, now) != i64::MAX)
        .min_by_key(|a| minutes_until(a, now))
}

pub fn maintenance_wakeup_delay(alarms: &[StoredAlarm], now: &DateTime) -> Option<Duration> {
    let now_epoch = now.to_unix();
    alarms
        .iter()
        .filter(|alarm| alarm.enabled)
        .filter_map(|alarm| match alarm.repeat {
            Repeat::Once { year, month, .. }
                if (year, month) > (now.year, now.month) && (1..=12).contains(&month) =>
            {
                Some(
                    DateTime {
                        year,
                        month,
                        day: 1,
                        ..DateTime::default()
                    }
                    .to_unix(),
                )
            }
            _ => None,
        })
        .min()
        .map(|target_month| Duration::from_secs(target_month.saturating_sub(now_epoch + 60).max(1)))
}

pub fn alarm_regs_for(alarm: &StoredAlarm, now: &DateTime) -> Option<AlarmRegs> {
    match &alarm.repeat {
        Repeat::Daily => Some(AlarmRegs {
            minute: alarm.minute,
            hour: alarm.hour,
            day: None,
            weekday: None,
        }),
        Repeat::Weekly { .. } => {
            let (_, _, _, dow) = next_occurrence_date(&alarm.repeat, alarm.hour, alarm.minute, now);
            Some(AlarmRegs {
                minute: alarm.minute,
                hour: alarm.hour,
                day: None,
                weekday: Some(dow),
            })
        }
        Repeat::Monthly { .. } => {
            let (_, _, day_of_month, _) =
                next_occurrence_date(&alarm.repeat, alarm.hour, alarm.minute, now);
            Some(AlarmRegs {
                minute: alarm.minute,
                hour: alarm.hour,
                day: Some(day_of_month),
                weekday: None,
            })
        }
        Repeat::Once { year, month, day } => {
            if now.year == *year && now.month == *month {
                Some(AlarmRegs {
                    minute: alarm.minute,
                    hour: alarm.hour,
                    day: Some(*day),
                    weekday: None,
                })
            } else {
                None
            }
        }
    }
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
        let repeat = Repeat::Weekly { days: vec![2] };
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
        assert!(weekly.fires_on(2026, 8, 22, 6));
        assert!(!weekly.fires_on(2026, 8, 24, 1));
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
        let now = dt(2026, 8, 31, 0, 0);
        let repeat = Repeat::Weekly { days: vec![2] };
        let (y, m, d, weekday) = next_occurrence_date(&repeat, 8, 0, &now);
        assert_eq!((y, m, d), (2026, 9, 1));
        assert_eq!(weekday, 2);
    }

    #[test]
    fn next_occurrence_date_monthly_wraps_across_year_boundary() {
        let now = dt(2026, 12, 10, 0, 0);
        let repeat = Repeat::Monthly { days: vec![5] };
        let (y, m, d, _) = next_occurrence_date(&repeat, 8, 0, &now);
        assert_eq!((y, m, d), (2027, 1, 5));
    }

    #[test]
    fn next_occurrence_date_monthly_skips_feb_29_on_non_leap_years() {
        let now = dt(2026, 2, 1, 0, 0);
        let repeat = Repeat::Monthly { days: vec![29] };
        let (y, m, d, _) = next_occurrence_date(&repeat, 0, 0, &now);
        assert_eq!((y, m, d), (2026, 3, 29));

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
            alarm(0, 23, 0, Repeat::Daily, true),
            alarm(1, 10, 30, Repeat::Daily, true),
            alarm(2, 10, 5, Repeat::Daily, false),
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

    #[test]
    fn next_due_none_when_only_expired_once() {
        let now = dt(2026, 8, 22, 12, 0);
        let alarms = vec![alarm(
            0,
            9,
            0,
            Repeat::Once {
                year: 2026,
                month: 8,
                day: 22,
            },
            true,
        )];
        assert!(next_due(&alarms, &now).is_none());
    }

    #[test]
    fn next_due_ignores_expired_once_when_a_valid_alarm_exists() {
        let now = dt(2026, 8, 22, 12, 0);
        let alarms = vec![
            alarm(
                0,
                9,
                0,
                Repeat::Once {
                    year: 2026,
                    month: 8,
                    day: 22,
                },
                true,
            ),
            alarm(1, 13, 0, Repeat::Daily, true),
        ];
        let due = next_due(&alarms, &now).expect("a valid alarm exists");
        assert_eq!(due.id, 1);
    }

    #[test]
    fn alarm_regs_for_daily_leaves_day_and_weekday_unset() {
        let now = dt(2026, 8, 22, 10, 0);
        let regs = alarm_regs_for(&alarm(0, 7, 15, Repeat::Daily, true), &now)
            .expect("daily is always armable");
        assert_eq!(
            regs,
            AlarmRegs {
                minute: 15,
                hour: 7,
                day: None,
                weekday: None,
            }
        );
    }

    #[test]
    fn alarm_regs_for_weekly_arms_next_covered_weekday() {
        let now = dt(2026, 8, 22, 10, 0);
        let mon = alarm(0, 8, 0, Repeat::Weekly { days: vec![1] }, true);
        let regs = alarm_regs_for(&mon, &now).expect("weekly is armable");

        assert_eq!(
            regs,
            AlarmRegs {
                minute: 0,
                hour: 8,
                day: None,
                weekday: Some(1),
            }
        );
    }

    #[test]
    fn alarm_regs_for_monthly_arms_target_day_of_month() {
        let now = dt(2026, 8, 22, 10, 0);
        let monthly = alarm(0, 8, 0, Repeat::Monthly { days: vec![15] }, true);
        let regs = alarm_regs_for(&monthly, &now).expect("monthly armable");
        assert_eq!(
            regs,
            AlarmRegs {
                minute: 0,
                hour: 8,
                day: Some(15),
                weekday: None,
            }
        );
    }

    #[test]
    fn alarm_regs_for_once_in_target_month_arms_day() {
        let now = dt(2026, 8, 22, 10, 0);
        let once = alarm(
            0,
            8,
            0,
            Repeat::Once {
                year: 2026,
                month: 8,
                day: 25,
            },
            true,
        );
        let regs = alarm_regs_for(&once, &now).expect("in target month -> armable");
        assert_eq!(
            regs,
            AlarmRegs {
                minute: 0,
                hour: 8,
                day: Some(25),
                weekday: None,
            }
        );
    }

    #[test]
    fn alarm_regs_for_once_outside_target_month_returns_none() {
        let now = dt(2026, 8, 22, 10, 0);
        let once = alarm(
            0,
            8,
            0,
            Repeat::Once {
                year: 2026,
                month: 9,
                day: 5,
            },
            true,
        );
        assert_eq!(alarm_regs_for(&once, &now), None);
    }
}
