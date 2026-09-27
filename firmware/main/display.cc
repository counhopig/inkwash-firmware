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
constexpr size_t kFrameBytes = ZECTRIX_EPD_1BPP_FRAME_BYTES;
constexpr int kRenderRows = 40;
constexpr int kMaxPartials = 20;
constexpr int kMinPartialsBeforeBigFull = 5;

zectrix_epd_handle_t g_epd = nullptr;
lv_display_t* g_display = nullptr;
uint8_t* g_frame = nullptr;  // rendered frame: row-major, MSB first, 1 = white
uint8_t* g_panel = nullptr;  // what the panel shows
bool g_have_base = false;
uint8_t g_partials = 0;

uint32_t TickMs() { return static_cast<uint32_t>(esp_timer_get_time() / 1000); }

bool IsWhite(uint16_t rgb565) {
    const uint32_t r = ((rgb565 >> 11) & 0x1F) << 3;
    const uint32_t g = ((rgb565 >> 5) & 0x3F) << 2;
    const uint32_t b = (rgb565 & 0x1F) << 3;
    return r * 77 + g * 150 + b * 29 >= 128u * 256u;  // Rec. 601 luma, mid-grey split
}

void Flush(lv_display_t* disp, const lv_area_t* area, uint8_t* px) {
    const uint16_t* src = reinterpret_cast<const uint16_t*>(px);
    const int src_w = area->x2 - area->x1 + 1;
    const int x1 = std::max<int>(0, area->x1), y1 = std::max<int>(0, area->y1);
    const int x2 = std::min<int>(kWidth - 1, area->x2), y2 = std::min<int>(kHeight - 1, area->y2);
    for (int y = y1; y <= y2; ++y) {
        const uint16_t* row = src + (y - area->y1) * src_w + (x1 - area->x1);
        uint8_t* dst = g_frame + y * kRowBytes;
        for (int x = x1; x <= x2; ++x) {
            const uint8_t mask = static_cast<uint8_t>(0x80 >> (x & 7));
            if (IsWhite(row[x - x1])) dst[x >> 3] |= mask;
            else dst[x >> 3] &= static_cast<uint8_t>(~mask);
        }
    }
    lv_display_flush_ready(disp);
}

// Bounding box of the bytes that differ between frame and panel.
bool ChangedArea(int* x1, int* y1, int* x2, int* y2) {
    int bx1 = kRowBytes, by1 = kHeight, bx2 = -1, by2 = -1;
    for (int y = 0; y < kHeight; ++y) {
        const uint8_t* a = g_frame + y * kRowBytes;
        const uint8_t* b = g_panel + y * kRowBytes;
        if (std::memcmp(a, b, kRowBytes) == 0) continue;
        by1 = std::min(by1, y);
        by2 = y;
        for (int x = 0; x < kRowBytes; ++x) {
            if (a[x] != b[x]) {
                bx1 = std::min(bx1, x);
                bx2 = std::max(bx2, x);
            }
        }
    }
    if (bx2 < 0) return false;
    *x1 = bx1 * 8;
    *x2 = bx2 * 8 + 7;
    *y1 = by1;
    *y2 = by2;
    return true;
}

esp_err_t RefreshFull() {
    esp_err_t err = zectrix_epd_power_on(g_epd);
    if (err == ESP_OK) err = zectrix_epd_refresh_full_1bpp(g_epd, g_frame, kFrameBytes);
    const esp_err_t off = zectrix_epd_power_off(g_epd);
    return err != ESP_OK ? err : off;
}

esp_err_t RefreshPartial(int x1, int y1, int x2, int y2) {
    const int w = x2 - x1 + 1, h = y2 - y1 + 1, row_bytes = w / 8;
    std::vector<uint8_t> pixels(static_cast<size_t>(row_bytes) * h);
    for (int y = 0; y < h; ++y) {
        std::memcpy(&pixels[static_cast<size_t>(y) * row_bytes], g_frame + (y1 + y) * kRowBytes + x1 / 8,
                    row_bytes);
    }
    const zectrix_epd_rect_t rect = {x1, y1, w, h};
    esp_err_t err = zectrix_epd_power_on(g_epd);
    if (err == ESP_OK) err = zectrix_epd_refresh_partial_1bpp(g_epd, &rect, pixels.data(), pixels.size());
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
    g_frame = static_cast<uint8_t*>(heap_caps_malloc(kFrameBytes, MALLOC_CAP_SPIRAM));
    g_panel = static_cast<uint8_t*>(heap_caps_malloc(kFrameBytes, MALLOC_CAP_SPIRAM));
    constexpr size_t kRenderBytes = kWidth * kRenderRows * 2;
    void* render = heap_caps_malloc(kRenderBytes, MALLOC_CAP_SPIRAM);
    if (!g_frame || !g_panel || !render) {
        ESP_LOGE(kTag, "frame buffers unavailable");
        return false;
    }
    std::memset(g_frame, 0xFF, kFrameBytes);
    std::memset(g_panel, 0xFF, kFrameBytes);

    lv_init();
    lv_tick_set_cb(TickMs);
    g_display = lv_display_create(kWidth, kHeight);
    lv_display_set_color_format(g_display, LV_COLOR_FORMAT_RGB565);
    lv_display_set_flush_cb(g_display, Flush);
    lv_display_set_buffers(g_display, render, nullptr, kRenderBytes, LV_DISPLAY_RENDER_MODE_PARTIAL);
    // Nothing animates on e-paper: render only when Update() asks.
    if (lv_timer_t* refr = lv_display_get_refr_timer(g_display)) lv_timer_pause(refr);
    return true;
}

lv_display_t* Lvgl() { return g_display; }

uint8_t PartialRefreshes() { return g_partials; }

void AdoptPanelContent(uint8_t partial_refreshes) {
    lv_obj_invalidate(lv_screen_active());
    lv_refr_now(g_display);
    std::memcpy(g_panel, g_frame, kFrameBytes);
    if (zectrix_epd_seed_shadow_1bpp(g_epd, g_panel, kFrameBytes) == ESP_OK) {
        g_have_base = true;
        g_partials = partial_refreshes;
    }
}

void Update(Refresh mode) {
    lv_timer_handler();
    lv_refr_now(g_display);
    int x1 = 0, y1 = 0, x2 = 0, y2 = 0;
    const bool changed = ChangedArea(&x1, &y1, &x2, &y2);
    if (mode != Refresh::Full && g_have_base && !changed) return;
    const bool big = changed && (x2 - x1 + 1) * (y2 - y1 + 1) > kWidth * kHeight * 7 / 10;
    const bool full = mode == Refresh::Full || !g_have_base || g_partials >= kMaxPartials ||
                      (big && g_partials >= kMinPartialsBeforeBigFull);
    const int64_t start = esp_timer_get_time();
    esp_err_t err;
    if (full) {
        err = RefreshFull();
    } else {
        err = RefreshPartial(x1, y1, x2, y2);
        if (err != ESP_OK) {
            ESP_LOGW(kTag, "partial refresh failed (0x%x); falling back to full", err);
            err = RefreshFull();
            if (err == ESP_OK) g_partials = 0;
        } else {
            ++g_partials;
        }
    }
    if (err == ESP_OK) {
        std::memcpy(g_panel, g_frame, kFrameBytes);
        if (full) {
            g_have_base = true;
            g_partials = 0;
        }
    } else {
        g_have_base = false;  // unknown panel state: next refresh is full
    }
    ESP_LOGI(kTag, "%s refresh (%d,%d)-(%d,%d) %s in %lld ms", full ? "full" : "partial", x1, y1,
             x2, y2, err == ESP_OK ? "ok" : "FAILED",
             static_cast<long long>((esp_timer_get_time() - start) / 1000));
}

}  // namespace display
