mod alarms;
mod app_runner;
mod audio;
mod ble_control;
mod board;
mod button;
mod canvas;
mod control;
mod ctx;
mod display;
mod epd_task;
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
mod sync_task;
mod todos;
mod ui;
mod usb_console;
mod wake;
mod watchdog;
mod wifi;

use std::thread;
use std::time::{Duration, Instant};

use alarms::AlarmStore;
use anyhow::Result;
use board::Note4Board;
use button::{ButtonEvent, POLL_INTERVAL_MS};
use canvas::Rect;
use ctx::{DeviceContext, SyncScheduler};
use epd_task::EpdCompletion;
use inbox::InboxStore;
use rtc::DateTime;
use storage::PersistedCounters;
use todos::TodoStore;

/// Idle-mode poll period: 1 s. Exceeds `CONFIG_FREERTOS_IDLE_TIME_BEFORE_SLEEP`
/// (200 ticks = 200 ms at FREERTOS_HZ=1000), so each idle sleep is ~0.8 s of
/// real light sleep per second.
const IDLE_POLL_INTERVAL_MS: u64 = 1000;
/// Quiet time before Home drops from the 20 ms cadence to the idle cadence.
const IDLE_ENTER_AFTER: Duration = Duration::from_millis(2000);
/// Active-mode PCF8563 re-read period (was 60 × 20 ms).
const CLOCK_POLL_INTERVAL: Duration = Duration::from_millis(1200);
/// Idle-mode PCF8563 re-read period. The visible clock shows HH:MM only
/// (home.rs), so a 10 s staleness window costs nothing on screen; minute
/// changes still trigger the clock-region refresh, up to 10 s late. Sync
/// scheduling, alarm re-arming, and due-todo reminders ride the same read -
/// 10 s boundary-detection granularity is fine for the :00/:30-aligned
/// scheduler.
const IDLE_CLOCK_POLL_INTERVAL: Duration = Duration::from_secs(10);
/// Quiet time before the deep-sleep tier: after this
/// long without *user* activity (clock-region refreshes do not count),
/// the device enters deep sleep instead of idling in light sleep.
const DEEP_SLEEP_AFTER: Duration = Duration::from_secs(5 * 60);
/// Fallback maintenance wake when no future alarm needs a month-boundary
/// wake: 10 minutes. Each wake boots, re-renders and refreshes just the
/// clock region, and realigns the sync scheduler, so the on-screen
/// time stays within ~10 min of reality and syncs stay alive through deep
/// sleep. Cost: ~1.6 s active per 10 min boot -> ~0.13 mA average, still
/// µA-class idle. (The old 1 h fallback left the frozen e-paper clock up
/// to an hour stale - the official firmware wakes far more often.)
const MAINTENANCE_WAKE_FALLBACK: Duration = Duration::from_secs(10 * 60);

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
    // True when this boot is a deep-sleep wake (any non-undefined wake
    // cause), as opposed to a power-on reset / fresh flash. Used by the
    // deep-sleep wake path to skip the full-screen refresh on wake.
    let woke_from_deep_sleep = power::log_wakeup_cause();
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
    // The sync task gets its own clone and opens its own store handles on
    // the same partition (`EspNvs` is Send but not Sync - see sync_task.rs).
    let sync_partition = nvs_partition.clone();
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
                    // The clock just jumped to an approximate build-time
                    // value, not a confirmed-correct one - drop any stale
                    // "last aligned" marker from before this reseed so
                    // `sync::maybe_align_rtc` doesn't see a marker that's
                    // now later than the reseeded clock and (before this
                    // fix, permanently) skip every future NTP alignment
                    // attempt. See its doc comment for the hardware case
                    // this was found from.
                    if let Err(err) = counters.clear_rtc_align_epoch() {
                        log::warn!("Failed to clear stale RTC alignment marker: {err}");
                    }
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
    // AppRunner is the sole alarm owner from step 2 onwards. The legacy
    // `alarms::handle_fired_alarm` boot ring is gone: AppRunner now
    // dispatches the BootSnapshot (with the raw AF/AIE facts captured
    // after `Note4Board::take` which no longer pre-clears them), the
    // state machine decides whether to ring, and the main loop drives
    // `alarms::ring_screen` if the state machine entered Firing. The
    // wake-cause flag is captured for the legacy AlarmScheduler and the
    // deep-sleep wake path; the actual ringing comes from the state
    // machine, not the legacy ring_until_dismissed call.
    let alarm_fired_at_boot = power::wake_cause() == power::WakeCause::RtcAlarm;

    // Keep the PCF8563's single hardware alarm slot pointed at whichever
    // stored alarm is nearest, every boot: after arming/editing an alarm,
    // The RTC alarm register is *not* reprogrammed here: doing so would
    // touch AF/AIE before AppRunner gets to see them and break the
    // state-machine-only alarm ownership. AppRunner's first dispatch
    // (Event::Boot) loads the alarm list from NVS, decides whether to
    // arm the RTC and emit ProgramRtcAlarm / DisableRtcAlarm. Once that
    // batch runs the RTC register matches the stored list.

    // Home's data fingerprint at the last actual render; the sync
    // completion path compares against it to skip unchanged redraws.
    let mut last_home_fp = Some(render_home_now(
        &mut board,
        &counters,
        &alarm_store,
        &todo_store,
        &inbox_store,
        clock.as_ref(),
    ));
    // On a deep-sleep wake the e-paper already shows the
    // pre-sleep frame, so skip the full refresh (the slowest part of boot)
    // and update only the clock region. Alarm-wakes already repainted the
    // ring screen with their own full refresh, so they keep the full path.
    if woke_from_deep_sleep && !alarm_fired_at_boot {
        board.display.refresh_partial_best_effort(CLOCK_RECT);
        log::info!("Deep-sleep wake: clock region refreshed");
    } else {
        board.display.refresh_full()?;
        log::info!("Initial display refresh queued to EPD task");
    }

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
                                last_home_fp = Some(render_home_now(
                                    &mut board,
                                    &counters,
                                    &alarm_store,
                                    &todo_store,
                                    &inbox_store,
                                    clock.as_ref(),
                                ));
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

    // Arm automatic light sleep now that the board is up and the
    // boot-time Wi-Fi/NTP sync is done (Wi-Fi disconnected): with
    // CONFIG_PM_ENABLE + tickless idle, the idle task enters light sleep
    // once the main loop drops to its idle cadence (see the loop below).
    // Sleep only happens when both cores are idle, so boot-time Wi-Fi work
    // was never at risk regardless of where this call lands. A config
    // failure must not boot-loop the device: degrade to always-awake
    // polling rather than aborting.
    if let Err(err) = power::configure_light_sleep() {
        log::warn!("Light sleep not armed (device stays fully awake): {err}");
    }

    // Move the process's one WifiManager into the dedicated sync task:
    // every Wi-Fi operation from here on - scheduled
    // syncs, the Sync Now menu, USB/BLE SetWifi/SyncNow - runs off the
    // main loop, so a slow HTTPS round-trip never blocks buttons, USB, or
    // the display. The main loop talks to it through the command/reply
    // channels in `DeviceContext`.
    let sync_task = sync_task::SyncTask::spawn(sync_partition, wifi_mgr)?;

    // Owned here (not inside `DeviceContext`) because its lifetime differs
    // from the rest of the bundled state: populated only while the BLE
    // pairing screen is open, torn down on leaving it. `DeviceContext` holds
    // a `&mut` to this slot rather than the `BleControl` itself, so every
    // blocking screen that goes through it (reminders, alarm ring) can still
    // reach BLE to reply `busy` to a queued command - see `ctx.rs`'s doc
    // comment.
    let mut ble_control: Option<ble_control::BleControl> = None;

    // AppRunner shared handle: the state machine is the sole alarm
    // business owner. Wrapped in Rc<RefCell<>> so blocking pages (which
    // own the main thread and run via DeviceContext::poll_background)
    // can dispatch RtcAlarmSnapshotReady through the same instance.
    let app_runner = std::rc::Rc::new(std::cell::RefCell::new(app_runner::AppRunner::new()));

    // Bundle the long-lived state into one context, then run the main loop
    // through it instead of threading board/stores/wifi individually.
    let mut ctx = DeviceContext {
        board: &mut board,
        counters: &counters,
        sync: &sync_task,
        alarm_store: &alarm_store,
        todo_store: &todo_store,
        inbox_store: &inbox_store,
        usb_console: &mut usb_console,
        ble_control: &mut ble_control,
        sync_scheduler: SyncScheduler::new(clock.as_ref(), &counters),
        pending_wifi_op: None,
        last_command: None,
        app_runner: app_runner.clone(),
        app_runner_alarm_exit: false,
        pending_renders: std::rc::Rc::new(std::cell::RefCell::new(Vec::new())),
        // Mirrors the `app_runner_enabled` local below; kept in lockstep
        // on boot (a core_failed snapshot sets both false) so blocking
        // page entry points cannot dispatch to a default AppState.
        app_runner_enabled: true,
    };

    // AppRunner: bridge between the host-testable state machine in
    // inkwash_logic::app and the existing drivers. The boot snapshot is
    // dispatched as the first event in the loop (boot_dispatched flag);
    // the runner is purely additive for step 2, observing the boot,
    // minute ticks, and RTC AF edges without disturbing the legacy
    // DeviceContext scheduling.
    app_runner.borrow_mut().set_last_clock(clock);
    let mut boot_dispatched = false;
    let mut last_rtc_alarm_flag = false;
    // True when AppRunner may process runtime events. Set false when the
    // BootSnapshot reports a core fact failure, meaning the state machine
    // has no trustworthy alarm/NVS/config data and must NOT interpret any
    // AF / Tick event. This prevents dispatch on a default AppState that
    // would otherwise treat a real alarm as residue and ACK it off.
    let mut app_runner_enabled = true;

    // Two-level polling cadence: while the user
    // interacts - a key is raw-low, a command arrived, a refresh is
    // pending - the loop runs at POLL_INTERVAL_MS so the poll-based
    // debounce/long-press timing stays exactly as before. Once the device
    // has been quiet for IDLE_ENTER_AFTER, the loop drops to the idle
    // cadence: the 1 s sleep exceeds CONFIG_FREERTOS_IDLE_TIME_BEFORE_SLEEP
    // (200 ticks), so automatic light sleep engages for ~0.8 s of every
    // idle second. Keys wake light sleep directly (GPIO wakeup, power.rs);
    // a raw-low key flips the loop back to the 20 ms cadence so the
    // debounce keeps its 4-sample timing, and the RTC alarm line
    // (`board.rtc_int`) runs the alarm path immediately after an ext1 wake.
    //
    // Cron-style wall-clock alignment: sync decisions fire when the
    // boundary index *advances*, not when a boot-relative timer elapses -
    // so "every 30s" means at :00/:30 of each minute and "every 1h" means
    // at the top of the hour. Initialized to the boot-time boundary so the
    // first aligned boundary after boot fires (and a never-synced device
    // syncs on its first urgent-poll boundary).
    let mut idle = false;
    let mut last_activity = Instant::now();
    let mut status_last = Instant::now();
    let mut clock_last = Instant::now();
    // When the device entered the idle tier; drives the deep-sleep tier.
    // Reset by *user* activity only - clock-region refreshes keep
    // the light-sleep cadence but must not postpone deep sleep.
    let mut deep_sleep_since: Option<Instant> = None;
    // In-flight AppRunner renders, tracked in FIFO order so the next
    // EPD completion matches the next in-flight render. The state
    // machine ignores out-of-order generations so a stale completion
    // is harmless.
    // Unified in-flight render registry, shared with DeviceContext so
    // blocking pages (which dispatch RtcAlarmSnapshotReady through the
    // same AppRunner) also register their kicks here for the EPD
    // completion match.
    // (ctx.pending_renders owns it; we clone the Rc handle.)
    // Set when the boot-alarm dismiss must repaint the whole screen back
    // to Home (otherwise the panel stays on the ALARM frame - review
    // round 6 P0 #3). The main loop's dirty-rect path consumes it.
    let mut boot_full_refresh = false;
    loop {
        watchdog::feed();
        if !boot_dispatched {
            boot_dispatched = true;
            let boot_result = collect_boot_snapshot(&mut ctx, clock);
            if boot_result.core_failed {
                // Architecture says core NVS / RTC failure must enter a
                // minimal safe mode. We set app_runner_enabled = false so
                // no subsequent Tick / RtcAlarmSnapshotReady reaches
                // `update` on an uninitialized AppState - an empty alarm
                // list would otherwise treat a real AF as residue and
                // ACK it off, or make alarm decisions on corrupt facts.
                app_runner_enabled = false;
                ctx.app_runner_enabled = false;
                log::error!("Core boot fact unavailable; AppRunner disabled for this boot");
            } else {
                if let Err(err) = app_runner.borrow_mut().dispatch(
                    inkwash_logic::app::Event::Boot(boot_result.snapshot),
                    &mut ctx,
                ) {
                    log::warn!("AppRunner Boot dispatch failed: {err}");
                }
                for kick in app_runner.borrow_mut().take_pending_kicks() {
                    track_kick_shared(&ctx, kick);
                }
            }
            // Boot alarm: if the state machine entered Firing (RTC-alarm
            // wake), drive the ring screen immediately so the user can
            // dismiss the alarm without waiting for the next loop
            // iteration. Same handoff path the runtime AF branch uses:
            // AppRunner owns ACK + persist + screen selection; the main
            // loop owns the blocking ring UI; ENTER press dispatches back
            // into the state machine for Firing -> WaitingForRearm.
            if app_runner_enabled
                && matches!(
                    app_runner.borrow().state().screen,
                    inkwash_logic::app::Screen::AlarmRinging
                )
            {
                log::info!("Boot from RTC alarm; entering ring screen");
                if let Err(err) =
                    alarms::ring_screen(ctx.board, ctx.usb_console, ctx.ble_control.as_mut())
                {
                    log::error!("Boot ring_screen failed: {err}");
                }
                let _ = app_runner.borrow_mut().dispatch(
                    inkwash_logic::app::Event::Button(
                        inkwash_logic::button_event::ButtonEvent::Pressed,
                    ),
                    &mut ctx,
                );
                for kick in app_runner.borrow_mut().take_pending_kicks() {
                    track_kick_shared(&ctx, kick);
                }
                // Dismissed back to Home: force a full refresh so the
                // panel leaves the ALARM screen. The dirty-rect path
                // (which runs once `dirty` is populated this iteration)
                // picks this up.
                boot_full_refresh = true;
            }
        }
        let mut dirty: Vec<Rect> = Vec::new();
        let now = Instant::now();
        if boot_full_refresh {
            dirty.push(FULL_SCREEN_RECT);
            boot_full_refresh = false;
        }

        // Power status once per second, unchanged cadence (it gates the
        // charge-status debounce tick).
        if now.duration_since(status_last) >= Duration::from_secs(1) {
            status_last = now;
            if let Err(err) = report_power_state(ctx.board) {
                log::warn!("Power status probe failed: {err}");
            }
        }

        // PCF8563 re-read: 1.2 s active, 10 s idle (see
        // IDLE_CLOCK_POLL_INTERVAL). Minute changes drive the clock-region
        // refresh; sync scheduling, alarm re-arming, and due-todo
        // reminders ride the same read.
        let clock_interval = if idle {
            IDLE_CLOCK_POLL_INTERVAL
        } else {
            CLOCK_POLL_INTERVAL
        };
        if now.duration_since(clock_last) >= clock_interval {
            clock_last = now;
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
                        app_runner.borrow_mut().set_last_clock(clock);
                        // AppRunner Tick: feed the state machine on every
                        // minute boundary. The state machine uses Tick to
                        // rearm the RTC alarm slot at minute edges and to
                        // fire scheduled syncs. Skipped when AppRunner is
                        // disabled (core boot fact failure): the default
                        // AppState has no alarm/NVS/config data and must
                        // not rearm or interpret anything from a Tick.
                        if app_runner_enabled {
                            if let Err(err) = app_runner
                                .borrow_mut()
                                .dispatch(inkwash_logic::app::Event::Tick(dt), &mut ctx)
                            {
                                log::warn!("AppRunner Tick dispatch failed: {err}");
                            }
                            for kick in app_runner.borrow_mut().take_pending_kicks() {
                                track_kick_shared(&ctx, kick);
                            }
                        }
                        dirty.push(CLOCK_RECT);
                    }
                    // Cron-style sync decisions ride the fresh clock read:
                    // boundaries only fire once each, aligned to wall
                    // clock. Both checks are cheap NVS reads unless a
                    // boundary actually advanced, so this never adds Wi-Fi
                    // traffic beyond the intended cadence. The due-todo
                    // reminder shares the urgent-poll cadence (once per
                    // 30 s boundary, gated once/day anyway). Ordinary menu
                    // loops service this same scheduler through
                    // DeviceContext.
                    match ctx.poll_runtime(&dt) {
                        crate::ctx::BackgroundOutcome::AlarmHandled => {
                            // An alarm rang from a reminder during the
                            // minute poll: AppRunner rendered Home and the
                            // reminder already unwound. Clear the sticky
                            // exit flag so a later navigation does not see
                            // this historical alarm as its own exit reason.
                            ctx.app_runner_alarm_exit = false;
                            dirty.push(FULL_SCREEN_RECT);
                        }
                        crate::ctx::BackgroundOutcome::VisibleChanged => {
                            dirty.push(FULL_SCREEN_RECT);
                        }
                        crate::ctx::BackgroundOutcome::NoChange => {}
                    }
                }
                Err(err) => log::warn!("PCF8563 read_time failed: {err}"),
            }
        }

        // AppRunner is the sole alarm owner from step 2 onwards. The
        // low -> high AF edge drives a `RtcAlarmSnapshotReady` event
        // through the state machine; the state machine then emits
        // AcknowledgeRtcAlarm + PersistAlarms + StartTone + Render. After
        // dispatch, if the state machine entered `Firing`, the main loop
        // drives `alarms::ring_screen` for the blocking ring/dismiss UI;
        // ENTER press dispatches `Event::Button(Enter/Pressed)` back into
        // the state machine which transitions to `WaitingForRearm`.
        //
        // The legacy `poll_alarm` ringing branch is skipped entirely:
        // AppRunner owns ACK, persist, ring start, and rearm scheduling.
        if ctx.board.rtc.alarm_flag().unwrap_or(false) {
            if !last_rtc_alarm_flag {
                // Read a *consistent* (time, AF, AIE) snapshot first. The
                // edge is only consumed (last_rtc_alarm_flag = true) after
                // ALL three reads succeed and the snapshot is built: a
                // failure keeps the edge retryable so the next loop
                // iteration retries instead of silently swallowing this
                // alarm forever. AIE read errors are propagated (not
                // masked as false) - a false AIE would make the state
                // machine treat a real alarm as residue and ACK it off.
                let read_result = (|| -> anyhow::Result<(DateTime, bool)> {
                    let dt = ctx.board.rtc.read_time()?;
                    let aie = ctx.board.rtc.alarm_interrupt_enabled()?;
                    Ok((dt, aie))
                })();
                match read_result {
                    Ok((dt, aie)) => {
                        last_rtc_alarm_flag = true;
                        if !app_runner_enabled {
                            // AppRunner is disabled (core boot fact
                            // failure): do not interpret the AF. Move on;
                            // the edge stays consumed so the next loop
                            // iteration does not re-read a snapshot that
                            // would go nowhere.
                            dirty.push(FULL_SCREEN_RECT);
                            continue;
                        }
                        let snapshot = inkwash_logic::app::RtcAlarmSnapshot {
                            now: dt,
                            alarm_flag: true,
                            alarm_interrupt_enabled: aie,
                        };
                        if let Err(err) = app_runner.borrow_mut().dispatch(
                            inkwash_logic::app::Event::RtcAlarmSnapshotReady(snapshot),
                            &mut ctx,
                        ) {
                            log::warn!("AppRunner RtcAlarmSnapshotReady dispatch failed: {err}");
                        }
                        for kick in app_runner.borrow_mut().take_pending_kicks() {
                            track_kick_shared(&ctx, kick);
                        }
                        if matches!(
                            app_runner.borrow().state().screen,
                            inkwash_logic::app::Screen::AlarmRinging
                        ) {
                            log::info!("RTC alarm fired; entering ring screen");
                            if let Err(err) = alarms::ring_screen(
                                ctx.board,
                                ctx.usb_console,
                                ctx.ble_control.as_mut(),
                            ) {
                                log::error!("ring_screen failed: {err}");
                            }
                            // Dismiss press dispatches ENTER as a state
                            // machine event; Firing -> WaitingForRearm.
                            let _ = app_runner.borrow_mut().dispatch(
                                inkwash_logic::app::Event::Button(
                                    inkwash_logic::button_event::ButtonEvent::Pressed,
                                ),
                                &mut ctx,
                            );
                            for kick in app_runner.borrow_mut().take_pending_kicks() {
                                track_kick_shared(&ctx, kick);
                            }
                            dirty.push(FULL_SCREEN_RECT);
                        }
                    }
                    Err(err) => {
                        // Edge left unlocked - the next loop iteration
                        // retries the snapshot read.
                        log::warn!(
                            "RTC alarm snapshot read failed; AF edge stays retryable: {err:#}"
                        );
                    }
                }
            }
        } else {
            last_rtc_alarm_flag = false;
        }

        // Poll USB console for incoming commands, dispatch them, and send replies.
        let (usb_changed, usb_activity) = ctx.poll_usb_control(clock.as_ref());
        if usb_changed {
            dirty.push(FULL_SCREEN_RECT);
        }

        // Poll BLE for incoming commands (if BLE is active), dispatch them, and send replies.
        // `poll_command` returns an owned `(id, Command)`, ending the borrow
        // of `ctx.ble_control` before `dispatch` needs `&mut ctx` - the same
        // shape `ble_pairing_screen` uses.
        let mut ble_changed = false;
        if let Some((id, cmd)) = ctx.ble_control.as_ref().and_then(|ble| ble.poll_command()) {
            let needs_full_redraw = matches!(cmd, control::Command::SyncNow);
            let reply = control::dispatch(
                &mut ctx,
                control::Channel::Ble,
                id.as_deref(),
                cmd,
                clock.as_ref(),
            );
            if needs_full_redraw && matches!(reply, control::Reply::Ok) {
                dirty.push(FULL_SCREEN_RECT);
                ble_changed = true;
            }
            if let Some(ble) = ctx.ble_control.as_ref() {
                ble.write_reply(&reply, id.as_deref());
            }
        }

        // EPD completion events: feed each one back to the state machine
        // as `EffectCompleted(RenderDone)` / `EffectFailed(Render)`. The
        // shared `pending_renders` registry (reachable from the main loop
        // and every blocking page) pairs each `EpdCompletion` with the
        // next in-flight render. Out-of-order or extra completions are
        // absorbed by the state machine, which drops effects whose
        // render_generation is older than the current visible state.
        while let Some(completion) = ctx.board.display.poll_completion() {
            // Match by request_id (echoed by the EPD task) instead of
            // FIFO: the single EPD slot can merge or overwrite requests,
            // so only the completion whose id matches an in-flight
            // AppRunner render is fed back. Legacy dirty-rect / ring /
            // boot refreshes carry ids with no matching kick and are
            // observed but not fed back.
            let matched_kick = {
                let mut reg = ctx.pending_renders.borrow_mut();
                let idx = reg
                    .iter()
                    .position(|kick| kick.request_id == Some(completion.request_id));
                idx.map(|i| reg.remove(i))
            };
            if let Some(kick) = matched_kick {
                if matches!(kick.effect, inkwash_logic::app::Effect::Render(_)) {
                    if completion.superseded {
                        // The request was replaced before its command ran
                        // (EPD latest-wins). It must NOT be reported as
                        // RenderDone - its pixels never reached the panel.
                        // The replacement request carries its own request_id
                        // and will complete (or fail) on its own; we simply
                        // clear this in-flight entry so the registry does
                        // not leak, and let the replacement's completion be
                        // the one fed to the state machine. Once RenderPlan
                        // (step 5) tracks generations precisely, this keeps
                        // the "entered queue" vs "displayed on panel"
                        // distinction honest.
                        log::warn!(
                            "AppRunner render (op {:?}) superseded before panel display",
                            kick.operation_id
                        );
                        continue;
                    }
                    let output = inkwash_logic::app::EffectOutput::RenderDone;
                    let failure = (!completion.ok).then(|| {
                        inkwash_logic::app::EffectError::Render(format!(
                            "epd refresh failed: {:?}",
                            completion.kind
                        ))
                    });
                    if let Err(err) = {
                        let mut ar = app_runner.borrow_mut();
                        feed_completion_back(&mut ar, &mut ctx, kick, output, failure)
                    } {
                        log::warn!("AppRunner EPD completion feed failed: {err}");
                    }
                }
            }
            match completion {
                EpdCompletion {
                    ok: true,
                    superseded: true,
                    kind,
                    ..
                } => log::warn!("EPD refresh superseded: {kind:?}"),
                EpdCompletion {
                    ok: true,
                    recovered: false,
                    kind,
                    ..
                } => {
                    log::info!("EPD refresh completed: {kind:?}");
                }
                EpdCompletion {
                    ok: true,
                    recovered: true,
                    kind,
                    ..
                } => {
                    log::warn!("EPD partial refresh failed; recovered via full refresh ({kind:?})");
                }
                EpdCompletion {
                    ok: false, kind, ..
                } => {
                    log::error!("EPD refresh failed: {kind:?}");
                }
            }
        }

        // Receipts from the sync task (scheduled syncs, deferred
        // USB/BLE SyncNow/SetWifi replies): applies the RTC re-arm, NTP
        // alignment, and transport replies. A successful sync redraws the
        // full screen only when it actually changed what Home shows
        // - an unchanged sync leaves the panel alone.
        if let Some(event) = ctx.poll_wifi_ops() {
            if matches!(event, sync_task::WifiOpEvent::SyncDone(Ok(_)))
                && Some(home_data_fingerprint(
                    ctx.counters,
                    ctx.alarm_store,
                    ctx.todo_store,
                    ctx.inbox_store,
                    clock.as_ref(),
                )) != last_home_fp
            {
                dirty.push(FULL_SCREEN_RECT);
            }
        }

        // Home has no single-key action: ENTER is root (no-op), long UP or
        // DOWN opens the navigation drawer. Any event counts as activity.
        let mut key_changed = false;
        if let Some(event) = ctx.board.key_enter.poll() {
            key_changed = true;
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
            key_changed = true;
            match event {
                ButtonEvent::Pressed => {
                    // Home has no vertical selection.
                }
                ButtonEvent::LongPressed => {
                    log::info!("UP long pressed; opening navigation");
                    screens::open_navigation(&mut ctx, clock.as_ref());
                    // Always restore Home after the navigation stack: a
                    // normal cancel or selecting Home leaves the drawer /
                    // old page on screen but keys already route to Home.
                    // The alarm flag only marks WHY the stack unwound and
                    // is cleared here so it does not leak into the next
                    // page.
                    ctx.app_runner_alarm_exit = false;
                    dirty.push(FULL_SCREEN_RECT);
                }
                ButtonEvent::Released => {}
            }
        }

        if let Some(event) = ctx.board.key_down.poll() {
            key_changed = true;
            match event {
                ButtonEvent::Pressed => {
                    // Home has no vertical selection.
                }
                ButtonEvent::LongPressed => {
                    log::info!("DOWN long pressed; opening navigation");
                    screens::open_navigation(&mut ctx, clock.as_ref());
                    ctx.app_runner_alarm_exit = false;
                    dirty.push(FULL_SCREEN_RECT);
                }
                ButtonEvent::Released => {}
            }
        }

        if !dirty.is_empty() {
            last_home_fp = Some(render_home_now(
                ctx.board,
                ctx.counters,
                ctx.alarm_store,
                ctx.todo_store,
                ctx.inbox_store,
                clock.as_ref(),
            ));
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
            log::info!("Partial display refresh queued to EPD task");
        }

        // Idle/active selection: input, a dispatched command, or a display
        // refresh counts as activity; a raw-low key forces the 20 ms
        // cadence so the polled debounce keeps its 4-sample timing after a
        // GPIO wake (the wake resumes the loop mid-idle-sleep).
        let any_key_pressed = ctx.board.key_enter.is_raw_pressed()
            || ctx.board.key_up.is_raw_pressed()
            || ctx.board.key_down.is_raw_pressed();
        // Any USB frame counts as user activity (idle is "no button, no
        // USB frame", not just visible
        // changes): GetStatus polls from the desktop tool must not let the
        // device drift into deep sleep mid-session.
        let user_activity = usb_activity || ble_changed || key_changed || any_key_pressed;
        let interacted = user_activity || !dirty.is_empty();
        if interacted || any_key_pressed {
            last_activity = now;
            idle = false;
            if user_activity {
                deep_sleep_since = None;
            }
        } else if now.duration_since(last_activity) >= IDLE_ENTER_AFTER {
            idle = true;
            deep_sleep_since.get_or_insert(now);
        }

        // Deep-sleep tier: after DEEP_SLEEP_AFTER of
        // idle light sleep with no user activity and no Wi-Fi op in
        // flight, drop to deep sleep. Wake sources: ENTER (GPIO0), DOWN
        // (GPIO18), the RTC alarm line (GPIO5), and the maintenance timer.
        // UP is not an RTC GPIO and cannot wake deep sleep - documented
        // in development-guide.md.
        if idle
            && ctx.pending_wifi_op.is_none()
            && deep_sleep_since.is_some_and(|since| now.duration_since(since) >= DEEP_SLEEP_AFTER)
        {
            // The wake interval is the *minimum* of the
            // next month-boundary alarm maintenance wake and the 10-minute
            // fallback - a far-future one-shot alarm must not stretch the
            // sleep so long that the on-screen time goes stale.
            let maintenance = clock
                .as_ref()
                .and_then(|dt| {
                    ctx.alarm_store
                        .load()
                        .ok()
                        .and_then(|alarms| alarms::maintenance_wakeup_delay(&alarms, dt))
                })
                .map(|delay| delay.min(MAINTENANCE_WAKE_FALLBACK))
                .or(Some(MAINTENANCE_WAKE_FALLBACK));
            log::info!(
                "Idle past deep-sleep threshold; entering deep sleep (maintenance={:?})",
                maintenance
            );
            crate::power::enter_deep_sleep_with_wakeups(maintenance);
        }

        if idle {
            // One-shot wake interrupts armed for the light-sleep window: a
            // key press or an asserted RTC alarm line returns the wait
            // immediately, so the loop polls the buttons / probes the
            // alarm line within milliseconds of the event (see `wake.rs`
            // for why a plain sleep cannot - the GPIO wakeup resumes the
            // CPU but not the blocked task).
            ctx.board.wake.arm();
            if ctx.board.wake.wait(IDLE_POLL_INTERVAL_MS as u32) {
                log::info!("Idle wait woken early (key pressed)");
            }
        } else {
            thread::sleep(Duration::from_millis(POLL_INTERVAL_MS as u64));
        }
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
) -> u64 {
    let next_alarm = clock.and_then(|dt| screens::next_alarm_label(alarm_store, dt));
    let todo_summary = screens::todo_summary(todo_store, clock);
    let unread_inbox = inbox_store.unread_count().unwrap_or(0);
    let wifi_configured = counters
        .wifi_creds()
        .map(|creds| creds.is_some())
        .unwrap_or(false);
    let battery_percent = board.battery_percent();
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
    home_data_fingerprint(counters, alarm_store, todo_store, inbox_store, clock)
}

/// Fingerprint of Home's data-backed content: the alarm list, todo list,
/// unread-inbox count, Wi-Fi-configured flag, and the calendar date. A
/// sync (or any remote state change) that leaves all of these identical
/// produces no visible difference on Home, so the full-screen redraw it
/// would trigger is skipped. Hour/minute are deliberately
/// excluded - clock drift is the clock-region refresh's job, and including
/// the minute would make every sync look "changed" (a sync takes longer
/// than a minute boundary).
fn home_data_fingerprint(
    counters: &PersistedCounters,
    alarm_store: &AlarmStore,
    todo_store: &TodoStore,
    inbox_store: &InboxStore,
    clock: Option<&DateTime>,
) -> u64 {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut hasher = DefaultHasher::new();
    if let Some(dt) = clock {
        (dt.year, dt.month, dt.day).hash(&mut hasher);
    }
    counters
        .wifi_creds()
        .map(|creds| creds.is_some())
        .unwrap_or(false)
        .hash(&mut hasher);
    if let Ok(alarms) = alarm_store.load() {
        serde_json::to_vec(&alarms)
            .unwrap_or_default()
            .hash(&mut hasher);
    }
    if let Ok(todos) = todo_store.load() {
        serde_json::to_vec(&todos)
            .unwrap_or_default()
            .hash(&mut hasher);
    }
    inbox_store.unread_count().unwrap_or(0).hash(&mut hasher);
    hasher.finish()
}

fn report_power_state(board: &mut Note4Board) -> Result<()> {
    let charge = board.charging_state();
    if let Err(err) = board.update_charging_led(charge) {
        log::warn!("Charging LED update failed: {err}");
    }
    match board.battery_millivolts() {
        Ok(vbat_mv) => log::info!(
            "Power state: power_present={} charging={} full={} fault={} no_battery={} vbat_mV={} ({}%)",
            charge.power_present,
            charge.charging,
            charge.full,
            charge.fault,
            charge.no_battery,
            vbat_mv,
            board::battery_percent_from_mv(vbat_mv)
        ),
        Err(err) => {
            log::warn!("Battery ADC read failed: {err}");
            log::info!(
                "Power state: power_present={} charging={} full={} fault={} no_battery={} vbat_mV=<n/a>",
                charge.power_present,
                charge.charging,
                charge.full,
                charge.fault,
                charge.no_battery
            );
        }
    }
    Ok(())
}

/// Build the BootSnapshot the state machine consumes in Event::Boot. Fact
/// gathering is defensive: every read is allowed to fail, with the
/// Result of a boot snapshot collection. Core failures (RTC reads, alarm
/// NVS load) are signalled via `core_failed`; the snapshot itself still
/// carries the partial facts we did manage to read so a future safe-mode
/// renderer can show a diagnostic screen. Soft failures (todo / inbox /
/// config) are logged and substituted with empty defaults.
#[derive(Debug)]
struct BootSnapshotResult {
    snapshot: inkwash_logic::app::BootSnapshot,
    /// True when a *core* fact (RTC AF/AIE, alarm list) could not be
    /// read - the firmware should enter safe mode rather than trust this
    /// snapshot for alarm logic.
    core_failed: bool,
}

fn collect_boot_snapshot(
    ctx: &mut DeviceContext<'_>,
    clock: Option<DateTime>,
) -> BootSnapshotResult {
    let mut core_failed = false;
    let rtc_alarm_flag = match ctx.board.rtc.alarm_flag() {
        Ok(flag) => flag,
        Err(err) => {
            log::error!("PCF8563 alarm_flag read failed: {err}");
            core_failed = true;
            false
        }
    };
    let rtc_alarm_interrupt_enabled = match ctx.board.rtc.alarm_interrupt_enabled() {
        Ok(flag) => flag,
        Err(err) => {
            log::error!("PCF8563 alarm_interrupt_enabled read failed: {err}");
            core_failed = true;
            false
        }
    };
    let alarms = match ctx.alarm_store.load() {
        Ok(list) => list,
        Err(err) => {
            log::error!("Alarm NVS load failed: {err}");
            core_failed = true;
            Vec::new()
        }
    };
    let todos = ctx.todo_store.load().unwrap_or_else(|err| {
        log::warn!("Todo NVS load failed (using empty list): {err}");
        Vec::new()
    });
    let inbox = ctx.inbox_store.load().unwrap_or_else(|err| {
        log::warn!("Inbox NVS load failed (using empty list): {err}");
        Vec::new()
    });
    let config = ctx
        .counters
        .device_config()
        .ok()
        .flatten()
        .unwrap_or_else(|| inkwash_logic::device_config::DeviceConfig {
            server_url: String::new(),
            auth_token: String::new(),
        });
    let wake_cause = power::wake_cause();
    BootSnapshotResult {
        snapshot: inkwash_logic::app::BootSnapshot {
            wake_cause,
            now: clock,
            rtc_alarm_flag,
            rtc_alarm_interrupt_enabled,
            alarms,
            todos,
            inbox,
            config,
        },
        core_failed,
    }
}

/// Records one in-flight render into the shared `pending_renders`
/// registry so the matching `EpdCompletion` (by request_id) can be fed
/// back to the state machine, regardless of which entry point (main
/// loop or a blocking page) dispatched it. Non-render async kicks are
/// surfaced with a log and dropped - their transports aren't wired yet.
fn track_kick_shared(ctx: &DeviceContext<'_>, kick: app_runner::AsyncKick) {
    let mut reg = ctx.pending_renders.borrow_mut();
    match &kick.effect {
        inkwash_logic::app::Effect::Render(_) => reg.push(kick),
        inkwash_logic::app::Effect::StartSync(_) => {
            log::warn!(
                "AppRunner StartSync kick not yet wired to sync task (op {:?}); dropped",
                kick.operation_id
            );
        }
        inkwash_logic::app::Effect::StartBlePairing(_)
        | inkwash_logic::app::Effect::StopBlePairing => {
            log::warn!(
                "AppRunner BLE pairing kick not yet wired (op {:?}); dropped",
                kick.operation_id
            );
        }
        other => log::warn!(
            "AppRunner async kick {:?} (op {:?}) not handled; dropped",
            other,
            kick.operation_id
        ),
    }
}

/// Translate one EPD completion into the matching `EffectCompleted` /
/// `EffectFailed` and dispatch it back through AppRunner. The kick is
/// consumed (removed from `pending_renders` by the caller before this is
/// called) so the generation / op id line up.
fn feed_completion_back(
    runner: &mut app_runner::AppRunner,
    ctx: &mut DeviceContext<'_>,
    kick: app_runner::AsyncKick,
    output: inkwash_logic::app::EffectOutput,
    failure: Option<inkwash_logic::app::EffectError>,
) -> anyhow::Result<()> {
    runner.feed_render_completion(kick, output, failure, ctx)
}
