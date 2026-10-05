#include "net/wifi.h"

#include <ctime>
#include <atomic>
#include <algorithm>
#include <cstring>

#include "esp_crt_bundle.h"
#include "esp_event.h"
#include "esp_http_client.h"
#include "esp_log.h"
#include "esp_netif.h"
#include "esp_netif_sntp.h"
#include "esp_timer.h"
#include "esp_wifi.h"
#include "freertos/FreeRTOS.h"
#include "freertos/event_groups.h"

namespace wifi {
namespace {

constexpr char kTag[] = "wifi";
constexpr int kConnectTimeoutMs = 20000;
constexpr int kNtpTimeoutMs = 10000;
constexpr int kHttpTimeoutMs = 5000;
constexpr EventBits_t kGotIp = BIT0;
constexpr EventBits_t kFailed = BIT1;

bool g_initialized = false;
bool g_started = false;
std::atomic<bool> g_connected{false};
esp_netif_t* g_netif = nullptr;
EventGroupHandle_t g_events = nullptr;

void OnEvent(void*, esp_event_base_t base, int32_t id, void* data) {
    if (base == WIFI_EVENT && id == WIFI_EVENT_STA_DISCONNECTED) {
        g_connected.store(false);
        const auto* info = static_cast<wifi_event_sta_disconnected_t*>(data);
        ESP_LOGW(kTag, "disconnected (reason %d)", info ? info->reason : -1);
        xEventGroupSetBits(g_events, kFailed);
    } else if (base == IP_EVENT && id == IP_EVENT_STA_GOT_IP) {
        g_connected.store(true);
        xEventGroupSetBits(g_events, kGotIp);
    } else if ((base == WIFI_EVENT && id == WIFI_EVENT_STA_STOP) ||
               (base == IP_EVENT && id == IP_EVENT_STA_LOST_IP)) {
        g_connected.store(false);
    }
}

bool EnsureInit() {
    if (g_initialized) return true;
    esp_netif_init();
    esp_err_t err = esp_event_loop_create_default();
    if (err != ESP_OK && err != ESP_ERR_INVALID_STATE) return false;
    g_netif = esp_netif_create_default_wifi_sta();
    wifi_init_config_t cfg = WIFI_INIT_CONFIG_DEFAULT();
    if (esp_wifi_init(&cfg) != ESP_OK) {
        ESP_LOGE(kTag, "esp_wifi_init failed");
        return false;
    }
    esp_wifi_set_storage(WIFI_STORAGE_RAM);
    esp_wifi_set_mode(WIFI_MODE_STA);
    g_events = xEventGroupCreate();
    esp_event_handler_register(WIFI_EVENT, ESP_EVENT_ANY_ID, OnEvent, nullptr);
    esp_event_handler_register(IP_EVENT, ESP_EVENT_ANY_ID, OnEvent, nullptr);
    g_initialized = true;
    return true;
}

// Waits for a disconnect that happens while still associating: the driver
// reports it per attempt, so retry until the overall deadline.
bool WaitForIp(int64_t deadline_us) {
    while (true) {
        const int64_t left_ms = (deadline_us - esp_timer_get_time()) / 1000;
        if (left_ms <= 0) return false;
        const EventBits_t bits = xEventGroupWaitBits(g_events, kGotIp | kFailed, pdTRUE, pdFALSE,
                                                     pdMS_TO_TICKS(left_ms));
        if (bits & kGotIp) return true;
        if (bits & kFailed) {
            vTaskDelay(pdMS_TO_TICKS(500));
            esp_wifi_connect();
            continue;
        }
        return false;
    }
}

}  // namespace

bool UsedThisBoot() {
    return g_initialized;
}

bool IsConnected() {
    return g_connected.load();
}

bool Connect(const store::WifiCreds& creds, std::string* error) {
    if (!EnsureInit()) {
        *error = "Wi-Fi driver initialization failed";
        return false;
    }
    wifi_config_t cfg = {};
    std::strncpy(reinterpret_cast<char*>(cfg.sta.ssid), creds.ssid.c_str(), sizeof(cfg.sta.ssid));
    std::memcpy(cfg.sta.password, creds.password.data(),
                std::min(creds.password.size(), sizeof(cfg.sta.password)));
    cfg.sta.threshold.authmode = creds.password.empty() ? WIFI_AUTH_OPEN : WIFI_AUTH_WPA2_PSK;
    cfg.sta.pmf_cfg.capable = true;
    cfg.sta.pmf_cfg.required = false;
    cfg.sta.sae_pwe_h2e = WPA3_SAE_PWE_BOTH;
    if (esp_wifi_set_config(WIFI_IF_STA, &cfg) != ESP_OK) {
        *error = "failed to set Wi-Fi station configuration";
        return false;
    }
    xEventGroupClearBits(g_events, kGotIp | kFailed);
    if (!g_started) {
        if (esp_wifi_start() != ESP_OK) {
            *error = "failed to start Wi-Fi";
            return false;
        }
        g_started = true;
    }
    esp_wifi_connect();
    if (!WaitForIp(esp_timer_get_time() + kConnectTimeoutMs * 1000LL)) {
        *error = "timed out waiting for Wi-Fi connection to '" + creds.ssid + "'";
        Disconnect();
        return false;
    }
    ESP_LOGI(kTag, "connected to '%s'", creds.ssid.c_str());
    return true;
}

void Disconnect() {
    g_connected.store(false);
    if (!g_started) return;
    esp_wifi_disconnect();
    esp_wifi_stop();
    g_started = false;
}

bool NtpEpoch(uint64_t* out_utc) {
    esp_sntp_config_t cfg = ESP_NETIF_SNTP_DEFAULT_CONFIG_MULTIPLE(
        2, ESP_SNTP_SERVER_LIST("pool.ntp.org", "ntp.aliyun.com"));
    if (esp_netif_sntp_init(&cfg) != ESP_OK) return false;
    const bool ok = esp_netif_sntp_sync_wait(pdMS_TO_TICKS(kNtpTimeoutMs)) == ESP_OK;
    esp_netif_sntp_deinit();
    if (!ok) {
        ESP_LOGW(kTag, "NTP sync timed out");
        return false;
    }
    time_t now = 0;
    time(&now);
    *out_utc = static_cast<uint64_t>(now);
    return true;
}

bool HttpsPost(const std::string& url, const std::string& token, const char* extra_header,
               const std::string& request, size_t max_len, std::string* body,
               std::string* error) {
    esp_http_client_config_t cfg = {};
    cfg.url = url.c_str();
    cfg.method = HTTP_METHOD_POST;
    cfg.timeout_ms = kHttpTimeoutMs;
    cfg.crt_bundle_attach = esp_crt_bundle_attach;
    esp_http_client_handle_t client = esp_http_client_init(&cfg);
    if (!client) {
        *error = "HTTP connection setup failed";
        return false;
    }
    esp_http_client_set_header(client, "accept", "application/json");
    esp_http_client_set_header(client, "content-type", "application/json");
    if (extra_header) {
        const char* colon = std::strchr(extra_header, ':');
        if (colon) {
            const std::string name(extra_header, colon - extra_header);
            esp_http_client_set_header(client, name.c_str(), colon + 1);
        }
    }
    std::string auth;
    if (!token.empty()) {
        auth = "Bearer " + token;
        esp_http_client_set_header(client, "authorization", auth.c_str());
    }
    bool ok = false;
    if (esp_http_client_open(client, static_cast<int>(request.size())) != ESP_OK) {
        *error = "POST " + url + " failed to start";
    } else if (esp_http_client_write(client, request.data(), static_cast<int>(request.size())) !=
               static_cast<int>(request.size())) {
        *error = "POST " + url + " body write failed";
    } else if (esp_http_client_fetch_headers(client) < 0) {
        *error = "POST " + url + " failed";
    } else if (const int status = esp_http_client_get_status_code(client); status != 200) {
        *error = "HTTP " + std::to_string(status);
    } else {
        body->clear();
        char chunk[512];
        ok = true;
        while (true) {
            const int n = esp_http_client_read(client, chunk, sizeof(chunk));
            if (n < 0) {
                *error = "HTTP response read failed";
                ok = false;
                break;
            }
            if (n == 0) break;
            if (body->size() + n > max_len) {
                *error = "sync response exceeded the device buffer";
                ok = false;
                break;
            }
            body->append(chunk, n);
        }
    }
    esp_http_client_close(client);
    esp_http_client_cleanup(client);
    return ok;
}

}  // namespace wifi
