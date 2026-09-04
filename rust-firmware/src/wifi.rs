use std::thread;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use esp_idf_svc::eventloop::EspSystemEventLoop;
use esp_idf_svc::hal::peripherals::Peripherals;
use esp_idf_svc::sntp::{EspSntp, SntpConf, SyncStatus};
use esp_idf_svc::systime::EspSystemTime;
use esp_idf_svc::wifi::{AuthMethod, ClientConfiguration, Configuration, EspWifi};
use heapless::String as HeaplessString;

use crate::rtc::DateTime;
use crate::rtc_executor::RtcExecutor;
use crate::storage::WifiCreds;
use crate::watchdog;

/// How long to wait for the STA link to come up before giving up.
const WIFI_CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
/// How long to wait for the first NTP sync to complete.
const NTP_SYNC_TIMEOUT: Duration = Duration::from_secs(10);

/// Owns the *one* `EspWifi` driver instance for the whole process. Create
/// this exactly once (`main.rs` does so right after taking `sysloop`) and
/// thread `&mut WifiManager` everywhere Wi-Fi is needed - boot-time sync,
/// the on-device setup wizard, and `sync::sync_now`.
///
/// **History of the "one connect per boot" limitation.** This driver was
/// long believed to allow only one successful `connect()` per boot session.
/// The original design had each caller create its own
/// `EspWifi::new(unsafe { Peripherals::steal() }.modem, ...)` and drop it
/// when done; that crashed on a second connect
/// (`Guru Meditation Error: InstrFetchProhibited`). Switching to this one
/// shared instance (avoiding a second `esp_wifi_init`) still crashed with
/// the same signature on a second `esp_wifi_start()` after
/// `esp_wifi_stop()`. Removing `stop()` from `disconnect()` (so `start()`
/// is only ever called once) *still* crashed, this time with a different
/// signature (`Unhandled debug exception`, `BREAK instr`) partway through
/// the second connection's lifecycle. The crashes matched known upstream
/// issues - espressif/esp-idf#7579 (netif rx-callback race on STA restart),
/// espressif/esp-idf#11171 (`esp_wifi_stop()` doesn't fully quiesce the
/// driver), esp-rs/esp-idf-svc#503 (reusing one `EspWifi` panics in the
/// wrapper's state machine) - and the only workaround found was a full
/// deep-sleep restart between uses (`restart_for_fresh_wifi_session`).
///
/// **Root cause found: the manual scan.** The old `connect()` ran a
/// blocking `esp_wifi_scan_start()` before every connection. That flips the
/// WiFi state machine into "scanning" (and the esp-idf-svc wrapper's
/// `status.scan = Started` flag) which a subsequent `esp_wifi_connect()` on
/// an already-started driver did not cleanly recover from. The scan was
/// only ever diagnostic - credentials always come from the desktop's
/// `SetWifi` command, and `esp_wifi_connect()` connects to the configured
/// SSID directly - so it has been removed. **Multiple connections per boot
/// now work reliably** (verified on hardware: three consecutive syncs in
/// one boot session all connected and completed). The raw FFI calls
/// (`esp_wifi_connect()`/`esp_wifi_disconnect()`) are kept over
/// `EspWifi`'s own methods because the wrapper's event-driven status
/// tracking still panics on reuse (esp-idf-svc#503).
///
/// `used`/`restart_for_fresh_wifi_session` are retained for compatibility
/// and as a fallback, but no caller relies on the restart anymore. The only
/// stop/start cycle is the explicit BLE hand-off below, serialized on this
/// owner task and using raw ESP-IDF calls.
pub struct WifiManager {
    wifi: EspWifi<'static>,
    used: bool,
    /// Whether the underlying driver is started.  This is kept separately
    /// from the esp-idf-svc status cache because BLE arbitration stops and
    /// starts the driver through the raw ESP-IDF API.
    started: bool,
    /// Whether BLE suspension actually stopped a started driver.  If Wi-Fi
    /// had never been started, leaving it stopped after BLE is the correct
    /// restoration of the pre-pairing state.
    suspended_was_started: bool,
}

impl WifiManager {
    /// Takes the modem peripheral (via `Peripherals::steal()` - safe here
    /// because this is called exactly once, immediately after `sysloop`
    /// exists and before anything else touches Wi-Fi) and brings up the
    /// driver in STA mode, disconnected. Call `connect`/`disconnect`/`scan`
    /// afterward as needed; this instance lives for the rest of the
    /// program.
    pub fn new(sysloop: &EspSystemEventLoop) -> Result<Self> {
        let peripherals = unsafe { Peripherals::steal() };
        let wifi = EspWifi::new(peripherals.modem, sysloop.clone(), None)
            .context("failed to create Wi-Fi driver with netif")?;
        Ok(Self {
            wifi,
            used: false,
            started: false,
            suspended_was_started: false,
        })
    }

    /// Whether `connect()` has already succeeded at least once this boot
    /// session. Purely informational now that multiple connects are safe -
    /// callers may log it for diagnostics but must not skip `connect()` on
    /// its account.
    pub fn used(&self) -> bool {
        self.used
    }

    /// Connects to `creds`, waiting for the STA link and a DHCP lease
    /// (netif up) before returning. Leaves the driver connected - call
    /// `disconnect` when done, mirroring the previous per-call
    /// connect-then-drop lifecycle but without tearing down the driver
    /// itself.
    pub fn connect(&mut self, creds: &WifiCreds) -> Result<()> {
        // Match the reference demo: keep the Wi-Fi config in RAM rather than
        // in NVS so a fresh session starts from the configuration we set here.
        esp_idf_svc::sys::esp!(unsafe {
            esp_idf_svc::sys::esp_wifi_set_storage(
                esp_idf_svc::sys::wifi_storage_t_WIFI_STORAGE_RAM,
            )
        })
        .map_err(|e| anyhow!("esp_wifi_set_storage failed: {e:?}"))?;

        let ssid: HeaplessString<32> = HeaplessString::try_from(creds.ssid.as_str())
            .map_err(|_| anyhow!("SSID longer than 32 characters"))?;
        let password: HeaplessString<64> = HeaplessString::try_from(creds.password.as_str())
            .map_err(|_| anyhow!("Wi-Fi password longer than 64 characters"))?;

        self.wifi
            .set_configuration(&Configuration::Client(ClientConfiguration {
                ssid,
                password,
                auth_method: if creds.password.is_empty() {
                    AuthMethod::None
                } else {
                    AuthMethod::WPA2Personal
                },
                ..Default::default()
            }))
            .context("failed to set Wi-Fi station configuration")?;

        if !self.started {
            self.wifi.start().context("failed to start Wi-Fi")?;
            self.started = true;
        }

        // Work around esp-idf-svc 0.52.1 hardcoding PMF to
        // `{capable: false, required: false}` in its
        // `TryFrom<&ClientConfiguration> for Newtype<wifi_sta_config_t>`
        // (`esp-idf-svc-0.52.1/src/wifi.rs`), which `set_configuration()`
        // just used above - it never reads `ClientConfiguration.pmf_cfg`,
        // so nothing settable on the Rust side changes it. Root-caused on
        // hardware (2026-08-22): a
        // WPA2/WPA3-mixed + PMF-required router accepted association but
        // then rejected the device (`assoc -> init` right after `Wi-Fi
        // connected`, before DHCP) because the driver advertised itself as
        // PMF-incapable. Same same router in WPA2-only mode connected
        // cleanly, isolating this to the PMF field specifically. Patched
        // here by reading back the config the wrapper just wrote and
        // rewriting only the PMF bits directly via the same
        // `esp_wifi_set_config()` the wrapper itself calls - `capable:
        // true, required: false` (PMF optional) is a superset of what the
        // hardcoded `false/false` allowed: still associates with
        // PMF-disabled APs, but no longer refused by a PMF-required or
        // PMF-optional one.
        unsafe {
            let mut sta_config: esp_idf_svc::sys::wifi_config_t = std::mem::zeroed();
            esp_idf_svc::sys::esp!(esp_idf_svc::sys::esp_wifi_get_config(
                esp_idf_svc::sys::wifi_interface_t_WIFI_IF_STA,
                &mut sta_config,
            ))
            .map_err(|e| anyhow!("esp_wifi_get_config (PMF patch) failed: {e:?}"))?;
            sta_config.sta.pmf_cfg.capable = true;
            sta_config.sta.pmf_cfg.required = false;
            esp_idf_svc::sys::esp!(esp_idf_svc::sys::esp_wifi_set_config(
                esp_idf_svc::sys::wifi_interface_t_WIFI_IF_STA,
                &mut sta_config,
            ))
            .map_err(|e| anyhow!("esp_wifi_set_config (PMF patch) failed: {e:?}"))?;
        }

        // No scan before connecting. Credentials always come from the
        // desktop's `SetWifi` command, so the target SSID is known and
        // `esp_wifi_connect()` connects to the configured SSID directly.
        // A manual `esp_wifi_scan_start()` here was the second-connection
        // crash trigger: it flips the WiFi state machine into "scanning"
        // (and the esp-idf-svc wrapper's `status.scan = Started` flag) that
        // a subsequent `esp_wifi_connect()` on an already-started driver
        // didn't cleanly recover from - see the struct doc comment.
        watchdog::feed();

        // Raw FFI from here, bypassing `EspWifi`'s own `connect()`/
        // `is_connected()` - the Rust wrapper tracks STA state via an
        // `Arc<Mutex<WifiDriverStatus>>` fed by its own event-loop
        // subscription, and esp-rs/esp-idf-svc#503 reports that state
        // machine panicking (`Result::unwrap() on Err('Invalid')`) on a
        // second `connect()` on one `EspWifi` - i.e. a bug in the
        // *wrapper's* reuse tracking, not necessarily the underlying
        // ESP-IDF C API. Slate (github.com/qiujun8023/slate, a working
        // reference firmware for this exact hardware) reconnects Wi-Fi
        // repeatedly in one process by calling raw `esp_wifi_connect()`/
        // `esp_wifi_disconnect()` directly - no deinit, no stop/start
        // between reconnects, matching what's left of this function after
        // switching away from the safe wrapper's connect/status calls.
        esp_idf_svc::sys::esp!(unsafe { esp_idf_svc::sys::esp_wifi_connect() })
            .map_err(|e| anyhow!("esp_wifi_connect failed: {e:?}"))?;

        // Raw `esp_wifi_sta_get_ap_info` instead of `self.wifi.is_connected()`
        // - returns ESP_OK once associated, ESP_ERR_WIFI_NOT_CONNECT until
        // then, so this polls the driver's actual state directly rather
        // than the wrapper's derived-from-events status.
        let deadline = (EspSystemTime {}).now() + WIFI_CONNECT_TIMEOUT;
        loop {
            watchdog::feed();
            let mut ap_info: esp_idf_svc::sys::wifi_ap_record_t = unsafe { std::mem::zeroed() };
            if unsafe { esp_idf_svc::sys::esp_wifi_sta_get_ap_info(&mut ap_info) } == 0 {
                log::info!("Wi-Fi connected to '{}'", creds.ssid);
                break;
            }
            if (EspSystemTime {}).now() >= deadline {
                return Err(anyhow!(
                    "timed out waiting for Wi-Fi connection to '{}'",
                    creds.ssid
                ));
            }
            thread::sleep(Duration::from_millis(500));
        }

        let netif_deadline = (EspSystemTime {}).now() + Duration::from_secs(10);
        loop {
            watchdog::feed();
            if let Ok(true) = self.wifi.sta_netif().is_up() {
                log::info!("Wi-Fi netif is up (DHCP done)");
                break;
            }
            if (EspSystemTime {}).now() >= netif_deadline {
                return Err(anyhow!("timed out waiting for DHCP lease"));
            }
            thread::sleep(Duration::from_millis(500));
        }

        self.used = true;
        Ok(())
    }

    /// Disconnects from the AP but leaves the STA interface *started*.
    /// Calling `esp_wifi_stop()` here (paired with `connect`'s `start()`
    /// call) used to be the actual crash trigger, not driver re-init as
    /// first suspected:
    /// confirmed by reproducing the exact same
    /// `Guru Meditation Error: InstrFetchProhibited` at `wifi:enable tsf`
    /// (inside `esp_wifi_start()`) on a *second* start-after-stop cycle,
    /// even after switching to this one-shared-instance design. Normal
    /// disconnects therefore leave the interface started. BLE is the
    /// exception: its explicit hand-off stops the driver to recover internal
    /// heap, then resumes it only after NimBLE has fully stopped. Safe to call
    /// even if not currently connected.
    ///
    /// Raw `esp_wifi_disconnect()` rather than `self.wifi.disconnect()`,
    /// for the same reason `connect()` bypasses the wrapper - see its
    /// comment.
    pub fn disconnect(&mut self) {
        let ret = unsafe { esp_idf_svc::sys::esp_wifi_disconnect() };
        if ret != 0 {
            log::warn!("esp_wifi_disconnect failed: 0x{ret:x}");
        }
    }

    /// Stops the Wi-Fi driver while it is owned by the sync task.  BLE and
    /// Wi-Fi cannot safely compete for the NOTE4's scarce internal DMA heap;
    /// stopping here releases the driver's run-time buffers before NimBLE
    /// controller initialization.
    pub fn suspend_for_ble(&mut self) -> Result<()> {
        if !self.started {
            return Ok(());
        }
        self.disconnect();
        esp_idf_svc::sys::esp!(unsafe { esp_idf_svc::sys::esp_wifi_stop() })
            .map_err(|e| anyhow!("esp_wifi_stop for BLE failed: {e:?}"))?;
        self.started = false;
        self.suspended_was_started = true;
        log::info!("Wi-Fi driver suspended for BLE");
        Ok(())
    }

    /// Restarts the same Wi-Fi driver instance after BLE has been torn down.
    /// Keeping construction and all calls on the sync task preserves the
    /// single-owner rule; the next sync configures and connects it normally.
    pub fn resume_after_ble(&mut self) -> Result<()> {
        if !self.suspended_was_started {
            return Ok(());
        }
        // Match suspend_for_ble's raw call.  EspWifi's event-driven started
        // cache is known to panic on a stop/start reuse cycle; the driver is
        // still the same instance and its netif remains owned by this task.
        esp_idf_svc::sys::esp!(unsafe { esp_idf_svc::sys::esp_wifi_start() })
            .map_err(|e| anyhow!("esp_wifi_start after BLE failed: {e:?}"))?;
        self.started = true;
        self.suspended_was_started = false;
        log::info!("Wi-Fi driver resumed after BLE");
        Ok(())
    }
}

/// Legacy fallback that restarts the device for a fresh Wi-Fi session.
/// Retained for compatibility and as an emergency escape hatch if a
/// second connect ever misbehaves on hardware again, but **no caller
/// depends on this anymore** - the manual scan that caused second-connect
/// crashes has been removed (see `WifiManager`'s doc comment), so multiple
/// connects per boot work. Never returns.
///
/// Goes through `power::enter_deep_sleep_with_wakeups` (a near-immediate
/// timer wake) rather than `esp_idf_svc::sys::esp_restart()`. The obvious
/// choice - `esp_restart()` - hit the same crash class this function exists
/// to avoid: even with the Wi-Fi driver never reconnected, calling it right
/// after a Wi-Fi session had been used crashed (`Guru Meditation Error:
/// Cache error` at `_UserExceptionVector`), reproduced on real hardware.
/// `esp_restart()` evidently performs its own internal Wi-Fi-aware teardown
/// that hits the same upstream instability. Deep sleep, by contrast, has
/// been exercised repeatedly and reliably in this project *after* an
/// already-used Wi-Fi session, so it's the proven path if a restart is ever
/// needed.
#[allow(dead_code)]
pub fn restart_for_fresh_wifi_session() -> ! {
    log::warn!(
        "Wi-Fi already used this boot session; power-cycling for a fresh (safe) session instead of reconnecting in-process"
    );
    // Best-effort: give any in-flight log/serial output a moment to flush
    // before sleep. Not load-bearing for correctness, just politeness.
    thread::sleep(Duration::from_millis(300));
    crate::power::restart_via_deep_sleep(Duration::from_millis(100))
}

/// Starts the SNTP client, waits for the first sync and returns the
/// obtained UTC epoch seconds without touching the RTC - used by the sync
/// task (which never accesses the I2C bus) for the daily alignment, whose
/// RTC write is applied by the main loop.
pub fn ntp_sync_epoch() -> Result<u64> {
    let sntp = EspSntp::new(&SntpConf {
        servers: ["pool.ntp.org", "ntp.aliyun.com"],
        ..Default::default()
    })
    .context("failed to start SNTP client")?;

    let deadline = EspSystemTime {}.now() + NTP_SYNC_TIMEOUT;
    loop {
        watchdog::feed();
        if sntp.get_sync_status() == SyncStatus::Completed {
            break;
        }
        if (EspSystemTime {}).now() >= deadline {
            return Err(anyhow!("timed out waiting for NTP sync"));
        }
        thread::sleep(Duration::from_millis(500));
    }
    Ok(EspSystemTime {}.now().as_secs())
}

/// Starts the SNTP client, waits for the first sync and pushes the obtained
/// time into the PCF8563 RTC through its executor client (the only owner
/// of the RTC driver).
pub fn ntp_sync_and_set_rtc(rtc: &RtcExecutor, timezone_offset_minutes: i16) -> Result<()> {
    let epoch_secs = ntp_sync_epoch()?;
    let dt = DateTime::from_unix(epoch_secs).shifted_minutes(timezone_offset_minutes as i32);
    rtc.write_time(&dt)
        .context("failed to write NTP time to PCF8563")?;
    log::info!(
        "NTP sync OK; RTC set to {:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        dt.year,
        dt.month,
        dt.day,
        dt.hour,
        dt.minute,
        dt.second
    );
    Ok(())
}
