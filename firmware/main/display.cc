#include "display.h"

#include <algorithm>
#include <cstring>
#include <vector>

#include "esp_heap_caps.h"
#include "esp_log.h"
#include "esp_timer.h"
#include "zectrix_epd.h"

namespace display {
namespace {

constexpr char kTag[] = "display";
constexpr int kWidth = ZECTRIX_EPD_PANEL_WIDTH;
constexpr int kHeight = ZECTRIX_EPD_PANEL_HEIGHT;
constexpr int kRowBytes = kWidth / 8;
constexpr int kRenderRows = 40;
// Partial refreshes accumulate ghosting; clear it with a full refresh.
constexpr int kPartialsBeforeFull = 10;

zectrix_epd_handle_t g_epd = nullptr;
lv_display_t* g_display = nullptr;
uint8_t* g_frame = nullptr;  // panel format: row-major, MSB first, 1 = white
bool g_have_base = false;    // a full refresh has put g_frame on the panel
int g_partials = 0;

struct Dirty {
    int x1 = kWidth, y1 = kHeight, x2 = -1, y2 = -1;
    bool Empty() const { return x2 < x1 || y2 < y1; }
    void Add(int ax1, int ay1, int ax2, int ay2) {
        x1 = std::min(x1, ax1);
        y1 = std::min(y1, ay1);
        x2 = std::max(x2, ax2);
        y2 = std::max(y2, ay2);
    }
} g_dirty;

uint32_t TickMs() {
    return static_cast<uint32_t>(esp_timer_get_time() / 1000);
}

bool IsWhite(uint16_t rgb565) {
    const uint32_t r = ((rgb565 >> 11) & 0x1F) << 3;
    const uint32_t g = ((rgb565 >> 5) & 0x3F) << 2;
    const uint32_t b = (rgb565 & 0x1F) << 3;
    // Rec. 601 luma; mid-grey splits black from white.
    return r * 77 + g * 150 + b * 29 >= 128u * 256u;
}

void Flush(lv_display_t* disp, const lv_area_t* area, uint8_t* px) {
    const uint16_t* src = reinterpret_cast<const uint16_t*>(px);
    const int src_w = area->x2 - area->x1 + 1;
    const int x1 = std::max<int>(0, area->x1);
    const int y1 = std::max<int>(0, area->y1);
    const int x2 = std::min<int>(kWidth - 1, area->x2);
    const int y2 = std::min<int>(kHeight - 1, area->y2);
    for (int y = y1; y <= y2; ++y) {
        const uint16_t* row = src + (y - area->y1) * src_w + (x1 - area->x1);
        uint8_t* dst = g_frame + y * kRowBytes;
        for (int x = x1; x <= x2; ++x) {
            const uint8_t mask = static_cast<uint8_t>(0x80 >> (x & 7));
            if (IsWhite(row[x - x1])) {
                dst[x >> 3] |= mask;
            } else {
                dst[x >> 3] &= static_cast<uint8_t>(~mask);
            }
        }
    }
    if (x1 <= x2 && y1 <= y2) {
        g_dirty.Add(x1, y1, x2, y2);
    }
    lv_display_flush_ready(disp);
}

esp_err_t RefreshFull() {
    esp_err_t err = zectrix_epd_power_on(g_epd);
    if (err == ESP_OK) {
        err = zectrix_epd_refresh_full_1bpp(g_epd, g_frame, ZECTRIX_EPD_1BPP_FRAME_BYTES);
    }
    const esp_err_t off = zectrix_epd_power_off(g_epd);
    return err != ESP_OK ? err : off;
}

esp_err_t RefreshPartial(int x1, int y1, int x2, int y2) {
    // The controller addresses whole bytes horizontally.
    x1 &= ~7;
    x2 |= 7;
    x2 = std::min(x2, kWidth - 1);
    const int w = x2 - x1 + 1;
    const int h = y2 - y1 + 1;
    const int row_bytes = w / 8;
    std::vector<uint8_t> pixels(static_cast<size_t>(row_bytes) * h);
    for (int y = 0; y < h; ++y) {
        std::memcpy(&pixels[static_cast<size_t>(y) * row_bytes],
                    g_frame + (y1 + y) * kRowBytes + x1 / 8, row_bytes);
    }
    zectrix_epd_rect_t rect = {x1, y1, w, h};
    esp_err_t err = zectrix_epd_power_on(g_epd);
    if (err == ESP_OK) {
        err = zectrix_epd_refresh_partial_1bpp(g_epd, &rect, pixels.data(), pixels.size());
    }
    const esp_err_t off = zectrix_epd_power_off(g_epd);
    return err != ESP_OK ? err : off;
}

}  // namespace

bool Init() {
    zectrix_epd_config_t cfg;
    zectrix_epd_get_default_config(&cfg);
    if (zectrix_epd_new(&cfg, &g_epd) != ESP_OK) {
        ESP_LOGE(kTag, "EPD init failed");
        return false;
    }
    g_frame = static_cast<uint8_t*>(
        heap_caps_malloc(ZECTRIX_EPD_1BPP_FRAME_BYTES, MALLOC_CAP_SPIRAM));
    constexpr size_t kRenderBytes = kWidth * kRenderRows * 2;
    void* render = heap_caps_malloc(kRenderBytes, MALLOC_CAP_SPIRAM);
    if (!g_frame || !render) {
        ESP_LOGE(kTag, "frame buffers unavailable");
        return false;
    }
    std::memset(g_frame, 0xFF, ZECTRIX_EPD_1BPP_FRAME_BYTES);

    lv_init();
    lv_tick_set_cb(TickMs);
    g_display = lv_display_create(kWidth, kHeight);
    lv_display_set_color_format(g_display, LV_COLOR_FORMAT_RGB565);
    lv_display_set_flush_cb(g_display, Flush);
    lv_display_set_buffers(g_display, render, nullptr, kRenderBytes,
                           LV_DISPLAY_RENDER_MODE_PARTIAL);
    // Nothing animates on e-paper; LVGL renders only when Update() asks.
    lv_timer_t* refr = lv_display_get_refr_timer(g_display);
    if (refr) {
        lv_timer_pause(refr);
    }
    return true;
}

lv_display_t* Lvgl() {
    return g_display;
}

void Update(bool full) {
    lv_timer_handler();
    lv_refr_now(g_display);
    const bool need_full = full || !g_have_base || g_partials >= kPartialsBeforeFull;
    if (!need_full && g_dirty.Empty()) {
        return;
    }
    const int64_t start = esp_timer_get_time();
    esp_err_t err;
    if (need_full) {
        err = RefreshFull();
        if (err == ESP_OK) {
            g_have_base = true;
            g_partials = 0;
        }
    } else {
        err = RefreshPartial(g_dirty.x1, g_dirty.y1, g_dirty.x2, g_dirty.y2);
        if (err != ESP_OK) {
            ESP_LOGW(kTag, "partial refresh failed (0x%x); falling back to full", err);
            err = RefreshFull();
            g_partials = 0;
        } else {
            ++g_partials;
        }
    }
    ESP_LOGI(kTag, "%s refresh (%d,%d)-(%d,%d) %s in %lld ms", need_full ? "full" : "partial",
             g_dirty.x1, g_dirty.y1, g_dirty.x2, g_dirty.y2, err == ESP_OK ? "ok" : "FAILED",
             static_cast<long long>((esp_timer_get_time() - start) / 1000));
    g_dirty = Dirty{};
}

}  // namespace display
