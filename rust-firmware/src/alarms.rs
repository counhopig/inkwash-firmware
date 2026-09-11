use anyhow::{anyhow, Result};
use esp_idf_svc::nvs::{EspDefaultNvs, EspDefaultNvsPartition};

use crate::nvs_blob::{read_blob, write_blob, DirtySet};

const NAMESPACE: &str = "inkwash_alrm";
const KEY_ALARMS: &str = "alarms";

const KEY_DIRTY: &str = "dirty";

const BLOB_BUF_LEN: usize = 1024;

pub use inkwash_logic::alarm_schedule::{
    date_from_days, days_since_epoch, days_until, next_due, next_occurrence_date, Repeat,
    StoredAlarm,
};

pub struct AlarmStore {
    nvs: EspDefaultNvs,
}

impl AlarmStore {
    pub fn open(partition: EspDefaultNvsPartition) -> Result<Self> {
        let nvs = EspDefaultNvs::new(partition, NAMESPACE, true)
            .map_err(|e| anyhow!("failed to open NVS namespace '{NAMESPACE}': {e}"))?;
        Ok(Self { nvs })
    }

    pub fn load(&self) -> Result<Vec<StoredAlarm>> {
        Ok(read_blob::<BLOB_BUF_LEN, _>(&self.nvs, KEY_ALARMS)?.unwrap_or_default())
    }

    pub fn save(&self, alarms: &[StoredAlarm]) -> Result<()> {
        write_blob::<BLOB_BUF_LEN, _>(&self.nvs, KEY_ALARMS, alarms)
    }

    fn dirty(&self) -> DirtySet<'_> {
        DirtySet::new(&self.nvs, KEY_DIRTY)
    }

    pub fn mark_dirty(&self, id: u8) -> Result<()> {
        self.dirty().mark(id)
    }

    pub fn dirty_ids(&self) -> Result<Vec<u8>> {
        self.dirty().ids()
    }

    pub fn clear_dirty_ids(&self, ids: &[u8]) -> Result<()> {
        self.dirty().clear_ids(ids)
    }
}
