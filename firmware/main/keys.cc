#include "board_pins.h"
#include "keys.h"

#include "driver/gpio.h"
#include "freertos/FreeRTOS.h"
#include "freertos/task.h"

namespace keys {
namespace {

constexpr int kPollMs = 20;
constexpr int kDebounceSamples = 4;
constexpr int kLongPressPolls = 50;
constexpr gpio_num_t kPins[3] = {board::pins::KeyEnter, board::pins::KeyUp, board::pins::KeyDown};
constexpr board::Key kKeys[3] = {board::Key::Enter, board::Key::Up, board::Key::Down};

std::function<void(Event)> g_on_event;
TaskHandle_t g_task = nullptr;

struct State {
    bool debounced = false;
    bool candidate = false;
    int samples = kDebounceSamples;
    int held_polls = 0;
    bool long_pressed = false;
    bool suppressed = false;  // held since boot
};

void IRAM_ATTR OnEdge(void* arg) {
    const auto pin = static_cast<gpio_num_t>(reinterpret_cast<intptr_t>(arg));
    gpio_intr_disable(pin);
    BaseType_t woken = pdFALSE;
    vTaskNotifyGiveFromISR(g_task, &woken);
    portYIELD_FROM_ISR(woken);
}

void Task(void*) {
    State state[3];
    for (int i = 0; i < 3; ++i) {
        const bool down = gpio_get_level(kPins[i]) == 0;
        state[i].debounced = state[i].candidate = down;
        state[i].suppressed = down;
    }
    while (true) {
        bool active = false;
        for (int i = 0; i < 3; ++i) {
            State& s = state[i];
            const bool raw = gpio_get_level(kPins[i]) == 0;
            if (raw != s.candidate) {
                s.candidate = raw;
                s.samples = 0;
            } else if (++s.samples >= kDebounceSamples) {
                s.samples = kDebounceSamples;
                if (s.debounced != s.candidate) {
                    s.debounced = s.candidate;
                    if (s.debounced) {
                        s.held_polls = 0;
                        s.long_pressed = false;
                    } else if (s.suppressed) {
                        s.suppressed = false;
                    } else if (s.long_pressed) {
                        g_on_event({Kind::Released, kKeys[i]});
                    } else {
                        g_on_event({Kind::Pressed, kKeys[i]});
                    }
                } else if (s.debounced && !s.suppressed && ++s.held_polls == kLongPressPolls) {
                    s.long_pressed = true;
                    g_on_event({Kind::LongPressed, kKeys[i]});
                }
            }
            active = active || s.debounced || s.candidate || s.samples < kDebounceSamples;
        }
        if (active) {
            vTaskDelay(pdMS_TO_TICKS(kPollMs));
            continue;
        }
        // All keys up and settled: sleep until the next press.
        for (gpio_num_t pin : kPins) gpio_intr_enable(pin);
        ulTaskNotifyTake(pdTRUE, portMAX_DELAY);
    }
}

}  // namespace

void Start(std::function<void(Event)> on_event) {
    g_on_event = std::move(on_event);
    const esp_err_t installed = gpio_install_isr_service(0);
    ESP_ERROR_CHECK(installed == ESP_ERR_INVALID_STATE ? ESP_OK : installed);
    for (gpio_num_t pin : kPins) {
        gpio_set_intr_type(pin, GPIO_INTR_LOW_LEVEL);
        gpio_intr_disable(pin);
        gpio_isr_handler_add(pin, OnEdge, reinterpret_cast<void*>(static_cast<intptr_t>(pin)));
    }
    // The task enables the interrupts once every key is up.
    configASSERT(xTaskCreate(Task, "keys", 4096, nullptr, 10, &g_task) == pdPASS);
}

}  // namespace keys
