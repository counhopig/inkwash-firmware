use anyhow::{bail, Result};
use core::ffi::{c_char, c_void};
use esp_idf_svc::sys::{
    esp_deep_sleep_start, esp_pm_config_t, esp_pm_configure, esp_pm_lock_acquire,
    esp_pm_lock_create, esp_pm_lock_delete, esp_pm_lock_handle_t, esp_pm_lock_release,
    esp_pm_lock_type_t_ESP_PM_NO_LIGHT_SLEEP, esp_sleep_disable_wakeup_source,
    esp_sleep_enable_ext1_wakeup, esp_sleep_enable_gpio_switch, esp_sleep_enable_gpio_wakeup,
    esp_sleep_enable_timer_wakeup, esp_sleep_ext1_wakeup_mode_t_ESP_EXT1_WAKEUP_ANY_LOW,
    esp_sleep_get_ext1_wakeup_status, esp_sleep_get_wakeup_cause,
    esp_sleep_source_t_ESP_SLEEP_WAKEUP_EXT1, esp_sleep_source_t_ESP_SLEEP_WAKEUP_GPIO,
    esp_sleep_source_t_ESP_SLEEP_WAKEUP_UNDEFINED, gpio_hold_dis, gpio_hold_en,
    gpio_int_type_t_GPIO_INTR_LOW_LEVEL, gpio_wakeup_disable, gpio_wakeup_enable,
};

/// Pure `WakeCause` vocabulary lives in `inkwash-logic` (re-exported here)
/// so the host-testable application state machine shares the single source
/// of truth.
pub use inkwash_logic::wake_cause::WakeCause;

const GPIO_NUM_17: i32 = 17;
pub const GPIO_NUM_0: i32 = 0;
/// PCF8563 `RTC_INT`, open-drain active low; asserted when the RTC alarm
/// fires (see `rtc::Pcf8563::set_alarm`). RTC-capable on ESP32-S3
/// (GPIOs 0-21), unlike GPIO39 (UP), which is why the alarm line was wired
/// here instead.
pub const GPIO_NUM_5: i32 = 5;
/// DOWN key (see `board.rs` `Note4Board::take`). RTC-capable on ESP32-S3
/// (GPIOs 0-21); a deep-sleep wake key via ext1 (in the
/// `enter_deep_sleep_with_wakeups` mask, like ENTER and the RTC alarm
/// line).
pub const GPIO_NUM_18: i32 = 18;
/// UP key: NOT an RTC GPIO (GPIO39), so it can wake light sleep (digital
/// GPIO wakeup) but never deep sleep.
pub const GPIO_NUM_39: i32 = 39;

/// Pins that must resume the idle loop from light sleep: the three nav
/// keys via digital GPIO wakeup (any GPIO works for light sleep).
/// `wake::Waker` subscribes one-shot wake interrupts on these;
/// `configure_light_sleep` arms the sleep wake sources for them.
///
/// The PCF8563 RTC_INT line (GPIO5) is deliberately NOT a light-sleep
/// wake source: its level reads low during light sleep on this board
/// (the sleep GPIO configuration drops the input pull-up and the
/// open-drain line floats), which made the device re-wake in a tight
/// loop. Alarm firing from light sleep is instead caught by the main
/// loop's RTC `alarm_flag` poll (<= 1 s, see `main.rs`); deep sleep keeps
/// the ext1 GPIO5 wake, where the RTC-IO path arms its own pull.
pub const WAKE_PINS: [i32; 3] = [GPIO_NUM_0, GPIO_NUM_18, GPIO_NUM_39];

/// Releases the GPIO17 RTC hold left over from a previous deep-sleep session.
/// Must be called before any `PinDriver::output(gpio17)` is constructed; the
/// hold bypasses normal output control, so the pin stays frozen high until
/// cleared.
pub fn release_power_latch_hold() -> Result<()> {
    unsafe { gpio_hold_dis(GPIO_NUM_17) };
    Ok(())
}

/// Logs the cause reported by the ROM bootloader and reports whether this
/// boot is a wake from deep sleep (as opposed to a power-on reset, a fresh
/// flash, or a reset button press).
pub fn log_wakeup_cause() -> bool {
    let cause = unsafe { esp_sleep_get_wakeup_cause() };
    log::info!("Wakeup cause raw = 0x{:x}", cause);
    cause != esp_sleep_source_t_ESP_SLEEP_WAKEUP_UNDEFINED
}

/// Configures wake sources - ENTER (GPIO0) or the RTC alarm line (GPIO5),
/// either driving low - plus an optional periodic timer wake for
/// background content resync, then enters deep sleep. GPIO17 is held high
/// via the RTC slow IO block so the main power latch survives the sleep.
/// The function does not return.
pub fn enter_deep_sleep_with_wakeups(resync_interval: Option<std::time::Duration>) -> ! {
    // A boot normally ran `configure_light_sleep`, which armed the keys
    // for digital GPIO wakeup; deep sleep must not inherit that (an armed
    // digital wakeup on GPIO0 would double-trigger with the ext1 wake and
    // garble `wake_cause` disambiguation).
    disable_light_sleep_gpio_wakeup();
    // ext1 = multi-GPIO, any-selected-pin-low wakes the chip. GPIO0, GPIO5,
    // and GPIO18 are all RTC-capable on ESP32-S3 (GPIOs 0-21); GPIO39 (UP)
    // is not and can never wake deep sleep.
    let mask: u64 = (1u64 << GPIO_NUM_0) | (1u64 << GPIO_NUM_5) | (1u64 << GPIO_NUM_18);
    let ret = unsafe {
        esp_sleep_enable_ext1_wakeup(mask, esp_sleep_ext1_wakeup_mode_t_ESP_EXT1_WAKEUP_ANY_LOW)
    };
    if ret != 0 {
        log::error!(
            "esp_sleep_enable_ext1_wakeup(GPIO0|GPIO5, low) failed: 0x{:x}",
            ret
        );
    }
    if let Some(interval) = resync_interval {
        let ret = unsafe { esp_sleep_enable_timer_wakeup(interval.as_micros() as u64) };
        if ret != 0 {
            log::error!("esp_sleep_enable_timer_wakeup failed: 0x{:x}", ret);
        }
    }
    // Keep GPIO switches off so the held output (GPIO17) is not yanked back
    // to its sleep default while we are asleep.
    unsafe { esp_sleep_enable_gpio_switch(false) };
    // Hold GPIO17 high so the main power latch does not release.
    unsafe { gpio_hold_en(GPIO_NUM_17) };
    log::info!("Entering deep sleep; wake on ENTER/RTC alarm/DOWN (GPIO0/5/18 low)");
    unsafe { esp_deep_sleep_start() };
}

/// Performs a controlled software power-cycle using only a short timer
/// wakeup. Unlike normal sleep this deliberately excludes ENTER and RTC_INT:
/// an already-low alarm line must not turn an internal Wi-Fi recovery reboot
/// into a false alarm wake.
///
/// Currently only reachable via `wifi::restart_for_fresh_wifi_session`,
/// which has no active callers since the Wi-Fi second-connect crash was
/// fixed by removing the pre-connect scan - kept as an emergency escape
/// hatch.
#[allow(dead_code)]
pub fn restart_via_deep_sleep(delay: std::time::Duration) -> ! {
    // Same disarm as `enter_deep_sleep_with_wakeups`: this path must be
    // wakeable by nothing but the timer, and a leftover light-sleep GPIO
    // wakeup on a key would break that.
    disable_light_sleep_gpio_wakeup();
    let ret = unsafe { esp_sleep_enable_timer_wakeup(delay.as_micros() as u64) };
    if ret != 0 {
        log::error!("esp_sleep_enable_timer_wakeup(restart) failed: 0x{ret:x}");
    }
    unsafe { esp_sleep_enable_gpio_switch(false) };
    unsafe { gpio_hold_en(GPIO_NUM_17) };
    log::info!("Controlled deep-sleep restart; timer wake only");
    unsafe { esp_deep_sleep_start() };
}

/// Disambiguates which ext1 pin caused the wake. Must be called before any
/// GPIO reconfiguration that might change pin levels.
pub fn wake_cause() -> WakeCause {
    let cause = unsafe { esp_sleep_get_wakeup_cause() };
    if cause != esp_sleep_source_t_ESP_SLEEP_WAKEUP_EXT1 {
        return WakeCause::Other;
    }
    let status = unsafe { esp_sleep_get_ext1_wakeup_status() };
    if status & (1u64 << GPIO_NUM_5) != 0 {
        WakeCause::RtcAlarm
    } else if status & (1u64 << GPIO_NUM_0) != 0 {
        WakeCause::Enter
    } else if status & (1u64 << GPIO_NUM_18) != 0 {
        WakeCause::Down
    } else {
        WakeCause::Other
    }
}
/// Enables automatic light sleep for the whole device: `esp_pm_configure`
/// keeps the CPU at its default frequency (no DFS) but enters light sleep
/// whenever both cores are idle past `CONFIG_FREERTOS_IDLE_TIME_BEFORE_SLEEP`
/// ticks, plus the wake sources that must survive the sleep:
/// - the three nav keys via digital GPIO wakeup (any GPIO works in light
///   sleep, which is exactly why UP=GPIO39 can wake light sleep but not
///   deep sleep);
/// - the PCF8563 RTC_INT line via ext1 (GPIO5 low), so a fired alarm
///   wakes the device immediately instead of waiting for the next gated
///   RTC poll (the main loop probes `board.rtc_int` on every iteration
///   and runs the alarm path right away - see `main.rs`);
/// - the RTOS tick/timer path: `vApplicationSleep` arms its own timer
///   wakeup on every light-sleep entry, so there is nothing to configure
///   here for the periodic wakeups (the main loop's idle cadence drives
///   them).
///
/// USB-Serial-JTAG cannot wake light sleep on ESP32-S3: no such API
/// exists in IDF 5.5 (`SOC_USB_SERIAL_JTAG_SUPPORT_LIGHT_SLEEP` is unset
/// on every chip; TODO IDF-6395). USB commands arriving while asleep sit
/// in the controller FIFO until the next periodic wake (<= 1 s at the
/// idle cadence), and opening the serial port resets the chip anyway
/// (development-guide.md §13), so the desktop tool always starts from an
/// awake boot.
///
/// Call once at boot, after the board and Wi-Fi bring-up (Wi-Fi being
/// disconnected is a precondition for actually sleeping; the config below
/// only arms the capability - sleep happens solely when idle).
pub fn configure_light_sleep() -> Result<()> {
    let config = esp_pm_config_t {
        max_freq_mhz: esp_idf_svc::sys::CONFIG_ESP_DEFAULT_CPU_FREQ_MHZ as i32,
        min_freq_mhz: esp_idf_svc::sys::CONFIG_ESP_DEFAULT_CPU_FREQ_MHZ as i32,
        light_sleep_enable: true,
    };
    let ret = unsafe { esp_pm_configure(&config as *const esp_pm_config_t as *const c_void) };
    if ret != 0 {
        bail!("esp_pm_configure(light sleep) failed: 0x{ret:x}");
    }

    // The three nav keys via digital GPIO wakeup. GPIO5 (RTC_INT) stays
    // out of light-sleep wakeup entirely - its level reads low during
    // light sleep on this board (sleep GPIO config drops the pull-up and
    // the open-drain line floats), which re-wakes the device in a tight
    // loop; alarms are caught by the main loop's alarm_flag poll instead.
    // Deep sleep re-arms GPIO5 via ext1, where the RTC-IO path handles
    // its own pull.
    for gpio in WAKE_PINS {
        let ret = unsafe { gpio_wakeup_enable(gpio, gpio_int_type_t_GPIO_INTR_LOW_LEVEL) };
        if ret != 0 {
            bail!("gpio_wakeup_enable(GPIO{gpio}) failed: 0x{ret:x}");
        }
    }
    let ret = unsafe { esp_sleep_enable_gpio_wakeup() };
    if ret != 0 {
        bail!("esp_sleep_enable_gpio_wakeup failed: 0x{ret:x}");
    }

    log::info!("Light sleep armed: keys GPIO0/18/39 wake");
    Ok(())
}

/// RAII block on automatic light sleep: an `esp_pm` `ESP_PM_NO_LIGHT_SLEEP`
/// lock. Holding one keeps the idle task out of light sleep, which is how
/// a radio that registers no IDF skip-light-sleep callback - the S3 BT
/// controller, active while the BLE pairing screen is open - stays
/// uninterrupted across idle windows.
///
/// The other admission conditions (sync in flight,
/// EPD refresh in flight) need no runtime guard while those operations
/// run on the main thread: a running core holds its RTOS pm lock, and an
/// active Wi-Fi radio registers its own skip-light-sleep callback, so
/// automatic light sleep cannot engage mid-operation. This guard only
/// matters once a condition can outlive the main thread (the EPD/sync
/// tasks, or BLE).
pub struct LightSleepBlock {
    handle: esp_pm_lock_handle_t,
    reason: &'static str,
}

impl LightSleepBlock {
    pub fn acquire(reason: &'static str) -> Result<Self> {
        let mut handle: esp_pm_lock_handle_t = core::ptr::null_mut();
        const LOCK_NAME: &[u8] = b"light_sleep_block\0";
        let ret = unsafe {
            esp_pm_lock_create(
                esp_pm_lock_type_t_ESP_PM_NO_LIGHT_SLEEP,
                0,
                LOCK_NAME.as_ptr() as *const c_char,
                &mut handle,
            )
        };
        if ret != 0 {
            bail!("esp_pm_lock_create(NO_LIGHT_SLEEP) failed: 0x{ret:x}");
        }
        let ret = unsafe { esp_pm_lock_acquire(handle) };
        if ret != 0 {
            unsafe { esp_pm_lock_delete(handle) };
            bail!("esp_pm_lock_acquire(NO_LIGHT_SLEEP) failed: 0x{ret:x}");
        }
        log::info!("Light sleep blocked: {reason}");
        Ok(Self { handle, reason })
    }
}

impl Drop for LightSleepBlock {
    fn drop(&mut self) {
        unsafe {
            esp_pm_lock_release(self.handle);
            esp_pm_lock_delete(self.handle);
        }
        log::info!("Light sleep unblocked: {}", self.reason);
    }
}

/// Disarms the light-sleep key wakeup before a deep-sleep entry. In deep
/// sleep digital GPIO wakeup is inert, but GPIO0 is also in the ext1 mask
/// below: an armed digital wakeup on the same pin would set a second
/// wakeup trigger, and `esp_sleep_get_wakeup_cause`'s per-source priority
/// could then report GPIO instead of EXT1 - garbling `wake_cause`'s
/// ENTER-vs-alarm disambiguation.
fn disable_light_sleep_gpio_wakeup() {
    // GPIO5 is in the light-sleep gpio-wakeup set too; deep sleep re-arms
    // it via ext1, and an armed digital wakeup on the same pin would
    // double-trigger and garble `wake_cause` disambiguation.
    for gpio in WAKE_PINS {
        unsafe { gpio_wakeup_disable(gpio) };
    }
    unsafe { esp_sleep_disable_wakeup_source(esp_sleep_source_t_ESP_SLEEP_WAKEUP_GPIO) };
}
