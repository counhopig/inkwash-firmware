//! Reminder de-duplication rules, factored out of `rust-firmware/src/
//! reminders.rs` and `rust-firmware/src/inbox.rs` (both call into this
//! instead of inlining the logic) so the "don't remind twice"/"don't lose a
//! pending-read ack across a resync" invariants are covered by host tests
//! instead of only ever being exercised on a physical device.

use crate::datetime::DateTime;
use crate::inbox_item::InboxItem;
use crate::todo::{Importance, Todo};

/// The per-day key `reminders.rs` stores in NVS (`todo_reminded_date`) to
/// track whether today's due-todo reminder has already fired.
pub fn reminder_date_key(now: &DateTime) -> String {
    format!("{:04}{:02}{:02}", now.year, now.month, now.day)
}

/// Whether today's due-todo reminder has already been shown, given the
/// date key persisted after the last time it fired (`None` if it has never
/// fired).
pub fn already_reminded_today(prev: Option<&str>, now: &DateTime) -> bool {
    prev == Some(reminder_date_key(now).as_str())
}

/// High-importance, not-yet-done todos due today - either by an explicit
/// `due_date` match, or because a recurrence `repeat` covers today. This is
/// the "what would remind" half of the due-todo reminder; `reminders.rs`
/// pairs it with `already_reminded_today` before actually showing anything.
pub fn due_high_importance_todos<'a>(todos: &'a [Todo], now: &DateTime) -> Vec<&'a Todo> {
    todos
        .iter()
        .filter(|todo| {
            if todo.done || todo.importance != Importance::High {
                return false;
            }
            match &todo.repeat {
                Some(repeat) => repeat.fires_on(now.year, now.month, now.day, now.weekday),
                None => todo.due_date.is_some_and(|date| {
                    date.year == now.year && date.month == now.month && date.day == now.day
                }),
            }
        })
        .collect()
}

/// Recomputes the locally-pending-read set after replacing the inbox with a
/// fresh server list (`InboxStore::save`): a `seq` stays pending only if the
/// server still lists it and still shows it as unread - if the server has
/// already applied the ack (item now `read`, or dropped entirely), the
/// pending entry has served its purpose and is dropped so it isn't
/// re-uploaded forever.
pub fn merge_pending_read(pending: &[u64], new_items: &[InboxItem]) -> Vec<u64> {
    pending
        .iter()
        .copied()
        .filter(|seq| new_items.iter().any(|it| it.id == *seq && !it.read))
        .collect()
}

/// Marks every item in `pending` as locally read, so a full replace
/// (`InboxStore::save`) doesn't un-read something the user already opened
/// this session, just because the server hasn't caught up to the ack yet.
pub fn apply_pending_read(items: &mut [InboxItem], pending: &[u64]) {
    for item in items.iter_mut() {
        if pending.contains(&item.id) {
            item.read = true;
        }
    }
}

/// Drops every `seq` the server has now acknowledged (`inbox_read_acked`)
/// from the locally-pending-read set.
pub fn ack_pending_read(pending: &[u64], acked: &[u64]) -> Vec<u64> {
    pending
        .iter()
        .copied()
        .filter(|seq| !acked.contains(seq))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inbox_item::{InboxKind, Priority};
    use crate::todo::TodoDue;

    fn dt(year: u16, month: u8, day: u8, weekday: u8) -> DateTime {
        DateTime {
            year,
            month,
            day,
            weekday,
            ..Default::default()
        }
    }

    fn todo(id: u8, importance: Importance, done: bool, due: Option<TodoDue>) -> Todo {
        Todo {
            id,
            text: String::new(),
            done,
            importance,
            due_date: due,
            repeat: None,
        }
    }

    fn item(id: u64, read: bool) -> InboxItem {
        InboxItem {
            id,
            kind: InboxKind::Alert,
            priority: Priority::High,
            title: String::new(),
            body: String::new(),
            when: None,
            read,
        }
    }

    #[test]
    fn already_reminded_today_false_when_never_reminded() {
        let now = dt(2026, 8, 22, 6);
        assert!(!already_reminded_today(None, &now));
    }

    #[test]
    fn already_reminded_today_true_for_matching_date_key_only() {
        let now = dt(2026, 8, 22, 6);
        assert!(already_reminded_today(Some("20260822"), &now));
        assert!(!already_reminded_today(Some("20260821"), &now));
    }

    #[test]
    fn due_high_importance_todos_excludes_done_and_low_importance() {
        let now = dt(2026, 8, 22, 6);
        let due = TodoDue {
            year: 2026,
            month: 8,
            day: 22,
        };
        let todos = vec![
            todo(1, Importance::High, false, Some(due)), // due today, matches
            todo(2, Importance::High, true, Some(due)),  // done - excluded
            todo(3, Importance::Medium, false, Some(due)), // not High - excluded
            todo(4, Importance::High, false, None),      // no due date - excluded
        ];
        let result = due_high_importance_todos(&todos, &now);
        assert_eq!(result.iter().map(|t| t.id).collect::<Vec<_>>(), vec![1]);
    }

    #[test]
    fn due_high_importance_todos_honors_recurrence_over_due_date() {
        use crate::alarm_schedule::Repeat;
        let now = dt(2026, 8, 22, 6); // Saturday
        let mut recurring = todo(1, Importance::High, false, None);
        recurring.repeat = Some(Repeat::Weekly { days: vec![6] });
        assert_eq!(
            due_high_importance_todos(&[recurring.clone()], &now).len(),
            1
        );
        recurring.repeat = Some(Repeat::Weekly { days: vec![1] }); // Monday only
        assert!(due_high_importance_todos(&[recurring], &now).is_empty());
    }

    #[test]
    fn merge_pending_read_keeps_only_still_unread_items() {
        let pending = vec![1, 2, 3];
        let new_items = vec![
            item(1, false), // still unread - kept
            item(2, true),  // server applied the ack - dropped
                            // 3 no longer present at all - dropped
        ];
        let mut result = merge_pending_read(&pending, &new_items);
        result.sort();
        assert_eq!(result, vec![1]);
    }

    #[test]
    fn apply_pending_read_marks_matching_items_read() {
        let mut items = vec![item(1, false), item(2, false)];
        apply_pending_read(&mut items, &[2]);
        assert!(!items[0].read);
        assert!(items[1].read);
    }

    #[test]
    fn ack_pending_read_drops_acknowledged_seqs_only() {
        let pending = vec![1, 2, 3];
        let mut result = ack_pending_read(&pending, &[2]);
        result.sort();
        assert_eq!(result, vec![1, 3]);
    }
}
