//! Pure audio-command arbitration shared by the firmware audio task.
//!
//! Alarm audio has priority over reminder audio.  Commands may arrive in a
//! burst while the codec is playing a note, so the task reduces the whole
//! pending sequence instead of treating the arrival of any command as an
//! implicit stop.

use std::collections::VecDeque;

/// Maximum number of audio commands waiting behind the codec task.
pub const AUDIO_MAILBOX_CAPACITY: usize = 8;

/// A sound request from the application executor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AudioCommand {
    StartAlarmTone,
    StartSiren,
    BeepTodo,
    Stop,
}

/// Fixed-capacity audio command mailbox. Critical alarm starts and Stop use
/// explicit coalescing when full; lower-priority reminder commands return to
/// the caller so ownership is retained for backpressure/error handling.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AudioMailbox {
    queue: VecDeque<AudioCommand>,
}

impl Default for AudioMailbox {
    fn default() -> Self {
        Self::new()
    }
}

impl AudioMailbox {
    pub fn new() -> Self {
        Self {
            queue: VecDeque::with_capacity(AUDIO_MAILBOX_CAPACITY),
        }
    }

    /// Enqueue without dropping a command. The newest Stop supersedes all
    /// queued starts; a new alarm start supersedes queued reminder/stop
    /// intent. Lower-priority reminders apply backpressure when full.
    pub fn enqueue(&mut self, command: AudioCommand) -> Result<(), AudioCommand> {
        if self.queue.len() < AUDIO_MAILBOX_CAPACITY {
            self.queue.push_back(command);
            return Ok(());
        }
        match command {
            AudioCommand::Stop | AudioCommand::StartAlarmTone => {
                self.queue.clear();
                self.queue.push_back(command);
                Ok(())
            }
            AudioCommand::StartSiren | AudioCommand::BeepTodo => Err(command),
        }
    }

    pub fn drain(&mut self) -> impl Iterator<Item = AudioCommand> + '_ {
        self.queue.drain(..)
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.queue.len()
    }
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
    fn mailbox_backpressures_reminders_at_fixed_capacity() {
        let mut mailbox = AudioMailbox::new();
        for _ in 0..AUDIO_MAILBOX_CAPACITY {
            mailbox.enqueue(AudioCommand::StartSiren).unwrap();
        }
        assert_eq!(mailbox.len(), AUDIO_MAILBOX_CAPACITY);
        assert_eq!(
            mailbox.enqueue(AudioCommand::BeepTodo),
            Err(AudioCommand::BeepTodo)
        );
        assert_eq!(mailbox.len(), AUDIO_MAILBOX_CAPACITY);
    }

    #[test]
    fn mailbox_full_still_accepts_stop_and_alarm_start() {
        let mut mailbox = AudioMailbox::new();
        for _ in 0..AUDIO_MAILBOX_CAPACITY {
            mailbox.enqueue(AudioCommand::StartSiren).unwrap();
        }
        mailbox.enqueue(AudioCommand::Stop).unwrap();
        assert_eq!(
            mailbox.drain().collect::<Vec<_>>(),
            vec![AudioCommand::Stop]
        );

        for _ in 0..AUDIO_MAILBOX_CAPACITY {
            mailbox.enqueue(AudioCommand::BeepTodo).unwrap();
        }
        mailbox.enqueue(AudioCommand::StartAlarmTone).unwrap();
        assert_eq!(
            mailbox.drain().collect::<Vec<_>>(),
            vec![AudioCommand::StartAlarmTone]
        );
    }

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
