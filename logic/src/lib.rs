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
pub mod datetime;
pub mod device_config;
pub mod epd_registry;
pub mod event_queue;
pub mod harness;
pub mod inbox_item;
pub mod list_window;
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
    fn ble_worker_uses_psram_and_checks_internal_heap_before_init() {
        assert!(BLE_SOURCE.contains("MALLOC_CAP_SPIRAM | esp_idf_svc::sys::MALLOC_CAP_8BIT"));
        assert!(BLE_SOURCE.contains("MALLOC_CAP_INTERNAL | esp_idf_svc::sys::MALLOC_CAP_DMA"));
        assert!(BLE_SOURCE.contains("heap_caps_get_free_size(BLE_INTERNAL_CAPS)"));
        assert!(BLE_SOURCE.contains("heap_caps_get_largest_free_block(BLE_INTERNAL_CAPS)"));
        assert!(BLE_SOURCE.contains("BLEDevice::init();"));
        assert!(BLE_SOURCE.contains("if !inkwash_logic::ble_memory::sufficient_internal_heap"));
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
}
