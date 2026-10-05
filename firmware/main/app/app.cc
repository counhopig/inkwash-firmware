#include "app/internal.h"

namespace app::detail {
State g;
QueueHandle_t g_events = nullptr;
QueueHandle_t g_net = nullptr;
TaskHandle_t g_net_task = nullptr;
std::atomic<int> g_ble_link{-1};
std::atomic<uint32_t> g_rejected_events{0};
int64_t NowMs() { return esp_timer_get_time() / 1000; }

bool Post(Event* e, TickType_t timeout) {
    if (xQueueSend(g_events, &e, timeout) == pdTRUE) return true;
    g_rejected_events.fetch_add(1);
    delete e;
    return false;
}

void NetTask(void*) {
    while (true) {
        NetRequest* req = nullptr;
        xQueueReceive(g_net, &req, portMAX_DELAY);
        auto* done = new Event{Event::Kind::NetDone};
        done->job = req->job;
        switch (req->job) {
            case NetJob::Sync: done->result = netsync::Run(req->now); break;
            case NetJob::UrgentPoll: done->result = netsync::PollUrgent(&done->urgent); break;
            case NetJob::VerifyWifi: done->result = netsync::VerifyWifi(req->creds); break;
            case NetJob::Ntp: done->result = netsync::NtpOnly(); break;
            case NetJob::None: break;
        }
        delete req;
        // Completion releases the application's network and sleep gates.
        // This worker owns no application locks while waiting for queue space.
        xQueueSend(g_events, &done, portMAX_DELAY);
    }
}

size_t NavHomeIndex(NavOrigin o) {
    switch (o) {
        case NavOrigin::Home: return 0;
        case NavOrigin::Calendar: return 1;
        case NavOrigin::Inbox: return 2;
        case NavOrigin::AlarmList: return 3;
        case NavOrigin::TodoList: return 4;
        case NavOrigin::Settings: return 5;
    }
    return 0;
}


}  // namespace app::detail

namespace app {
using namespace detail;
void Run(power::WakeCause wake) {
    g_events = xQueueCreate(16, sizeof(Event*));
    g_net = xQueueCreate(2, sizeof(NetRequest*));
    configASSERT(g_events != nullptr && g_net != nullptr);

    Boot(wake);

    usb_console::Start([](const std::string& line) {
        auto* e = new Event{Event::Kind::Command};
        e->channel = Channel::Usb;
        e->line = line;
        Post(e);
    });
    keys::Start([](keys::Event k) {
        auto* e = new Event{Event::Kind::Key};
        e->key = k;
        Post(e);
    });
    power::EnableLightSleep();

    while (true) {
        Event* e = nullptr;
        bool redraw = false;
        if (xQueueReceive(g_events, &e, pdMS_TO_TICKS(NextWaitMs())) == pdTRUE) {
            // Handle everything already queued (keys pressed during the last
            // panel refresh) before drawing, so one refresh covers them all.
            unsigned handled = 0;
            do {
                std::unique_ptr<Event> owned(e);
                if (HandleEvent(*e)) redraw = true;
            } while (++handled < 16 && xQueueReceive(g_events, &e, 0) == pdTRUE);
        }
        const int link = g_ble_link.exchange(-1);
        if (link >= 0) {
            Event event{Event::Kind::BleLink};
            event.ble_event = static_cast<ble::Event>(link);
            if (HandleEvent(event)) redraw = true;
        }
        const uint32_t rejected = g_rejected_events.exchange(0);
        if (rejected) event_log::Critical("event_queue_rejected count=%lu", static_cast<unsigned long>(rejected));
        if (Housekeeping()) redraw = true;
        if (redraw || !display::Healthy()) Render();

        const int64_t idle_limit = power_policy::IdleMs(g.background);
        if (NowMs() - g.last_activity_ms >= idle_limit) {
            if (!SleepBlocked()) GoToDeepSleep();
            LogSleepBlock();
        }
    }
}

}  // namespace app
