//! Dedicated RTC executor task: the single owner of the PCF8563 driver.
//!
//! Stage 2 of docs/firmware-migration-completion-plan.md (and
//! docs/firmware-architecture.md's layered model) requires that no code
//! outside one owning context touches the RTC driver or its I2C registers.
//! This module provides that context as a small task that owns the one
//! `Pcf8563` instance and serializes every RTC I2C operation on its own
//! thread:
//!
//! - the event-collection path (main loop / blocking pages) never performs
//!   RTC I2C itself: it asks this executor for an AF/AIE/time fact or a
//!   consistent alarm snapshot and continues collecting other events while
//!   the reply is in flight;
//! - the effect-execution path converts `AcknowledgeRtcAlarm` /
//!   `ProgramRtcAlarm` / `DisableRtcAlarm` into commands to this executor
//!   and awaits the confirmable result;
//! - boot time reads / VL reseeds / NTP alignment writes all arrive here
//!   as commands too, so the PCF8563 register file is touched only on this
//!   task.
//!
//! The underlying I2C0 bus is still shared with the ES8311 codec and the
//! NFC tag through `SharedI2c` (`Arc<parking_lot::Mutex<I2cDriver>>`, see
//! `board.rs`); only the *RTC driver object and its register protocol* are
//! single-owner. Every command carries a per-command reply channel, the
//! same request/reply shape `sync_task.rs` uses, so a caller can either
//! await the result synchronously (fast RTC effects) or park the reply for
//! a later drain (snapshot requests).
//!
//! The executor also owns the duplicate-snapshot latch required by the
//! architecture: while AF stays asserted (or an ACK has not yet released
//! it) a repeated snapshot request is answered from the same consistent
//! read instead of hammering the bus, and the latch is dropped once an
//! ACK/disable clears the flag.

use std::sync::mpsc::{
    channel, sync_channel, Receiver, RecvTimeoutError, Sender, SyncSender, TrySendError,
};
use std::time::Duration;

use anyhow::{bail, Result};
use inkwash_logic::alarm_regs::AlarmRegs;
use inkwash_logic::app::RtcAlarmSnapshot;

use crate::board::SharedI2c;
use crate::rtc::{DateTime, Pcf8563, PCF8563_ADDR};

/// 8 KiB stack: RTC register I2C calls are shallow (no TLS/FFI depth like
/// the EPD or sync tasks), and pthread stacks are forced to internal RAM;
/// 8 KiB leaves comfortable headroom over the 4 KiB default for anyhow
/// formatting and log calls without overrunning internal RAM.
const RTC_TASK_STACK: usize = 8 * 1024;
/// A small bounded command queue preserves the non-blocking collection path;
/// callers receive an explicit saturation error when the executor is
/// temporarily full. One-shot reply channels remain unbounded because each
/// has one producer and one pending receiver.
const RTC_COMMAND_CAPACITY: usize = 8;
/// Idle drain timeout: the task feeds the watchdog on this cadence while
/// no command is pending.
const IDLE_DRAIN: Duration = Duration::from_secs(1);

/// A command the main thread submits to the RTC executor. Each variant
/// carries the reply channel for its own result.
enum RtcCommand {
    /// Read current time (VL flag included).
    ReadTime { reply: Sender<Result<DateTime>> },
    /// Set current time (used by boot VL reseed and NTP alignment).
    WriteTime {
        dt: DateTime,
        reply: Sender<Result<()>>,
    },
    /// Read the alarm status facts (AF + AIE) without a full time read.
    AlarmStatus { reply: Sender<Result<AlarmStatus>> },
    /// Read a consistent (time, AF, AIE) snapshot. Honors the duplicate
    /// latch: while AF stays asserted without an intervening ACK/disable,
    /// repeated requests return the cached consistent read.
    Snapshot {
        reply: Sender<Result<RtcAlarmSnapshot>>,
    },
    /// Clear AF (acknowledge a fired alarm).
    Acknowledge { reply: Sender<Result<()>> },
    /// Clear the alarm compare registers + AF + AIE (empty list / disarm).
    Disable { reply: Sender<Result<()>> },
    /// Program the single hardware alarm slot and enable AIE.
    Program {
        regs: AlarmRegs,
        reply: Sender<Result<()>>,
    },
}

/// The AF/AIE facts read on the executor's cadence by the collection
/// layer; the application only sees whether an edge exists, never the bus.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AlarmStatus {
    pub alarm_flag: bool,
    pub alarm_interrupt_enabled: bool,
}

/// Client handle kept by the main thread: the command sender. Each method
/// blocks for its reply (RTC operations are single-digit-ms I2C round
/// trips), which keeps `EffectRunner`'s synchronous-completion contract
/// intact while the actual bus work happens on the executor task.
#[derive(Clone)]
pub struct RtcExecutor {
    tx: SyncSender<RtcCommand>,
}

impl RtcExecutor {
    /// Spawns the executor task. The PCF8563 probe runs on the task and
    /// its result is awaited here, so a dead RTC still fails board
    /// bring-up loudly (matching the pre-executor `Note4Board::take`
    /// behaviour) instead of silently degrading mid-boot.
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

    /// Submit a time read without waiting for the I2C transaction.
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

    /// Submit an alarm-status read without waiting for the I2C transaction.
    pub fn request_alarm_status(&self) -> Result<Receiver<Result<AlarmStatus>>> {
        let (reply_tx, reply_rx) = channel();
        self.try_send(RtcCommand::AlarmStatus { reply: reply_tx })?;
        Ok(reply_rx)
    }

    /// Requests a consistent alarm snapshot. Blocks for the executor's
    /// read (sub-ms I2C), matching the old synchronous snapshot reads on
    /// the collection path; the duplicate latch keeps an asserted AF from
    /// turning every poll into a fresh bus transaction.
    #[allow(dead_code)]
    pub fn snapshot(&self) -> Result<RtcAlarmSnapshot> {
        let (reply_tx, reply_rx) = channel();
        self.try_send(RtcCommand::Snapshot { reply: reply_tx })?;
        reply_rx.recv()?
    }

    /// Submit a consistent alarm snapshot without waiting for I2C. The
    /// caller polls the returned receiver from its event collector.
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

/// Owns the sole `Pcf8563` and processes commands in arrival order, so all
/// RTC I2C on the device is serialized here.
fn run(bus: SharedI2c, rx: Receiver<RtcCommand>, probe_tx: Sender<Result<()>>) {
    let watchdog_subscribed = match crate::watchdog::subscribe() {
        Ok(()) => true,
        Err(err) => {
            log::warn!("RTC executor watchdog subscribe failed: {err}");
            false
        }
    };
    // The executor owns the only Pcf8563 handle. Probing here (instead of
    // in `Note4Board::take`) keeps the driver and its init in one owner;
    // the result is sent back so `spawn` can fail loudly.
    let mut rtc = Pcf8563::new(bus, PCF8563_ADDR);
    let probe_result = rtc.probe();
    let probe_ok = probe_result.is_ok();
    let _ = probe_tx.send(probe_result);
    if !probe_ok {
        log::error!("RTC executor: PCF8563 not responding on I2C bus");
        return;
    }
    log::info!("RTC executor started (sole PCF8563 owner)");

    // Duplicate-snapshot latch (host-testable logic in
    // `inkwash-logic::rtc_latch`): while AF stays asserted and no
    // ACK/disable has released it, snapshot requests are answered from the
    // cached consistent read instead of touching the bus.
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
                // A plain time read does not carry AF, so it cannot update
                // the latch itself; AF is observed through AlarmStatus /
                // Snapshot reads and released by ACK/disable.
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
                    // AF cleared: release the duplicate latch.
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
                // Programming does not clear an asserted AF by itself; the
                // latch stays so an ACK that follows is the release point.
                let _ = reply.send(result);
            }
        }

        // Feed after each command as well as in the idle timeout branch. The
        // main loop continuously polls alarm state while pages are active, so
        // a busy command stream can otherwise prevent recv_timeout from ever
        // reaching its watchdog-feed timeout path.
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

/// Returns the consistent snapshot, honoring the duplicate latch (pure
/// logic in `inkwash-logic::rtc_latch`): while AF is asserted and no
/// ACK/disable has released it, repeated requests are answered from the
/// cached read without a fresh bus transaction.
fn snapshot_or_latch(
    rtc: &mut Pcf8563,
    latch: &mut inkwash_logic::rtc_latch::RtcSnapshotLatch,
) -> Result<RtcAlarmSnapshot> {
    if latch.action() == inkwash_logic::rtc_latch::SnapshotAction::UseCached {
        // A held latch answers directly - no fresh bus transaction for a
        // repeat request during the same asserted AF.
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
