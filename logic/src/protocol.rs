use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Channel {
    Usb,
    Ble,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Command {
    SetWifi { ssid: String, password: String },

    SetServer { url: String, token: String },

    SyncNow,

    SetRtc { epoch_secs: u64 },

    GetStatus,

    ClearAlarms,

    SetTimezone { offset_minutes: i16 },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum Reply {
    Ok,

    Busy,

    Pending,

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

    Error {
        message: String,
    },
}

pub type ControlRequest = Command;

pub type ControlReply = Reply;

pub const MAX_COMMAND_NESTING: usize = 4;

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
        assert!(!nesting_exceeds(r#"{"a":[1]}"#, MAX_COMMAND_NESTING));

        assert!(nesting_exceeds(r#"{"a":[[[[[1]]]]]}"#, MAX_COMMAND_NESTING));
        assert!(nesting_exceeds("[[[[[", MAX_COMMAND_NESTING));
    }

    #[test]
    fn brackets_inside_strings_are_not_counted() {
        let line = r#"{"cmd":"set_wifi","ssid":"a","password":"{{{[[[[[[[["}"#;
        assert!(
            !nesting_exceeds(line, MAX_COMMAND_NESTING),
            "brackets inside a string literal are data, not nesting"
        );
    }

    #[test]
    fn an_escaped_quote_does_not_end_the_string() {
        let line = r#"{"cmd":"set_wifi","ssid":"a\"[[[[[[[[","password":"p"}"#;
        assert!(!nesting_exceeds(line, MAX_COMMAND_NESTING));
    }

    #[test]
    fn the_scan_stops_as_soon_as_the_limit_is_exceeded() {
        let deep = "[".repeat(200_000);
        assert!(nesting_exceeds(&deep, MAX_COMMAND_NESTING));
    }

    #[test]
    fn the_documented_hazard_depth_is_rejected() {
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
        assert!(!nesting_exceeds("]]]]]]", MAX_COMMAND_NESTING));
    }
}
