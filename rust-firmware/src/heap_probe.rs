use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

use esp_idf_svc::sys::{
    esp_timer_get_time, heap_caps_get_free_size, heap_caps_get_largest_free_block, MALLOC_CAP_8BIT,
    MALLOC_CAP_DMA, MALLOC_CAP_INTERNAL, MALLOC_CAP_SPIRAM,
};

static CONTROL_COMMANDS: AtomicU32 = AtomicU32::new(0);
static SYNC_ATTEMPTS: AtomicU32 = AtomicU32::new(0);

pub fn note_control_command() {
    CONTROL_COMMANDS.fetch_add(1, Ordering::Relaxed);
}

pub fn note_sync_attempt() {
    SYNC_ATTEMPTS.fetch_add(1, Ordering::Relaxed);
}

fn metric(caps: u32) -> (usize, usize) {
    unsafe {
        (
            heap_caps_get_free_size(caps),
            heap_caps_get_largest_free_block(caps),
        )
    }
}

pub fn snapshot(stage: &str) {
    let internal = metric(MALLOC_CAP_INTERNAL | MALLOC_CAP_8BIT);
    let dma = metric(MALLOC_CAP_INTERNAL | MALLOC_CAP_DMA);
    let psram = metric(MALLOC_CAP_SPIRAM | MALLOC_CAP_8BIT);
    let uptime_ms = unsafe { esp_timer_get_time() } / 1000;
    log::info!(
        "HEAPPROBE {stage} uptime_ms={uptime_ms} cmd={} sync={} int_free={} int_largest={} \
         dma_free={} dma_largest={} psram_free={} psram_largest={}",
        CONTROL_COMMANDS.load(Ordering::Relaxed),
        SYNC_ATTEMPTS.load(Ordering::Relaxed),
        internal.0,
        internal.1,
        dma.0,
        dma.1,
        psram.0,
        psram.1,
    );
}

pub const TASK_SLOTS: [&str; 9] = [
    "main",
    "rtc",
    "epd",
    "sync",
    "ble",
    "effect-task",
    "audio",
    "usb-console-rx",
    "usb-console-writer",
];

const TASK_STACK_BYTES: [usize; 9] = [
    32 * 1024,
    8 * 1024,
    12 * 1024,
    16 * 1024,
    16 * 1024,
    16 * 1024,
    8 * 1024,
    12 * 1024,
    8 * 1024,
];

pub const SLOT_MAIN: usize = 0;
pub const SLOT_RTC: usize = 1;
pub const SLOT_EPD: usize = 2;
pub const SLOT_SYNC: usize = 3;
pub const SLOT_BLE: usize = 4;
pub const SLOT_EFFECT: usize = 5;
pub const SLOT_AUDIO: usize = 6;
pub const SLOT_USB_RX: usize = 7;
pub const SLOT_USB_WRITER: usize = 8;

static TASK_HANDLES: [AtomicUsize; 9] = [const { AtomicUsize::new(0) }; 9];

pub fn register_current_task(slot: usize) {
    let handle = unsafe { esp_idf_svc::sys::xTaskGetCurrentTaskHandle() } as usize;
    if slot < TASK_HANDLES.len() {
        TASK_HANDLES[slot].store(handle, Ordering::Relaxed);
    }
}

pub fn log_registered_stacks(stage: &str) {
    let mut summary = String::new();
    for (slot, name) in TASK_SLOTS.iter().enumerate() {
        let handle = TASK_HANDLES[slot].load(Ordering::Relaxed);
        if handle == 0 {
            continue;
        }
        let hwm = unsafe {
            esp_idf_svc::sys::uxTaskGetStackHighWaterMark2(handle as esp_idf_svc::sys::TaskHandle_t)
        };
        let stack_start = unsafe {
            esp_idf_svc::sys::pxTaskGetStackStart(handle as esp_idf_svc::sys::TaskHandle_t) as usize
        };
        let stack_end = stack_start.saturating_add(TASK_STACK_BYTES[slot]);
        if hwm < 2048 {
            log::warn!(
                "STACKLOW {stage} task={name} tcb={handle:#010x} stack={stack_start:#010x}..{stack_end:#010x} hwm_free={hwm} bytes"
            );
        }
        summary.push_str(&format!(" {name}={hwm}"));
    }
    if !summary.is_empty() {
        log::info!("STACKPROBE {stage}{summary}");
    }
}
