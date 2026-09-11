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

const IDLE_POLL_INTERVAL_MS: u64 = 1000;

const IDLE_ENTER_AFTER: Duration = Duration::from_millis(2000);

const CLOCK_POLL_INTERVAL: Duration = Duration::from_millis(1200);

const IDLE_CLOCK_POLL_INTERVAL: Duration = Duration::from_secs(10);

const RTC_READ_SETTLE_TIMEOUT: Duration = Duration::from_millis(50);
const SAFE_MODE_REPLY_CAPACITY: usize = usb_console::REPLY_WRITER_CAPACITY;

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

    let mut usb_console = usb_console::UsbConsole::start();
    let mut usb_reply_writer = UsbReplyWriter::start()?;

    let rtc = match rtc_executor::RtcExecutor::spawn(board.i2c_bus.clone()) {
        Ok(rtc) => rtc,
        Err(err) => run_safe_mode(
            &mut board,
            &mut usb_console,
            &mut usb_reply_writer,
            &format!("RTC executor start failed: {err}"),
        ),
    };

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

    let boot_core = match collect_boot_core_facts(&rtc, &alarm_store, clock, core_failure) {
        Ok(facts) => facts,
        Err(reason) => {
            run_safe_mode(&mut board, &mut usb_console, &mut usb_reply_writer, &reason);
        }
    };

    if option_env!("INKWASH_FORCE_SAFE_MODE").is_some_and(|v| v == "1") {
        run_safe_mode(
            &mut board,
            &mut usb_console,
            &mut usb_reply_writer,
            "forced by INKWASH_FORCE_SAFE_MODE=1 (test hook)",
        );
    }

    if board.nfc.is_none() {
        log::warn!("NFC not available");
    }

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

    let sysloop = esp_idf_svc::eventloop::EspSystemEventLoop::take()?;

    let mut wifi_mgr = wifi::WifiManager::new(&sysloop)?;

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

    let boot_result = collect_boot_snapshot(boot_core, &counters, &todo_store, &inbox_store, clock);
    let scheduler_config = inkwash_logic::app::SyncSchedulerConfig {
        now_unix: clock.map(|now| now.to_unix()).unwrap_or(0),
        interval_minutes: counters.sync_interval_minutes().unwrap_or(60),
        last_sync_epoch: counters.last_sync_epoch().unwrap_or(None),
    };

    let sync_task = sync_task::SyncTask::spawn(sync_partition, wifi_mgr)?;

    let effect_task = effect_task::EffectTask::spawn(effect_task::EffectDrivers {
        alarm_store: effect_alarm_store,
        todo_store: effect_todo_store,
        inbox_store: effect_inbox_store,
        counters: effect_counters,
        rtc: rtc.clone(),
    })?;

    let mut ble_control = ble_control::BleControl::spawn()?;

    let app_runner = std::rc::Rc::new(std::cell::RefCell::new(app_runner::AppRunner::new()));

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

        app_runner_enabled: true,
    };

    ctx.command_sessions
        .begin(control::Channel::Usb, ctx.usb_session_id);

    app_runner.borrow_mut().set_last_clock(clock);
    let mut boot_dispatched = false;
    let mut boot_result = Some(boot_result);
    let mut scheduler_config = Some(scheduler_config);

    let app_runner_enabled = true;

    let mut idle = false;
    let mut light_sleep_enabled = false;
    let power_ticks_origin = Instant::now();
    let mut last_activity = Instant::now();
    let mut status_last = Instant::now();
    let mut clock_last = Instant::now();
    let mut usb_host_was_connected = false;

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
        }
        let now = Instant::now();

        if now.duration_since(status_last) >= Duration::from_secs(1) {
            status_last = now;
            if let Err(err) = report_power_state(ctx.board) {
                log::warn!("Power status probe failed: {err}");
            }
        }

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

        if ctx.poll_alarm_snapshot()? {
            let _ = ctx
                .alarm_poll
                .consume_for(inkwash_logic::alarm_flow::AlarmSource::Home);
        }

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

        let (_usb_changed, usb_activity) = ctx.poll_usb_control(clock.as_ref())?;

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

        let mut ble_changed = poll_ble_lifecycle(&app_runner, &mut ctx)?;
        if let Some((_reply_id, result)) = ctx.pending_ble_pairing_success.take() {
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

        while let Some(completion) = ctx.board.display.poll_completion() {
            let outcome = {
                let mut reg = ctx.pending_renders.borrow_mut();
                reg.feed(completion.request_id, completion.ok, completion.superseded)
            };
            match outcome {
                inkwash_logic::epd_registry::FeedOutcome::Matched(kick, terminal) => match terminal
                {
                    inkwash_logic::epd_registry::RenderTerminal::Superseded => {
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
                            let current_generation = app_runner.borrow().state().render_generation;
                            app_runner::apply_render_cache_terminal(
                                &ctx.pending_renders,
                                &retry.kick,
                                failed,
                                current_generation,
                            );
                        }
                    }
                },
                inkwash_logic::epd_registry::FeedOutcome::Ignored => {}
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

        ctx.poll_wifi_ops()?;

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

        let any_key_pressed = ctx.board.key_enter.is_raw_pressed()
            || ctx.board.key_up.is_raw_pressed()
            || ctx.board.key_down.is_raw_pressed();

        let user_activity = usb_activity || ble_changed || key_changed || any_key_pressed;
        let interacted = user_activity;

        if let Some(kick) = ctx.pending_sleep_kick.take() {
            ctx.settle_sleep_reads(RTC_READ_SETTLE_TIMEOUT);
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

            let fact_reduced = ctx.pending_app_events.is_empty()
                && ctx.pending_dispatch_event.is_none()
                && ctx.pending_dispatch_sources.is_empty()
                && !runner.borrow().has_work();
            if dispatched
                && fact_reduced
                && (committed_or_cancelled || (prepare_fact && ctx.pending_sleep_kick.is_none()))
            {
                ctx.prepared_wake_plan = None;
            }
        }

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
                    && ctx.pending_sleep_kick.is_none()
                    && ctx.pending_app_events.is_empty()
                    && ctx.pending_dispatch_event.is_none()
                    && ctx.pending_dispatch_sources.is_empty(),
                input_latch_clear: !any_key_pressed,
                wake_plan_confirmed: false,

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
            light_sleep_enabled = true;
        } else if !light_sleep_committed && light_sleep_enabled {
            light_sleep_enabled = false;
        }
        if interacted || any_key_pressed {
            last_activity = now;
            idle = false;
        } else if now.duration_since(last_activity) >= IDLE_ENTER_AFTER {
            idle = true;
        }

        if light_sleep_committed {
            ctx.board.wake.arm();
            if ctx.board.wake.wait(IDLE_POLL_INTERVAL_MS as u32) {
                log::info!("Idle wait woken early (key pressed)");
            }
        } else {
            thread::sleep(Duration::from_millis(POLL_INTERVAL_MS as u64));
        }
    }
}

fn run_safe_mode(
    board: &mut Note4Board,
    usb_console: &mut crate::usb_console::UsbConsole,
    usb_reply_writer: &mut UsbReplyWriter,
    reason: &str,
) -> ! {
    log::error!("Entering minimum safe mode (core boot fact unavailable: {reason})");

    {
        let mut canvas = board.display.canvas_mut();
        canvas.clear();
        crate::ui::header(&mut canvas, "SAFE MODE");
        canvas.draw_text_prop(8, 60, 1, "CORE DATA UNAVAILABLE");

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

    loop {
        watchdog::feed();
        service_safe_mode_replies(usb_reply_writer, &mut pending_replies);

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

        board.key_enter.poll();
        board.key_up.poll();
        board.key_down.poll();

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

pub(crate) fn dispatch_app_runner(
    runner: &std::rc::Rc<std::cell::RefCell<app_runner::AppRunner>>,
    event: inkwash_logic::app::Event,
    ctx: &mut DeviceContext<'_>,
) -> anyhow::Result<()> {
    if ctx.pending_dispatch_event.is_none() {
        ctx.pending_dispatch_event = Some(event);
    } else {
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
                return Err(anyhow::Error::new(DispatchSaturated { event }));
            }
            break;
        }
    }
    reduce_effect_batches(runner, ctx)
}

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

#[allow(clippy::result_large_err)]
fn retain_or_overflow_dispatch_event(
    ctx: &mut DeviceContext<'_>,
    event: inkwash_logic::app::Event,
) -> Result<(), DispatchSaturated> {
    if let Err(event) = retain_pending_app_event(ctx, event) {
        if let Err(event) = ctx.pending_dispatch_sources.retain(event) {
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

                log::warn!(
                    "effect notice retained: runtime queue full; reducing queued events to make room"
                );
                return Ok(());
            }
        }

        ctx.worker_batch_in_flight = false;
        worker_completed = true;
    }

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
        reduce_effect_batches(runner, ctx)?;
    }
    Ok(())
}

fn reduce_effect_batches(
    runner: &std::rc::Rc<std::cell::RefCell<app_runner::AppRunner>>,
    ctx: &mut DeviceContext<'_>,
) -> anyhow::Result<()> {
    let notice_retained = ctx.effect_task.has_retained_notice();
    loop {
        if ctx.pending_effect_batch.is_some() {
            break;
        }
        if ctx.worker_batch_in_flight && !notice_retained {
            break;
        }
        let batches = match runner.borrow_mut().reduce_next() {
            Some(batches) => batches,
            None => break,
        };

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

            false
        }
    }
}

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
                    ctx.pending_render_retries[index] = kick;
                } else {
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
            && ctx.pending_sleep_kick.is_none()
            && ctx.pending_app_events.is_empty()
            && ctx.pending_dispatch_event.is_none()
            && ctx.pending_dispatch_sources.is_empty(),
        input_latch_clear: !input_pending,
    }
}

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
