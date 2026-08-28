//! One-shot interrupt wake for the idle loop.
//!
//! The Home loop's idle cadence is a 1 s wait that doubles as the light
//! sleep window (see `power::configure_light_sleep`). A plain
//! `thread::sleep` cannot be shortened by a key press: the GPIO/EXT1 wake
//! source resumes the CPU, but the task blocked in a delay only runs when
//! the delay expires - which would miss fast taps entirely and, while the
//! pin is held low, spin the chip re-entering sleep.
//!
//! Instead the three nav keys and the RTC alarm line (GPIO5) are wired to
//! one-shot low-level GPIO interrupts that disable their own pin and notify
//! the main task (`xTaskGenericNotifyFromISR`). The idle loop arms the pins
//! before waiting and re-arms after every wake; a press or an asserted
//! alarm line returns the wait immediately, and the loop polls the buttons
//! / probes the alarm line as usual. Debounce and long-press semantics in
//! `button::Button` are untouched.

use core::ffi::c_void;

use anyhow::Result;
use esp_idf_svc::sys::{
    eNotifyAction_eNoAction, gpio_int_type_t_GPIO_INTR_LOW_LEVEL, gpio_intr_disable,
    gpio_intr_enable, gpio_isr_handler_add, gpio_set_intr_type, xTaskGenericNotifyFromISR,
    xTaskGenericNotifyWait, xTaskGetCurrentTaskHandle, BaseType_t, TaskHandle_t, TickType_t,
};

/// Per-pin ISR context: which task to notify and which pin to re-disable.
struct WakeCtx {
    task: TaskHandle_t,
    gpio: i32,
}

/// Shared ISR for all four wake pins. The first assertion disables the pin
/// that fired - a held-low level must not storm the ISR - then notifies the
/// main task. `gpio_isr_handler_add` passes the per-pin `WakeCtx` as the
/// argument.
unsafe extern "C" fn wake_isr(ctx: *mut c_void) {
    let ctx = unsafe { &*(ctx as *const WakeCtx) };
    unsafe {
        gpio_intr_disable(ctx.gpio);
        let mut higher_prio_woken: BaseType_t = 0;
        xTaskGenericNotifyFromISR(
            ctx.task,
            0,
            0,
            eNotifyAction_eNoAction,
            core::ptr::null_mut(),
            &mut higher_prio_woken,
        );
    }
}

/// One-shot wake wiring for the pins that must resume the idle loop: the
/// three nav keys and the PCF8563 `RTC_INT` line (GPIO5). Owned by
/// `Note4Board` (the pins live there) and used by the main loop's idle
/// wait.
pub struct Waker {
    pins: &'static [i32],
}

impl Waker {
    /// Records the pins to arm/disarm.
    pub fn new(pins: &'static [i32]) -> Self {
        Self { pins }
    }

    /// Wires a one-shot low-level interrupt for `gpio_num` to the shared
    /// wake ISR, capturing the calling task (the main loop) as the
    /// notification target. The `WakeCtx` is leaked on purpose: it lives
    /// for the process, exactly like the ISR wiring. The pin starts
    /// disabled; the loop arms it via [`Waker::arm`] ahead of each idle
    /// wait.
    pub fn subscribe(&self, gpio_num: i32) -> Result<()> {
        esp_idf_svc::hal::gpio::enable_isr_service()?;
        let task = unsafe { xTaskGetCurrentTaskHandle() };
        let ctx = Box::into_raw(Box::new(WakeCtx {
            task,
            gpio: gpio_num,
        }));
        let ret = unsafe {
            gpio_set_intr_type(gpio_num, gpio_int_type_t_GPIO_INTR_LOW_LEVEL);
            gpio_isr_handler_add(gpio_num, Some(wake_isr), ctx as *mut c_void)
        };
        if ret != 0 {
            // SAFETY: the box was just leaked; nothing else holds it.
            unsafe { drop(Box::from_raw(ctx)) };
            anyhow::bail!("gpio_isr_handler_add(GPIO{gpio_num}) failed: 0x{ret:x}");
        }
        unsafe { gpio_intr_disable(gpio_num) };
        Ok(())
    }

    /// Enables every subscribed pin's interrupt ahead of an idle wait.
    pub fn arm(&self) {
        for gpio in self.pins {
            unsafe { gpio_intr_enable(*gpio) };
        }
    }

    /// Blocks until a wake notification or `timeout_ticks`, consuming the
    /// notification state either way. Returns true when a notification was
    /// pending (a key was pressed or the RTC alarm line asserted).
    pub fn wait(&self, timeout_ticks: u32) -> bool {
        unsafe {
            xTaskGenericNotifyWait(0, 0, 0, core::ptr::null_mut(), timeout_ticks as TickType_t) != 0
        }
    }
}
