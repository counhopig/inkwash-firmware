# 并发模型与资源所有权

> 行号基于 commit `b30c3af`。线程与栈大小已逐处核对；共享资源与仲裁标志来自
> `esp32_review` 静态分析 + 人工复核。

## 1. 线程与栈预算

| 线程 | 创建点 | 栈 | 分配方式 | 职责 | 独占资源 |
|---|---|---|---|---|---|
| 主线程 (`main`) | — | 32 KiB (`sdkconfig.defaults:1`) | IDF 主任务 | 唯一事件循环 + 主线程 Effect | `DeviceContext`、`Canvas`、`Note4Board` 引脚、音频句柄、渲染注册表 |
| `rtc` | `rtc_executor.rs:66-69` | 8 KiB (`:13`) | `Builder` | 串行化 PCF8563 全部 I2C | PCF8563（唯一所有者，`:168` 日志自述） |
| `epd` | `epd_task.rs:229-232` | 12 KiB (`:215`) | `Builder` | 唯一 EPD 刷新执行者 | `zectrix_epd_handle_t`、`scratch` |
| `sync` | `sync_task.rs:75-78` | 16 KiB (`:17`) | `Builder` | Wi-Fi/HTTPS/NTP | **`WifiManager`（含 `EspWifi`）**、NVS store 副本 |
| `ble` | `ble_control.rs:349-356` | 16 KiB (`:19`) | `spawn_internal_stack` | NimBLE 主机/广播/notify | NimBLE 全局状态 |
| `effect-task` | `effect_task.rs:306-308` | 16 KiB (`:20`) | `spawn_internal_stack` | 执行 worker-safe Effect 批次 | NVS store 副本 + `RtcExecutor` 句柄 |
| `audio` | `audio_task.rs:46-49` | 8 KiB (`:47`) | `Builder` | ES8311 音调 | I2S + ES8311 |
| `usb-console-rx` | `usb_console.rs:28-32` | 12 KiB (`:12`) | `Builder` | 行解析 | stdin |
| `usb-console-writer` | `usb_console.rs:138-141` | 8 KiB (`:13`) | `Builder` | 应答写出 | stdout |

**合计 131,072 字节 = 恰好 128 KiB。** 其中 `ble`、`effect-task` 经
`tasks.rs:9-10` 被钉在 **内 RAM**
（`MALLOC_CAP_INTERNAL | MALLOC_CAP_8BIT`），其余 7 个线程使用 IDF 默认策略。

再加 IDF 自身的 NimBLE host（5 KiB，`sdkconfig.defaults:73`）、WiFi、lwIP、esp_timer、
双 IDLE（4 KiB each），**内 RAM 的栈占用超过 160 KiB**。

> **这是本板的第一约束。** 内存吃紧 → BLE init 前必须腾地方 → 于是有
> `ble_memory.rs` 的堆门、整条 `SuspendForBle`/`ResumeAfterBle` 交棒链、
> `ctx.rs` 的 6 个 `ble_*` 字段、`main.rs:498-809` 约 300 行会话管理。
> `sync`(16K) 与 `ble`(16K) 因硬件互斥**永不同时工作**，合并可省 16 KiB；
> `usb-rx`+`usb-writer` 合并再省 8 KiB。详见 `hardware-assessment.md`。

### 看门狗覆盖（关键缺口）

`CONFIG_ESP_TASK_WDT_TIMEOUT_S=10` + `CONFIG_ESP_TASK_WDT_PANIC=y`（`sdkconfig.defaults:58-59`）。

**只有 4 个线程订阅了 TWDT**：

| 订阅点 | 线程 |
|---|---|
| `main.rs:89` | 主线程 |
| `rtc_executor.rs:152` | `rtc` |
| `sync_task.rs:127` | `sync` |
| `audio_task.rs:83` | `audio` |

**未订阅**：`epd`、`ble`、`effect-task`、`usb-console-rx`、`usb-console-writer`。

其中 **`effect-task` 最严重**：它是唯一 NVS 写入执行者。若它挂起（不 disconnect、
不返回），`main.rs:1579` 的 `try_next_notice` 永远收不到东西 →
`worker_batch_in_flight` 永远为 true → `reduce_effect_batches` 在 `main.rs:1637`
直接 break → **所有持久化永久停摆，而设备看起来完全正常**（时钟走、闹钟响、界面刷新，
只是改动不再落盘）。无超时、无计数、无恢复路径。见 `review-findings.md` P0-1。

### 一个隐式单线程契约

`tasks.rs:7-25` 修改的是 **IDF 进程级 pthread 默认配置**，无锁：

```rust
let mut cfg = unsafe { esp_sys::esp_pthread_get_default_config() };
cfg.stack_size = stack_size;
cfg.stack_alloc_caps = MALLOC_CAP_INTERNAL | MALLOC_CAP_8BIT;
esp_sys::esp_pthread_set_cfg(&cfg)      // ← 全局状态
let spawned = thread::Builder::new()...spawn(body);
let default_cfg = esp_sys::esp_pthread_get_default_config();
esp_sys::esp_pthread_set_cfg(&default_cfg)   // ← 恢复
```

当前只有主线程调用（`ble_control.rs:349`、`effect_task.rs:306`），所以安全。
但这是**无注释、无测试守护的隐式契约**——从别的线程新增 spawn 点即竞态。

## 2. 共享资源矩阵

### canvas（帧缓冲）

| handle | 声明 | 引用自 |
|---|---|---|
| `canvas` | `display.rs:17` | `app_runner.rs`, `display.rs`, `epd_task.rs`, `home.rs`, `icons.rs`, `main.rs`, `screens.rs`, `ui.rs`, `tools/preview` |

**关键设计**：epd 线程**从不读 `Canvas`**。`display.rs:63-69` 把整帧
`to_vec().into_boxed_slice()` 后交给 `RenderCommand`（`epd_task.rs:81-86`），
`MutexGuard` 在提交前释放。用 15 KB 拷贝换掉一个不可能的数据竞争——在 240 MHz 上是对的。

锁本身是 `parking_lot::Mutex`，临界区只覆盖绘制与整帧复制，无阻塞等待。

### i2c-bus（唯一跨线程共享的物理总线）

| handle | 声明 | 引用自 |
|---|---|---|
| `i2c_bus` | `board.rs:51`（`Arc<Mutex<I2cDriver>>`，`:186` 构造） | `board.rs`, `main.rs` |
| `i2c` | `audio.rs:56` | `audio.rs`, `board.rs`, `nfc.rs` |
| `i2c` | `nfc.rs:18` | 同上 |
| `bus` | `rtc.rs:21` | `rtc.rs`, `rtc_executor.rs` |
| `bus` | `rtc_executor.rs:151` | `rtc.rs`, `rtc_executor.rs` |

共 6 个文件、34 处引用。**实际锁竞争已被限制**：

| 使用者 | 频率 | 每次事务 |
|---|---|---|
| RTC | 每 1.2 s（活动）/ 10 s（空闲）读一次 | 7 字节 |
| RTC 闹钟状态 | 每轮循环一次 | 1 字节 × 2 |
| ES8311 | 仅初始化 | — |
| NFC | 仅初始化 | — |

即 RTC 每轮最多两次 1 字节读，其余为零。**这是可接受的**，但见 `review-findings.md` P2
关于 `rtc_executor` 与主线程能否并发持有锁的确认建议。

### 其它

| handle | 声明 | 说明 |
|---|---|---|
| `notify_char` | `ble_control.rs:801` | `Arc<esp32_nimble::Mutex<BLECharacteristic>>`，仅 ble 线程 |
| `mailbox` | `audio_task.rs:27` | `Arc<std::sync::Mutex<AudioMailbox>>`，主线程入队 / audio 线程 drain，有损背压 |
| `notify_tx_pending` | `ble_control.rs:799` | `Arc<StdMutex<VecDeque>>`，NimBLE 回调（ISR 上下文）→ ble 线程 |
| `lifecycle_mailbox` | `ble_control.rs:797` | `Arc<(Mutex<LifecycleMailbox>, Condvar)>`，带阻塞等待（`ble_control.rs:205-214`） |

## 3. Effect 批次背压容量表

每一级都有容量 + "退还生产者"路径，**事件不静默丢失**：

| 级 | 容量 | 常量 | 满时行为 |
|---|---|---|---|
| 事件队列 High | 16 | `event_queue.rs:5` | `Err(event)` 退还 |
| 事件队列 Mergeable | 4 | `event_queue.rs:7` | High 退还；**Mergeable 静默丢最旧**（`:115-118`） |
| dispatch 来源：按钮 | 3 | `ctx.rs:45` | 退还 |
| dispatch 来源：生命周期 | 4 | `ctx.rs:46` | 退还 |
| dispatch 来源：worker | 8 | `ctx.rs:47` | 退还 |
| dispatch 来源：sleep | 2 | `ctx.rs:48` | 退还 |
| `pending_app_events` | 16 | `main.rs:1483` | 先退 `PendingDispatchSources`，再退 `pending_dispatch_event`，最后 `Err(DispatchSaturated)` |
| effect 批次通道 | 1 | `effect_task.rs:16` | 保留在 `pending_effect_batch` |
| effect notice 通道 | 8 | `effect_task.rs:18` | 保留在 `pending_effect_notices` |
| BLE 命令/结果通道 | 16 | `ble_control.rs:16` | `pending_stop` / `pending_replies` |
| EPD 完成邮箱 | 16（带 reserve） | `epd_task.rs:16` | `reserve()` 失败则拒绝提交，上层转 `pending_render_retries` |
| 渲染注册表 | 16 | `epd_registry.rs:5` | 进 `pending_render_retries`，超限按 generation 替换 |
| USB 应答邮箱 / 闩锁 | 32 / 1 | `ctx.rs:188` | 闩锁满 → `Err`，调用方 `cancel_pending` |
| BLE 应答邮箱 / 闩锁 | 32 / 1 | `ctx.rs:190` | 同上 |
| BLE worker 内部应答 | 16 | `ble_control.rs:16` | `BleReplyError::QueueFull` |
| 音频邮箱 | 8 | `audio_command.rs:3` | 有损：满时清空队列接受 Stop/StartAlarmTone |

`PendingDispatchSources::take_next()`（`ctx.rs:158-171`）定义了**事件消费优先级**：
`tick → power_poll → rtc_alarm_snapshot → buttons → usb → ble → lifecycle → worker
→ sleep → boot → scheduler`。

## 4. 仲裁标志（feature-arbitration flags）

38 个标志中，多数是"在途操作"记账。以下为**风险相关**的子集；
"S/C" 为置位/清除站点数。

| 标志 | 声明 | S/C | 备注 |
|---|---|---|---|
| `pending_usb_reply` | `app.rs:798` | 1/1 | 命令槽位占用 → `Reply::Busy` |
| `pending_ble_reply` | `app.rs:799` | 1/1 | 同上 |
| `pending_rtc` | `app.rs:819` | 3/2 | 阻塞深睡（`rtc_alarm_plan_confirmed`） |
| `pending_residue_ack` | `app.rs:821` | 4/2 | 残留响铃清理 |
| **`pending_residue_time`** | `app.rs:823` | **1/0** | ⚠️ 只写不读 → 死字段 |
| `pending_sync_*` ×5 | `app.rs:830-834` | — | 同步两阶段在途记账 |
| `pending_urgent_poll` | `app.rs:835` | 3/5 | 阻塞深睡 |
| `pending_wifi_op` | `ctx.rs:240` | 12/1 | 单飞 Wi-Fi 操作；`:861` 取出处理 |
| `ble_wifi_suspended` | `ctx.rs:247` | 1/5 | 射频交棒状态 |
| `ble_set_wifi_after_resume` | `ctx.rs:248` | 1/7 | 交棒期间到达的 SetWifi 延后执行 |
| `pending_ble_handoff` | `ctx.rs:257` | 2/4 | SetWifi 跨代际交棒 |
| `pending_ble_set_wifi_ack` | `ctx.rs:258` | 4/5 | 配对成功确认 |
| **`pending_ble_pairing_success`** | `ctx.rs:260` | **0/2** | ⚠️ 只清不写 → 死字段 |
| **`pending_render_completion`** | `ctx.rs:272` | **0/1** | ⚠️ 只清不写 → 死字段 |
| `pending_sleep_kick` | `ctx.rs:274` | 2/1 | 睡眠 kick 待办 |
| `worker_batch_in_flight` | `ctx.rs:290` | 2/1 | ⚠️ 见 P0-1 |
| `inflight` (BLE) | `ble_control.rs:805` | 1/5 | notify 在途 + 2 s 超时兜底 |
| `suspended_was_started` | `wifi.rs:28` | 1/2 | 交棒前是否已 start |

**已验证的死标志**（grep 全仓库）：

- `ctx.rs:260` `pending_ble_pairing_success`：仅 `main.rs:512, 1399` 清除，无处赋值。
- `ctx.rs:272` `pending_render_completion`：仅 `main.rs:811` 读取（恒 `None`，分支从不进入）。
- `logic/src/app.rs:823` `pending_residue_time`：仅 `:1754` 赋值，无读取。

三者都是 `29755a8 refactor: land single-event-loop firmware architecture` 那次
事件循环重构后的残留——**重构删掉了生产者，留下了消费者**。

## 5. 中断上下文

| 来源 | 处理 | 是否安全 |
|---|---|---|
| GPIO 按键 ISR | `wake.rs:15-30`：`gpio_intr_disable` + `xTaskGenericNotifyFromISR` | ✅ 只做通知，不分配、不阻塞 |
| NimBLE notify-tx 回调 | `ble_control.rs:174-198`：先试 `try_send`，满则进 `StdMutex<VecDeque>` | ⚠️ 持 `std::sync::Mutex`；须确认该回调不在真正 ISR 上下文（NimBLE 通常派发到 host task） |
| BLE lifecycle 回调 | `ble_control.rs:205-214`：**阻塞等待** `Condvar` 直到队列有空间 | ⚠️ 同上，阻塞式；若在 ISR 上下文即非法 |

BLE 两处回调的上下文是**唯一需要真机/框架确认的并发点**。若 NimBLE 把回调派发到
host task（`CONFIG_BT_NIMBLE_HOST_TASK_STACK_SIZE=5120`），则当前写法正确；
若在 ISR，则 `Condvar` 等待与 `Mutex` 都是问题。**(未验证)**

## 6. 资源所有权小结

```
PCF8563 ──────────── rtc 线程（唯一所有者）
EPD 驱动 ─────────── epd 线程（唯一所有者）
EspWifi ──────────── sync 线程（main.rs:306 move 进入，主线程只能发命令）
Canvas  ──────────── 主线程（epd 线程只收拷贝）
NimBLE  ──────────── ble 线程（按需创建，ble_control.rs:344-357）
ES8311/I2S ───────── audio 线程（main.rs:253 交出所有权）
stdin/stdout ─────── usb-console-rx / -writer
NVS  ─────────────── 3 条写入路径：主线程 Effect、effect-task、sync 线程
I2C0 ─────────────── Arc<Mutex>，rtc / audio / nfc 共用
```

**唯一"多写入者"是 NVS**：`main.rs:157-192` 给 `effect_task` 另开一套 store，
`sync_task.rs:135` 再开一套。ESP-IDF NVS 本身线程安全，这是为了避免跨线程借用的
设计选择而非 bug，但意味着同一份数据有三条写入路径——改持久化逻辑时必须同时考虑三处。
