//! BLE GATT control channel for on-demand pairing (Phase 5).
//!
//! Mirrors `usb_console.rs`'s shape: commands arrive via a WRITE
//! characteristic (fired from the NimBLE stack's own task context, not the
//! main loop thread - `Note4Board` isn't safe to touch from there), pushed
//! into a bounded `std::sync::mpsc` channel, then drained non-blockingly by
//! the main loop each poll cycle via `poll_command()` and dispatched
//! through `control::dispatch()` - the same entry point USB uses. Replies
//! go out via a separate NOTIFY characteristic.
//!
//! The BLE stack is only brought up when the user enters the pairing
//! screen (`screens::ble_pairing_screen`) and fully torn down
//! (`BLEDevice::deinit_full()`) when they leave it, since NimBLE costs
//! real RAM (~150KB) this device also needs for Wi-Fi/display/audio.

use std::sync::mpsc;
use std::sync::Arc;

use anyhow::{anyhow, Result};
use esp32_nimble::utilities::mutex::Mutex;
use esp32_nimble::{uuid128, BLEAdvertisementData, BLECharacteristic, BLEDevice, NimbleProperties};

use crate::control;

/// Bounded channel size for queued commands, matching `usb_console.rs`.
const CHANNEL_CAPACITY: usize = 16;

/// Service UUID128: d2c25e50-5e22-48d8-a8b3-34f2f8e2c7d4
/// Write characteristic (commands in) UUID128: d2c25e51-...
/// Notify characteristic (replies out) UUID128: d2c25e52-...
/// See `docs/control-protocol.md`'s "BLE Framing" section for the full
/// wire-format contract these carry (same JSON `Command`/`Reply` schema as
/// USB, no line-framing needed since GATT is already message-delimited).
const SERVICE_UUID: &str = "d2c25e50-5e22-48d8-a8b3-34f2f8e2c7d4";
const WRITE_CHAR_UUID: &str = "d2c25e51-5e22-48d8-a8b3-34f2f8e2c7d4";
const NOTIFY_CHAR_UUID: &str = "d2c25e52-5e22-48d8-a8b3-34f2f8e2c7d4";

/// A radio lifecycle fact reported by the NimBLE callbacks (which run on
/// the NimBLE host task, never the main thread) and drained non-blockingly
/// by the main loop so the state machine can react - the same shape as the
/// command channel below.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BleLifecycle {
    /// A client connected (GATT). While Screen::BlePairing is current the
    /// SM treats this as pairing having begun (BlePairingStarted).
    Connected,
    /// The client disconnected.
    Disconnected,
}

/// Handle to poll BLE commands and manage the GATT server lifecycle.
pub struct BleControl {
    rx: mpsc::Receiver<String>,
    /// Kept alive only so cloning it into the write callback's closure
    /// doesn't outlive the channel; never sent from directly.
    _tx: mpsc::SyncSender<String>,
    lifecycle_rx: mpsc::Receiver<BleLifecycle>,
    /// Sender cloned into the connect/disconnect callbacks.
    _lifecycle_tx: mpsc::SyncSender<BleLifecycle>,
    notify_char: Arc<Mutex<BLECharacteristic>>,
}

impl BleControl {
    /// Brings up the NimBLE stack, registers the control service, and
    /// starts advertising. Torn down by `Drop`.
    pub fn start() -> Result<Self> {
        // `BLEDevice::take()` forces a process-wide `Lazy` that only runs
        // the underlying `nimble_port_init()` the *first* time it's
        // forced - after `Drop`'s `deinit_full()` below, a later `take()`
        // alone would silently return a device backed by a stopped stack.
        // `BLEDevice::init()` is what actually (re)starts the port, and is
        // itself idempotent (internally flag-guarded), so calling it
        // unconditionally here is correct on both first entry and re-entry.
        BLEDevice::init();
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
        // Runs in the NimBLE stack's own task context, not the main loop
        // thread - must never touch `Note4Board`. Only pushes into the
        // channel; `control::dispatch` is called exclusively from the main
        // loop after draining it via `poll_command`.
        write_char.lock().on_write(move |args| {
            match String::from_utf8(args.recv_data().to_vec()) {
                Ok(line) => {
                    // Never block the NimBLE host task. A full queue means the
                    // main loop is busy; dropping one command is recoverable,
                    // wedging the radio callback is not.
                    if let Err(err) = tx_for_callback.try_send(line) {
                        log::warn!("BLE: command queue unavailable: {err}");
                    }
                }
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

    /// Non-blocking poll for the next BLE command. Returns
    /// `Some((id, Command))` if a complete command was received and parsed
    /// successfully (`id` is the client's correlation id, if it sent one),
    /// or `None` if there are no queued commands, or if one was queued but
    /// failed to parse (a warning is logged in that case).
    pub fn poll_command(&self) -> Option<(Option<String>, control::Command)> {
        match self.rx.try_recv() {
            Ok(line) => match control::parse_command(&line) {
                Ok(parsed) => Some(parsed),
                Err(err) => {
                    log::warn!("BLE: failed to parse command '{line}': {err}");
                    None
                }
            },
            Err(mpsc::TryRecvError::Empty) => None,
            Err(mpsc::TryRecvError::Disconnected) => {
                log::warn!("BLE: command channel disconnected");
                None
            }
        }
    }

    /// Non-blocking poll for the next radio lifecycle fact. Returns
    /// `Some(BleLifecycle)` if a connect/disconnect fired since the last
    /// poll, else `None`.
    pub fn poll_lifecycle(&mut self) -> Option<BleLifecycle> {
        self.lifecycle_rx.try_recv().ok()
    }

    /// Sends a reply to the connected BLE client via the notify
    /// characteristic, echoing back the triggering command's correlation
    /// `id` if it had one. A no-op (from the client's perspective) if
    /// nothing is connected/subscribed - NimBLE just drops notifications
    /// with no subscriber rather than erroring, so there's nothing to
    /// propagate.
    pub fn write_reply(&self, reply: &control::Reply, id: Option<&str>) {
        let json = control::render_reply(reply, id);
        self.notify_char.lock().set_value(json.as_bytes()).notify();
    }
}

impl Drop for BleControl {
    fn drop(&mut self) {
        // `deinit_full()` stops the NimBLE port, resets the server (which
        // drops the service/characteristics this instance registered), and
        // resets advertising config - this is the actual RAM/radio
        // reclaim, not just an ordinary Rust field drop (`BLEDevice`,
        // `BLEServer`, and `BLEAdvertising` are all `&'static` handles into
        // process-wide state, not values this struct owns, so nothing here
        // would be freed without this explicit call).
        if let Err(err) = BLEDevice::deinit_full() {
            log::warn!("BLE deinit_full failed: {err:?}");
        } else {
            log::info!("BLE control torn down; advertising stopped");
        }
    }
}
