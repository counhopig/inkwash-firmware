//! Inbox store, backed by one NVS blob - same shape as `todos.rs` but for
//! notifications delivered from external sources (webhook/CalDAV) via the
//! server. The device only browses, marks read, and uploads read `seq`s back;
//! it never authors inbox content.
//!
//! Two pieces of state are kept separately so a power cut between "user
//! opened a message" and "server acked the read" can't lose the pending read:
//! - the authoritative `InboxItem` list (from the server),
//! - a set of `seq`s locally marked read but not yet acknowledged by the
//!   server. They are only removed once `inbox_read_acked` echoes them back.

use anyhow::{anyhow, Result};
use esp_idf_svc::nvs::{EspDefaultNvs, EspDefaultNvsPartition};

use crate::nvs_blob::{read_blob, write_blob};

/// `InboxKind`/`Priority`/`InboxItem` live in `inkwash-logic` (re-exported
/// here), alongside the pending-read merge/dedup rules `save`/`mark_read`/
/// `ack_read` below call into instead of inlining - see "Remaining
/// engineering work" #1 in `../docs/remaining-work.md`.
pub use inkwash_logic::inbox_item::{InboxItem, InboxKind, Priority};

const NAMESPACE: &str = "inkwash_inbox";
const KEY_ITEMS: &str = "items";
const KEY_PENDING: &str = "pending";
const BLOB_BUF_LEN: usize = 4096;

/// Maximum number of items kept locally; anything beyond this is dropped on
/// save so a large server inbox can't overflow NVS.
pub const MAX_ITEMS: usize = 32;

/// Per-item body cap (in chars) applied on save - a single 1000-char
/// Chinese body serializes to ~3KB and would crowd every other item out of
/// the NVS blob. The device detail page reads fine with 300 chars.
const MAX_BODY_CHARS: usize = 300;

/// Headroom left below `BLOB_BUF_LEN` for JSON overhead while trimming
/// items to the byte budget.
const BLOB_HEADROOM: usize = 256;

pub struct InboxStore {
    nvs: EspDefaultNvs,
}

impl InboxStore {
    /// `partition` must be a clone of the one shared `EspDefaultNvsPartition`
    /// handle `main.rs` takes once - see the doc comment on
    /// `storage::PersistedCounters::open`.
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

    /// The authoritative inbox list, empty if never synced.
    pub fn load(&self) -> Result<Vec<InboxItem>> {
        Ok(self.read_blob(KEY_ITEMS)?.unwrap_or_default())
    }

    /// Replaces the inbox with the server's authoritative list. Any locally
    /// pending-read `seq`s that are now `read` (or no longer present) are
    /// dropped; the rest are preserved so they can still be acked later.
    pub fn save(&self, items: &[InboxItem]) -> Result<()> {
        let pending = self.pending_read()?;
        let pending = inkwash_logic::reminder_dedup::merge_pending_read(&pending, items);
        let mut owned: Vec<InboxItem> = items.to_vec();
        owned.truncate(MAX_ITEMS);
        // Preserve the locally-read flag for anything the server still sends
        // back as unread but that we've already opened this session.
        inkwash_logic::reminder_dedup::apply_pending_read(&mut owned, &pending);
        // NVS blobs are capped at `BLOB_BUF_LEN`, so long bodies and many
        // items must be trimmed to fit: cut each body to `MAX_BODY_CHARS`,
        // then drop the *oldest* items (the server sends newest-first) until
        // the serialized blob fits. At least one item always survives.
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

    /// `seq`s locally read but not yet acked by the server.
    pub fn pending_read(&self) -> Result<Vec<u64>> {
        Ok(self.read_blob(KEY_PENDING)?.unwrap_or_default())
    }

    /// Marks `seq` read locally and adds it to the pending-read set (unless
    /// already there). Used when the user opens a message on-device.
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

    /// Removes `seq`s from the pending-read set once the server acknowledges
    /// them (`inbox_read_acked`).
    pub fn ack_read(&self, acked: &[u64]) -> Result<()> {
        let pending = self.pending_read()?;
        let remaining = inkwash_logic::reminder_dedup::ack_pending_read(&pending, acked);
        self.write_blob(KEY_PENDING, &remaining)
    }

    /// Count of locally-unread items (for the home badge). `Alert` items that
    /// have been notified also count as unread until the user opens them.
    pub fn unread_count(&self) -> Result<usize> {
        Ok(self.load()?.iter().filter(|it| !it.read).count())
    }

    /// The `seq`s of unread high-priority `alert` items, in store order. These
    /// drive the urgent full-screen reminder + tone.
    pub fn unread_urgent(&self) -> Result<Vec<u64>> {
        Ok(self
            .load()?
            .iter()
            .filter(|it| !it.read && it.priority == Priority::High && it.kind == InboxKind::Alert)
            .map(|it| it.id)
            .collect())
    }
}
