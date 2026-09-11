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

const WIFI_CONNECT_TIMEOUT: Duration = Duration::from_secs(20);

const NTP_SYNC_TIMEOUT: Duration = Duration::from_secs(10);

pub struct WifiManager {
    wifi: Option<EspWifi<'static>>,
    sysloop: EspSystemEventLoop,
    used: bool,

    started: bool,

    suspended_was_started: bool,
}

impl WifiManager {
    pub fn new(sysloop: &EspSystemEventLoop) -> Result<Self> {
        Ok(Self {
            wifi: None,
            sysloop: sysloop.clone(),
            used: false,
            started: false,
            suspended_was_started: false,
        })
    }

    fn ensure_driver(&mut self) -> Result<&mut EspWifi<'static>> {
        if self.wifi.is_none() {
            let modem = unsafe { Peripherals::steal() }.modem;
            let wifi = EspWifi::new(modem, self.sysloop.clone(), None)
                .context("failed to create Wi-Fi driver with netif")?;
            self.wifi = Some(wifi);
            log::info!("Wi-Fi driver constructed");
        }
        Ok(self
            .wifi
            .as_mut()
            .expect("Wi-Fi driver exists after ensure_driver"))
    }

    pub fn used(&self) -> bool {
        self.used
    }

    pub fn connect(&mut self, creds: &WifiCreds) -> Result<()> {
        self.ensure_driver()?;

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

        self.ensure_driver()?
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
            self.ensure_driver()?
                .start()
                .context("failed to start Wi-Fi")?;
            self.started = true;
        }

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

        watchdog::feed();

        esp_idf_svc::sys::esp!(unsafe { esp_idf_svc::sys::esp_wifi_connect() })
            .map_err(|e| anyhow!("esp_wifi_connect failed: {e:?}"))?;

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
            if let Ok(true) = self.ensure_driver()?.sta_netif().is_up() {
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

    pub fn disconnect(&mut self) {
        if !self.started || self.wifi.is_none() {
            return;
        }
        let ret = unsafe { esp_idf_svc::sys::esp_wifi_disconnect() };
        if ret != 0 {
            log::warn!("esp_wifi_disconnect failed: 0x{ret:x}");
        }
    }

    pub fn suspend_for_ble(&mut self) -> Result<()> {
        self.suspended_was_started = self.started;
        if self.started {
            self.disconnect();
        }
        self.started = false;
        if self.wifi.take().is_some() {
            log::info!("Wi-Fi driver dropped for BLE; internal heap released");
        } else {
            log::info!("Wi-Fi driver was not constructed; BLE hand-off needs no Wi-Fi drop");
        }
        Ok(())
    }

    pub fn resume_after_ble(&mut self) -> Result<()> {
        if !self.suspended_was_started {
            return Ok(());
        }
        self.ensure_driver()?
            .start()
            .context("failed to start fresh Wi-Fi driver after BLE")?;
        self.started = true;
        self.suspended_was_started = false;
        log::info!("Fresh Wi-Fi driver resumed after BLE");
        Ok(())
    }

    pub fn abort_ble_resume(&mut self) {
        self.started = false;
        self.suspended_was_started = false;
    }
}

#[allow(dead_code)]
pub fn restart_for_fresh_wifi_session() -> ! {
    log::warn!(
        "Wi-Fi already used this boot session; power-cycling for a fresh (safe) session instead of reconnecting in-process"
    );

    thread::sleep(Duration::from_millis(300));
    crate::power::restart_via_deep_sleep(Duration::from_millis(100))
}

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
