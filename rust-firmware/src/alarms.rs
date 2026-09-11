//! Multi-alarm store, backed by one NVS blob. The PCF8563 has one live
//! hardware alarm slot; the application state machine derives and programs it
//! through the RTC effect executor.

use anyhow::{anyhow, Result};
use esp_idf_svc::nvs::{EspDefaultNvs, EspDefaultNvsPartition};

use crate::nvs_blob::{read_blob, write_blob, DirtySet};

const NAMESPACE: &str = "inkwash_alrm";
const KEY_ALARMS: &str = "alarms";
/// Locally-changed `local_id`s pending upload (two-way sync dirty set).
const KEY_DIRTY: &str = "dirty";
/// Generous headroom over what a few dozen short JSON alarm records need;
/// NVS blob entries top out around ~4000 bytes on this partition anyway.
const BLOB_BUF_LEN: usize = 1024;
/// `Repeat`/`StoredAlarm` and every pure ordering/recurrence/ID-allocation
/// function live in `inkwash-logic` so they can be unit-tested on the host
/// — this crate is the single source of truth, re-exported here so every
/// existing `alarms::Repeat` / `alarms::StoredAlarm` / `alarms::next_due`
/// call site keeps working unchanged.
pub use inkwash_logic::alarm_schedule::{
    date_from_days, days_since_epoch, days_until, next_due, next_occurrence_date, Repeat,
    StoredAlarm,
};

pub struct AlarmStore {
    nvs: EspDefaultNvs,
}

impl AlarmStore {
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
    pub fn load(&self) -> Result<Vec<StoredAlarm>> {
        Ok(read_blob::<BLOB_BUF_LEN, _>(&self.nvs, KEY_ALARMS)?.unwrap_or_default())
    }

    pub fn save(&self, alarms: &[StoredAlarm]) -> Result<()> {
        write_blob::<BLOB_BUF_LEN, _>(&self.nvs, KEY_ALARMS, alarms)
    }

    // --- Two-way sync dirty tracking -------------------------------------
    //
    // Same contract as `TodoStore::mark_dirty`: only `local_id`s whose
    // `enabled` flag changed *locally* are uploaded, so a Server/Desktop
    // edit isn't clobbered by the device's stale copy on the next sync.

    fn dirty(&self) -> DirtySet<'_> {
        DirtySet::new(&self.nvs, KEY_DIRTY)
    }

    /// Marks `id` as locally changed (enabled flag) and pending upload.
    pub fn mark_dirty(&self, id: u8) -> Result<()> {
        self.dirty().mark(id)
    }

    /// `local_id`s changed locally since the last successful sync.
    pub fn dirty_ids(&self) -> Result<Vec<u8>> {
        self.dirty().ids()
    }

    /// Clears only the IDs that were uploaded in this sync, preserving any
    /// dirty flags set during the round-trip (P1-3 race fix).
    pub fn clear_dirty_ids(&self, ids: &[u8]) -> Result<()> {
        self.dirty().clear_ids(ids)
    }
}
