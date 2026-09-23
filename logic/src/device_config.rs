#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WifiCreds {
    pub ssid: String,
    pub password: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeviceConfig {
    pub server_url: String,
    pub auth_token: String,
}
