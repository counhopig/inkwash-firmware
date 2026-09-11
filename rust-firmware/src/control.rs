use anyhow::{anyhow, Result};

pub use inkwash_logic::protocol::{Channel, Command, Reply};

pub use inkwash_logic::protocol::{nesting_exceeds, MAX_COMMAND_NESTING};

pub fn parse_command(line: &str) -> Result<(Option<String>, Command)> {
    if nesting_exceeds(line, MAX_COMMAND_NESTING) {
        return Err(anyhow!(
            "Failed to parse command: JSON nesting exceeds {MAX_COMMAND_NESTING}"
        ));
    }
    let value: serde_json::Value =
        serde_json::from_str(line).map_err(|e| anyhow!("Failed to parse command: {e}"))?;
    let id = value
        .get("id")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let cmd = serde_json::from_value(value).map_err(|e| anyhow!("Failed to parse command: {e}"))?;
    Ok((id, cmd))
}

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
