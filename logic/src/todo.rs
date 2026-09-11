use serde::{Deserialize, Serialize};

use crate::alarm_schedule::Repeat;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Importance {
    Low,
    #[default]
    Medium,
    High,
}

impl Importance {
    pub const fn as_str(self) -> &'static str {
        match self {
            Importance::Low => "low",
            Importance::Medium => "medium",
            Importance::High => "high",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TodoDue {
    #[serde(default)]
    pub year: u16,
    pub month: u8,
    pub day: u8,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Todo {
    pub id: u8,
    pub text: String,
    pub done: bool,
    #[serde(default)]
    pub importance: Importance,

    #[serde(default)]
    pub due_date: Option<TodoDue>,

    #[serde(default)]
    pub repeat: Option<Repeat>,
}
