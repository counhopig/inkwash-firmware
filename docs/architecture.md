# 架构

> 全部行号基于 commit `b30c3af`。凡未经真机验证的推断都标注了 **(未验证)**。

## 1. 一句话

**单事件循环 + 纯函数状态机 + 声明式副作用。**

`logic` crate 里没有硬件、没有线程、没有 IO，只有一个纯转移函数
`update(&mut AppState, Event) -> Vec<EffectBatch>`；`rust-firmware` 是它的执行器，
把每个 `Effect` 分派到主线程或 worker，并独占所有真实资源。

```
   [按键] [RTC tick] [USB/BLE 命令] [同步结果] [EPD 完成]
                          │
                    dispatch_or_retain            main.rs:1466
                          ▼
        EventQueue(High 16 / Mergeable 4)         event_queue.rs:16
                          ▼
        update(state, event) → Vec<EffectBatch>   app.rs:969
        （34 个 transition_*，纯函数，无 IO）
                          ▼
        batch_is_worker_safe?                     runner.rs:6
          ├─ 是 → effect-task 线程（仅 NVS/RTC）   effect_task.rs:45
          └─ 否 → 主线程 EffectRunner             app_runner.rs:61
                          ▼
        完成回调 EffectCompleted/Failed → 回到 update（四元组匹配）
```

## 2. 物理分层：两个 crate

| crate | 位置 | 行数 | 主机可跑 | 说明 |
|---|---|---|---|---|
| `inkwash-logic` | `logic/` | 18037 | ✅ `cargo test` | 只依赖 serde |
| `inkwash-note4` | `rust-firmware/` | 10514 | ❌ 需 ESP-IDF | 依赖 logic |
| `inkwash-preview` | `tools/preview/` | 569 | ✅ | 用 `#[path]` 引用固件的 canvas/home/字体 |

依赖方向严格单向：`rust-firmware → logic`（`rust-firmware/Cargo.toml:24`），logic 永不反向依赖。
唯一的"反向可见性"是 `logic/src/lib.rs:33-36` 用 `include_str!` 读取固件源码做契约断言，
只在测试编译期生效。

**这条边界是最有价值的资产**：调度、睡眠准入、渲染分区、协议校验、列表窗口、渲染缓存
全部可在 PC 上毫秒级验证，固件侧只剩"把 Effect 接到外设上"。

## 3. 主循环

`main.rs:400-1059` 是全部逻辑。每轮固定顺序：

```
watchdog::feed()                            main.rs:401
service_effect_task()                       main.rs:402   worker 批次回收
retry_render_registry()                     main.rs:403
service_usb_reply_writer()                  main.rs:404   USB 应答重发
─ 一次性：Boot + SyncSchedulerConfigured     main.rs:405-421
─ 1 Hz：电池/充电状态                        main.rs:424-429
─ 读 RTC：空闲 10 s / 活动 1.2 s             main.rs:431-472  → Event::Tick
─ 闹钟 AF 轮询                               main.rs:474-478
─ USB 主机插拔 → 会话号自增                   main.rs:480-494
─ USB 命令                                   main.rs:496
─ BLE：启动失败→生命周期→worker结果→命令      main.rs:498-809
─ EPD 完成回调 → feed_completion_back         main.rs:811-915
─ Wi-Fi 操作完成                              main.rs:917
─ 按键（三键 × Pressed/LongPressed/Released） main.rs:919-932
─ 睡眠 kick（PrepareSleep / CommitSleep）      main.rs:941-993
─ Event::PowerPoll（睡眠准入的唯一判决输入）    main.rs:995-1035
─ 浅睡：arm → wait / 否则 sleep(20ms)         main.rs:1051-1058
```

三个关键设计：

1. **只有一个大循环**。没有 RTOS 队列分发、没有抢占式任务。所有事件经 `dispatch_or_retain`
   交给状态机，所有动作同步执行。"设备此刻在做什么"完全可枚举。
2. **`PowerPoll` 是睡眠的唯一入口**（`main.rs:996-1029`）。循环打包 8 个布尔量
   （显示待完成/持久化待完成/事件队列空/输入闩锁/USB 连接…）交给状态机，状态机回 Effect。
   **循环本身不判断能不能睡**——判断在 `logic/src/power_state.rs`。
3. **空闲用浅睡替代 `sleep(20ms)`**（`main.rs:1051-1058`）：`wake.arm()` 使能 GPIO0/18/39
   低电平中断，`wake.wait(1000)` 阻塞在 FreeRTOS 任务通知上（`wake.rs:66-76`），
   按键在 ISR 里 `xTaskGenericNotifyFromISR`（`wake.rs:21-28`）立即唤醒。

## 4. 控制回路内核

### 4.1 数据规模

| 类型 | 变体数 | 定义 |
|---|---|---|
| `Event` | 25 | `logic/src/app.rs:642-685` |
| `Effect` | 35 | `logic/src/app.rs:332-403` |
| `RenderView` | 14 | `logic/src/app.rs:406-465` |
| `Command` | 7 | `logic/src/protocol.rs:11-25` |
| `Screen` | 13 | `logic/src/app.rs:31-75` |

事件优先级硬编码为两档（`logic/src/event_queue.rs:16-45`）：`High`（16 深度 FIFO）与
`Mergeable`（4 深度，`Tick` 就地合并）。这是"新按键立即响应、旧时钟 tick 可丢弃"的策略。

### 4.2 转移

`logic/src/app.rs:969-1033` 按事件类型分派到 34 个 `transition_*`，共同形状是
`&mut AppState` 进、`Vec<EffectBatch>` 出，不做 IO。

三处最值得注意的复杂度：

- **闹钟响铃是多阶段提交状态机**：`AlarmRuntimeState::{Disarmed, Armed, Firing,
  WaitingForRearm, Degraded}`（`app.rs:697-724`），用 `CommitState`（`app.rs:766-782`）
  分别跟踪"ACK 寄存器"与"持久化"两条**独立**出路；任一失败进 `retries` 队列按分钟重试
  （`app.rs:2739-2760`），退避 `RETRY_BACKOFF_MINUTES = 1`（`app.rs:907`），
  重试优先级 `Ack(0) → PersistAlarms(1) → ProgramRtc(2)`（`app.rs:2762-2768`）。
  `maybe_rearm`（`app.rs:2714-2737`）要求 `ack == Succeeded && persistence == Succeeded
  && minute_advanced` 才回到 `Disarmed`——防止"寄存器清了但 NVS 没写，于是每分钟重响"。
- **副作用完成闭环**：`EffectCompleted`/`EffectFailed` 回到
  `transition_effect_completed`（`app.rs:2806`）/`transition_effect_failed`（`app.rs:3068`），
  用 `(batch_id, effect_id, operation_id, render_generation)` 四元组匹配在途操作。
- **同步两阶段**：`SyncState::{Idle, Running, Applying}`（`app.rs:298-307`），
  先 `Effect::ApplySyncedData` 再写 metadata。

### 4.3 双执行器（本项目最核心的不变式）

同一个 `Effect` 枚举由两个 `EffectExecutor` 实现执行：

| 执行器 | 位置 | 在哪跑 | 能做什么 |
|---|---|---|---|
| `EffectRunner` | `rust-firmware/src/app_runner.rs:61-403` | **主线程**（`main.rs:1666`） | 全部 35 个 Effect |
| `TaskExecutor` | `rust-firmware/src/effect_task.rs:45-278` | `effect-task` 线程 | 只有 20 个 NVS/RTC 类 |

分工由 `logic/src/runner.rs:6-32` 的 `batch_is_worker_safe` 静态判定：
**只有整批都是 NVS 写或 RTC 写的批次才下放 worker**。worker 侧兜底在
`effect_task.rs:255-275`，对 Render/Tone/Sync/BLE/Sleep 显式返回
`Err(EffectCategory::Render, "effect requires main-thread-owned resources")`。

**为什么这个分裂是对的**：`Effect::Render` 要 `&mut Canvas`、`StartBlePairing` 要
`&mut BleControl`、`StartSync` 要 `pending_wifi_op`——这些类型是 `!Send` 或独占的。
按"资源归属"切执行器，比给每个资源加锁简单得多，也比让状态机自己判断更不易错。

**代价**：`DeviceContext` 出现 24 个 `pending_*` 字段（`ctx.rs:224-303`），
外加 `PendingDispatchSources`（`ctx.rs:31-186`）的 11 个来源各自闩锁。
详见 `concurrency-and-resources.md`。

## 5. 渲染流水线

```
AppState ──ViewModel::from_state()──▶ ViewModel { generation, view, clock_minute,
   (render_plan.rs:61-105)                       overlay, data_fingerprint }
                                              │
                        RenderRegistry::plan_for()   ← last_shown 缓存
                        (epd_registry.rs:77)
                                              ▼
                             RenderPlan::{Noop | Partial(region) | Full}
                             (render_plan.rs:108-161)
                                              ▼
              Effect::Render(RenderRequest) ──▶ draw_* 写 Canvas
                                              ▼
              display.refresh_full/partial() → 复制整帧 → epd 线程
                                              ▼
              EpdCompletion{request_id, ok, superseded}
                                              ▼
              RenderRegistry::feed() → EffectCompleted/Failed 回状态机
```

- **决策在 logic，像素在固件**。`plan_render` 只比较 `ViewModel` 的 5 个字段，输出 5 种
  `PartialRegion`（Clock/NavBar/List/CalendarGrid/Surface）。区域→像素矩形的映射
  **硬编码在主线程侧**（`app_runner.rs:423-453`）：logic 说"只刷 NavBar"，
  固件知道 NavBar 是 `(16,34,176,266)`。
- **`data_fingerprint` 用 `DefaultHasher`**（`render_plan.rs:62-88`），只对
  AlarmList/TodoList/Inbox/InboxItem 四屏采集数据。`DefaultHasher` 不保证跨 Rust 版本稳定，
  当前只做进程内相等比较（成立，但属隐含假设）。
- **刷新合并**：`RefreshSlot::submit_partial`（`epd_task.rs:109-149`）在刷新执行中再次提交时
  **不排队，而是就地替换 + 矩形取并集**（`union_rect`，`epd_task.rs:344-357`），
  被顶掉的那次以 `superseded: true` 上报。这是对 e-paper 重影/闪烁的正确应对。
- **失败自愈**：局部刷新失败自动降级整屏（`epd_task.rs:304-315`）并置 `recovered: true`。
- **帧传递是拷贝不是共享**：`display.rs:63-69` 把 `Canvas` 整帧 `to_vec()` 后交给 epd 线程，
  刷新线程从不读 `Canvas`。代价是每次刷屏复制 15 KB。

## 6. 传输与协议

两条链路共享同一套协议（`logic/src/protocol.rs`）。完整线上格式见 `control-protocol.md`。

| | USB | BLE |
|---|---|---|
| 收 | `usb-console-rx` 读 stdin，按 `">>IW "` 前缀逐行（`usb_console.rs:7,74`） | NimBLE write 特征 `d2c25e51-…`（`ble_control.rs:25`） |
| 发 | `usb-console-writer` 写 stdout `"<<IW "`（`usb_console.rs:9,199`） | NimBLE notify 特征 `d2c25e52-…`（`ble_control.rs:26`） |
| 上限 | 512 字节/行（`usb_console.rs:10`） | 受 MTU 限制；notify 超时 2 s（`ble_control.rs:758-782`） |
| 幂等 | `CommandSessions` 缓存 8 条终态应答（`command_sessions.rs:5`） | 同左 |
| 会话标识 | 主机插拔 → 会话号自增（`main.rs:480-494`） | connection generation + handle（`main.rs:1310-1412`） |

**幂等/重试链路**是防御性代码最密集处：

- 命令先 `reserve_pending`（`ctx.rs:331-336`）；重复 id 命中缓存直接回放（`main.rs:706-728`）；
  缓存未命中且槽位被占 → `Reply::Busy`。
- 应答投递三重闩锁：`pending_usb_replies`(32) + `pending_usb_reply_latch`(1)
  （`ctx.rs:188-190`）；`pending_ble_replies` + `pending_ble_deliveries` +
  `pending_ble_reply_latch`（`ctx.rs:190`）。重试上限 3（`ctx.rs:191`、`ble_control.rs:17`），
  超限 `cancel_pending` 并回调状态机。
- BLE 侧额外有 `NotifyAttemptMailbox`（`ble_control.rs:104-165`）：用 `attempt_id` +
  `retired_handles[1024]` 位图过滤**迟到的 notify-tx 回调**。
- ⚠️ 应答缓存满时 **FIFO 静默淘汰**（`command_sessions.rs:201-203`），容量 8，
  所以幂等性**不严格**——第 9 条之后重放同一 id 会重新执行命令。见 `review-findings.md` P1-5。

**射频仲裁**：`logic/src/ble_radio.rs` 的 `BleRadioCoordinator` 状态机是**死代码**
（`rust-firmware/src` 零引用）。真实实现是 `ctx.rs:795-1004` + `sync_task.rs:190-206`：
`SuspendForBle` 断开并 **drop 整个 `EspWifi`**（`wifi.rs:166-171`），BLE 结束后重建驱动
（`wifi.rs:174-185`）。

> ⚠️ `wifi.rs:167` 的日志声称"internal heap released"，但 `EspWifi::drop`
> （`esp-idf-svc-0.52.1/src/wifi.rs:1853-1859`）只调 `detach_netif()`，
> **不调 `esp_wifi_stop`/`esp_wifi_deinit`**。释放的是 netif 缓冲，驱动本体仍在。
> 交棒释放的内存可能少于代码的假设。**(未验证——需真机日志确认)**

## 7. 网络同步与持久化

- **单向拉取 + 脏标记上传**：`POST /api/sync`，请求体只带 *locally dirty* 的
  `{alarms:[{id,enabled}], todos:[{id,done}], inbox_read:[]}`（`sync.rs:68-86,201-221`），
  dirty 集合持久化在 NVS（`nvs_blob.rs:52-85`），服务端合并后回权威列表。
  ETag/304 保留但**只读取不发送**（`sync.rs:177` 形参 `_etag` 未使用）——legacy 路径半退役。
- **响应缓冲在 PSRAM**：16 KiB `PsramBuffer`（`sync.rs:19-45`），读满时做 1 字节溢出探测
  （`sync.rs:101-112`），避免在截断数据上做 JSON 解析。
- **校验在 logic**：`validate_sync_response`（`sync_validate.rs:41-95`）做 id 去重、
  时间/日期范围、repeat 合法性，并限制 alarms ≤1024 / todos ≤2048 字节。

### NVS 布局

| namespace | 内容 | 上限 |
|---|---|---|
| `inkwash` | wifi/server/tz/etag/sync 元数据/提醒日期 | 标量 64 或 256 字节 |
| `inkwash_alrm` | `alarms` blob + `dirty` 集合 | 1024 |
| `inkwash_todo` | `todos` blob + `dirty` 集合 | 2048 |
| `inkwash_inbox` | `items` + `pending`（待 ACK 的已读） | 4096（截到 32 条、300 字符/条） |

储存均为 `serde_json` blob（`nvs_blob.rs:5-35`）。
`InboxStore::save`（`inbox.rs:42-69`）写前按字节预算逐条 pop，并**先把 pending-read
合并回 items**，避免本地已读被服务端快照覆盖。

## 8. 电源

三档，全部经 `power_state.rs` 的令牌协议：

| 档 | 触发 | 阻塞因子 | 代码 |
|---|---|---|---|
| 保持唤醒 | 默认 | — | `main.rs:1057` `sleep(20ms)` |
| 浅睡 | `Screen::Home` 空闲 2 s（`main.rs:1047`） | 5 项（`power_state.rs:161-175`） | `wake.rs` + `power.rs:95-133` |
| 深睡 | 设置页手动 | 13 项（`power_state.rs:177-202`） | `power.rs:37-63` |

`SleepState` 用 `(activity_version, request_id)` 令牌 + Prepare/Commit 两段，
`commit` **二次校验**同一组输入（`power_state.rs:136-158`）：任何按键、任何未落盘写入、
任何在途网络操作都让 token 作废。深睡唤醒源是 GPIO0/5/18 低电平（ext1）+ 可选定时器
（`power.rs:47-63`），睡前后 `gpio_hold_en(17)` 维持电源自锁（`power.rs:42`）。

另有一层**IDF 自动浅睡**在 sdkconfig 层生效：`CONFIG_PM_ENABLE=y` +
`CONFIG_FREERTOS_USE_TICKLESS_IDLE=y` + `CONFIG_FREERTOS_IDLE_TIME_BEFORE_SLEEP=200`
（200 tick = 200 ms @ 1000 Hz）。配合主循环空闲时 1 s 的轮询节奏，
`sdkconfig.defaults` 的注释称每秒约 0.8 s 处于真实浅睡。

## 9. 代码规模与知识留存

```
logic/src         18037 行   prod ≈ 6713 / test ≈ 11324   （测试是生产的 1.69 倍）
  app.rs          10420      prod  3753 / test  6667       （1.78 倍）
  harness.rs       1105      prod   510 / test   595       ← prod 部分只服务测试
  runtime.rs        752      prod    66 / test   686
rust-firmware/src 10514 行
  main.rs          2166 / ble_control 1118 / ctx.rs 1060 / screens.rs 730
```

- **Rust 源码注释为零**：`rust-firmware/src/*.rs` 与 `logic/src/*.rs` 的 `//` 计数均为 0
  （logic 里 13 处是测试夹具的 `https://` 字符串）。commit `ba5cd3d` 的记录是
  "remove all source comments"。
- **但配置层文档保留**：`sdkconfig.defaults`（36 行注释）记录了 TWDT 10 s 的由来、
  USJ 浅睡问题、BLE/Wi-Fi 硬件互斥等物理约束；`scripts/build-rust.sh` 注明了
  `espup`/`LIBCLANG_PATH` 的坑。**查"为什么"先看这里，不在 Rust 源码里。**
- **唯一的固件侧约束载体**是 `logic/src/lib.rs:30-289` 的契约测试：用 `include_str!`
  对固件源码做文本 `contains` 断言，守护"栈必须内 RAM""advertising 先于 deinit"
  "BT 角色关闭"等无法在主机运行验证的约束。

构建与 CI 细节见 `verification.md`。
