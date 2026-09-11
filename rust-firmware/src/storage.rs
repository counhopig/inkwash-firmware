use anyhow::{anyhow, Result};
use esp_idf_svc::nvs::{EspDefaultNvs, EspDefaultNvsPartition};

use crate::nvs_blob;

const NAMESPACE: &str = "inkwash";
const KEY_WIFI_SSID: &str = "wifi_ssid";
const KEY_WIFI_PASS: &str = "wifi_pass";
const KEY_SERVER_URL: &str = "server_url";
const KEY_AUTH_TOKEN: &str = "auth_token";
const KEY_SYNC_ETAG: &str = "sync_etag";
const KEY_TIMEZONE_OFFSET: &str = "timezone_min";
const KEY_SYNC_INTERVAL_MIN: &str = "sync_interval_min";
const KEY_LAST_SYNC_EPOCH: &str = "last_sync_epoch";
const KEY_RTC_ALIGN_EPOCH: &str = "rtc_align_epoch";

const KEY_TODO_REMINDED_DATE: &str = "todo_rem_date";

const WIFI_CRED_MAX_LEN: usize = 64;

const SERVER_CONFIG_MAX_LEN: usize = 256;

const NUM_STR_MAX_LEN: usize = 20;

pub use inkwash_logic::device_config::{DeviceConfig, WifiCreds};

pub struct PersistedCounters {
    nvs: EspDefaultNvs,
}

impl PersistedCounters {
    pub fn open(partition: EspDefaultNvsPartition) -> Result<Self> {
        let nvs = EspDefaultNvs::new(partition, NAMESPACE, true)
            .map_err(|e| anyhow!("failed to open NVS namespace '{NAMESPACE}': {e}"))?;
        Ok(Self { nvs })
    }

    fn read_num<T: std::str::FromStr>(&self, key: &str, label: &str) -> Result<Option<T>>
    where
        T::Err: std::fmt::Display,
    {
        match nvs_blob::read_scalar::<NUM_STR_MAX_LEN>(&self.nvs, key)? {
            Some(value) => value
                .parse::<T>()
                .map(Some)
                .map_err(|e| anyhow!("invalid stored {label} '{value}': {e}")),
            None => Ok(None),
        }
    }

    fn write_num<T: ToString>(&self, key: &str, value: T) -> Result<()> {
        nvs_blob::write_scalar(&self.nvs, key, &value.to_string())
    }

    pub fn wifi_creds(&self) -> Result<Option<WifiCreds>> {
        let Some(ssid) = nvs_blob::read_scalar::<WIFI_CRED_MAX_LEN>(&self.nvs, KEY_WIFI_SSID)?
        else {
            return Ok(None);
        };
        let password = nvs_blob::read_scalar::<WIFI_CRED_MAX_LEN>(&self.nvs, KEY_WIFI_PASS)?
            .unwrap_or_default();
        Ok(Some(WifiCreds { ssid, password }))
    }

    pub fn save_wifi_creds(&self, creds: &WifiCreds) -> Result<()> {
        nvs_blob::write_scalar(&self.nvs, KEY_WIFI_SSID, &creds.ssid)?;
        nvs_blob::write_scalar(&self.nvs, KEY_WIFI_PASS, &creds.password)
    }

    pub fn device_config(&self) -> Result<Option<DeviceConfig>> {
        let Some(server_url) =
            nvs_blob::read_scalar::<SERVER_CONFIG_MAX_LEN>(&self.nvs, KEY_SERVER_URL)?
        else {
            return Ok(None);
        };
        let auth_token = nvs_blob::read_scalar::<SERVER_CONFIG_MAX_LEN>(&self.nvs, KEY_AUTH_TOKEN)?
            .unwrap_or_default();
        Ok(Some(DeviceConfig {
            server_url,
            auth_token,
        }))
    }

    pub fn save_device_config(&self, cfg: &DeviceConfig) -> Result<()> {
        nvs_blob::write_scalar(&self.nvs, KEY_SERVER_URL, &cfg.server_url)?;
        nvs_blob::write_scalar(&self.nvs, KEY_AUTH_TOKEN, &cfg.auth_token)
    }

    pub fn sync_etag(&self) -> Result<Option<String>> {
        nvs_blob::read_scalar::<SERVER_CONFIG_MAX_LEN>(&self.nvs, KEY_SYNC_ETAG)
    }

    pub fn save_sync_etag(&self, etag: &str) -> Result<()> {
        nvs_blob::write_scalar(&self.nvs, KEY_SYNC_ETAG, etag)
    }

    pub fn clear_sync_etag(&self) -> Result<()> {
        self.nvs
            .remove(KEY_SYNC_ETAG)
            .map(|_| ())
            .map_err(|e| anyhow!("NVS remove({KEY_SYNC_ETAG}) failed: {e}"))
    }

    pub fn sync_interval_minutes(&self) -> Result<u16> {
        Ok(self
            .read_num::<u16>(KEY_SYNC_INTERVAL_MIN, "sync interval")?
            .unwrap_or(60))
    }

    pub fn set_sync_interval_minutes(&self, minutes: u16) -> Result<()> {
        if !(1..=1440).contains(&minutes) {
            return Err(anyhow!("sync interval must be between 1 and 1440 minutes"));
        }
        self.write_num(KEY_SYNC_INTERVAL_MIN, minutes)
    }

    pub fn last_sync_epoch(&self) -> Result<Option<u64>> {
        self.read_num::<u64>(KEY_LAST_SYNC_EPOCH, "last-sync epoch")
    }

    pub fn set_last_sync_epoch(&self, epoch: u64) -> Result<()> {
        self.write_num(KEY_LAST_SYNC_EPOCH, epoch)
    }

    pub fn rtc_align_epoch(&self) -> Result<Option<u64>> {
        self.read_num::<u64>(KEY_RTC_ALIGN_EPOCH, "rtc-align epoch")
    }

    pub fn set_rtc_align_epoch(&self, epoch: u64) -> Result<()> {
        self.write_num(KEY_RTC_ALIGN_EPOCH, epoch)
    }

    pub fn clear_rtc_align_epoch(&self) -> Result<()> {
        self.nvs
            .remove(KEY_RTC_ALIGN_EPOCH)
            .map(|_| ())
            .map_err(|e| anyhow!("NVS remove({KEY_RTC_ALIGN_EPOCH}) failed: {e}"))
    }

    pub fn timezone_offset_minutes(&self) -> Result<i16> {
        Ok(self
            .read_num::<i16>(KEY_TIMEZONE_OFFSET, "timezone offset")?
            .unwrap_or(0))
    }

    pub fn save_timezone_offset_minutes(&self, offset: i16) -> Result<()> {
        if !(-720..=840).contains(&offset) {
            return Err(anyhow!(
                "timezone offset must be between -720 and 840 minutes"
            ));
        }
        self.write_num(KEY_TIMEZONE_OFFSET, offset)
    }

    pub fn todo_reminded_date(&self) -> Result<Option<String>> {
        nvs_blob::read_scalar::<NUM_STR_MAX_LEN>(&self.nvs, KEY_TODO_REMINDED_DATE)
    }

    pub fn set_todo_reminded_date(&self, date: &str) -> Result<()> {
        nvs_blob::write_scalar(&self.nvs, KEY_TODO_REMINDED_DATE, date)
    }
}
