#include "diagnostics/event_log.h"

#include <cstdarg>
#include <cstdio>
#include <cstring>
#include "cJSON.h"
#include "core/codec.h"
#include "core/event_journal.h"
#include "esp_partition.h"
#include "esp_timer.h"
#include "freertos/FreeRTOS.h"
#include "freertos/semphr.h"

namespace event_log {
namespace {
class PartitionFlash : public event_journal::Flash {
 public:
    const esp_partition_t* partition = nullptr;
    uint64_t erases = 0;
    size_t Size() const override { return partition ? partition->size : 0; }
    bool Read(size_t off, void* data, size_t size) override { return esp_partition_read(partition, off, data, size) == ESP_OK; }
    bool Write(size_t off, const void* data, size_t size) override { return esp_partition_write(partition, off, data, size) == ESP_OK; }
    bool Erase(size_t off, size_t size) override {
        // Even a failed erase may have changed the sector.
        ++erases;
        return esp_partition_erase_range(partition, off, size) == ESP_OK;
    }
};
PartitionFlash g_flash;
event_journal::Journal g_journal(g_flash);
SemaphoreHandle_t g_lock = nullptr;
bool g_ready = false;
event_journal::Record g_pending[16];
size_t g_count = 0;
uint64_t g_boot = 0;
uint64_t g_utc = 0;
int64_t g_clock_us = 0;
bool g_estimated = false;
int64_t g_last_flush_us = 0;
uint32_t g_dropped = 0;
uint32_t g_io_errors = 0;
bool g_export_active = false;
size_t g_export_anchor = 0;
uint64_t g_export_snapshot = 0;
uint64_t g_export_erases = 0;

bool FlushLocked() {
    size_t written = 0;
    while (written < g_count && g_journal.Append(g_pending[written])) ++written;
    if (written < g_count) ++g_io_errors;
    g_count -= written;
    std::memmove(g_pending, g_pending + written, g_count * sizeof(g_pending[0]));
    g_last_flush_us = esp_timer_get_time();
    return g_count == 0;
}
bool AddArgs(const char* format, va_list args, bool critical) {
    if (!g_ready) return false;
    xSemaphoreTake(g_lock, portMAX_DELAY);
    if (g_count == 16 && !FlushLocked()) {
        ++g_dropped;
        xSemaphoreGive(g_lock);
        return false;
    }
    auto& r = g_pending[g_count++];
    r = {};
    r.boot = g_boot;
    const int64_t now = esp_timer_get_time();
    r.uptime_ms = static_cast<uint64_t>(now / 1000);
    if (g_utc) {
        r.utc_secs = g_utc + static_cast<uint64_t>((now - g_clock_us) / 1000000);
        r.flags = g_estimated ? event_journal::kTimeEstimated : event_journal::kTimeValid;
    }
    vsnprintf(r.event, sizeof(r.event), format, args);
    const bool ok = !critical || FlushLocked();
    xSemaphoreGive(g_lock);
    return ok;
}
void StringNumber(cJSON* root, const char* key, uint64_t value) {
    char text[24];
    snprintf(text, sizeof(text), "%llu", static_cast<unsigned long long>(value));
    cJSON_AddStringToObject(root, key, text);
}
}  // namespace

bool Init() {
    g_flash.partition = esp_partition_find_first(ESP_PARTITION_TYPE_DATA,
                         static_cast<esp_partition_subtype_t>(0x40), "eventlog");
    if (!g_flash.partition || g_flash.Size() != 0x100000 || !g_journal.Init()) return false;
    g_lock = xSemaphoreCreateMutex();
    if (!g_lock) return false;
    g_boot = g_journal.Sequence() + 1;
    g_last_flush_us = esp_timer_get_time();
    g_ready = true;
    return true;
}
void Add(const char* format, ...) {
    va_list args;
    va_start(args, format);
    AddArgs(format, args, false);
    va_end(args);
}
bool Critical(const char* format, ...) {
    va_list args;
    va_start(args, format);
    const bool ok = AddArgs(format, args, true);
    va_end(args);
    return ok;
}
bool LastClockEstimated() {
    if (!g_ready) return false;
    xSemaphoreTake(g_lock, portMAX_DELAY);
    const bool estimated = (g_journal.LastClockFlags() & event_journal::kTimeEstimated) != 0;
    xSemaphoreGive(g_lock);
    return estimated;
}
void SetClock(uint64_t utc, bool estimated) {
    if (!g_ready) return;
    xSemaphoreTake(g_lock, portMAX_DELAY);
    g_utc = utc;
    g_estimated = estimated;
    g_clock_us = esp_timer_get_time();
    xSemaphoreGive(g_lock);
}
bool Flush() {
    if (!g_ready) return false;
    xSemaphoreTake(g_lock, portMAX_DELAY);
    const bool ok = FlushLocked();
    xSemaphoreGive(g_lock);
    return ok;
}
void Poll() {
    if (!g_ready) return;
    xSemaphoreTake(g_lock, portMAX_DELAY);
    if (g_count && esp_timer_get_time() - g_last_flush_us >= 30000000) FlushLocked();
    xSemaphoreGive(g_lock);
}
std::string Export(const protocol::Command& c) {
    const std::string* id = c.has_id ? &c.id : nullptr;
    if (!g_ready) return protocol::ReplyError("Event log unavailable", id);
    xSemaphoreTake(g_lock, portMAX_DELAY);
    if (!FlushLocked()) {
        xSemaphoreGive(g_lock);
        return protocol::ReplyError("Event log write failed", id);
    }
    const size_t capacity = g_journal.Capacity();
    const size_t anchor = c.log_resume ? c.log_anchor : g_journal.Next();
    const uint64_t snapshot = c.log_resume ? c.log_snapshot : g_journal.Sequence();
    size_t cursor = c.log_cursor;
    if (c.log_resume && (!g_export_active || anchor != g_export_anchor ||
        snapshot != g_export_snapshot || g_flash.erases != g_export_erases)) {
        xSemaphoreGive(g_lock);
        return protocol::ReplyError("Log snapshot expired; start a new export", id);
    }
    if (!c.log_resume) {
        g_export_active = true;
        g_export_anchor = anchor;
        g_export_snapshot = snapshot;
        g_export_erases = g_flash.erases;
    }
    if (anchor >= capacity || cursor > capacity || snapshot > g_journal.Sequence()) {
        xSemaphoreGive(g_lock);
        return protocol::ReplyError("Invalid log cursor", id);
    }
    cJSON* root = cJSON_CreateObject();
    cJSON_AddStringToObject(root, "status", "ok");
    if (id) cJSON_AddStringToObject(root, "id", id->c_str());
    cJSON_AddNumberToObject(root, "format", 1);
    cJSON_AddNumberToObject(root, "anchor", anchor);
    StringNumber(root, "snapshot", snapshot);
    cJSON_AddNumberToObject(root, "capacity", capacity);
    cJSON_AddNumberToObject(root, "invalid_records", g_journal.InvalidRecords());
    cJSON_AddNumberToObject(root, "dropped_this_boot", g_dropped);
    cJSON_AddNumberToObject(root, "io_errors_this_boot", g_io_errors);
    cJSON* entries = cJSON_AddArrayToObject(root, "entries");
    unsigned count = 0;
    bool read_ok = true;
    while (cursor < capacity && count < 8) {
        event_journal::Record r;
        if (!g_journal.Read((anchor + cursor) % capacity, &r)) { read_ok = false; break; }
        ++cursor;
        if (!event_journal::Valid(r) || r.sequence > snapshot) continue;
        cJSON* entry = cJSON_CreateObject();
        StringNumber(entry, "sequence", r.sequence);
        StringNumber(entry, "boot", r.boot);
        StringNumber(entry, "utc_secs", r.utc_secs);
        StringNumber(entry, "uptime_ms", r.uptime_ms);
        cJSON_AddStringToObject(entry, "time_quality", r.flags & event_journal::kTimeValid ? "rtc" :
                                r.flags & event_journal::kTimeEstimated ? "estimated" : "unknown");
        cJSON_AddStringToObject(entry, "event", r.event);
        cJSON_AddItemToArray(entries, entry);
        ++count;
    }
    cJSON_AddNumberToObject(root, "cursor", cursor);
    cJSON_AddBoolToObject(root, "done", cursor == capacity);
    xSemaphoreGive(g_lock);
    if (!read_ok) {
        cJSON_Delete(root);
        return protocol::ReplyError("Event log read failed", id);
    }
    return codec::Print(root);
}
}  // namespace event_log
