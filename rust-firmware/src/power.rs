use anyhow::{bail, Result};
use core::ffi::c_void;
use esp_idf_svc::sys::{
    esp_deep_sleep_start, esp_pm_config_t, esp_pm_configure, esp_sleep_disable_wakeup_source,
    esp_sleep_enable_ext1_wakeup, esp_sleep_enable_gpio_switch, esp_sleep_enable_gpio_wakeup,
    esp_sleep_enable_timer_wakeup, esp_sleep_ext1_wakeup_mode_t_ESP_EXT1_WAKEUP_ANY_LOW,
    esp_sleep_get_ext1_wakeup_status, esp_sleep_get_wakeup_cause,
    esp_sleep_source_t_ESP_SLEEP_WAKEUP_EXT1, esp_sleep_source_t_ESP_SLEEP_WAKEUP_GPIO,
    esp_sleep_source_t_ESP_SLEEP_WAKEUP_UNDEFINED, gpio_hold_dis, gpio_hold_en,
    gpio_int_type_t_GPIO_INTR_LOW_LEVEL, gpio_wakeup_disable, gpio_wakeup_enable,
};

pub use inkwash_logic::wake_cause::WakeCause;

const GPIO_NUM_17: i32 = 17;
pub const GPIO_NUM_0: i32 = 0;

pub const GPIO_NUM_5: i32 = 5;

pub const GPIO_NUM_18: i32 = 18;

pub const GPIO_NUM_39: i32 = 39;

pub const WAKE_PINS: [i32; 3] = [GPIO_NUM_0, GPIO_NUM_18, GPIO_NUM_39];

pub fn release_power_latch_hold() -> Result<()> {
    unsafe { gpio_hold_dis(GPIO_NUM_17) };
    Ok(())
}

pub fn log_wakeup_cause() -> bool {
    let cause = unsafe { esp_sleep_get_wakeup_cause() };
    log::info!("Wakeup cause raw = 0x{:x}", cause);
    cause != esp_sleep_source_t_ESP_SLEEP_WAKEUP_UNDEFINED
}

pub fn enter_deep_sleep_with_wakeups(resync_interval: Option<std::time::Duration>) -> Result<()> {
    prepare_deep_sleep_wakeups(resync_interval)?;

    unsafe { esp_sleep_enable_gpio_switch(false) };

    unsafe { gpio_hold_en(GPIO_NUM_17) };
    log::info!("Entering deep sleep; wake on ENTER/RTC alarm/DOWN (GPIO0/5/18 low)");
    unsafe { esp_deep_sleep_start() };
}

pub fn prepare_deep_sleep_wakeups(resync_interval: Option<std::time::Duration>) -> Result<()> {
    disable_light_sleep_gpio_wakeup();
    let mask: u64 = (1u64 << GPIO_NUM_0) | (1u64 << GPIO_NUM_5) | (1u64 << GPIO_NUM_18);
    let ret = unsafe {
        esp_sleep_enable_ext1_wakeup(mask, esp_sleep_ext1_wakeup_mode_t_ESP_EXT1_WAKEUP_ANY_LOW)
    };
    if ret != 0 {
        bail!("esp_sleep_enable_ext1_wakeup(GPIO0|GPIO5|GPIO18, low) failed: 0x{ret:x}");
    }
    if let Some(interval) = resync_interval {
        let ret = unsafe { esp_sleep_enable_timer_wakeup(interval.as_micros() as u64) };
        if ret != 0 {
            bail!("esp_sleep_enable_timer_wakeup failed: 0x{ret:x}");
        }
    }
    Ok(())
}

#[allow(dead_code)]
pub fn restart_via_deep_sleep(delay: std::time::Duration) -> ! {
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

pub fn configure_light_sleep() -> Result<()> {
    prepare_light_sleep_wakeups()?;
    set_light_sleep_enabled(true)?;
    log::info!("Light sleep armed: keys GPIO0/18/39 wake");
    Ok(())
}

pub fn disable_light_sleep() -> Result<()> {
    set_light_sleep_enabled(false)
}

fn set_light_sleep_enabled(enabled: bool) -> Result<()> {
    let config = esp_pm_config_t {
        max_freq_mhz: esp_idf_svc::sys::CONFIG_ESP_DEFAULT_CPU_FREQ_MHZ as i32,
        min_freq_mhz: esp_idf_svc::sys::CONFIG_ESP_DEFAULT_CPU_FREQ_MHZ as i32,
        light_sleep_enable: enabled,
    };
    let ret = unsafe { esp_pm_configure(&config as *const esp_pm_config_t as *const c_void) };
    if ret != 0 {
        bail!("esp_pm_configure(light sleep) failed: 0x{ret:x}");
    }

    Ok(())
}

pub fn prepare_light_sleep_wakeups() -> Result<()> {
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

    Ok(())
}

pub fn usb_host_connected() -> bool {
    unsafe { esp_idf_svc::sys::usb_serial_jtag_is_connected() }
}

fn disable_light_sleep_gpio_wakeup() {
    for gpio in WAKE_PINS {
        unsafe { gpio_wakeup_disable(gpio) };
    }
    unsafe { esp_sleep_disable_wakeup_source(esp_sleep_source_t_ESP_SLEEP_WAKEUP_GPIO) };
}
