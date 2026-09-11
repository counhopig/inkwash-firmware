use anyhow::{anyhow, Result};
use esp_idf_svc::nvs::EspDefaultNvs;
use serde::{Deserialize, Serialize};

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

pub fn read_scalar<const N: usize>(nvs: &EspDefaultNvs, key: &str) -> Result<Option<String>> {
    let mut buf = [0u8; N];
    Ok(nvs
        .get_str(key, &mut buf)
        .map_err(|e| anyhow!("NVS get_str({key}) failed: {e}"))?
        .map(str::to_owned))
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

    pub fn ids(&self) -> Result<Vec<u8>> {
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
