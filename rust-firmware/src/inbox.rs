use anyhow::{anyhow, Result};
use esp_idf_svc::nvs::{EspDefaultNvs, EspDefaultNvsPartition};

use crate::nvs_blob::{read_blob, write_blob};

pub use inkwash_logic::inbox_item::{InboxItem, InboxKind, Priority};

const NAMESPACE: &str = "inkwash_inbox";
const KEY_ITEMS: &str = "items";
const KEY_PENDING: &str = "pending";
const BLOB_BUF_LEN: usize = 4096;

pub const MAX_ITEMS: usize = 32;

const MAX_BODY_CHARS: usize = 300;

const BLOB_HEADROOM: usize = 256;

pub struct InboxStore {
    nvs: EspDefaultNvs,
}

impl InboxStore {
    pub fn open(partition: EspDefaultNvsPartition) -> Result<Self> {
        let nvs = EspDefaultNvs::new(partition, NAMESPACE, true)
            .map_err(|e| anyhow!("failed to open NVS namespace '{NAMESPACE}': {e}"))?;
        Ok(Self { nvs })
    }

    fn read_blob<T: for<'de> serde::Deserialize<'de>>(&self, key: &str) -> Result<Option<T>> {
        read_blob::<BLOB_BUF_LEN, _>(&self.nvs, key)
    }

    fn write_blob<T: serde::Serialize + ?Sized>(&self, key: &str, value: &T) -> Result<()> {
        write_blob::<BLOB_BUF_LEN, _>(&self.nvs, key, value)
    }

    pub fn load(&self) -> Result<Vec<InboxItem>> {
        Ok(self.read_blob(KEY_ITEMS)?.unwrap_or_default())
    }

    pub fn save(&self, items: &[InboxItem]) -> Result<()> {
        let pending = self.pending_read()?;
        let pending = inkwash_logic::reminder_dedup::merge_pending_read(&pending, items);
        let mut owned: Vec<InboxItem> = items.to_vec();
        owned.truncate(MAX_ITEMS);

        inkwash_logic::reminder_dedup::apply_pending_read(&mut owned, &pending);

        let budget = BLOB_BUF_LEN.saturating_sub(BLOB_HEADROOM);
        for item in owned.iter_mut() {
            if item.body.chars().count() > MAX_BODY_CHARS {
                item.body = format!(
                    "{}…",
                    item.body.chars().take(MAX_BODY_CHARS).collect::<String>()
                );
            }
        }
        while owned.len() > 1
            && serde_json::to_vec(&owned)
                .map(|v| v.len())
                .unwrap_or(usize::MAX)
                > budget
        {
            owned.pop();
        }
        self.write_blob(KEY_ITEMS, &owned)?;
        self.write_blob(KEY_PENDING, &pending)
    }

    pub fn pending_read(&self) -> Result<Vec<u64>> {
        Ok(self.read_blob(KEY_PENDING)?.unwrap_or_default())
    }

    pub fn mark_read(&self, seq: u64) -> Result<()> {
        let mut items = self.load()?;
        let mut pending = self.pending_read()?;
        if let Some(item) = items.iter_mut().find(|it| it.id == seq) {
            item.read = true;
        }
        if !pending.contains(&seq) {
            pending.push(seq);
        }
        self.write_blob(KEY_ITEMS, &items)?;
        self.write_blob(KEY_PENDING, &pending)
    }

    pub fn ack_read(&self, acked: &[u64]) -> Result<()> {
        let pending = self.pending_read()?;
        let remaining = inkwash_logic::reminder_dedup::ack_pending_read(&pending, acked);
        self.write_blob(KEY_PENDING, &remaining)
    }

    pub fn unread_urgent(&self) -> Result<Vec<u64>> {
        Ok(self
            .load()?
            .iter()
            .filter(|it| !it.read && it.priority == Priority::High && it.kind == InboxKind::Alert)
            .map(|it| it.id)
            .collect())
    }
}
