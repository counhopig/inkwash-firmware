# 控制协议（USB / BLE）

> **由源码复原**。原始 `docs/control-protocol.md` 已被 commit `b30c3af` 删除。
> 本文件的每条命令与 JSON 形状都**经实际运行验证**（用 `inkwash-logic` 直接
> 序列化/反序列化确认），不是从代码推断。

## 1. 传输层差异

两条链路共享同一套 JSON 协议，但**成帧方式不同**：

| | USB (USB-Serial-JTAG) | BLE (GATT) |
|---|---|---|
| 上行成帧 | `>>IW ` + JSON + `\n` | **裸 JSON**（无前缀、无换行要求） |
| 下行成帧 | `<<IW ` + JSON + `\n` | **裸 JSON** |
| 最大长度 | 512 字节/行（`usb_console.rs:10`）超出则丢弃整行并告警（`:88-93`） | 受协商 MTU 限制 |
| 解析位置 | `usb_console.rs:74-85` | `ble_control.rs:942-970` |
| 解析失败 | 记 `log::warn` 并丢弃该行 | 记 `log::warn` 并 `args.reject()` |
| 无前缀的行 | **静默忽略**（`usb_console.rs:74` 的 `strip_prefix` 为 `None` 时不进入解析） | 不适用 |

**前缀字面量**：`COMMAND_PREFIX = ">>IW "`、`REPLY_PREFIX = "<<IW "`
（`usb_console.rs:7,9`）。注意包含尾随空格。

**BLE GATT 定义**（`ble_control.rs:24-26`）：

| 用途 | UUID |
|---|---|
| Service | `d2c25e50-5e22-48d8-a8b3-34f2f8e2c7d4` |
| Write（上位机 → 设备） | `d2c25e51-5e22-48d8-a8b3-34f2f8e2c7d4`（`WRITE`，`ble_control.rs:932-933`） |
| Notify（设备 → 上位机） | `d2c25e52-5e22-48d8-a8b3-34f2f8e2c7d4`（`READ \| NOTIFY`，`ble_control.rs:934-937`） |

设备是 **GATT peripheral，单连接**（`CONFIG_BT_NIMBLE_MAX_CONNECTIONS=1`）。
第二个连接会被主动断开（`ble_control.rs:888-891`）。

## 2. 请求（上位机 → 设备）

`Command` 用 `#[serde(tag = "cmd", rename_all = "snake_case")]`
（`logic/src/protocol.rs:9-25`），即**内部标签**：变体名成为顶层 `cmd` 字段的值。

| `cmd` | 参数 | 示例 |
|---|---|---|
| `get_status` | — | `{"cmd":"get_status"}` |
| `sync_now` | — | `{"cmd":"sync_now"}` |
| `set_wifi` | `ssid`, `password` | `{"cmd":"set_wifi","ssid":"MyAP","password":"secret"}` |
| `set_server` | `url`, `token` | `{"cmd":"set_server","url":"https://example.com","token":"tok"}` |
| `set_rtc` | `epoch_secs` (u64) | `{"cmd":"set_rtc","epoch_secs":1770000000}` |
| `set_timezone` | `offset_minutes` (i16) | `{"cmd":"set_timezone","offset_minutes":480}` |
| `clear_alarms` | — | `{"cmd":"clear_alarms"}` |

### 可选 `id`（请求关联）

任意请求可带顶层字符串字段 `id`，设备会在应答中原样回传
（`control.rs:15-18` 读取，`control.rs:28-30` 写回）。

```
>>IW {"cmd":"get_status","id":"req-1"}
<<IW {"id":"req-1","status":"status", ...}
```

- `id` **不是** `Command` 的字段；反序列化时作为未知字段被忽略（已实测：
  `{"cmd":"get_status","extra":1}` 解析成功）。
- `id` 用于**幂等重放**：设备缓存最近 8 条终态应答（`command_sessions.rs:5`），
  同 `id` 重发直接回放而不重新执行。**但缓存满会静默淘汰最旧条目**，
  所以幂等性不是严格的——见 `review-findings.md` P1-4。
- 不带 `id` 的请求无法去重。

### 入参约束

| 命令 | 约束 | 违例应答 | 来源 |
|---|---|---|---|
| `set_rtc` | `epoch_secs ∈ [946684800, 4102444800]`（2000-01-01 ～ 2100-01-01） | `error` | `app.rs:3293-3295` |
| `set_timezone` | `offset_minutes ∈ [-720, 840]` | `error` | `storage.rs:146-153` |
| `sync_now` | 时钟必须可用 | `error: "System time not available"` | `app.rs:3334-3342` |
| `sync_now` | 已有同步在途 | `busy` | `app.rs:3330-3333` |
| `set_wifi` | `ssid ≤ 32` 字符、`password ≤ 64` 字符 | `error`（在 sync 线程侧） | `wifi.rs:70-73` |
| 任意 | 该通道已有未完成命令 | `busy` | `app.rs:3283-3289` |
| 任意 | JSON 嵌套深度 > 4 | 解析失败/丢弃 | `protocol.rs:56`、`control.rs:8-12` |

`MAX_COMMAND_NESTING = 4`（`protocol.rs:56`）是为保护解析栈而设
（`protocol.rs:141-148` 的测试注释提到 "the overflow the 4096-byte worker stack"）。

## 3. 应答（设备 → 上位机）

`Reply` 用 `#[serde(tag = "status", rename_all = "snake_case")]`
（`logic/src/protocol.rs:27-50`）。

| 变体 | 线上 JSON |
|---|---|
| `Ok` | `{"status":"ok"}` |
| `Busy` | `{"status":"busy"}` |
| `Pending` | `{"status":"pending"}` |
| `Error` | `{"status":"error","message":"..."}` |
| `Status` | `{"status":"status", ...}` |

带 `id` 时额外插入 `"id"` 字段（实测 `{"id":"a1","status":"ok"}`）。

### ⚠️ `Status` 应答的标签值就是 `"status"`

变体名 `Status` 经 `snake_case` 后与其标签字段名相同，因此线上是
**`{"status":"status", ...}`**（已实测）。上位机判别时应匹配
`status == "status"`，不要误以为 `"status"` 是某种错误。

完整字段（实测输出）：

```json
{
  "status": "status",
  "wifi_configured": true,
  "server_configured": false,
  "wifi_connected": false,
  "wifi_ssid": "MyAP",
  "wifi_has_password": true,
  "server_url": null,
  "server_has_token": false,
  "timezone_offset_minutes": 480
}
```

- `wifi_ssid` / `server_url` 为 `null` 表示未配置。
- `wifi_has_password` / `server_has_token` 是**布尔标志**——设备从不回传
  密码或 token 本体。
- `wifi_connected` 表示当前是否已连接（由状态机的 connectivity 决定）。

### 序列化兜底

若 `Reply` 序列化失败（理论上不会），`control.rs:24-27` 返回固定串：
`{"status":"error","message":"Failed to serialize reply"}`。

### 终态与在途

`command_sessions.rs:262-267` 把 `Ok` / `Status` / `Error` 视为**终态**并写入
幂等缓存；`Pending` 与 `Busy` 不写入。

## 4. 典型会话

### USB

```
>>IW {"cmd":"get_status","id":"1"}
<<IW {"id":"1","status":"status","wifi_configured":true,...}

>>IW {"cmd":"set_wifi","ssid":"MyAP","password":"secret","id":"2"}
<<IW {"id":"2","status":"ok"}

>>IW {"cmd":"sync_now","id":"3"}
<<IW {"id":"3","status":"ok"}
```

### BLE（同一命令，无前缀）

```
写 d2c25e51: {"cmd":"get_status","id":"1"}
收 d2c25e52 通知: {"id":"1","status":"status",...}
```

> BLE 的 notify 有 **2 秒超时兜底**（`ble_control.rs:758-782`）：若某条应答的
> notify-tx 回调迟迟不返回或连接已断，设备把它标记为 `ReplyTerminated`
> 并按 generation + conn_handle 丢弃，不会永久阻塞后续命令。
> 上位机侧遇到无响应应重发（带同一 `id` 可命中幂等缓存）。

## 5. 设备状态机侧的行为

- **命令槽位**：每条通道（USB / BLE）同时只允许一条未完成命令。
  槽位被占时新命令立即得 `busy`（`app.rs:3282-3287`）。
- **任务路由**：`sync_now` / `set_wifi` 走 sync 线程（异步，业务结果通过
  `Effect::Reply` 回投）；`set_rtc` / `set_timezone` / `set_server` /
  `clear_alarms` 立即回 `ok` 或 `error`。
- **USB 会话号**：主机插拔会使会话号自增（`main.rs:480-494`），
  旧会话的未完成应答被取消。BLE 侧对应 connection generation + conn_handle。
- **安全模式**：核心启动事实不可用（RTC 读失败、NVS 打开失败等）时设备进入
  safe mode，**只有 `get_status` 被应答**（返回全 `false`/`0`），
  其余命令回 `error: "device is in safe mode (core data unavailable); command ... rejected"`
  （`main.rs:1100-1135`）。

## 6. USB 附加：串口日志

USB-Serial-JTAG 同时承载**日志输出**（`CONFIG_ESP_CONSOLE_USB_SERIAL_JTAG=y`）
与控制协议。上位机解析时应只识别 `<<IW ` 前缀行，其余为 `log` crate 输出。

`scripts/capture-serial.py` 提供了非交互式采集（自动探测
`/dev/cu.usbmodem*` 与 `/dev/ttyACM*`），可用于抓取上述日志与控制应答。

## 7. 已知未覆盖

- 原始 `docs/control-protocol.md` 可能还记录了**版本协商、错误码表、
  上位机重试建议**等内容；这些未在源码中体现，本文件无法复原。
- BLE 配对流程（`Screen::BlePairing`）的交互时序未在协议层定义，
  属于 UI 状态机（`logic/src/app.rs` 的 `transition_ble_pairing_*`），
  必要时应单独成文。
