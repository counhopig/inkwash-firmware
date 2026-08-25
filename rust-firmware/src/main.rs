mod alarms;
mod audio;
mod ble_control;
mod board;
mod button;
mod canvas;
mod control;
mod ctx;
mod display;
mod font5x7;
mod font8x16;
mod font_cjk;
mod home;
mod icons;
mod inbox;
mod nfc;
mod nvs_blob;
mod power;
mod reminders;
mod rtc;
mod screens;
mod storage;
mod sync;
mod todos;
mod ui;
mod usb_console;
mod watchdog;
mod wifi;

use std::thread;
use std::time::Duration;

use alarms::AlarmStore;
use anyhow::Result;
use board::Note4Board;
use button::{ButtonEvent, POLL_INTERVAL_MS};
use canvas::Rect;
use ctx::{AlarmScheduler, DeviceContext, SyncScheduler};
use inbox::InboxStore;
use rtc::DateTime;
use storage::PersistedCounters;
use todos::TodoStore;

/// Poll cycles between two consecutive `power status` log lines.
/// 50 × 20 ms = 1 s.
const STATUS_REPORT_INTERVAL_POLLS: u32 = 50;

/// Poll cycles between two consecutive PCF8563 re-reads.
/// 60 × 20 ms = 1.2 s, fast enough that the on-screen seconds tick at least
/// once per refresh.
const CLOCK_POLL_INTERVAL_POLLS: u32 = 60;

/// Bounding rect for the large home clock and its date/status metadata.
const CLOCK_RECT: Rect = Rect {
    x: 16,
    y: 36,
    width: 368,
    height: 92,
};

const FULL_SCREEN_RECT: Rect = Rect {
    x: 0,
    y: 0,
    width: 400,
    height: 300,
};

/// Epoch seconds captured at firmware build time. Used as the fallback RTC
/// seed when PCF8563 reports `voltage_low = true` (battery was disconnected
/// or drained). Captured by `build.rs` so a rebuild refreshes the value;
/// once the RTC keeps time on its coin cell we stop consulting this.
const BUILD_EPOCH_SECS: u64 = build_epoch_secs();

const fn build_epoch_secs() -> u64 {
    let bytes: &[u8] = env!("BUILD_EPOCH_SECS").as_bytes();
    let mut i = 0;
    let mut value: u64 = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if !b.is_ascii_digit() {
            break;
        }
        value = value * 10 + (b - b'0') as u64;
        i += 1;
    }
    value
}

fn main() -> Result<()> {
    esp_idf_svc::sys::link_patches();
    esp_idf_svc::log::EspLogger::initialize_default();

    log::info!(
        "Inkwash NOTE4 Rust bring-up starting (git {})",
        env!("GIT_REV")
    );
    if let Err(err) = watchdog::subscribe() {
        log::warn!("Task watchdog subscribe failed: {err}");
    }
    power::log_wakeup_cause();
    let mut board = Note4Board::take()?;
    log::info!("Power latch is high; rendering home screen");

    // Taken once and cloned into each store: `EspDefaultNvsPartition::take()`
    // is a true singleton (a global taken-flag, not a ref-counted "take a
    // new handle" call) and errors with `ESP_ERR_INVALID_STATE` if called
    // again while an earlier handle is still alive - three independent
    // `open()`s each calling `take()` themselves made every boot fail here
    // once `alarms.rs`/`todos.rs` were added, since `counters`'s handle was
    // still alive when `AlarmStore::open()` tried to take its own.
    let nvs_partition = esp_idf_svc::nvs::EspDefaultNvsPartition::take()
        .map_err(|e| anyhow::anyhow!("failed to initialise default NVS partition: {e}"))?;
    let counters = PersistedCounters::open(nvs_partition.clone())?;
    let alarm_store = AlarmStore::open(nvs_partition.clone())?;
    let todo_store = TodoStore::open(nvs_partition.clone())?;
    let inbox_store = InboxStore::open(nvs_partition)?;
    let mut usb_console = usb_console::UsbConsole::start();

    // Wi-Fi/NTP resync is needed only when the battery-backed RTC cannot be
    // trusted. A firmware flash or ordinary reset does not erase PCF8563
    // time, so connecting on every reset merely consumes the one safe Wi-Fi
    // session and forces the first user-triggered Sync Now to reboot.
    let mut needs_wifi_sync = false;
    let mut clock = match board.rtc.read_time() {
        Ok(mut dt) => {
            log::info!(
                "PCF8563: {:04}-{:02}-{:02} {:02}:{:02}:{:02} vl={}",
                dt.year,
                dt.month,
                dt.day,
                dt.hour,
                dt.minute,
                dt.second,
                dt.voltage_low
            );
            if dt.voltage_low {
                log::warn!("PCF8563 VL set (RTC battery low/lost); reseeding from build time");
                needs_wifi_sync = true;
                let offset = counters.timezone_offset_minutes().unwrap_or(0);
                let seeded = DateTime::from_unix(BUILD_EPOCH_SECS).shifted_minutes(offset as i32);
                if let Err(err) = board.rtc.write_time(&seeded) {
                    log::warn!("PCF8563 reseed failed: {err}");
                } else {
                    dt = seeded;
                }
            }
            Some(dt)
        }
        Err(err) => {
            log::warn!("PCF8563 read_time failed: {err}");
            needs_wifi_sync = true;
            None
        }
    };

    // Ring before the normal boot render, if this boot is the RTC alarm
    // firing: latency to sound matters more than latency to the home
    // screen. `power::wake_cause()` reads `esp_sleep_get_wakeup_cause`
    // again (harmless, not a consuming read); unlike the raw cause logged
    // above, this distinguishes ENTER-wake from alarm-wake.
    let alarm_fired_at_boot = power::wake_cause() == power::WakeCause::RtcAlarm;
    if alarm_fired_at_boot {
        log::info!("Woke from RTC alarm; ringing");
        // BLE is never up this early (only started on entering the pairing
        // screen), so there is nothing to poll yet - `None` is accurate,
        // not a shortcut.
        alarms::handle_fired_alarm(
            &mut board,
            &alarm_store,
            &mut usb_console,
            None,
            clock.as_ref(),
        )?;
    }

    // Keep the PCF8563's single hardware alarm slot pointed at whichever
    // stored alarm is nearest, every boot: after arming/editing an alarm,
    // after a ring+ack above, or just because nothing armed it yet this
    // session (the RTC keeps its own alarm config across deep sleep, but a
    // fresh flash or an edit made while the device was off both need this).
    if !alarm_fired_at_boot {
        if let Some(dt) = clock.as_ref() {
            match alarm_store.load() {
                Ok(list) => {
                    if let Err(err) = alarms::program_hardware_alarm(&mut board.rtc, &list, dt) {
                        log::warn!("Failed to program hardware alarm: {err}");
                    }
                }
                Err(err) => log::warn!("Failed to load alarms: {err}"),
            }
        }
    }

    render_home_now(
        &mut board,
        &counters,
        &alarm_store,
        &todo_store,
        &inbox_store,
        clock.as_ref(),
    );
    board.display.refresh_full()?;
    log::info!("Initial display refresh completed");

    if board.audio.is_none() {
        log::warn!("ES8311 not available");
    }
    if board.nfc.is_none() {
        log::warn!("NFC not available");
    }

    report_power_state(&mut board)?;

    // Taken once up front (it's a singleton) so both the boot-time Wi-Fi
    // bring-up below and the on-device Wi-Fi setup wizard (triggered from
    // the main loop by holding UP, or from the menu) can use it.
    let sysloop = esp_idf_svc::eventloop::EspSystemEventLoop::take()?;

    // Also created exactly once and reused for the rest of the program -
    // see `wifi::WifiManager`'s doc comment for why: a second
    // `EspWifi::new()` anywhere in the process reliably crashes
    // (`Guru Meditation Error: InstrFetchProhibited`), confirmed on real
    // hardware, so every Wi-Fi user (boot-time sync below, the setup
    // wizard, `sync::sync_now`) shares this one instance instead of each
    // creating and dropping its own.
    let mut wifi_mgr = wifi::WifiManager::new(&sysloop)?;

    // Optional Wi-Fi bring-up: connect with credentials stored in NVS
    // (`wifi_ssid` / `wifi_pass`), then sync the clock over NTP and push the
    // time into the PCF8563 so it keeps ticking while the device sleeps.
    // Failure to connect or sync only logs a warning; the rest of the UI
    // keeps working regardless. A healthy battery-backed RTC needs no boot
    // network traffic; periodic auto-sync (see the main loop below) brings
    // Wi-Fi up on its own schedule, and multiple connects per boot are safe
    // (see `wifi::WifiManager`).
    if !needs_wifi_sync {
        log::info!("RTC is healthy; skipping boot-time Wi-Fi/NTP resync");
    } else {
        match counters.wifi_creds() {
            Ok(Some(creds)) => match wifi_mgr.connect(&creds) {
                Ok(()) => {
                    let timezone_offset = counters.timezone_offset_minutes().unwrap_or(0);
                    match wifi::ntp_sync_and_set_rtc(&mut board.rtc, timezone_offset) {
                        Ok(()) => match board.rtc.read_time() {
                            Ok(dt) => {
                                clock = Some(dt);
                                render_home_now(
                                    &mut board,
                                    &counters,
                                    &alarm_store,
                                    &todo_store,
                                    &inbox_store,
                                    clock.as_ref(),
                                );
                                board.display.refresh_partial_best_effort(CLOCK_RECT);
                                log::info!("Clock region refreshed after NTP sync");
                            }
                            Err(err) => {
                                log::warn!("PCF8563 read_time after NTP sync failed: {err}")
                            }
                        },
                        Err(err) => log::warn!("NTP sync failed: {err}"),
                    }
                    wifi_mgr.disconnect();
                }
                Err(err) => log::warn!("Wi-Fi connect failed: {err}"),
            },
            Ok(None) => {
                log::info!(
                    "No Wi-Fi credentials in NVS; skipping connect (see scripts/gen-nvs-wifi.py)"
                );
            }
            Err(err) => log::warn!("Could not read Wi-Fi credentials from NVS: {err}"),
        }
    };

    // Owned here (not inside `DeviceContext`) because its lifetime differs
    // from the rest of the bundled state: populated only while the BLE
    // pairing screen is open, torn down on leaving it. `DeviceContext` holds
    // a `&mut` to this slot rather than the `BleControl` itself, so every
    // blocking screen that goes through it (reminders, alarm ring) can still
    // reach BLE to reply `busy` to a queued command - see `ctx.rs`'s doc
    // comment and item 3 in `docs/remaining-work.md`.
    let mut ble_control: Option<ble_control::BleControl> = None;

    // Bundle the long-lived state into one context, then run the main loop
    // through it instead of threading board/stores/wifi individually.
    let mut ctx = DeviceContext {
        board: &mut board,
        counters: &counters,
        wifi_mgr: &mut wifi_mgr,
        alarm_store: &alarm_store,
        todo_store: &todo_store,
        inbox_store: &inbox_store,
        usb_console: &mut usb_console,
        ble_control: &mut ble_control,
        sync_scheduler: SyncScheduler::new(clock.as_ref(), &counters),
        alarm_scheduler: AlarmScheduler::new(clock.as_ref(), alarm_fired_at_boot),
        last_command: None,
    };

    let mut status_tick = 0u32;
    let mut clock_tick = 0u32;
    // Cron-style wall-clock alignment: sync decisions fire when the
    // boundary index *advances*, not when a boot-relative timer elapses -
    // so "every 30s" means at :00/:30 of each minute and "every 1h" means
    // at the top of the hour. Initialized to the boot-time boundary so the
    // first aligned boundary after boot fires (and a never-synced device
    // syncs on its first urgent-poll boundary).
    loop {
        watchdog::feed();
        let mut dirty: Vec<Rect> = Vec::new();

        status_tick += 1;
        if status_tick >= STATUS_REPORT_INTERVAL_POLLS {
            status_tick = 0;
            if let Err(err) = report_power_state(ctx.board) {
                log::warn!("Power status probe failed: {err}");
            }
        }

        clock_tick += 1;
        if clock_tick >= CLOCK_POLL_INTERVAL_POLLS {
            clock_tick = 0;
            match ctx.board.rtc.read_time() {
                Ok(dt) => {
                    let changed = clock
                        .as_ref()
                        .map(|prev| {
                            prev.minute != dt.minute
                                || prev.hour != dt.hour
                                || prev.day != dt.day
                                || prev.month != dt.month
                                || prev.year != dt.year
                        })
                        .unwrap_or(true);
                    if changed {
                        clock = Some(dt);
                        dirty.push(CLOCK_RECT);
                    }
                    // Cron-style sync decisions ride the fresh clock read
                    // (every 1.2 s): boundaries only fire once each, aligned
                    // to wall clock. Both checks are cheap NVS reads unless a
                    // boundary actually advanced, so this never adds Wi-Fi
                    // traffic beyond the intended cadence. The due-todo
                    // reminder shares the urgent-poll cadence (once per 30 s
                    // boundary, gated once/day anyway). Ordinary menu loops
                    // service this same scheduler through DeviceContext.
                    if ctx.poll_runtime(&dt) {
                        dirty.push(FULL_SCREEN_RECT);
                    }
                }
                Err(err) => log::warn!("PCF8563 read_time failed: {err}"),
            }
        }

        // Poll USB console for incoming commands, dispatch them, and send replies.
        if ctx.poll_usb_control(clock.as_ref()) {
            dirty.push(FULL_SCREEN_RECT);
        }

        // Poll BLE for incoming commands (if BLE is active), dispatch them, and send replies.
        // `poll_command` returns an owned `(id, Command)`, ending the borrow
        // of `ctx.ble_control` before `dispatch` needs `&mut ctx` - the same
        // shape `ble_pairing_screen` uses.
        if let Some((id, cmd)) = ctx.ble_control.as_ref().and_then(|ble| ble.poll_command()) {
            let needs_full_redraw = matches!(cmd, control::Command::SyncNow);
            let reply = control::dispatch(&mut ctx, id.as_deref(), cmd, clock.as_ref());
            if needs_full_redraw && matches!(reply, control::Reply::Ok) {
                dirty.push(FULL_SCREEN_RECT);
            }
            if let Some(ble) = ctx.ble_control.as_ref() {
                ble.write_reply(&reply, id.as_deref());
            }
        }

        if let Some(event) = ctx.board.key_enter.poll() {
            match event {
                ButtonEvent::Pressed => {
                    // Home has no primary action. Settings is reached only
                    // through the long-UP/DOWN navigation drawer.
                }
                ButtonEvent::LongPressed => {
                    // Home is the root screen, so "back" stays on Home.
                    log::info!("ENTER long pressed on Home; already at root");
                }
                ButtonEvent::Released => {}
            }
        }
        if let Some(event) = ctx.board.key_up.poll() {
            match event {
                ButtonEvent::Pressed => {
                    // Home has no vertical selection.
                }
                ButtonEvent::LongPressed => {
                    log::info!("UP long pressed; opening navigation");
                    screens::open_navigation(&mut ctx, clock.as_ref());
                    dirty.push(FULL_SCREEN_RECT);
                }
                ButtonEvent::Released => {}
            }
        }

        if let Some(event) = ctx.board.key_down.poll() {
            match event {
                ButtonEvent::Pressed => {
                    // Home has no vertical selection.
                }
                ButtonEvent::LongPressed => {
                    log::info!("DOWN long pressed; opening navigation");
                    screens::open_navigation(&mut ctx, clock.as_ref());
                    dirty.push(FULL_SCREEN_RECT);
                }
                ButtonEvent::Released => {}
            }
        }

        if !dirty.is_empty() {
            render_home_now(
                ctx.board,
                ctx.counters,
                ctx.alarm_store,
                ctx.todo_store,
                ctx.inbox_store,
                clock.as_ref(),
            );
            if dirty
                .iter()
                .any(|rect| rect.width == 400 && rect.height == 300)
            {
                ctx.board
                    .display
                    .refresh_partial_best_effort(FULL_SCREEN_RECT);
            } else {
                for rect in &dirty {
                    ctx.board.display.refresh_partial_best_effort(*rect);
                }
            }
            log::info!("Partial display refresh completed");
        }

        thread::sleep(Duration::from_millis(POLL_INTERVAL_MS as u64));
    }
}

/// Renders the idle/background screen with a freshly-loaded next-alarm
/// label and pending-todo count. Called after every edit made in
/// `screens::open_menu` (via the caller re-rendering on return) and on
/// every clock tick, so these two NVS-backed reads happen fairly often;
/// both stores are tiny JSON blobs, cheap next to the EPD refresh itself.
fn render_home_now(
    board: &mut Note4Board,
    counters: &PersistedCounters,
    alarm_store: &AlarmStore,
    todo_store: &TodoStore,
    inbox_store: &InboxStore,
    clock: Option<&DateTime>,
) {
    let next_alarm = clock.and_then(|dt| screens::next_alarm_label(alarm_store, dt));
    let todo_summary = screens::todo_summary(todo_store, clock);
    let unread_inbox = inbox_store.unread_count().unwrap_or(0);
    let wifi_configured = counters
        .wifi_creds()
        .map(|creds| creds.is_some())
        .unwrap_or(false);
    let battery_percent = board
        .battery_millivolts()
        .ok()
        .map(board::battery_percent_from_mv);
    let charge = board.charge_snapshot();
    board.display.render_home(
        clock,
        next_alarm.as_ref().map(|label| label.time.as_str()),
        next_alarm.as_ref().and_then(|label| label.date.as_deref()),
        next_alarm.as_ref().map(|label| label.days_left),
        todo_summary.pending,
        todo_summary.due_today,
        unread_inbox,
        wifi_configured,
        battery_percent,
        charge,
    );
}

fn report_power_state(board: &mut Note4Board) -> Result<()> {
    let charge = board.charging_state();
    if let Err(err) = board.update_charging_led(charge) {
        log::warn!("Charging LED update failed: {err}");
    }
    match board.battery_millivolts() {
        Ok(vbat_mv) => log::info!(
            "Power state: power_present={} charging={} full={} vbat_mV={} ({}%)",
            charge.power_present,
            charge.charging,
            charge.full,
            vbat_mv,
            board::battery_percent_from_mv(vbat_mv)
        ),
        Err(err) => {
            log::warn!("Battery ADC read failed: {err}");
            log::info!(
                "Power state: power_present={} charging={} full={} vbat_mV=<n/a>",
                charge.power_present,
                charge.charging,
                charge.full
            );
        }
    }
    Ok(())
}
