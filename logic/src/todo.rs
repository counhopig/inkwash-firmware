//! Todo data shapes, moved out of `rust-firmware/src/todos.rs` (which
//! re-exports them) so `sync_validate` can share one definition instead of
//! duplicating the wire shape.

use serde::{Deserialize, Serialize};

use crate::alarm_schedule::Repeat;

/// Todo importance, wire-compatible with the server's `models::Importance`
/// (`"low"`/`"medium"`/`"high"`). `Medium` is the default so records synced
/// from before importance existed keep a sane value.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Importance {
    Low,
    #[default]
    Medium,
    High,
}

impl Importance {
    /// Stable discriminator string - the server uses this as its SQLite
    /// column value, so it must not change without a migration on that
    /// side.
    pub const fn as_str(self) -> &'static str {
        match self {
            Importance::Low => "low",
            Importance::Medium => "medium",
            Importance::High => "high",
        }
    }
}

/// Full due date (year/month/day). The calendar page draws a marker on that
/// date, and a `High` todo due today triggers a one-shot reminder through
/// `reminders.rs`. The `year` field defaults to 0 so records synced before it
/// existed deserialize safely (year 0 never matches a real date, so such
/// todos simply don't mark the calendar or remind).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TodoDue {
    #[serde(default)]
    pub year: u16,
    pub month: u8,
    pub day: u8,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Todo {
    pub id: u8,
    pub text: String,
    pub done: bool,
    #[serde(default)]
    pub importance: Importance,
    /// Single due date (used when `repeat` is `None`).
    #[serde(default)]
    pub due_date: Option<TodoDue>,
    /// Recurrence schedule; when set, the todo is due on every date the
    /// schedule covers instead of just `due_date`.
    #[serde(default)]
    pub repeat: Option<Repeat>,
}
