use std::time::Duration;

use anyhow::{anyhow, Result};
use esp_idf_svc::sys::{esp, esp_task_wdt_add, esp_task_wdt_reset};

pub const WORKER_IDLE_FEED: Duration = Duration::from_secs(4);

pub fn subscribe() -> Result<()> {
    esp!(unsafe { esp_task_wdt_add(std::ptr::null_mut()) })
        .map_err(|e| anyhow!("esp_task_wdt_add failed: {e}"))
}

pub fn feed() {
    if let Err(err) = esp!(unsafe { esp_task_wdt_reset() }) {
        log::warn!("esp_task_wdt_reset failed: {err}");
    }
}
