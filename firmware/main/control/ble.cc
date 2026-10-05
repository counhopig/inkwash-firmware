#include "control/ble.h"

#include <cstring>
#include <atomic>
#include <mutex>

#include "esp_log.h"
#include "esp_random.h"
#include "host/ble_hs.h"
#include "host/util/util.h"
#include "nimble/nimble_port.h"
#include "nimble/nimble_port_freertos.h"
#include "services/gap/ble_svc_gap.h"
#include "services/gatt/ble_svc_gatt.h"

extern "C" void ble_store_config_init(void);

namespace ble {
namespace {

constexpr char kTag[] = "ble";
constexpr char kName[] = "Inkwash";

// d2c25e50-5e22-48d8-a8b3-34f2f8e2c7d4 and siblings, little-endian.
const ble_uuid128_t kServiceUuid = BLE_UUID128_INIT(
    0xd4, 0xc7, 0xe2, 0xf8, 0xf2, 0x34, 0xb3, 0xa8, 0xd8, 0x48, 0x22, 0x5e, 0x50, 0x5e, 0xc2, 0xd2);
const ble_uuid128_t kWriteUuid = BLE_UUID128_INIT(
    0xd4, 0xc7, 0xe2, 0xf8, 0xf2, 0x34, 0xb3, 0xa8, 0xd8, 0x48, 0x22, 0x5e, 0x51, 0x5e, 0xc2, 0xd2);
const ble_uuid128_t kNotifyUuid = BLE_UUID128_INIT(
    0xd4, 0xc7, 0xe2, 0xf8, 0xf2, 0x34, 0xb3, 0xa8, 0xd8, 0x48, 0x22, 0x5e, 0x52, 0x5e, 0xc2, 0xd2);

std::atomic<bool> g_active{false};
uint32_t g_passkey = 0;
std::atomic<uint16_t> g_conn{BLE_HS_CONN_HANDLE_NONE};
uint16_t g_notify_handle = 0;
std::string g_last_reply;
std::mutex g_reply_mutex;
std::function<bool(const std::string&)> g_on_command;
std::function<void(Event)> g_on_event;

void Advertise();

int AccessWrite(uint16_t, uint16_t, ble_gatt_access_ctxt* ctxt, void*) {
    if (ctxt->op != BLE_GATT_ACCESS_OP_WRITE_CHR) return BLE_ATT_ERR_UNLIKELY;
    const uint16_t len = OS_MBUF_PKTLEN(ctxt->om);
    if (len == 0 || len > 512) return BLE_ATT_ERR_INVALID_ATTR_VALUE_LEN;
    std::string line(len, '\0');
    uint16_t copied = 0;
    if (ble_hs_mbuf_to_flat(ctxt->om, line.data(), len, &copied) != 0) {
        return BLE_ATT_ERR_UNLIKELY;
    }
    line.resize(copied);
    return g_on_command && g_on_command(line) ? 0 : BLE_ATT_ERR_INSUFFICIENT_RES;
}

int AccessNotify(uint16_t, uint16_t, ble_gatt_access_ctxt* ctxt, void*) {
    if (ctxt->op != BLE_GATT_ACCESS_OP_READ_CHR) return BLE_ATT_ERR_UNLIKELY;
    std::lock_guard<std::mutex> lock(g_reply_mutex);
    return os_mbuf_append(ctxt->om, g_last_reply.data(), g_last_reply.size()) == 0
               ? 0
               : BLE_ATT_ERR_INSUFFICIENT_RES;
}

const ble_gatt_chr_def kCharacteristics[] = {
    {
        .uuid = &kWriteUuid.u,
        .access_cb = AccessWrite,
        .flags = BLE_GATT_CHR_F_WRITE | BLE_GATT_CHR_F_WRITE_ENC | BLE_GATT_CHR_F_WRITE_AUTHEN,
    },
    {
        .uuid = &kNotifyUuid.u,
        .access_cb = AccessNotify,
        .flags = BLE_GATT_CHR_F_READ | BLE_GATT_CHR_F_READ_ENC | BLE_GATT_CHR_F_READ_AUTHEN |
                 BLE_GATT_CHR_F_NOTIFY,
        .val_handle = &g_notify_handle,
    },
    {},
};

const ble_gatt_svc_def kServices[] = {
    {
        .type = BLE_GATT_SVC_TYPE_PRIMARY,
        .uuid = &kServiceUuid.u,
        .characteristics = kCharacteristics,
    },
    {},
};

int OnGap(ble_gap_event* event, void*) {
    switch (event->type) {
        case BLE_GAP_EVENT_CONNECT:
            if (event->connect.status == 0) {
                g_conn = event->connect.conn_handle;
                ESP_LOGI(kTag, "client connected");
                if (g_on_event) g_on_event(Event::Connected);
            } else {
                Advertise();
            }
            return 0;
        case BLE_GAP_EVENT_DISCONNECT:
            ESP_LOGI(kTag, "client disconnected (reason %d)", event->disconnect.reason);
            g_conn = BLE_HS_CONN_HANDLE_NONE;
            if (g_on_event) g_on_event(Event::Disconnected);
            if (g_active) Advertise();
            return 0;
        case BLE_GAP_EVENT_ADV_COMPLETE:
            if (g_active && g_conn == BLE_HS_CONN_HANDLE_NONE) Advertise();
            return 0;
        case BLE_GAP_EVENT_ENC_CHANGE:
            if (event->enc_change.status == 0) {
                if (g_on_event) g_on_event(Event::Encrypted);
            } else {
                ESP_LOGW(kTag, "encryption failed (%d)", event->enc_change.status);
            }
            return 0;
        case BLE_GAP_EVENT_PASSKEY_ACTION: {
            if (event->passkey.params.action == BLE_SM_IOACT_DISP) {
                ble_sm_io pkey = {};
                pkey.action = BLE_SM_IOACT_DISP;
                pkey.passkey = g_passkey;
                ble_sm_inject_io(event->passkey.conn_handle, &pkey);
            }
            return 0;
        }
        case BLE_GAP_EVENT_REPEAT_PAIRING: {
            // The phone forgot the bond; drop ours and pair again.
            ble_gap_conn_desc desc;
            if (ble_gap_conn_find(event->repeat_pairing.conn_handle, &desc) == 0) {
                ble_store_util_delete_peer(&desc.peer_id_addr);
            }
            return BLE_GAP_REPEAT_PAIRING_RETRY;
        }
        default:
            return 0;
    }
}

void Advertise() {
    ble_hs_adv_fields fields = {};
    fields.flags = BLE_HS_ADV_F_DISC_GEN | BLE_HS_ADV_F_BREDR_UNSUP;
    fields.name = reinterpret_cast<const uint8_t*>(kName);
    fields.name_len = std::strlen(kName);
    fields.name_is_complete = 1;
    fields.uuids128 = &kServiceUuid;
    fields.num_uuids128 = 1;
    fields.uuids128_is_complete = 1;
    if (ble_gap_adv_set_fields(&fields) != 0) {
        ESP_LOGE(kTag, "set advertisement data failed");
        return;
    }
    ble_gap_adv_params params = {};
    params.conn_mode = BLE_GAP_CONN_MODE_UND;
    params.disc_mode = BLE_GAP_DISC_MODE_GEN;
    uint8_t own_addr_type = 0;
    ble_hs_id_infer_auto(0, &own_addr_type);
    const int rc = ble_gap_adv_start(own_addr_type, nullptr, BLE_HS_FOREVER, &params, OnGap, nullptr);
    if (rc != 0 && rc != BLE_HS_EALREADY) ESP_LOGE(kTag, "advertise failed (%d)", rc);
}

void OnSync() {
    ble_hs_util_ensure_addr(0);
    Advertise();
}

void HostTask(void*) {
    nimble_port_run();
    nimble_port_freertos_deinit();
}

}  // namespace

bool Start(uint32_t* passkey, std::function<bool(const std::string&)> on_command,
           std::function<void(Event)> on_event) {
    if (g_active) return false;
    g_on_command = std::move(on_command);
    g_on_event = std::move(on_event);
    g_passkey = esp_random() % 1000000;
    {
        std::lock_guard<std::mutex> lock(g_reply_mutex);
        g_last_reply.clear();
    }
    if (nimble_port_init() != ESP_OK) {
        ESP_LOGE(kTag, "NimBLE init failed");
        return false;
    }
    ble_hs_cfg.sync_cb = OnSync;
    ble_hs_cfg.store_status_cb = ble_store_util_status_rr;
    ble_hs_cfg.sm_io_cap = BLE_SM_IO_CAP_DISP_ONLY;
    ble_hs_cfg.sm_bonding = 1;
    ble_hs_cfg.sm_mitm = 1;
    ble_hs_cfg.sm_sc = 1;
    ble_hs_cfg.sm_our_key_dist = BLE_SM_PAIR_KEY_DIST_ENC | BLE_SM_PAIR_KEY_DIST_ID;
    ble_hs_cfg.sm_their_key_dist = BLE_SM_PAIR_KEY_DIST_ENC | BLE_SM_PAIR_KEY_DIST_ID;
    ble_svc_gap_init();
    ble_svc_gatt_init();
    if (ble_gatts_count_cfg(kServices) != 0 || ble_gatts_add_svcs(kServices) != 0) {
        ESP_LOGE(kTag, "GATT service registration failed");
        nimble_port_deinit();
        return false;
    }
    ble_svc_gap_device_name_set(kName);
    // JSON replies exceed the default 23-byte ATT MTU.
    ble_att_set_preferred_mtu(517);
    ble_store_config_init();
    g_active = true;
    nimble_port_freertos_init(HostTask);
    *passkey = g_passkey;
    ESP_LOGI(kTag, "advertising");
    return true;
}

void Stop() {
    if (!g_active) return;
    g_active = false;
    ble_gap_adv_stop();
    if (g_conn != BLE_HS_CONN_HANDLE_NONE) {
        ble_gap_terminate(g_conn, BLE_ERR_REM_USER_CONN_TERM);
    }
    if (nimble_port_stop() == 0) {
        nimble_port_deinit();
    }
    g_conn = BLE_HS_CONN_HANDLE_NONE;
    ESP_LOGI(kTag, "stopped");
}

bool Active() { return g_active; }

bool Notify(const std::string& json) {
    {
        std::lock_guard<std::mutex> lock(g_reply_mutex);
        g_last_reply = json;
    }
    const uint16_t connection = g_conn.load();
    if (connection == BLE_HS_CONN_HANDLE_NONE) return false;
    os_mbuf* om = ble_hs_mbuf_from_flat(json.data(), json.size());
    return om && ble_gatts_notify_custom(connection, g_notify_handle, om) == 0;
}

}  // namespace ble
