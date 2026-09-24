use std::time::Duration;

use anyhow::{anyhow, Result};
use esp_idf_svc::sys::{esp, esp_task_wdt_add, esp_task_wdt_reset, ESP_ERR_NOT_FOUND};

pub const WORKER_IDLE_FEED: Duration = Duration::from_secs(4);

pub fn subscribe() -> Result<()> {
    esp!(unsafe { esp_task_wdt_add(std::ptr::null_mut()) })
        .map_err(|e| anyhow!("esp_task_wdt_add failed: {e}"))
}

pub fn feed() {
    match esp!(unsafe { esp_task_wdt_reset() }) {
        Ok(()) => {}
        Err(err) if err.code() == ESP_ERR_NOT_FOUND as i32 => {}
        Err(err) => log::warn!("esp_task_wdt_reset failed: {err}"),
    }
}
