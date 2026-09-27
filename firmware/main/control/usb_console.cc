#include "control/usb_console.h"

#include <cstring>

#include "driver/usb_serial_jtag.h"
#include "driver/usb_serial_jtag_vfs.h"
#include "esp_log.h"
#include "freertos/FreeRTOS.h"
#include "freertos/semphr.h"
#include "freertos/task.h"

namespace usb_console {
namespace {

constexpr char kTag[] = "usb";
constexpr char kCommandPrefix[] = ">>IW ";
constexpr char kReplyPrefix[] = "<<IW ";
constexpr size_t kMaxLine = 512;

std::function<void(const std::string&)> g_on_line;
SemaphoreHandle_t g_write_lock = nullptr;

void ReaderTask(void*) {
    std::string line;
    bool overflow = false;
    uint8_t buf[64];
    while (true) {
        const int n = usb_serial_jtag_read_bytes(buf, sizeof(buf), portMAX_DELAY);
        for (int i = 0; i < n; ++i) {
            const char c = static_cast<char>(buf[i]);
            if (c == '\n' || c == '\r') {
                if (!overflow && line.rfind(kCommandPrefix, 0) == 0) {
                    g_on_line(line.substr(std::strlen(kCommandPrefix)));
                } else if (overflow) {
                    ESP_LOGW(kTag, "command line longer than %u bytes dropped", unsigned(kMaxLine));
                }
                line.clear();
                overflow = false;
            } else if (line.size() < kMaxLine) {
                line.push_back(c);
            } else {
                overflow = true;
            }
        }
    }
}

}  // namespace

void Start(std::function<void(const std::string&)> on_line) {
    g_on_line = std::move(on_line);
    g_write_lock = xSemaphoreCreateMutex();
    usb_serial_jtag_driver_config_t cfg = USB_SERIAL_JTAG_DRIVER_CONFIG_DEFAULT();
    cfg.rx_buffer_size = 1024;
    cfg.tx_buffer_size = 1024;
    if (usb_serial_jtag_driver_install(&cfg) != ESP_OK) {
        ESP_LOGE(kTag, "driver install failed");
        return;
    }
    usb_serial_jtag_vfs_use_driver();
    xTaskCreate(ReaderTask, "usb-rx", 4096, nullptr, 5, nullptr);
}

void Reply(const std::string& json) {
    const std::string line = std::string(kReplyPrefix) + json + "\n";
    xSemaphoreTake(g_write_lock, portMAX_DELAY);
    usb_serial_jtag_write_bytes(line.data(), line.size(), pdMS_TO_TICKS(200));
    xSemaphoreGive(g_write_lock);
}

bool HostConnected() {
    return usb_serial_jtag_is_connected();
}

}  // namespace usb_console
