pub mod alarm_flow;
pub mod alarm_regs;
pub mod alarm_schedule;
pub mod app;
pub mod audio_command;
pub mod ble_memory;
pub mod ble_radio;
pub mod boot_guard;
pub mod boot_store;
pub mod button_event;
pub mod command_sessions;
pub mod datetime;
pub mod device_config;
pub mod diag;
pub mod epd_geometry;
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
pub mod sanitize;
pub mod scheduler;
pub mod sync_validate;
pub mod todo;
pub mod wake_cause;
pub mod worker_heartbeat;

#[cfg(test)]
mod ble_memory_contract {

    const BLE_SOURCE: &str = include_str!("../../rust-firmware/src/ble_control.rs");
    const EFFECT_SOURCE: &str = include_str!("../../rust-firmware/src/effect_task.rs");
    const TASKS_SOURCE: &str = include_str!("../../rust-firmware/src/tasks.rs");
    const SDKCONFIG: &str = include_str!("../../rust-firmware/sdkconfig.defaults");

    fn config_is(name: &str, value: &str) -> bool {
        SDKCONFIG
            .lines()
            .any(|line| line.trim() == format!("CONFIG_{name}={value}"))
    }

    #[test]
    fn ble_worker_uses_internal_stack_and_checks_internal_heap_before_init() {
        assert!(BLE_SOURCE.contains("const BLE_TASK_STACK: usize = 16 * 1024"));

        assert!(
            TASKS_SOURCE.contains("MALLOC_CAP_INTERNAL | esp_idf_svc::sys::MALLOC_CAP_8BIT"),
            "the shared spawner must pin worker stacks to internal RAM"
        );
        assert!(!TASKS_SOURCE.contains("MALLOC_CAP_SPIRAM"));
        assert!(
            TASKS_SOURCE.contains("esp_pthread_set_cfg(&default_cfg)"),
            "the spawner must restore the default pthread policy"
        );
        assert!(
            BLE_SOURCE.contains("crate::tasks::PRIORITY_BLE"),
            "the BLE worker must be spawned through the internal-stack helper"
        );
        assert!(BLE_SOURCE.contains("MALLOC_CAP_INTERNAL | esp_idf_svc::sys::MALLOC_CAP_DMA"));
        assert!(BLE_SOURCE.contains("heap_caps_get_free_size(BLE_INTERNAL_CAPS)"));
        assert!(BLE_SOURCE.contains("heap_caps_get_largest_free_block(BLE_INTERNAL_CAPS)"));
        assert!(BLE_SOURCE.contains("BLEDevice::init();"));
        assert!(BLE_SOURCE.contains("if !inkwash_logic::ble_memory::sufficient_internal_heap"));
        assert!(BLE_SOURCE.contains("log_stack_high_watermark(\"before BLE init\")"));
        assert!(BLE_SOURCE.contains("log_stack_high_watermark(\"after BLE init\")"));
    }

    #[test]
    fn effect_worker_uses_an_internal_stack_with_blob_headroom() {
        assert!(
            EFFECT_SOURCE.contains("crate::tasks::PRIORITY_EFFECT"),
            "the effect worker must be spawned through the internal-stack helper"
        );
        let stack = EFFECT_SOURCE
            .split("const EFFECT_TASK_STACK: usize = ")
            .nth(1)
            .and_then(|rest| rest.split(';').next())
            .expect("EFFECT_TASK_STACK must stay a literal so this budget is checkable");
        assert!(
            stack.contains("16 * 1024"),
            "effect task stack must stay at/above 16 KiB, found `{stack}`"
        );
        assert!(!EFFECT_SOURCE.contains("MALLOC_CAP_SPIRAM"));
    }

    #[test]
    fn ble_worker_thread_is_created_on_demand() {
        assert!(
            BLE_SOURCE.contains("fn ensure_worker"),
            "the BLE worker must be spawned lazily"
        );
        assert!(
            BLE_SOURCE.contains("worker_launch: Some(WorkerLaunch"),
            "spawn() must only prepare the channel ends, not start a thread"
        );
        let start_body = BLE_SOURCE
            .split("pub fn start(")
            .nth(1)
            .expect("BleControl::start must exist");
        assert!(
            start_body.contains("self.ensure_worker()"),
            "start() must create the worker thread on first use"
        );
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
    fn ble_lifecycle_callbacks_never_block_the_nimble_host() {
        assert!(BLE_SOURCE.contains("struct LifecycleSender"));
        assert!(BLE_SOURCE.contains("fn send_lifecycle"));
        assert!(BLE_SOURCE.contains("struct LifecycleMailbox"));
        assert!(BLE_SOURCE.contains("queue: VecDeque<SequencedLifecycle>"));
        assert!(BLE_SOURCE.contains("overflow_latest: Option<SequencedLifecycle>"));
        assert!(!BLE_SOURCE.contains("Condvar"));
        assert!(!BLE_SOURCE.contains("while mailbox.queue.len() >= CHANNEL_CAPACITY"));
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
    fn ble_control_characteristics_require_authenticated_encryption() {
        assert!(BLE_SOURCE.contains("AuthReq::Bond | AuthReq::Mitm | AuthReq::Sc"));
        assert!(BLE_SOURCE.contains("SecurityIOCap::DisplayOnly"));
        assert!(BLE_SOURCE.contains("NimbleProperties::WRITE_ENC"));
        assert!(BLE_SOURCE.contains("NimbleProperties::WRITE_AUTHEN"));
        assert!(BLE_SOURCE.contains("BleTaskResult::Started"));
        assert!(BLE_SOURCE.contains("passkey,"));
    }

    #[test]
    fn ble_passkey_is_drawn_after_the_radio_is_enabled() {
        let init = BLE_SOURCE
            .find("BLEDevice::init();")
            .expect("the session must initialize NimBLE");
        let draw = BLE_SOURCE
            .find("esp_random()")
            .expect("the passkey must come from the hardware RNG");
        assert!(
            init < draw,
            "esp_random() is only a true RNG once the radio is on; before \
             BLEDevice::init() the six-digit passkey would be pseudo-random"
        );
        assert_eq!(BLE_SOURCE.matches("esp_random()").count(), 1);
    }

    #[test]
    fn ble_notify_completion_is_attempt_bound_and_stale_safe() {
        assert!(BLE_SOURCE.contains("struct NotifyAttempt"));
        assert!(BLE_SOURCE.contains("attempt: NotifyAttempt"));
        assert!(BLE_SOURCE.contains("attempt_id: u64"));
        assert!(BLE_SOURCE.contains("const BLE_REPLY_MAX_RETRIES: u8 = 3"));
        assert!(BLE_SOURCE.contains("ReplyTerminated"));
        assert!(BLE_SOURCE.contains("retired_handles: VecDeque<(u16, std::time::Instant)>"));
        assert!(BLE_SOURCE.contains("const RETIRED_HANDLE_GRACE: Duration"));
        assert!(BLE_SOURCE.contains("fn discard_expired_handles"));
        assert!(BLE_SOURCE.contains("fn release_generation"));
        assert!(BLE_SOURCE.contains("self.is_retired(attempt.conn_handle)"));
        assert!(BLE_SOURCE.contains("self.retire_handle(conn_handle)"));
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
        assert!(CTX_SOURCE.contains("pending.reply == *reply"));
        assert!(CTX_SOURCE.contains("pub const USB_REPLY_PENDING_CAPACITY: usize = 32"));
        assert!(CTX_SOURCE.contains("const USB_REPLY_MAX_RETRIES: u8 = 3"));
        assert!(CTX_SOURCE.contains("fn cancel_usb_reply"));
        assert!(CTX_SOURCE.contains("retry_count = pending.retry_count.saturating_add(1)"));
        assert!(MAIN_SOURCE.contains("fn queue_ble_delivery"));
        assert!(MAIN_SOURCE.contains("BLE_REPLY_PENDING_CAPACITY"));
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
        for service in [
            "PROX", "ANS", "CTS", "HTP", "IPSS", "TPS", "IAS", "LLS", "SPS", "HR", "BAS", "DIS",
        ] {
            assert!(
                config_is(&format!("BT_NIMBLE_{service}_SERVICE"), "n"),
                "NimBLE sample service {service} is never registered and must stay out of the image"
            );
        }
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

    #[test]
    fn effect_worker_subscribes_to_the_task_watchdog() {
        assert!(
            EFFECT_SOURCE.contains("crate::watchdog::subscribe()"),
            "the only NVS writer must be watched: an unwatched hang latches \
             worker_batch_in_flight forever and silently stops all persistence"
        );
        assert!(
            EFFECT_SOURCE.contains("crate::watchdog::feed()"),
            "subscribing without feeding turns the watchdog into a reboot loop"
        );
        assert!(
            EFFECT_SOURCE.contains("batch_rx.recv_timeout("),
            "the worker must wake periodically so it can feed while idle"
        );
    }

    #[test]
    fn idle_workers_block_long_enough_for_automatic_light_sleep() {
        const AUDIO: &str = include_str!("../../rust-firmware/src/audio_task.rs");
        const USB: &str = include_str!("../../rust-firmware/src/usb_console.rs");
        const WATCHDOG: &str = include_str!("../../rust-firmware/src/watchdog.rs");
        const RTC: &str = include_str!("../../rust-firmware/src/rtc_executor.rs");
        const EPD: &str = include_str!("../../rust-firmware/src/epd_task.rs");
        const SYNC: &str = include_str!("../../rust-firmware/src/sync_task.rs");

        let threshold_ms: u64 = SDKCONFIG
            .lines()
            .find_map(|line| {
                line.trim()
                    .strip_prefix("CONFIG_FREERTOS_IDLE_TIME_BEFORE_SLEEP=")
            })
            .and_then(|value| value.parse().ok())
            .expect("the light-sleep threshold must stay a literal");
        let feed_secs: u64 = WATCHDOG
            .split("WORKER_IDLE_FEED: Duration = Duration::from_secs(")
            .nth(1)
            .and_then(|rest| rest.split(')').next())
            .and_then(|value| value.parse().ok())
            .expect("WORKER_IDLE_FEED must stay a literal");
        assert!(
            feed_secs * 1000 >= 5 * threshold_ms && feed_secs < 10,
            "idle workers must sleep well past the light-sleep threshold yet feed the 10 s TWDT"
        );
        for (name, source) in [
            ("rtc", RTC),
            ("epd", EPD),
            ("sync", SYNC),
            ("effect", EFFECT_SOURCE),
            ("audio", AUDIO),
        ] {
            assert!(
                source.contains("WORKER_IDLE_FEED"),
                "the {name} worker must idle on the shared watchdog cadence"
            );
        }
        assert!(
            AUDIO.contains("AudioMode::Idle => wait_for_command(")
                && AUDIO.contains("ready.wait_timeout("),
            "an idle audio task must block on its mailbox instead of polling"
        );
        assert!(
            BLE_SOURCE.contains("command_rx\n                .recv()\n"),
            "the BLE worker must block while it has no session"
        );
        assert!(
            USB.contains("const NO_HOST_POLL: Duration = Duration::from_millis(500)"),
            "the USB reader must back off while no host is attached"
        );
    }

    #[test]
    fn crash_dumps_and_public_releases_keep_secrets_in_bounds() {
        const RELEASE: &str = include_str!("../../scripts/release.sh");
        assert!(
            config_is("ESP_COREDUMP_CAPTURE_DRAM", "n"),
            "flash is not encrypted in the production image, so the coredump must hold \
             task stacks only and never copy the heap that holds the Wi-Fi password and token"
        );
        for gate in [
            "require_config '# CONFIG_ESP_COREDUMP_CAPTURE_DRAM is not set'",
            "require_config '# CONFIG_SECURE_BOOT is not set'",
            "require_config '# CONFIG_SECURE_FLASH_ENC_ENABLED is not set'",
            "./scripts/build-rust.sh --release --locked",
            "SOURCE_DATE_EPOCH=\"$(git show -s --format=%ct HEAD)\"",
            "Security boundary",
        ] {
            assert!(RELEASE.contains(gate), "release.sh lost `{gate}`");
        }
    }

    #[test]
    fn flashing_goes_through_the_identity_check() {
        const RUNNER: &str = include_str!("../../rust-firmware/.cargo/config.toml");
        const FLASH: &str = include_str!("../../scripts/flash-note4.sh");
        assert!(RUNNER.contains("runner = [\"../scripts/flash-note4.sh\""));
        assert!(!RUNNER.contains("runner = \"espflash"));
        for check in [
            "read_mac",
            "flash_id",
            "INKWASH_NOTE4_MAC",
            "--flash-size 16mb --flash-mode dio --flash-freq 80mhz",
            "--partition-table \"$partitions\" --partition-table-offset 0x10000",
        ] {
            assert!(FLASH.contains(check), "flash-note4.sh lost `{check}`");
        }
        let identity = FLASH
            .find("probe read_mac")
            .expect("the MAC is read before flashing");
        let flash = FLASH
            .find("espflash flash")
            .expect("the wrapper flashes with espflash");
        assert!(
            identity < flash,
            "identity must be proven before anything is written"
        );
    }

    #[test]
    fn secure_builds_sign_and_verify_the_application() {
        const BUILD: &str = include_str!("../../scripts/build-rust.sh");
        for step in [
            "--secure-pad-v2",
            "espsecure sign_data --version 2",
            "espsecure verify_signature --version 2 --keyfile \"$key\" \"$signed\"",
            "\"$out/bootloader.bin\"",
            "'# CONFIG_SECURE_BOOT_INSECURE is not set'",
        ] {
            assert!(BUILD.contains(step), "secure profile lost `{step}`");
        }
    }

    #[test]
    fn firmware_keeps_no_unrunnable_test_modules() {
        const EP_TASK: &str = include_str!("../../rust-firmware/src/epd_task.rs");
        const USB: &str = include_str!("../../rust-firmware/src/usb_console.rs");
        const CARGO: &str = include_str!("../../rust-firmware/Cargo.toml");

        assert!(
            CARGO.contains("harness = false"),
            "the firmware bin disables the libtest harness; host tests must live in inkwash-logic"
        );
        assert!(
            !EFFECT_SOURCE.contains("#[cfg(test)]"),
            "with `harness = false` no `#[test]` body is ever run, and cargo does not \
             even type-check those bodies (ordinary helpers in a cfg(test) module still \
             are); a test module here can therefore only rot, so keep it in inkwash-logic"
        );
        assert!(!EP_TASK.contains("#[cfg(test)]"));
        assert!(!USB.contains("#[cfg(test)]"));
    }

    #[test]
    fn boot_ledger_lives_in_retained_memory_and_clears_only_after_core_init() {
        const BOOT_LEDGER: &str = include_str!("../../rust-firmware/src/boot_ledger.rs");
        const MAIN: &str = include_str!("../../rust-firmware/src/main.rs");

        assert!(
            BOOT_LEDGER.contains("#[link_section = \".rtc_noinit\"]"),
            "the boot ledger must live in RTC noinit memory: anywhere else it is either \
             re-initialized from the flash image on every boot (the loop it counts is \
             never seen) or cleared by the reset it is meant to count"
        );
        assert!(BOOT_LEDGER.contains("write_volatile"));
        assert!(MAIN.contains("if ledger.exhausted()"));

        let dispatch = MAIN
            .find("Event::SyncSchedulerConfigured(scheduler_config)")
            .expect("the boot path must configure the sync scheduler once at startup");
        let clear = MAIN
            .find("boot_ledger::clear()")
            .expect("the boot ledger must be cleared on purpose");
        assert!(
            dispatch < clear,
            "the ledger may only be cleared after core initialization dispatched, so a \
             run that dies earlier keeps counting"
        );
    }

    #[test]
    fn diagnostic_counters_are_atomic_and_stay_off_the_flash() {
        const DIAG: &str = include_str!("../../rust-firmware/src/diag.rs");

        assert!(
            DIAG.contains("AtomicU32"),
            "counters are written from the main, EPD and BLE workers, so a plain static \
             would be a data race under -D warnings' shared-mutable-state rules"
        );
        assert!(DIAG.contains("fetch_update"));
        assert!(
            !DIAG.contains("Nvs") && !DIAG.contains("nvs_blob"),
            "counters are incremented on hot paths and must never write NVS"
        );
        assert!(
            !DIAG.contains("Mutex"),
            "an uncontended atomic keeps the queue-full and fallback paths lock-free"
        );
    }

    #[test]
    fn the_boot_ledger_image_gate_is_wired_into_every_build() {
        const CI: &str = include_str!("../../.github/workflows/ci.yml");
        const RELEASE: &str = include_str!("../../scripts/release.sh");
        // Including the gate here also keeps it from disappearing quietly.
        const GATE: &str = include_str!("../../scripts/check-boot-ledger.sh");

        assert!(GATE.contains("--self-test"), "the gate must be testable");
        assert!(
            GATE.contains("image_info"),
            "the gate must inspect the produced image, not the ELF it came from"
        );
        assert_eq!(
            CI.matches("./scripts/check-boot-ledger.sh").count(),
            3,
            "release, diagnostic and secure builds must each run the image gate"
        );
        assert!(
            RELEASE.contains("./scripts/check-boot-ledger.sh"),
            "a release must not ship an image whose segments cover the boot ledger"
        );
    }

    #[test]
    fn safe_mode_never_touches_stored_data() {
        const MAIN_SOURCE: &str = include_str!("../../rust-firmware/src/main.rs");
        let safe_mode = MAIN_SOURCE
            .split("fn run_safe_mode(")
            .nth(1)
            .expect("the firmware must keep a minimum safe mode");
        let body = safe_mode
            .split("\nfn ")
            .next()
            .expect("safe mode must be a self-contained function")
            .to_lowercase();
        assert!(
            !body.contains("nvs") && !body.contains("erase"),
            "safe mode promises the operator that stored data is untouched: it must not \
             take a store handle, and it must never erase one"
        );
    }
}
