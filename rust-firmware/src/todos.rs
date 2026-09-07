//! Todo-list store, backed by one NVS blob - same shape as `alarms.rs` but
//! with no RTC coupling.

use anyhow::{anyhow, Result};
use esp_idf_svc::nvs::{EspDefaultNvs, EspDefaultNvsPartition};

use crate::nvs_blob::{read_blob, write_blob, DirtySet};

/// `Importance`/`TodoDue`/`Todo` live in `inkwash-logic` (re-exported here)
/// so `sync_validate`'s host tests share the exact same wire shape instead
/// of a hand-copied one that could drift.
#[allow(unused_imports)]
// TodoDue: part of Todo's public shape; no on-device code names it directly (due dates are server-authored, never constructed on-device).
pub use inkwash_logic::todo::{Importance, Todo, TodoDue};

const NAMESPACE: &str = "inkwash_todo";
const KEY_TODOS: &str = "todos";
/// Locally-changed `local_id`s pending upload (two-way sync dirty set).
const KEY_DIRTY: &str = "dirty";
const BLOB_BUF_LEN: usize = 2048;

pub struct TodoStore {
    nvs: EspDefaultNvs,
}

impl TodoStore {
    /// `partition` must be a clone of the one shared `EspDefaultNvsPartition`
    /// handle `main.rs` takes once - see the doc comment on
    /// `storage::PersistedCounters::open` for why a second independent
    /// `EspDefaultNvsPartition::take()` here would fail at boot.
    pub fn open(partition: EspDefaultNvsPartition) -> Result<Self> {
        let nvs = EspDefaultNvs::new(partition, NAMESPACE, true)
            .map_err(|e| anyhow!("failed to open NVS namespace '{NAMESPACE}': {e}"))?;
        Ok(Self { nvs })
    }

    /// Empty list if nothing has been saved yet.
    pub fn load(&self) -> Result<Vec<Todo>> {
        Ok(read_blob::<BLOB_BUF_LEN, _>(&self.nvs, KEY_TODOS)?.unwrap_or_default())
    }

    pub fn save(&self, todos: &[Todo]) -> Result<()> {
        write_blob::<BLOB_BUF_LEN, _>(&self.nvs, KEY_TODOS, todos)
    }

    // --- Two-way sync dirty tracking -------------------------------------
    //
    // The device uploads only `local_id`s that changed *locally* since the
    // last successful sync, so a `done` edit made on the
    // Server/Desktop side is not clobbered by the device's stale copy on
    // the next sync. The set is cleared only after a successful sync.

    fn dirty(&self) -> DirtySet<'_> {
        DirtySet::new(&self.nvs, KEY_DIRTY)
    }

    /// Marks `id` as locally changed (done flag) and pending upload.
    pub fn mark_dirty(&self, id: u8) -> Result<()> {
        self.dirty().mark(id)
    }

    /// `local_id`s changed locally since the last successful sync.
    pub fn dirty_ids(&self) -> Result<Vec<u8>> {
        self.dirty().ids()
    }

    /// Drops the dirty set after a successful sync.
    pub fn clear_dirty(&self) -> Result<()> {
        self.dirty().clear()
    }
    /// Clears only the IDs that were uploaded in this sync, preserving any
    /// dirty flags set during the round-trip (P1-3 race fix).
    pub fn clear_dirty_ids(&self, ids: &[u8]) -> Result<()> {
        self.dirty().clear_ids(ids)
    }
}
