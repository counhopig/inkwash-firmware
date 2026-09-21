use anyhow::{anyhow, Result};
use parking_lot::Mutex;

static PTHREAD_CONFIG_LOCK: Mutex<()> = Mutex::new(());

pub fn spawn_internal_stack<F>(name: &str, stack_size: usize, body: F) -> Result<()>
where
    F: FnOnce() + Send + 'static,
{
    let _config_guard = PTHREAD_CONFIG_LOCK.lock();
    let default_cfg = unsafe { esp_idf_svc::sys::esp_pthread_get_default_config() };
    let mut cfg = default_cfg;
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

    if let Err(err) =
        esp_idf_svc::sys::esp!(unsafe { esp_idf_svc::sys::esp_pthread_set_cfg(&default_cfg) })
    {
        log::warn!("{name} pthread policy restore failed: {err:?}");
    }

    spawned
        .map(|_handle| ())
        .map_err(|e| anyhow!("{name} thread spawn failed (internal RAM for the stack?): {e}"))
}
