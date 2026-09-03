//! Dedicated sync task: owns the process's single
//! `WifiManager` and executes every Wi-Fi operation off the main loop, so
//! an HTTPS sync (up to ~10 s on a flaky link) never blocks buttons, USB,
//! BLE, or the display. The main loop, menus, and control channels send
//! commands over a channel and receive results over per-command reply
//! channels.
//!
//! The task opens its own NVS store handles on the shared partition -
//! `EspNvs` is `Send` but not `Sync`, so two threads must not share one
//! store instance - and never touches the I2C bus: alarm reprogramming and
//! NTP alignment are applied by the main loop from the sync receipt, and
//! merged sync data is persisted by the state machine (the task never
//! writes the alarm/todo/inbox stores itself).

use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender};
use std::time::Duration;

use anyhow::Result;
use esp_idf_svc::nvs::EspDefaultNvsPartition;

use crate::alarms::AlarmStore;
use crate::inbox::InboxStore;
use crate::rtc::DateTime;
use crate::storage::{PersistedCounters, WifiCreds};
use crate::sync::{self, SyncOutcome, SyncResult};
use crate::todos::TodoStore;
use crate::wifi::WifiManager;

/// 16 KiB stack: the HTTPS + TLS (mbedTLS) stack is the deepest caller on
/// this thread; the default 4 KiB pthread stack would overflow, and 32 KiB
/// overran internal RAM at boot (`pthread: Failed to create task!` /
/// `Not enough space` - pthread stacks are forced to MALLOC_CAP_INTERNAL).
/// The old main task ran the same TLS code on 32 KiB with plenty to spare.
const SYNC_TASK_STACK: usize = 16 * 1024;

/// Commands the main loop dispatches to the sync task.
pub enum SyncCommand {
    /// Full "Sync Now": connect, fetch/apply, disconnect. `now` is the
    /// main loop's latest RTC read; the result goes to `reply`.
    SyncNow {
        now: DateTime,
        reply: Sender<SyncResult>,
    },
    /// Verify credentials by connecting, then persist them to NVS.
    SetWifi {
        creds: WifiCreds,
        reply: Sender<Result<()>>,
    },
    /// Lightweight urgent-message poll; `true` when the server has urgent
    /// content (the scheduler then dispatches a full sync).
    UrgentPoll { reply: Sender<Result<bool>> },
}

/// A Wi-Fi operation dispatched to the sync task, awaiting its receipt.
/// Owned by `DeviceContext::pending_wifi_op`; polled by
/// `DeviceContext::poll_wifi_ops`. `cmd` is the exact command the client
/// sent, kept so the dedup cache can be updated with the real reply once
/// the operation completes.
pub enum PendingWifiOp {
    Sync { reply: Receiver<SyncResult> },
    SetWifi { reply: Receiver<Result<()>> },
    UrgentPoll { reply: Receiver<Result<bool>> },
}

/// What a completed Wi-Fi operation reports, for the caller (main-loop
/// redraw, sync-now menu result). Errors are pre-rendered strings
/// (`anyhow::Error` is not `Clone`).
pub enum WifiOpEvent {
    SyncDone(Result<SyncOutcome, String>),
    SetWifiDone,
}

/// Client handle kept by the main loop: the command sender. The task owns
/// the Wi-Fi driver, so the main loop never touches Wi-Fi directly.
pub struct SyncTask {
    tx: Sender<SyncCommand>,
}

impl SyncTask {
    /// Spawns the sync task, moving the process's one `WifiManager` and a
    /// clone of the shared NVS partition into it. Call after boot-time
    /// Wi-Fi work (the boot NTP resync) is done.
    pub fn spawn(partition: EspDefaultNvsPartition, wifi: WifiManager) -> Result<Self> {
        let (tx, rx) = channel();
        std::thread::Builder::new()
            .name("sync".to_string())
            .stack_size(SYNC_TASK_STACK)
            .spawn(move || run(wifi, partition, rx))?;
        Ok(Self { tx })
    }

    pub fn sync_now(&self, now: DateTime) -> Result<Receiver<SyncResult>> {
        let (reply_tx, reply_rx) = channel();
        self.tx.send(SyncCommand::SyncNow {
            now,
            reply: reply_tx,
        })?;
        Ok(reply_rx)
    }

    pub fn set_wifi(&self, creds: WifiCreds) -> Result<Receiver<Result<()>>> {
        let (reply_tx, reply_rx) = channel();
        self.tx.send(SyncCommand::SetWifi {
            creds,
            reply: reply_tx,
        })?;
        Ok(reply_rx)
    }

    pub fn poll_urgent(&self) -> Result<Receiver<Result<bool>>> {
        let (reply_tx, reply_rx) = channel();
        self.tx.send(SyncCommand::UrgentPoll { reply: reply_tx })?;
        Ok(reply_rx)
    }
}

fn run(mut wifi: WifiManager, partition: EspDefaultNvsPartition, rx: Receiver<SyncCommand>) {
    // This task runs the HTTPS+TLS round-trips, so it must be TWDT-watched
    // too: the feeds inside sync.rs only take effect for subscribed tasks,
    // and an un-watched hang would leave a pending op stuck forever.
    let watchdog_subscribed = match crate::watchdog::subscribe() {
        Ok(()) => true,
        Err(err) => {
            log::warn!("Sync task watchdog subscribe failed: {err}");
            false
        }
    };
    // Independent NVS handles on the shared partition (see module doc).
    let (counters, alarm_store, todo_store, inbox_store) = match open_stores(partition) {
        Ok(stores) => stores,
        Err(err) => {
            log::error!("Sync task: failed to open NVS stores: {err}");
            return;
        }
    };

    loop {
        let cmd = match rx.recv_timeout(Duration::from_secs(1)) {
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
            SyncCommand::SyncNow { now, reply } => {
                let result = sync::sync_now(
                    &counters,
                    &mut wifi,
                    &alarm_store,
                    &todo_store,
                    &inbox_store,
                    &now,
                );
                let _ = reply.send(result);
            }
            SyncCommand::SetWifi { creds, reply } => {
                let result = verify_and_save(&mut wifi, &counters, &creds);
                let _ = reply.send(result);
            }
            SyncCommand::UrgentPoll { reply } => {
                let result = sync::poll_urgent(&counters, &mut wifi);
                let _ = reply.send(result);
            }
        }
    }
}

fn open_stores(
    partition: EspDefaultNvsPartition,
) -> Result<(PersistedCounters, AlarmStore, TodoStore, InboxStore)> {
    let counters = PersistedCounters::open(partition.clone())?;
    let alarm_store = AlarmStore::open(partition.clone())?;
    let todo_store = TodoStore::open(partition.clone())?;
    let inbox_store = InboxStore::open(partition)?;
    Ok((counters, alarm_store, todo_store, inbox_store))
}

fn verify_and_save(
    wifi: &mut WifiManager,
    counters: &PersistedCounters,
    creds: &WifiCreds,
) -> Result<()> {
    // Attempt to connect and verify the credentials work before saving.
    // Only credentials we know are valid end up in NVS; if the connection
    // fails, return an error without persisting.
    wifi.connect(creds)?;
    wifi.disconnect();
    counters.save_wifi_creds(creds)
}
