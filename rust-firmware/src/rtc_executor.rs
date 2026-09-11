use std::sync::mpsc::{
    channel, sync_channel, Receiver, RecvTimeoutError, Sender, SyncSender, TrySendError,
};
use std::time::Duration;

use anyhow::{bail, Result};
use inkwash_logic::alarm_regs::AlarmRegs;
use inkwash_logic::app::RtcAlarmSnapshot;

use crate::board::SharedI2c;
use crate::rtc::{DateTime, Pcf8563, PCF8563_ADDR};

const RTC_TASK_STACK: usize = 8 * 1024;

const RTC_COMMAND_CAPACITY: usize = 8;

const IDLE_DRAIN: Duration = Duration::from_secs(1);

enum RtcCommand {
    ReadTime {
        reply: Sender<Result<DateTime>>,
    },

    WriteTime {
        dt: DateTime,
        reply: Sender<Result<()>>,
    },

    AlarmStatus {
        reply: Sender<Result<AlarmStatus>>,
    },

    Snapshot {
        reply: Sender<Result<RtcAlarmSnapshot>>,
    },

    Acknowledge {
        reply: Sender<Result<()>>,
    },

    Disable {
        reply: Sender<Result<()>>,
    },

    Program {
        regs: AlarmRegs,
        reply: Sender<Result<()>>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AlarmStatus {
    pub alarm_flag: bool,
    pub alarm_interrupt_enabled: bool,
}

#[derive(Clone)]
pub struct RtcExecutor {
    tx: SyncSender<RtcCommand>,
}

impl RtcExecutor {
    pub fn spawn(bus: SharedI2c) -> Result<Self> {
        let (tx, rx) = sync_channel(RTC_COMMAND_CAPACITY);
        let (probe_tx, probe_rx) = channel();
        std::thread::Builder::new()
            .name("rtc".to_string())
            .stack_size(RTC_TASK_STACK)
            .spawn(move || run(bus, rx, probe_tx))?;
        match probe_rx.recv()? {
            Ok(()) => Ok(Self { tx }),
            Err(err) => bail!("RTC executor probe failed: {err}"),
        }
    }

    pub fn read_time(&self) -> Result<DateTime> {
        let (reply_tx, reply_rx) = channel();
        self.try_send(RtcCommand::ReadTime { reply: reply_tx })?;
        reply_rx.recv()?
    }

    pub fn request_read_time(&self) -> Result<Receiver<Result<DateTime>>> {
        let (reply_tx, reply_rx) = channel();
        self.try_send(RtcCommand::ReadTime { reply: reply_tx })?;
        Ok(reply_rx)
    }

    pub fn write_time(&self, dt: &DateTime) -> Result<()> {
        let (reply_tx, reply_rx) = channel();
        self.try_send(RtcCommand::WriteTime {
            dt: *dt,
            reply: reply_tx,
        })?;
        reply_rx.recv()?
    }

    pub fn alarm_status(&self) -> Result<AlarmStatus> {
        let (reply_tx, reply_rx) = channel();
        self.try_send(RtcCommand::AlarmStatus { reply: reply_tx })?;
        reply_rx.recv()?
    }

    pub fn request_alarm_status(&self) -> Result<Receiver<Result<AlarmStatus>>> {
        let (reply_tx, reply_rx) = channel();
        self.try_send(RtcCommand::AlarmStatus { reply: reply_tx })?;
        Ok(reply_rx)
    }

    #[allow(dead_code)]
    pub fn snapshot(&self) -> Result<RtcAlarmSnapshot> {
        let (reply_tx, reply_rx) = channel();
        self.try_send(RtcCommand::Snapshot { reply: reply_tx })?;
        reply_rx.recv()?
    }

    pub fn request_snapshot(&self) -> Result<Receiver<Result<RtcAlarmSnapshot>>> {
        let (reply_tx, reply_rx) = channel();
        self.try_send(RtcCommand::Snapshot { reply: reply_tx })?;
        Ok(reply_rx)
    }

    pub fn acknowledge(&self) -> Result<()> {
        let (reply_tx, reply_rx) = channel();
        self.try_send(RtcCommand::Acknowledge { reply: reply_tx })?;
        reply_rx.recv()?
    }

    pub fn disable(&self) -> Result<()> {
        let (reply_tx, reply_rx) = channel();
        self.try_send(RtcCommand::Disable { reply: reply_tx })?;
        reply_rx.recv()?
    }

    pub fn program(&self, regs: &AlarmRegs) -> Result<()> {
        let (reply_tx, reply_rx) = channel();
        self.try_send(RtcCommand::Program {
            regs: *regs,
            reply: reply_tx,
        })?;
        reply_rx.recv()?
    }

    fn try_send(&self, command: RtcCommand) -> Result<()> {
        self.tx.try_send(command).map_err(|err| match err {
            TrySendError::Full(_) => anyhow::anyhow!("RTC command queue is full"),
            TrySendError::Disconnected(_) => anyhow::anyhow!("RTC executor disconnected"),
        })
    }
}

fn run(bus: SharedI2c, rx: Receiver<RtcCommand>, probe_tx: Sender<Result<()>>) {
    let watchdog_subscribed = match crate::watchdog::subscribe() {
        Ok(()) => true,
        Err(err) => {
            log::warn!("RTC executor watchdog subscribe failed: {err}");
            false
        }
    };

    let mut rtc = Pcf8563::new(bus, PCF8563_ADDR);
    let probe_result = rtc.probe();
    let probe_ok = probe_result.is_ok();
    let _ = probe_tx.send(probe_result);
    if !probe_ok {
        log::error!("RTC executor: PCF8563 not responding on I2C bus");
        return;
    }
    log::info!("RTC executor started (sole PCF8563 owner)");

    let mut latch = inkwash_logic::rtc_latch::RtcSnapshotLatch::new();

    loop {
        let cmd = match rx.recv_timeout(IDLE_DRAIN) {
            Ok(cmd) => cmd,
            Err(RecvTimeoutError::Timeout) => {
                if watchdog_subscribed {
                    crate::watchdog::feed();
                }
                continue;
            }
            Err(RecvTimeoutError::Disconnected) => break,
        };
        match cmd {
            RtcCommand::ReadTime { reply } => {
                let result = rtc.read_time();
                let _ = reply.send(result);
            }
            RtcCommand::WriteTime { dt, reply } => {
                let result = rtc.write_time(&dt);
                let _ = reply.send(result);
            }
            RtcCommand::AlarmStatus { reply } => {
                let result = read_alarm_status(&mut rtc);
                if let Ok(status) = &result {
                    latch.observe_alarm_flag(status.alarm_flag);
                }
                let _ = reply.send(result);
            }
            RtcCommand::Snapshot { reply } => {
                let result = snapshot_or_latch(&mut rtc, &mut latch);
                let _ = reply.send(result);
            }
            RtcCommand::Acknowledge { reply } => {
                let result = rtc.ack_alarm();
                if result.is_ok() {
                    latch.on_ack_or_disable_success();
                }
                let _ = reply.send(result);
            }
            RtcCommand::Disable { reply } => {
                let result = rtc.clear_alarm();
                if result.is_ok() {
                    latch.on_ack_or_disable_success();
                }
                let _ = reply.send(result);
            }
            RtcCommand::Program { regs, reply } => {
                let result = rtc.set_alarm(&regs);

                let _ = reply.send(result);
            }
        }

        if inkwash_logic::worker_heartbeat::should_feed_after_command(watchdog_subscribed) {
            crate::watchdog::feed();
        }
    }
}

fn read_alarm_status(rtc: &mut Pcf8563) -> Result<AlarmStatus> {
    let af = rtc.alarm_flag()?;
    let aie = rtc.alarm_interrupt_enabled()?;
    Ok(AlarmStatus {
        alarm_flag: af,
        alarm_interrupt_enabled: aie,
    })
}

fn snapshot_or_latch(
    rtc: &mut Pcf8563,
    latch: &mut inkwash_logic::rtc_latch::RtcSnapshotLatch,
) -> Result<RtcAlarmSnapshot> {
    if latch.action() == inkwash_logic::rtc_latch::SnapshotAction::UseCached {
        if let Some(cached) = latch.cached() {
            return Ok(cached.clone());
        }
    }
    let now = rtc.read_time()?;
    let af = rtc.alarm_flag()?;
    let aie = rtc.alarm_interrupt_enabled()?;
    let snapshot = RtcAlarmSnapshot {
        now,
        alarm_flag: af,
        alarm_interrupt_enabled: aie,
    };
    latch.observe_snapshot(snapshot.clone());
    Ok(snapshot)
}
