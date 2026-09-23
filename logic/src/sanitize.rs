use crate::alarm_schedule::{Repeat, StoredAlarm};
use crate::datetime::{days_in_month, is_leap};
use crate::todo::Todo;

pub fn valid_date(year: u16, month: u8, day: u8) -> bool {
    if !(2000..=2099).contains(&year) || !(1..=12).contains(&month) {
        return false;
    }
    let dim = if month == 2 && is_leap(year as i64) {
        29
    } else {
        days_in_month(year, month)
    };
    (1..=dim).contains(&day)
}

fn sanitize_repeat(repeat: &mut Repeat) {
    match repeat {
        Repeat::Daily => {}

        Repeat::Weekly { days } => {
            days.retain(|day| *day <= 6);
            days.sort_unstable();
            days.dedup();
        }

        Repeat::Monthly { days } => {
            days.retain(|day| (1..=31).contains(day));
            days.sort_unstable();
            days.dedup();
        }

        Repeat::Once { year, month, day } => {
            if valid_date(*year, *month, *day) {
                return;
            }

            *month = (*month).clamp(1, 12);
            let dim = days_in_month(*year, *month).max(1);
            *day = (*day).clamp(1, dim);
        }
    }
}

pub fn sanitize_alarms(alarms: &mut [StoredAlarm]) {
    for alarm in alarms {
        alarm.hour = alarm.hour.min(23);
        alarm.minute = alarm.minute.min(59);
        sanitize_repeat(&mut alarm.repeat);
    }
}

pub fn sanitize_todos(todos: &mut [Todo]) {
    for todo in todos {
        if let Some(due) = &todo.due_date {
            if !valid_date(due.year, due.month, due.day) {
                todo.due_date = None;
            }
        }
        if let Some(repeat) = &mut todo.repeat {
            if matches!(repeat, Repeat::Once { .. }) {
                todo.repeat = None;
                continue;
            }
            sanitize_repeat(repeat);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn alarm(hour: u8, minute: u8, repeat: Repeat) -> StoredAlarm {
        StoredAlarm {
            id: 1,
            hour,
            minute,
            repeat,
            enabled: true,
            label: String::new(),
        }
    }

    #[test]
    fn sanitize_clamps_impossible_alarm_time() {
        let mut alarms = vec![alarm(99, 99, Repeat::Daily)];
        sanitize_alarms(&mut alarms);
        assert_eq!((alarms[0].hour, alarms[0].minute), (23, 59));
    }

    #[test]
    fn sanitize_drops_out_of_range_weekly_days() {
        let mut alarms = vec![alarm(
            9,
            0,
            Repeat::Weekly {
                days: vec![0, 8, 6, 6, 255],
            },
        )];
        sanitize_alarms(&mut alarms);
        assert_eq!(alarms[0].repeat, Repeat::Weekly { days: vec![0, 6] });
    }

    #[test]
    fn sanitize_drops_out_of_range_monthly_days() {
        let mut alarms = vec![alarm(
            9,
            0,
            Repeat::Monthly {
                days: vec![0, 15, 32, 15],
            },
        )];
        sanitize_alarms(&mut alarms);
        assert_eq!(alarms[0].repeat, Repeat::Monthly { days: vec![15] });
    }

    #[test]
    fn sanitize_clamps_once_repeat_into_a_real_date() {
        let mut alarms = vec![alarm(
            9,
            0,
            Repeat::Once {
                year: 2026,
                month: 0,
                day: 0,
            },
        )];
        sanitize_alarms(&mut alarms);
        assert_eq!(
            alarms[0].repeat,
            Repeat::Once {
                year: 2026,
                month: 1,
                day: 1
            }
        );
    }

    #[test]
    fn sanitize_leaves_valid_alarms_untouched() {
        let mut alarms = vec![alarm(
            6,
            30,
            Repeat::Weekly {
                days: vec![1, 3, 5],
            },
        )];
        let expected = alarms.clone();
        sanitize_alarms(&mut alarms);
        assert_eq!(alarms, expected);
    }

    #[test]
    fn sanitize_clears_an_impossible_todo_due_date() {
        let mut todos = vec![Todo {
            id: 1,
            text: "ship it".to_string(),
            done: false,
            importance: Default::default(),
            due_date: Some(crate::todo::TodoDue {
                year: 2026,
                month: 2,
                day: 30,
            }),
            repeat: None,
        }];
        sanitize_todos(&mut todos);
        assert!(todos[0].due_date.is_none());
    }

    #[test]
    fn sanitize_clears_the_unsupported_once_repeat_on_todos() {
        let mut todos = vec![Todo {
            id: 1,
            text: "ship it".to_string(),
            done: false,
            importance: Default::default(),
            due_date: None,
            repeat: Some(Repeat::Once {
                year: 2026,
                month: 1,
                day: 1,
            }),
        }];
        sanitize_todos(&mut todos);
        assert!(todos[0].repeat.is_none());
    }

    #[test]
    fn valid_date_accepts_leap_day_only_in_leap_years() {
        assert!(valid_date(2024, 2, 29));
        assert!(!valid_date(2023, 2, 29));
        assert!(!valid_date(2026, 0, 1));
        assert!(!valid_date(2026, 13, 1));
    }
}
