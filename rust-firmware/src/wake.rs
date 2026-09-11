use core::ffi::c_void;

use anyhow::Result;
use esp_idf_svc::sys::{
    eNotifyAction_eSetValueWithOverwrite, gpio_int_type_t_GPIO_INTR_LOW_LEVEL, gpio_intr_disable,
    gpio_intr_enable, gpio_isr_handler_add, gpio_set_intr_type, xTaskGenericNotifyFromISR,
    xTaskGenericNotifyWait, xTaskGetCurrentTaskHandle, BaseType_t, TaskHandle_t, TickType_t,
};

struct WakeCtx {
    task: TaskHandle_t,
    gpio: i32,
}

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
