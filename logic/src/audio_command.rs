//! Pure audio-command arbitration shared by the firmware audio task.
//!
//! Alarm audio has priority over reminder audio.  Commands may arrive in a
//! burst while the codec is playing a note, so the task reduces the whole
//! pending sequence instead of treating the arrival of any command as an
//! implicit stop.

/// A sound request from the application executor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AudioCommand {
    StartAlarmTone,
    StartSiren,
    BeepTodo,
    Stop,
}

/// The sound currently desired by the application.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AudioMode {
    Idle,
    AlarmRing,
    Siren,
    TodoBeep { pip: u8 },
}

/// Deterministic reducer for queued audio commands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AudioReducer {
    mode: AudioMode,
}

impl Default for AudioReducer {
    fn default() -> Self {
        Self::new()
    }
}

impl AudioReducer {
    pub const fn new() -> Self {
        Self {
            mode: AudioMode::Idle,
        }
    }

    pub const fn mode(&self) -> AudioMode {
        self.mode
    }

    /// Apply one command.  A lower-priority reminder cannot replace an
    /// alarm that is already ringing; Stop always wins at its position in
    /// the queue and is idempotent.
    pub fn apply(&mut self, command: AudioCommand) {
        self.mode = match command {
            AudioCommand::StartAlarmTone => AudioMode::AlarmRing,
            AudioCommand::Stop => AudioMode::Idle,
            AudioCommand::StartSiren => match self.mode {
                AudioMode::AlarmRing => AudioMode::AlarmRing,
                _ => AudioMode::Siren,
            },
            AudioCommand::BeepTodo => match self.mode {
                AudioMode::AlarmRing | AudioMode::Siren => self.mode,
                _ => AudioMode::TodoBeep { pip: 0 },
            },
        };
    }

    /// Advance a completed todo beep pip.  A command arriving between pips
    /// is reduced separately by [`AudioReducer::apply`].
    pub fn advance_todo(&mut self) {
        if let AudioMode::TodoBeep { pip } = self.mode {
            self.mode = AudioMode::TodoBeep { pip: pip + 1 };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alarm_preempts_reminder_and_cannot_be_overwritten() {
        let mut reducer = AudioReducer::new();
        reducer.apply(AudioCommand::StartSiren);
        reducer.apply(AudioCommand::StartAlarmTone);
        reducer.apply(AudioCommand::BeepTodo);
        assert_eq!(reducer.mode(), AudioMode::AlarmRing);
    }

    #[test]
    fn todo_then_alarm_is_alarm() {
        let mut reducer = AudioReducer::new();
        reducer.apply(AudioCommand::BeepTodo);
        reducer.apply(AudioCommand::StartAlarmTone);
        assert_eq!(reducer.mode(), AudioMode::AlarmRing);
    }

    #[test]
    fn stop_is_idempotent_and_can_be_followed_by_a_new_start() {
        let mut reducer = AudioReducer::new();
        reducer.apply(AudioCommand::StartAlarmTone);
        reducer.apply(AudioCommand::Stop);
        reducer.apply(AudioCommand::Stop);
        assert_eq!(reducer.mode(), AudioMode::Idle);
        reducer.apply(AudioCommand::StartSiren);
        assert_eq!(reducer.mode(), AudioMode::Siren);
    }

    #[test]
    fn duplicate_starts_have_deterministic_restart_semantics() {
        let mut reducer = AudioReducer::new();
        reducer.apply(AudioCommand::BeepTodo);
        reducer.advance_todo();
        reducer.apply(AudioCommand::BeepTodo);
        assert_eq!(reducer.mode(), AudioMode::TodoBeep { pip: 0 });
        reducer.apply(AudioCommand::StartSiren);
        reducer.apply(AudioCommand::StartSiren);
        assert_eq!(reducer.mode(), AudioMode::Siren);
    }

    #[test]
    fn reminder_burst_cannot_replace_alarm() {
        let mut reducer = AudioReducer::new();
        for command in [
            AudioCommand::StartSiren,
            AudioCommand::StartAlarmTone,
            AudioCommand::BeepTodo,
            AudioCommand::StartSiren,
        ] {
            reducer.apply(command);
        }
        assert_eq!(reducer.mode(), AudioMode::AlarmRing);
    }
}
