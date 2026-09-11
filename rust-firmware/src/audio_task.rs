//! Dedicated audio task.
//!
//! Owns the single `Es8311` codec (I2S + PA + I2C control) and serializes
//! every tone on one thread, so the main loop never blocks on a
//! `play_sine_stereo` call (each of which sleeps ~10ms + the I2S DMA write
//! and a 150ms drain).
//!
//! Request model: the caller sends one [`AudioCommand`] on a bounded channel
//! and never waits. The task plays the tone pattern in a tight loop until a
//! `Stop` arrives (for the alarm ring, which must keep ringing until the SM
//! dismisses / times out) or until a finite pattern completes (the bounded
//! todo beep / urgent siren are still driven by the task, not by a blocking
//! caller loop). A `Stop` also stops a bounded pattern early.
//!
//! Degradation: if the codec failed to initialise at boot the task is not
//! spawned and the command sender reports an error immediately - the state
//! machine's StartTone effect fails cleanly (audio is a "degraded run, not a
//! blocker" category), so an alarm still rings visually and dismisses.

use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use anyhow::{anyhow, Result};
use esp_idf_svc::systime::EspSystemTime;

use crate::audio::Es8311;
use crate::watchdog;

/// Alarm ring: steady 880 Hz bursts (the legacy blocking ring's tone).
const ALARM_TONE_HZ: f32 = 880.0;
const ALARM_TONE_BURST_SECS: f32 = 0.05;
/// The gap between alarm bursts, in milliseconds.
const ALARM_BURST_GAP_MS: u64 = 150;
/// Urgent-reminder siren: alternating 1397/1046 Hz notes (legacy
/// show_urgent's SIREN table).
const SIREN_NOTES: [(f32, f32); 2] = [(1397.0, 0.12), (1046.0, 0.12)];
const SIREN_AMPLITUDE: i16 = 24000;
/// Todo-reminder attention beep: three 1046 Hz pips.
const BEEP_HZ: f32 = 1046.0;
const BEEP_PIP_SECS: f32 = 0.15;
const BEEP_COUNT: u8 = 3;

/// What the audio task should play.
pub use inkwash_logic::audio_command::AudioCommand;
use inkwash_logic::audio_command::{AudioMailbox, AudioMode, AudioReducer};

/// Handle kept by the main loop: the command sender.
pub struct AudioTask {
    /// A fixed-capacity mutex-protected mailbox keeps submissions non-blocking.
    /// Critical Stop/alarm commands coalesce when full; reminder commands
    /// return backpressure to the effect runner instead of being dropped.
    mailbox: Arc<Mutex<AudioMailbox>>,
}

/// Result of a spawned audio task.
pub enum AudioSpawn {
    /// The codec is available; the task is running.
    Running(AudioTask),
    /// The codec failed to initialise at boot; audio is unavailable.
    /// `StartTone` reports an error through the state machine.
    Unavailable,
}

impl AudioTask {
    /// Spawns the audio task, moving the process's one `Es8311` onto it.
    pub fn spawn(audio: Option<Es8311>) -> Result<AudioSpawn> {
        let Some(mut codec) = audio else {
            return Ok(AudioSpawn::Unavailable);
        };
        // Unmute so any tone is audible (the codec boots muted; the legacy
        // executor StartTone did this set_mute(false)).
        codec.set_mute(false)?;

        let mailbox = Arc::new(Mutex::new(AudioMailbox::new()));
        let task_mailbox = mailbox.clone();
        thread::Builder::new()
            .stack_size(8 * 1024)
            .name("audio".into())
            .spawn(move || run(codec, task_mailbox))?;
        Ok(AudioSpawn::Running(AudioTask { mailbox }))
    }

    /// Ask the task to start the alarm ring. Non-blocking; the caller never
    /// waits for the sound. Errors only if the channel is gone.
    pub fn start_alarm_tone(&self) -> Result<()> {
        self.enqueue(AudioCommand::StartAlarmTone)
    }

    /// Ask the task to play the urgent siren until stopped.
    pub fn start_siren(&self) -> Result<()> {
        self.enqueue(AudioCommand::StartSiren)
    }

    /// Ask the task to play the bounded todo beep.
    pub fn beep_todo(&self) -> Result<()> {
        self.enqueue(AudioCommand::BeepTodo)
    }

    /// Ask the task to stop any tone. Non-blocking; idempotent.
    pub fn stop(&self) -> Result<()> {
        self.enqueue(AudioCommand::Stop)
    }

    fn enqueue(&self, command: AudioCommand) -> Result<()> {
        let mut mailbox = self
            .mailbox
            .lock()
            .map_err(|_| anyhow!("audio mailbox poisoned"))?;
        mailbox.enqueue(command).map_err(|rejected| {
            anyhow!("audio mailbox full; rejected reminder command {rejected:?}")
        })?;
        Ok(())
    }
}

fn run(mut codec: Es8311, mailbox: Arc<Mutex<AudioMailbox>>) {
    log::info!("Audio task running");
    let watchdog_subscribed = match watchdog::subscribe() {
        Ok(()) => true,
        Err(err) => {
            log::warn!("Audio task watchdog subscribe failed: {err}");
            false
        }
    };
    let mut reducer = AudioReducer::new();
    loop {
        if watchdog_subscribed {
            watchdog::feed();
        }
        drain_commands(&mailbox, &mut reducer);
        match reducer.mode() {
            AudioMode::Idle => thread::sleep(Duration::from_millis(5)),
            AudioMode::AlarmRing => {
                // One burst + a short gap, re-checking for Stop between
                // steps so dismiss latency stays ~the gap, not the tone.
                if let Err(err) = codec.play_sine_stereo(ALARM_TONE_HZ, ALARM_TONE_BURST_SECS, 8000)
                {
                    log::warn!("Alarm tone playback failed: {err}");
                }
                wait_gap(
                    &mailbox,
                    &mut reducer,
                    Duration::from_millis(ALARM_BURST_GAP_MS),
                );
            }
            AudioMode::Siren => {
                for (freq, dur) in SIREN_NOTES {
                    if let Err(err) = codec.play_sine_stereo(freq, dur, SIREN_AMPLITUDE) {
                        log::warn!("Siren note failed: {err}");
                    }
                    // A quick check between notes so Stop latency is low.
                    drain_commands(&mailbox, &mut reducer);
                    if reducer.mode() != AudioMode::Siren {
                        break;
                    }
                }
            }
            AudioMode::TodoBeep { pip } => {
                if pip >= BEEP_COUNT {
                    reducer.apply(AudioCommand::Stop);
                    continue;
                }
                if let Err(err) = codec.play_sine_stereo(BEEP_HZ, BEEP_PIP_SECS, 8000) {
                    log::warn!("Todo beep failed: {err}");
                }
                wait_gap(&mailbox, &mut reducer, Duration::from_millis(150));
                if matches!(reducer.mode(), AudioMode::TodoBeep { .. }) {
                    reducer.advance_todo();
                }
            }
        }
    }
}

/// Drain commands during a tone gap. Every command is reduced explicitly;
/// in particular a StartAlarmTone is never mistaken for Stop.
fn wait_gap(mailbox: &Arc<Mutex<AudioMailbox>>, reducer: &mut AudioReducer, duration: Duration) {
    let until = EspSystemTime {}.now() + duration;
    while (EspSystemTime {}).now() < until {
        drain_commands(mailbox, reducer);
        thread::sleep(Duration::from_millis(5));
    }
}

fn drain_commands(mailbox: &Arc<Mutex<AudioMailbox>>, reducer: &mut AudioReducer) {
    let commands = match mailbox.lock() {
        Ok(mut queue) => queue.drain().collect::<Vec<_>>(),
        Err(_) => return,
    };
    for command in commands {
        reducer.apply(command);
    }
}
