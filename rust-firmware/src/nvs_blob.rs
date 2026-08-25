//! Generic NVS-blob JSON store helpers, shared by `AlarmStore`, `TodoStore`
//! and `InboxStore`. All three used to hand-roll their own identical
//! read/decode + encode/size-check/write pair, and `AlarmStore`/`TodoStore`
//! additionally each hand-rolled an identical copy of the two-way-sync
//! dirty-`local_id`-set tracking on top of it. `InboxStore` proved the
//! blob helper generalizes cleanly (it was the only one of the three that
//! didn't inline it); this module is that generalization applied to all
//! three, so a fix to the size-check or the empty-vs-missing-blob handling
//! only has to happen once.

use anyhow::{anyhow, Result};
use esp_idf_svc::nvs::EspDefaultNvs;
use serde::{Deserialize, Serialize};

/// Reads and JSON-decodes a blob at `key` into an `N`-byte stack buffer.
/// `Ok(None)` if the key has never been written.
pub fn read_blob<const N: usize, T>(nvs: &EspDefaultNvs, key: &str) -> Result<Option<T>>
where
    T: for<'de> Deserialize<'de>,
{
    let mut buf = [0u8; N];
    let bytes = nvs
        .get_blob(key, &mut buf)
        .map_err(|e| anyhow!("NVS get_blob({key}) failed: {e}"))?;
    match bytes {
        Some(bytes) => serde_json::from_slice(bytes)
            .map(Some)
            .map_err(|e| anyhow!("{key} JSON decode failed: {e}")),
        None => Ok(None),
    }
}

/// JSON-encodes `value` and writes it to `key`, rejecting anything that
/// wouldn't fit in the `N`-byte buffer a matching `read_blob::<N, _>` call
/// would later use.
pub fn write_blob<const N: usize, T: Serialize + ?Sized>(
    nvs: &EspDefaultNvs,
    key: &str,
    value: &T,
) -> Result<()> {
    let bytes = serde_json::to_vec(value).map_err(|e| anyhow!("{key} JSON encode failed: {e}"))?;
    if bytes.len() > N {
        return Err(anyhow!(
            "{key} blob too large: {} bytes (max {N})",
            bytes.len()
        ));
    }
    nvs.set_blob(key, &bytes)
        .map_err(|e| anyhow!("NVS set_blob({key}) failed: {e}"))
}

/// Comfortably covers the worst case of all 256 `u8` ids dirty at once
/// (`"[255,254,...,0]"` serializes to well under 1024 bytes).
const DIRTY_SET_BUF_LEN: usize = 1024;

/// Two-way-sync dirty-`local_id`-set tracking: only ids changed *locally*
/// since the last successful sync are uploaded, so a Server/Desktop edit
/// isn't clobbered by the device's stale copy on the next sync. The set is
/// cleared only after a successful sync. Same contract for both
/// `AlarmStore` (dirty = `enabled` changed) and `TodoStore` (dirty =
/// `done`/`importance` changed).
pub struct DirtySet<'a> {
    nvs: &'a EspDefaultNvs,
    key: &'static str,
}

impl<'a> DirtySet<'a> {
    pub fn new(nvs: &'a EspDefaultNvs, key: &'static str) -> Self {
        Self { nvs, key }
    }

    /// Marks `id` as locally changed and pending upload.
    pub fn mark(&self, id: u8) -> Result<()> {
        let mut dirty = self.ids()?;
        if !dirty.contains(&id) {
            dirty.push(id);
        }
        write_blob::<DIRTY_SET_BUF_LEN, _>(self.nvs, self.key, &dirty)
    }

    /// `local_id`s changed locally since the last successful sync.
    pub fn ids(&self) -> Result<Vec<u8>> {
        Ok(read_blob::<DIRTY_SET_BUF_LEN, _>(self.nvs, self.key)?.unwrap_or_default())
    }

    /// Drops the dirty set after a successful sync.
    pub fn clear(&self) -> Result<()> {
        self.nvs
            .remove(self.key)
            .map(|_| ())
            .map_err(|e| anyhow!("NVS remove({}) failed: {e}", self.key))
    }
}
