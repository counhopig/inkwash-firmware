use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InboxKind {
    Alert,
    Event,
    Info,
}

impl From<&str> for InboxKind {
    fn from(s: &str) -> Self {
        match s {
            "alert" => InboxKind::Alert,
            "event" => InboxKind::Event,
            _ => InboxKind::Info,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Priority {
    #[default]
    Normal,
    High,
}

impl From<&str> for Priority {
    fn from(s: &str) -> Self {
        match s {
            "high" => Priority::High,
            _ => Priority::Normal,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct InboxItem {
    pub id: u64,
    pub kind: InboxKind,
    #[serde(default)]
    pub priority: Priority,
    pub title: String,
    #[serde(default)]
    pub body: String,
    #[serde(default)]
    pub when: Option<i64>,
    #[serde(default)]
    pub read: bool,
}
