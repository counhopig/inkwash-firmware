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

pub mod alarm_regs;
pub mod alarm_schedule;
pub mod app;
pub mod button_event;
pub mod datetime;
pub mod device_config;
pub mod inbox_item;
pub mod protocol;
pub mod reminder_dedup;
pub mod scheduler;
pub mod sync_validate;
pub mod todo;
pub mod wake_cause;
