//! Pure, hardware-independent firmware business logic, split out of
//! `rust-firmware` so it can be unit-tested on the host - plain
//! `cargo test` from this directory - without the ESP-IDF/xtensa toolchain
//! or any hardware attached.
//!
//! `rust-firmware` cross-compiles only for `xtensa-esp32s3-espidf` and links
//! against `esp-idf-sys`, whose build script shells out to `idf.py` and the
//! ESP-IDF SDK; that dependency can't be built for a host target at all, so
//! before this split, no part of the firmware crate's logic could be tested
//! anywhere but on the physical device. This crate has no ESP-IDF
//! dependency (just `serde`), so it builds and tests on any host.
//!
//! `rust-firmware` depends on this crate by path and re-exports each type
//! from its usual module (`rtc::DateTime`, `alarms::{Repeat, StoredAlarm}`,
//! `sync::{validate_repeat, validate_date}`, ...) so nothing calling into
//! them needs to change - this crate is the single source of truth for the
//! logic itself; the firmware modules add the hardware-facing parts (NVS
//! storage, I2C, display, buttons) around it.

pub mod alarm_flow;
pub mod alarm_regs;
pub mod alarm_schedule;
pub mod app;
pub mod audio_command;
pub mod ble_memory;
pub mod ble_radio;
pub mod button_event;
pub mod command_sessions;
pub mod datetime;
pub mod device_config;
pub mod epd_registry;
pub mod event_queue;
pub mod harness;
pub mod inbox_item;
pub mod list_window;
pub mod power_state;
pub mod protocol;
pub mod reminder_dedup;
pub mod render_plan;
pub mod rtc_latch;
pub mod runner;
pub mod runtime;
pub mod scheduler;
pub mod sync_validate;
pub mod todo;
pub mod wake_cause;
pub mod worker_heartbeat;

#[cfg(test)]
mod ble_memory_contract {
    // Keep the host-only guard next to the host-testable state-machine tests.
    // The hardware crate cannot be built for the host, but these invariants
    // must stay coupled to its ESP-IDF memory configuration.
    const BLE_SOURCE: &str = include_str!("../../rust-firmware/src/ble_control.rs");
    const SDKCONFIG: &str = include_str!("../../rust-firmware/sdkconfig.defaults");

    fn config_is(name: &str, value: &str) -> bool {
        SDKCONFIG
            .lines()
            .any(|line| line.trim() == format!("CONFIG_{name}={value}"))
    }

    #[test]
    fn ble_worker_uses_internal_stack_and_checks_internal_heap_before_init() {
        assert!(BLE_SOURCE.contains("const BLE_TASK_STACK: usize = 16 * 1024"));
        assert!(BLE_SOURCE.contains("MALLOC_CAP_INTERNAL | esp_idf_svc::sys::MALLOC_CAP_8BIT"));
        assert!(!BLE_SOURCE.contains("MALLOC_CAP_SPIRAM"));
        assert!(BLE_SOURCE.contains("MALLOC_CAP_INTERNAL | esp_idf_svc::sys::MALLOC_CAP_DMA"));
        assert!(BLE_SOURCE.contains("heap_caps_get_free_size(BLE_INTERNAL_CAPS)"));
        assert!(BLE_SOURCE.contains("heap_caps_get_largest_free_block(BLE_INTERNAL_CAPS)"));
        assert!(BLE_SOURCE.contains("BLEDevice::init();"));
        assert!(BLE_SOURCE.contains("if !inkwash_logic::ble_memory::sufficient_internal_heap"));
        assert!(BLE_SOURCE.contains("log_stack_high_watermark(\"before BLE init\")"));
        assert!(BLE_SOURCE.contains("log_stack_high_watermark(\"after BLE init\")"));
    }

    #[test]
    fn ble_worker_rejects_duplicate_starts_without_dropping_active_session() {
        let start_guard = BLE_SOURCE
            .find("if session.is_some()")
            .expect("worker must guard duplicate Start commands");
        let start_init = BLE_SOURCE
            .find("match BleSession::start(session_id)")
            .expect("worker must initialize after duplicate Start guard");
        assert!(start_guard < start_init);
        assert!(BLE_SOURCE.contains("active_session_id"));
        assert!(BLE_SOURCE.contains("rejecting Start session"));
        assert!(BLE_SOURCE.contains("ignoring stale Stop session"));
    }

    #[test]
    fn ble_lifecycle_callbacks_retain_full_channel_observations() {
        assert!(BLE_SOURCE.contains("struct LifecycleSender"));
        assert!(BLE_SOURCE.contains("fn send_lifecycle"));
        assert!(BLE_SOURCE.contains("struct LifecycleMailbox"));
        assert!(BLE_SOURCE.contains("queue: VecDeque<SequencedLifecycle>"));
        assert!(BLE_SOURCE.contains("Condvar"));
        assert!(BLE_SOURCE.contains("while mailbox.queue.len() >= CHANNEL_CAPACITY"));
        assert!(!BLE_SOURCE.contains("latest: Option<SequencedLifecycle>"));
        assert!(BLE_SOURCE.contains("poll_lifecycle(&self.lifecycle_mailbox"));
        assert!(BLE_SOURCE.contains(
            "send_lifecycle(\n                &lc_tx,\n                BleLifecycle::Connected"
        ));
        assert!(BLE_SOURCE.contains(
            "send_lifecycle(\n                &lc_tx,\n                BleLifecycle::Disconnected"
        ));
        assert!(!BLE_SOURCE.contains("let _ = lc_tx.try_send(BleLifecycle"));
    }

    #[test]
    fn ble_notify_completion_is_attempt_bound_and_stale_safe() {
        assert!(BLE_SOURCE.contains("struct NotifyAttempt"));
        assert!(BLE_SOURCE.contains("attempt: NotifyAttempt"));
        assert!(BLE_SOURCE.contains("attempt_id: u64"));
        assert!(BLE_SOURCE.contains("const BLE_REPLY_MAX_RETRIES: u8 = 3"));
        assert!(BLE_SOURCE.contains("ReplyTerminated"));
        assert!(BLE_SOURCE.contains("retired_handles: [u64; 1024]"));
        assert!(BLE_SOURCE.contains("fn release_generation"));
        assert!(BLE_SOURCE.contains("self.is_retired(attempt.conn_handle)"));
        assert!(BLE_SOURCE.contains("self.retire_handle(conn_handle)"));
        assert!(BLE_SOURCE.contains("late callback can\n    /// never consume"));
        assert!(BLE_SOURCE.contains("pending.push_back(event)"));
        assert!(!BLE_SOURCE.contains("pending: StdArc<StdMutex<Option<NotifyTxEvent>>>"));
        assert!(BLE_SOURCE.contains("inflight.attempt_id != event.attempt.attempt_id"));
        assert!(BLE_SOURCE.contains("BLE stale notify-tx callback ignored"));
        assert!(BLE_SOURCE.contains("ReplyTerminated"));
        assert!(BLE_SOURCE.contains("pending_command: Option<BleCommand>"));
    }

    #[test]
    fn ble_main_lifecycle_precedes_results_and_filters_stale_events() {
        const MAIN_SOURCE: &str = include_str!("../../rust-firmware/src/main.rs");
        const CTX_SOURCE: &str = include_str!("../../rust-firmware/src/ctx.rs");
        let lifecycle_poll = MAIN_SOURCE
            .find("let mut ble_changed = poll_ble_lifecycle")
            .expect("main must reduce lifecycle before worker results");
        let result_poll = MAIN_SOURCE
            .find("while let Some(result) = ctx.ble_control.poll_result()")
            .expect("main must poll worker results");
        assert!(lifecycle_poll < result_poll);
        assert!(MAIN_SOURCE.contains("Ignoring stale BLE connect session"));
        assert!(MAIN_SOURCE.contains("Ignoring stale BLE disconnect session"));
        assert!(MAIN_SOURCE.contains("abort_ble_set_wifi_handoff"));
        assert!(MAIN_SOURCE.contains("if let Err(err) = ctx.abort_ble_set_wifi_handoff()"));
        assert!(MAIN_SOURCE.contains("pending_render_completion.is_some()"));
        assert!(MAIN_SOURCE.contains("pending_app_events.is_empty()"));
        assert!(MAIN_SOURCE.contains("ctx.pending_sleep_kick = Some(kick)"));
        assert!(MAIN_SOURCE.contains("replayed_notices"));
        assert!(CTX_SOURCE.contains("queue_usb_reply"));
        assert!(CTX_SOURCE.contains("let reply = Reply::Busy"));
        assert!(CTX_SOURCE.contains("self.ble_wifi_suspended = false"));
        assert!(CTX_SOURCE.contains("self.ble_session_id = None"));
    }

    #[test]
    fn dispatch_saturation_uses_source_latches_instead_of_a_central_overflow() {
        const MAIN_SOURCE: &str = include_str!("../../rust-firmware/src/main.rs");
        const CTX_SOURCE: &str = include_str!("../../rust-firmware/src/ctx.rs");
        assert!(CTX_SOURCE.contains("pub struct PendingDispatchSources"));
        assert!(CTX_SOURCE.contains("BUTTON_PENDING_CAPACITY"));
        assert!(CTX_SOURCE.contains("WORKER_PENDING_CAPACITY"));
        assert!(MAIN_SOURCE.contains("pending_dispatch_sources.take_next()"));
        assert!(MAIN_SOURCE.contains("pub(crate) struct DispatchSaturated"));
        assert!(MAIN_SOURCE.contains("pub event: inkwash_logic::app::Event"));
        assert!(MAIN_SOURCE.contains("dispatch_or_retain"));
        assert!(!MAIN_SOURCE.contains("pending_dispatch_return"));
        assert!(!MAIN_SOURCE.contains("source dispatch latch saturated; retaining current event"));
        assert!(!MAIN_SOURCE.contains("pending_dispatch_overflow"));
        assert!(!MAIN_SOURCE.contains("application dispatch mailbox is saturated"));
    }

    #[test]
    fn dispatch_over_capacity_returns_the_event_to_the_producer() {
        const MAIN_SOURCE: &str = include_str!("../../rust-firmware/src/main.rs");
        const CTX_SOURCE: &str = include_str!("../../rust-firmware/src/ctx.rs");
        assert!(CTX_SOURCE.contains("if self.buttons.len() >= BUTTON_PENDING_CAPACITY"));
        assert!(CTX_SOURCE.contains("if self.lifecycle.len() >= LIFECYCLE_PENDING_CAPACITY"));
        assert!(CTX_SOURCE.contains("if self.worker.len() >= WORKER_PENDING_CAPACITY"));
        assert!(CTX_SOURCE.contains("if self.sleep.len() >= SLEEP_PENDING_CAPACITY"));
        assert!(MAIN_SOURCE.contains("return Err(DispatchSaturated { event })"));
        assert!(MAIN_SOURCE
            .contains("Err(event) => Err(anyhow::Error::new(DispatchSaturated { event }))"));
        assert!(MAIN_SOURCE.contains("dispatch_or_retain(runner, event, ctx)"));
    }

    #[test]
    fn transport_reply_paths_retain_owned_frames_and_bound_failures() {
        const MAIN_SOURCE: &str = include_str!("../../rust-firmware/src/main.rs");
        const CTX_SOURCE: &str = include_str!("../../rust-firmware/src/ctx.rs");
        const SESSIONS_SOURCE: &str = include_str!("../src/command_sessions.rs");
        assert!(CTX_SOURCE.contains("pending.reply == *reply"));
        assert!(CTX_SOURCE.contains("pub const USB_REPLY_PENDING_CAPACITY: usize = 32"));
        assert!(CTX_SOURCE.contains("const USB_REPLY_MAX_RETRIES: u8 = 3"));
        assert!(CTX_SOURCE.contains("fn cancel_usb_reply"));
        assert!(CTX_SOURCE.contains("retry_count = pending.retry_count.saturating_add(1)"));
        assert!(MAIN_SOURCE.contains("fn queue_ble_delivery"));
        assert!(MAIN_SOURCE.contains("BLE_REPLY_PENDING_CAPACITY"));
        assert!(MAIN_SOURCE.contains("Reserve an owned delivery record before enqueueing"));
        assert!(MAIN_SOURCE.contains("BleReplyError::QueueFull"));
        assert!(MAIN_SOURCE.contains("deliver_ble_reply("));
        assert!(MAIN_SOURCE.contains("ctx.command_sessions.cancel_pending"));
        assert!(CTX_SOURCE.contains("pub const BLE_REPLY_PENDING_CAPACITY: usize = 32"));
        assert!(MAIN_SOURCE.contains("BLE reply mailbox and producer latch full"));
        assert!(MAIN_SOURCE.contains("pending_ble_reply_latch"));
        assert!(CTX_SOURCE.contains("pending_usb_reply_latch"));
        assert!(MAIN_SOURCE.contains("terminate_ble_set_wifi_delivery"));
        assert!(!MAIN_SOURCE.contains("pending_ble_overflow_retry"));
        assert!(!CTX_SOURCE.contains("pending_usb_terminal_retry"));
        assert!(MAIN_SOURCE.contains("pending_ble_handoff"));
        assert!(MAIN_SOURCE.contains("begin_preserving_pending"));
        assert!(MAIN_SOURCE.contains("pending_ble_reply_latch"));
        assert!(CTX_SOURCE.contains("pending_usb_reply_latch"));
        assert!(MAIN_SOURCE.contains("BLE SetWifi terminal reply terminated after radio handoff"));
        assert!(SESSIONS_SOURCE.contains("Busy/Pending is a transport response"));
    }

    #[test]
    fn ble_controller_memory_budget_is_peripheral_only() {
        assert!(config_is("BT_NIMBLE_ROLE_CENTRAL", "n"));
        assert!(config_is("BT_NIMBLE_ROLE_OBSERVER", "n"));
        assert!(config_is("BT_NIMBLE_MAX_CONNECTIONS", "1"));
        assert!(config_is("BT_NIMBLE_50_FEATURE_SUPPORT", "n"));
        assert!(config_is("BT_CTRL_BLE_MAX_ACT", "2"));
        assert!(config_is("BT_CTRL_BLE_SCAN", "n"));
        assert!(config_is("BT_CTRL_DTM_ENABLE", "n"));
        assert!(config_is("BT_CTRL_RUN_IN_FLASH_ONLY", "y"));
    }

    #[test]
    fn firmware_owns_radio_arbitration_and_restores_wifi() {
        const SYNC_TASK: &str = include_str!("../../rust-firmware/src/sync_task.rs");
        const CTX: &str = include_str!("../../rust-firmware/src/ctx.rs");
        const APP_RUNNER: &str = include_str!("../../rust-firmware/src/app_runner.rs");
        assert!(SYNC_TASK.contains("SyncCommand::SuspendForBle"));
        assert!(SYNC_TASK.contains("SyncCommand::ResumeAfterBle"));
        assert!(SYNC_TASK.contains("wifi.suspend_for_ble()"));
        assert!(SYNC_TASK.contains("wifi.resume_after_ble()"));
        const WIFI: &str = include_str!("../../rust-firmware/src/wifi.rs");
        assert!(WIFI.contains("wifi: Option<EspWifi<'static>>"));
        assert!(WIFI.contains("self.wifi.take()"));
        assert!(WIFI.contains("fn ensure_driver"));
        assert!(WIFI.contains("EspWifi::new(modem, self.sysloop.clone(), None)"));
        assert!(CTX.contains("self.sync.suspend_for_ble()?"));
        assert!(CTX.contains("self.ble_control.start(&name, session_id)"));
        assert!(CTX.contains("self.sync.resume_after_ble()"));
        assert!(CTX.contains("ble_start_cancelled"));
        assert!(CTX.contains("self.resume_wifi_after_ble()"));
        assert!(APP_RUNNER.contains("start_ble_pairing(req)"));
        assert!(CTX.contains("pub fn ble_stopped"));
    }

    #[test]
    fn ble_cleanup_stops_advertising_before_deinit() {
        let stop = BLE_SOURCE
            .find("advertising.lock().stop()")
            .expect("cleanup must stop advertising explicitly");
        let deinit = BLE_SOURCE
            .find("BLEDevice::deinit_full()")
            .expect("cleanup must deinitialize NimBLE");
        assert!(stop < deinit, "advertising must stop before NimBLE deinit");
    }
}
