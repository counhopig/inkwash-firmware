use core::ffi::c_void;
use std::sync::Mutex;

use anyhow::Result;
use esp_idf_svc::sys::{
    _frxt_setup_switch, eNotifyAction_eSetValueWithOverwrite, gpio_int_type_t_GPIO_INTR_LOW_LEVEL,
    gpio_intr_disable, gpio_intr_enable, gpio_isr_handler_add, gpio_set_intr_type,
    xTaskGenericNotifyFromISR, xTaskGenericNotifyWait, xTaskGetCurrentTaskHandle, BaseType_t,
    TaskHandle_t, TickType_t,
};

// Each subscription leaks its WakeCtx on purpose: the ISR may run until reset,
// and no handler is ever removed. A second subscription for the same GPIO would
// replace the handler while the first context stays live, so it is refused.
static SUBSCRIBED: Mutex<u64> = Mutex::new(0);

struct WakeCtx {
    task: TaskHandle_t,
    gpio: i32,
}

#[link_section = ".iram1.wake_isr"]
unsafe extern "C" fn wake_isr(ctx: *mut c_void) {
    let ctx = unsafe { &*(ctx as *const WakeCtx) };
    unsafe {
        gpio_intr_disable(ctx.gpio);
        let mut higher_prio_woken: BaseType_t = 0;

        xTaskGenericNotifyFromISR(
            ctx.task,
            0,
            1,
            eNotifyAction_eSetValueWithOverwrite,
            core::ptr::null_mut(),
            &mut higher_prio_woken,
        );
        if higher_prio_woken != 0 {
            _frxt_setup_switch();
        }
    }
}

pub struct Waker {
    pins: &'static [i32],
}

impl Waker {
    pub fn new(pins: &'static [i32]) -> Self {
        Self { pins }
    }

    pub fn subscribe(&self, gpio_num: i32) -> Result<()> {
        if !(0..64).contains(&gpio_num) {
            anyhow::bail!("GPIO{gpio_num} cannot carry a wake interrupt");
        }
        let bit = 1u64 << gpio_num;
        let mut subscribed = SUBSCRIBED
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if *subscribed & bit != 0 {
            anyhow::bail!("GPIO{gpio_num} wake interrupt is already subscribed");
        }
        self.install(gpio_num)?;
        *subscribed |= bit;
        Ok(())
    }

    fn install(&self, gpio_num: i32) -> Result<()> {
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
            unsafe { drop(Box::from_raw(ctx)) };
            anyhow::bail!("gpio_isr_handler_add(GPIO{gpio_num}) failed: 0x{ret:x}");
        }
        unsafe { gpio_intr_disable(gpio_num) };
        Ok(())
    }

    pub fn arm(&self) {
        for gpio in self.pins {
            unsafe { gpio_intr_enable(*gpio) };
        }
    }

    pub fn wait(&self, timeout_ticks: u32) -> bool {
        unsafe {
            xTaskGenericNotifyWait(
                0,
                0xffffffff,
                0,
                core::ptr::null_mut(),
                timeout_ticks as TickType_t,
            ) != 0
        }
    }
}
