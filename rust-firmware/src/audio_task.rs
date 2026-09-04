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

use std::sync::mpsc::{sync_channel, Receiver, SyncSender};
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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioCommand {
    /// Start a continuous alarm ring (repeating 880 Hz bursts) until
    /// [`AudioCommand::Stop`]. Returns immediately to the caller.
    StartAlarmTone,
    /// Start the urgent-reminder siren (alternating 1397/1046 Hz notes)
    /// until [`AudioCommand::Stop`]. Non-blocking.
    StartSiren,
    /// Play a short bounded beep sequence (the todo-reminder attention
    /// tone: three 1046 Hz pips). Completes on its own; a later Stop cuts
    /// it short. Non-blocking.
    BeepTodo,
    /// Stop whatever is playing (alarm ring or bounded pattern). Idempotent.
    Stop,
}

/// Handle kept by the main loop: the command sender.
pub struct AudioTask {
    tx: SyncSender<AudioCommand>,
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

        const CAP: usize = 4;
        let (tx, rx) = sync_channel(CAP);
        thread::Builder::new()
            .stack_size(8 * 1024)
            .name("audio".into())
            .spawn(move || run(codec, rx))?;
        Ok(AudioSpawn::Running(AudioTask { tx }))
    }

    /// Ask the task to start the alarm ring. Non-blocking; the caller never
    /// waits for the sound. Errors only if the channel is gone.
    pub fn start_alarm_tone(&self) -> Result<()> {
        self.tx
            .send(AudioCommand::StartAlarmTone)
            .map_err(|_| anyhow!("audio task stopped"))
    }

    /// Ask the task to play the urgent siren until stopped.
    pub fn start_siren(&self) -> Result<()> {
        self.tx
            .send(AudioCommand::StartSiren)
            .map_err(|_| anyhow!("audio task stopped"))
    }

    /// Ask the task to play the bounded todo beep.
    pub fn beep_todo(&self) -> Result<()> {
        self.tx
            .send(AudioCommand::BeepTodo)
            .map_err(|_| anyhow!("audio task stopped"))
    }

    /// Ask the task to stop any tone. Non-blocking; idempotent.
    pub fn stop(&self) -> Result<()> {
        self.tx
            .send(AudioCommand::Stop)
            .map_err(|_| anyhow!("audio task stopped"))
    }
}

/// The active sound: a repeating pattern that runs until a `Stop` arrives,
/// or a bounded one-shot that finishes on its own.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Playing {
    AlarmRing,
    Siren,
    TodoBeep { pip: u8 },
}

fn run(mut codec: Es8311, rx: Receiver<AudioCommand>) {
    log::info!("Audio task running");
    let mut playing: Option<Playing> = None;
    loop {
        // Drain any pending command, then act on the newest state.
        while let Ok(cmd) = rx.try_recv() {
            match cmd {
                AudioCommand::StartAlarmTone => playing = Some(Playing::AlarmRing),
                AudioCommand::StartSiren => playing = Some(Playing::Siren),
                AudioCommand::BeepTodo => playing = Some(Playing::TodoBeep { pip: 0 }),
                AudioCommand::Stop => playing = None,
            }
        }
        match playing {
            None => match rx.recv() {
                Ok(AudioCommand::StartAlarmTone) => playing = Some(Playing::AlarmRing),
                Ok(AudioCommand::StartSiren) => playing = Some(Playing::Siren),
                Ok(AudioCommand::BeepTodo) => playing = Some(Playing::TodoBeep { pip: 0 }),
                Ok(AudioCommand::Stop) => {}
                Err(_) => break,
            },
            Some(Playing::AlarmRing) => {
                watchdog::feed();
                // One burst + a short gap, re-checking for Stop between
                // steps so dismiss latency stays ~the gap, not the tone.
                if let Err(err) = codec.play_sine_stereo(ALARM_TONE_HZ, ALARM_TONE_BURST_SECS, 8000)
                {
                    log::warn!("Alarm tone playback failed: {err}");
                }
                if wait_gap(&rx, Duration::from_millis(ALARM_BURST_GAP_MS)) {
                    playing = None;
                }
            }
            Some(Playing::Siren) => {
                watchdog::feed();
                for (freq, dur) in SIREN_NOTES {
                    if let Err(err) = codec.play_sine_stereo(freq, dur, SIREN_AMPLITUDE) {
                        log::warn!("Siren note failed: {err}");
                    }
                    // A quick check between notes so Stop latency is low.
                    if rx.try_recv().is_ok() {
                        playing = None;
                        break;
                    }
                }
            }
            Some(Playing::TodoBeep { pip }) => {
                if pip >= BEEP_COUNT {
                    playing = None;
                    continue;
                }
                if let Err(err) = codec.play_sine_stereo(BEEP_HZ, BEEP_PIP_SECS, 8000) {
                    log::warn!("Todo beep failed: {err}");
                }
                if wait_gap(&rx, Duration::from_millis(150)) {
                    playing = None;
                } else if let Some(Playing::TodoBeep { pip }) = playing {
                    playing = Some(Playing::TodoBeep { pip: pip + 1 });
                }
            }
        }
    }
    log::info!("Audio task stopped");
}

/// Sleeps `duration`, waking early if a `Stop` (or any command) arrives.
/// Returns true when a command arrived (caller should re-evaluate).
fn wait_gap(rx: &Receiver<AudioCommand>, duration: Duration) -> bool {
    let until = EspSystemTime {}.now() + duration;
    while (EspSystemTime {}).now() < until {
        if rx.try_recv().is_ok() {
            return true;
        }
        thread::sleep(Duration::from_millis(5));
    }
    false
}
