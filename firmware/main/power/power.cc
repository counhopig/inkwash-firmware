#include "board_pins.h"
#include "power/power.h"

#include <initializer_list>
#include <cstring>

#include "driver/gpio.h"
#include "core/sleep_trace.h"
#include "driver/rtc_io.h"
#include "esp_attr.h"
#include "esp_log.h"
#include "esp_pm.h"
#include "esp_sleep.h"
#include "esp_system.h"
#include "esp_timer.h"
#include "freertos/FreeRTOS.h"
#include "freertos/task.h"

namespace power {
namespace {

constexpr char kTag[] = "power";
constexpr uint32_t kMagic = 0x494B5732;  // "IKW2"

RTC_NOINIT_ATTR Retained g_retained;
RTC_NOINIT_ATTR uint32_t g_controlled_restart;
// Separate from g_retained: that one is wiped on every non-deep-sleep reset,
// the ledger must survive a panic reset. Plain words, not boot_guard::Ledger:
// its member initializers would make a static constructor zero it at boot.
RTC_NOINIT_ATTR uint32_t g_boot_magic;
RTC_NOINIT_ATTR uint32_t g_boot_failures;
RTC_NOINIT_ATTR sleep_trace::Ring g_sleep_trace;

void Trace(sleep_trace::Kind kind, int64_t timer_secs, uint64_t utc_secs, uint64_t mask) {
    sleep_trace::Entry e = {};
    e.kind = kind;
    e.uptime_ms = static_cast<uint32_t>(esp_timer_get_time() / 1000);
    e.utc_secs = utc_secs;
    e.timer_secs = timer_secs;
    e.wake_mask = mask;
    sleep_trace::Append(&g_sleep_trace, e);
}

boot_guard::ResetKind ResetKindOf(esp_reset_reason_t reason) {
    using boot_guard::ResetKind;
    switch (reason) {
        case ESP_RST_POWERON: return ResetKind::PowerOn;
        case ESP_RST_EXT: return ResetKind::ExternalReset;
        case ESP_RST_SW: return ResetKind::SoftwareReset;
        case ESP_RST_PANIC: return ResetKind::Panic;
        case ESP_RST_INT_WDT: return ResetKind::InterruptWatchdog;
        case ESP_RST_TASK_WDT: return ResetKind::TaskWatchdog;
        case ESP_RST_WDT: return ResetKind::Watchdog;
        case ESP_RST_DEEPSLEEP: return ResetKind::DeepSleep;
        case ESP_RST_BROWNOUT: return ResetKind::Brownout;
        case ESP_RST_USB: return ResetKind::Usb;
        case ESP_RST_JTAG: return ResetKind::Jtag;
        case ESP_RST_PWR_GLITCH: return ResetKind::PowerGlitch;
        case ESP_RST_CPU_LOCKUP: return ResetKind::CpuLockup;
        default: return ResetKind::Other;
    }
}

void PrepareCommon() {
    // Keep the NFC tag unpowered and the battery latched on through sleep.
    gpio_set_level(board::pins::NfcPower, 0);
    gpio_hold_en(board::pins::NfcPower);
    gpio_set_level(board::pins::PowerLatch, 1);
    gpio_hold_en(board::pins::PowerLatch);
    esp_sleep_enable_gpio_switch(false);
    for (gpio_num_t pin : {board::pins::KeyEnter, board::pins::KeyDown, board::pins::KeyUp}) {
        gpio_wakeup_disable(pin);
    }
    esp_sleep_disable_wakeup_source(ESP_SLEEP_WAKEUP_GPIO);
}

void PrepareWakeInput(gpio_num_t pin) {
    ESP_ERROR_CHECK(rtc_gpio_init(pin));
    ESP_ERROR_CHECK(rtc_gpio_set_direction(pin, RTC_GPIO_MODE_INPUT_ONLY));
    ESP_ERROR_CHECK(rtc_gpio_pulldown_dis(pin));
    ESP_ERROR_CHECK(rtc_gpio_pullup_en(pin));
    // Keep active-low inputs high when their external pull-up rail is off.
    ESP_ERROR_CHECK(rtc_gpio_hold_en(pin));
}

}  // namespace

boot_guard::Ledger NoteBootAttempt(boot_guard::ResetKind* reset) {
    const boot_guard::ResetKind kind = ResetKindOf(esp_reset_reason());
    if (kind == boot_guard::ResetKind::PowerOn) std::memset(&g_sleep_trace, 0, sizeof(g_sleep_trace));
    sleep_trace::Entry e = {};
    e.kind = sleep_trace::Kind::Boot;
    e.reset = static_cast<uint8_t>(kind);
    e.wake = static_cast<uint8_t>(esp_sleep_get_wakeup_cause());
    if (esp_sleep_get_wakeup_cause() == ESP_SLEEP_WAKEUP_EXT1) e.wake_mask = esp_sleep_get_ext1_wakeup_status();
    sleep_trace::Append(&g_sleep_trace, e);
    const boot_guard::Ledger next = boot_guard::Ledger{g_boot_magic, g_boot_failures}.NoteAttempt(kind);
    g_boot_magic = next.magic;
    g_boot_failures = next.failures;
    if (reset) *reset = kind;
    return next;
}

void ClearBootLedger() {
    g_boot_magic = boot_guard::kMagic;
    g_boot_failures = 0;
}

void PrintSleepTrace(uint64_t boot_utc_secs) {
    sleep_trace::SetBootTime(&g_sleep_trace, boot_utc_secs);
    for (size_t i = 0; i < g_sleep_trace.count; ++i) {
        const auto* e = sleep_trace::At(g_sleep_trace, i);
        if (!e) break;
        const char* kind = e->kind == sleep_trace::Kind::Boot ? "boot" :
                           e->kind == sleep_trace::Kind::Sleep ? "sleep" :
                           e->kind == sleep_trace::Kind::SafeSleep ? "safe-sleep" : "restart";
        const char* wake = "none";
        if (e->wake == ESP_SLEEP_WAKEUP_TIMER) wake = "timer";
        if (e->wake == ESP_SLEEP_WAKEUP_EXT1) {
            wake = e->wake_mask & (1ULL << board::pins::RtcInt) ? "rtc-alarm" :
                   e->wake_mask & (1ULL << board::pins::KeyEnter) ? "enter" :
                   e->wake_mask & (1ULL << board::pins::KeyDown) ? "down" : "external";
        }
        const auto reset = e->reset <= static_cast<uint8_t>(boot_guard::ResetKind::Other) ?
            static_cast<boot_guard::ResetKind>(e->reset) : boot_guard::ResetKind::Other;
        ESP_LOGI(kTag, "SLEEPTRACE seq=%lu event=%s utc_s=%llu up_ms=%lu timer_s=%lld reset=%s wake=%s mask=0x%llx",
                 static_cast<unsigned long>(e->sequence), kind,
                 static_cast<unsigned long long>(e->utc_secs), static_cast<unsigned long>(e->uptime_ms),
                 static_cast<long long>(e->timer_secs), e->kind == sleep_trace::Kind::Boot ? boot_guard::Label(reset) : "none",
                 wake, static_cast<unsigned long long>(e->wake_mask));
    }
}

void DeepSleepUntilEnter() {
    PrepareWakeInput(board::pins::KeyEnter);
    PrepareCommon();
    esp_sleep_disable_wakeup_source(ESP_SLEEP_WAKEUP_TIMER);
    esp_sleep_enable_ext1_wakeup_io(1ULL << board::pins::KeyEnter, ESP_EXT1_WAKEUP_ANY_LOW);
    ESP_LOGI(kTag, "deep sleep; wake on Enter only");
    Trace(sleep_trace::Kind::SafeSleep, -1, 0, 1ULL << board::pins::KeyEnter);
    vTaskDelay(pdMS_TO_TICKS(20));
    esp_deep_sleep_start();
}

Retained& State() {
    if (g_retained.magic != kMagic) {
        std::memset(&g_retained, 0, sizeof(g_retained));
        g_retained.magic = kMagic;
    }
    return g_retained;
}

WakeCause ReadWakeCause() {
    const esp_sleep_wakeup_cause_t cause = esp_sleep_get_wakeup_cause();
    const bool controlled = g_controlled_restart == kMagic;
    g_controlled_restart = 0;
    if (esp_reset_reason() != ESP_RST_DEEPSLEEP) {
        // A power-on or reset leaves RTC memory undefined.
        std::memset(&g_retained, 0, sizeof(g_retained));
        g_retained.magic = kMagic;
        return WakeCause::PowerOn;
    }
    if (cause == ESP_SLEEP_WAKEUP_TIMER) {
        return controlled ? WakeCause::ControlledRestart : WakeCause::Timer;
    }
    if (cause == ESP_SLEEP_WAKEUP_EXT1) {
        const uint64_t status = esp_sleep_get_ext1_wakeup_status();
        if (status & (1ULL << board::pins::RtcInt)) return WakeCause::RtcAlarm;
        if (status & (1ULL << board::pins::KeyEnter)) return WakeCause::Enter;
        if (status & (1ULL << board::pins::KeyDown)) return WakeCause::Down;
    }
    return WakeCause::PowerOn;
}

void EnableLightSleep() {
    for (gpio_num_t pin : {board::pins::KeyEnter, board::pins::KeyDown, board::pins::KeyUp}) {
        gpio_wakeup_enable(pin, GPIO_INTR_LOW_LEVEL);
    }
    esp_sleep_enable_gpio_wakeup();
    esp_pm_config_t cfg = {};
    cfg.max_freq_mhz = CONFIG_ESP_DEFAULT_CPU_FREQ_MHZ;
    cfg.min_freq_mhz = 40;
    cfg.light_sleep_enable = true;
    if (esp_pm_configure(&cfg) != ESP_OK) {
        ESP_LOGW(kTag, "automatic light sleep unavailable");
    }
}

void DeepSleep(int64_t timer_secs, uint64_t utc_secs) {
    PrepareCommon();
    for (gpio_num_t pin : {board::pins::KeyEnter, board::pins::RtcInt, board::pins::KeyDown}) PrepareWakeInput(pin);
    const uint64_t mask = (1ULL << board::pins::KeyEnter) | (1ULL << board::pins::RtcInt) | (1ULL << board::pins::KeyDown);
    esp_sleep_enable_ext1_wakeup_io(mask, ESP_EXT1_WAKEUP_ANY_LOW);
    if (timer_secs > 0) {
        esp_sleep_enable_timer_wakeup(static_cast<uint64_t>(timer_secs) * 1000000ULL);
        ESP_LOGI(kTag, "deep sleep; wake on keys, RTC alarm or in %llds", (long long)timer_secs);
    } else {
        // Automatic light sleep leaves a timer source armed; clear it.
        esp_sleep_disable_wakeup_source(ESP_SLEEP_WAKEUP_TIMER);
        ESP_LOGI(kTag, "deep sleep; wake on keys or RTC alarm");
    }
    vTaskDelay(pdMS_TO_TICKS(20));  // let the log drain
    Trace(sleep_trace::Kind::Sleep, timer_secs, utc_secs, mask);
    esp_deep_sleep_start();
}

void Restart() {
    PrepareCommon();
    g_controlled_restart = kMagic;
    esp_sleep_enable_timer_wakeup(100 * 1000);
    ESP_LOGI(kTag, "controlled restart through deep sleep");
    Trace(sleep_trace::Kind::Restart, 0, 0, 0);
    vTaskDelay(pdMS_TO_TICKS(20));
    esp_deep_sleep_start();
}

}  // namespace power
