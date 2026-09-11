# 评审发现与风险清单

> 行号基于 commit `b30c3af`。P0 项已用算术复算确认；标注 **(未验证)** 的项需要真机日志。

severity 定义：
- **P0** — 可导致设备重启（panic → abort → reboot），或使核心功能静默失效
- **P1** — 数据越界 / 陈旧显示 / 幂等性失效等行为缺陷，多数可由服务端数据或 NVS 数据触发
- **P2** — 死代码、跨 crate 漂移、结构性问题

---

## P0-1 · `effect-task` 未订阅看门狗 → 持久化可能静默停摆

**位置**：`rust-firmware/src/effect_task.rs:280-293`（`run` 函数，无 `watchdog::subscribe()`）

**机制**：

```
effect-task 挂起（不 disconnect、不返回、不 panic）
   └─ main.rs:1579  effect_task.try_next_notice() → Ok(None)
        └─ ctx.worker_batch_in_flight 永远保持 true   (main.rs:1612 置位，唯一清除点 main.rs:1601)
             └─ reduce_effect_batches: main.rs:1637  if ctx.worker_batch_in_flight { break }
                  └─ 后续所有 EffectBatch 只进 pending_effect_batches，永不执行
                       └─ 所有 NVS 写入停摆
```

**为什么特别危险**：设备**看起来完全正常**——时钟在走、闹钟照响（RTC 硬件保证）、
界面照刷新（渲染走主线程），只是任何改动都不再落盘，重启后全部丢失。
没有任何超时、计数或恢复路径，也没有日志。

**对比证据**：另外 4 个长驻线程都订阅了 TWDT（`main.rs:89` 主、`rtc_executor.rs:152`、
`sync_task.rs:127`、`audio_task.rs:83`），说明作者知道该订阅，`effect-task` 是遗漏。
而 `sdkconfig.defaults:52-58` 的注释明确写着引入 TWDT 的目的就是
"catch a genuine hang (stuck peripheral, a future infinite loop) and reboot instead of
leaving the device stuck" —— 恰恰是 `effect-task` 这个最需要保护的对象没被覆盖。

**修法**：在 `run()` 开头订阅，并在 `recv()` 阻塞处按 `rtc_executor.rs:172-181` 的模式
加超时喂狗；再加一条 `worker_batch_in_flight` 超时恢复路径（超时后清标志、
把批次退回 `pending_effect_batches` 并记日志）。

---

## P0-2 · 周视图省略号按字节切片 → 含汉字即 panic（已复算确认）

**位置**：`rust-firmware/src/screens.rs:378-389`

```rust
if truncated && line_index + 1 == lines.len() {
    let ellipsis_w = Canvas::text_small_width("...");
    let mut end = line.len();                                   // ← 字节长度
    while end > 0 && Canvas::text_small_width(&line[..end]) + ellipsis_w > text_w {
        end -= 1;                                               // ← 字节递减
    }
    canvas.draw_text_small(text_x, y_cursor, &line[..end]);     // ← 非字符边界 → panic
```

**参数（全部已核对）**：

| 量 | 值 | 来源 |
|---|---|---|
| `text_w` | `50 - (8+2) = 40` | `screens.rs:319` `COL_WIDTH=50`，`:365-366` `TEXT_INSET=8` |
| `ellipsis_w` | `3 × 6 = 18` | `screens.rs:379`；`font5x7.rs:8-11` 恒返回宽 5 → `canvas.rs:118` 用 `width+1` |
| 汉字宽 | `13` | `font_cjk.rs:7` `WIDTH_12 = 13` |

**复算结果**（进入循环的条件是行宽 > 22）：

| 行内容 | 宽度 | 字节 | 结果 |
|---|---|---|---|
| `汉` | 13 | 3 | ✅ 安全 |
| `汉汉` | 26 | 6 | ❌ **PANIC** `&line[..5]`，边界 = {0,3,6} |
| `汉汉汉` | 39 | 9 | ❌ **PANIC** `&line[..8]`，边界 = {0,3,6,9} |
| `汉ab` | 25 | 5 | ❌ **PANIC** `&line[..2]` |
| `你好世界` | 52 | 12 | ❌ **PANIC** `&line[..11]` |

注意 `汉汉汉` 宽 39 ≤ 40，是 `wrap_text_small` 在 40px 内**最自然的换行结果**
（13×3=39，第 4 个字就超宽）。也就是说：**中文待办文本一旦折行超过 3 行，
第 3 行几乎必然触发 panic。**

**触发条件**：进入周视图 → 该日有 ≥4 个折行行（`screens.rs:370-371` 判定
`truncated`）→ 第 3 行含任何多字节字符且宽度 > 22px。

**输入来源**：待办文本经 `/api/sync` 从服务端同步而来 → **远程可触发的重启**。

**修法**：与同文件 `wrap_text_small`（`screens.rs:229-234`）已有的正确写法一致，
改用 `char_indices().next_back()` 回退；或直接复用 `drop_last_char` 式的按字符回退。
另建议补一条"含 CJK 且折行 >3 行"的回归测试。

---

## P1 · 数据与边界

### P1-1 · 无边界数组索引（NVS 越界值可触发 panic）

| 位置 | 代码 | 说明 |
|---|---|---|
| `screens.rs:427` | `WEEKDAY_SHORT[*d as usize]` | `format_alarm_row` 的 `Repeat::Weekly{days}` |
| `screens.rs:602` | 同上 | `format_todo_row`（todo 的 repeat） |
| `screens.rs:409` | `DAYS[(month - 1) as usize]` | `month == 0` 时下溢 panic |

**关键不对称**：经网络进入的数据**已被校验**——`sync_validate.rs:101` 强制
`days` 非空且 `<= 6`，`:117` 强制 `month ∈ 1..=12`。但
**`AlarmStore::load()`（`alarms.rs:29`）与 `TodoStore::load()`（`todos.rs:26`）
不做任何校验**（直接 `read_blob(...).unwrap_or_default()`）。

因此：NVS 中既有的越界值（旧版固件写入、或任何非经 `/api/sync` 写入的路径）
会在启动后**首次绘制列表时 panic**，而网络数据不会。

**修法**：把校验收到"数据进入设备"的唯一入口（`load()`），
让 NVS 数据与网络数据走同一套校验；渲染层改用 `.get(..)` + 钳制。

### P1-2 · `days_in_month` 跨 crate 重复实现，固件版不安全

`logic/src/datetime.rs:50-53` 有 `(month as i64 - 1).clamp(0, 11)`；
`rust-firmware/src/screens.rs:404-411` 是**另写的一份**，用 `DAYS[(month-1)]`，
`month == 0` 时 usize 下溢 panic。

调用点 `screens.rs:114` 的入参是 RTC 读回的月份（`rtc.rs:70` 的 `bcd_to_bin(buf[5] & 0x1F)`
在寄存器异常时可为 0）。logic 版有 Zeller 交叉验证测试（`datetime.rs:128-141`），
固件版**无任何校验**。

### P1-3 · 充电图标的三路分支是死区分

`icons.rs:72-97` 的 `CHARGING_LOW` / `CHARGING_MEDIUM` / `CHARGING_HIGH`
三个 18 行位图**逐字节相同**（已比对确认）。`home.rs:36-42` 按
`percent < 34` / `< 67` 三路选择同一张图 → 分支无视觉意义。

### P1-4 · 应答缓存 FIFO 淘汰使幂等性不严格

`command_sessions.rs:201-203`：缓存满（`DEFAULT_CACHE_CAPACITY = 8`，`:5`）时
**静默淘汰最旧的终态应答**。因此第 9 条命令之后重放同一 `id` 会**重新执行命令**。
调用方（`main.rs:710`）无法区分"从未执行"与"已执行但已被淘汰"。
该行为仅由 `command_sessions.rs:426-441` 的测试描述。

对 USB 重连场景影响有限（会话号会自增使旧缓存失效），但对 BLE
"上位机超时重发"是真实的语义缺口。

### P1-5 · 详情页越界不清屏 → 陈旧画面

`screens.rs:631-634`：`draw_inbox_item_detail` 在 `items.get(selected)` 为 `None` 时
**直接 `return`，不调用 `canvas.clear()`**；调用点 `app_runner.rs:488-490` 也不清屏
→ 面板保留上一帧内容，不进入明确状态。

### P1-6 · Inbox 正文可见性不足

`inbox.rs:15` `MAX_BODY_CHARS = 300`（按**字符**）与
`screens.rs:657-663` 的显示上限（最多 11 行，`break if y + 16 > 282`）不匹配：

- 300 个汉字 ≈ 15 行（368px / 17px ≈ 21 字/行）→ **第 12 行起永久不可见**
- 300 个 ASCII ≈ 9 行 → 可见

即中文正文尾部静默丢失，且无滚动、无"更多"提示。

### P1-7 · 数值适配失败后无裁剪

`home.rs:169-174` `fit_scale` 从 scale 3 递减，无解时 `unwrap_or(1)`。
`home.rs:136` 的待办计数无上限，超长数值以 scale 1 直接溢出卡片
（值列 `home.rs:106` 为 x 32..184）并压到右侧卡片（x 208 起）。
`Canvas::set_pixel` 只在 x ≥ 400 处裁切（`canvas.rs:37-39`）。

### P1-8 · `home.rs:63` 的无校验 usize 减法

`7 + battery_rows - WIFI_rows`：靠"所有图标恰好 18 行"成立。
新增一个矮图标即在 debug 构建下 panic（release 下回绕成巨大值）。

### P1-9 · 日历标记被最后一条待办覆盖

`screens.rs:126` `marks[day as usize].todo = Some(todo.importance)` 是覆盖式赋值，
同日多条待办时**最后一条胜出**，而非取最高重要度 →
`screens.rs:190-192` 的 6px/4px 标记不反映真实优先级。

---

## P2 · 死代码与漂移

### P2-1 · 整模块死代码

| 模块 | 规模 | 证据 |
|---|---|---|
| `logic/src/ble_radio.rs` | 138 行（prod 104） | 除 `lib.rs:7` 的 `pub mod` 外全仓库零引用；`BleRadioCoordinator` 只在自身测试出现。真实射频仲裁在 `ctx.rs` + `sync_task.rs` |
| `logic/src/harness.rs` 生产部分 | 510 行 | `lib.rs:12` 无条件 `pub mod harness;` → 编进固件依赖的 lib；唯一消费者是 `runtime.rs` 的测试（`runtime.rs:345,425,451,458,475,503,607,635,644,654,716`）。**建议改为 `#[cfg(test)]`** |
| `power.rs:66-76` `restart_via_deep_sleep` | 11 行 | `#[allow(dead_code)]`，唯一调用者 `wifi.rs:194-201 restart_for_fresh_wifi_session` 本身也无人调用 |

### P2-2 · 死字段（重构残留）

| 字段 | 声明 | 状态 |
|---|---|---|
| `pending_ble_pairing_success` | `ctx.rs:260` | 仅 `main.rs:512, 1399` 清除，**无处赋值** |
| `pending_render_completion` | `ctx.rs:272` | 仅 `main.rs:811` 读取（恒 `None`，分支从不进入） |
| `pending_residue_time` | `logic/src/app.rs:823` | 仅 `:1754` 赋值，无读取 |

三者都是 `29755a8 refactor: land single-event-loop firmware architecture` 的残留：
**重构删掉了生产者，留下了消费者。**

### P2-3 · 死 UI 代码

- `ui.rs:12` `pub fn footer(_canvas, _hint) {}` 是**空函数**，11 处调用
  （`screens.rs:89,100,133,466,482,510,522,555,576,628,664`）传入的提示串全是死字符串
  → 占用 flash 并误导维护者（每屏底部其实是空白）。
- `screens.rs:504` 的 `truncate_prop(line, 300)` 被 `reminders.rs:25` 的
  `truncate_prop(&item.title, 330)` 先行截断覆盖 → 330 是死截断。

### P2-4 · 跨 crate 重复实现（会漂移）

| 重复项 | 位置 A | 位置 B | 风险 |
|---|---|---|---|
| `days_in_month` | `logic/src/datetime.rs:50`（有 clamp） | `screens.rs:404`（无 clamp） | 见 P1-2 |
| 星期算法 | `logic/src/datetime.rs:94`（有 Zeller 测试） | `screens.rs:413`（无测试） | 静默不一致 |
| 存储上限 1024/2048 | `sync_validate.rs:83,90` | `alarms.rs:11`、`todos.rs:13` | 无共享常量、无编译期关联，且 `sync_validate` 的测试**未覆盖**这两个上限 |
| 导航项顺序 | `screens.rs:46` `NAV_DESTINATIONS` | `logic/src/app.rs:1978-2012` 分派 | 两处独立字面量，无编译期绑定 |
| 同步间隔选项 | `screens.rs:92` `SYNC_INTERVAL_OPTIONS` | `logic/src/app.rs:2200` `SYNC_INTERVAL_MINUTES` | 同上（目前同序） |
| 行宽度公式 | `canvas.rs:124-134` | `screens.rs:20-24`（`truncate_prop` 内联） | 三处各写一遍 |
| 折行函数 | `screens.rs:206-247` `wrap_text_small` | `screens.rs:249-290` `wrap_text_prop` | 42 行 ×2，仅宽度函数不同 |
| Home 顶栏 | `ui.rs:4-6,9` | `home.rs:28-30,83` | 逐字重复，Home 未调用 `header()` |

### P2-5 · 仓库级元数据已失效

| 项 | 问题 |
|---|---|
| `.esp32-review.yml:27-28` | 记录 `Peripherals::steal()` 许可行号为 `board.rs:416` / `wifi.rs:94`，**实际在 `board.rs:276` / `wifi.rs:44`** → 豁免机制在误报与漏报之间摇摆 |
| `.esp32-review.yml:34-37` | 记录的另两条已不成立：`tasks.rs` 已无 `CONFIG_SPIRAM_ALLOW_STACK_EXTERNAL_MEMORY` 注释（`ba5cd3d` 删除）；`usb_console.rs:28,138` 现在**都**设了栈大小 |
| `README.md:9,68,72,88,123` | 指向 `docs/development-guide.md`、`docs/control-protocol.md`、`docs/screenshots/*`，但 `docs/` 已被 `b30c3af` 删除 → 首页链接 404 |
| `scripts/build-rust.sh:57` | 注释写 "see docs."，指向已删除的文档 |
| `scripts/build-rust.ps1:14` | 注释写 "Windows side remains unverified on a real toolchain - see docs."，同样悬空 |
| `sdkconfig.defaults:18-19` | `CONFIG_PARTITION_TABLE_CUSTOM_FILENAME` 展开成七层 `../`，从构建目录反推；迁移构建目录即失效 |

### P2-6 · 结构性问题

- **超长文件**：`logic/src/app.rs` 10420 行、`rust-firmware/src/main.rs` 2166 行。
  `main.rs:400-1059` 的 660 行循环体里嵌了 BLE 会话状态机（`498-809`）、
  EPD 完成处理（`811-915`）、睡眠 kick（`941-993`）。
- **同名类型冲突**：`event_queue.rs:10` 的 `Priority{High,Mergeable}` 与
  `inbox_item.rs:21` 的 `Priority{Normal,High}` 语义不同，无法同时 `use`。
- **冗余分支**：`logic/src/render_plan.rs:96-101` 的 `Full{..}` 与 `Partial{..}`
  臂体逐字相同；`epd_registry.rs:95-103` 同理。
- **`DefaultHasher` 作为指纹**：`render_plan.rs:62-88` 的 `data_fingerprint`
  不保证跨 Rust 版本稳定（当前只做进程内比较，成立但属隐含假设），
  且只对 4 个屏幕采集数据——其余屏幕的数据变化不触发局刷。
- **`deep_blocker` 的布尔开关**：`power_state.rs:177` 的 `require_wake_plan: bool`
  改变语义（`prepare` 传 `false` / `commit`+`final_check` 传 `true`），
  签名上看不出差别。
- **每帧堆分配**：`screens.rs:87,94-98,462,573,621` 每帧重建整表 `Vec<String>`；
  `truncate_prop` 每个字符触发一次 `font_cjk::cell_index` 二分查找，
  绘制时再查一次（同字符每帧 ≥2 次）。

---

## 未经真机验证的疑点

### 疑点 1 · `EspWifi` drop 是否真的释放了 Wi-Fi 内存

`wifi.rs:166-171` 的日志声称 `"Wi-Fi driver dropped for BLE; internal heap released"`，
`sdkconfig.defaults:74` 的注释也称 NimBLE "consumes ~150KB RAM" 需靠交棒腾出。

但 `esp-idf-svc-0.52.1/src/wifi.rs:1853-1859` 的 `Drop for EspWifi`
**只调 `self.detach_netif()`，不调 `esp_wifi_stop` / `esp_wifi_deinit`**：

```rust
impl Drop for EspWifi<'_> {
    fn drop(&mut self) {
        self.detach_netif().unwrap();
        ::log::info!("EspWifi dropped");
    }
}
```

释放的是 netif 缓冲，**Wi-Fi 驱动本体与其收发缓冲（通常 30–40 KB）仍在**。
推论：交棒释放的内存可能远少于代码假设；`resume_after_ble` 用 `EspWifi::new` 重建
（`wifi.rs:178`）构成同进程内二次 init。

`logic/src/lib.rs:265-269` 的契约测试只断言了**代码形状**
（`WIFI.contains("wifi: Option<EspWifi<'static>>")` 等），**未断言行为**。

**建议**：抓一次真机串口日志，对比交棒前后的
`heap_caps_get_free_size(MALLOC_CAP_INTERNAL)`，而不是继续依据代码推断。

### 疑点 2 · NimBLE 回调的上下文

`ble_control.rs:174-198`（notify-tx）持 `std::sync::Mutex`；
`ble_control.rs:205-214`（lifecycle）用 `Condvar` **阻塞等待**。
若这些回调运行在真正的 ISR 上下文，两者都非法；若 NimBLE 派发到 host task
（`CONFIG_BT_NIMBLE_HOST_TASK_STACK_SIZE=5120`）则正确。需确认框架行为。

### 疑点 3 · `CONFIG_BT_CTRL_BLE_MAX_ACT=2` 与硬件互斥的关系

`sdkconfig.defaults:63-70` 的注释称 "only one can run at a time (Bluetooth SoC limitation)"，
但代码中并未见显式的"Wi-Fi 未运行时才启 BLE"断言（只有 `ctx.rs:799-804` 的
`pending_wifi_op.is_some() || ble_wifi_suspended` 守卫）。需确认是否所有路径都被覆盖。

---

## 修复优先级建议

| 优先级 | 项 | 理由 |
|---|---|---|
| 1 | P0-1 `effect-task` 订阅看门狗 | 一行代码，消除"静默数据丢失"这一最坏失效模式 |
| 2 | P0-2 `screens.rs:381` 字节切片 | 远程可触发重启，修法在同文件已有正确范例 |
| 3 | P1-1/P1-2 边界校验收口 | 让 NVS 数据与网络数据走同一套校验 |
| 4 | P2-5 重置 `.esp32-review.yml` + 补 README 链接 | 恢复工具与文档的可信度 |
| 5 | P2-1 删死代码 | `ble_radio.rs`、`harness.rs` 生产部分、3 个死字段、`footer` |
| 6 | P2-6 拆 `main.rs` 循环体为 reducer 函数 | 唯一能在不改语义前提下降复杂度的手段 |
| 7 | 审视 `logic/src/app.rs` 拆分 | 需先把 30+ 私有字段收敛成 3–4 个内聚子结构，风险较高 |
