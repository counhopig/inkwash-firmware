#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WakeCause {
    Enter,
    RtcAlarm,
    Down,
    /// The deep-sleep timer fired: a scheduled background wake with no user
    /// waiting at the screen.
    Timer,
    Other,
}
