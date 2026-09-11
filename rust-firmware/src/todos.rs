use anyhow::{anyhow, Result};
use esp_idf_svc::nvs::{EspDefaultNvs, EspDefaultNvsPartition};

use crate::nvs_blob::{read_blob, write_blob, DirtySet};

#[allow(unused_imports)]
pub use inkwash_logic::todo::{Importance, Todo, TodoDue};

const NAMESPACE: &str = "inkwash_todo";
const KEY_TODOS: &str = "todos";

const KEY_DIRTY: &str = "dirty";
const BLOB_BUF_LEN: usize = 2048;

pub struct TodoStore {
    nvs: EspDefaultNvs,
}

impl TodoStore {
    pub fn open(partition: EspDefaultNvsPartition) -> Result<Self> {
        let nvs = EspDefaultNvs::new(partition, NAMESPACE, true)
            .map_err(|e| anyhow!("failed to open NVS namespace '{NAMESPACE}': {e}"))?;
        Ok(Self { nvs })
    }

    pub fn load(&self) -> Result<Vec<Todo>> {
        Ok(read_blob::<BLOB_BUF_LEN, _>(&self.nvs, KEY_TODOS)?.unwrap_or_default())
    }

    pub fn save(&self, todos: &[Todo]) -> Result<()> {
        write_blob::<BLOB_BUF_LEN, _>(&self.nvs, KEY_TODOS, todos)
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
