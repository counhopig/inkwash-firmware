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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
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

    /// Set the hardware RTC from a trusted host Unix timestamp. The firmware
    /// applies the stored local UTC offset before writing the PCF8563 clock.
    SetRtc { epoch_secs: u64 },

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

/// Maximum JSON nesting accepted in a control command.
///
/// Every command in this protocol is a flat object whose values are strings,
/// integers or booleans (`{"cmd":"set_wifi","ssid":…,"password":…}`), so no
/// legitimate frame contains a nested container at all. The limit exists
/// because deserialization recurses once per nesting level, and the recursion
/// depth the peer chooses decides the stack the parse needs — not the frame's
/// length.
///
/// Measured against the pinned `serde_json`, parsing the way
/// `control::parse_command` does (a `Value` pass then `from_value`) on the
/// shipped `opt-level="s"` profile:
///
/// | nesting | input | peak stack |
/// |---|---|---|
/// | 8 | 16 B | ~2.3 KB |
/// | 14 | 28 B | ~4.1 KB |
/// | 16 | 69 B | ~6.9 KB |
/// | 128 (`serde_json`'s own limit) | 256 B | ~35 KB |
///
/// The USB reader worker runs on the 4096-byte
/// `CONFIG_PTHREAD_TASK_STACK_SIZE_DEFAULT` and the BLE callback runs on the
/// 5120-byte NimBLE host task, so both are already past their budget by
/// nesting 14–16 while the frame is still far inside the 512-byte line cap.
/// Rejecting deep nesting before parsing costs no stack and loses no valid
/// frame, which is why this is a pre-parse scan rather than a length cap.
pub const MAX_COMMAND_NESTING: usize = 4;

/// Whether `line` nests JSON containers deeper than `max`.
///
/// A single linear scan that counts `{`/`[` and ignores anything inside a
/// string literal, so a bracket inside an `ssid` or a password cannot trip it.
/// Escapes are honoured (`"a\\"` does not end the string).
///
/// The scan returns as soon as the limit is exceeded, so it cannot itself be
/// driven to use meaningful stack by the input it is guarding against.
pub fn nesting_exceeds(line: &str, max: usize) -> bool {
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    for byte in line.bytes() {
        if in_string {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                in_string = false;
            }
            continue;
        }
        match byte {
            b'"' => in_string = true,
            b'{' | b'[' => {
                depth += 1;
                if depth > max {
                    return true;
                }
            }
            b'}' | b']' => depth = depth.saturating_sub(1),
            _ => {}
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flat_commands_are_within_the_nesting_limit() {
        // Every real command shape, at its largest.
        for line in [
            r#"{"cmd":"get_status"}"#,
            r#"{"cmd":"sync_now","id":"abc"}"#,
            r#"{"cmd":"set_rtc","epoch_secs":1757200000}"#,
            r#"{"cmd":"set_timezone","offset_minutes":-480}"#,
            r#"{"cmd":"clear_alarms"}"#,
            r#"{"cmd":"set_wifi","ssid":"net","password":"pw","id":"1"}"#,
            r#"{"cmd":"set_server","url":"https://example.test","token":"t"}"#,
        ] {
            assert!(
                !nesting_exceeds(line, MAX_COMMAND_NESTING),
                "a legitimate flat command must never be rejected: {line}"
            );
        }
    }

    #[test]
    fn containers_are_counted_at_any_depth() {
        // One object plus one array is depth 2: allowed.
        assert!(!nesting_exceeds(r#"{"a":[1]}"#, MAX_COMMAND_NESTING));
        // Beyond the limit is rejected.
        assert!(nesting_exceeds(r#"{"a":[[[[[1]]]]]}"#, MAX_COMMAND_NESTING));
        assert!(nesting_exceeds("[[[[[", MAX_COMMAND_NESTING));
    }

    #[test]
    fn brackets_inside_strings_are_not_counted() {
        // A password full of brackets must not be mistaken for nesting.
        let line = r#"{"cmd":"set_wifi","ssid":"a","password":"{{{[[[[[[[["}"#;
        assert!(
            !nesting_exceeds(line, MAX_COMMAND_NESTING),
            "brackets inside a string literal are data, not nesting"
        );
    }

    #[test]
    fn an_escaped_quote_does_not_end_the_string() {
        // `\"` inside the value must not be read as the closing quote, which
        // would let the following brackets be miscounted as structure.
        let line = r#"{"cmd":"set_wifi","ssid":"a\"[[[[[[[[","password":"p"}"#;
        assert!(!nesting_exceeds(line, MAX_COMMAND_NESTING));
    }

    #[test]
    fn the_scan_stops_as_soon_as_the_limit_is_exceeded() {
        // A very deep payload must not make the guard itself expensive.
        let deep = "[".repeat(200_000);
        assert!(nesting_exceeds(&deep, MAX_COMMAND_NESTING));
    }

    #[test]
    fn the_documented_hazard_depth_is_rejected() {
        // The measured ~6.9 KB case: 16 levels inside an otherwise valid
        // command. This is the frame the pre-parse guard exists to stop.
        let payload = format!(
            r#"{{"cmd":"get_status","id":"x","note":{}}}"#,
            "[".repeat(16) + &"]".repeat(16)
        );
        assert!(
            nesting_exceeds(&payload, MAX_COMMAND_NESTING),
            "the shape that overflows the 4096-byte worker stack must be rejected"
        );
    }

    #[test]
    fn unbalanced_closing_brackets_do_not_underflow() {
        // saturating_sub keeps the depth counter from wrapping; the malformed
        // frame is left for serde to reject as invalid JSON.
        assert!(!nesting_exceeds("]]]]]]", MAX_COMMAND_NESTING));
    }
}
