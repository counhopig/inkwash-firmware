mod alarms;
mod app_runner;
mod audio;
mod audio_task;
mod ble_control;
mod board;
mod button;
mod canvas;
mod control;
mod ctx;
mod display;
mod effect_task;
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
mod rtc_executor;
mod screens;
mod storage;
mod sync;
mod sync_task;
mod tasks;
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
use button::POLL_INTERVAL_MS;
use ctx::DeviceContext;
use epd_task::EpdCompletion;
use inbox::InboxStore;
use rtc::DateTime;
use storage::PersistedCounters;
use todos::TodoStore;
use usb_console::UsbReplyWriter;

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
const SAFE_MODE_REPLY_CAPACITY: usize = usb_console::REPLY_WRITER_CAPACITY;

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
    // Start USB diagnostics before core RTC/NVS probes so failures can enter
    // the existing safe-mode loop.
    let mut usb_console = usb_console::UsbConsole::start();
    let mut usb_reply_writer = UsbReplyWriter::start()?;
    // The RTC executor owns the sole Pcf8563 driver (see rtc_executor.rs).
    // Spawned right after board bring-up from the shared I2C bus; every RTC
    // read/write below goes through this client handle, never `board.rtc`
    // (which no longer exists).
    let rtc = match rtc_executor::RtcExecutor::spawn(board.i2c_bus.clone()) {
        Ok(rtc) => rtc,
        Err(err) => run_safe_mode(
            &mut board,
            &mut usb_console,
            &mut usb_reply_writer,
            &format!("RTC executor start failed: {err}"),
        ),
    };

    // Taken once and cloned into each store: `EspDefaultNvsPartition::take()`
    // is a true singleton (a global taken-flag, not a ref-counted "take a
    // new handle" call) and errors with `ESP_ERR_INVALID_STATE` if called
    // again while an earlier handle is still alive - three independent
    // `open()`s each calling `take()` themselves made every boot fail here
    // once `alarms.rs`/`todos.rs` were added, since `counters`'s handle was
    // still alive when `AlarmStore::open()` tried to take its own.
    let nvs_partition = match esp_idf_svc::nvs::EspDefaultNvsPartition::take() {
        Ok(partition) => partition,
        Err(err) => run_safe_mode(
            &mut board,
            &mut usb_console,
            &mut usb_reply_writer,
            &format!("NVS partition init failed: {err}"),
        ),
    };
    let counters = match PersistedCounters::open(nvs_partition.clone()) {
        Ok(counters) => counters,
        Err(err) => run_safe_mode(
            &mut board,
            &mut usb_console,
            &mut usb_reply_writer,
            &format!("Counters NVS open failed: {err}"),
        ),
    };
    let alarm_store = match AlarmStore::open(nvs_partition.clone()) {
        Ok(store) => store,
        Err(err) => run_safe_mode(
            &mut board,
            &mut usb_console,
            &mut usb_reply_writer,
            &format!("Alarm NVS open failed: {err}"),
        ),
    };
    let todo_store = match TodoStore::open(nvs_partition.clone()) {
        Ok(store) => store,
        Err(err) => run_safe_mode(
            &mut board,
            &mut usb_console,
            &mut usb_reply_writer,
            &format!("Todo NVS open failed: {err}"),
        ),
    };
    // The sync task gets its own clone and opens its own store handles on
    // the same partition (`EspNvs` is Send but not Sync - see sync_task.rs).
    let sync_partition = nvs_partition.clone();
    let effect_partition = nvs_partition.clone();
    let inbox_store = match InboxStore::open(nvs_partition) {
        Ok(store) => store,
        Err(err) => run_safe_mode(
            &mut board,
            &mut usb_console,
            &mut usb_reply_writer,
            &format!("Inbox NVS open failed: {err}"),
        ),
    };
    let effect_counters = match PersistedCounters::open(effect_partition.clone()) {
        Ok(store) => store,
        Err(err) => run_safe_mode(
            &mut board,
            &mut usb_console,
            &mut usb_reply_writer,
            &format!("Effect counters NVS open failed: {err}"),
        ),
    };
    let effect_alarm_store = match AlarmStore::open(effect_partition.clone()) {
        Ok(store) => store,
        Err(err) => run_safe_mode(
            &mut board,
            &mut usb_console,
            &mut usb_reply_writer,
            &format!("Effect alarm NVS open failed: {err}"),
        ),
    };
    let effect_todo_store = match TodoStore::open(effect_partition.clone()) {
        Ok(store) => store,
        Err(err) => run_safe_mode(
            &mut board,
            &mut usb_console,
            &mut usb_reply_writer,
            &format!("Effect todo NVS open failed: {err}"),
        ),
    };
    let effect_inbox_store = match InboxStore::open(effect_partition) {
        Ok(store) => store,
        Err(err) => run_safe_mode(
            &mut board,
            &mut usb_console,
            &mut usb_reply_writer,
            &format!("Effect inbox NVS open failed: {err}"),
        ),
    };

    // Wi-Fi/NTP resync is needed only when the battery-backed RTC cannot be
    // trusted. A firmware flash or ordinary reset does not erase PCF8563
    // time, so connecting on every reset merely consumes the one safe Wi-Fi
    // session and forces the first user-triggered Sync Now to reboot.
    let mut needs_wifi_sync = false;
    let mut core_failure: Option<String> = None;
    let mut clock = match rtc.read_time() {
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
                if let Err(err) = rtc.write_time(&seeded) {
                    log::warn!("PCF8563 reseed failed: {err}");
                    core_failure = Some(format!("PCF8563 reseed failed: {err}"));
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
            core_failure = Some(format!("PCF8563 time read failed: {err}"));
            None
        }
    };

    // Probe the core BootSnapshot facts before any network, BLE, or effect
    // task exists. A failure enters safe mode while only the diagnostic
    // resources are alive; the successful facts are reused below so the
    // normal boot path never performs a second core read after task startup.
    let boot_core = match collect_boot_core_facts(&rtc, &alarm_store, clock, core_failure) {
        Ok(facts) => facts,
        Err(reason) => {
            run_safe_mode(&mut board, &mut usb_console, &mut usb_reply_writer, &reason);
        }
    };
    // Injectable test hook (see build.rs): a binary built with
    // INKWASH_FORCE_SAFE_MODE=1 forces the safe-mode entry on a healthy boot
    // without destroying NVS/RTC. The normal build never sets it.
    if option_env!("INKWASH_FORCE_SAFE_MODE").is_some_and(|v| v == "1") {
        run_safe_mode(
            &mut board,
            &mut usb_console,
            &mut usb_reply_writer,
            "forced by INKWASH_FORCE_SAFE_MODE=1 (test hook)",
        );
    }

    // Ring before the normal boot render, if this boot is the RTC alarm
    // firing: latency to sound matters more than latency to the home
    // screen. `power::wake_cause()` reads `esp_sleep_get_wakeup_cause`
    // again (harmless, not a consuming read); unlike the raw cause logged
    // above, this distinguishes ENTER-wake from alarm-wake.
    // AppRunner is the sole alarm owner from step 2 onwards. The previous
    // `alarms::handle_fired_alarm` boot ring and `ring_screen` are gone:
    // AppRunner dispatches the BootSnapshot (with the raw AF/AIE facts
    // captured after `Note4Board::take`, which no longer pre-clears them),
    // the state machine decides whether to ring (Screen::AlarmRinging), and
    // the executor renders the ring frame + starts the tone through the
    // audio task. The wake-cause flag is captured for the deep-sleep wake
    // path; the ringing comes entirely from the state machine.
    // Keep the PCF8563's single hardware alarm slot pointed at whichever
    // stored alarm is nearest, every boot: after arming/editing an alarm,
    // The RTC alarm register is *not* reprogrammed here: doing so would
    // touch AF/AIE before AppRunner gets to see them and break the
    // state-machine-only alarm ownership. AppRunner's first dispatch
    // (Event::Boot) consumes the alarm list collected in the boot snapshot,
    // decides whether to arm the RTC and emits ProgramRtcAlarm /
    // DisableRtcAlarm. Once that batch runs the RTC register matches the
    // stored list.

    if board.nfc.is_none() {
        log::warn!("NFC not available");
    }

    // The audio task owns the process's one ES8311 codec; every tone (alarm
    // ring, reminders) goes through its command channel so the main loop
    // never blocks on a tone. A missing codec degrades to silence; the
    // state machine's StartTone effect reports the failure cleanly.
    let audio_handle = match audio_task::AudioTask::spawn(board.take_audio())? {
        audio_task::AudioSpawn::Running(handle) => {
            log::info!("Audio task spawned");
            Some(handle)
        }
        audio_task::AudioSpawn::Unavailable => {
            log::warn!("ES8311 not available; tones disabled");
            None
        }
    };

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
                    match wifi::ntp_sync_and_set_rtc(&rtc, timezone_offset) {
                        Ok(()) => match rtc.read_time() {
                            Ok(dt) => clock = Some(dt),
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

    // All BootSnapshot facts are collected before runtime tasks are created.
    // The core RTC/alarm facts were probed above; this pass only gathers the
    // remaining local state and never repeats the core reads.
    let boot_result = collect_boot_snapshot(boot_core, &counters, &todo_store, &inbox_store, clock);
    let scheduler_config = inkwash_logic::app::SyncSchedulerConfig {
        now_unix: clock.map(|now| now.to_unix()).unwrap_or(0),
        interval_minutes: counters.sync_interval_minutes().unwrap_or(60),
        last_sync_epoch: counters.last_sync_epoch().unwrap_or(None),
    };

    // Move the process's one WifiManager into the dedicated sync task:
    // every Wi-Fi operation from here on - scheduled
    // syncs, the Sync Now menu, USB/BLE SetWifi/SyncNow - runs off the
    // main loop, so a slow HTTPS round-trip never blocks buttons, USB, or
    // the display. The main loop talks to it through the command/reply
    // channels in `DeviceContext`.
    let sync_task = sync_task::SyncTask::spawn(sync_partition, wifi_mgr)?;

    let effect_task = effect_task::EffectTask::spawn(effect_task::EffectDrivers {
        alarm_store: effect_alarm_store,
        todo_store: effect_todo_store,
        inbox_store: effect_inbox_store,
        counters: effect_counters,
        rtc: rtc.clone(),
    })?;

    // The BLE worker owns NimBLE and is idle until the state machine enters
    // pairing. Its handle stays in the main context so callbacks and replies
    // remain serviceable while start/stop run asynchronously off-thread.
    let mut ble_control = ble_control::BleControl::spawn()?;

    // AppRunner shared handle: the state machine is the sole alarm
    // business owner. Wrapped in Rc<RefCell<>> so collection paths (which
    // run via DeviceContext::poll_background)
    // can dispatch RtcAlarmSnapshotReady through the same instance.
    let app_runner = std::rc::Rc::new(std::cell::RefCell::new(app_runner::AppRunner::new()));

    // Bundle the long-lived state into one context, then run the main loop
    // through it instead of threading board/stores/wifi individually.
    let mut ctx = DeviceContext {
        board: &mut board,
        rtc: &rtc,
        counters: &counters,
        sync: &sync_task,
        alarm_store: &alarm_store,
        todo_store: &todo_store,
        inbox_store: &inbox_store,
        usb_console: &mut usb_console,
        usb_reply_writer,
        ble_control: &mut ble_control,
        audio_task: audio_handle.as_ref(),
        pending_wifi_op: None,
        ble_session_id: None,
        ble_connection_generation: None,
        ble_connection_handle: None,
        ble_wifi_suspended: false,
        ble_set_wifi_after_resume: None,
        ble_start_failure: None,
        ble_start_cancelled: false,
        pending_ble_replies: Vec::new(),
        pending_ble_deliveries: std::collections::VecDeque::with_capacity(
            ctx::BLE_REPLY_PENDING_CAPACITY,
        ),
        pending_ble_reply_latch: None,
        pending_ble_handoff: None,
        pending_ble_set_wifi_ack: None,
        pending_ble_pairing_success: None,
        command_sessions: inkwash_logic::command_sessions::CommandSessions::default(),
        usb_session_id: 1,
        app_runner: app_runner.clone(),
        pending_renders: std::rc::Rc::new(std::cell::RefCell::new(
            inkwash_logic::epd_registry::RenderRegistry::new(),
        )),
        pending_render_retries: std::collections::VecDeque::with_capacity(
            inkwash_logic::epd_registry::RENDER_REGISTRY_CAPACITY,
        ),
        pending_render_completion: None,
        pending_sleep_kick: None,
        prepared_wake_plan: None,
        alarm_poll: inkwash_logic::alarm_flow::AlarmPoll::new(),
        pending_alarm_status: None,
        pending_alarm_snapshot: None,
        pending_clock_read: None,
        effect_task: &effect_task,
        pending_effect_batch: None,
        pending_effect_batches: std::collections::VecDeque::new(),
        worker_batch_in_flight: false,
        pending_app_events: std::collections::VecDeque::with_capacity(
            inkwash_logic::event_queue::HIGH_CAPACITY,
        ),
        pending_dispatch_event: None,
        pending_dispatch_sources: ctx::PendingDispatchSources::default(),
        pending_effect_notices: None,
        pending_usb_replies: std::collections::VecDeque::with_capacity(
            ctx::USB_REPLY_PENDING_CAPACITY,
        ),
        pending_usb_reply_latch: None,
        // Runtime collection is enabled only after the pre-task boot facts
        // pass their core checks; keep this defensive guard in lockstep.
        app_runner_enabled: true,
    };
    // Keep USB command handling usable on boards where the connection probe
    // is conservative and reports false even though a frame arrives.
    ctx.command_sessions
        .begin(control::Channel::Usb, ctx.usb_session_id);

    // AppRunner: bridge between the host-testable state machine in
    // inkwash_logic::app and the existing drivers. The boot snapshot is
    // dispatched as the first event in the loop (boot_dispatched flag);
    // the runner receives boot, minute-tick, and RTC AF facts from the
    // collection loop and owns their business transitions.
    app_runner.borrow_mut().set_last_clock(clock);
    let mut boot_dispatched = false;
    let mut boot_result = Some(boot_result);
    let mut scheduler_config = Some(scheduler_config);
    // True when AppRunner may process runtime events. Set false when the
    // BootSnapshot reports a core fact failure, meaning the state machine
    // has no trustworthy alarm/NVS/config data and must NOT interpret any
    // AF / Tick event. This prevents dispatch on a default AppState that
    // would otherwise treat a real alarm as residue and ACK it off.
    // Always true on the surviving path: a core boot-fact failure diverges
    // into `run_safe_mode` (which never returns) before this is consulted.
    let app_runner_enabled = true;

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
    let mut light_sleep_enabled = false;
    let power_ticks_origin = Instant::now();
    let mut last_activity = Instant::now();
    let mut status_last = Instant::now();
    let mut clock_last = Instant::now();
    let mut usb_host_was_connected = false;
    // In-flight AppRunner renders, tracked in FIFO order so the next
    // EPD completion matches the next in-flight render. The state
    // machine ignores out-of-order generations so a stale completion
    // is harmless.
    // Unified in-flight render registry, shared with DeviceContext so all
    // collection paths register their kicks for the EPD completion match.
    // (ctx.pending_renders owns it; we clone the Rc handle.)
    loop {
        watchdog::feed();
        service_effect_task(&app_runner, &mut ctx)?;
        retry_render_registry(&mut ctx)?;
        ctx.service_usb_reply_writer();
        if !boot_dispatched {
            boot_dispatched = true;
            let boot_result = boot_result.take().expect("boot snapshot is collected once");
            dispatch_or_retain(
                &app_runner,
                inkwash_logic::app::Event::Boot(boot_result.snapshot),
                &mut ctx,
            )?;
            let scheduler_config = scheduler_config
                .take()
                .expect("scheduler configuration is collected once");
            dispatch_or_retain(
                &app_runner,
                inkwash_logic::app::Event::SyncSchedulerConfigured(scheduler_config),
                &mut ctx,
            )?
            // Boot alarm needs no special casing: the SM entered
            // Screen::AlarmRinging from the Boot dispatch (same transition
            // as a runtime RtcAlarmSnapshotReady), the executor rendered the
            // ring frame and started the tone through the audio task; ENTER
            // presses flow through the normal button poll below and dismiss
            // via the SM's dismiss_ringing.
        }
        let now = Instant::now();

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
        if ctx.pending_clock_read.is_none() && now.duration_since(clock_last) >= clock_interval {
            clock_last = now;
            match ctx.rtc.request_read_time() {
                Ok(reply) => ctx.pending_clock_read = Some(reply),
                Err(err) => log::warn!("RTC read_time request failed: {err}"),
            }
        }
        if let Some(reply) = ctx.pending_clock_read.take() {
            match reply.try_recv() {
                Ok(Ok(dt)) => {
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
                    }
                    // Runtime Tick is fed on every RTC sample so the state
                    // machine can observe the 30-second urgent boundary;
                    // its render path still refreshes only on visible minute
                    // changes.
                    dispatch_or_retain(&app_runner, inkwash_logic::app::Event::Tick(dt), &mut ctx)
                        .map(|_| ())?
                }
                Ok(Err(err)) => log::warn!("PCF8563 read_time failed: {err}"),
                Err(std::sync::mpsc::TryRecvError::Empty) => {
                    ctx.pending_clock_read = Some(reply);
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    log::warn!("RTC read_time executor disconnected");
                }
            }
        }

        // AppRunner is the sole alarm owner from step 2 onwards. The
        // low -> high AF edge is resolved by the *shared* `AlarmPoll`
        // orchestration (the same code `DeviceContext::poll_alarm_snapshot`
        // runs for every collection path): AF edge -> consistent snapshot ->
        // dispatch `RtcAlarmSnapshotReady`. The SM enters
        // Screen::AlarmRinging (non-blocking), the executor renders the
        // ring + starts the audio-task tone; ENTER / the ring-deadline Tick
        // dismiss through the SM. `main` consumes the exit flag below; the
        // edge / retry / firing semantics are identical at boot and runtime.
        if ctx.poll_alarm_snapshot()? {
            // The SM owns the ring's Full renders (enter + dismiss); nothing
            // here needs a caller-side dirty-path suppression anymore.
            // Home is the root page: the sticky exit flag exists to let
            // *nested* pages unwind layer by layer. Here we are already
            // at Home, so the flag must be consumed immediately -
            // otherwise the next Navigation/Settings entry would see a
            // historical alarm as its own exit reason and return at once
            // (round-22 P1). Blocking-page / reminder paths keep the
            // flag set for their caller chains and main consumes it when
            // they return (see the open_navigation handlers). The
            // consume-for-source policy is the shared production code the
            // host harness drives.
            let _ = ctx
                .alarm_poll
                .consume_for(inkwash_logic::alarm_flow::AlarmSource::Home);
        }
        // USB command deduplication is scoped to a host connection. The
        // Serial/JTAG connection probe is the platform's available
        // connection fact; use its edges to create/end command sessions.
        let usb_host_connected = power::usb_host_connected();
        if usb_host_connected && !usb_host_was_connected {
            ctx.command_sessions
                .end(control::Channel::Usb, ctx.usb_session_id);
            ctx.usb_session_id = ctx.usb_session_id.wrapping_add(1).max(1);
            ctx.command_sessions
                .begin(control::Channel::Usb, ctx.usb_session_id);
            ctx.drop_stale_usb_replies();
        } else if !usb_host_connected && usb_host_was_connected {
            ctx.command_sessions
                .end(control::Channel::Usb, ctx.usb_session_id);
            ctx.usb_session_id = ctx.usb_session_id.wrapping_add(1).max(1);
            ctx.drop_stale_usb_replies();
        }
        usb_host_was_connected = usb_host_connected;
        // Poll USB console for incoming commands, dispatch them, and send replies.
        // Stage 5: the redraw is NOT pushed here - state-changing commands
        // (ClearAlarms / SetWifi / SetTimezone / SyncNow) emit their own
        // render from the state machine at the point the visible state
        // changes; the renderer diff decides Noop vs refresh.
        let (_usb_changed, usb_activity) = ctx.poll_usb_control(clock.as_ref())?;

        // Poll BLE worker results, lifecycle facts, and incoming commands;
        // every operation is non-blocking on the main loop.
        while let Some((session_id, message)) = ctx.take_ble_start_failure() {
            let current_session = inkwash_logic::app::ble_pairing_session_matches(
                &app_runner.borrow().state().screen,
                session_id,
            );
            if current_session {
                let event = inkwash_logic::app::Event::BlePairingFailed(
                    inkwash_logic::app::BlePairingFailure { message },
                );
                dispatch_or_retain(&app_runner, event, &mut ctx)?;
            }
        }
        // Lifecycle is reduced before worker results so a disconnect cannot
        // turn a late notify callback into a delivery for the old connection.
        let mut ble_changed = poll_ble_lifecycle(&app_runner, &mut ctx)?;
        if let Some((_reply_id, result)) = ctx.pending_ble_pairing_success.take() {
            // The dispatcher owns the event even when servicing the effect
            // worker fails. Do not put a second copy back into the BLE slot.
            ctx.pending_ble_set_wifi_ack = None;
            match dispatch_or_retain(
                &app_runner,
                inkwash_logic::app::Event::BlePairingSucceeded(result.clone()),
                &mut ctx,
            ) {
                Ok(()) => {}
                Err(err) => return Err(err),
            }
        }
        while let Some(result) = ctx.ble_control.poll_result() {
            let (session_id, event) = match result {
                ble_control::BleTaskResult::Started { session_id } => {
                    (session_id, inkwash_logic::app::Event::BlePairingStarted)
                }
                ble_control::BleTaskResult::Failed {
                    session_id,
                    message,
                } => (
                    session_id,
                    inkwash_logic::app::Event::BlePairingFailed(
                        inkwash_logic::app::BlePairingFailure { message },
                    ),
                ),
                ble_control::BleTaskResult::Stopped { session_id } => {
                    if ctx.ble_stopped(session_id) {
                        log::info!("BLE session {session_id} stopped; Wi-Fi resume queued");
                    } else {
                        log::warn!("Ignoring stale BLE stop result for session {session_id}");
                    }
                    continue;
                }
                ble_control::BleTaskResult::ReplyDelivered {
                    session_id,
                    generation,
                    conn_handle,
                    reply_id,
                } => {
                    log::debug!(
                        "BLE reply {reply_id} notify completed for session {session_id}, generation {generation}, handle {conn_handle}"
                    );
                    if let Some(index) = ctx.pending_ble_replies.iter().position(
                        |(reply_session, reply_generation, reply_conn, id, _, _, _)| {
                            *reply_session == session_id
                                && *reply_generation == generation
                                && *reply_conn == conn_handle
                                && *id == reply_id
                        },
                    ) {
                        let (_, _, _, _, id, command, reply) =
                            ctx.pending_ble_replies.remove(index);
                        let clears_handoff =
                            ctx.pending_ble_handoff.as_ref().is_some_and(|handoff| {
                                handoff.generation == generation
                                    && handoff.conn_handle == conn_handle
                                    && handoff.id == id
                                    && handoff.command == command
                            });
                        if ctx.ble_session_id == Some(session_id)
                            && ctx.ble_connection_generation == Some(generation)
                            && ctx.ble_connection_handle == Some(conn_handle)
                        {
                            if let Some(id) = id {
                                ctx.command_sessions.complete_terminal(
                                    control::Channel::Ble,
                                    generation,
                                    id,
                                    command,
                                    reply,
                                );
                            } else {
                                ctx.command_sessions.complete_untagged_terminal(
                                    control::Channel::Ble,
                                    generation,
                                    command,
                                    reply,
                                );
                            }
                            if clears_handoff {
                                ctx.pending_ble_handoff = None;
                            }
                            if ctx.pending_ble_set_wifi_ack == Some(reply_id) {
                                let pairing_result = inkwash_logic::app::BlePairingResult {
                                    name: "Inkwash".into(),
                                };
                                // The notify completion has already been
                                // consumed; the state-machine event below is
                                // now the sole retry owner.
                                ctx.pending_ble_set_wifi_ack = None;
                                dispatch_or_retain(
                                    &app_runner,
                                    inkwash_logic::app::Event::BlePairingSucceeded(
                                        pairing_result.clone(),
                                    ),
                                    &mut ctx,
                                )?
                            }
                        }
                    }
                    continue;
                }
                ble_control::BleTaskResult::ReplyFailed {
                    session_id,
                    generation,
                    conn_handle,
                    reply_id,
                } => {
                    log::warn!(
                        "BLE reply {reply_id} notify failed for session {session_id}, generation {generation}, handle {conn_handle}; retry queued"
                    );
                    continue;
                }
                ble_control::BleTaskResult::ReplyTerminated {
                    session_id,
                    generation,
                    conn_handle,
                    reply_id,
                } => {
                    if let Some(index) = ctx.pending_ble_replies.iter().position(
                        |(reply_session, reply_generation, reply_conn, id, _, _, _)| {
                            *reply_session == session_id
                                && *reply_generation == generation
                                && *reply_conn == conn_handle
                                && *id == reply_id
                        },
                    ) {
                        let (_, _, _, _, id, command, _) = ctx.pending_ble_replies.remove(index);
                        if ctx.ble_session_id == Some(session_id)
                            && ctx.ble_connection_generation == Some(generation)
                            && ctx.ble_connection_handle == Some(conn_handle)
                        {
                            let clears_handoff =
                                ctx.pending_ble_handoff.as_ref().is_some_and(|handoff| {
                                    handoff.session_id == session_id
                                        && handoff.generation == generation
                                        && handoff.conn_handle == conn_handle
                                        && handoff.id == id
                                        && handoff.command == command
                                });
                            ctx.command_sessions.cancel_pending(
                                control::Channel::Ble,
                                generation,
                                id.as_deref(),
                                &command,
                            );
                            if clears_handoff {
                                ctx.pending_ble_handoff = None;
                            }
                            if ctx.pending_ble_set_wifi_ack == Some(reply_id) {
                                ctx.pending_ble_set_wifi_ack = None;
                                if let Err(err) = ctx.abort_ble_set_wifi_handoff() {
                                    log::warn!(
                                        "BLE SetWifi transport termination cleanup failed: {err}"
                                    );
                                }
                            }
                        }
                    }
                    log::warn!(
                        "BLE reply {reply_id} terminated for session {session_id}, generation {generation}, handle {conn_handle}"
                    );
                    continue;
                }
            };
            let current_session = inkwash_logic::app::ble_pairing_session_matches(
                &app_runner.borrow().state().screen,
                session_id,
            );
            if current_session {
                dispatch_or_retain(&app_runner, event, &mut ctx)?;
            } else {
                log::warn!("Ignoring stale BLE worker result for session {session_id}");
            }
        }

        ctx.ble_control
            .retry_pending_replies(ctx.ble_session_id, ctx.ble_connection_generation);
        retry_ble_deliveries(&mut ctx);
        if let Some(message) = ctx.ble_control.poll_command() {
            let id = message.id;
            let cmd = message.command;
            let command_session_id = message.session_id;
            let command_generation = message.generation;
            let command_conn_handle = message.conn_handle;
            if ctx.ble_session_id != Some(command_session_id)
                || ctx.ble_connection_generation != Some(command_generation)
                || ctx.ble_connection_handle != Some(command_conn_handle)
            {
                log::warn!(
                    "Ignoring stale BLE command for session {command_session_id}, generation {command_generation}, handle {command_conn_handle}"
                );
                continue;
            }
            // A valid frame is activity even when it is a cached replay or a
            // transport-level Busy response. Those fast paths continue before
            // the normal command dispatcher, but must still cancel idle sleep.
            ble_changed = true;
            if let Some(id_value) = id.as_deref() {
                let ble_session_id = command_generation;
                ctx.command_sessions
                    .begin(control::Channel::Ble, ble_session_id);
                if let Some(reply) = ctx.command_sessions.lookup(
                    control::Channel::Ble,
                    ble_session_id,
                    id_value,
                    &cmd,
                ) {
                    let generation = ctx.ble_connection_generation.unwrap_or(0);
                    deliver_ble_reply(
                        &mut ctx,
                        command_session_id,
                        generation,
                        command_conn_handle,
                        Some(id_value.to_owned()),
                        cmd.clone(),
                        reply,
                    );
                    continue;
                }
            }
            // SetRtc falls through to dispatch_migrated_command (Phase 5:
            // routed through Effect::WriteRtcTime, not handled directly).
            if matches!(cmd, control::Command::SyncNow) && ctx.pending_wifi_op.is_some() {
                let generation = ctx.ble_connection_generation.unwrap_or(0);
                deliver_ble_reply(
                    &mut ctx,
                    command_session_id,
                    generation,
                    command_conn_handle,
                    id.clone(),
                    cmd.clone(),
                    control::Reply::Busy,
                );
                continue;
            }
            let runner = app_runner.clone();
            let is_time_write = matches!(
                cmd,
                control::Command::SetRtc { .. } | control::Command::SetTimezone { .. }
            );
            let pre_event = if matches!(cmd, control::Command::SetTimezone { .. }) {
                runner
                    .borrow()
                    .last_clock()
                    .map(inkwash_logic::app::Event::Tick)
            } else {
                None
            };
            let ble_session_id = command_generation;
            let request_for_cache = cmd.clone();
            let is_ble_set_wifi_handoff =
                matches!(&request_for_cache, control::Command::SetWifi { .. })
                    && ctx.ble_wifi_suspended;
            let reply = ctx::dispatch_migrated_command(
                &mut ctx,
                &runner,
                inkwash_logic::app::Event::BleCommand(cmd.clone()),
                pre_event,
                cmd,
                id.as_deref(),
                ble_session_id,
            )?;
            if is_ble_set_wifi_handoff && ctx.pending_ble_handoff.is_none() {
                if let Some((pending_id, pending_command)) = ctx
                    .command_sessions
                    .pending_key(control::Channel::Ble, ble_session_id)
                    .map(|(id, command)| (id.map(str::to_owned), command.clone()))
                    .filter(|(pending_id, pending_command)| {
                        pending_id.as_deref() == id.as_deref()
                            && pending_command == &request_for_cache
                    })
                {
                    ctx.pending_ble_handoff = Some(ctx::PendingBleHandoff {
                        session_id: command_session_id,
                        generation: ble_session_id,
                        conn_handle: command_conn_handle,
                        id: pending_id,
                        command: pending_command,
                    });
                }
            }
            if let Some(reply) = reply {
                if matches!(reply, control::Reply::Ok) {
                    ble_changed = true;
                }
                deliver_ble_reply(
                    &mut ctx,
                    command_session_id,
                    ble_session_id,
                    command_conn_handle,
                    id,
                    request_for_cache,
                    reply.clone(),
                );
                if is_time_write && matches!(reply, control::Reply::Ok) {
                    ctx.pending_alarm_status = None;
                    ctx.pending_alarm_snapshot = None;
                    ctx.pending_clock_read = None;
                    ctx.alarm_poll.observe_alarm_flag(false);
                }
            }
        }

        // Retry a previously observed EPD terminal fact before consuming a
        // newer completion. The registry entry was removed only when the
        // source fact was matched; this retained envelope owns the retry.
        if let Some(pending) = ctx.pending_render_completion.take() {
            let current_generation = app_runner.borrow().state().render_generation;
            let failed = pending.failure.is_some();
            let retry = ctx::PendingRenderCompletion {
                kick: pending.kick.clone(),
                output: pending.output.clone(),
                failure: pending.failure.clone(),
            };
            if let Err(err) = feed_completion_back(
                &app_runner,
                &mut ctx,
                pending.kick,
                pending.output,
                pending.failure,
            ) {
                log::warn!("AppRunner EPD completion retry failed: {err}");
            } else {
                app_runner::apply_render_cache_terminal(
                    &ctx.pending_renders,
                    &retry.kick,
                    failed,
                    current_generation,
                );
            }
        }

        // EPD completion events: feed each one back to the state machine
        // as `EffectCompleted(RenderDone)` / `EffectFailed(Render)`. The
        // shared `pending_renders` registry (reachable from the main loop
        // and every collection path) pairs each `EpdCompletion` with the
        // next in-flight render. Out-of-order or extra completions are
        // absorbed by the state machine, which drops effects whose
        // render_generation is older than the current visible state.
        while let Some(completion) = ctx.board.display.poll_completion() {
            // Match by request_id (echoed by the EPD task) instead of
            // FIFO: the single EPD slot can merge or overwrite requests,
            // so only the completion whose id matches an in-flight
            // AppRunner render is fed back. Unregistered dirty-rect / ring /
            // boot refreshes carry ids with no matching kick and are
            // observed but not fed back.
            // The shared `RenderRegistry` implements the per-request
            // terminal rule; this loop drives it and feeds Completed /
            // Failed back through the returned kick.
            let outcome = {
                let mut reg = ctx.pending_renders.borrow_mut();
                reg.feed(completion.request_id, completion.ok, completion.superseded)
            };
            match outcome {
                inkwash_logic::epd_registry::FeedOutcome::Matched(kick, terminal) => {
                    match terminal {
                        inkwash_logic::epd_registry::RenderTerminal::Superseded => {
                            // The request was replaced before its command
                            // ran (EPD latest-wins). It must NOT be
                            // reported as RenderDone - its pixels never
                            // reached the panel. The replacement request
                            // carries its own request_id and will complete
                            // (or fail) on its own; we simply terminate
                            // this in-flight entry here.
                            log::warn!(
                                "AppRunner render (op {:?}) superseded before panel display",
                                kick.operation_id
                            );
                        }
                        terminal => {
                            let failed =
                                terminal == inkwash_logic::epd_registry::RenderTerminal::Failed;
                            let output = inkwash_logic::app::EffectOutput::RenderDone;
                            let failure = if failed {
                                Some(inkwash_logic::app::EffectError::Render(format!(
                                    "epd refresh failed: {:?}",
                                    completion.kind
                                )))
                            } else {
                                None
                            };
                            let retry = ctx::PendingRenderCompletion {
                                kick: kick.clone(),
                                output: output.clone(),
                                failure: failure.clone(),
                            };
                            if let Err(err) =
                                feed_completion_back(&app_runner, &mut ctx, kick, output, failure)
                            {
                                log::warn!("AppRunner EPD completion feed failed: {err}");
                                break;
                            } else {
                                let current_generation =
                                    app_runner.borrow().state().render_generation;
                                app_runner::apply_render_cache_terminal(
                                    &ctx.pending_renders,
                                    &retry.kick,
                                    failed,
                                    current_generation,
                                );
                            }
                        }
                    }
                }
                inkwash_logic::epd_registry::FeedOutcome::Ignored => {
                    // No matching kick: unregistered dirty-rect / ring / boot
                    // refresh, or a duplicate for an already-terminated
                    // request. Observed but not fed back.
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
        // alignment, and transport replies. Stage 5: the redraw is NOT
        // decided here - poll_wifi_ops feeds the completion into the state
        // machine (which owns the merged-data apply + transport reply and
        // emits a render whose ViewModel diff decides Noop vs refresh), so
        // an unchanged sync costs no panel refresh and a changed list
        // repaints exactly what shows it.
        ctx.poll_wifi_ops()?;

        // The main loop hosts the state-machine screens: every screen (Home,
        // Navigation drawer, Settings, AlarmList, ... ) owns the buttons, and
        // every debounced button event is fed to the state machine as
        // Event::Button through the single dispatch_app_runner pump. The SM
        // owns what the keys mean there and renders its own screen changes as
        // Effect::Render kicks. No parallel render wedges remain (the SM-disabled nav
        // loop and the blocking BLE-pairing wedge were deleted in P1#3/P1#4).
        let mut key_changed = false;
        {
            let enter_event = ctx.board.key_enter.poll();
            let up_event = ctx.board.key_up.poll();
            let down_event = ctx.board.key_down.poll();
            for event in [enter_event, up_event, down_event].into_iter().flatten() {
                key_changed = true;
                dispatch_or_retain(
                    &app_runner,
                    inkwash_logic::app::Event::Button(event),
                    &mut ctx,
                )?
            }
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
        let interacted = user_activity;

        // Resolve a platform sleep handshake only after this full polling
        // turn has observed input, transports, EPD completions and worker
        // receipts. Prepare success is represented by the retained kick;
        // commit gets a fresh mechanical snapshot and can still cancel.
        if let Some(kick) = ctx.pending_sleep_kick.take() {
            let event = match &kick.effect {
                inkwash_logic::app::Effect::PrepareSleep { token, .. } => {
                    let wake_plan_confirmed =
                        wake_plan_confirmed_for(&ctx, *token, kick.operation_id);
                    inkwash_logic::app::Event::SleepPrepared {
                        token: *token,
                        inputs: collect_sleep_inputs(&ctx, wake_plan_confirmed),
                    }
                }
                inkwash_logic::app::Effect::CommitSleep(token) => {
                    let wake_plan_confirmed =
                        wake_plan_confirmed_for(&ctx, *token, kick.operation_id);
                    let inputs = collect_sleep_inputs(&ctx, wake_plan_confirmed);
                    let safe = app_runner
                        .borrow()
                        .state()
                        .sleep
                        .final_check(*token, inputs);
                    if safe {
                        inkwash_logic::app::Event::SleepCommitted(*token)
                    } else {
                        inkwash_logic::app::Event::SleepCancelled(*token)
                    }
                }
                _ => continue,
            };
            let committed_or_cancelled = matches!(
                &event,
                inkwash_logic::app::Event::SleepCommitted(_)
                    | inkwash_logic::app::Event::SleepCancelled(_)
            );
            let prepare_fact = matches!(&event, inkwash_logic::app::Event::SleepPrepared { .. });
            let runner = app_runner.clone();
            let dispatched = match dispatch_or_retain(&runner, event, &mut ctx) {
                Ok(()) => true,
                Err(err) => {
                    return Err(err);
                }
            };
            // Only clear the platform wake-plan after the runtime queue has
            // no pending copy and the fact has therefore reached the reducer.
            let fact_reduced = ctx.pending_app_events.is_empty()
                && ctx.pending_dispatch_event.is_none()
                && ctx.pending_dispatch_sources.is_empty()
                && !runner.borrow().has_work();
            if dispatched && fact_reduced {
                if committed_or_cancelled {
                    ctx.prepared_wake_plan = None;
                } else if prepare_fact && ctx.pending_sleep_kick.is_none() {
                    // Prepare's business gates rejected the platform fact,
                    // so do not retain a wake plan for a token that was
                    // cleared.
                    ctx.prepared_wake_plan = None;
                }
            }
        }

        // Feed the state machine's sole sleep admission path after all input
        // and transport facts for this iteration have been collected. The
        // state machine owns the idle thresholds and page/business gates;
        // this poll carries only mechanical facts from the platform loop.
        if app_runner_enabled {
            let power_poll = inkwash_logic::app::PowerPoll {
                now_ticks: now.duration_since(power_ticks_origin).as_millis() as u64,
                activity_observed: user_activity,
                final_display_pending: !ctx.pending_renders.borrow().pending.is_empty()
                    || !ctx.pending_render_retries.is_empty()
                    || ctx.pending_render_completion.is_some(),
                final_persist_pending: ctx.pending_effect_batch.is_some()
                    || !ctx.pending_effect_batches.is_empty()
                    || ctx.worker_batch_in_flight
                    || ctx.pending_effect_notices.is_some(),
                usb_connected: usb_host_connected,
                event_queue_empty: !app_runner.borrow().has_work()
                    && ctx.pending_effect_batch.is_none()
                    && ctx.pending_effect_batches.is_empty()
                    && !ctx.worker_batch_in_flight
                    && ctx.pending_effect_notices.is_none()
                    && ctx.pending_ble_pairing_success.is_none()
                    && ctx.pending_usb_replies.is_empty()
                    && ctx.pending_usb_reply_latch.is_none()
                    && ctx.pending_render_retries.is_empty()
                    && ctx.pending_ble_replies.is_empty()
                    && ctx.pending_ble_deliveries.is_empty()
                    && ctx.pending_ble_handoff.is_none()
                    && ctx.pending_alarm_status.is_none()
                    && ctx.pending_alarm_snapshot.is_none()
                    && ctx.pending_clock_read.is_none()
                    && ctx.pending_sleep_kick.is_none()
                    && ctx.pending_app_events.is_empty()
                    && ctx.pending_dispatch_event.is_none()
                    && ctx.pending_dispatch_sources.is_empty(),
                input_latch_clear: !any_key_pressed,
                wake_plan_confirmed: false,
                // The current sync worker has no resumable light-sleep
                // checkpoint; keep this false even while a request exists.
                network_resumable: false,
                light_wake_after_ms: IDLE_POLL_INTERVAL_MS,
            };
            dispatch_or_retain(
                &app_runner,
                inkwash_logic::app::Event::PowerPoll(power_poll),
                &mut ctx,
            )?
        }
        let light_sleep_committed = app_runner_enabled
            && app_runner.borrow().state().sleep.committed_kind()
                == Some(inkwash_logic::power_state::SleepKind::Light);
        if light_sleep_committed && !light_sleep_enabled {
            // `EnterLightSleep` is executed by the state-machine effect
            // runner when SleepCommitted is reduced. The committed token is
            // therefore the acknowledgement that the single platform path
            // succeeded; do not configure PM a second time here.
            light_sleep_enabled = true;
        } else if !light_sleep_committed && light_sleep_enabled {
            // The state machine emits DisableLightSleep for this transition;
            // the effect runner owns the platform call.
            light_sleep_enabled = false;
        }
        if interacted || any_key_pressed {
            last_activity = now;
            idle = false;
        } else if now.duration_since(last_activity) >= IDLE_ENTER_AFTER {
            idle = true;
        }

        // A connected USB host is an active debugging/control session even
        // when it is only reading logs and sends no command frames. IDF's
        // CONFIG_USJ_NO_AUTO_LS_ON_CONNECTION lock keeps automatic light
        // sleep from breaking the USB peripheral; the state machine applies
        // the matching deep-sleep guard through `SleepBlocker::UsbConnected`
        // (`PowerPoll.usb_connected`), so no timer bookkeeping is needed here.

        // Sleep entry is emitted only by the application state machine after
        // PowerPoll -> prepare -> final mechanical commit. The loop below is
        // only the wakeable idle wait used by the committed light-sleep tier.

        if light_sleep_committed {
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

/// Architecture minimum safe mode (firmware-architecture.md "启动失败与安全
/// 模式"). Entered when a *core* boot fact is unavailable (RTC alarm-status
/// read failed, or the alarm NVS list could not be loaded): there is no
/// trustworthy local data or time to build a meaningful BootSnapshot, so the
/// normal App state machine must not run (an empty alarm list would treat a
/// real AF as residue and ACK it off). This is an independent minimal loop -
/// NOT the normal Home/product loop with the state machine disabled:
///
/// - renders a fixed error screen once;
/// - services only USB diagnostics (GetStatus -> a Status reply with no
///   configured/connected facts; every other command -> an explicit Error)
///   so `inkwash-desktop` can see the device is in safe mode;
/// - keeps the watchdog fed and watches the reset (ENTER) button;
/// - never initializes or drives Wi-Fi, BLE, light sleep or deep sleep, and
///   never opens navigation/content pages;
/// - never returns: the only way out is a reset / power-cycle, which re-runs
///   the normal boot detection.
fn run_safe_mode(
    board: &mut Note4Board,
    usb_console: &mut crate::usb_console::UsbConsole,
    usb_reply_writer: &mut UsbReplyWriter,
    reason: &str,
) -> ! {
    log::error!("Entering minimum safe mode (core boot fact unavailable: {reason})");
    // Fixed error screen. Drawn once; safe mode makes no further display
    // decisions (no renderer cache, no partials, no clock).
    {
        let mut canvas = board.display.canvas_mut();
        canvas.clear();
        crate::ui::header(&mut canvas, "SAFE MODE");
        canvas.draw_text_prop(8, 60, 1, "CORE DATA UNAVAILABLE");
        // Reason text at scale 1; hard-split at 40 chars onto two lines.
        let (r1, r2) = if reason.len() > 40 {
            let (head, rest) = reason.split_at(40);
            (head.to_string(), rest.chars().take(40).collect::<String>())
        } else {
            (reason.to_string(), String::new())
        };
        canvas.draw_text_prop(8, 80, 1, &r1);
        if !r2.is_empty() {
            canvas.draw_text_prop(8, 96, 1, &r2);
        }
        canvas.draw_text_prop(8, 140, 1, "USB DIAGNOSTICS ACTIVE");
        canvas.draw_text_prop(8, 156, 1, "HOLD ENTER + RESET TO EXIT");
        drop(canvas);
        if let Err(err) = board.display.refresh_full() {
            log::error!("Safe-mode error screen refresh failed: {err:#}");
        }
    }
    let mut pending_replies: std::collections::VecDeque<(usb_console::QueuedReply, bool)> =
        std::collections::VecDeque::with_capacity(SAFE_MODE_REPLY_CAPACITY);
    // Minimal service loop.
    loop {
        watchdog::feed();
        service_safe_mode_replies(usb_reply_writer, &mut pending_replies);
        // USB diagnostics: GetStatus reports a device with nothing
        // configured/connected; everything else returns an explicit error.
        if let Some((id, cmd)) = usb_console.poll_command() {
            match cmd {
                crate::control::Command::GetStatus => {
                    let reply = inkwash_logic::protocol::Reply::Status {
                        wifi_configured: false,
                        server_configured: false,
                        wifi_connected: false,
                        wifi_ssid: None,
                        wifi_has_password: false,
                        server_url: None,
                        server_has_token: false,
                        timezone_offset_minutes: 0,
                    };
                    queue_safe_mode_reply(
                        usb_reply_writer,
                        &mut pending_replies,
                        &reply,
                        id.as_deref(),
                    );
                    log::info!("Safe mode: answered GetStatus (device in safe mode)");
                }
                other => {
                    let reply = inkwash_logic::protocol::Reply::Error {
                        message: format!(
                            "device is in safe mode (core data unavailable); command {other:?} rejected"
                        ),
                    };
                    queue_safe_mode_reply(
                        usb_reply_writer,
                        &mut pending_replies,
                        &reply,
                        id.as_deref(),
                    );
                }
            }
        }
        // Reset/button detection only (no navigation). ENTER long-press is
        // polled so a stuck boot never hides the reset affordance; the
        // action itself is a hardware reset via the watchdog / power latch
        // on the next power-cycle - here we only observe.
        board.key_enter.poll();
        board.key_up.poll();
        board.key_down.poll();
        // Sane poll cadence. Deliberately NO light-sleep arm and NO
        // deep-sleep: safe mode stays fully awake (architecture).
        std::thread::sleep(std::time::Duration::from_millis(
            crate::button::POLL_INTERVAL_MS as u64,
        ));
    }
}

fn queue_safe_mode_reply(
    writer: &mut UsbReplyWriter,
    pending: &mut std::collections::VecDeque<(usb_console::QueuedReply, bool)>,
    reply: &inkwash_logic::protocol::Reply,
    id: Option<&str>,
) {
    if pending.len() >= SAFE_MODE_REPLY_CAPACITY {
        log::error!("safe-mode USB reply mailbox full; retaining no additional frame");
        return;
    }
    let queued = writer.prepare(reply, id);
    let accepted = match writer.enqueue_owned(queued.clone()) {
        Ok(_) => true,
        Err(usb_console::ReplyQueueError::Full(_))
        | Err(usb_console::ReplyQueueError::Disconnected(_)) => false,
    };
    pending.push_back((queued, accepted));
}

fn service_safe_mode_replies(
    writer: &mut UsbReplyWriter,
    pending: &mut std::collections::VecDeque<(usb_console::QueuedReply, bool)>,
) {
    while let Ok(Some(ack)) = writer.try_completion() {
        let Some(index) = pending
            .iter()
            .position(|(queued, _)| queued.sequence == ack.sequence)
        else {
            continue;
        };
        if ack.result.is_ok() {
            pending.remove(index);
        } else if let Some((_, accepted)) = pending.get_mut(index) {
            *accepted = false;
        }
    }
    for (queued, accepted) in pending.iter_mut() {
        if *accepted {
            continue;
        }
        match writer.retry(queued.clone()) {
            Ok(()) => *accepted = true,
            Err(usb_console::ReplyQueueError::Full(_))
            | Err(usb_console::ReplyQueueError::Disconnected(_)) => break,
        }
    }
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

/// The core facts are read once before any runtime task is created. They are
/// retained as an owned boot fact so the later snapshot construction cannot
/// re-read RTC/NVS after SyncTask, EffectTask, or BLE has started.
#[derive(Debug)]
struct BootCoreFacts {
    alarm_status: crate::rtc_executor::AlarmStatus,
    alarms: Vec<crate::alarms::StoredAlarm>,
}

fn collect_boot_core_facts(
    rtc: &crate::rtc_executor::RtcExecutor,
    alarm_store: &AlarmStore,
    clock: Option<DateTime>,
    prior_failure: Option<String>,
) -> std::result::Result<BootCoreFacts, String> {
    if let Some(reason) = prior_failure {
        return Err(reason);
    }
    if clock.is_none() {
        return Err("No trusted RTC time available for BootSnapshot".to_string());
    }
    let alarm_status = rtc
        .alarm_status()
        .map_err(|err| format!("RTC alarm status read failed: {err}"))?;
    let alarms = alarm_store
        .load()
        .map_err(|err| format!("Alarm NVS load failed: {err}"))?;
    Ok(BootCoreFacts {
        alarm_status,
        alarms,
    })
}

/// Build the BootSnapshot the state machine consumes in Event::Boot. Core
/// RTC/alarm facts come from `BootCoreFacts`; remaining NVS facts are soft
/// failures and become empty/default values so the snapshot remains owned by
/// the application state machine.
#[derive(Debug)]
struct BootSnapshotResult {
    snapshot: inkwash_logic::app::BootSnapshot,
}

fn collect_boot_snapshot(
    core: BootCoreFacts,
    counters: &PersistedCounters,
    todo_store: &TodoStore,
    inbox_store: &InboxStore,
    clock: Option<DateTime>,
) -> BootSnapshotResult {
    let todos = todo_store.load().unwrap_or_else(|err| {
        log::warn!("Todo NVS load failed (using empty list): {err}");
        Vec::new()
    });
    let inbox = inbox_store.load().unwrap_or_else(|err| {
        log::warn!("Inbox NVS load failed (using empty list): {err}");
        Vec::new()
    });
    let config = counters.device_config().ok().flatten().unwrap_or_else(|| {
        inkwash_logic::device_config::DeviceConfig {
            server_url: String::new(),
            auth_token: String::new(),
        }
    });
    let wake_cause = power::wake_cause();
    // Status-visible facts (no secrets) for GetStatus: the wi-fi SSID /
    // password-presence, the timezone, and last-known connectivity.
    let wifi = counters.wifi_creds().ok().flatten();
    let status = inkwash_logic::app::DeviceStatus {
        wifi_ssid: wifi.as_ref().map(|creds| creds.ssid.clone()),
        wifi_has_password: wifi.is_some_and(|creds| !creds.password.is_empty()),
        timezone_offset_minutes: counters.timezone_offset_minutes().unwrap_or(0),
    };
    BootSnapshotResult {
        snapshot: inkwash_logic::app::BootSnapshot {
            wake_cause,
            now: clock,
            rtc_alarm_flag: core.alarm_status.alarm_flag,
            rtc_alarm_interrupt_enabled: core.alarm_status.alarm_interrupt_enabled,
            alarms: core.alarms,
            todos,
            inbox,
            config,
            status,
        },
    }
}

fn poll_ble_lifecycle(
    runner: &std::rc::Rc<std::cell::RefCell<app_runner::AppRunner>>,
    ctx: &mut DeviceContext<'_>,
) -> anyhow::Result<bool> {
    let mut changed = false;
    while let Some(lifecycle) = ctx.ble_control.poll_lifecycle() {
        match lifecycle {
            ble_control::BleLifecycle::Connected {
                session_id,
                generation,
                conn_handle,
            } => {
                if ctx.ble_session_id != Some(session_id) {
                    log::warn!(
                        "Ignoring stale BLE connect session {session_id}, generation {generation}"
                    );
                    continue;
                }
                ctx.ble_connection_generation = Some(generation);
                ctx.ble_connection_handle = Some(conn_handle);
                if let Some(handoff) = ctx.pending_ble_handoff.as_mut() {
                    if handoff.session_id == session_id {
                        ctx.command_sessions
                            .begin_preserving_pending(control::Channel::Ble, generation);
                        handoff.generation = generation;
                        handoff.conn_handle = conn_handle;
                    } else {
                        ctx.command_sessions
                            .begin(control::Channel::Ble, generation);
                    }
                } else {
                    ctx.command_sessions
                        .begin(control::Channel::Ble, generation);
                }
                dispatch_or_retain(runner, inkwash_logic::app::Event::BlePairingStarted, ctx)?;
                changed = true;
            }
            ble_control::BleLifecycle::Disconnected {
                session_id,
                generation,
                conn_handle,
            } => {
                let current = ctx.ble_session_id == Some(session_id)
                    && ctx.ble_connection_generation == Some(generation)
                    && ctx.ble_connection_handle == Some(conn_handle);
                if !current {
                    log::warn!(
                        "Ignoring stale BLE disconnect session {session_id}, generation {generation}, handle {conn_handle}"
                    );
                    continue;
                }
                ctx.ble_control
                    .drop_pending_generation(session_id, generation);
                ctx.pending_ble_replies.retain(
                    |(reply_session, reply_generation, _, _, _, _, _)| {
                        *reply_session != session_id || *reply_generation != generation
                    },
                );
                ctx.pending_ble_deliveries.retain(|delivery| {
                    delivery.session_id != session_id || delivery.generation != generation
                });
                if ctx
                    .pending_ble_reply_latch
                    .as_ref()
                    .is_some_and(|delivery| {
                        delivery.session_id == session_id && delivery.generation == generation
                    })
                {
                    if let Some(delivery) = ctx.pending_ble_reply_latch.take() {
                        ctx.command_sessions.cancel_pending(
                            control::Channel::Ble,
                            delivery.generation,
                            delivery.id.as_deref(),
                            &delivery.command,
                        );
                    }
                }
                if ctx.pending_ble_handoff.is_none() && ctx.ble_set_wifi_after_resume.is_some() {
                    if let Some((pending_id, pending_command)) = ctx
                        .command_sessions
                        .pending_key(control::Channel::Ble, generation)
                        .map(|(id, command)| (id.map(str::to_owned), command.clone()))
                        .filter(|(_, command)| matches!(command, control::Command::SetWifi { .. }))
                    {
                        ctx.pending_ble_handoff = Some(ctx::PendingBleHandoff {
                            session_id,
                            generation,
                            conn_handle,
                            id: pending_id,
                            command: pending_command,
                        });
                    }
                }
                ctx.pending_ble_set_wifi_ack = None;
                ctx.pending_ble_pairing_success = None;
                if ctx.pending_ble_handoff.is_none() {
                    ctx.command_sessions.end(control::Channel::Ble, generation);
                    if ctx.ble_set_wifi_after_resume.is_some() {
                        if let Err(err) = ctx.abort_ble_set_wifi_handoff() {
                            log::warn!("BLE SetWifi disconnect cleanup failed: {err}");
                        }
                    }
                }
                ctx.ble_connection_generation = None;
                ctx.ble_connection_handle = None;
                dispatch_or_retain(runner, inkwash_logic::app::Event::BleDisconnected, ctx)?;
                changed = true;
            }
        }
    }
    Ok(changed)
}

#[derive(Debug)]
pub(crate) struct DispatchSaturated {
    pub event: inkwash_logic::app::Event,
}

impl std::fmt::Display for DispatchSaturated {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("all bounded dispatch ownership slots are full; producer must retry")
    }
}

impl std::error::Error for DispatchSaturated {}

/// Admit one event and service the state machine without waiting for a worker
/// batch. Events that meet a full runtime queue are returned in a typed error
/// carrying the event so the actual producer can retain or reject it.
pub(crate) fn dispatch_app_runner(
    runner: &std::rc::Rc<std::cell::RefCell<app_runner::AppRunner>>,
    event: inkwash_logic::app::Event,
    ctx: &mut DeviceContext<'_>,
) -> anyhow::Result<()> {
    // Keep the incoming event in DeviceContext before servicing the effect
    // worker. A disconnected worker can make service_effect_task fail; the
    // event must remain owned by the producer for a later retry in that case.
    if ctx.pending_dispatch_event.is_none() {
        ctx.pending_dispatch_event = Some(event);
    } else {
        // A previous service failure already owns one event. Preserve this
        // newer event in the normal bounded FIFO while retrying the older one.
        retain_or_overflow_dispatch_event(ctx, event).map_err(anyhow::Error::new)?;
    }
    service_effect_task(runner, ctx)?;
    let event = ctx
        .pending_dispatch_event
        .take()
        .expect("dispatch event retained before worker service");
    admit_pending_app_events(runner, ctx);
    if ctx.pending_app_events.is_empty() {
        let admitted = runner.borrow_mut().admit_event(event);
        if let Err(event) = admitted {
            retain_or_overflow_dispatch_event(ctx, event).map_err(anyhow::Error::new)?;
        }
    } else {
        retain_or_overflow_dispatch_event(ctx, event).map_err(anyhow::Error::new)?;
    }
    while let Some(event) = ctx.pending_dispatch_sources.take_next() {
        if let Err(event) = runner.borrow_mut().admit_event(event) {
            if let Err(event) = ctx.pending_dispatch_sources.retain(event) {
                // The event was removed from its source latch before the
                // retry attempt, so this is only reachable if another event
                // of the same source filled it concurrently. Keep one final
                // producer-owned retry slot rather than consuming the fact.
                return Err(anyhow::Error::new(DispatchSaturated { event }));
            }
            break;
        }
    }
    reduce_effect_batches(runner, ctx)
}

/// Return a saturated event to its source owner. Every producer calls this
/// immediately after dispatch; if that source is already full, the typed
/// error is propagated to the producer with the event still owned by it.
pub(crate) fn dispatch_or_retain(
    runner: &std::rc::Rc<std::cell::RefCell<app_runner::AppRunner>>,
    event: inkwash_logic::app::Event,
    ctx: &mut DeviceContext<'_>,
) -> anyhow::Result<()> {
    match dispatch_app_runner(runner, event, ctx) {
        Ok(()) => Ok(()),
        Err(error) => match error.downcast::<DispatchSaturated>() {
            Ok(saturated) => match ctx.pending_dispatch_sources.retain(saturated.event) {
                Ok(()) => Ok(()),
                Err(event) => Err(anyhow::Error::new(DispatchSaturated { event })),
            },
            Err(error) => Err(error),
        },
    }
}

const PENDING_APP_EVENT_CAPACITY: usize = inkwash_logic::event_queue::HIGH_CAPACITY;

/// Retains one dispatch event in the bounded FIFO, returning it to its
/// producer when the FIFO is full. The `Err` variant deliberately carries the
/// event by value so this saturation path stays allocation-free (same
/// rationale as `PendingDispatchSources::retain`).
#[allow(clippy::result_large_err)]
fn retain_pending_app_event(
    ctx: &mut DeviceContext<'_>,
    event: inkwash_logic::app::Event,
) -> Result<(), inkwash_logic::app::Event> {
    if is_mergeable_dispatch_event(&event) {
        ctx.pending_app_events
            .retain(|queued| !same_mergeable_dispatch_kind(queued, &event));
    }
    if ctx.pending_app_events.len() >= PENDING_APP_EVENT_CAPACITY {
        return Err(event);
    }
    ctx.pending_app_events.push_back(event);
    Ok(())
}

/// Typed dispatch saturation: hands event ownership back to the real producer
/// instead of buffering it in an unbounded queue. Carried by value for the
/// same allocation-free reason as `retain_pending_app_event`.
#[allow(clippy::result_large_err)]
fn retain_or_overflow_dispatch_event(
    ctx: &mut DeviceContext<'_>,
    event: inkwash_logic::app::Event,
) -> Result<(), DispatchSaturated> {
    if let Err(event) = retain_pending_app_event(ctx, event) {
        if let Err(event) = ctx.pending_dispatch_sources.retain(event) {
            // A normal dispatch has already taken the primary slot above;
            // retain one final source-owned retry there if a fixed latch is
            // unexpectedly full. The only remaining failure means the
            // worker itself already owns an earlier retry and the caller
            // must stop producing until it reconnects.
            if ctx.pending_dispatch_event.is_none() {
                ctx.pending_dispatch_event = Some(event);
                return Ok(());
            }
            return Err(DispatchSaturated { event });
        }
    }
    Ok(())
}

fn is_mergeable_dispatch_event(event: &inkwash_logic::app::Event) -> bool {
    matches!(
        event,
        inkwash_logic::app::Event::Tick(_)
            | inkwash_logic::app::Event::PowerPoll(_)
            | inkwash_logic::app::Event::RtcAlarmSnapshotReady(_)
    )
}

fn same_mergeable_dispatch_kind(
    left: &inkwash_logic::app::Event,
    right: &inkwash_logic::app::Event,
) -> bool {
    matches!(
        (left, right),
        (
            inkwash_logic::app::Event::Tick(_),
            inkwash_logic::app::Event::Tick(_)
        ) | (
            inkwash_logic::app::Event::PowerPoll(_),
            inkwash_logic::app::Event::PowerPoll(_)
        ) | (
            inkwash_logic::app::Event::RtcAlarmSnapshotReady(_),
            inkwash_logic::app::Event::RtcAlarmSnapshotReady(_)
        )
    )
}

fn admit_pending_app_events(
    runner: &std::rc::Rc<std::cell::RefCell<app_runner::AppRunner>>,
    ctx: &mut DeviceContext<'_>,
) {
    while let Some(event) = ctx.pending_app_events.pop_front() {
        if let Err(event) = runner.borrow_mut().admit_event(event) {
            ctx.pending_app_events.push_front(event);
            break;
        }
    }
}

fn service_effect_task(
    runner: &std::rc::Rc<std::cell::RefCell<app_runner::AppRunner>>,
    ctx: &mut DeviceContext<'_>,
) -> anyhow::Result<()> {
    let mut worker_completed = false;
    let mut replayed_notices = false;
    if let Some(notices) = ctx.pending_effect_notices.take() {
        replayed_notices = true;
        let mut notices_iter = notices.into_iter();
        while let Some(notice) = notices_iter.next() {
            let mut runtime = runner.borrow_mut();
            if let Err(rejected) = runtime.submit_notice(notice) {
                let mut remaining = Vec::with_capacity(1);
                remaining.push(rejected);
                remaining.extend(notices_iter);
                ctx.pending_effect_notices = Some(remaining);
                return Ok(());
            }
        }
    }

    while let Some(result) = ctx
        .effect_task
        .try_next_notice()
        .map_err(|err| anyhow::anyhow!("effect worker disconnected: {err:?}"))?
    {
        let mut notices_iter = result.notices.into_iter();
        while let Some(notice) = notices_iter.next() {
            let mut runtime = runner.borrow_mut();
            if let Err(rejected) = runtime.submit_notice(notice) {
                let mut remaining = Vec::with_capacity(1);
                remaining.push(rejected);
                remaining.extend(notices_iter);
                ctx.effect_task
                    .retain_notice(crate::effect_task::BatchResult { notices: remaining });
                return Ok(());
            }
        }
        // The envelope, rather than the number of effects, marks completion.
        // In particular, AbortBatch may return a single failure notice.
        ctx.worker_batch_in_flight = false;
        worker_completed = true;
    }

    // Worker notices may include async kicks in future executor variants.
    // Route every kick through the shared production sink before reducing the
    // completion events; no event source may leave a kick stranded in Runtime.
    if worker_completed || replayed_notices {
        for kick in runner.borrow_mut().take_kicks() {
            track_kick_shared(ctx, kick)?;
        }
    }
    if let Some(batch) = ctx.pending_effect_batch.take() {
        match ctx.effect_task.try_submit_batch(batch) {
            Ok(()) => ctx.worker_batch_in_flight = true,
            Err((batch, crate::effect_task::EffectTaskError::QueueFull)) => {
                ctx.pending_effect_batch = Some(batch)
            }
            Err((_, crate::effect_task::EffectTaskError::Disconnected)) => {
                anyhow::bail!("effect worker disconnected while submitting pending batch")
            }
        }
    }
    admit_pending_app_events(runner, ctx);
    if worker_completed {
        // A worker completion is itself a state-machine input. Reduce its
        // derived batches in this service pass so no unrelated external event
        // is required to make progress. This is deliberately one-way:
        // `reduce_effect_batches` never services the worker recursively.
        reduce_effect_batches(runner, ctx)?;
    }
    Ok(())
}

fn reduce_effect_batches(
    runner: &std::rc::Rc<std::cell::RefCell<app_runner::AppRunner>>,
    ctx: &mut DeviceContext<'_>,
) -> anyhow::Result<()> {
    loop {
        if ctx.pending_effect_batch.is_some() || ctx.worker_batch_in_flight {
            break;
        }
        let batches = match runner.borrow_mut().reduce_next() {
            Some(batches) => batches,
            None => break,
        };
        // Queue the whole event's batches: one event can yield several
        // ordered batches, and a worker-safe one pauses the chain (the effect
        // task runs a single batch at a time). Running the rest of them from
        // a queue - instead of returning out of this loop - is what keeps a
        // Tick's render batch from being dropped behind its reminder fact
        // batch, which silently froze the on-screen clock.
        ctx.pending_effect_batches.extend(batches);
        while ctx.pending_effect_batch.is_none() && !ctx.worker_batch_in_flight {
            let Some(batch) = ctx.pending_effect_batches.pop_front() else {
                break;
            };
            if inkwash_logic::runner::batch_is_worker_safe(&batch) {
                match ctx.effect_task.try_submit_batch(batch) {
                    Ok(()) => {
                        ctx.worker_batch_in_flight = true;
                        break;
                    }
                    Err((batch, crate::effect_task::EffectTaskError::QueueFull)) => {
                        // Retry the head batch before anything queued after it.
                        ctx.pending_effect_batches.push_front(batch);
                        break;
                    }
                    Err((_, crate::effect_task::EffectTaskError::Disconnected)) => {
                        anyhow::bail!("effect worker disconnected while submitting batch")
                    }
                }
            }
            let last_clock = runner.borrow().last_clock();
            let mut executor = app_runner::EffectRunner::new(ctx, last_clock);
            let notices = inkwash_logic::runner::execute_batch(batch, &mut executor);
            let replies = executor.take_replies();
            drop(executor);
            deliver_effect_replies(ctx, replies);
            if let Err(notices) = runner.borrow_mut().submit_notices(notices) {
                ctx.pending_effect_notices = Some(notices);
                return Ok(());
            }
            for kick in runner.borrow_mut().take_kicks() {
                track_kick_shared(ctx, kick)?;
            }
        }
    }
    Ok(())
}

fn deliver_effect_replies(
    ctx: &mut DeviceContext<'_>,
    replies: Vec<(
        inkwash_logic::protocol::Channel,
        inkwash_logic::protocol::Reply,
    )>,
) {
    for (channel, reply) in replies {
        let session_id = match channel {
            control::Channel::Usb => ctx.usb_session_id,
            control::Channel::Ble => ctx
                .ble_connection_generation
                .or_else(|| {
                    ctx.pending_ble_handoff
                        .as_ref()
                        .map(|handoff| handoff.generation)
                })
                .unwrap_or(0),
        };
        let Some((id, command)) = ctx
            .command_sessions
            .pending_key(channel, session_id)
            .map(|(id, command)| (id.map(str::to_owned), command.clone()))
        else {
            log::warn!("reply effect has no pending correlation for {channel:?}");
            continue;
        };
        if deliver_protocol_reply(ctx, channel, session_id, id.as_deref(), &command, &reply) {
            if let Some(id) = id {
                ctx.command_sessions
                    .complete_terminal(channel, session_id, id, command, reply);
            } else {
                ctx.command_sessions
                    .complete_untagged_terminal(channel, session_id, command, reply);
            }
        }
    }
}

fn queue_ble_delivery(
    ctx: &mut DeviceContext<'_>,
    session_id: u64,
    generation: u64,
    conn_handle: u16,
    id: Option<String>,
    command: control::Command,
    reply: control::Reply,
) -> bool {
    if ctx.pending_ble_replies.len() + ctx.pending_ble_deliveries.len()
        >= ctx::BLE_REPLY_PENDING_CAPACITY
    {
        let delivery = ctx::PendingBleDelivery {
            session_id,
            generation,
            conn_handle,
            id,
            command,
            reply,
        };
        if ctx.pending_ble_reply_latch.is_none() {
            ctx.pending_ble_reply_latch = Some(delivery);
            return true;
        }
        log::error!(
            "BLE reply mailbox and producer latch full (capacity {}); rejecting delivery",
            ctx::BLE_REPLY_PENDING_CAPACITY
        );
        return false;
    }
    ctx.pending_ble_deliveries
        .push_back(ctx::PendingBleDelivery {
            session_id,
            generation,
            conn_handle,
            id,
            command,
            reply,
        });
    true
}

fn deliver_ble_reply(
    ctx: &mut DeviceContext<'_>,
    session_id: u64,
    generation: u64,
    conn_handle: u16,
    id: Option<String>,
    command: control::Command,
    reply: control::Reply,
) -> Option<u64> {
    if ctx.pending_ble_replies.len() + ctx.pending_ble_deliveries.len()
        >= ctx::BLE_REPLY_PENDING_CAPACITY
    {
        if !queue_ble_delivery(
            ctx,
            session_id,
            generation,
            conn_handle,
            id.clone(),
            command.clone(),
            reply.clone(),
        ) {
            terminate_ble_set_wifi_delivery(ctx, generation, id.as_deref(), &command);
            ctx.command_sessions.cancel_pending(
                control::Channel::Ble,
                generation,
                id.as_deref(),
                &command,
            );
        }
        return None;
    }
    // Reserve an owned delivery record before enqueueing into the worker.
    // QueueFull can then leave this exact request in the bounded mailbox,
    // while a successful enqueue atomically transfers it to notify tracking.
    ctx.pending_ble_deliveries
        .push_back(ctx::PendingBleDelivery {
            session_id,
            generation,
            conn_handle,
            id: id.clone(),
            command: command.clone(),
            reply: reply.clone(),
        });
    match ctx
        .ble_control
        .write_reply(&reply, id.as_deref(), session_id, generation, conn_handle)
    {
        Ok(reply_id) => {
            let delivery = ctx
                .pending_ble_deliveries
                .pop_back()
                .expect("BLE delivery reservation");
            ctx.pending_ble_replies.push((
                session_id,
                generation,
                conn_handle,
                reply_id,
                delivery.id,
                delivery.command,
                delivery.reply,
            ));
            Some(reply_id)
        }
        Err(ble_control::BleReplyError::QueueFull) => None,
        Err(ble_control::BleReplyError::Disconnected) => {
            let delivery = ctx
                .pending_ble_deliveries
                .pop_back()
                .expect("BLE delivery reservation");
            log::warn!("BLE reply delivery disconnected; terminating request");
            terminate_ble_set_wifi_delivery(
                ctx,
                generation,
                delivery.id.as_deref(),
                &delivery.command,
            );
            ctx.command_sessions.cancel_pending(
                control::Channel::Ble,
                generation,
                delivery.id.as_deref(),
                &delivery.command,
            );
            None
        }
    }
}

fn terminate_ble_set_wifi_delivery(
    ctx: &mut DeviceContext<'_>,
    generation: u64,
    id: Option<&str>,
    command: &control::Command,
) {
    if !matches!(command, control::Command::SetWifi { .. }) {
        return;
    }
    ctx.pending_ble_handoff = None;
    ctx.pending_ble_set_wifi_ack = None;
    ctx.ble_set_wifi_after_resume = None;
    ctx.command_sessions
        .cancel_pending(control::Channel::Ble, generation, id, command);
}

fn retry_ble_deliveries(ctx: &mut DeviceContext<'_>) {
    if let Some(delivery) = ctx.pending_ble_reply_latch.take() {
        retry_one_ble_delivery(ctx, delivery);
    }
    let pending = std::mem::take(&mut ctx.pending_ble_deliveries);
    for delivery in pending {
        retry_one_ble_delivery(ctx, delivery);
    }
}

fn retry_one_ble_delivery(ctx: &mut DeviceContext<'_>, delivery: ctx::PendingBleDelivery) {
    if ctx.ble_session_id != Some(delivery.session_id)
        || ctx.ble_connection_generation != Some(delivery.generation)
        || ctx.ble_connection_handle != Some(delivery.conn_handle)
    {
        terminate_ble_set_wifi_delivery(
            ctx,
            delivery.generation,
            delivery.id.as_deref(),
            &delivery.command,
        );
        ctx.command_sessions.cancel_pending(
            control::Channel::Ble,
            delivery.generation,
            delivery.id.as_deref(),
            &delivery.command,
        );
        return;
    }
    let is_pairing_set_wifi = matches!(&delivery.command, control::Command::SetWifi { .. })
        && matches!(&delivery.reply, control::Reply::Ok)
        && matches!(
            ctx.app_runner.borrow().state().screen,
            inkwash_logic::app::Screen::BlePairing(_)
        );
    let reply_id = deliver_ble_reply(
        ctx,
        delivery.session_id,
        delivery.generation,
        delivery.conn_handle,
        delivery.id,
        delivery.command,
        delivery.reply,
    );
    if is_pairing_set_wifi {
        ctx.pending_ble_set_wifi_ack = reply_id;
    }
}

fn mark_pairing_set_wifi_reply(
    ctx: &mut DeviceContext<'_>,
    command: &control::Command,
    reply: &control::Reply,
    reply_id: Option<u64>,
) {
    if reply_id.is_some()
        && matches!(reply, control::Reply::Ok)
        && matches!(command, control::Command::SetWifi { .. })
        && matches!(
            ctx.app_runner.borrow().state().screen,
            inkwash_logic::app::Screen::BlePairing(_)
        )
    {
        ctx.pending_ble_set_wifi_ack = reply_id;
    }
}

fn deliver_protocol_reply(
    ctx: &mut DeviceContext<'_>,
    channel: control::Channel,
    session_id: u64,
    id: Option<&str>,
    command: &control::Command,
    reply: &control::Reply,
) -> bool {
    match channel {
        control::Channel::Usb => {
            if let Err(err) = ctx.queue_usb_reply(session_id, id, command, reply) {
                log::error!("USB reply could not be retained for session {session_id}: {err}");
                ctx.command_sessions
                    .cancel_pending(channel, session_id, id, command);
            }
            false
        }
        control::Channel::Ble => {
            let Some((worker_session_id, reply_generation, conn_handle)) = ctx
                .ble_connection_generation
                .zip(ctx.ble_connection_handle)
                .map(|(generation, handle)| (ctx.ble_session_id.unwrap_or(0), generation, handle))
                .or_else(|| {
                    ctx.pending_ble_handoff.as_ref().map(|handoff| {
                        (handoff.session_id, handoff.generation, handoff.conn_handle)
                    })
                })
            else {
                ctx.command_sessions
                    .cancel_pending(channel, session_id, id, command);
                return false;
            };
            if ctx.ble_connection_generation.is_none() {
                if let Some(handoff) = ctx.pending_ble_handoff.take() {
                    ctx.command_sessions.cancel_pending(
                        channel,
                        handoff.generation,
                        handoff.id.as_deref(),
                        &handoff.command,
                    );
                    ctx.command_sessions.end(channel, handoff.generation);
                    log::warn!(
                        "BLE SetWifi terminal reply terminated after radio handoff (generation {})",
                        handoff.generation
                    );
                } else {
                    ctx.command_sessions
                        .cancel_pending(channel, session_id, id, command);
                }
                return false;
            }
            let reply_id = deliver_ble_reply(
                ctx,
                worker_session_id,
                reply_generation,
                conn_handle,
                id.map(str::to_owned),
                command.clone(),
                reply.clone(),
            );
            mark_pairing_set_wifi_reply(ctx, command, reply, reply_id);
            // BLE completion is reported by ReplyDelivered after the
            // notify-tx callback, so the command cache must remain pending.
            false
        }
    }
}

/// Render kicks enter the EPD registry; sleep kicks are retained until the
/// next complete platform polling turn.
fn track_kick_shared(
    ctx: &mut DeviceContext<'_>,
    kick: app_runner::AsyncKick,
) -> anyhow::Result<()> {
    if kick.is_render() {
        if let Err(kick) = ctx.pending_renders.borrow_mut().register(kick) {
            if ctx.pending_render_retries.len()
                >= inkwash_logic::epd_registry::RENDER_REGISTRY_CAPACITY
            {
                let kick = *kick;
                let replacement_index = {
                    let pending = ctx.pending_render_retries.make_contiguous();
                    inkwash_logic::epd_registry::retry_replacement_index(pending, &kick)
                };
                if let Some(index) = replacement_index {
                    // The replaced entry was never submitted to EPD. Its
                    // screen/generation is rebuildable, so keep the newer
                    // request and avoid stranding an older retry forever.
                    ctx.pending_render_retries[index] = kick;
                } else {
                    // Every retained retry is newer than this request (or an
                    // equal-generation duplicate). Keeping those entries is
                    // the safe latest-generation policy; this stale request
                    // has no hardware completion to await.
                    log::debug!("dropping superseded render retry kick");
                }
            } else {
                ctx.pending_render_retries.push_back(*kick);
            }
        }
        return Ok(());
    }

    match &kick.effect {
        inkwash_logic::app::Effect::PrepareSleep { token, .. } => {
            // The runner emits this kick only after the platform wake-source
            // preparation returned Ok; retain that fact with its exact token
            // and prepare operation for the collector.
            ctx.prepared_wake_plan = Some(ctx::PreparedWakePlan {
                token: *token,
                prepare_operation_id: kick.operation_id,
                commit_operation_id: None,
            });
            ctx.pending_sleep_kick = Some(kick);
        }
        inkwash_logic::app::Effect::CommitSleep(token) => {
            if let Some(plan) = ctx.prepared_wake_plan.as_mut() {
                if plan.token == *token {
                    plan.commit_operation_id = Some(kick.operation_id);
                    ctx.pending_sleep_kick = Some(kick);
                }
            }
        }
        _ => {}
    }
    Ok(())
}

fn retry_render_registry(ctx: &mut DeviceContext<'_>) -> anyhow::Result<()> {
    while let Some(kick) = ctx.pending_render_retries.pop_front() {
        match ctx.pending_renders.borrow_mut().register(kick) {
            Ok(()) => {}
            Err(kick) => {
                ctx.pending_render_retries.push_front(*kick);
                break;
            }
        }
    }
    Ok(())
}

fn wake_plan_confirmed_for(
    ctx: &DeviceContext<'_>,
    token: inkwash_logic::power_state::SleepToken,
    operation_id: inkwash_logic::app::OperationId,
) -> bool {
    ctx.prepared_wake_plan.is_some_and(|plan| {
        plan.token == token
            && (plan.prepare_operation_id == operation_id
                || plan.commit_operation_id == Some(operation_id))
    })
}

fn collect_sleep_inputs(
    ctx: &DeviceContext<'_>,
    wake_plan_confirmed: bool,
) -> inkwash_logic::power_state::SleepInputs {
    let input_pending = ctx.board.key_enter.is_raw_pressed()
        || ctx.board.key_up.is_raw_pressed()
        || ctx.board.key_down.is_raw_pressed();
    let runner = ctx.app_runner.borrow();
    let state = runner.state();
    let worker_in_flight = ctx.pending_effect_batch.is_some()
        || !ctx.pending_effect_batches.is_empty()
        || ctx.worker_batch_in_flight
        || ctx.pending_effect_notices.is_some();
    let pending_usb_reply = !ctx.pending_usb_replies.is_empty();
    let pending_usb_reply_latch = ctx.pending_usb_reply_latch.is_some();
    let pending_render_retry = !ctx.pending_render_retries.is_empty();
    let pending_ble_reply = !ctx.pending_ble_replies.is_empty()
        || !ctx.pending_ble_deliveries.is_empty()
        || ctx.pending_ble_reply_latch.is_some();
    let rtc_read_in_flight = ctx.pending_alarm_status.is_some()
        || ctx.pending_alarm_snapshot.is_some()
        || ctx.pending_clock_read.is_some();
    inkwash_logic::power_state::SleepInputs {
        input_pending,
        page_allows_sleep: matches!(state.screen, inkwash_logic::app::Screen::Home)
            || state.requested_sleep.is_some(),
        final_display_pending: !ctx.pending_renders.borrow().pending.is_empty()
            || pending_render_retry
            || ctx.pending_render_completion.is_some(),
        final_persist_pending: worker_in_flight,
        network_in_flight: ctx.pending_wifi_op.is_some()
            || !matches!(state.sync, inkwash_logic::app::SyncState::Idle),
        network_resumable: false,
        protocol_reply_pending: state.pending_usb_reply.is_some()
            || state.pending_ble_reply.is_some()
            || pending_usb_reply
            || pending_usb_reply_latch
            || pending_ble_reply
            || ctx.pending_ble_pairing_success.is_some(),
        operation_in_flight: !state.rtc_alarm_plan_confirmed()
            || !matches!(state.sync, inkwash_logic::app::SyncState::Idle)
            || state.urgent_poll_in_flight()
            || worker_in_flight
            || rtc_read_in_flight
            || pending_usb_reply
            || pending_usb_reply_latch
            || pending_ble_reply
            || ctx.pending_ble_handoff.is_some()
            || pending_render_retry
            || ctx.pending_sleep_kick.is_some()
            || ctx.pending_dispatch_event.is_some()
            || !ctx.pending_dispatch_sources.is_empty(),
        rtc_plan_confirmed: state.rtc_alarm_plan_confirmed(),
        wake_plan_confirmed,
        usb_connected: power::usb_host_connected(),
        event_queue_empty: !runner.has_work()
            && !worker_in_flight
            && !pending_usb_reply
            && !pending_usb_reply_latch
            && !pending_render_retry
            && !pending_ble_reply
            && !rtc_read_in_flight
            && ctx.pending_sleep_kick.is_none()
            && ctx.pending_app_events.is_empty()
            && ctx.pending_dispatch_event.is_none()
            && ctx.pending_dispatch_sources.is_empty(),
        input_latch_clear: !input_pending,
    }
}

/// Translate one EPD completion into the matching `EffectCompleted` /
/// `EffectFailed` and dispatch it back through the runtime. The kick is
/// consumed (removed from `pending_renders` by the caller before this is
/// called) so the generation / op id line up.
fn feed_completion_back(
    runner: &std::rc::Rc<std::cell::RefCell<app_runner::AppRunner>>,
    ctx: &mut DeviceContext<'_>,
    kick: app_runner::AsyncKick,
    output: inkwash_logic::app::EffectOutput,
    failure: Option<inkwash_logic::app::EffectError>,
) -> anyhow::Result<()> {
    let event = match failure {
        Some(error) => inkwash_logic::app::Event::EffectFailed(inkwash_logic::app::EffectFailure {
            batch_id: kick.batch_id,
            effect_id: kick.effect_id,
            operation_id: kick.operation_id,
            render_generation: kick.render_generation,
            error,
        }),
        None => inkwash_logic::app::Event::EffectCompleted(inkwash_logic::app::EffectCompletion {
            batch_id: kick.batch_id,
            effect_id: kick.effect_id,
            operation_id: kick.operation_id,
            render_generation: kick.render_generation,
            output,
        }),
    };
    dispatch_or_retain(runner, event, ctx)
}
