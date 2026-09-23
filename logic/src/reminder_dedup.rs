use crate::datetime::DateTime;
use crate::inbox_item::InboxItem;
use crate::todo::{Importance, Todo};

pub fn reminder_date_key(now: &DateTime) -> String {
    format!("{:04}{:02}{:02}", now.year, now.month, now.day)
}

pub fn already_reminded_today(prev: Option<&str>, now: &DateTime) -> bool {
    prev == Some(reminder_date_key(now).as_str())
}

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

pub fn merge_pending_read(pending: &[u64], new_items: &[InboxItem]) -> Vec<u64> {
    pending
        .iter()
        .copied()
        .filter(|seq| new_items.iter().any(|it| it.id == *seq && !it.read))
        .collect()
}

pub fn apply_pending_read(items: &mut [InboxItem], pending: &[u64]) {
    for item in items.iter_mut() {
        if pending.contains(&item.id) {
            item.read = true;
        }
    }
}

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
            todo(1, Importance::High, false, Some(due)),
            todo(2, Importance::High, true, Some(due)),
            todo(3, Importance::Medium, false, Some(due)),
            todo(4, Importance::High, false, None),
        ];
        let result = due_high_importance_todos(&todos, &now);
        assert_eq!(result.iter().map(|t| t.id).collect::<Vec<_>>(), vec![1]);
    }

    #[test]
    fn due_high_importance_todos_honors_recurrence_over_due_date() {
        use crate::alarm_schedule::Repeat;
        let now = dt(2026, 8, 22, 6);
        let mut recurring = todo(1, Importance::High, false, None);
        recurring.repeat = Some(Repeat::Weekly { days: vec![6] });
        assert_eq!(
            due_high_importance_todos(&[recurring.clone()], &now).len(),
            1
        );
        recurring.repeat = Some(Repeat::Weekly { days: vec![1] });
        assert!(due_high_importance_todos(&[recurring], &now).is_empty());
    }

    #[test]
    fn merge_pending_read_keeps_only_still_unread_items() {
        let pending = vec![1, 2, 3];
        let new_items = vec![item(1, false), item(2, true)];
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
