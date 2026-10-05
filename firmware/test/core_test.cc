// Host tests for firmware/main/core. Build and run with ./run.sh.
#include <cstdio>
#include <cstring>
#include <initializer_list>
#include <string>

#include "cJSON.h"
#include "core/boot_guard.h"
#include "core/power_policy.h"
#include "core/refresh_policy.h"
#include "core/sleep_trace.h"
#include "core/codec.h"
#include "core/datetime.h"
#include "core/protocol.h"
#include "core/schedule.h"
#include "core/sync_payload.h"

static int g_failures = 0;
#define CHECK(cond)                                                              \
    do {                                                                         \
        if (!(cond)) {                                                           \
            std::printf("FAIL %s:%d: %s\n", __FILE__, __LINE__, #cond);          \
            ++g_failures;                                                        \
        }                                                                        \
    } while (0)
#define CHECK_EQ_STR(a, b)                                                       \
    do {                                                                         \
        const std::string _a = (a), _b = (b);                                    \
        if (_a != _b) {                                                          \
            std::printf("FAIL %s:%d:\n  got      %s\n  expected %s\n", __FILE__, \
                        __LINE__, _a.c_str(), _b.c_str());                       \
            ++g_failures;                                                        \
        }                                                                        \
    } while (0)

static DateTime Dt(int y, int mo, int d, int h, int mi) {
    DateTime dt;
    dt.year = y;
    dt.month = mo;
    dt.day = d;
    dt.hour = h;
    dt.minute = mi;
    dt.weekday = WeekdayOf(y, mo, d);
    return dt;
}

static void TestDatetime() {
    CHECK(DateTime::FromUnix(0).weekday == 4);
    CHECK(WeekdayOf(2000, 1, 1) == 6);
    CHECK(WeekdayOf(2024, 1, 1) == 1);
    const DateTime dt = Dt(2026, 9, 27, 14, 5);
    CHECK(DateTime::FromUnix(dt.ToUnix()).SameMinute(dt));
    CHECK(DaysInMonth(2024, 2) == 29 && DaysInMonth(2026, 2) == 28);
    CHECK(dt.ShiftedMinutes(480).hour == 22);
}

// Strings below are exactly what serde_json produced from the Rust types.
static void TestCodecMatchesSerde() {
    Alarm a;
    a.id = 3;
    a.hour = 7;
    a.minute = 30;
    a.repeat.kind = Repeat::Kind::Weekly;
    a.repeat.days = {1, 3};
    a.label = "run";
    CHECK_EQ_STR(codec::Print(codec::EncodeAlarm(a)),
                 R"({"id":3,"hour":7,"minute":30,"repeat":{"Weekly":{"days":[1,3]}},"enabled":true,"label":"run"})");
    Alarm daily;
    CHECK_EQ_STR(codec::Print(codec::EncodeAlarm(daily)),
                 R"({"id":0,"hour":0,"minute":0,"repeat":"Daily","enabled":true,"label":""})");
    Alarm once = a;
    once.repeat = Repeat{};
    once.repeat.kind = Repeat::Kind::Once;
    once.repeat.year = 2026;
    once.repeat.month = 10;
    once.repeat.day = 2;
    CHECK_EQ_STR(codec::Print(codec::EncodeRepeat(once.repeat)),
                 R"({"Once":{"year":2026,"month":10,"day":2}})");

    Todo t;
    t.id = 9;
    t.text = "买菜";
    t.importance = Importance::High;
    t.has_due = true;
    t.due = {2026, 9, 30};
    CHECK_EQ_STR(codec::Print(codec::EncodeTodo(t)),
                 R"({"id":9,"text":"买菜","done":false,"importance":"high","due_date":{"year":2026,"month":9,"day":30},"repeat":null})");

    InboxItem it;
    it.id = 42;
    it.kind = InboxKind::Alert;
    it.priority = Priority::High;
    it.title = "CI failed";
    CHECK_EQ_STR(codec::Print(codec::EncodeInboxItem(it)),
                 R"({"id":42,"kind":"alert","priority":"high","title":"CI failed","body":"","when":null,"read":false})");

    // Round trips and serde(default) tolerance.
    cJSON* j = cJSON_Parse(R"({"id":5,"text":"x","done":true})");
    Todo decoded;
    CHECK(codec::DecodeTodo(j, &decoded));
    CHECK(decoded.importance == Importance::Medium && !decoded.has_due && !decoded.has_repeat);
    cJSON_Delete(j);
    j = cJSON_Parse(R"({"id":5,"hour":1,"minute":2,"repeat":{"Monthly":{"days":[1,15]}},"enabled":false,"label":"l"})");
    Alarm da;
    CHECK(codec::DecodeAlarm(j, &da));
    CHECK(da.repeat.kind == Repeat::Kind::Monthly && da.repeat.days.size() == 2 && !da.enabled);
    cJSON_Delete(j);
    j = cJSON_Parse(R"({"id":5,"hour":1,"minute":2,"repeat":"Hourly","enabled":false,"label":"l"})");
    CHECK(!codec::DecodeAlarm(j, &da));
    cJSON_Delete(j);
}

static void TestSchedule() {
    const DateTime now = Dt(2026, 9, 27, 14, 5);  // a Sunday
    CHECK(now.weekday == 0);
    Alarm daily;
    daily.hour = 14;
    daily.minute = 0;
    CHECK(schedule::MinutesUntil(daily, now) == 24 * 60 - 5);
    Alarm weekly;
    weekly.id = 1;
    weekly.hour = 8;
    weekly.minute = 0;
    weekly.repeat.kind = Repeat::Kind::Weekly;
    weekly.repeat.days = {1};  // Monday
    CHECK(schedule::MinutesUntil(weekly, now) == (24 * 60 - (14 * 60 + 5)) + 8 * 60);
    std::vector<Alarm> alarms = {daily, weekly};
    const Alarm* next = schedule::NextDue(alarms, now);
    CHECK(next && next->id == 1);
    schedule::AlarmRegs regs;
    CHECK(schedule::AlarmRegsFor(weekly, now, &regs) && regs.weekday == 1 && regs.day < 0);
    Alarm once;
    once.repeat.kind = Repeat::Kind::Once;
    once.repeat.year = 2026;
    once.repeat.month = 11;
    once.repeat.day = 3;
    CHECK(!schedule::AlarmRegsFor(once, now, &regs));
    std::vector<Alarm> with_once = {once};
    const int64_t wake = schedule::MaintenanceWakeupSecs(with_once, now);
    CHECK(wake > 0 && DateTime::FromUnix(now.ToUnix() + wake + 60).month == 11);
    CHECK(schedule::NextAlarmId(alarms) == 2);
}

static void TestProtocol() {
    protocol::Command c;
    std::string err;
    CHECK(protocol::Parse(R"({"cmd":"set_wifi","ssid":"net","password":"pw","id":"1"})", &c, &err));
    CHECK(c.cmd == protocol::Cmd::SetWifi && c.ssid == "net" && c.has_id && c.id == "1");
    CHECK(protocol::Parse(R"({"cmd":"set_timezone","offset_minutes":-480})", &c, &err));
    CHECK(c.offset_minutes == -480);
    CHECK(!protocol::Parse(R"({"cmd":"reboot"})", &c, &err));
    CHECK(!protocol::Parse(R"({"cmd":"get_status","x":[[[[[1]]]]]})", &c, &err));
    const std::string id = "wifi";
    CHECK_EQ_STR(protocol::ReplyOk(&id), R"({"status":"ok","id":"wifi"})");
    CHECK_EQ_STR(protocol::ReplyError("bad", nullptr), R"({"status":"error","message":"bad"})");
    protocol::Status s;
    s.wifi_configured = true;
    s.has_ssid = true;
    s.wifi_ssid = "X";
    CHECK_EQ_STR(protocol::ReplyStatus(s, nullptr),
                 R"({"status":"status","wifi_configured":true,"server_configured":false,"wifi_connected":false,"wifi_ssid":"X","wifi_has_password":false,"server_url":null,"server_has_token":false,"timezone_offset_minutes":0})");
}

static void TestSyncPayload() {
    sync_payload::Upload up;
    up.alarms = {{3, false}};
    up.inbox_read = {7};
    CHECK_EQ_STR(sync_payload::EncodeUpload(up),
                 R"({"alarms":[{"id":3,"enabled":false}],"todos":[],"inbox_read":[7]})");
    const char* body =
        R"({"alarms":[{"id":1,"hour":7,"minute":0,"repeat":"Daily","enabled":true,"label":""}],"todos":[],"inbox":[{"id":2,"kind":"info","title":"t"}],"inbox_read_acked":[7]})";
    sync_payload::Response r;
    CHECK_EQ_STR(sync_payload::DecodeResponse(body, std::strlen(body), &r), "");
    CHECK(r.alarms.size() == 1 && r.inbox.size() == 1 && r.inbox_read_acked.size() == 1);
    const char* dup = R"({"alarms":[{"id":1,"hour":7,"minute":0,"repeat":"Daily","enabled":true,"label":""},{"id":1,"hour":8,"minute":0,"repeat":"Daily","enabled":true,"label":""}]})";
    CHECK(!sync_payload::DecodeResponse(dup, std::strlen(dup), &r).empty());
    const char* bad_time = R"({"alarms":[{"id":1,"hour":24,"minute":0,"repeat":"Daily","enabled":true,"label":""}]})";
    CHECK(!sync_payload::DecodeResponse(bad_time, std::strlen(bad_time), &r).empty());
    sync_payload::Applied applied;
    applied.data.alarms.push_back(Alarm{});
    applied.uploaded_todo_ids = {4};
    sync_payload::Applied back;
    CHECK(sync_payload::DecodeApplied(sync_payload::EncodeApplied(applied), &back));
    CHECK(back.data.alarms.size() == 1 && back.uploaded_todo_ids == std::vector<uint8_t>{4});
}

void TestBootGuard() {
    using boot_guard::Ledger;
    using boot_guard::ResetKind;
    // Cold memory starts a fresh run.
    for (Ledger raw : {Ledger{0, 0}, Ledger{0xDEADBEEF, 2}, Ledger{boot_guard::kMagic, 4},
                       Ledger{boot_guard::kMagic + 1, 1}}) {
        CHECK(!raw.Recorded());
        CHECK(raw.NoteAttempt(ResetKind::Panic).failures == 1);
        CHECK(!raw.Exhausted());
    }
    // Consecutive failures reach the limit and saturate.
    Ledger l = Ledger{}.NoteAttempt(ResetKind::PowerOn);
    CHECK(l.failures == 0 && !l.Exhausted());
    l = l.NoteAttempt(ResetKind::Panic);
    CHECK(l.failures == 1 && !l.Exhausted());
    l = l.NoteAttempt(ResetKind::TaskWatchdog);
    CHECK(l.failures == 2 && !l.Exhausted());
    l = l.NoteAttempt(ResetKind::Brownout);
    CHECK(l.failures == 3 && l.Exhausted());
    l = l.NoteAttempt(ResetKind::Panic);
    CHECK(l.failures == 3 && l.Exhausted());
    // An operator action or a deep-sleep wake starts over.
    for (ResetKind k : {ResetKind::PowerOn, ResetKind::ExternalReset, ResetKind::DeepSleep,
                        ResetKind::Usb, ResetKind::Jtag, ResetKind::Other}) {
        CHECK(!l.NoteAttempt(k).Exhausted());
        CHECK(l.NoteAttempt(k).failures == 0);
    }
    // A finished boot clears the count but keeps the marker.
    CHECK(l.Cleared().Recorded() && l.Cleared().failures == 0);
}

static void TestPowerAndRefreshPolicies() {
    using power_policy::NextWakeSecs;
    CHECK(NextWakeSecs(true, 0, -1) == 60);
    CHECK(NextWakeSecs(true, 59, -1) == 1);
    CHECK(NextWakeSecs(true, 86399, -1) == 1);
    CHECK(NextWakeSecs(true, 86400, -1) == 60);
    CHECK(NextWakeSecs(true, 15, 7) == 7);
    CHECK(NextWakeSecs(false, 59, -1) == 60);
    CHECK(power_policy::RetryPending(100, 160, 60));
    CHECK(!power_policy::RetryPending(160, 160, 60));
    CHECK(!power_policy::RetryPending(200, 160, 60));
    CHECK(!power_policy::RetryPending(1, 160, 60));
    power_policy::Work work;
    CHECK(power_policy::CanSleep(work));
    bool power_policy::Work::* gates[] = {
        &power_policy::Work::usb, &power_policy::Work::network, &power_policy::Work::alarm,
        &power_policy::Work::reminder, &power_policy::Work::pairing, &power_policy::Work::reply,
        &power_policy::Work::audio, &power_policy::Work::display, &power_policy::Work::events,
        &power_policy::Work::keys,
    };
    for (auto gate : gates) {
        work = {};
        work.*gate = true;
        CHECK(!power_policy::CanSleep(work));
    }
    using refresh_policy::Plan;
    CHECK(refresh_policy::Choose(false, false, false, false) == Plan::Full);
    CHECK(refresh_policy::Choose(true, false, false, false) == Plan::None);
    CHECK(refresh_policy::Choose(true, true, false, false) == Plan::Partial);
    CHECK(refresh_policy::Choose(true, true, false, true) == Plan::Full);
    CHECK(refresh_policy::Choose(true, false, true, false) == Plan::Full);
}

int main() {
    sleep_trace::Ring trace = {};
    CHECK(!sleep_trace::Valid(trace));
    for (uint32_t i = 1; i <= 40; ++i) {
        sleep_trace::Entry e = {};
        e.kind = i % 2 ? sleep_trace::Kind::Sleep : sleep_trace::Kind::Boot;
        e.utc_secs = 1700000000 + i;
        sleep_trace::Append(&trace, e);
    }
    CHECK(trace.count == 16 && sleep_trace::Valid(trace));
    CHECK(sleep_trace::At(trace, 0)->sequence == 25);
    CHECK(sleep_trace::At(trace, 15)->sequence == 40);
    CHECK(sleep_trace::At(trace, 16) == nullptr);
    sleep_trace::SetBootTime(&trace, 1800000000);
    CHECK(sleep_trace::At(trace, 15)->utc_secs == 1800000000);
    trace.next = 100;
    CHECK(!sleep_trace::Valid(trace));
    sleep_trace::Entry e = {};
    sleep_trace::Append(&trace, e);
    CHECK(trace.count == 1 && sleep_trace::Valid(trace));
    trace.entries[0].sequence = 99;
    CHECK(!sleep_trace::Valid(trace));
    sleep_trace::Append(&trace, e);
    CHECK(trace.count == 1 && sleep_trace::Valid(trace));
    TestPowerAndRefreshPolicies();
    TestDatetime();
    TestCodecMatchesSerde();
    TestSchedule();
    TestProtocol();
    TestSyncPayload();
    TestBootGuard();
    if (g_failures == 0) {
        std::printf("all core tests passed\n");
    }
    return g_failures == 0 ? 0 : 1;
}
