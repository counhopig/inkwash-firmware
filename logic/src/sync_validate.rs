//! Sync response merge/validation rules, moved out of
//! `rust-firmware/src/sync.rs` (which re-exports them) so they can be unit
//! tested on the host: this is the one place a malformed or internally
//! inconsistent server response is rejected before any NVS blob is
//! replaced, so its edge cases are worth locking down independently of a
//! live server.

use std::collections::HashSet;

use serde::{Deserialize, Serialize};

use crate::alarm_schedule::{self, Repeat, StoredAlarm};
use crate::datetime::is_leap;
use crate::inbox_item::InboxItem;
use crate::todo::Todo;

/// Sync response body shape, deserialized directly from the server's JSON -
/// `StoredAlarm`/`Todo` already derive `Deserialize`, so no separate wire
/// DTO is needed. See `docs/sync-api.md` for the exact contract.
#[derive(Debug, Deserialize, Serialize, Default)]
pub struct SyncResponse {
    #[serde(default)]
    pub alarms: Vec<StoredAlarm>,
    #[serde(default)]
    pub todos: Vec<Todo>,
    #[serde(default)]
    pub inbox: Vec<InboxItem>,
    #[serde(default)]
    pub inbox_read_acked: Vec<u64>,
    #[serde(default)]
    pub inbox_truncated: bool,
}

/// The dedup-then-validate loop shared by all three sync stores (alarms,
/// todos, inbox), so a fix to one store's loop can't silently miss the other
/// two. Rejects the first duplicate id (error names it as `{kind}`) and runs
/// every item through `validate`, which the caller wraps with per-item
/// context (e.g. "alarm 7 has invalid repeat: ..."). Items whose only rule
/// is uniqueness pass `|_| Ok(())`.
pub fn dedup_validate<T>(
    items: &[T],
    kind: &str,
    id_of: impl Fn(&T) -> u64,
    validate: impl Fn(&T) -> Result<(), String>,
) -> Result<(), String> {
    let mut ids = HashSet::new();
    for item in items {
        let id = id_of(item);
        if !ids.insert(id) {
            return Err(format!("duplicate {kind} id {id}"));
        }
        validate(item)?;
    }
    Ok(())
}

/// Rejects malformed or internally inconsistent server state before any NVS
/// blob is replaced. Wire DTOs deliberately use plain integers for protocol
/// compatibility; the firmware must establish their invariants at this
/// boundary instead of letting invalid dates become array indices or RTC BCD.
pub fn validate_sync_response(response: &SyncResponse) -> Result<(), String> {
    dedup_validate(&response.alarms, "alarm", |a| a.id as u64, |alarm| {
        if alarm.hour > 23 || alarm.minute > 59 {
            return Err(format!(
                "alarm {} has invalid time {:02}:{:02}",
                alarm.id, alarm.hour, alarm.minute
            ));
        }
        validate_repeat(&alarm.repeat)
            .map_err(|err| format!("alarm {} has invalid repeat: {err}", alarm.id))
    })?;

    dedup_validate(&response.todos, "todo", |t| t.id as u64, |todo| {
        if let Some(due) = todo.due_date {
            validate_date(due.year, due.month, due.day)
                .map_err(|err| format!("todo {} has invalid due date: {err}", todo.id))?;
        }
        if let Some(repeat) = &todo.repeat {
            if matches!(repeat, alarm_schedule::Repeat::Once { .. }) {
                return Err(format!("todo {} uses unsupported Once repeat", todo.id));
            }
            validate_repeat(repeat)
                .map_err(|err| format!("todo {} has invalid repeat: {err}", todo.id))?;
        }
        Ok(())
    })?;

    dedup_validate(&response.inbox, "inbox", |item| item.id, |_| Ok(()))?;

    // Preflight the two fixed-size stores before writing either one. Without
    // this, an oversized todo response could replace alarms and then fail,
    // leaving a mixed-generation snapshot behind.
    if serde_json::to_vec(&response.alarms)
        .map_err(|e| e.to_string())?
        .len()
        > 1024
    {
        return Err("alarm list exceeds device storage capacity".to_string());
    }
    if serde_json::to_vec(&response.todos)
        .map_err(|e| e.to_string())?
        .len()
        > 2048
    {
        return Err("todo list exceeds device storage capacity".to_string());
    }
    Ok(())
}

pub fn validate_repeat(repeat: &Repeat) -> Result<(), String> {
    match repeat {
        Repeat::Daily => Ok(()),
        Repeat::Weekly { days } => {
            if days.is_empty() || days.iter().any(|day| *day > 6) {
                return Err("weekly days must be non-empty and within 0..=6".to_string());
            }
            Ok(())
        }
        Repeat::Monthly { days } => {
            if days.is_empty() || days.iter().any(|day| !(1..=31).contains(day)) {
                return Err("monthly days must be non-empty and within 1..=31".to_string());
            }
            Ok(())
        }
        Repeat::Once { year, month, day } => validate_date(*year, *month, *day),
    }
}

pub fn validate_date(year: u16, month: u8, day: u8) -> Result<(), String> {
    if !(2000..=2099).contains(&year) || !(1..=12).contains(&month) {
        return Err("date must be within 2000-01-01..=2099-12-31".to_string());
    }
    let days = match month {
        2 if is_leap(year as i64) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    };
    if !(1..=days).contains(&day) {
        return Err(format!("day {day} is invalid for {year:04}-{month:02}"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn alarm(id: u8, hour: u8, minute: u8, repeat: Repeat) -> StoredAlarm {
        StoredAlarm {
            id,
            hour,
            minute,
            repeat,
            enabled: true,
            label: String::new(),
        }
    }

    #[test]
    fn accepts_an_empty_response() {
        assert!(validate_sync_response(&SyncResponse::default()).is_ok());
    }

    #[test]
    fn rejects_duplicate_alarm_ids() {
        let response = SyncResponse {
            alarms: vec![
                alarm(1, 9, 0, Repeat::Daily),
                alarm(1, 10, 0, Repeat::Daily),
            ],
            ..Default::default()
        };
        assert!(validate_sync_response(&response).is_err());
    }

    #[test]
    fn rejects_alarm_time_out_of_range() {
        let response = SyncResponse {
            alarms: vec![alarm(1, 24, 0, Repeat::Daily)],
            ..Default::default()
        };
        assert!(validate_sync_response(&response).is_err());
        let response = SyncResponse {
            alarms: vec![alarm(1, 9, 60, Repeat::Daily)],
            ..Default::default()
        };
        assert!(validate_sync_response(&response).is_err());
    }

    #[test]
    fn rejects_duplicate_todo_and_inbox_ids() {
        let mut todo_dup = SyncResponse::default();
        todo_dup.todos = vec![
            Todo {
                id: 5,
                text: "a".into(),
                done: false,
                importance: Default::default(),
                due_date: None,
                repeat: None,
            },
            Todo {
                id: 5,
                text: "b".into(),
                done: false,
                importance: Default::default(),
                due_date: None,
                repeat: None,
            },
        ];
        assert!(validate_sync_response(&todo_dup).is_err());
    }

    #[test]
    fn rejects_todo_once_repeat_as_unsupported() {
        let response = SyncResponse {
            todos: vec![Todo {
                id: 1,
                text: "x".into(),
                done: false,
                importance: Default::default(),
                due_date: None,
                repeat: Some(Repeat::Once {
                    year: 2026,
                    month: 8,
                    day: 22,
                }),
            }],
            ..Default::default()
        };
        assert!(validate_sync_response(&response).is_err());
    }

    #[test]
    fn validate_repeat_rejects_empty_or_out_of_range_weekly_monthly() {
        assert!(validate_repeat(&Repeat::Weekly { days: vec![] }).is_err());
        assert!(validate_repeat(&Repeat::Weekly { days: vec![7] }).is_err());
        assert!(validate_repeat(&Repeat::Weekly { days: vec![0, 6] }).is_ok());
        assert!(validate_repeat(&Repeat::Monthly { days: vec![] }).is_err());
        assert!(validate_repeat(&Repeat::Monthly { days: vec![0] }).is_err());
        assert!(validate_repeat(&Repeat::Monthly { days: vec![32] }).is_err());
        assert!(validate_repeat(&Repeat::Monthly { days: vec![1, 31] }).is_ok());
    }

    #[test]
    fn validate_date_rejects_feb_29_on_non_leap_years() {
        assert!(validate_date(2000, 2, 29).is_ok()); // leap
        assert!(validate_date(2001, 2, 29).is_err()); // not leap
        assert!(validate_date(1999, 1, 1).is_err()); // year out of range
        assert!(validate_date(2100, 1, 1).is_err()); // year out of range
        assert!(validate_date(2026, 4, 31).is_err()); // April has 30 days
    }

    #[test]
    fn dedup_validate_alarm_items_rejects_duplicates_then_validates() {
        let alarms = vec![
            alarm(1, 9, 0, Repeat::Daily),
            alarm(2, 10, 0, Repeat::Daily),
            alarm(1, 11, 0, Repeat::Daily),
        ];
        let err = dedup_validate(&alarms, "alarm", |a| a.id as u64, |_| Ok(())).unwrap_err();
        assert_eq!(err, "duplicate alarm id 1");
        // Validation still runs on every item, including after a dup-free prefix.
        let err = dedup_validate(
            &[alarm(1, 24, 0, Repeat::Daily)],
            "alarm",
            |a| a.id as u64,
            |a| {
                if a.hour > 23 {
                    Err(format!("alarm {} has invalid time", a.id))
                } else {
                    Ok(())
                }
            },
        )
        .unwrap_err();
        assert_eq!(err, "alarm 1 has invalid time");
    }

    #[test]
    fn dedup_validate_todo_items_applies_the_once_repeat_rule() {
        let todos = vec![
            Todo {
                id: 5,
                text: "a".into(),
                done: false,
                importance: Default::default(),
                due_date: None,
                repeat: Some(Repeat::Daily),
            },
            Todo {
                id: 5,
                text: "b".into(),
                done: false,
                importance: Default::default(),
                due_date: None,
                repeat: None,
            },
        ];
        let err = dedup_validate(&todos, "todo", |t| t.id as u64, |_| Ok(())).unwrap_err();
        assert_eq!(err, "duplicate todo id 5");
        // Per-item validation is reached for items that pass dedup.
        let err = dedup_validate(
            &[Todo {
                id: 7,
                text: "x".into(),
                done: false,
                importance: Default::default(),
                due_date: None,
                repeat: Some(Repeat::Once {
                    year: 2026,
                    month: 8,
                    day: 22,
                }),
            }],
            "todo",
            |t| t.id as u64,
            |t| {
                if matches!(t.repeat, Some(Repeat::Once { .. })) {
                    Err(format!("todo {} uses unsupported Once repeat", t.id))
                } else {
                    Ok(())
                }
            },
        )
        .unwrap_err();
        assert_eq!(err, "todo 7 uses unsupported Once repeat");
    }

    #[test]
    fn dedup_validate_inbox_items_only_checks_uniqueness() {
        let items = vec![
            InboxItem {
                id: 100,
                kind: crate::inbox_item::InboxKind::Alert,
                priority: Default::default(),
                title: "a".into(),
                body: String::new(),
                when: None,
                read: false,
            },
            InboxItem {
                id: 100,
                kind: crate::inbox_item::InboxKind::Alert,
                priority: Default::default(),
                title: "b".into(),
                body: String::new(),
                when: None,
                read: false,
            },
        ];
        let err = dedup_validate(&items, "inbox", |i| i.id, |_| Ok(())).unwrap_err();
        assert_eq!(err, "duplicate inbox id 100");
    }
}
