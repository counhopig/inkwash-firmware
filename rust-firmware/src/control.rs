//! Shared command/reply protocol for USB and BLE control channels.
//!
//! **Wire format:** Commands and replies are framed by the transport layer
//! (e.g., `usb_console.rs`) with a sentinel prefix to distinguish them from
//! ordinary log output:
//! - Command frame: `>>IW {json}\n` (one JSON object per line)
//! - Reply frame: `<<IW {json}\n` (one JSON object per line)
//!
//! See `docs/control-protocol.md` for the complete specification.

use anyhow::{anyhow, Result};

use crate::ctx::DeviceContext;
use crate::rtc::DateTime;
use crate::storage::{DeviceConfig, WifiCreds};
use crate::sync_task::OpSource;

/// Pure wire-protocol shapes (`Channel`, `Command`, `Reply`) now live in
/// `inkwash-logic` so the host-testable application state machine shares
/// the single source of truth; re-exported here so every existing
/// `control::Command` / `control::Reply` / `control::Channel` call site
/// keeps working unchanged.
pub use inkwash_logic::protocol::{Channel, Command, Reply};

/// Parses a command from a JSON string, along with the client's optional
/// correlation `id` (any string; absent if the client didn't send one - see
/// docs/control-protocol.md's "Request Correlation" section). Returns `Err`
/// with a descriptive message if parsing fails (e.g., invalid JSON, missing
/// required field, unknown command). Extracting `id` via `serde_json::Value`
/// first, rather than adding it as a field on every `Command` variant, keeps
/// old clients that never send `id` and this parser's rejection of malformed
/// commands both unaffected - serde already ignores unknown object keys by
/// default, so an `id` field is invisible to `Command`'s own derive either
/// way.
pub fn parse_command(line: &str) -> Result<(Option<String>, Command)> {
    let value: serde_json::Value =
        serde_json::from_str(line).map_err(|e| anyhow!("Failed to parse command: {e}"))?;
    let id = value
        .get("id")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let cmd = serde_json::from_value(value).map_err(|e| anyhow!("Failed to parse command: {e}"))?;
    Ok((id, cmd))
}

/// Renders a reply as JSON, echoing back `id` (the same correlation id the
/// triggering command carried, if any) as an extra top-level key. Falls back
/// to a hardcoded error JSON string if serialization somehow fails (should be
/// extremely rare, but we don't want to panic here).
pub fn render_reply(reply: &Reply, id: Option<&str>) -> String {
    let mut value = match serde_json::to_value(reply) {
        Ok(v) => v,
        Err(_) => return r#"{"status":"error","message":"Failed to serialize reply"}"#.to_string(),
    };
    if let (Some(id), Some(obj)) = (id, value.as_object_mut()) {
        obj.insert("id".to_string(), serde_json::Value::String(id.to_string()));
    }
    value.to_string()
}

/// Executes a command and returns a reply. This is the single point where
/// both USB (Phase 4) and BLE (Phase 5) control channels will call into,
/// so it must remain transport-agnostic. It accesses the board (for
/// RTC and Wi-Fi state), the stores (for syncing), and the time.
///
/// `id` (the client's correlation id, if any) is checked against
/// `ctx.last_command` before doing any work: a client resends a command
/// verbatim - same `id` - whenever it hasn't seen a reply yet (USB boot
/// reset, a `Busy` reply, or just a slow op like `SetWifi`/`SyncNow` taking
/// longer than the client's retry interval), and the transports queue every
/// resent copy as an independent command with no dedup of their own (see
/// `usb_console::UsbConsole::poll_command`). Without this check, a command
/// that takes a while to complete gets replayed - fully re-executed, not
/// just re-acknowledged - once for every resend still queued by the time
/// the device catches up. Confirmed on hardware 2026-08-25: a single
/// `set_wifi` ran 10 times (9 extra Wi-Fi disconnect/reconnect/save cycles)
/// and `sync_now` ran twice, both because the device was busy long enough
/// for the desktop's 2s retry to queue several duplicates. The cache key is
/// `(id, Command)` together, not
/// `id` alone: the desktop's request-id counter restarts at 1 every process
/// launch, so `id` collisions *across sessions* are the common case, not an
/// edge case - keying on content too means a same-numbered command from an
/// unrelated session can never replay a stale reply for the wrong command.
pub fn dispatch(
    ctx: &mut DeviceContext,
    channel: Channel,
    id: Option<&str>,
    cmd: Command,
    now: Option<&DateTime>,
) -> Reply {
    if let Some(id) = id {
        if let Some((last_id, last_cmd, last_reply)) = &ctx.last_command {
            if last_id == id && *last_cmd == cmd {
                log::info!(
                    "Duplicate command id={id}; replaying cached reply without re-executing"
                );
                return last_reply.clone();
            }
        }
    }
    let cmd_for_cache = cmd.clone();
    let reply = dispatch_inner(ctx, channel, cmd, now, id);
    if let Some(id) = id {
        ctx.last_command = Some((id.to_string(), cmd_for_cache, reply.clone()));
    }
    reply
}

fn dispatch_inner(
    ctx: &mut DeviceContext,
    channel: Channel,
    cmd: Command,
    now: Option<&DateTime>,
    id: Option<&str>,
) -> Reply {
    match cmd {
        Command::SetWifi {
            ref ssid,
            ref password,
        } => {
            // Verification + NVS save run on the sync task (it owns the
            // Wi-Fi driver); the reply is deferred and delivered via
            // `DeviceContext::poll_wifi_ops`. `Pending` tells the client
            // the command was accepted and the real result is coming; a
            // second op already in flight replies `Busy` instead - not
            // executed, safe to retry.
            let creds = WifiCreds {
                ssid: ssid.clone(),
                password: password.clone(),
            };
            let source = match channel {
                Channel::Usb => OpSource::Usb {
                    id: id.map(str::to_owned),
                },
                Channel::Ble => OpSource::Ble {
                    id: id.map(str::to_owned),
                },
            };
            match ctx.start_set_wifi(creds, source, cmd.clone()) {
                Ok(true) => Reply::Pending,
                Ok(false) => Reply::Busy,
                Err(err) => Reply::Error {
                    message: format!("Failed to start Wi-Fi verification: {err}"),
                },
            }
        }

        Command::SetServer { url, token } => {
            // Server config is saved immediately without verification -
            // it's hard to validate a URL without a network request, and
            // we want this command to be fast.
            let cfg = DeviceConfig {
                server_url: url,
                auth_token: token,
            };
            match ctx.counters.save_device_config(&cfg) {
                Ok(()) => {
                    if let Err(err) = ctx.counters.clear_sync_etag() {
                        log::warn!(
                            "Server config saved but old sync ETag could not be cleared: {err}"
                        );
                    }
                    log::info!("USB control: server config saved");
                    Reply::Ok
                }
                Err(err) => {
                    log::warn!("USB control: server config save failed: {err}");
                    Reply::Error {
                        message: format!("Failed to save server config: {err}"),
                    }
                }
            }
        }

        Command::SyncNow => {
            // The sync runs on the sync task (it owns Wi-Fi); the reply is
            // deferred and delivered via `DeviceContext::poll_wifi_ops`.
            // `Pending` means accepted - the real result follows as a
            // deferred reply with the same id.
            let Some(now_dt) = now else {
                return Reply::Error {
                    message: "System time not available".to_string(),
                };
            };
            let source = match channel {
                Channel::Usb => OpSource::Usb {
                    id: id.map(str::to_owned),
                },
                Channel::Ble => OpSource::Ble {
                    id: id.map(str::to_owned),
                },
            };
            match ctx.start_sync(source, *now_dt, cmd.clone()) {
                Ok(true) => Reply::Pending,
                Ok(false) => Reply::Busy,
                Err(err) => Reply::Error {
                    message: format!("Failed to start sync: {err}"),
                },
            }
        }

        Command::GetStatus => {
            // Report whatever we can cheaply check without network activity.
            let wifi_configured = ctx
                .counters
                .wifi_creds()
                .map(|opt| opt.is_some())
                .unwrap_or(false);
            let server_configured = ctx
                .counters
                .device_config()
                .map(|opt| opt.is_some())
                .unwrap_or(false);

            // Report Wi-Fi connection state (last-known, from the sync
            // task - the main loop never touches the driver) plus what is
            // actually stored in NVS. Secrets (Wi-Fi password, server auth
            // token) are never sent back to the client - only whether one
            // is set.
            let wifi_connected = ctx.sync.is_connected();
            let (wifi_ssid, wifi_has_password) = ctx
                .counters
                .wifi_creds()
                .ok()
                .flatten()
                .map(|creds| (Some(creds.ssid), !creds.password.is_empty()))
                .unwrap_or((None, false));
            let (server_url, server_has_token) = ctx
                .counters
                .device_config()
                .ok()
                .flatten()
                .map(|cfg| (Some(cfg.server_url), !cfg.auth_token.is_empty()))
                .unwrap_or((None, false));
            let timezone_offset_minutes = ctx.counters.timezone_offset_minutes().unwrap_or(0);

            Reply::Status {
                wifi_configured,
                server_configured,
                wifi_connected,
                wifi_ssid,
                wifi_has_password,
                server_url,
                server_has_token,
                timezone_offset_minutes,
            }
        }

        Command::ClearAlarms => match ctx.alarm_store.save(&[]) {
            Ok(()) => match ctx.board.rtc.clear_alarm() {
                Ok(()) => {
                    log::info!("USB/BLE control: all alarms cleared");
                    Reply::Ok
                }
                Err(err) => Reply::Error {
                    message: format!("Alarms cleared, but RTC disarm failed: {err}"),
                },
            },
            Err(err) => Reply::Error {
                message: format!("Failed to clear alarms: {err}"),
            },
        },

        Command::SetTimezone { offset_minutes } => {
            if !(-720..=840).contains(&offset_minutes) {
                return Reply::Error {
                    message: "Timezone offset must be between -720 and 840 minutes".to_string(),
                };
            }
            let old_offset = ctx.counters.timezone_offset_minutes().unwrap_or(0);
            let adjusted = ctx
                .board
                .rtc
                .read_time()
                .map(|dt| dt.shifted_minutes((offset_minutes - old_offset) as i32));
            // Commit the hardware clock first. Persisting the new offset
            // before this write would make a retry calculate a zero delta
            // after an RTC failure, permanently leaving the two out of sync.
            match adjusted.and_then(|dt| ctx.board.rtc.write_time(&dt)) {
                Ok(()) => match ctx.counters.save_timezone_offset_minutes(offset_minutes) {
                    Ok(()) => Reply::Ok,
                    Err(err) => {
                        // Best-effort rollback keeps the RTC consistent with
                        // the still-persisted old offset.
                        if let Ok(dt) = ctx.board.rtc.read_time() {
                            let _ = ctx.board.rtc.write_time(
                                &dt.shifted_minutes((old_offset - offset_minutes) as i32),
                            );
                        }
                        Reply::Error {
                            message: format!("RTC updated, but timezone save failed: {err}"),
                        }
                    }
                },
                Err(err) => Reply::Error {
                    message: format!("Failed to update RTC timezone: {err}"),
                },
            }
        }
    }
}
