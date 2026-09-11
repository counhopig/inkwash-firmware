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

/// Reads a single *scalar* string value stored under one NVS `key`, in
/// contrast to the list-oriented blob pair above. This is the variant used
/// by `storage.rs::PersistedCounters` for its simple per-field keys (Wi-Fi
/// credentials, server URL/token, ETag, numeric settings recorded as their
/// decimal string, ...): each field is one plain NVS string item rather than
/// a JSON blob, so it stays individually inspectable with `idf.py`/parttool
/// and costs no serde round-trip. `Ok(None)` if the key has never been
/// written; the `N`-byte stack buffer plays the same role as in
/// `read_blob` - a value longer than `N` fails the underlying `get_str`
/// instead of silently truncating. Thin wrapper over the ESP-only NVS FFI,
/// hence not host-unit-testable (the sibling `logic` crate deliberately has
/// zero ESP-IDF deps).
pub fn read_scalar<const N: usize>(nvs: &EspDefaultNvs, key: &str) -> Result<Option<String>> {
    let mut buf = [0u8; N];
    Ok(nvs
        .get_str(key, &mut buf)
        .map_err(|e| anyhow!("NVS get_str({key}) failed: {e}"))?
        .map(str::to_owned))
}

/// Writes a single *scalar* string value under one NVS `key` - the write
/// half of [`read_scalar`] (same per-field contract; see above).
pub fn write_scalar(nvs: &EspDefaultNvs, key: &str, value: &str) -> Result<()> {
    nvs.set_str(key, value)
        .map_err(|e| anyhow!("NVS set_str({key}) failed: {e}"))
}

/// Comfortably covers the worst case of all 256 `u8` ids dirty at once
/// (`"[255,254,...,0]"` serializes to well under 1024 bytes).
const DIRTY_SET_BUF_LEN: usize = 1024;

/// Two-way-sync dirty-`local_id`-set tracking: only ids changed *locally*
/// since the last successful sync are uploaded, so a Server/Desktop edit
/// isn't clobbered by the device's stale copy on the next sync. The set is
/// cleared only after a successful sync (via `clear_ids`). Same contract for
/// both `AlarmStore` (dirty = `enabled` changed) and `TodoStore` (dirty =
/// `done` changed).
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

    /// Removes only the given `ids` from the dirty set, preserving any
    /// entries made *after* the sync snapshot was taken (e.g. a user toggle
    /// during the network round-trip), so concurrent local edits aren't
    /// silently lost.
    pub fn clear_ids(&self, ids: &[u8]) -> Result<()> {
        if ids.is_empty() {
            return Ok(());
        }
        let remaining: Vec<u8> = self
            .ids()?
            .into_iter()
            .filter(|id| !ids.contains(id))
            .collect();
        write_blob::<DIRTY_SET_BUF_LEN, _>(self.nvs, self.key, &remaining)
    }
}
