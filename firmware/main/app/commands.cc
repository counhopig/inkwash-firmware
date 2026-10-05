#include "app/internal.h"

namespace app::detail {

// ---- Commands ---------------------------------------------------------------------------

void SendReply(Channel ch, const std::string& json) {
    if (ch == Channel::Usb) usb_console::Reply(json);
    else ble::Notify(json);
}

PendingReply& Slot(Channel ch) { return ch == Channel::Usb ? g.usb_reply : g.ble_reply; }

void Resolve(Channel ch, const std::string& json) {
    SendReply(ch, json);
    Slot(ch).active = false;
}

const std::string* IdOf(const PendingReply& p) { return p.has_id ? &p.id : nullptr; }

bool StartNet(NetJob job, const store::WifiCreds* creds) {
    if (g.net_job != NetJob::None) return false;
    if (!g_net_task && xTaskCreate(NetTask, "net", 16384, nullptr, 4, &g_net_task) != pdPASS) {
        ESP_LOGE(kTag, "network worker unavailable");
        return false;
    }
    auto* req = new NetRequest{job, g.now, creds ? *creds : store::WifiCreds{}};
    if (xQueueSend(g_net, &req, 0) != pdTRUE) {
        delete req;
        return false;
    }
    g.net_job = job;
    g.net_started_ms = NowMs();
    ESP_LOGI(kTag, "net job %d started", static_cast<int>(job));
    return true;
}


void HandleCommand(Channel ch, const std::string& line) {
    protocol::Command c;
    std::string err;
    if (!protocol::Parse(line, &c, &err)) {
        SendReply(ch, protocol::ReplyError(err, nullptr));
        return;
    }
    g.last_activity_ms = NowMs();
    g.background = false;
    const std::string* id = c.has_id ? &c.id : nullptr;
    PendingReply& slot = Slot(ch);
    if (slot.active) {
        SendReply(ch, protocol::ReplyBusy(id));
        return;
    }
    switch (c.cmd) {
        case protocol::Cmd::GetStatus: {
            protocol::Status s;
            s.wifi_configured = g.wifi_configured;
            s.server_configured = g.server_configured;
            s.wifi_connected = wifi::IsConnected();
            s.has_ssid = g.wifi_configured;
            s.wifi_ssid = g.wifi_ssid;
            s.wifi_has_password = g.wifi_has_password;
            s.has_server_url = g.server_configured;
            s.server_url = g.server_url;
            s.server_has_token = g.server_has_token;
            s.timezone_offset_minutes = g.timezone;
            SendReply(ch, protocol::ReplyStatus(s, id));
            return;
        }
        case protocol::Cmd::SetRtc: {
            if (c.epoch_secs < 946684800ULL || c.epoch_secs > 4102444800ULL) {
                SendReply(ch, protocol::ReplyError("RTC timestamp must be between 2000-01-01 and 2100-01-01", id));
                return;
            }
            const DateTime local = DateTime::FromUnix(c.epoch_secs).ShiftedMinutes(g.timezone);
            if (local.year < 2000 || local.year > 2099) {
                SendReply(ch, protocol::ReplyError("RTC local time must remain between 2000-01-01 and 2099-12-31 after timezone conversion", id));
                return;
            }
            store::ClearRtcAlignEpoch();
            if (!pcf8563::WriteTime(local)) {
                SendReply(ch, protocol::ReplyError("RTC write failed", id));
                return;
            }
            g.now = local;
            g.have_clock = true;
            g.programmed_valid = false;
            g.last_full_boundary = Boundary(g.now.ToUnix(), std::max<uint16_t>(1, g.sync_interval) * 60ULL);
            g.last_urgent_boundary = Boundary(g.now.ToUnix(), kUrgentPeriodSecs);
            power::State().network_retry_utc = 0;
            ProgramRtcAlarm();
            SendReply(ch, protocol::ReplyOk(id));
            Render();
            return;
        }
        case protocol::Cmd::SetTimezone: {
            if (c.offset_minutes < store::kMinTimezone || c.offset_minutes > store::kMaxTimezone) {
                SendReply(ch, protocol::ReplyError("Timezone offset must be between -720 and 840 minutes", id));
                return;
            }
            if (c.offset_minutes == g.timezone) {
                SendReply(ch, protocol::ReplyOk(id));
                return;
            }
            DateTime current;
            if (!pcf8563::ReadTime(&current) || current.voltage_low) {
                SendReply(ch, protocol::ReplyError("System time not available", id));
                return;
            }
            g.now = current;
            g.have_clock = true;
            const DateTime shifted = g.now.ShiftedMinutes(c.offset_minutes - g.timezone);
            if (!pcf8563::WriteTime(shifted)) {
                SendReply(ch, protocol::ReplyError("Failed to apply the timezone", id));
                return;
            }
            if (!store::SaveTimezone(c.offset_minutes)) {
                if (!pcf8563::WriteTime(current)) {
                    g.have_clock = false;
                    ESP_LOGE(kTag, "timezone persistence and RTC rollback failed");
                }
                SendReply(ch, protocol::ReplyError("Failed to save the timezone", id));
                return;
            }
            g.timezone = c.offset_minutes;
            g.now = shifted;
            g.programmed_valid = false;
            g.last_full_boundary = Boundary(g.now.ToUnix(), std::max<uint16_t>(1, g.sync_interval) * 60ULL);
            g.last_urgent_boundary = Boundary(g.now.ToUnix(), kUrgentPeriodSecs);
            power::State().network_retry_utc = 0;
            ProgramRtcAlarm();
            SendReply(ch, protocol::ReplyOk(id));
            Render();
            return;
        }
        case protocol::Cmd::SetServer: {
            store::ServerConfig cfg{c.url, c.token};
            if (!store::ValidServerUrl(c.url) || c.token.size() > store::kMaxTokenLen) {
                SendReply(ch, protocol::ReplyError("Server URL must be an HTTPS URL without embedded credentials and token must not exceed 255 bytes", id));
                return;
            }
            if (!store::SaveServer(cfg)) {
                SendReply(ch, protocol::ReplyError("Failed to save the server configuration", id));
                return;
            }
            LoadConfig();
            SendReply(ch, protocol::ReplyOk(id));
            return;
        }
        case protocol::Cmd::ClearAlarms: {
            if (g.ringing) StopRinging();
            g.alarms.clear();
            const bool ok = store::SaveAlarms(g.alarms) && pcf8563::ClearAlarm();
            g.programmed_valid = ok;
            g.programmed_none = ok;
            SendReply(ch, ok ? protocol::ReplyOk(id) : protocol::ReplyError("Failed to clear alarms", id));
            Render();
            return;
        }
        case protocol::Cmd::SetWifi: {
            store::WifiCreds creds{c.ssid, c.password};
            if (!store::ValidWifiCreds(creds)) {
                SendReply(ch, protocol::ReplyError("Wi-Fi SSID must be 1-32 bytes; password must be empty, 8-63 bytes, or a 64-digit hexadecimal PSK", id));
                return;
            }
            if (ch == Channel::Ble) {
                // The radio is busy with BLE: save now, prove the credentials
                // with a sync once pairing ends.
                if (!store::SaveWifi(creds)) {
                    SendReply(ch, protocol::ReplyError("Failed to save Wi-Fi credentials", id));
                    return;
                }
                LoadConfig();
                SendReply(ch, protocol::ReplyOk(id));
                g.ble_finish_pending = true;
                g.ble_finish_notice = "PAIRED - WI-FI SAVED";
                return;
            }
            if (!StartNet(NetJob::VerifyWifi, &creds)) {
                SendReply(ch, protocol::ReplyBusy(id));
                return;
            }
            slot = PendingReply{true, c.has_id, c.id, c.cmd};
            g.pending_wifi = creds;
            return;
        }
        case protocol::Cmd::SyncNow: {
            if (ch == Channel::Ble || ble::Active()) {
                SendReply(ch, protocol::ReplyError("Sync is unavailable while BLE pairing is active", id));
                return;
            }
            if (g.net_job != NetJob::None) {
                SendReply(ch, protocol::ReplyBusy(id));
                return;
            }
            if (!g.have_clock) {
                SendReply(ch, protocol::ReplyError("System time not available", id));
                return;
            }
            if (!StartNet(NetJob::Sync)) {
                SendReply(ch, protocol::ReplyBusy(id));
                return;
            }
            slot = PendingReply{true, c.has_id, c.id, c.cmd};
            return;
        }
    }
}


}  // namespace app::detail
