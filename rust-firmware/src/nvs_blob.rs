use anyhow::{anyhow, Result};
use esp_idf_svc::nvs::EspDefaultNvs;
use serde::{Deserialize, Serialize};

use inkwash_logic::boot_store::StoreFault;

/// Reads a fixed-size blob. `Ok(None)` means the key was never written, which
/// the caller may treat as first-time configuration; every other failure is a
/// typed fault so the boot path can refuse to run on unreadable data.
pub fn read_blob<const N: usize, T>(nvs: &EspDefaultNvs, key: &str) -> Result<Option<T>, StoreFault>
where
    T: for<'de> Deserialize<'de>,
{
    let mut buf = [0u8; N];
    let stored = match nvs.get_blob(key, &mut buf) {
        Ok(Some(bytes)) => bytes,
        Ok(None) => return Ok(None),
        Err(err) => {
            log::error!("NVS read failed for key {key}: {err}");
            return Err(StoreFault::Io);
        }
    };
    decode(stored, key).map(Some)
}

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

pub fn read_dynamic_blob<T>(
    nvs: &EspDefaultNvs,
    key: &str,
    max_len: usize,
) -> Result<Option<T>, StoreFault>
where
    T: for<'de> Deserialize<'de>,
{
    let Some(len) = nvs.blob_len(key).map_err(|err| {
        log::error!("NVS length read failed for key {key}: {err}");
        StoreFault::Io
    })?
    else {
        return Ok(None);
    };
    if len > max_len {
        log::error!("NVS key {key} holds {len} bytes (max {max_len})");
        return Err(StoreFault::Corrupt);
    }
    let mut bytes = vec![0u8; len];
    let stored = nvs
        .get_blob(key, &mut bytes)
        .map_err(|err| {
            log::error!("NVS read failed for key {key}: {err}");
            StoreFault::Io
        })?
        .ok_or_else(|| {
            log::error!("NVS key {key} disappeared between the length and the value read");
            StoreFault::Io
        })?;
    decode(stored, key).map(Some)
}

/// Turns stored bytes into a value. The decoder's own message can quote the
/// bytes it rejected — which for the Wi-Fi and server blobs means credentials —
/// so only the failure class and its position are logged.
fn decode<T>(stored: &[u8], key: &str) -> Result<T, StoreFault>
where
    T: for<'de> Deserialize<'de>,
{
    serde_json::from_slice(stored).map_err(|err| {
        log::error!(
            "NVS key {key} holds undecodable data ({:?} at line {}, column {})",
            err.classify(),
            err.line(),
            err.column()
        );
        StoreFault::Corrupt
    })
}

pub fn read_scalar<const N: usize>(
    nvs: &EspDefaultNvs,
    key: &str,
) -> Result<Option<String>, StoreFault> {
    let mut buf = [0u8; N];
    match nvs.get_str(key, &mut buf) {
        Ok(Some(value)) => Ok(Some(value.to_owned())),
        Ok(None) => Ok(None),
        Err(err) => {
            log::error!("NVS string read failed for key {key}: {err}");
            Err(StoreFault::Io)
        }
    }
}

pub fn write_scalar(nvs: &EspDefaultNvs, key: &str, value: &str) -> Result<()> {
    nvs.set_str(key, value)
        .map_err(|e| anyhow!("NVS set_str({key}) failed: {e}"))
}

const DIRTY_SET_BUF_LEN: usize = 1024;

pub struct DirtySet<'a> {
    nvs: &'a EspDefaultNvs,
    key: &'static str,
}

impl<'a> DirtySet<'a> {
    pub fn new(nvs: &'a EspDefaultNvs, key: &'static str) -> Self {
        Self { nvs, key }
    }

    pub fn mark(&self, id: u8) -> Result<()> {
        let mut dirty = self.ids()?;
        if !dirty.contains(&id) {
            dirty.push(id);
        }
        write_blob::<DIRTY_SET_BUF_LEN, _>(self.nvs, self.key, &dirty)
    }

    pub fn ids(&self) -> Result<Vec<u8>, StoreFault> {
        Ok(read_blob::<DIRTY_SET_BUF_LEN, _>(self.nvs, self.key)?.unwrap_or_default())
    }

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
