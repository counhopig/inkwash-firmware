// Host tests for firmware/main/core. Build and run with ./run.sh.
#include <cstdio>
#include <cstring>
#include <initializer_list>
#include <string>

#include "cJSON.h"
#include "core/boot_guard.h"
#include "core/event_journal.h"
#include <vector>
#include <limits>
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


class TestFlash : public event_journal::Flash {
 public:
    std::vector<uint8_t> data = std::vector<uint8_t>(8192, 0xff);
    // Interrupt a programming operation after exactly this many bytes.
    int write_budget = -1;
    bool fail_read = false;
    bool report_commit_error = false;
    bool fail_erase = false;
    size_t Size() const override { return data.size(); }
    bool Read(size_t offset, void* out, size_t size) override {
        if (fail_read || offset + size > data.size()) return false;
        std::memcpy(out, data.data() + offset, size);
        return true;
    }
    bool Write(size_t offset, const void* in, size_t size) override {
        if (offset + size > data.size()) return false;
        const auto* bytes = static_cast<const uint8_t*>(in);
        for (size_t i = 0; i < size; ++i) {
            if (write_budget == 0 || (data[offset+i] & bytes[i]) != bytes[i]) return false;
            data[offset+i] &= bytes[i];
            if (write_budget > 0) --write_budget;
        }
        return !(report_commit_error && size == 4);
    }
    bool Erase(size_t offset, size_t size) override {
        if (fail_erase || offset % 4096 || size % 4096 || offset + size > data.size()) return false;
        std::memset(data.data() + offset, 0xff, size);
        return true;
    }
};

static void TestEventJournal() {
    using namespace event_journal;
    Record record = {};
    record.boot = 1;
    record.utc_secs = 1700000000;
    record.flags = kTimeValid;
    std::strcpy(record.event, "test");
    TestFlash flash;
    Journal journal(flash);
    CHECK(journal.Init());
    CHECK(journal.Sequence() == 0);
    CHECK(journal.Append(record));
    Record read;
    CHECK(journal.Read(0, &read) && Valid(read) && read.sequence == 1);
    read.event[0] ^= 1;
    CHECK(!Valid(read));
    // Power-off and new process: no RAM metadata is retained.
    Journal reboot(flash);
    CHECK(reboot.Init() && reboot.Sequence() == 1 && reboot.Next() == 1);
    for (int i = 0; i < 80; ++i) CHECK(reboot.Append(record));
    Journal wrapped(flash);
    CHECK(wrapped.Init() && wrapped.Sequence() == 81);
    uint64_t previous = 0;
    unsigned count = 0;
    for (size_t i = 0; i < wrapped.Capacity(); ++i) {
        CHECK(wrapped.Read((wrapped.Next() + i) % wrapped.Capacity(), &read));
        if (!Valid(read)) continue;
        CHECK(read.sequence > previous);
        previous = read.sequence;
        ++count;
    }
    CHECK(count == 49 && previous == 81);  // sector reclamation preserves 49 committed records
    // Every possible byte boundary of a body/commit write may lose power.
    for (int cut = 0; cut < 128; ++cut) {
        TestFlash interrupted;
        Journal first(interrupted);
        CHECK(first.Init() && first.Append(record));
        interrupted.write_budget = cut;
        CHECK(!first.Append(record));
        interrupted.write_budget = -1;
        Journal recovered(interrupted);
        CHECK(recovered.Init() && recovered.Sequence() == 1);
        CHECK(recovered.Append(record));
        Journal again(interrupted);
        CHECK(again.Init() && again.Sequence() == 2);
        CHECK(again.Read(0, &read) && Valid(read) && read.sequence == 1);
    }
    // Interrupted sector erase must not destroy the latest record in another sector.
    for (size_t cut = 0; cut <= 4096; cut += 128) {
        TestFlash erased;
        Journal writer(erased);
        CHECK(writer.Init());
        for (int i = 0; i < 64; ++i) CHECK(writer.Append(record));
        std::memset(erased.data.data(), 0xff, cut);
        Journal recovered(erased);
        CHECK(recovered.Init() && recovered.Sequence() == 64);
        CHECK(recovered.Append(record));
        CHECK(recovered.Read(0, &read) && Valid(read) && read.sequence == 65);
    }
    // A failed commit report may still leave a fully valid record on flash.
    TestFlash uncertain;
    Journal live(uncertain);
    CHECK(live.Init() && live.Append(record));
    uncertain.report_commit_error = true;
    CHECK(!live.Append(record));
    uncertain.report_commit_error = false;
    CHECK(live.Append(record));
    CHECK(live.Read(1, &read) && Valid(read) && read.sequence == 2);
    CHECK(live.Read(2, &read) && Valid(read) && read.sequence == 3);
    record.flags = kTimeEstimated;
    CHECK(live.Append(record));
    Journal provenance(uncertain);
    CHECK(provenance.Init() && provenance.LastClockFlags() == kTimeEstimated);
    record.flags = kTimeValid;
    CHECK(provenance.Append(record));
    Journal aligned(uncertain);
    CHECK(aligned.Init() && aligned.LastClockFlags() == kTimeValid);
    // Corrupt newest record: recover earlier history and skip the damaged tail.
    TestFlash damaged;
    Journal writer(damaged);
    CHECK(writer.Init() && writer.Append(record) && writer.Append(record));
    damaged.data[128 + 80] ^= 1;
    Journal recovery(damaged);
    CHECK(recovery.Init() && recovery.Sequence() == 1 && recovery.InvalidRecords() == 1);
    CHECK(recovery.Append(record));
    CHECK(recovery.Read(2, &read) && Valid(read) && read.sequence == 2);
    damaged.fail_read = true;
    CHECK(!recovery.Init() && !recovery.Append(record));
    damaged.fail_read = false;
    CHECK(recovery.Init());
    damaged.fail_erase = true;
    for (int i = 0; i < 29; ++i) CHECK(recovery.Append(record));
    CHECK(!recovery.Append(record));
    CHECK(recovery.Sequence() == 31);
    protocol::Command command;
    std::string error;
    CHECK(protocol::Parse(R"({"cmd":"get_logs"})", &command, &error));
    CHECK(command.cmd == protocol::Cmd::GetLogs && !command.log_resume);
    CHECK(protocol::Parse(R"({"cmd":"get_logs","cursor":32,"anchor":12,"snapshot":"18446744073709551615"})", &command, &error));
    CHECK(command.log_resume && command.log_cursor == 32 && command.log_snapshot == std::numeric_limits<uint64_t>::max());
    for (const char* invalid : {
        R"({"cmd":"get_logs","cursor":1})",
        R"({"cmd":"get_logs","cursor":-1,"anchor":0,"snapshot":"1"})",
        R"({"cmd":"get_logs","cursor":1.5,"anchor":0,"snapshot":"1"})",
        R"({"cmd":"get_logs","cursor":8193,"anchor":0,"snapshot":"1"})",
        R"({"cmd":"get_logs","cursor":0,"anchor":8192,"snapshot":"1"})",
        R"({"cmd":"get_logs","cursor":0,"anchor":0,"snapshot":1})",
        R"({"cmd":"get_logs","cursor":0,"anchor":0,"snapshot":"18446744073709551616"})",
        R"({"cmd":"get_logs","cursor":0,"anchor":0,"snapshot":"-1"})"}) {
        error.clear();
        CHECK(!protocol::Parse(invalid, &command, &error));
    }
}

int main() {
    TestEventJournal();
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
