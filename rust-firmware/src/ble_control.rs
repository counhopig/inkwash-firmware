//! BLE GATT control channel for on-demand pairing (Phase 5).
//!
//! NimBLE owns thread-affine global state and its initialization can block for
//! several seconds. `BleControl` is therefore only a main-loop handle. A
//! dedicated worker owns the `BleSession` and serializes start, replies, and
//! stop commands; callback facts are forwarded through bounded channels.

use std::sync::mpsc;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Result};
use esp32_nimble::utilities::mutex::Mutex;
use esp32_nimble::{uuid128, BLEAdvertisementData, BLECharacteristic, BLEDevice, NimbleProperties};

use crate::control;

const CHANNEL_CAPACITY: usize = 16;
/// BLE setup is driven from this worker, but the pthread stack itself lives
/// in PSRAM so NimBLE can reserve internal RAM for its controller pools.
/// 8 KiB leaves headroom over the configured 5120-byte NimBLE host stack
/// without consuming the scarce internal heap.
const BLE_TASK_STACK: usize = 8 * 1024;
/// Match the ESP32-S3 controller's allocation capabilities. NimBLE host
/// buffers are separate; controller startup uses internal DMA-capable RAM.
const BLE_INTERNAL_CAPS: u32 =
    esp_idf_svc::sys::MALLOC_CAP_INTERNAL | esp_idf_svc::sys::MALLOC_CAP_DMA;

const SERVICE_UUID: &str = "d2c25e50-5e22-48d8-a8b3-34f2f8e2c7d4";
const WRITE_CHAR_UUID: &str = "d2c25e51-5e22-48d8-a8b3-34f2f8e2c7d4";
const NOTIFY_CHAR_UUID: &str = "d2c25e52-5e22-48d8-a8b3-34f2f8e2c7d4";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BleLifecycle {
    Connected,
    Disconnected,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BleTaskResult {
    Started { session_id: u64 },
    Failed { session_id: u64, message: String },
    Stopped { session_id: u64 },
}

enum WorkerCommand {
    Start { name: String, session_id: u64 },
    Stop { session_id: u64 },
    Reply(String),
}

/// Main-loop handle for the BLE worker. No NimBLE handle crosses this type's
/// thread boundary; the worker owns the session for its whole lifetime.
pub struct BleControl {
    command_tx: mpsc::SyncSender<WorkerCommand>,
    rx: mpsc::Receiver<(Option<String>, control::Command)>,
    lifecycle_rx: mpsc::Receiver<BleLifecycle>,
    result_rx: mpsc::Receiver<BleTaskResult>,
}

impl BleControl {
    /// Starts the worker thread. NimBLE is not initialized until the state
    /// machine submits `StartBlePairing`, so boot and the main loop remain
    /// responsive while the radio is being brought up.
    pub fn spawn() -> Result<Self> {
        let (command_tx, command_rx) = mpsc::sync_channel(CHANNEL_CAPACITY);
        let (tx, rx) = mpsc::sync_channel(CHANNEL_CAPACITY);
        let (lifecycle_tx, lifecycle_rx) = mpsc::sync_channel(CHANNEL_CAPACITY);
        let (result_tx, result_rx) = mpsc::sync_channel(CHANNEL_CAPACITY);
        // ESP-IDF pthread stacks default to internal RAM. Put this worker's
        // stack in external PSRAM; its NimBLE handles remain worker-owned,
        // while the internal heap is reserved for controller allocations.
        let mut pthread_cfg = unsafe { esp_idf_svc::sys::esp_pthread_get_default_config() };
        pthread_cfg.stack_size = BLE_TASK_STACK;
        pthread_cfg.stack_alloc_caps =
            esp_idf_svc::sys::MALLOC_CAP_SPIRAM | esp_idf_svc::sys::MALLOC_CAP_8BIT;
        pthread_cfg.inherit_cfg = false;
        esp_idf_svc::sys::esp!(unsafe { esp_idf_svc::sys::esp_pthread_set_cfg(&pthread_cfg) })
            .map_err(|e| anyhow!("BLE worker PSRAM stack configuration failed: {e:?}"))?;
        let worker = std::thread::Builder::new()
            .name("ble".to_string())
            .stack_size(BLE_TASK_STACK)
            .spawn(move || run(command_rx, tx, lifecycle_tx, result_tx));
        // Do not leak the BLE-specific pthread policy into any later thread
        // creation on the main task.
        let default_cfg = unsafe { esp_idf_svc::sys::esp_pthread_get_default_config() };
        if let Err(err) =
            esp_idf_svc::sys::esp!(unsafe { esp_idf_svc::sys::esp_pthread_set_cfg(&default_cfg) })
        {
            log::warn!("BLE worker pthread policy restore failed: {err:?}");
        }
        worker?;
        Ok(Self {
            command_tx,
            rx,
            lifecycle_rx,
            result_rx,
        })
    }

    /// Queue radio initialization without blocking the main loop.
    pub fn start(&self, name: &str, session_id: u64) -> Result<()> {
        self.command_tx
            .try_send(WorkerCommand::Start {
                name: name.to_string(),
                session_id,
            })
            .map_err(|err| anyhow!("BLE worker start queue unavailable: {err}"))
    }

    /// Queue radio teardown without blocking the main loop. The worker's
    /// command order guarantees Stop runs before a later Start.
    pub fn stop(&self, session_id: u64) -> Result<()> {
        self.command_tx
            .try_send(WorkerCommand::Stop { session_id })
            .map_err(|err| anyhow!("BLE worker stop queue unavailable: {err}"))
    }

    pub fn poll_result(&self) -> Option<BleTaskResult> {
        self.result_rx.try_recv().ok()
    }

    pub fn poll_command(&self) -> Option<(Option<String>, control::Command)> {
        match self.rx.try_recv() {
            Ok(parsed) => Some(parsed),
            Err(mpsc::TryRecvError::Empty) => None,
            Err(mpsc::TryRecvError::Disconnected) => {
                log::warn!("BLE: command channel disconnected");
                None
            }
        }
    }

    pub fn poll_lifecycle(&self) -> Option<BleLifecycle> {
        self.lifecycle_rx.try_recv().ok()
    }

    /// Forward a reply to the worker-owned notify characteristic.
    pub fn write_reply(&self, reply: &control::Reply, id: Option<&str>) {
        let json = control::render_reply(reply, id);
        if let Err(err) = self.command_tx.try_send(WorkerCommand::Reply(json)) {
            log::warn!("BLE: reply queue unavailable: {err}");
        }
    }
}

impl Drop for BleControl {
    fn drop(&mut self) {
        // Best-effort wakeup; dropping the last sender also makes the worker
        // leave its receive loop and drop any session it still owns.
        let _ = self
            .command_tx
            .try_send(WorkerCommand::Stop { session_id: 0 });
    }
}

fn run(
    command_rx: mpsc::Receiver<WorkerCommand>,
    tx: mpsc::SyncSender<(Option<String>, control::Command)>,
    lifecycle_tx: mpsc::SyncSender<BleLifecycle>,
    result_tx: mpsc::SyncSender<BleTaskResult>,
) {
    let mut session = None;
    loop {
        match command_rx.recv_timeout(Duration::from_millis(20)) {
            Ok(WorkerCommand::Start {
                name: _name,
                session_id,
            }) => {
                // A re-entry is serialized after the preceding Stop and is
                // therefore safe even if the caller moved quickly.
                session.take();
                match BleSession::start() {
                    Ok(new_session) => {
                        session = Some(new_session);
                        let _ = result_tx.try_send(BleTaskResult::Started { session_id });
                    }
                    Err(err) => {
                        let _ = result_tx.try_send(BleTaskResult::Failed {
                            session_id,
                            message: format!("{err:#}"),
                        });
                    }
                }
            }
            Ok(WorkerCommand::Stop { session_id }) => {
                session.take();
                let _ = result_tx.try_send(BleTaskResult::Stopped { session_id });
            }
            Ok(WorkerCommand::Reply(json)) => {
                if let Some(active) = session.as_ref() {
                    active.notify(json.as_bytes());
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
        if let Some(active) = session.as_ref() {
            while let Some(command) = active.poll_command() {
                if tx.try_send(command).is_err() {
                    log::warn!("BLE: main-loop command queue full");
                    break;
                }
            }
            while let Some(event) = active.poll_lifecycle() {
                if lifecycle_tx.try_send(event).is_err() {
                    log::warn!("BLE: lifecycle queue full");
                    break;
                }
            }
        }
    }
    session.take();
}

/// The NimBLE-owning half of the BLE implementation. It never crosses the
/// worker boundary, including its callback channels and notify handle.
struct BleSession {
    rx: mpsc::Receiver<(Option<String>, control::Command)>,
    _tx: mpsc::SyncSender<(Option<String>, control::Command)>,
    lifecycle_rx: mpsc::Receiver<BleLifecycle>,
    _lifecycle_tx: mpsc::SyncSender<BleLifecycle>,
    notify_char: Arc<Mutex<BLECharacteristic>>,
}

impl BleSession {
    fn start() -> Result<Self> {
        let free = unsafe { esp_idf_svc::sys::heap_caps_get_free_size(BLE_INTERNAL_CAPS) };
        let largest =
            unsafe { esp_idf_svc::sys::heap_caps_get_largest_free_block(BLE_INTERNAL_CAPS) };
        log::info!("BLE preflight: internal free={free} largest_block={largest} bytes");
        if !inkwash_logic::ble_memory::sufficient_internal_heap(free, largest) {
            return Err(anyhow!(
                "BLE unavailable: internal heap free={free} largest_block={largest}"
            ));
        }
        BLEDevice::init();
        let result = Self::start_initialized();
        if result.is_err() {
            if let Err(err) = BLEDevice::deinit_full() {
                log::warn!("BLE cleanup after start failure failed: {err:?}");
            }
        }
        result
    }

    fn start_initialized() -> Result<Self> {
        let device = BLEDevice::take();
        let ble_advertising = device.get_advertising();
        let server = device.get_server();

        let (lifecycle_tx, lifecycle_rx) = mpsc::sync_channel(CHANNEL_CAPACITY);
        let lc_tx = lifecycle_tx.clone();
        server.on_connect(move |_server, _desc| {
            log::info!("BLE client connected");
            let _ = lc_tx.try_send(BleLifecycle::Connected);
        });
        let lc_tx = lifecycle_tx.clone();
        server.on_disconnect(move |_desc, _reason| {
            log::info!("BLE client disconnected ({_reason:?})");
            let _ = lc_tx.try_send(BleLifecycle::Disconnected);
        });

        let control_service = server.create_service(uuid128!(SERVICE_UUID));
        let write_char = control_service
            .lock()
            .create_characteristic(uuid128!(WRITE_CHAR_UUID), NimbleProperties::WRITE);
        let notify_char = control_service.lock().create_characteristic(
            uuid128!(NOTIFY_CHAR_UUID),
            NimbleProperties::READ | NimbleProperties::NOTIFY,
        );
        notify_char.lock().set_value(b"");

        let (tx, rx) = mpsc::sync_channel(CHANNEL_CAPACITY);
        let tx_for_callback = tx.clone();
        write_char.lock().on_write(move |args| {
            match String::from_utf8(args.recv_data().to_vec()) {
                Ok(line) => match control::parse_command(&line) {
                    Ok(parsed) => {
                        if let Err(err) = tx_for_callback.try_send(parsed) {
                            log::warn!("BLE: command queue unavailable: {err}");
                        }
                    }
                    Err(err) => log::warn!("BLE: failed to parse command '{line}': {err}"),
                },
                Err(_) => log::warn!("BLE: received non-UTF-8 command data"),
            }
        });

        ble_advertising
            .lock()
            .set_data(
                BLEAdvertisementData::new()
                    .name("Inkwash")
                    .add_service_uuid(uuid128!(SERVICE_UUID)),
            )
            .map_err(|e| anyhow!("BLE set advertisement data failed: {e:?}"))?;
        ble_advertising
            .lock()
            .start()
            .map_err(|e| anyhow!("BLE start advertising failed: {e:?}"))?;
        log::info!("BLE advertising started");

        Ok(Self {
            rx,
            _tx: tx,
            lifecycle_rx,
            _lifecycle_tx: lifecycle_tx,
            notify_char,
        })
    }

    fn poll_command(&self) -> Option<(Option<String>, control::Command)> {
        self.rx.try_recv().ok()
    }

    fn poll_lifecycle(&self) -> Option<BleLifecycle> {
        self.lifecycle_rx.try_recv().ok()
    }

    fn notify(&self, json: &[u8]) {
        self.notify_char.lock().set_value(json).notify();
    }
}

impl Drop for BleSession {
    fn drop(&mut self) {
        if let Err(err) = BLEDevice::deinit_full() {
            log::warn!("BLE deinit_full failed: {err:?}");
        } else {
            log::info!("BLE control torn down; advertising stopped");
        }
    }
}
