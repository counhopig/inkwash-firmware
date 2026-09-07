//! HTTPS sync client for pulling alarms and todos from the inkwash-server
//! (contract: `docs/sync-api.md`).
//!
//! `fetch_and_apply` handles conditional requests via `If-None-Match`/ETag,
//! parses the sync response, writes the fetched data into the local NVS
//! stores, and re-arms the RTC hardware alarm to whichever is now nearest -
//! this is the only place outside `screens.rs`'s on-device edit paths that
//! mutates `alarms::AlarmStore`/`todos::TodoStore`.

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

/// `SyncResponse` and `validate_sync_response` (the sync merge rules) live
/// in `inkwash-logic` so they can be unit-tested on the host. This crate
/// is the single source of truth; everything below just wires the result
/// into HTTP + NVS.
use inkwash_logic::sync_validate::{validate_sync_response, SyncResponse};

/// Response bodies from a compliant server are small (alarms/todos are
/// themselves capped to a couple KB each in NVS - see `alarms::BLOB_BUF_LEN`
/// / `todos::BLOB_BUF_LEN`); the inbox adds up to 20 small items.
const RESPONSE_BUF_LEN: usize = 16384;

/// Fixed-capacity response storage allocated explicitly in PSRAM. Keeping
/// this 16 KiB buffer off the pthread stack leaves scarce internal RAM for
/// mbedTLS and the hardware AES driver during the response read.
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

/// Explicit socket timeout for every HTTP client used in this module.
/// `Configuration::timeout` defaults to `None`, which leaves the native
/// `esp_http_client`'s own default in effect rather than a value under our
/// control. It bounds each blocked read inside `request.submit()` below -
/// a call that also contains the TLS handshake, so the whole thing runs
/// without a single `watchdog::feed()` for up to handshake + this value.
/// Both live TWDT aborts died in exactly that window: handshake
/// ~1-4.7s plus a stalled submit 5-8.7s blew the 10s watchdog budget even
/// with the old 8s timeout. 5s keeps handshake + submit + error teardown
/// under `CONFIG_ESP_TASK_WDT_TIMEOUT_S` (20s) on a flaky link while still
/// being generous for a real server over the public internet.
const HTTP_TIMEOUT: Duration = Duration::from_secs(5);

/// Outcome of a bidirectional sync after the merged server state is fetched.
/// The merged data is returned to the caller - the network task never
/// writes NVS (the state machine applies it via `Effect::ApplySyncedData`).
#[derive(Clone, Debug)]
pub enum SyncOutcome {
    Applied {
        /// Merged alarms the server returned (authoritative).
        alarms: Vec<inkwash_logic::alarm_schedule::StoredAlarm>,
        /// Merged todos the server returned (authoritative).
        todos: Vec<crate::todos::Todo>,
        /// Inbox items the server returned (authoritative).
        inbox: Vec<crate::inbox::InboxItem>,
        /// Inbox sequence numbers the server confirmed as read.
        inbox_read_acked: Vec<u64>,
        inbox_truncated: bool,
        etag: Option<String>,
        /// Alarm `local_id`s uploaded during this sync — the snapshot taken
        /// before the request. On apply, only these IDs are cleared from the
        /// dirty set so edits made during the round-trip survive (P1-3).
        uploaded_alarm_ids: Vec<u8>,
        /// Todo `local_id`s uploaded during this sync — same snapshot
        /// semantics as above.
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

/// Reads `read`'s body into `buf` until it fills, the peer closes the
/// connection (a `0`-byte read), or the per-call socket read times out
/// (`HTTP_TIMEOUT`, surfaced as `Err` by the underlying connection). Feeds
/// the task watchdog between every read so a connection that keeps trickling
/// small chunks in - never hitting the idle timeout, but also never handing
/// control back for long enough between chunks - still cannot starve the
/// TWDT the way one unbroken `try_read_full` call could.
///
/// Returns `Err` when the buffer fills but the server still has more data —
/// a truncated body would fail JSON parsing downstream with a misleading
/// "format corrupted" error; detecting it here surfaces "response too large"
/// instead so the user can trim their alarm/todo/inbox list.
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
    // Buffer is full — check whether the server still has more data.
    if offset == buf.len() {
        let probe = response.read(&mut [0u8; 1])
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

/// One complete HTTPS POST round-trip shared by `fetch_and_apply` and
/// `poll_urgent` (which used to hand-roll identical copies of every step):
/// builds the TLS/socket `HttpConfiguration` (cert bundle + `HTTP_TIMEOUT`),
/// assembles the JSON headers (`accept`/`content-type`/`content-length`)
/// plus any `extra_headers` and the optional Bearer auth header for a
/// non-empty `token`, streams `body`, submits, then - after a watchdog feed -
/// verifies the status is 200 (`err_label` names the caller in the failure
/// message) and reads the whole body into `buf` through `read_body_fully`.
/// That last step is load-bearing: the body MUST go through the
/// watchdog-feeding read loop rather than one unbroken blocking read, the
/// prime suspect of the live TWDT abort documented on `read_body_fully`.
/// Returns `(bytes_read, etag)` -
/// the response's `etag` header if present, surfaced because
/// `fetch_and_apply` needs it for conditional-request bookkeeping and the
/// connection is gone once this helper returns.
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
    // Feed on the error path too: a timed-out/stalled submit returns here
    // without ever reaching the `feed()` below, and the handshake+submit
    // window just elapsed was entirely un-feedable - the exact shape of
    // both TWDT aborts. Any post-error teardown in the caller
    // (NVS write, disconnect, redraw) now runs with a fresh budget.
    watchdog::feed();
    let mut response = submit.map_err(|e| anyhow!("POST {url} failed: {e}"))?;
    if response.status() != 200 {
        return Err(anyhow!("{err_label}: HTTP {}", response.status()));
    }

    let etag = response.header("etag").map(|s| s.to_string());
    let bytes_read = read_body_fully(&mut response, buf)?;
    Ok((bytes_read, etag))
}

/// Fetches alarms and todos from `server_url`, applying conditional-request
/// semantics. Uploads local mutable flags first and returns the merged
/// server data's counts and new ETag. Any HTTP error, TLS error, or
/// JSON parse error returns `Err(...)` with a descriptive message rather
/// than panicking.
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
    // Two-way sync: only items the user actually changed on-device are
    // uploaded (alarm `enabled`, todo `done`). Server-side
    // edits therefore survive the next sync instead of being clobbered by
    // the device's stale copy; the dirty sets are cleared on success below.
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
    // The full HTTP round-trip (config, headers, body write, status check,
    // watchdog-feeding body read) lives in `https_post` - see its doc
    // comment for why the body read must keep feeding the task watchdog.
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
        // Snapshot of dirty IDs captured before the request — passed
        // through to ApplySyncedData so only these are cleared on apply,
        // preserving any local edits made during the round-trip (P1-3).
        uploaded_alarm_ids: dirty_alarms,
        uploaded_todo_ids: dirty_todos,
    })
}

/// Result of a full sync, plus the RTC-relevant side effects the main loop
/// must apply itself: the sync task runs on its own thread and never
/// touches the I2C bus (the shared-`Pcf8563`
/// route was rejected because `SharedI2c` is `Rc<RefCell<...>>`, which is
/// not `Send`; converting it to `Arc<Mutex<...>>` would ripple through the
/// audio/NFC drivers for no functional gain over the receipt).
pub struct SyncResult {
    pub outcome: Result<SyncOutcome>,
    /// NTP epoch seconds for the daily RTC alignment, when the alignment
    /// gate is due and Wi-Fi was up long enough for SNTP. The main loop
    /// writes this into the PCF8563.
    pub ntp_epoch: Option<u64>,
}

/// Full "Sync Now" flow, executed on the sync task (see `sync_task.rs`):
/// connects Wi-Fi using the stored credentials (via the process's one
/// shared `WifiManager` - see its doc comment for why a fresh `EspWifi`
/// per call crashes), loads server config + cached ETag, calls
/// `fetch_and_apply`, persists any new ETag, then disconnects Wi-Fi again -
/// mirroring `main.rs`'s boot-time "connect only for as long as needed"
/// pattern. The RTC hardware alarm and NTP alignment are deferred to the
/// main loop via [`SyncResult`].
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

        // A second in-process Wi-Fi connect used to crash; the codebase once
        // worked around it by deep-sleep restarting for a fresh session (see
        // `wifi::WifiManager`'s doc comment for the full history). The
        // manual `esp_wifi_scan_start()` that `connect()` ran before every
        // connection turned out to be the trigger - credentials here always
        // come from the desktop's `SetWifi` command, so that scan was never
        // needed and has been removed. Multiple connects per boot now work;
        // no restart needed.
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

        // Daily NTP alignment rides the live connection (SNTP needs Wi-Fi
        // up); skipped on failed syncs so a dead network retries next time.
        let ntp_epoch = if outcome.is_ok() {
            maybe_ntp_epoch(counters, now)
        } else {
            None
        };

        // Disconnect regardless of outcome - nothing else needs Wi-Fi to
        // stay connected after this.
        wifi_mgr.disconnect();

        // The network task writes NO NVS here: the sync ETag is persisted by
        // the state machine (Effect::ApplySyncedData) and the last-sync
        // timestamp is recorded by the main loop from the receipt
        // (apply_sync_side_effects). This task only transports.

        // Unwrap at the end: connect/fetch failures early-return via `?`,
        // but the disconnect above must run first, exactly like the
        // original synchronous flow.
        Ok((outcome?, ntp_epoch))
    })() {
        Ok((outcome, ntp_epoch)) => (Ok(outcome), ntp_epoch),
        Err(err) => (Err(err), None),
    };
    SyncResult { outcome, ntp_epoch }
}

/// Captures the NTP epoch for the daily RTC alignment when the alignment
/// gate is due. Must be called while Wi-Fi is still connected (SNTP cannot
/// start on a dead link) - `sync_now` calls it before its final
/// disconnect, so the daily check piggybacks on whichever sync path
/// happened to run. The main loop applies the returned epoch to the
/// PCF8563 and records the alignment timestamp (the sync task never
/// touches the I2C bus); a failed NTP round-trip retries on the next sync.
fn maybe_ntp_epoch(counters: &PersistedCounters, now: &DateTime) -> Option<u64> {
    let align_due = match counters.rtc_align_epoch() {
        // `abs_diff`, not `saturating_sub(...) >= 24h`: if the RTC clock
        // itself ever jumps backward - the boot-time VL reseed in main.rs
        // sets the clock from a stale BUILD_EPOCH_SECS without touching
        // this marker - `last` can end up *later* than `now`. A
        // `saturating_sub` of a negative delta clamps to 0, which reads as
        // "just aligned", permanently blocking any future resync attempt
        // with no error and no way to self-correct. Confirmed on hardware
        // 2026-08-25: `now` read ~2026-08-22 while the stored marker read
        // ~2026-08-24, and the device had been silently stuck on the wrong
        // date ever since. A clock running backward is exactly as strong a
        // signal that the stored marker is untrustworthy as one running
        // forward by the same amount.
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

/// Lightweight urgent-message poll: connects, sends a `POST` with the
/// `X-Inkwash-Poll` header, reads a tiny `{"urgent": bool}` response, and
/// disconnects. The server answers immediately (no hold), so the firmware can
/// call this on a short timer to detect high-priority messages without
/// keeping a long connection open or blocking the main loop for long.
///
/// Returns `Ok(true)` when the server has an unread high-priority message.
/// Errors (no Wi-Fi, no server config, network failure) are returned so the
/// caller can fall back to a regular sync or log.
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
