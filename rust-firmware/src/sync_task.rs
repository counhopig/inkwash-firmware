use std::sync::mpsc::{
    channel, sync_channel, Receiver, RecvTimeoutError, Sender, SyncSender, TrySendError,
};
use std::time::Duration;

use anyhow::Result;
use esp_idf_svc::nvs::EspDefaultNvsPartition;

use crate::alarms::AlarmStore;
use crate::inbox::InboxStore;
use crate::rtc::DateTime;
use crate::storage::{PersistedCounters, WifiCreds};
use crate::sync::{self, SyncResult};
use crate::todos::TodoStore;
use crate::wifi::WifiManager;

const SYNC_TASK_STACK: usize = 16 * 1024;

const SYNC_COMMAND_CAPACITY: usize = 4;

pub enum SyncCommand {
    SyncNow {
        now: DateTime,
        reply: Sender<SyncResult>,
    },

    SetWifi {
        creds: WifiCreds,
        reply: Sender<Result<WifiCreds>>,
    },

    UrgentPoll {
        reply: Sender<Result<bool>>,
    },

    SuspendForBle {
        reply: Sender<Result<()>>,
    },

    ResumeAfterBle {
        reply: Sender<Result<()>>,
    },
}

pub enum PendingWifiOp {
    Sync {
        reply: Receiver<SyncResult>,
    },
    SetWifi {
        reply: Receiver<Result<WifiCreds>>,
    },
    PostBleSetWifi {
        reply: Receiver<Result<WifiCreds>>,
    },
    UrgentPoll {
        reply: Receiver<Result<bool>>,
    },
    SuspendForBle {
        reply: Receiver<Result<()>>,
        session_id: u64,
        name: String,
    },
    ResumeAfterBle {
        reply: Receiver<Result<()>>,
    },
}

pub struct SyncTask {
    tx: SyncSender<SyncCommand>,
}

impl SyncTask {
    pub fn spawn(partition: EspDefaultNvsPartition, wifi: WifiManager) -> Result<Self> {
        let (tx, rx) = sync_channel(SYNC_COMMAND_CAPACITY);
        std::thread::Builder::new()
            .name("sync".to_string())
            .stack_size(SYNC_TASK_STACK)
            .spawn(move || run(wifi, partition, rx))?;
        Ok(Self { tx })
    }

    pub fn sync_now(&self, now: DateTime) -> Result<Receiver<SyncResult>> {
        let (reply_tx, reply_rx) = channel();
        self.try_send(SyncCommand::SyncNow {
            now,
            reply: reply_tx,
        })?;
        Ok(reply_rx)
    }

    pub fn set_wifi(&self, creds: WifiCreds) -> Result<Receiver<Result<WifiCreds>>> {
        let (reply_tx, reply_rx) = channel();
        self.try_send(SyncCommand::SetWifi {
            creds,
            reply: reply_tx,
        })?;
        Ok(reply_rx)
    }

    pub fn poll_urgent(&self) -> Result<Receiver<Result<bool>>> {
        let (reply_tx, reply_rx) = channel();
        self.try_send(SyncCommand::UrgentPoll { reply: reply_tx })?;
        Ok(reply_rx)
    }

    pub fn suspend_for_ble(&self) -> Result<Receiver<Result<()>>> {
        let (reply_tx, reply_rx) = channel();
        self.try_send(SyncCommand::SuspendForBle { reply: reply_tx })?;
        Ok(reply_rx)
    }

    pub fn resume_after_ble(&self) -> Result<Receiver<Result<()>>> {
        let (reply_tx, reply_rx) = channel();
        self.try_send(SyncCommand::ResumeAfterBle { reply: reply_tx })?;
        Ok(reply_rx)
    }

    fn try_send(&self, command: SyncCommand) -> Result<()> {
        self.tx.try_send(command).map_err(|err| match err {
            TrySendError::Full(_) => anyhow::anyhow!("sync command queue is full"),
            TrySendError::Disconnected(_) => anyhow::anyhow!("sync task disconnected"),
        })
    }
}

fn run(mut wifi: WifiManager, partition: EspDefaultNvsPartition, rx: Receiver<SyncCommand>) {
    let watchdog_subscribed = match crate::watchdog::subscribe() {
        Ok(()) => true,
        Err(err) => {
            log::warn!("Sync task watchdog subscribe failed: {err}");
            false
        }
    };

    let (counters, alarm_store, todo_store, inbox_store) = match open_stores(partition) {
        Ok(stores) => stores,
        Err(err) => {
            log::error!("Sync task: failed to open NVS stores: {err}");
            return;
        }
    };

    let mut wifi_suspended = false;
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
                let result = if wifi_suspended {
                    crate::sync::SyncResult {
                        outcome: Err(anyhow::anyhow!("Wi-Fi is suspended for BLE pairing")),
                        ntp_epoch: None,
                    }
                } else {
                    sync::sync_now(
                        &counters,
                        &mut wifi,
                        &alarm_store,
                        &todo_store,
                        &inbox_store,
                        &now,
                    )
                };
                let _ = reply.send(result);
            }
            SyncCommand::SetWifi { creds, reply } => {
                let result = if wifi_suspended {
                    Err(anyhow::anyhow!("Wi-Fi is suspended for BLE pairing"))
                } else {
                    verify_credentials(&mut wifi, &creds)
                };
                let _ = reply.send(result);
            }
            SyncCommand::UrgentPoll { reply } => {
                let result = if wifi_suspended {
                    Err(anyhow::anyhow!("Wi-Fi is suspended for BLE pairing"))
                } else {
                    sync::poll_urgent(&counters, &mut wifi)
                };
                let _ = reply.send(result);
            }
            SyncCommand::SuspendForBle { reply } => {
                let result = wifi.suspend_for_ble();
                if result.is_ok() {
                    wifi_suspended = true;
                }
                let _ = reply.send(result);
            }
            SyncCommand::ResumeAfterBle { reply } => {
                let result = wifi.resume_after_ble();
                if result.is_ok() {
                    wifi_suspended = false;
                } else {
                    wifi.abort_ble_resume();
                    wifi_suspended = false;
                }
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

fn verify_credentials(wifi: &mut WifiManager, creds: &WifiCreds) -> Result<WifiCreds> {
    wifi.connect(creds)?;
    wifi.disconnect();
    Ok(creds.clone())
}
