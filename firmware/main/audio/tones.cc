#include "diagnostics/event_log.h"
#include "board_pins.h"
#include "audio/tones.h"

#include <cmath>
#include <atomic>

#include "board.h"
#include "driver/gpio.h"
#include "driver/i2s_std.h"
#include "esp_log.h"
#include "freertos/FreeRTOS.h"
#include "freertos/queue.h"
#include "freertos/task.h"

namespace tones {
namespace {

constexpr char kTag[] = "tones";
constexpr uint16_t kCodecAddress = 0x18;
constexpr uint32_t kSampleRate = 16000;

// Device alert tones.
constexpr float kAlarmHz = 880.0f;
constexpr float kAlarmBurstSecs = 0.05f;
constexpr int kAlarmGapMs = 150;
constexpr float kSirenNotes[2][2] = {{1397.0f, 0.12f}, {1046.0f, 0.12f}};
constexpr int16_t kSirenAmplitude = 24000;
constexpr float kBeepHz = 1046.0f;
constexpr float kBeepSecs = 0.15f;
constexpr int kBeepCount = 3;
constexpr int kBeepGapMs = 150;
constexpr int16_t kAmplitude = 8000;

enum class Command { AlarmRing, Siren, TodoBeep, Stop };

i2c_master_dev_handle_t g_codec = nullptr;
i2s_chan_handle_t g_tx = nullptr;
QueueHandle_t g_queue = nullptr;
bool g_awake = false;
std::atomic<bool> g_playing{false};
std::atomic<unsigned> g_pending{0};

bool WriteReg(uint8_t reg, uint8_t value) {
    const uint8_t buf[2] = {reg, value};
    return i2c_master_transmit(g_codec, buf, 2, 100) == ESP_OK;
}

bool ReadReg(uint8_t reg, uint8_t* value) {
    return i2c_master_transmit_receive(g_codec, &reg, 1, value, 1, 100) == ESP_OK;
}

// ES8311 register initialization.
bool CodecInit() {
    uint8_t v = 0;
    bool ok = WriteReg(0x45, 0x00) && WriteReg(0x00, 0x1F);
    vTaskDelay(1);
    ok = ok && WriteReg(0x00, 0x00) && WriteReg(0x00, 0x80);
    ok = ok && WriteReg(0x01, 0x3F | 0x80);
    ok = ok && ReadReg(0x06, &v) && WriteReg(0x06, v & ~(1 << 5));
    // Clock coefficients for MCLK = 256 * fs at 16 kHz.
    constexpr uint8_t kPreDiv = 0x01, kPreMulti = 0x00, kAdcDiv = 0x01, kDacDiv = 0x01;
    constexpr uint8_t kFsMode = 0x00, kLrckH = 0x00, kLrckL = 0xFF, kBclkDiv = 0x04;
    constexpr uint8_t kAdcOsr = 0x10, kDacOsr = 0x10;
    ok = ok && ReadReg(0x02, &v) &&
         WriteReg(0x02, (v & 0x07) | ((kPreDiv - 1) << 5) | (kPreMulti << 3));
    ok = ok && WriteReg(0x03, (kFsMode << 6) | kAdcOsr) && WriteReg(0x04, kDacOsr);
    ok = ok && WriteReg(0x05, ((kAdcDiv - 1) << 4) | (kDacDiv - 1));
    ok = ok && ReadReg(0x06, &v) &&
         WriteReg(0x06, (v & 0xE0) | (kBclkDiv - (kBclkDiv < 19 ? 1 : 0)));
    ok = ok && ReadReg(0x07, &v) && WriteReg(0x07, (v & 0xC0) | kLrckH) && WriteReg(0x08, kLrckL);
    ok = ok && ReadReg(0x00, &v) && WriteReg(0x00, v & 0xBF);
    ok = ok && WriteReg(0x09, 3 << 2) && WriteReg(0x0A, 3 << 2);
    ok = ok && WriteReg(0x0D, 0x01) && WriteReg(0x0E, 0x02) && WriteReg(0x12, 0x00) &&
         WriteReg(0x13, 0x10) && WriteReg(0x1C, 0x6A) && WriteReg(0x37, 0x08);
    ok = ok && WriteReg(0x32, 200);                                    // volume
    ok = ok && ReadReg(0x31, &v) && WriteReg(0x31, v & ~((1 << 6) | (1 << 5)));  // unmute
    g_awake = ok;
    return ok;
}

// Powers down the DAC, ADC, references and bias between tones.
void CodecStandby() {
    if (!g_awake) return;
    g_awake = false;
    WriteReg(0x32, 0x00);
    WriteReg(0x17, 0x00);
    WriteReg(0x0E, 0xFF);
    WriteReg(0x12, 0x02);
    WriteReg(0x14, 0x00);
    WriteReg(0x0D, 0xFA);
    WriteReg(0x15, 0x00);
    WriteReg(0x37, 0x08);
    WriteReg(0x45, 0x01);
}

bool InitPlayback() {
    gpio_set_level(board::pins::PaEnable, 0);
    i2c_device_config_t dev = {};
    dev.dev_addr_length = I2C_ADDR_BIT_LEN_7;
    dev.device_address = kCodecAddress;
    dev.scl_speed_hz = 400000;
    if (!g_codec && i2c_master_bus_add_device(board::I2cBus(), &dev, &g_codec) != ESP_OK) return false;

    i2s_chan_config_t chan = I2S_CHANNEL_DEFAULT_CONFIG(I2S_NUM_0, I2S_ROLE_MASTER);
    if (i2s_new_channel(&chan, &g_tx, nullptr) != ESP_OK) return false;
    i2s_std_config_t std_cfg = {};
    std_cfg.clk_cfg = I2S_STD_CLK_DEFAULT_CONFIG(kSampleRate);
    std_cfg.slot_cfg = I2S_STD_PHILIPS_SLOT_DEFAULT_CONFIG(I2S_DATA_BIT_WIDTH_16BIT, I2S_SLOT_MODE_STEREO);
    std_cfg.gpio_cfg.mclk = board::pins::AudioMclk;
    std_cfg.gpio_cfg.bclk = board::pins::AudioBclk;
    std_cfg.gpio_cfg.ws = board::pins::AudioWs;
    std_cfg.gpio_cfg.dout = board::pins::AudioOut;
    std_cfg.gpio_cfg.din = I2S_GPIO_UNUSED;
    if (i2s_channel_init_std_mode(g_tx, &std_cfg) != ESP_OK) {
        i2s_del_channel(g_tx);
        g_tx = nullptr;
        return false;
    }

    return true;
}

bool PlaySine(float hz, float secs, int16_t amplitude) {
    if (!g_tx && !InitPlayback()) return false;
    if (!g_awake && !CodecInit()) {
        event_log::Critical("audio_codec_wake_failed");
        ESP_LOGW(kTag, "codec wake failed");
        return false;
    }
    gpio_set_level(board::pins::PaEnable, 1);
    vTaskDelay(pdMS_TO_TICKS(10));
    if (i2s_channel_enable(g_tx) != ESP_OK) {
        gpio_set_level(board::pins::PaEnable, 0);
        return false;
    }
    constexpr int kChunk = 256;
    int16_t buf[kChunk * 2];
    const int total = static_cast<int>(kSampleRate * secs);
    for (int frame = 0; frame < total;) {
        const int n = total - frame < kChunk ? total - frame : kChunk;
        for (int i = 0; i < n; ++i) {
            const float t = static_cast<float>(frame + i) / kSampleRate;
            const int16_t s = static_cast<int16_t>(amplitude * std::sin(2.0f * 3.14159265f * hz * t));
            buf[i * 2] = s;
            buf[i * 2 + 1] = s;
        }
        size_t written = 0;
        if (i2s_channel_write(g_tx, buf, n * 4, &written, 500) != ESP_OK ||
            written != static_cast<size_t>(n * 4)) {
            i2s_channel_disable(g_tx);
            gpio_set_level(board::pins::PaEnable, 0);
            event_log::Critical("audio_dma_write_failed");
            ESP_LOGW(kTag, "audio DMA write failed");
            return false;
        }
        frame += n;
    }
    vTaskDelay(pdMS_TO_TICKS(150));  // drain the DMA before the amplifier goes off
    i2s_channel_disable(g_tx);
    gpio_set_level(board::pins::PaEnable, 0);
    return true;
}

// Waits up to ms for a new command; returns true (and *cmd) when one came.
bool WaitCommand(int ms, Command* cmd) {
    if (xQueueReceive(g_queue, cmd, ms < 0 ? portMAX_DELAY : pdMS_TO_TICKS(ms)) != pdTRUE) return false;
    g_playing.store(true);
    g_pending.fetch_sub(1);
    return true;
}

void Task(void*) {
    Command mode = Command::Stop;
    while (true) {
        Command next;
        if (mode == Command::Stop) {
            CodecStandby();
            g_playing.store(false);
            WaitCommand(-1, &next);
            mode = next;
            continue;
        }
        switch (mode) {
            case Command::AlarmRing:
                PlaySine(kAlarmHz, kAlarmBurstSecs, kAmplitude);
                if (WaitCommand(kAlarmGapMs, &next)) mode = next;
                break;
            case Command::Siren:
                for (const auto& note : kSirenNotes) {
                    PlaySine(note[0], note[1], kSirenAmplitude);
                }
                if (WaitCommand(0, &next)) mode = next;
                break;
            case Command::TodoBeep:
                for (int i = 0; i < kBeepCount && mode == Command::TodoBeep; ++i) {
                    PlaySine(kBeepHz, kBeepSecs, kAmplitude);
                    if (WaitCommand(kBeepGapMs, &next)) mode = next;
                }
                if (mode == Command::TodoBeep) mode = Command::Stop;
                break;
            case Command::Stop:
                break;
        }
    }
}

}  // namespace

bool Init() {
    g_queue = xQueueCreate(8, sizeof(Command));
    if (!g_queue) return false;
    if (xTaskCreate(Task, "tones", 4096, nullptr, 6, nullptr) != pdPASS) {
        vQueueDelete(g_queue);
        g_queue = nullptr;
        return false;
    }
    return true;
}

namespace {
void Send(Command cmd) {
    if (!g_queue) return;
    g_pending.fetch_add(1);
    // The tone task owns no application locks while consuming commands.
    xQueueSend(g_queue, &cmd, portMAX_DELAY);
}
}  // namespace

void Start(Tone tone) {
    Send(tone == Tone::AlarmRing ? Command::AlarmRing
         : tone == Tone::Siren   ? Command::Siren
                                 : Command::TodoBeep);
}

void Stop() { Send(Command::Stop); }

bool Busy() { return g_playing.load() || g_pending.load() != 0; }

}  // namespace tones
