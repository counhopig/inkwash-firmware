use anyhow::Result;
use inkwash_logic::app::SyncedData;

use crate::alarms::AlarmStore;
use crate::inbox::InboxStore;
use crate::storage::PersistedCounters;
use crate::todos::TodoStore;

pub fn apply(
    data: &SyncedData,
    counters: &PersistedCounters,
    alarms: &AlarmStore,
    todos: &TodoStore,
    inbox: &InboxStore,
) -> Result<()> {
    counters.save_sync_apply_journal(data)?;
    replay(data, alarms, todos, inbox)?;
    counters.clear_sync_apply_journal()
}

pub fn recover(
    counters: &PersistedCounters,
    alarms: &AlarmStore,
    todos: &TodoStore,
    inbox: &InboxStore,
) -> Result<bool> {
    let Some(data) = counters.sync_apply_journal()? else {
        return Ok(false);
    };
    replay(&data, alarms, todos, inbox)?;
    counters.clear_sync_apply_journal()?;
    Ok(true)
}

fn replay(
    data: &SyncedData,
    alarms: &AlarmStore,
    todos: &TodoStore,
    inbox: &InboxStore,
) -> Result<()> {
    alarms.save(&data.alarms)?;
    todos.save(&data.todos)?;
    inbox.save(&data.inbox)?;
    inbox.ack_read(&data.inbox_read_acked)?;
    alarms.clear_dirty_ids(&data.uploaded_alarm_ids)?;
    todos.clear_dirty_ids(&data.uploaded_todo_ids)?;
    Ok(())
}
