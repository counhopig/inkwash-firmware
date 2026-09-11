use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use anyhow::{anyhow, Result};
use esp_idf_svc::systime::EspSystemTime;

use crate::audio::Es8311;
use crate::watchdog;

const ALARM_TONE_HZ: f32 = 880.0;
const ALARM_TONE_BURST_SECS: f32 = 0.05;

const ALARM_BURST_GAP_MS: u64 = 150;

const SIREN_NOTES: [(f32, f32); 2] = [(1397.0, 0.12), (1046.0, 0.12)];
const SIREN_AMPLITUDE: i16 = 24000;

const BEEP_HZ: f32 = 1046.0;
const BEEP_PIP_SECS: f32 = 0.15;
const BEEP_COUNT: u8 = 3;

pub use inkwash_logic::audio_command::AudioCommand;
use inkwash_logic::audio_command::{AudioMailbox, AudioMode, AudioReducer};

pub struct AudioTask {
    mailbox: Arc<Mutex<AudioMailbox>>,
}

pub enum AudioSpawn {
    Running(AudioTask),

    Unavailable,
}

impl AudioTask {
    pub fn spawn(audio: Option<Es8311>) -> Result<AudioSpawn> {
        let Some(mut codec) = audio else {
            return Ok(AudioSpawn::Unavailable);
        };

        codec.set_mute(false)?;

        let mailbox = Arc::new(Mutex::new(AudioMailbox::new()));
        let task_mailbox = mailbox.clone();
        thread::Builder::new()
            .stack_size(8 * 1024)
            .name("audio".into())
            .spawn(move || run(codec, task_mailbox))?;
        Ok(AudioSpawn::Running(AudioTask { mailbox }))
    }

    pub fn start_alarm_tone(&self) -> Result<()> {
        self.enqueue(AudioCommand::StartAlarmTone)
    }

    pub fn start_siren(&self) -> Result<()> {
        self.enqueue(AudioCommand::StartSiren)
    }

    pub fn beep_todo(&self) -> Result<()> {
        self.enqueue(AudioCommand::BeepTodo)
    }

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
