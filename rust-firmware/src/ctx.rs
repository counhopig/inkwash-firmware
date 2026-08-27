//! DeviceContext: a single bundle of the firmware's long-lived shared state,
//! so screen/sync/control functions pass one `&mut DeviceContext` instead of
//! threading individual arguments through every call site. `clock` is
//! intentionally NOT here: it's a transient value re-read from the RTC each
//! poll, so it stays an explicit parameter where it's needed.
//!
//! `ble_control` holds a `&'a mut Option<BleControl>` (not a `BleControl`
//! directly) because its *contents* have a distinct lifetime from the rest
//! of this struct - owned by the main loop, populated only while the BLE
//! pairing screen is open, and torn down (`= None`) when leaving it. Storing
//! the reference to the slot rather than threading `&mut Option<BleControl>`
//! as a separate parameter everywhere means every blocking screen that goes
//! through `DeviceContext` automatically gets the chance to reply `busy` to
//! a queued BLE command instead of leaving BLE the one control channel with
//! no reply at all during a ring/reminder.
//!
//! The store fields are `&'a` (immutable) because their methods all take
//! `&self` (the underlying NVS handles have internal mutability); only
//! `board`, `wifi_mgr`, `usb_console`, and `ble_control` need `&'a mut`. This
//! lets a function read a store and mutate the board in the same scope
//! without fighting the borrow checker.

use std::time::{Duration, Instant};

use crate::alarms::AlarmStore;
use crate::ble_control::BleControl;
use crate::board::Note4Board;
use crate::control::{self, Command, Reply};
use crate::inbox::InboxStore;
use crate::rtc::DateTime;
use crate::storage::PersistedCounters;
use crate::todos::TodoStore;
use crate::usb_console::UsbConsole;
use crate::wifi::WifiManager;

/// Wall-clock sync cursors shared by Home and every blocking UI loop.
/// Keeping them here prevents each screen from inventing its own timer and
/// ensures a successful first sync disables the fresh-device fast path.
pub struct SyncScheduler {
    last_urgent_boundary: u64,
    last_full_boundary: u64,
    never_synced: bool,
    last_ui_poll: Instant,
}

/// State needed to safely re-arm the PCF8563's single alarm slot without
/// immediately retriggering while its hour/minute comparator is still true.
pub struct AlarmScheduler {
    programmed_date: Option<(u16, u8, u8)>,
    rearm_after_minute: Option<u64>,
}

impl AlarmScheduler {
    pub fn new(now: Option<&DateTime>, fired_at_boot: bool) -> Self {
        Self {
            programmed_date: now.map(|dt| (dt.year, dt.month, dt.day)),
            rearm_after_minute: fired_at_boot
                .then(|| now.map(|dt| dt.to_unix() / 60))
                .flatten(),
        }
    }
}

impl SyncScheduler {
    pub fn new(now: Option<&DateTime>, counters: &PersistedCounters) -> Self {
        let unix = now.map(|dt| dt.to_unix()).unwrap_or(0);
        let interval = counters.sync_interval_minutes().unwrap_or(60) as u64;
        Self {
            last_urgent_boundary: inkwash_logic::scheduler::boundary_index(unix, 30),
            last_full_boundary: inkwash_logic::scheduler::boundary_index(unix, interval * 60),
            never_synced: counters.last_sync_epoch().unwrap_or(None).is_none(),
            last_ui_poll: Instant::now(),
        }
    }
}

/// All of the firmware's shared, long-lived state, created once in `main()`
/// and threaded through the UI/sync/control layers by reference.
pub struct DeviceContext<'a> {
    pub board: &'a mut Note4Board,
    pub counters: &'a PersistedCounters,
    pub wifi_mgr: &'a mut WifiManager,
    pub alarm_store: &'a AlarmStore,
    pub todo_store: &'a TodoStore,
    pub inbox_store: &'a InboxStore,
    pub usb_console: &'a mut UsbConsole,
    pub ble_control: &'a mut Option<BleControl>,
    pub sync_scheduler: SyncScheduler,
    pub alarm_scheduler: AlarmScheduler,
    /// The last id-tagged command `control::dispatch` actually executed
    /// (USB or BLE, whichever came last), and the reply it produced.
    /// Resent duplicates of that exact `(id, Command)` replay the cached
    /// reply instead of re-executing - see `control::dispatch`'s doc
    /// comment for why this exists and why the key includes `Command`,
    /// not just `id`.
    pub last_command: Option<(String, Command, Reply)>,
}

impl DeviceContext<'_> {
    /// Services one queued USB command from any UI loop. Returns true when a
    /// successful command may have changed visible device state and the
    /// current screen should redraw from its stores/RTC.
    pub fn poll_usb_control(&mut self, now: Option<&DateTime>) -> bool {
        let Some((id, cmd)) = self.usb_console.poll_command() else {
            return false;
        };
        let changes_visible_state = !matches!(cmd, Command::GetStatus);
        let fresh_now = self.board.rtc.read_time().ok();
        let reply = control::dispatch(self, id.as_deref(), cmd, fresh_now.as_ref().or(now));
        crate::usb_console::write_reply(&reply, id.as_deref());
        changes_visible_state && matches!(reply, Reply::Ok)
    }

    /// Services USB plus wall-clock scheduled sync while an ordinary screen
    /// owns the main thread. BLE pairing deliberately calls
    /// `poll_usb_control` directly because BLE and Wi-Fi share the radio.
    pub fn poll_background(&mut self, now: Option<&DateTime>) -> bool {
        let usb_changed = self.poll_usb_control(now);
        let runtime_changed =
            if self.sync_scheduler.last_ui_poll.elapsed() >= Duration::from_secs(1) {
                self.sync_scheduler.last_ui_poll = Instant::now();
                match self.board.rtc.read_time() {
                    Ok(fresh) => self.poll_runtime(&fresh),
                    Err(err) => {
                        log::warn!("RTC read failed in UI background poll: {err}");
                        false
                    }
                }
            } else {
                false
            };
        usb_changed || runtime_changed
    }

    /// Services all RTC-driven work shared by Home and ordinary screens.
    pub fn poll_runtime(&mut self, now: &DateTime) -> bool {
        let alarm_changed = self.poll_alarm(now);
        let sync_changed = self.poll_scheduled_sync(now);
        let reminder_changed = self.poll_reminders(now);
        alarm_changed || sync_changed || reminder_changed
    }

    /// Services alerts that do not use Wi-Fi. BLE pairing uses this variant
    /// because BLE and Wi-Fi cannot be active together on this device.
    pub fn poll_local_alerts(&mut self, now: &DateTime) -> bool {
        self.poll_alarm(now) || self.poll_reminders(now)
    }

    fn poll_reminders(&mut self, now: &DateTime) -> bool {
        crate::reminders::poll(
            self.board,
            self.counters,
            self.todo_store,
            self.inbox_store,
            self.usb_console,
            self.ble_control.as_mut(),
            now,
        )
    }

    /// Re-arms alarms at minute/date boundaries and handles a live AF flag.
    /// Returns true when a ringing screen replaced the current UI.
    pub fn poll_alarm(&mut self, now: &DateTime) -> bool {
        let current_date = (now.year, now.month, now.day);
        let current_minute = now.to_unix() / 60;
        if self
            .alarm_scheduler
            .rearm_after_minute
            .is_some_and(|minute| current_minute > minute)
        {
            match self.alarm_store.load().and_then(|list| {
                crate::alarms::program_hardware_alarm(&mut self.board.rtc, &list, now)
            }) {
                Ok(()) => {
                    self.alarm_scheduler.rearm_after_minute = None;
                    self.alarm_scheduler.programmed_date = Some(current_date);
                }
                Err(err) => log::warn!("Failed to re-arm alarm after ring: {err}"),
            }
        }
        if self.alarm_scheduler.rearm_after_minute.is_none()
            && self.alarm_scheduler.programmed_date != Some(current_date)
        {
            match self.alarm_store.load().and_then(|list| {
                crate::alarms::program_hardware_alarm(&mut self.board.rtc, &list, now)
            }) {
                Ok(()) => self.alarm_scheduler.programmed_date = Some(current_date),
                Err(err) => {
                    log::warn!("Failed to re-evaluate hardware alarm at date boundary: {err}")
                }
            }
        }
        if self.alarm_scheduler.rearm_after_minute.is_some() {
            return false;
        }
        match self.board.rtc.alarm_flag() {
            Ok(true) => {
                log::info!("RTC alarm fired while device was awake; ringing");
                if let Err(err) = crate::alarms::handle_fired_alarm(
                    self.board,
                    self.alarm_store,
                    self.usb_console,
                    self.ble_control.as_mut(),
                    Some(now),
                ) {
                    log::error!("Failed to handle live RTC alarm: {err}");
                }
                self.alarm_scheduler.rearm_after_minute = Some(current_minute);
                true
            }
            Ok(false) => false,
            Err(err) => {
                log::warn!("Failed to read RTC alarm flag: {err}");
                false
            }
        }
    }

    /// Runs an urgent/full sync when its wall-clock boundary advances.
    /// Returns true only when a full sync applied state that callers should
    /// redraw. Failed attempts are retried at the next boundary.
    pub fn poll_scheduled_sync(&mut self, now: &DateTime) -> bool {
        let server_configured = self
            .counters
            .device_config()
            .map(|cfg| cfg.is_some())
            .unwrap_or(false);
        let wifi_configured = self
            .counters
            .wifi_creds()
            .map(|creds| creds.is_some())
            .unwrap_or(false);
        if !server_configured || !wifi_configured {
            return false;
        }

        let interval = self.counters.sync_interval_minutes().unwrap_or(60) as u64;
        let unix = now.to_unix();
        let urgent_due = inkwash_logic::scheduler::boundary_advanced(
            unix,
            30,
            self.sync_scheduler.last_urgent_boundary,
        );
        let full_due = inkwash_logic::scheduler::boundary_advanced(
            unix,
            interval * 60,
            self.sync_scheduler.last_full_boundary,
        );
        if !urgent_due && !full_due {
            return false;
        }

        if urgent_due {
            self.sync_scheduler.last_urgent_boundary =
                inkwash_logic::scheduler::boundary_index(unix, 30);
            match crate::sync::poll_urgent(self.counters, self.wifi_mgr) {
                Ok(true) => {
                    log::info!("Urgent message available; syncing");
                    return self.run_full_sync(now);
                }
                Ok(false) => {}
                Err(err) => log::warn!("Urgent poll failed: {err}"),
            }
        }

        if full_due || (self.sync_scheduler.never_synced && urgent_due) {
            self.sync_scheduler.last_full_boundary =
                inkwash_logic::scheduler::boundary_index(unix, interval * 60);
            log::info!("Aligned sync due (interval {interval} min); syncing");
            return self.run_full_sync(now);
        }
        false
    }

    fn run_full_sync(&mut self, now: &DateTime) -> bool {
        match crate::sync::sync_now(
            self.counters,
            self.wifi_mgr,
            self.alarm_store,
            self.todo_store,
            self.inbox_store,
            &mut self.board.rtc,
            now,
        ) {
            Ok(_) => {
                self.sync_scheduler.never_synced = false;
                log::info!("Full sync completed");
                true
            }
            Err(err) => {
                log::warn!("Full sync failed: {err}");
                false
            }
        }
    }
}
