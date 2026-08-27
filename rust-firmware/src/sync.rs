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
use std::time::Duration;

use crate::alarms::{self, AlarmStore};
use crate::inbox::InboxStore;
use crate::rtc::{DateTime, Pcf8563};
use crate::storage::PersistedCounters;
use crate::todos::{Importance, TodoStore};
use crate::watchdog;
use crate::wifi;

/// `SyncResponse` and `validate_sync_response` (the sync merge rules) live
/// in `inkwash-logic` so they can be unit-tested on the host - see
/// "Remaining engineering work" #1 in `../docs/remaining-work.md`. This crate
/// is the single source of truth; everything below just wires the result
/// into HTTP + NVS.
use inkwash_logic::sync_validate::{validate_sync_response, SyncResponse};

/// Response bodies from a compliant server are small (alarms/todos are
/// themselves capped to a couple KB each in NVS - see `alarms::BLOB_BUF_LEN`
/// / `todos::BLOB_BUF_LEN`); the inbox adds up to 20 small items. A fixed
/// buffer avoids a heap-growing read loop for a payload this bounded.
const RESPONSE_BUF_LEN: usize = 16384;

/// Explicit socket timeout for every HTTP client used in this module.
/// `Configuration::timeout` defaults to `None`, which leaves the native
/// `esp_http_client`'s own default in effect rather than a value under our
/// control. It bounds each blocked read inside `request.submit()` below -
/// a call that also contains the TLS handshake, so the whole thing runs
/// without a single `watchdog::feed()` for up to handshake + this value.
/// Both live TWDT aborts (item -1) died in exactly that window: handshake
/// ~1-4.7s plus a stalled submit 5-8.7s blew the 10s watchdog budget even
/// with the old 8s timeout. 5s keeps handshake + submit + error teardown
/// under `CONFIG_ESP_TASK_WDT_TIMEOUT_S` (20s) on a flaky link while still
/// being generous for a real server over the public internet.
const HTTP_TIMEOUT: Duration = Duration::from_secs(5);

/// Outcome of a bidirectional sync after the merged server state is applied.
#[derive(Clone, Debug)]
pub enum SyncOutcome {
    Applied {
        alarm_count: usize,
        todo_count: usize,
        inbox_count: usize,
        inbox_truncated: bool,
        etag: Option<String>,
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
    /// The device may cycle importance on-device; the server merges it.
    /// Serialized as `Some(..)` always by the current firmware, but kept
    /// optional so an older firmware's upload (without the field) is still
    /// wire-compatible on the server side.
    importance: Option<Importance>,
}

/// Reads `read`'s body into `buf` until it fills, the peer closes the
/// connection (a `0`-byte read), or the per-call socket read times out
/// (`HTTP_TIMEOUT`, surfaced as `Err` by the underlying connection). Feeds
/// the task watchdog between every read so a connection that keeps trickling
/// small chunks in - never hitting the idle timeout, but also never handing
/// control back for long enough between chunks - still cannot starve the
/// TWDT the way one unbroken `try_read_full` call could.
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
/// prime suspect of the live TWDT abort documented on `read_body_fully` and
/// item -1 in `../docs/remaining-work.md`. Returns `(bytes_read, etag)` -
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
    // both item -1 TWDT aborts. Any post-error teardown in the caller
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
#[allow(clippy::too_many_arguments)]
pub fn fetch_and_apply(
    server_url: &str,
    token: &str,
    _etag: Option<&str>,
    alarm_store: &AlarmStore,
    todo_store: &TodoStore,
    inbox_store: &InboxStore,
    rtc: &mut Pcf8563,
    now: &DateTime,
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
    // uploaded (alarm `enabled`, todo `done`/`importance`). Server-side
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
                // The device owns the todo's importance (long-ENTER cycles
                // it in `screens.rs`), so it's uploaded for the server to
                // merge.
                importance: Some(todo.importance),
            })
            .collect(),
        inbox_read: pending_read,
    };
    let request_body =
        serde_json::to_vec(&upload).map_err(|e| anyhow!("device sync JSON encode failed: {e}"))?;
    // The full HTTP round-trip (config, headers, body write, status check,
    // watchdog-feeding body read) lives in `https_post` - see its doc
    // comment for why the body read must keep feeding the task watchdog.
    let mut buf = [0u8; RESPONSE_BUF_LEN];
    let (bytes_read, new_etag) = https_post(
        server_url,
        token,
        &[],
        &request_body,
        &mut buf,
        "sync request failed",
    )?;
    let body = &buf[..bytes_read];
    watchdog::feed();

    let parsed: SyncResponse = serde_json::from_slice(body)
        .map_err(|e| anyhow!("sync response JSON decode failed: {e}"))?;
    validate_sync_response(&parsed).map_err(|e| anyhow!("sync response validation failed: {e}"))?;
    watchdog::feed();

    alarm_store
        .save(&parsed.alarms)
        .map_err(|e| anyhow!("failed to save synced alarms: {e}"))?;
    todo_store
        .save(&parsed.todos)
        .map_err(|e| anyhow!("failed to save synced todos: {e}"))?;
    inbox_store
        .save(&parsed.inbox)
        .map_err(|e| anyhow!("failed to save synced inbox: {e}"))?;
    inbox_store
        .ack_read(&parsed.inbox_read_acked)
        .map_err(|e| anyhow!("failed to ack inbox reads: {e}"))?;
    watchdog::feed();
    // The merged server state now reflects everything we uploaded, so the
    // pending local changes are spent. Cleared only here, after a
    // successful round-trip - a failed sync keeps them for the retry.
    // The hardware alarm is part of the committed device state, not an
    // optional side effect. Do not acknowledge local mutations when the RTC
    // could not be brought in line with the newly stored alarm list.
    alarms::program_hardware_alarm(rtc, &parsed.alarms, now)
        .map_err(|e| anyhow!("failed to reprogram hardware alarm after sync: {e}"))?;
    alarm_store
        .clear_dirty()
        .map_err(|e| anyhow!("failed to clear alarm dirty set: {e}"))?;
    todo_store
        .clear_dirty()
        .map_err(|e| anyhow!("failed to clear todo dirty set: {e}"))?;
    watchdog::feed();

    log::info!(
        "Sync applied: {} alarms, {} todos, {} inbox (truncated={})",
        parsed.alarms.len(),
        parsed.todos.len(),
        parsed.inbox.len(),
        parsed.inbox_truncated
    );

    Ok(SyncOutcome::Applied {
        alarm_count: parsed.alarms.len(),
        todo_count: parsed.todos.len(),
        inbox_count: parsed.inbox.len(),
        inbox_truncated: parsed.inbox_truncated,
        etag: new_etag,
    })
}

/// Full "Sync Now" flow, shared by the on-device menu (`screens.rs`) and
/// the USB/BLE `SyncNow` command (`control.rs`): connects Wi-Fi using the
/// stored credentials (via the process's one shared `WifiManager` - see
/// its doc comment for why a fresh `EspWifi` per call crashes), loads
/// server config + cached ETag, calls `fetch_and_apply`, persists any new
/// ETag, then disconnects Wi-Fi again - mirroring `main.rs`'s boot-time
/// "connect only for as long as needed" pattern. `fetch_and_apply` itself
/// has no idea whether Wi-Fi is up; both call sites used to assume it
/// already was (main.rs disconnects after the initial NTP sync, so by the
/// time a user actually presses "Sync Now" minutes or hours later, there
/// is no active STA connection) - confirmed as a real bug via an
/// end-to-end hardware test (`ESP_ERR_HTTP_CONNECT` / "Host is
/// unreachable"), not just a hypothetical.
pub fn sync_now(
    counters: &PersistedCounters,
    wifi_mgr: &mut wifi::WifiManager,
    alarm_store: &AlarmStore,
    todo_store: &TodoStore,
    inbox_store: &InboxStore,
    rtc: &mut Pcf8563,
    now: &DateTime,
) -> Result<SyncOutcome> {
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
    // `wifi::WifiManager`'s doc comment for the full history). The manual
    // `esp_wifi_scan_start()` that `connect()` ran before every connection
    // turned out to be the trigger - credentials here always come from the
    // desktop's `SetWifi` command, so that scan was never needed and has
    // been removed. Multiple connects per boot now work; no restart needed.
    if wifi_mgr.used() {
        log::warn!("Wi-Fi already used this boot session; attempting a second connect (scan-free)");
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
        rtc,
        now,
    );

    // Daily NTP RTC alignment rides the live connection (SNTP needs Wi-Fi
    // up); skipped on failed syncs so a dead network retries next time.
    if outcome.is_ok() {
        maybe_align_rtc(counters, rtc, now);
    }

    // Disconnect regardless of outcome - nothing else needs Wi-Fi to stay
    // connected after this.
    wifi_mgr.disconnect();

    if let Ok(SyncOutcome::Applied {
        etag: Some(ref new_etag),
        ..
    }) = outcome
    {
        if let Err(err) = counters.save_sync_etag(new_etag) {
            log::warn!("Failed to save sync ETag: {err}");
        }
    }
    // Record when this sync ran, so the periodic auto-sync checker
    // (`main.rs`) knows how long it has been since the last one.
    if outcome.is_ok() {
        if let Err(err) = counters.set_last_sync_epoch(now.to_unix()) {
            log::warn!("Failed to record last-sync time: {err}");
        }
    }

    outcome
}

/// Resyncs the PCF8563 over NTP once per day. Must be called while Wi-Fi
/// is still connected (SNTP cannot start on a dead link) - `sync_now`
/// calls it before its final disconnect, so the daily check piggybacks on
/// whichever sync path happened to run. A failed NTP round-trip retries on
/// the next sync; the alignment timestamp is only recorded on success.
fn maybe_align_rtc(counters: &PersistedCounters, rtc: &mut Pcf8563, now: &DateTime) {
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
        return;
    }
    let timezone_offset = counters.timezone_offset_minutes().unwrap_or(0);
    match wifi::ntp_sync_and_set_rtc(rtc, timezone_offset) {
        Ok(()) => {
            if let Err(err) = counters.set_rtc_align_epoch(now.to_unix()) {
                log::warn!("Failed to record RTC alignment time: {err}");
            }
        }
        Err(err) => log::warn!("Periodic RTC alignment failed: {err}"),
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
