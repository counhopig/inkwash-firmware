//! Runtime reminder fact collection.
//!
//! Store reads happen on the effect worker. The application loop receives an
//! owned `ReminderPayload` as an effect completion and never reads NVS while
//! collecting input or rendering.

use anyhow::Result;

use crate::inbox::InboxStore;
use crate::rtc::DateTime;
use crate::storage::PersistedCounters;
use crate::todos::TodoStore;

pub fn collect(
    inbox_store: &InboxStore,
    todo_store: &TodoStore,
    counters: &PersistedCounters,
    now: &DateTime,
) -> Result<Option<inkwash_logic::app::ReminderPayload>> {
    use inkwash_logic::app::{ReminderKind, ReminderPayload};
    use inkwash_logic::reminder_dedup::{
        already_reminded_today, due_high_importance_todos, reminder_date_key,
    };

    let list = inbox_store.load()?;
    let urgent = inbox_store.unread_urgent()?;
    if !urgent.is_empty() {
        let lines: Vec<String> = list
            .iter()
            .filter(|item| urgent.contains(&item.id))
            .map(|item| crate::screens::truncate_prop(&item.title, 330))
            .collect();
        if !lines.is_empty() {
            return Ok(Some(ReminderPayload {
                kind: ReminderKind::Urgent,
                lines,
                urgent_read_ids: urgent,
                todo_date: None,
            }));
        }
    }

    if let Some(previous) = counters.todo_reminded_date()? {
        if already_reminded_today(Some(previous.as_str()), now) {
            return Ok(None);
        }
    }
    let todos = todo_store.load()?;
    let due = due_high_importance_todos(&todos, now);
    if due.is_empty() {
        return Ok(None);
    }
    let lines = due
        .iter()
        .map(|todo| crate::screens::truncate_prop(&todo.text, 300))
        .collect();
    Ok(Some(ReminderPayload {
        kind: ReminderKind::Todo,
        lines,
        urgent_read_ids: vec![],
        todo_date: Some(reminder_date_key(now)),
    }))
}
