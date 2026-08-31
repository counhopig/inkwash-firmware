//! Device configuration data shapes, moved out of
//! `rust-firmware/src/storage.rs` (which re-exports them) so the
//! host-testable application state machine can reference them without the
//! NVS driver.

/// Wi-Fi credentials the device connects with to reach the sync server.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WifiCreds {
    pub ssid: String,
    pub password: String,
}

/// Server endpoint configuration for the sync client.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeviceConfig {
    pub server_url: String,
    pub auth_token: String,
}
