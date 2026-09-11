#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WakeCause {
    Enter,
    RtcAlarm,
    Down,
    Other,
}
