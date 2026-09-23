# 修复记录（第四 ~ 六轮）

> 本文件记录 `review-findings.md` 中每一条缺陷的处置状态。
> **行号以修复后的工作区为准**；未修复项给出原因与前置条件。
>
> 基线：`logic` 测试 **386 → 413 passed / 0 failed**（本机 LF 检出），
> `rust-firmware` 以 `cargo check --target xtensa-esp32s3-espidf` + `cargo fmt --check`
> 验证，**0 warning**。所有改动不含 Rust 源码注释（遵守 `ba5cd3d` 的仓库约定）。
>
> **第六轮（遗留墙钟超时收口 + 真机验证）**：提醒与响铃两处 deadline 也改为单调时钟，
> `logic` 中不再存在墙钟锚定的超时判定；测试 **412 → 413**。
> 本轮修复固件已**烧入真机并验证**（`espflash` 按 README 红线：
> DIO / 16mb / 80mhz；app 62.98%），新增可复现的 `scripts/smoke-note4.py`（10/10 passed）。
> 真机结果与新增的 **P0-5（HTTPS/TLS 必然失败，A/B 证实与本次修复无关）**、
> **P0-6（命令压力下间歇性硬崩溃，落点在本轮未改动的 `command_sessions.rs`；
> 已排除 `effect_task` 看门狗改动，但因基线 6 次全 0 而含本轮改动的构建出现 4 次事件，
> 归因未定、不能排除本轮改动影响触发概率）** 见 `verification.md` §7 与
> `review-findings.md` P0-5 / P0-6。
>
> ⚠️ **发布判断**：P0-6 定性之前不建议把本轮改动视为可发布。
>
> **第六轮下半场：P0-5 的堆取证**（`verification.md` §7.6）。加了临时插桩
> `rust-firmware/src/heap_probe.rs`（`HEAPPROBE` 行：内部/DMA/PSRAM 的空闲量与
> **最大连续块**，关联 `uptime_ms` 与命令计数）。实测 **70/70 次同步尝试全部
> `mbedtls_ssl_setup` 失败**（run1 15 + run2 27 + run3 28），失败时 `int_largest` 仅 7.7–12.3 KB，而
> `psram_free` ≈ 8.37 MB 未用；生成配置里 `MBEDTLS_DYNAMIC_BUFFER` 与
> `MBEDTLS_EXTERNAL_MEM_ALLOC` 均未开。
> **第 ① 步已确认失败分配的实际尺寸与 caps**（从烧入 ELF 反汇编 `mbedtls_ssl_setup`）：
> 失败的是第 1 次分配 `ssl->in_buf = mbedtls_calloc(1, 16717)`（常量 `0x414D`），
> caps 为 `MALLOC_CAP_INTERNAL|MALLOC_CAP_8BIT`（`CONFIG_MBEDTLS_INTERNAL_MEM_ALLOC=y`），
> **按定义不允许 PSRAM**；失败瞬间 `int_free` 24.6–28.7 KB（够）而
> **`int_largest` 仅 7.7–12.3 KB（不够）**。即"没有足够大的**连续**内部块"，
> 不是"内存不足"。**这确认的是失败的直接机制，不是修法**；未改配置、未做实验。
> P0-6 本轮 run1 出现 1 次 `rst:0x8 (TG1WDT_SYS_RST)`（第三种签名，落点仍是异常保存
> 例程，不能指向破坏源），run2/run3 无复位；**本轮未启用堆毒化/栈检测**，按约定留待决策。
>
> **对照实验 ②（仅 `CONFIG_MBEDTLS_DYNAMIC_BUFFER=y`，`verification.md` §7.7）**：
> 单变量，生成 sdkconfig 差异仅 3 行。`mbedtls_ssl_setup` 失败从 **70/70 → 0/45**，
> `Sync fetched` 从 0 → **17**，复位 0 次。**但根因未消除**：失败点移到 handshake，
> 尺寸降到 ~4,770 B，在 `int_largest` = 4,608 时仍失败（21/45）——**是缓解不是修法**。
> 且 17 次 `Sync fetched` 全为空列表，**"非空数据被应用"未证明**。
> 该选项在实验③前已**移除**（回到原始基线）。
>
> **实验③（仅 `CONFIG_MBEDTLS_EXTERNAL_MEM_ALLOC=y`，`verification.md` §7.9）**：
> 从原始基线出发的单变量对照，caps 由 `0x804`(INTERNAL) 变为 `0x404`(SPIRAM)。
> `mbedtls_ssl_setup` 失败 0、`request-ok` **40/40**、**分配失败 0**、`Sync fetched` 29
> —— 分配维度上最干净。**但 P0-6 仍发生**（`Guru (LoadProhibited)`，
> 落点 `ctx::DeviceContext::poll_alarm_snapshot`，`EXCVADDR=0x0c`，第三个不同落点，
> 且 `ctx.rs` 未被改动）。**TLS 全成功时崩溃照样出现。**
>
> **发布继续阻断；P0-6 独立追踪。** 工作区当前为实验③ 配置（未验收）。
> 原始日志 + ELF + 源码快照在 `logs/hw-forensics/`（`.gitignore` 忽略，需另存请拷贝）。
>
> **第五轮（评审独立复现后的三项调整）**：
> 1. **Home 指纹变化仍可能漏刷** —— 分钟与数据同时变化时 `plan_render` 仍优先返回
>    `PartialRegion::Clock`，刷完时钟后缓存了新指纹，下一次 `Noop`，图标停在旧状态。
>    已改为仅当指纹也未变时才用时钟局刷。见 P1-10。
> 2. **配对超时被时区调整提前触发** —— deadline 原先锚在可修改的 RTC 墙钟上。
>    已改为锚定单调 `PowerPoll.now_ticks`。见 P1-11。
> 3. **文档把 `#[test]` 函数与被跳过的范围写宽了** —— 被跳过的只有 `#[test]`
>    函数体；`#[cfg(test)]` 模块内的普通函数/方法/类型**仍会被类型检查**。
>    已收窄 `verification.md` §6、本文件与契约测试里的表述。见"验证基建"。

## 状态总览

| 编号 | 缺陷 | 状态 |
|---|---|---|
| P0-1 | `effect-task` 未订阅看门狗 → 持久化静默停摆 | ✅ **已修** |
| P0-2 | `screens.rs` 省略号按字节切片 → 中文折行 panic | ✅ **已修** |
| P0-3 | 响铃期 `clear_alarms` → 永久卡死 + 铃声不停 | ✅ **已修**（+ 兜底出路） |
| P0-4 | BLE 重连后应答永久失效 | ⚠️ **未修改代码**，见下（需单独立项） |
| P1-1 | 无边界数组索引（NVS 越界值 panic） | ✅ **已修**（+ load 收口） |
| P1-2 | `days_in_month` / `weekday_of` 跨 crate 重复且不安全 | ✅ **已修** |
| P1-3 | 充电图标三路分支是死区分 | ❌ 未修（低风险，flash 冗余） |
| P1-4 | 应答缓存 FIFO 静默淘汰使幂等不严格 | ❌ 未修（语义取舍，见下） |
| P1-5 | 详情页越界不清屏 → 陈旧画面 | ✅ **已修** |
| P1-6 | Inbox 正文超 11 行静默不可见 | ❌ 未修（需滚动/摘要设计） |
| P1-7 | `fit_scale` 失败后无裁剪 | ❌ 未修 |
| P1-8 | `home.rs` 无校验 usize 减法 | ✅ **已修** |
| P1-9 | 日历标记被最后一条待办覆盖 | ✅ **已修**（改取最高重要度） |
| P1-10 | 渲染指纹漏采字段与屏幕 | ✅ **已修**（含新发现的 Home 缺口 + 第五轮补修时钟局刷） |
| P1-11 | 配对超时是死代码，配对页无超时 | ✅ **已修**（第五轮改为单调时钟 deadline） |
| P1-12 | 日历无法翻月 | ❌ 未修（产品取舍待定） |
| P1-13 | 安全模式 `split_at` 可 panic | ✅ **已修** |
| P2-1/2/3 | 死代码、死字段、死 UI | ❌ 未动（受契约测试耦合，见下） |
| P2-4 | 跨 crate 重复实现 | 🟡 **部分**：日期/星期已收口 |
| P2-5 | 仓库级元数据失效 | 🟡 **部分**：`.esp32-review.yml`、README 链接、脚本注释已修 |
| P2-6 | 结构性问题 | ❌ 未动（需独立重构） |
| P2-7 | 死路径与边界失配 | ❌ 未动 |
| 验证基建 | 固件测试从不运行 + 零门禁 | ✅ **已修**（结论已两轮更正：`--all-targets` 无效 + 范围收窄到 `#[test]`） |

---

## 已修项明细

### P0-1 · `effect-task` 订阅看门狗

`rust-firmware/src/effect_task.rs`：`run()` 开头 `crate::watchdog::subscribe()`，
把无限 `recv()` 改为 `recv_timeout(1 s)`；空闲超时喂狗，**每批执行前**也喂一次
（否则背靠背批次会饿死喂狗点）。挂起 10 s 触发 TWDT panic → 重启。

契约测试：`logic/src/lib.rs::effect_worker_subscribes_to_the_task_watchdog`
断言订阅点、喂狗点与 `recv_timeout` 同时存在（订阅而不喂狗等于制造重启循环）。

### P0-2 · 省略号改为按字符回退

`rust-firmware/src/screens.rs` 的周视图省略号裁剪：`end -= 1`（字节）改为
`char_indices().next_back()`（字符边界），与同文件 `wrap_text_small` 的既有正确写法一致。

### P0-3 · 响铃期 `ClearAlarms` 出路

两处改动，**互为纵深防御**：

1. `logic/src/app.rs` 的 `ClearAlarms` 分支：若当前是 `Firing`，先恢复
   `screen_before_ring` 并追加 `Effect::StopTone`，再清列表 / `Disarmed` / 持久化 / 关 RTC。
2. `transition_button`：把 `AlarmRinging` 的处置提到最前面——`Firing` 时保持原语义
   （仅 ENTER 消音），**非 `Firing` 时任意按键走 `leave_stranded_ringing()`**
   （停铃 + 回到 `screen_before_ring`）。

第 2 条是文档建议的"阻断页必须有**无条件**出路"，但做了收窄：只在 runtime 已不在
`Firing`（即已被推离正常轨道）时对任意按键生效，不改变"响铃中需按 ENTER 消音"的既有 UX。

新增 3 个测试：`clear_alarms_while_ringing_silences_tone_and_restores_screen`、
`clear_alarms_while_ringing_leaves_no_dead_end_for_buttons`、
`any_button_escapes_a_stranded_alarm_ringing_page`。

### P1-1 / P1-2 · 边界校验收口到"数据进入设备"的唯一入口

- `screens.rs` 的裸索引全部替换为安全访问：`month_name()` / `weekday_short()`
  用 `.get(..).copied().unwrap_or("???")`。注意 RTC 的 `weekday` 是 `buf[4] & 0x07`
  → 实际值域 **0..=7**，而 `WEEKDAY_SHORT` 只有 7 项，**`weekday == 7` 是真实可达的 panic 路径**。
- 删除 `screens.rs` 重复的 `days_in_month` / `weekday_of`，改用
  `inkwash_logic::datetime::{days_in_month, weekday_of}`。
- 为此在 `logic/src/datetime.rs` 新增 `pub fn weekday_of(year, month, day) -> u8`
  （文档第二轮正确地指出：`weekday_from_days` 是 `pub(crate)` 且接收 epoch 天数，
  **不存在"直接替换"的路径**，所以补了这个包装）。
- 新增 `logic/src/sanitize.rs`：`sanitize_alarms` / `sanitize_todos`，
  在 `AlarmStore::load()` / `TodoStore::load()` 里调用，让 **NVS 数据与网络数据走同一套边界**。

新增 11 个测试（`datetime` 3、`sanitize` 8）。

### P1-5 / P1-8 / P1-9

- P1-5：`draw_inbox_item_detail` 在 `items.get(selected)` 为 `None` 时先清屏并画页头，
  不再把上一帧留在面板上。
- P1-8：`home.rs` 的 `7 + battery_rows - WIFI_rows` 改为 `saturating_add`/`saturating_sub`。
- P1-9：`marks[day].todo` 由覆盖赋值改为**保留最高重要度**（为此给 `Importance`
  补 `PartialOrd, Ord`，枚举顺序 Low < Medium < High 即语义顺序）。

### P1-10 · 渲染指纹收口（并修正一处遗漏）

指纹从"按屏幕挑字段"改为**对设备持有的全部渲染数据求哈希**：
alarms（id/hour/minute/enabled/label/repeat）、todos（text/done/importance/due_date/repeat）、
inbox（id/kind/priority/title/**body**/when/read）、`wifi_configured`。
为此给 `Repeat`、`Importance`、`TodoDue`、`InboxKind`、`Priority` 补 `Hash`。

这么改的理由：逐字段列举**已经漏过两轮**，而"画什么"与"比什么"在结构上仍然脱钩。
全量哈希只会**多刷**（任何数据变化都使任何页面失效），不会**漏刷**；数据变化来自
sync 或本地编辑，频率很低，代价可接受。要重新拿到精细化，正确做法是让 logic
直接哈希渲染层输出，而不是再手列一遍字段。

> ⚠️ **新增发现（原评审遗漏）**：`Screen::Home` 也在 `_ => {}` 分支里、指纹恒为 0，
> 而 Home 明确渲染待办计数、未读角标、下一条闹钟与 Wi-Fi 标志。
> 原文档只列了 `Calendar` / `WeekView`。**Home 是用户停留最久的页面**，
> 这条缺口的用户可见度比那两个都高。现已由全量哈希覆盖。

> 🔧 **第五轮补修（评审复现）**：光修指纹还不够。`plan_render` 的 Home 分支
> **先**判断分钟变化并返回 `PartialRegion::Clock`，所以"分钟 + 数据同时变化"时
> 只刷时钟区域；随后注册表缓存了**新**指纹，下一次直接 `Noop`，图标永久停在旧状态。
> 已改为**仅当 `data_fingerprint` 也未变时**才使用时钟局刷，否则整屏刷新。
> 新增测试 `home_data_change_with_a_minute_tick_must_not_be_clock_only`。

新增 6 个测试，覆盖 body / importance / due_date / Calendar / WeekView / Home / wifi / label / repeat / 分钟+数据同时变化。

### P1-11 · 配对超时赋真实 deadline（第五轮改为单调时钟）

初版修法是赋一个基于**可修改的 RTC 墙钟**的 deadline
（`pairing_deadline_unix = now.to_unix() + 120`）。评审复现出两个后果：
调快时区会让超时**提前触发**（实际并未经过 120 秒），调慢则**延长**。

第五轮改为锚定**单调时钟**：

- `BlePairingState.pairing_deadline_unix` → **`pairing_deadline_ticks`**（毫秒）。
- 进入配对时用 `state.last_power_poll_ticks + BLE_PAIRING_TIMEOUT_MS`；
  新字段 `AppState.last_power_poll_ticks` 在每次 `PowerPoll` 时更新。
- 超时判定从 `transition_tick`（墙钟）**移到** `expire_ble_pairing()`，
  由 `transition_power_poll`（携带 `now_ticks`）驱动。
  `main.rs` 的 ticks 来自 `Instant::now().duration_since(power_ticks_origin)`，
  时区 / RTC 校时都不会影响它；`app_runner_enabled` 恒为 `true`，
  所以 `PowerPoll` 每轮主循环都会到达。

附带收益：配对超时不再依赖 RTC 时间是否可用。

新增/改写测试：
`entering_ble_pairing_arms_a_real_deadline`、
`ble_pairing_times_out_on_monotonic_ticks_not_the_wall_clock`、
`timezone_change_during_pairing_does_not_shift_the_timeout`（完整走
`SetTimezone` → `WriteRtcTime` 完成 → 墙钟跳 1 小时 → `Tick` 的真实路径）。

**第六轮：同类问题已全部收口。** 另两处墙钟 deadline 也改为单调时钟：

| 项 | 原字段 | 现字段 | 超时 |
|---|---|---|---|
| 提醒 | `ReminderState.deadline_unix` | `deadline_ticks` | `REMINDER_TIMEOUT_MS = 120_000` |
| 响铃自动消音 | `Firing.ring_deadline_unix` | `ring_deadline_ticks` | `RING_AUTO_SILENCE_MS = 300_000` |

置位改用 `state.last_power_poll_ticks + <常量>`；两处判定从 `transition_tick`
（墙钟）移出，与配对超时一起由 `transition_power_poll` 依次驱动
（`expire_ble_pairing` → `expire_ring_auto_silence` → `expire_reminder`）。
`logic/src/app.rs` 里**已不存在** `now.to_unix() >= deadline` 形式的墙钟超时判定。

附带收益：提醒超时不再依赖 RTC 时间是否可用（原实现要在 `clock.now` 为 `None` 时
退化成 epoch+120 的兜底值，那会让首个 Tick 立刻到期）。

行为变化：**"响铃期间调整时区"不再导致提前消音或永不消音**——这是原先
`docs/review-findings.md` 未列出、但与前两条同源的真实缺陷。

新增/改写测试：
`firing_alarm_wall_clock_jump_does_not_silence_the_ring`（墙钟跳 11 小时仍响）、
`firing_alarm_timeout_dismisses_like_enter`（tick 到 300 s 才消音，恰好一次）、
`reminder_wall_clock_jump_does_not_dismiss_it`（墙钟跳 12 小时仍显示）、
`reminder_deadline_dismisses_on_the_monotonic_clock`。

### P1-13 · 安全模式切片

`main.rs` 的 `reason.split_at(40)` 改为 `chars()` 游标取前 40 / 后 40 个字符，
长度判断也从 `reason.len()`（字节）改为 `chars().count()`。

### 验证基建 · 固件测试与门禁

见 `verification.md` §6（含可复现证据）。要点：

- `rust-firmware/Cargo.toml` 的 `harness = false` 使**所有**固件 `#[test]` 函数
  既不运行、**其函数体也不被类型检查**（`cargo check --all-targets` 传 `--cfg test`
  但不传 `--test`）。范围限定：`#[cfg(test)]` 模块里的**普通**函数/方法/类型
  仍会被正常检查，被跳过的只有 `#[test]` 函数体——第五轮已更正此处的表述。
  → 原文档"`--all-targets` 能抓到那 4 个测试"**已被证伪**。
- 固件侧真实测试覆盖是 **0 个模块**（原文档先称 3、后称 2）——因为该 crate 里
  **没有任何 `#[test]` 函数会被执行**。注意这不等价于"`#[cfg(test)]` 模块没被编译"：
  模块内的普通代码仍进类型检查，被跳过的只有 `#[test]` 函数体。
- 处置：删除固件侧全部 `#[cfg(test)]` 模块；把 `Continue` / `AbortBatch` 的语义断言
  移入 `logic/src/runner.rs` 真实运行；新增契约测试
  `firmware_keeps_no_unrunnable_test_modules` 防止假覆盖回潮。
- CI 新增 `firmware-format` job：`cargo +stable fmt --check`
  （`+stable` 用于绕开 `rust-toolchain.toml` 里 CI 装不上的 `esp` 工具链；不需要 ESP-IDF）。
- 新增 `.gitattributes`（`*.rs text eol=lf`），修掉 CRLF 检出下契约测试必红的问题。

---

## 未修项与理由

### P0-4 · BLE 重连后应答永久失效 —— 必须单独立项

**没有修改任何代码。** 复核确认了第二/三轮的结论：`on_notify_tx` 回调**只携带
`conn_handle`**（`ble_control.rs` 的 `take_for_callback`），generation 与 attempt_id
是邮箱"猜"出来的。因此：

- 直接清退休位 → 旧连接的迟到回调会取走**新**请求的 attempt（误配）；
- 改用 `attempt_id` 集合 → 回调里根本没有 `attempt_id` 可比对。

`retired_handles` 位图是当前**唯一**的"该 handle 上可能还有迟到回调"记账，**机制不可删**。
根治只有两条路（都需要设计 + 真机验证）：

1. 建立**确定性的旧回调排空边界**（`release_notify_generation` 只排空了已进入邮箱的事件，
   无法证明 NimBLE 内部没有未回调的）；
2. **换掉 correlation 载体**：不依赖 `on_notify_tx` 做身份判定，改成"发一条、等一条、
   自带超时"的严格串行模型（现有 `inflight` + 2 s 超时已接近这个形状），
   把位图降级为"当前 handle 是否可用"的单值状态。

路线 2 的代价是：重连后可能出现**单条应答丢失**（被旧回调误判为已送达），
但协议本身支持同 `id` 幂等重放，客户端重试即可恢复——比"整个会话永久哑掉"好得多。
这属于**行为契约变更**，需要真机验证，不应在无硬件的情况下顺手改。

**未验证前提**：整个 P0-4 依赖"NimBLE 会复用 `conn_handle`"。
需真机抓一次断开-重连日志比对两次 `conn_handle`。**如果 handle 不复用，这条不成立。**

### P1-3 · 充电图标三张位图相同

低风险；改动收益是 flash 而非正确性。删除两张位图需同步改 `home.rs` 的三路选择。

### P1-4 · 应答缓存 FIFO 静默淘汰

这是**语义取舍**而非纯 bug：容量 8 的缓存满了必然要淘汰。
"重放会重新执行"对 USB（会话号自增使旧缓存失效）影响有限，对 BLE 超时重发是真实缺口。
可选修法：把 `DEFAULT_CACHE_CAPACITY` 调大、或淘汰时回 `busy` 而不是静默丢。
需要先决定期望语义。

### P1-6 / P1-7 / P1-12

- P1-6：需要滚动或"更多"提示的设计，涉及渲染布局。
- P1-7：`fit_scale` 无解时的兜底需要裁剪策略（截断还是缩到更小）。
- P1-12：日历翻月是**产品功能取舍**，文档也承认"不确定是刻意取舍还是遗漏"。

### P2-1 / P2-2 / P2-3 · 死代码

**受契约测试耦合**：`logic/src/lib.rs` 断言
`MAIN_SOURCE.contains("pending_render_completion.is_some()")` 与
`BLE_SOURCE.contains("retired_handles: [u64; 1024]")`。
删除死字段/死位图必须同步改契约测试。
其中 `retired_handles` 与 P0-4 是同一处，应在 P0-4 立项时一起处理。

### P2-6 / P2-7 · 结构性与边界项

`main.rs` 660 行循环体拆分、`app.rs` 拆分、`Mergeable` 背压不可达、
Wi-Fi 密码缓冲差一字节、`battery_percent_from_mv` 的 i32 理论上溢、`mark_read` 非原子
—— 均未改动。`Mergeable` 与 `mark_read` 两条值得单独排期。

---

## 遗留的验证缺口（未因本次修复而消失）

| 项 | 状态 |
|---|---|
| P0-4 的"NimBLE 复用 handle" | ❌ 需真机日志 |
| `EspWifi::drop` 是否真释放内存 | ❌ 需真机 `heap_caps_get_free_size` 对比 |
| NimBLE 回调上下文（ISR vs host task） | ❌ 未验证 |
| 功耗/时序数值 | ❌ 未经真机测量 |
| 本次全部修改的真机行为 | ⚠️ **部分已上机**（第六轮）：启动、控制协议、NVS 跨复位持久化、压力/soak 均通过；P0-2 / P0-3 / BLE 仍无真机证据（被 P0-5 阻断） |
| **P0-5 HTTPS/TLS `ALLOC_FAILED`** | ✅ **真机实测 100% 复现**，A/B 证实为既存缺陷（基线 `0f493d7` 同样失败） |
