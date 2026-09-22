use anyhow::{anyhow, Result};
use esp_idf_svc::nvs::{EspDefaultNvs, EspDefaultNvsPartition};
use serde::{Deserialize, Serialize};

use inkwash_logic::boot_store::StoreFault;

use crate::nvs_blob::{read_blob, write_blob};

pub use inkwash_logic::inbox_item::{InboxItem, InboxKind, Priority};

const NAMESPACE: &str = "inkwash_inbox";
const KEY_ITEMS: &str = "items";
const KEY_PENDING: &str = "pending";
const KEY_STATE: &str = "state_v1";
const LEGACY_BLOB_BUF_LEN: usize = 4096;
const STATE_BLOB_BUF_LEN: usize = 6144;

pub const MAX_ITEMS: usize = 32;

const MAX_BODY_CHARS: usize = 300;

const BLOB_HEADROOM: usize = 256;

#[derive(Default, Deserialize, Serialize)]
struct InboxState {
    version: u8,
    items: Vec<InboxItem>,
    pending_read: Vec<u64>,
}

pub struct InboxStore {
    nvs: EspDefaultNvs,
}

impl InboxStore {
    pub fn open(partition: EspDefaultNvsPartition) -> Result<Self> {
        let nvs = EspDefaultNvs::new(partition, NAMESPACE, true)
            .map_err(|e| anyhow!("failed to open NVS namespace '{NAMESPACE}': {e}"))?;
        Ok(Self { nvs })
    }

    fn load_state(&self) -> Result<InboxState, StoreFault> {
        if let Some(state) = read_blob::<STATE_BLOB_BUF_LEN, InboxState>(&self.nvs, KEY_STATE)? {
            if state.version != 1 {
                log::error!(
                    "stored inbox state version {} is not supported",
                    state.version
                );
                return Err(StoreFault::UnsupportedVersion);
            }
            return Ok(state);
        }

        Ok(InboxState {
            version: 1,
            items: read_blob::<LEGACY_BLOB_BUF_LEN, _>(&self.nvs, KEY_ITEMS)?.unwrap_or_default(),
            pending_read: read_blob::<LEGACY_BLOB_BUF_LEN, _>(&self.nvs, KEY_PENDING)?
                .unwrap_or_default(),
        })
    }

    fn save_state(&self, state: &InboxState) -> Result<()> {
        write_blob::<STATE_BLOB_BUF_LEN, _>(&self.nvs, KEY_STATE, state)
    }

    pub fn load(&self) -> Result<Vec<InboxItem>, StoreFault> {
        Ok(self.load_state()?.items)
    }

    pub fn save(&self, items: &[InboxItem]) -> Result<()> {
        let current = self.load_state()?;
        let pending =
            inkwash_logic::reminder_dedup::merge_pending_read(&current.pending_read, items);
        let mut owned: Vec<InboxItem> = items.to_vec();
        owned.truncate(MAX_ITEMS);

        inkwash_logic::reminder_dedup::apply_pending_read(&mut owned, &pending);

        for item in owned.iter_mut() {
            if item.body.chars().count() > MAX_BODY_CHARS {
                item.body = format!(
                    "{}…",
                    item.body.chars().take(MAX_BODY_CHARS).collect::<String>()
                );
            }
        }
        let mut state = InboxState {
            version: 1,
            items: owned,
            pending_read: pending,
        };
        let budget = STATE_BLOB_BUF_LEN.saturating_sub(BLOB_HEADROOM);
        while state.items.len() > 1
            && serde_json::to_vec(&state)
                .map(|bytes| bytes.len())
                .unwrap_or(usize::MAX)
                > budget
        {
            state.items.pop();
        }
        state
            .pending_read
            .retain(|seq| state.items.iter().any(|item| item.id == *seq));
        if serde_json::to_vec(&state)
            .map(|bytes| bytes.len())
            .unwrap_or(usize::MAX)
            > budget
        {
            return Err(anyhow!("inbox state exceeds storage budget"));
        }
        self.save_state(&state)
    }

    pub fn pending_read(&self) -> Result<Vec<u64>, StoreFault> {
        Ok(self.load_state()?.pending_read)
    }

    pub fn mark_read(&self, seq: u64) -> Result<()> {
        let mut state = self.load_state()?;
        let Some(item) = state.items.iter_mut().find(|item| item.id == seq) else {
            return Err(anyhow!("inbox item {seq} not found"));
        };
        item.read = true;
        if !state.pending_read.contains(&seq) {
            state.pending_read.push(seq);
        }
        self.save_state(&state)
    }

    pub fn ack_read(&self, acked: &[u64]) -> Result<()> {
        let mut state = self.load_state()?;
        state.pending_read =
            inkwash_logic::reminder_dedup::ack_pending_read(&state.pending_read, acked);
        self.save_state(&state)
    }

    pub fn unread_urgent(&self) -> Result<Vec<u64>, StoreFault> {
        Ok(self
            .load()?
            .iter()
            .filter(|it| !it.read && it.priority == Priority::High && it.kind == InboxKind::Alert)
            .map(|it| it.id)
            .collect())
    }
}
