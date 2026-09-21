use std::sync::mpsc::{
    channel, sync_channel, Receiver, RecvTimeoutError, Sender, SyncSender, TrySendError,
};
use std::sync::{Arc, Mutex};
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
    ReadTime,
    WriteTime(DateTime),
    AlarmStatus,
    Snapshot,
    Acknowledge,
    Disable,
    Program(AlarmRegs),
}

enum RtcReply {
    DateTime(Result<DateTime>),
    AlarmStatus(Result<AlarmStatus>),
    Snapshot(Result<RtcAlarmSnapshot>),
    Unit(Result<()>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AlarmStatus {
    pub alarm_flag: bool,
    pub alarm_interrupt_enabled: bool,
}

#[derive(Clone)]
pub struct RtcExecutor {
    tx: SyncSender<RtcCommand>,
    replies: Arc<Mutex<Receiver<RtcReply>>>,
}

impl RtcExecutor {
    pub fn spawn(bus: SharedI2c) -> Result<Self> {
        let (tx, rx) = sync_channel(RTC_COMMAND_CAPACITY);
        let (reply_tx, reply_rx) = sync_channel(1);
        let (probe_tx, probe_rx) = channel();
        std::thread::Builder::new()
            .name("rtc".to_string())
            .stack_size(RTC_TASK_STACK)
            .spawn(move || run(bus, rx, reply_tx, probe_tx))?;
        match probe_rx.recv()? {
            Ok(()) => Ok(Self {
                tx,
                replies: Arc::new(Mutex::new(reply_rx)),
            }),
            Err(err) => bail!("RTC executor probe failed: {err}"),
        }
    }

    pub fn read_time(&self) -> Result<DateTime> {
        match self.request(RtcCommand::ReadTime)? {
            RtcReply::DateTime(result) => result,
            _ => bail!("RTC executor returned a mismatched read-time reply"),
        }
    }

    pub fn write_time(&self, dt: &DateTime) -> Result<()> {
        self.request_unit(RtcCommand::WriteTime(*dt))
    }

    pub fn alarm_status(&self) -> Result<AlarmStatus> {
        match self.request(RtcCommand::AlarmStatus)? {
            RtcReply::AlarmStatus(result) => result,
            _ => bail!("RTC executor returned a mismatched alarm-status reply"),
        }
    }

    #[allow(dead_code)]
    pub fn snapshot(&self) -> Result<RtcAlarmSnapshot> {
        match self.request(RtcCommand::Snapshot)? {
            RtcReply::Snapshot(result) => result,
            _ => bail!("RTC executor returned a mismatched snapshot reply"),
        }
    }

    pub fn acknowledge(&self) -> Result<()> {
        self.request_unit(RtcCommand::Acknowledge)
    }

    pub fn disable(&self) -> Result<()> {
        self.request_unit(RtcCommand::Disable)
    }

    pub fn program(&self, regs: &AlarmRegs) -> Result<()> {
        self.request_unit(RtcCommand::Program(*regs))
    }

    fn request_unit(&self, command: RtcCommand) -> Result<()> {
        match self.request(command)? {
            RtcReply::Unit(result) => result,
            _ => bail!("RTC executor returned a mismatched unit reply"),
        }
    }

    fn request(&self, command: RtcCommand) -> Result<RtcReply> {
        let replies = self
            .replies
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.try_send(command)?;
        Ok(replies.recv()?)
    }

    fn try_send(&self, command: RtcCommand) -> Result<()> {
        self.tx.try_send(command).map_err(|err| match err {
            TrySendError::Full(_) => anyhow::anyhow!("RTC command queue is full"),
            TrySendError::Disconnected(_) => anyhow::anyhow!("RTC executor disconnected"),
        })
    }
}

fn run(
    bus: SharedI2c,
    rx: Receiver<RtcCommand>,
    reply: SyncSender<RtcReply>,
    probe_tx: Sender<Result<()>>,
) {
    crate::heap_probe::register_current_task(crate::heap_probe::SLOT_RTC);
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
        let response = match cmd {
            RtcCommand::ReadTime => RtcReply::DateTime(rtc.read_time()),
            RtcCommand::WriteTime(dt) => RtcReply::Unit(rtc.write_time(&dt)),
            RtcCommand::AlarmStatus => {
                let result = read_alarm_status(&mut rtc);
                if let Ok(status) = &result {
                    latch.observe_alarm_flag(status.alarm_flag);
                }
                RtcReply::AlarmStatus(result)
            }
            RtcCommand::Snapshot => RtcReply::Snapshot(snapshot_or_latch(&mut rtc, &mut latch)),
            RtcCommand::Acknowledge => {
                let result = rtc.ack_alarm();
                if result.is_ok() {
                    latch.on_ack_or_disable_success();
                }
                RtcReply::Unit(result)
            }
            RtcCommand::Disable => {
                let result = rtc.clear_alarm();
                if result.is_ok() {
                    latch.on_ack_or_disable_success();
                }
                RtcReply::Unit(result)
            }
            RtcCommand::Program(regs) => RtcReply::Unit(rtc.set_alarm(&regs)),
        };
        if reply.send(response).is_err() {
            break;
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
