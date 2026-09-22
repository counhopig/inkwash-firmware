use anyhow::{anyhow, Result};
use esp_idf_svc::nvs::{EspDefaultNvs, EspDefaultNvsPartition};
use serde::{Deserialize, Serialize};

use inkwash_logic::boot_store::StoreFault;

use crate::nvs_blob;

const NAMESPACE: &str = "inkwash";
const KEY_WIFI_SSID: &str = "wifi_ssid";
const KEY_WIFI_PASS: &str = "wifi_pass";
const KEY_WIFI_CONFIG: &str = "wifi_cfg_v1";
const KEY_SERVER_URL: &str = "server_url";
const KEY_AUTH_TOKEN: &str = "auth_token";
const KEY_SERVER_CONFIG: &str = "server_cfg_v1";
const KEY_TIMEZONE_OFFSET: &str = "timezone_min";
const KEY_SYNC_INTERVAL_MIN: &str = "sync_interval_min";
const KEY_LAST_SYNC_EPOCH: &str = "last_sync_epoch";
const KEY_RTC_ALIGN_EPOCH: &str = "rtc_align_epoch";
const KEY_SYNC_APPLY_JOURNAL: &str = "sync_jrn_v1";
const SYNC_APPLY_JOURNAL_MAX_LEN: usize = 12 * 1024;

const KEY_TODO_REMINDED_DATE: &str = "todo_rem_date";

const WIFI_CRED_MAX_LEN: usize = 65;
const WIFI_CONFIG_BUF_LEN: usize = 768;

const SERVER_CONFIG_MAX_LEN: usize = 256;
const SERVER_CONFIG_BLOB_LEN: usize = 2304;
const MAX_SERVER_URL_LEN: usize = 240;
const MAX_AUTH_TOKEN_LEN: usize = SERVER_CONFIG_MAX_LEN - 1;

const NUM_STR_MAX_LEN: usize = 20;

pub use inkwash_logic::device_config::{DeviceConfig, WifiCreds};

#[derive(Serialize, Deserialize)]
struct StoredWifiConfig {
    version: u8,
    ssid: String,
    password: String,
}

#[derive(Serialize, Deserialize)]
struct StoredServerConfig {
    version: u8,
    server_url: String,
    auth_token: String,
}

fn validate_wifi_creds(creds: &WifiCreds) -> Result<()> {
    let valid_password = creds.password.is_empty()
        || (8..=63).contains(&creds.password.len())
        || (creds.password.len() == 64
            && creds.password.bytes().all(|byte| byte.is_ascii_hexdigit()));
    if creds.ssid.is_empty() || creds.ssid.len() > 32 || !valid_password {
        return Err(anyhow!("Wi-Fi credentials exceed 802.11 limits"));
    }
    Ok(())
}

fn validate_device_config(cfg: &DeviceConfig) -> Result<()> {
    if cfg.server_url.len() > MAX_SERVER_URL_LEN {
        return Err(anyhow!(
            "server URL is {} bytes (max {MAX_SERVER_URL_LEN})",
            cfg.server_url.len()
        ));
    }
    let Some(rest) = cfg.server_url.strip_prefix("https://") else {
        return Err(anyhow!("server URL must use HTTPS"));
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    if authority.is_empty() || authority.contains('@') || authority.chars().any(char::is_whitespace)
    {
        return Err(anyhow!("server URL contains an invalid authority"));
    }
    if cfg.auth_token.len() > MAX_AUTH_TOKEN_LEN {
        return Err(anyhow!(
            "authentication token is {} bytes (max {MAX_AUTH_TOKEN_LEN})",
            cfg.auth_token.len()
        ));
    }
    Ok(())
}

pub struct PersistedCounters {
    nvs: EspDefaultNvs,
}

impl PersistedCounters {
    pub fn open(partition: EspDefaultNvsPartition) -> Result<Self> {
        let nvs = EspDefaultNvs::new(partition, NAMESPACE, true)
            .map_err(|e| anyhow!("failed to open NVS namespace '{NAMESPACE}': {e}"))?;
        Ok(Self { nvs })
    }

    fn read_num<T: std::str::FromStr>(
        &self,
        key: &str,
        label: &str,
    ) -> Result<Option<T>, StoreFault>
    where
        T::Err: std::fmt::Display,
    {
        match nvs_blob::read_scalar::<NUM_STR_MAX_LEN>(&self.nvs, key)? {
            Some(value) => value.parse::<T>().map(Some).map_err(|err| {
                log::error!("stored {label} is not a valid number: {err}");
                StoreFault::Corrupt
            }),
            None => Ok(None),
        }
    }

    fn write_num<T: ToString>(&self, key: &str, value: T) -> Result<()> {
        nvs_blob::write_scalar(&self.nvs, key, &value.to_string())
    }

    pub fn wifi_creds(&self) -> Result<Option<WifiCreds>, StoreFault> {
        if let Some(stored) = nvs_blob::read_blob::<WIFI_CONFIG_BUF_LEN, StoredWifiConfig>(
            &self.nvs,
            KEY_WIFI_CONFIG,
        )? {
            if stored.version != 1 {
                log::error!(
                    "stored Wi-Fi config version {} is not supported",
                    stored.version
                );
                return Err(StoreFault::UnsupportedVersion);
            }
            let creds = WifiCreds {
                ssid: stored.ssid,
                password: stored.password,
            };
            validate_wifi_creds(&creds).map_err(|err| {
                log::error!("stored Wi-Fi config is unusable: {err}");
                StoreFault::Corrupt
            })?;
            return Ok(Some(creds));
        }
        let Some(ssid) = nvs_blob::read_scalar::<WIFI_CRED_MAX_LEN>(&self.nvs, KEY_WIFI_SSID)?
        else {
            return Ok(None);
        };
        let password = nvs_blob::read_scalar::<WIFI_CRED_MAX_LEN>(&self.nvs, KEY_WIFI_PASS)?
            .unwrap_or_default();
        let creds = WifiCreds { ssid, password };
        validate_wifi_creds(&creds).map_err(|err| {
            log::error!("stored Wi-Fi credentials are unusable: {err}");
            StoreFault::Corrupt
        })?;
        Ok(Some(creds))
    }

    pub fn save_wifi_creds(&self, creds: &WifiCreds) -> Result<()> {
        validate_wifi_creds(creds)?;
        nvs_blob::write_blob::<WIFI_CONFIG_BUF_LEN, _>(
            &self.nvs,
            KEY_WIFI_CONFIG,
            &StoredWifiConfig {
                version: 1,
                ssid: creds.ssid.clone(),
                password: creds.password.clone(),
            },
        )
    }

    pub fn device_config(&self) -> Result<Option<DeviceConfig>, StoreFault> {
        if let Some(stored) = nvs_blob::read_blob::<SERVER_CONFIG_BLOB_LEN, StoredServerConfig>(
            &self.nvs,
            KEY_SERVER_CONFIG,
        )? {
            if stored.version != 1 {
                log::error!(
                    "stored server config version {} is not supported",
                    stored.version
                );
                return Err(StoreFault::UnsupportedVersion);
            }
            let cfg = DeviceConfig {
                server_url: stored.server_url,
                auth_token: stored.auth_token,
            };
            validate_device_config(&cfg).map_err(|err| {
                log::error!("stored server config is unusable: {err}");
                StoreFault::Corrupt
            })?;
            return Ok(Some(cfg));
        }
        let Some(server_url) =
            nvs_blob::read_scalar::<SERVER_CONFIG_MAX_LEN>(&self.nvs, KEY_SERVER_URL)?
        else {
            return Ok(None);
        };
        let auth_token = nvs_blob::read_scalar::<SERVER_CONFIG_MAX_LEN>(&self.nvs, KEY_AUTH_TOKEN)?
            .unwrap_or_default();
        let cfg = DeviceConfig {
            server_url,
            auth_token,
        };
        validate_device_config(&cfg).map_err(|err| {
            log::error!("stored server config is unusable: {err}");
            StoreFault::Corrupt
        })?;
        Ok(Some(cfg))
    }

    pub fn save_device_config(&self, cfg: &DeviceConfig) -> Result<()> {
        validate_device_config(cfg)?;
        nvs_blob::write_blob::<SERVER_CONFIG_BLOB_LEN, _>(
            &self.nvs,
            KEY_SERVER_CONFIG,
            &StoredServerConfig {
                version: 1,
                server_url: cfg.server_url.clone(),
                auth_token: cfg.auth_token.clone(),
            },
        )
    }

    pub fn save_sync_apply_journal(&self, data: &inkwash_logic::app::SyncedData) -> Result<()> {
        nvs_blob::write_blob::<SYNC_APPLY_JOURNAL_MAX_LEN, _>(
            &self.nvs,
            KEY_SYNC_APPLY_JOURNAL,
            data,
        )
    }

    pub fn sync_apply_journal(&self) -> Result<Option<inkwash_logic::app::SyncedData>, StoreFault> {
        nvs_blob::read_dynamic_blob(
            &self.nvs,
            KEY_SYNC_APPLY_JOURNAL,
            SYNC_APPLY_JOURNAL_MAX_LEN,
        )
    }

    pub fn clear_sync_apply_journal(&self) -> Result<()> {
        self.nvs
            .remove(KEY_SYNC_APPLY_JOURNAL)
            .map(|_| ())
            .map_err(|e| anyhow!("NVS remove({KEY_SYNC_APPLY_JOURNAL}) failed: {e}"))
    }

    pub fn sync_interval_minutes(&self) -> Result<Option<u16>, StoreFault> {
        self.read_num::<u16>(KEY_SYNC_INTERVAL_MIN, "sync interval")
    }

    pub fn set_sync_interval_minutes(&self, minutes: u16) -> Result<()> {
        if !(1..=1440).contains(&minutes) {
            return Err(anyhow!("sync interval must be between 1 and 1440 minutes"));
        }
        self.write_num(KEY_SYNC_INTERVAL_MIN, minutes)
    }

    pub fn last_sync_epoch(&self) -> Result<Option<u64>, StoreFault> {
        self.read_num::<u64>(KEY_LAST_SYNC_EPOCH, "last-sync epoch")
    }

    pub fn set_last_sync_epoch(&self, epoch: u64) -> Result<()> {
        self.write_num(KEY_LAST_SYNC_EPOCH, epoch)
    }

    pub fn rtc_align_epoch(&self) -> Result<Option<u64>, StoreFault> {
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

    pub fn timezone_offset_minutes(&self) -> Result<Option<i16>, StoreFault> {
        self.read_num::<i16>(KEY_TIMEZONE_OFFSET, "timezone offset")
    }

    pub fn save_timezone_offset_minutes(&self, offset: i16) -> Result<()> {
        if !(-720..=840).contains(&offset) {
            return Err(anyhow!(
                "timezone offset must be between -720 and 840 minutes"
            ));
        }
        self.write_num(KEY_TIMEZONE_OFFSET, offset)
    }

    pub fn todo_reminded_date(&self) -> Result<Option<String>, StoreFault> {
        nvs_blob::read_scalar::<NUM_STR_MAX_LEN>(&self.nvs, KEY_TODO_REMINDED_DATE)
    }

    pub fn set_todo_reminded_date(&self, date: &str) -> Result<()> {
        nvs_blob::write_scalar(&self.nvs, KEY_TODO_REMINDED_DATE, date)
    }
}
