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
