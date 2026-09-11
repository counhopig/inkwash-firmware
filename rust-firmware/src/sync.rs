use anyhow::{anyhow, Result};
use embedded_svc::http::client::Client as HttpClient;
use embedded_svc::http::Method;
use esp_idf_svc::http::client::{Configuration as HttpConfiguration, EspHttpConnection};
use serde::Serialize;
use std::ptr::NonNull;
use std::time::Duration;

use crate::alarms::AlarmStore;
use crate::inbox::InboxStore;
use crate::rtc::DateTime;
use crate::storage::PersistedCounters;
use crate::todos::TodoStore;
use crate::watchdog;
use crate::wifi;

use inkwash_logic::sync_validate::{validate_sync_response, SyncResponse};

const RESPONSE_BUF_LEN: usize = 16384;

struct PsramBuffer {
    ptr: NonNull<u8>,
    len: usize,
}

impl PsramBuffer {
    fn new(len: usize) -> Result<Self> {
        let caps = esp_idf_svc::sys::MALLOC_CAP_SPIRAM | esp_idf_svc::sys::MALLOC_CAP_8BIT;
        let ptr = unsafe { esp_idf_svc::sys::heap_caps_calloc(len, 1, caps) } as *mut u8;
        let ptr = NonNull::new(ptr).ok_or_else(|| {
            anyhow!("failed to allocate {len}-byte sync response buffer in PSRAM")
        })?;
        Ok(Self { ptr, len })
    }

    fn as_mut_slice(&mut self) -> &mut [u8] {
        unsafe { std::slice::from_raw_parts_mut(self.ptr.as_ptr(), self.len) }
    }
}

impl Drop for PsramBuffer {
    fn drop(&mut self) {
        unsafe { esp_idf_svc::sys::heap_caps_free(self.ptr.as_ptr().cast()) };
    }
}

const HTTP_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone, Debug)]
pub enum SyncOutcome {
    Applied {
        alarms: Vec<inkwash_logic::alarm_schedule::StoredAlarm>,

        todos: Vec<crate::todos::Todo>,

        inbox: Vec<crate::inbox::InboxItem>,

        inbox_read_acked: Vec<u64>,
        inbox_truncated: bool,
        etag: Option<String>,

        uploaded_alarm_ids: Vec<u8>,

        uploaded_todo_ids: Vec<u8>,
    },
}

#[derive(Debug, Serialize)]
struct DeviceSyncRequest {
    alarms: Vec<DeviceAlarmState>,
    todos: Vec<DeviceTodoState>,
    #[serde(default)]
    inbox_read: Vec<u64>,
}

#[derive(Debug, Serialize)]
struct DeviceAlarmState {
    id: u8,
    enabled: bool,
}

#[derive(Debug, Serialize)]
struct DeviceTodoState {
    id: u8,
    done: bool,
}

fn read_body_fully(response: &mut impl embedded_svc::io::Read, buf: &mut [u8]) -> Result<usize> {
    let mut offset = 0;
    while offset < buf.len() {
        watchdog::feed();
        let n = response
            .read(&mut buf[offset..])
            .map_err(|e| anyhow!("HTTP response read failed: {e:?}"))?;
        if n == 0 {
            break;
        }
        offset += n;
    }

    if offset == buf.len() {
        let probe = response
            .read(&mut [0u8; 1])
            .map_err(|e| anyhow!("HTTP response read failed during overflow probe: {e:?}"))?;
        if probe > 0 {
            return Err(anyhow!(
                "sync response exceeded the {} byte buffer; \
                 server payload too large for firmware (reduce alarms, todos, or inbox items)",
                buf.len()
            ));
        }
    }
    Ok(offset)
}

fn https_post(
    url: &str,
    token: &str,
    extra_headers: &[(&str, &str)],
    body: &[u8],
    buf: &mut [u8],
    err_label: &str,
) -> Result<(usize, Option<String>)> {
    let config = HttpConfiguration {
        crt_bundle_attach: Some(esp_idf_svc::sys::esp_crt_bundle_attach),
        timeout: Some(HTTP_TIMEOUT),
        ..Default::default()
    };
    let mut client = HttpClient::wrap(
        EspHttpConnection::new(&config)
            .map_err(|e| anyhow!("HTTP connection setup failed: {e}"))?,
    );

    let content_length = body.len().to_string();
    let mut headers: Vec<(&str, &str)> = vec![
        ("accept", "application/json"),
        ("content-type", "application/json"),
    ];
    headers.extend_from_slice(extra_headers);
    headers.push(("content-length", &content_length));
    let auth_header;
    if !token.is_empty() {
        auth_header = format!("Bearer {token}");
        headers.push(("authorization", &auth_header));
    }

    let mut request = client
        .request(Method::Post, url, &headers)
        .map_err(|e| anyhow!("POST {url} failed to start: {e}"))?;
    let mut written = 0usize;
    while written < body.len() {
        watchdog::feed();
        let count = request
            .write(&body[written..])
            .map_err(|e| anyhow!("POST {url} body write failed: {e}"))?;
        if count == 0 {
            return Err(anyhow!("POST {url} body write made no progress"));
        }
        written += count;
    }
    let submit = request.submit();

    watchdog::feed();
    let mut response = submit.map_err(|e| anyhow!("POST {url} failed: {e}"))?;
    if response.status() != 200 {
        return Err(anyhow!("{err_label}: HTTP {}", response.status()));
    }

    let etag = response.header("etag").map(|s| s.to_string());
    let bytes_read = read_body_fully(&mut response, buf)?;
    Ok((bytes_read, etag))
}

pub fn fetch_and_apply(
    server_url: &str,
    token: &str,
    _etag: Option<&str>,
    alarm_store: &AlarmStore,
    todo_store: &TodoStore,
    inbox_store: &InboxStore,
    _now: &DateTime,
) -> Result<SyncOutcome> {
    watchdog::feed();

    let local_alarms = alarm_store
        .load()
        .map_err(|e| anyhow!("failed to load local alarms for upload: {e}"))?;
    let local_todos = todo_store
        .load()
        .map_err(|e| anyhow!("failed to load local todos for upload: {e}"))?;
    let pending_read = inbox_store
        .pending_read()
        .map_err(|e| anyhow!("failed to load pending inbox reads for upload: {e}"))?;

    let dirty_alarms = alarm_store
        .dirty_ids()
        .map_err(|e| anyhow!("failed to load alarm dirty set: {e}"))?;
    let dirty_todos = todo_store
        .dirty_ids()
        .map_err(|e| anyhow!("failed to load todo dirty set: {e}"))?;
    let upload = DeviceSyncRequest {
        alarms: local_alarms
            .iter()
            .filter(|alarm| dirty_alarms.contains(&alarm.id))
            .map(|alarm| DeviceAlarmState {
                id: alarm.id,
                enabled: alarm.enabled,
            })
            .collect(),
        todos: local_todos
            .iter()
            .filter(|todo| dirty_todos.contains(&todo.id))
            .map(|todo| DeviceTodoState {
                id: todo.id,
                done: todo.done,
            })
            .collect(),
        inbox_read: pending_read,
    };
    let request_body =
        serde_json::to_vec(&upload).map_err(|e| anyhow!("device sync JSON encode failed: {e}"))?;

    let mut buf = PsramBuffer::new(RESPONSE_BUF_LEN)?;
    let (bytes_read, new_etag) = https_post(
        server_url,
        token,
        &[],
        &request_body,
        buf.as_mut_slice(),
        "sync request failed",
    )?;
    let body = &buf.as_mut_slice()[..bytes_read];
    watchdog::feed();

    let parsed: SyncResponse = serde_json::from_slice(body)
        .map_err(|e| anyhow!("sync response JSON decode failed: {e}"))?;
    validate_sync_response(&parsed).map_err(|e| anyhow!("sync response validation failed: {e}"))?;
    watchdog::feed();

    log::info!(
        "Sync fetched: {} alarms, {} todos, {} inbox (truncated={})",
        parsed.alarms.len(),
        parsed.todos.len(),
        parsed.inbox.len(),
        parsed.inbox_truncated
    );

    Ok(SyncOutcome::Applied {
        alarms: parsed.alarms,
        todos: parsed.todos,
        inbox: parsed.inbox,
        inbox_read_acked: parsed.inbox_read_acked,
        inbox_truncated: parsed.inbox_truncated,
        etag: new_etag,

        uploaded_alarm_ids: dirty_alarms,
        uploaded_todo_ids: dirty_todos,
    })
}

pub struct SyncResult {
    pub outcome: Result<SyncOutcome>,

    pub ntp_epoch: Option<u64>,
}

pub fn sync_now(
    counters: &PersistedCounters,
    wifi_mgr: &mut wifi::WifiManager,
    alarm_store: &AlarmStore,
    todo_store: &TodoStore,
    inbox_store: &InboxStore,
    now: &DateTime,
) -> SyncResult {
    let (outcome, ntp_epoch) = match (|| -> Result<(SyncOutcome, Option<u64>)> {
        let creds = counters
            .wifi_creds()
            .map_err(|e| anyhow!("failed to load Wi-Fi credentials: {e}"))?
            .ok_or_else(|| {
                anyhow!("Wi-Fi not configured; use SetWifi or the on-device wizard first")
            })?;
        let cfg = counters
            .device_config()
            .map_err(|e| anyhow!("failed to load server config: {e}"))?
            .ok_or_else(|| anyhow!("Server not configured; use SetServer first"))?;
        let etag = counters.sync_etag().ok().flatten();

        if wifi_mgr.used() {
            log::warn!(
                "Wi-Fi already used this boot session; attempting a second connect (scan-free)"
            );
        }

        wifi_mgr
            .connect(&creds)
            .map_err(|e| anyhow!("Wi-Fi connect failed: {e}"))?;

        let outcome = fetch_and_apply(
            &cfg.server_url,
            &cfg.auth_token,
            etag.as_deref(),
            alarm_store,
            todo_store,
            inbox_store,
            now,
        );

        let ntp_epoch = if outcome.is_ok() {
            maybe_ntp_epoch(counters, now)
        } else {
            None
        };

        wifi_mgr.disconnect();

        Ok((outcome?, ntp_epoch))
    })() {
        Ok((outcome, ntp_epoch)) => (Ok(outcome), ntp_epoch),
        Err(err) => (Err(err), None),
    };
    SyncResult { outcome, ntp_epoch }
}

fn maybe_ntp_epoch(counters: &PersistedCounters, now: &DateTime) -> Option<u64> {
    let align_due = match counters.rtc_align_epoch() {
        Ok(Some(last)) => now.to_unix().abs_diff(last) >= 24 * 3600,
        Ok(None) => true,
        Err(err) => {
            log::warn!("Failed to read RTC alignment time: {err}");
            false
        }
    };
    if !align_due {
        return None;
    }
    match wifi::ntp_sync_epoch() {
        Ok(epoch) => Some(epoch),
        Err(err) => {
            log::warn!("Periodic RTC alignment failed: {err}");
            None
        }
    }
}

pub fn poll_urgent(counters: &PersistedCounters, wifi_mgr: &mut wifi::WifiManager) -> Result<bool> {
    let creds = counters
        .wifi_creds()?
        .ok_or_else(|| anyhow!("Wi-Fi not configured"))?;
    let cfg = counters
        .device_config()?
        .ok_or_else(|| anyhow!("Server not configured"))?;

    wifi_mgr.connect(&creds)?;

    let result = (|| -> Result<bool> {
        let mut buf = [0u8; 256];
        let (bytes_read, _) = https_post(
            &cfg.server_url,
            &cfg.auth_token,
            &[("x-inkwash-poll", "1")],
            b"{}",
            &mut buf,
            "urgent poll failed",
        )?;
        let parsed: serde_json::Value = serde_json::from_slice(&buf[..bytes_read])
            .map_err(|e| anyhow!("urgent poll JSON decode failed: {e}"))?;
        Ok(parsed
            .get("urgent")
            .and_then(|v| v.as_bool())
            .unwrap_or(false))
    })();

    wifi_mgr.disconnect();
    result
}
