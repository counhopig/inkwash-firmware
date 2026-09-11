//! Shared thread-spawn helper that keeps a worker's stack in internal RAM.
//!
//! Several workers run code that must not touch PSRAM: NimBLE controller init
//! and NVS/flash writes both execute with the flash cache disabled, and a
//! stack that lives in external SPI RAM is unreachable in that window. The
//! build allows pthread stacks in PSRAM
//! (`CONFIG_SPIRAM_ALLOW_STACK_EXTERNAL_MEMORY=y`), so the allocation caps
//! have to be requested explicitly per spawn.

use anyhow::{anyhow, Result};

/// Spawns a named thread whose stack is forced into internal RAM.
///
/// The caps are applied through the process-wide `esp_pthread_set_cfg` and
/// restored immediately afterwards, so no later `spawn` inherits them. Call
/// this only from a context that is not spawning other threads concurrently:
/// the configuration is global for the duration of the call.
pub fn spawn_internal_stack<F>(name: &str, stack_size: usize, body: F) -> Result<()>
where
    F: FnOnce() + Send + 'static,
{
    let mut cfg = unsafe { esp_idf_svc::sys::esp_pthread_get_default_config() };
    cfg.stack_size = stack_size;
    cfg.stack_alloc_caps =
        esp_idf_svc::sys::MALLOC_CAP_INTERNAL | esp_idf_svc::sys::MALLOC_CAP_8BIT;
    cfg.inherit_cfg = false;
    esp_idf_svc::sys::esp!(unsafe { esp_idf_svc::sys::esp_pthread_set_cfg(&cfg) })
        .map_err(|e| anyhow!("{name} internal stack configuration failed: {e:?}"))?;

    let spawned = std::thread::Builder::new()
        .name(name.to_string())
        .stack_size(stack_size)
        .spawn(body);

    // Do not leak this worker's pthread policy into any later thread creation.
    let default_cfg = unsafe { esp_idf_svc::sys::esp_pthread_get_default_config() };
    if let Err(err) =
        esp_idf_svc::sys::esp!(unsafe { esp_idf_svc::sys::esp_pthread_set_cfg(&default_cfg) })
    {
        log::warn!("{name} pthread policy restore failed: {err:?}");
    }

    spawned
        .map(|_handle| ())
        .map_err(|e| anyhow!("{name} thread spawn failed (internal RAM for the stack?): {e}"))
}
