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
/// ESP-IDF NVS keys are limited to 15 characters; `todo_reminded_date` (18
/// chars) silently failed every write with `ESP_ERR_NVS_KEY_TOO_LONG`.
const KEY_TODO_REMINDED_DATE: &str = "todo_rem_date";

/// Maximum length of the NVS strings used for Wi-Fi credentials.
/// ESP32-S3 NVS limits a single string item to ~4000 bytes; 64 chars is
/// plenty for an SSID and WPA2 passphrase.
const WIFI_CRED_MAX_LEN: usize = 64;

/// Maximum length of the NVS strings used for server URL, auth token, and ETag.
/// Server URLs typically fit in ~256 chars, tokens often 128-256 chars, and
/// ETags vary but are rarely over 128 chars.
const SERVER_CONFIG_MAX_LEN: usize = 256;

/// Read-buffer size for every numeric/date scalar (stored as its decimal
/// string): a `u64` needs at most 20 digits and the `YYYYMMDD` reminder
/// marker 8, so one shared size covers them all.
const NUM_STR_MAX_LEN: usize = 20;

/// Pure config data shapes (`WifiCreds`, `DeviceConfig`) live in
/// `inkwash-logic` so the host-testable application state machine shares
/// the single source of truth; re-exported here so every existing
/// `storage::WifiCreds` / `storage::DeviceConfig` call site keeps working.
pub use inkwash_logic::device_config::{DeviceConfig, WifiCreds};

pub struct PersistedCounters {
    nvs: EspDefaultNvs,
}

impl PersistedCounters {
    /// `partition` is a clone of the one shared `EspDefaultNvsPartition`
    /// handle `main.rs` takes once - `EspDefaultNvsPartition::take()` is a
    /// true singleton (guarded by a global taken-flag, not a ref-counted
    /// "take a new handle" call) and errors with `ESP_ERR_INVALID_STATE` if
    /// called again while an earlier handle is still alive, so every NVS
    /// namespace opener in this codebase (`AlarmStore`, `TodoStore`, this
    /// one) must share a single taken partition rather than each calling
    /// `take()` independently.
    pub fn open(partition: EspDefaultNvsPartition) -> Result<Self> {
        let nvs = EspDefaultNvs::new(partition, NAMESPACE, true)
            .map_err(|e| anyhow!("failed to open NVS namespace '{NAMESPACE}': {e}"))?;
        Ok(Self { nvs })
    }

    /// Parses a decimal-string scalar into `T`; `label` only feeds the error
    /// message (mirroring each former hand-written getter's wording).
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

    /// Stores a numeric scalar as its decimal string.
    fn write_num<T: ToString>(&self, key: &str, value: T) -> Result<()> {
        nvs_blob::write_scalar(&self.nvs, key, &value.to_string())
    }

    /// Reads the Wi-Fi credentials stored in NVS, if any.
    pub fn wifi_creds(&self) -> Result<Option<WifiCreds>> {
        let Some(ssid) = nvs_blob::read_scalar::<WIFI_CRED_MAX_LEN>(&self.nvs, KEY_WIFI_SSID)?
        else {
            return Ok(None);
        };
        let password = nvs_blob::read_scalar::<WIFI_CRED_MAX_LEN>(&self.nvs, KEY_WIFI_PASS)?
            .unwrap_or_default();
        Ok(Some(WifiCreds { ssid, password }))
    }

    /// Stores the Wi-Fi credentials in NVS for later connections.
    pub fn save_wifi_creds(&self, creds: &WifiCreds) -> Result<()> {
        nvs_blob::write_scalar(&self.nvs, KEY_WIFI_SSID, &creds.ssid)?;
        nvs_blob::write_scalar(&self.nvs, KEY_WIFI_PASS, &creds.password)
    }

    /// Reads the server configuration (URL and auth token) from NVS, if any.
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

    /// Stores the server configuration (URL and auth token) in NVS.
    pub fn save_device_config(&self, cfg: &DeviceConfig) -> Result<()> {
        nvs_blob::write_scalar(&self.nvs, KEY_SERVER_URL, &cfg.server_url)?;
        nvs_blob::write_scalar(&self.nvs, KEY_AUTH_TOKEN, &cfg.auth_token)
    }

    /// Reads the last-seen sync ETag from NVS for conditional requests, if any.
    pub fn sync_etag(&self) -> Result<Option<String>> {
        nvs_blob::read_scalar::<SERVER_CONFIG_MAX_LEN>(&self.nvs, KEY_SYNC_ETAG)
    }

    /// Stores the sync ETag in NVS for future conditional requests.
    pub fn save_sync_etag(&self, etag: &str) -> Result<()> {
        nvs_blob::write_scalar(&self.nvs, KEY_SYNC_ETAG, etag)
    }

    /// Invalidates conditional-sync state after changing server identity.
    pub fn clear_sync_etag(&self) -> Result<()> {
        self.nvs
            .remove(KEY_SYNC_ETAG)
            .map(|_| ())
            .map_err(|e| anyhow!("NVS remove({KEY_SYNC_ETAG}) failed: {e}"))
    }

    /// Automatic-sync interval in minutes. The device re-syncs with the
    /// configured server every this many minutes while running on Home (see
    /// `main.rs`'s periodic sync check). Defaults to 60 (1 hour).
    pub fn sync_interval_minutes(&self) -> Result<u16> {
        Ok(self
            .read_num::<u16>(KEY_SYNC_INTERVAL_MIN, "sync interval")?
            .unwrap_or(60))
    }

    /// Sets the automatic-sync interval in minutes.
    pub fn set_sync_interval_minutes(&self, minutes: u16) -> Result<()> {
        if !(1..=1440).contains(&minutes) {
            return Err(anyhow!("sync interval must be between 1 and 1440 minutes"));
        }
        self.write_num(KEY_SYNC_INTERVAL_MIN, minutes)
    }

    /// Unix seconds of the last successful sync, if any. `main.rs` uses this
    /// with `sync_interval_minutes` to decide when to trigger the next
    /// automatic sync.
    pub fn last_sync_epoch(&self) -> Result<Option<u64>> {
        self.read_num::<u64>(KEY_LAST_SYNC_EPOCH, "last-sync epoch")
    }

    /// Records the time of a successful sync so the periodic checker knows
    /// how long it has been since the last one.
    pub fn set_last_sync_epoch(&self, epoch: u64) -> Result<()> {
        self.write_num(KEY_LAST_SYNC_EPOCH, epoch)
    }

    /// Unix seconds of the last successful periodic NTP RTC alignment, if
    /// any. `main.rs` uses this to resync the PCF8563 roughly once a day,
    /// keeping the wall-clock boundaries the cron sync aligns to accurate.
    pub fn rtc_align_epoch(&self) -> Result<Option<u64>> {
        self.read_num::<u64>(KEY_RTC_ALIGN_EPOCH, "rtc-align epoch")
    }

    /// Records when the RTC was last aligned via NTP.
    pub fn set_rtc_align_epoch(&self, epoch: u64) -> Result<()> {
        self.write_num(KEY_RTC_ALIGN_EPOCH, epoch)
    }

    /// Drops the last-alignment marker. Call whenever the RTC clock is set
    /// from a source other than a confirmed-successful NTP sync (the
    /// boot-time VL reseed in `main.rs`, from a stale build-time constant) -
    /// leaving a stale marker in place can leave it *later* than the
    /// now-reseeded clock, which would otherwise block every future
    /// alignment attempt indefinitely (see `sync.rs::maybe_align_rtc`'s doc
    /// comment).
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

    /// The last calendar date (as `YYYYMMDD`) on which the due-todo
    /// reminder fired, if ever. `main.rs` compares it against today so a
    /// due `High` todo rings once per day, not every poll.
    pub fn todo_reminded_date(&self) -> Result<Option<String>> {
        nvs_blob::read_scalar::<NUM_STR_MAX_LEN>(&self.nvs, KEY_TODO_REMINDED_DATE)
    }

    /// Records that the due-todo reminder fired on `date` (`YYYYMMDD`).
    pub fn set_todo_reminded_date(&self, date: &str) -> Result<()> {
        nvs_blob::write_scalar(&self.nvs, KEY_TODO_REMINDED_DATE, date)
    }
}
