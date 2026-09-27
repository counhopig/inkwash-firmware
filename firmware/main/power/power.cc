#include "power/power.h"

#include <initializer_list>
#include <cstring>

#include "driver/gpio.h"
#include "driver/rtc_io.h"
#include "esp_attr.h"
#include "esp_log.h"
#include "esp_pm.h"
#include "esp_sleep.h"
#include "esp_system.h"
#include "freertos/FreeRTOS.h"
#include "freertos/task.h"

namespace power {
namespace {

constexpr char kTag[] = "power";
constexpr gpio_num_t kKeyEnter = GPIO_NUM_0;
constexpr gpio_num_t kRtcInt = GPIO_NUM_5;
constexpr gpio_num_t kKeyDown = GPIO_NUM_18;
constexpr gpio_num_t kKeyUp = GPIO_NUM_39;
constexpr gpio_num_t kPowerLatch = GPIO_NUM_17;
constexpr gpio_num_t kNfcPower = GPIO_NUM_21;
constexpr uint32_t kMagic = 0x494B5731;  // "IKW1"

RTC_NOINIT_ATTR Retained g_retained;
RTC_NOINIT_ATTR uint32_t g_controlled_restart;
// Separate from g_retained: that one is wiped on every non-deep-sleep reset,
// the ledger must survive a panic reset. Plain words, not boot_guard::Ledger:
// its member initializers would make a static constructor zero it at boot.
RTC_NOINIT_ATTR uint32_t g_boot_magic;
RTC_NOINIT_ATTR uint32_t g_boot_failures;

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
    gpio_set_level(kNfcPower, 0);
    gpio_hold_en(kNfcPower);
    gpio_set_level(kPowerLatch, 1);
    gpio_hold_en(kPowerLatch);
    esp_sleep_enable_gpio_switch(false);
    for (gpio_num_t pin : {kKeyEnter, kKeyDown, kKeyUp}) {
        gpio_wakeup_disable(pin);
    }
    esp_sleep_disable_wakeup_source(ESP_SLEEP_WAKEUP_GPIO);
}

}  // namespace

boot_guard::Ledger NoteBootAttempt(boot_guard::ResetKind* reset) {
    const boot_guard::ResetKind kind = ResetKindOf(esp_reset_reason());
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

void DeepSleepUntilEnter() {
    PrepareCommon();
    esp_sleep_disable_wakeup_source(ESP_SLEEP_WAKEUP_TIMER);
    esp_sleep_enable_ext1_wakeup_io(1ULL << kKeyEnter, ESP_EXT1_WAKEUP_ANY_LOW);
    ESP_LOGI(kTag, "deep sleep; wake on Enter only");
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
        if (status & (1ULL << kRtcInt)) return WakeCause::RtcAlarm;
        if (status & (1ULL << kKeyEnter)) return WakeCause::Enter;
        if (status & (1ULL << kKeyDown)) return WakeCause::Down;
    }
    return WakeCause::PowerOn;
}

void EnableLightSleep() {
    for (gpio_num_t pin : {kKeyEnter, kKeyDown, kKeyUp}) {
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

void DeepSleep(int64_t timer_secs) {
    PrepareCommon();
    const uint64_t mask = (1ULL << kKeyEnter) | (1ULL << kRtcInt) | (1ULL << kKeyDown);
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
    esp_deep_sleep_start();
}

void Restart() {
    PrepareCommon();
    g_controlled_restart = kMagic;
    esp_sleep_enable_timer_wakeup(100 * 1000);
    ESP_LOGI(kTag, "controlled restart through deep sleep");
    vTaskDelay(pdMS_TO_TICKS(20));
    esp_deep_sleep_start();
}

}  // namespace power
