//! Pure wire-protocol types shared by the USB and BLE control channels,
//! moved verbatim out of `rust-firmware/src/control.rs` (which re-exports
//! them) so the host-testable application state machine can reference the
//! same single source of truth without pulling in ESP-IDF.
//!
//! These are serde-shaped exactly as the desktop tool expects
//! (`docs/control-protocol.md`); the framing (`>>IW`/`<<IW` lines) and the
//! dispatch/execution live in the firmware crate, alongside the transport.

use serde::{Deserialize, Serialize};

/// Which transport a command arrived on. Long operations (SyncNow,
/// SetWifi) defer their reply; the deferred reply must be written back to
/// the same channel the command came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Channel {
    Usb,
    Ble,
}

/// Incoming command from a USB/BLE client.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Command {
    /// Configure Wi-Fi credentials. Will attempt to connect to verify before
    /// saving to NVS - only credentials we know work end up persisted.
    SetWifi { ssid: String, password: String },

    /// Configure the server URL and authentication token for syncing alarms
    /// and todos. Saves immediately without verification (URLs are hard to
    /// validate without attempting a network request).
    SetServer { url: String, token: String },

    /// Trigger an immediate sync with the configured server. Requires a
    /// live Wi-Fi connection and a valid system time; returns an error
    /// if either is unavailable.
    SyncNow,

    /// Query the device's current configuration and connectivity state.
    GetStatus,

    /// Remove every locally stored alarm and disarm the RTC alarm slot.
    ClearAlarms,

    /// Set local time as a fixed UTC offset in minutes (UTC-12 through UTC+14).
    SetTimezone { offset_minutes: i16 },
}

/// Reply sent back to a USB/BLE client.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum Reply {
    /// Command succeeded.
    Ok,

    /// A command arrived while a full-screen reminder (due-todo or urgent
    /// inbox) was actively ringing. It was not executed; the client should
    /// retry after the user dismisses the reminder or it times out.
    Busy,

    /// A long-running command (SyncNow, SetWifi) was accepted and is being
    /// executed asynchronously on the sync task. The real result is
    /// delivered as a deferred reply with the same `id` when the operation
    /// completes; the client should keep waiting rather than resending
    /// (a resend replays this interim reply until the real result replaces
    /// it - see `control-protocol.md`'s `pending` reply).
    Pending,

    /// Device status snapshot.
    Status {
        wifi_configured: bool,
        server_configured: bool,
        wifi_connected: bool,
        wifi_ssid: Option<String>,
        wifi_has_password: bool,
        server_url: Option<String>,
        server_has_token: bool,
        timezone_offset_minutes: i16,
    },

    /// Command failed.
    Error { message: String },
}

/// Architecture-level name for the request a control command carries into
/// the state machine (`Event::UsbCommand` / `Event::BleCommand`): the
/// transport channel only affects where the response goes, never what the
/// request means, so USB and BLE share one type.
pub type ControlRequest = Command;

/// Architecture-level name for the reply the state machine produces
/// (`Effect::Reply`).
pub type ControlReply = Reply;
