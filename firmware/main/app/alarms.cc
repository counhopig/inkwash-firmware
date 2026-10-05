#include "app/internal.h"

namespace app::detail {

// ---- Alarms ------------------------------------------------------------------------

void ProgramRtcAlarm() {
    if (!g.have_clock) return;
    const Alarm* next = schedule::NextDue(g.alarms, g.now);
    schedule::AlarmRegs regs{};
    const bool want = next && schedule::AlarmRegsFor(*next, g.now, &regs);
    if (!want) {
        if (g.programmed_valid && g.programmed_none) return;
        if (pcf8563::ClearAlarm()) {
            event_log::Add("rtc_alarm_clear");
            g.programmed_valid = true;
            g.programmed_none = true;
        } else {
            event_log::Critical("rtc_alarm_clear_failed");
        }
        return;
    }
    if (g.programmed_valid && !g.programmed_none && g.programmed.minute == regs.minute &&
        g.programmed.hour == regs.hour && g.programmed.day == regs.day &&
        g.programmed.weekday == regs.weekday) {
        return;
    }
    if (pcf8563::SetAlarm(regs)) {
        event_log::Add("rtc_alarm id=%u hour=%u minute=%u day=%d weekday=%d",
                       unsigned(next->id), unsigned(regs.hour), unsigned(regs.minute), regs.day, regs.weekday);
        g.programmed = regs;
        g.programmed_valid = true;
        g.programmed_none = false;
        ESP_LOGI(kTag, "RTC alarm programmed %02u:%02u day=%d weekday=%d", regs.hour, regs.minute,
                 regs.day, regs.weekday);
    } else {
        event_log::Critical("rtc_alarm_program_failed id=%u", unsigned(next->id));
    }
}

void StartRinging() {
    if (g.screen.kind == ScreenKind::Reminder) {
        tones::Stop();
        g.before_ring = g.before_reminder;
    } else if (g.screen.kind == ScreenKind::Navigation) {
        g.before_ring = g.before_nav;
    } else {
        g.before_ring = g.screen;
    }
    g.ringing = true;
    g.ring_deadline_ms = NowMs() + kRingAutoSilenceMs;
    g.fired_minute = g.now.ToUnix() / 60;
    g.screen = Screen{ScreenKind::AlarmRinging};
    tones::Start(tones::Tone::AlarmRing);
    event_log::Critical("alarm_start");
}

void StopRinging() {
    if (!g.ringing) return;
    event_log::Critical("alarm_stop timeout=%u", unsigned(NowMs() >= g.ring_deadline_ms));
    g.ringing = false;
    tones::Stop();
    g.screen = g.before_ring;
}

// Handles the PCF8563 alarm flag: ring when an enabled alarm is due now.
bool CheckRtcAlarm() {
    pcf8563::AlarmStatus st;
    if (!pcf8563::ReadAlarmStatus(&st) || !st.flag) return false;
    pcf8563::AckAlarm();
    if (g.ringing || !st.interrupt_enabled || !g.have_clock) return false;
    const Alarm* due = nullptr;
    for (const Alarm& a : g.alarms) {
        if (schedule::IsDueNow(a, g.now) && (!due || a.id < due->id)) due = &a;
    }
    if (!due) return false;
    const uint8_t id = due->id;
    // A one-shot alarm is used up once it rings.
    const size_t before = g.alarms.size();
    g.alarms.erase(std::remove_if(g.alarms.begin(), g.alarms.end(),
                                  [&](const Alarm& a) {
                                      return a.id == id && a.repeat.kind == Repeat::Kind::Once;
                                  }),
                   g.alarms.end());
    if (g.alarms.size() != before) store::SaveAlarms(g.alarms);
    event_log::Add("alarm_due id=%u", unsigned(id));
    ESP_LOGI(kTag, "alarm %u ringing", id);
    StartRinging();
    return true;
}


}  // namespace app::detail
