# 评审发现与风险清单

> 行号基于 commit `b30c3af`（`11e469c` 只新增了本目录，未动源码）。
> P0 项已用算术复算或可执行测试确认；标注 **(未验证)** 的项需要真机日志。
>
> 🔴 **真机新增 P0-5（第六轮实测）**：HTTPS/TLS 在设备上必然
> `MBEDTLS_ERR_SSL_ALLOC_FAILED`，同步从未成功——A/B 证实与本次修复无关。
> 详见本文件 P0-5 与 `verification.md` §7。
>
> 🔴 **真机新增 P0-6（第六轮实测）**：命令压力下间歇性硬崩溃（堆破坏迹象），
> 落点在**本轮未改动**的 `command_sessions.rs`；已排除本轮 `effect_task` 看门狗改动，
> 但**基线 6 次全 0 而含本轮改动的构建出现 4 次事件**，
> **不能排除本轮改动影响触发概率，也不能断言既存**。发布定性前需先取证。
>
> 🔧 **第四轮已实施修复**：完整处置状态见 [`fixes.md`](fixes.md)。
> 已修：P0-1、P0-2、P0-3、P1-1、P1-2、P1-5、P1-8、P1-9、P1-10、P1-11、P1-13、
> 验证基建（固件测试 + CI 门禁 + `.gitattributes`）。
>
> 🔁 **第五轮（评审独立复现后）又修了两处**：① P1-10 的指纹修复还漏了
> "分钟与数据同时变化时只刷时钟区域"这一层（刷完即缓存新指纹 → 下次 `Noop`）；
> ② P1-11 的 deadline 原先锚在可修改的 RTC 墙钟上，调时区会提前/延后触发，
> 已改为单调时钟。另外收窄了 §6 关于 `#[test]` 编译行为的表述范围。
> **P0-4 未改代码**（需单独立项，理由见该条与 `fixes.md`）。
> 本文件以下内容保留**发现时的原始行号与分析**，未随修复回填。

severity 定义：
- **P0** — 可导致设备重启（panic → abort → reboot），或使核心功能静默失效 / 永久卡死
- **P1** — 数据越界 / 陈旧显示 / 幂等性失效等行为缺陷，多数可由服务端数据或 NVS 数据触发
- **P2** — 死代码、跨 crate 漂移、结构性问题

## 第二轮复核补充（逐行重读全部源码）

首版文档由源码通读得出，第二轮对两个 crate 的全部 66 个模块做了逐行重读，并对结论做了
**可执行验证**（临时集成测试实跑、编译器证伪、二进制层面查行尾）。结果：

- 首版的 P0-1 / P0-2 / 死代码清单**全部属实**，行号逐条核对无误。
- 新增 **P0-3、P0-4** 两项（响铃期 ClearAlarms 死锁、BLE 重连后应答永久失效）。
- 新增 P1-10 ~ P1-13、P2-7。
- `verification.md` 中"386 passed, 0 failed"等三处结论已被证伪，详见该文件的更正段。

标注 **✅ 实测** 的条目表示写过可运行的测试或让编译器复现过，不是从代码推断。

## 第三轮更正（对第二轮结论的修订）

第二轮的**缺陷判定**经复核全部成立，但**几处修复建议是错的、覆盖范围有遗漏**，已就地更正：

| 条目 | 第二轮的问题 | 现状 |
|---|---|---|
| P0-4 修法 | 建议"清退休位"或"改用 attempt_id 集合" —— 两者都会引入旧回调误配，因为 NimBLE 回调只带 `conn_handle` | 已重写修法，说明必须先解决回调归属；并从"顺手可修"降级为"需单独立项" |
| P1-2 修法 | 称 `weekday_from_days` 已由 `alarm_schedule` 重导出、可直接替换 | 已更正：它是 `pub(crate)`、未重导出、且接收 epoch 天数而非 Y/M/D |
| P1-10 范围 | 只列了 `AlarmList` / `TodoList` 两处漏字段 | 已扩充：补 `TodoList.repeat`、`InboxItem.body`，以及**完全未采集**的 `Calendar` / `WeekView` |
| P2-7d | 推论"`mark_read` 导致 items 超预算写失败"不成立 | 已删除并替换为两个真实问题：`pending` 无界增长、items/pending 部分提交 |

`hardware-assessment.md` 同步更正了编译门禁的写法（裸 `cargo check` 不检查
`#[cfg(test)]`，需 `--all-targets`）与两处表述过强的地方。

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

## P0-3 · 响铃期间收到 `clear_alarms` → 闹钟界面永久无法退出、铃声不停 ✅ 实测

**位置**：`logic/src/app.rs:3412`（`transition_command` 的 `ClearAlarms` 分支）

```rust
ControlRequest::ClearAlarms => {
    ...
    state.alarms.alarms.clear();
    state.alarm_runtime = AlarmRuntimeState::Disarmed;   // ← 无条件覆盖，不看当前是否 Firing
```

**机制**：退出 `Screen::AlarmRinging` 的三条路**全部**以 `Firing` 为前提：

| 出路 | 位置 | 前提 |
|---|---|---|
| ENTER 键消音 | `app.rs:1865-1869` | `screen == AlarmRinging && matches!(alarm_runtime, Firing{..})` |
| 5 分钟自动消音 | `app.rs:2644-2654` | `Firing { ring_deadline_unix: Some(..) }` |

> 🔁 **第六轮补充**：该自动消音原先锚在**墙钟**上，因此"响铃期间调整时区"
> 会提前消音或永不消音。现已与提醒、配对超时一起改为单调 `PowerPoll.now_ticks`
> （字段 `ring_deadline_ticks`，由 `expire_ring_auto_silence` 判定）。
> 这与 P0-3 是**同一条不变式**的两个侧面：响铃这条出路不能依赖任何可被外部
> 改动的量。详见 `fixes.md`。
| `transition_button` 兜底 | `app.rs:1877-1891` | 该屏幕白名单**不含 `AlarmRinging`**，落到 `vec![]` |

`ClearAlarms` 把 `alarm_runtime` 打成 `Disarmed` 后三条路同时失效：

```
响铃中（screen=AlarmRinging, runtime=Firing, StartTone 已发出）
   └─ 上位机 clear_alarms（USB 或 BLE 均可）
        └─ runtime := Disarmed，screen 仍是 AlarmRinging
             ├─ ENTER → transition_button 三个分支全不匹配 → vec![]
             ├─ Tick → ring_deadline 分支要求 Firing → 不触发
             └─ StopTone 永远发不出去
                  └─ audio_task.rs:98 的 AlarmRing 分支是无界循环 → 铃声一直响
```

深睡也救不了：`app_sleep_inputs` 的 `page_allows_sleep` 要求 `Screen::Home`
（`app.rs:1077`），响铃屏永远不满足。**唯一出路是断电。**

**验证**：写过临时集成测试实跑确认——ENTER 返回空批次、30 分钟 tick 后仍在
`AlarmRinging`、全程无一个 `StopTone`。现有 386 个测试里**没有任何一个**覆盖
"`Firing` 状态下收到 `ClearAlarms`"（`app.rs:8933` / `:8994` 的两个用例都只从
`Armed` 出发）。

**修法**：`ClearAlarms` 分支应先判断 `alarm_runtime`：若正在 `Firing`，走
`dismiss_ringing(state)` 的路径（转 `WaitingForRearm` + `StopTone` + 恢复
`screen_before_ring`）再清空列表；不要直接赋 `Disarmed`。
**另建议给 `transition_button` 的兜底分支加上 `AlarmRinging`**，让任意按键在
状态机被推到意外角落时仍能离开阻断页——这是比单点修复更有价值的护栏。

---

## P0-4 · BLE 断开重连后所有应答永久失效（conn_handle 被永久退休）

**位置**：`rust-firmware/src/ble_control.rs:104-165`（`NotifyAttemptMailbox`）

`retired_handles: [u64; 1024]` 位图用于过滤**迟到的 notify-tx 回调**，设计意图是对的，
但**只置位、从不清位**（全仓库 grep 确认：仅 `:129` 一处写入，无任何清除）：

```rust
fn arm(&mut self, attempt: NotifyAttempt) -> bool {
    if self.armed.is_some() || self.is_retired(attempt.conn_handle) {
        return false;          // ← 该 handle 一旦退休，永远拒发
    }
```

退休发生在两处，**断开连接是其中之一**：

- `quarantine()`（`:140-145`）—— notify 失败 / 2 s 超时
- `release_generation()`（`:147-156`）—— **每次 `Disconnected` 都调用**（`run()` 的 `:700`）

**后果链**：

```
客户端断开 → release_notify_generation → retire_handle(conn_handle)
   └─ 客户端重连（同一次配对会话内，BleSession 未重建 → 位图不重置）
        └─ NimBLE 单连接场景几乎必然复用同一个 conn_handle
             └─ write_reply → worker notify() → arm() 见 is_retired → false
                  └─ Err(BLEError::fail()) → Err((false, ...)) → ReplyTerminated
                       └─ 此后该连接上每一条命令都拿不到应答
```

`NotifyAttemptMailbox` 的生命周期绑定在 `BleSession`（`:867` 创建），而 `BleSession`
覆盖整个 Start..Stop 配对会话。**在一次配对会话内断线重连一次，命令通道就彻底哑了**，
上位机侧只会看到超时。

**(未验证)**：结论依赖"NimBLE 会复用 conn_handle"这一假设。需真机抓一次
断开-重连的串口日志，比对两次 `conn_handle` 的值即可证实/证伪。

### ⚠️ 修法没有看上去那么简单——不能直接清位

> 本节是评审第三轮的更正。初版曾建议"在 `release_generation()` 或新连接建立时清掉该
> handle 的位"，或"改用 `attempt_id` 集合，位图是冗余的"。**这两个建议都是错的**，
> 已在主机侧用邮箱实现复现。

根因在于 **NimBLE 的 notify-tx 回调不携带任何请求身份**。
`ble_control.rs:972-976` 能从回调里拿到的只有 `conn_handle`：

```rust
notify_char.lock().on_notify_tx(move |event| {
    let conn_handle = event.desc().map(|desc| desc.conn_handle()).unwrap_or(u16::MAX);
    let Some(attempt) = ...attempts.lock().ok()
        .and_then(|mut attempts| attempts.take_for_callback(conn_handle))
```

而 `take_for_callback`（`:158-164`）**只按 `conn_handle` 匹配**，把当前 armed 的
attempt 直接取走：

```rust
fn take_for_callback(&mut self, conn_handle: u16) -> Option<NotifyAttempt> {
    let attempt = self.armed?;
    if attempt.conn_handle != conn_handle { return None; }
    self.armed.take()          // ← 身份来自邮箱，不来自回调
}
```

**也就是说：`generation` 和 `attempt_id` 是邮箱"猜"出来的，不是回调给的。**
位图因此不是冗余的——它是"这个 handle 上还可能有迟到回调在路上"的唯一记账。

于是初版的两个建议各自失败：

| 建议 | 为什么不行 |
|---|---|
| 清掉退休位 | 新请求被 `arm()` 后，**旧连接的迟到回调**会 `take_for_callback(同一个 conn_handle)` → 取走**新**请求的 attempt → 新请求被误报为已完成/失败 |
| 换成 `attempt_id` 集合 | 回调里根本没有 `attempt_id` 可比对，集合无从查起 |

**正确的修法必须先解决"回调归属"，二选一：**

1. **建立可靠的旧回调排空边界** —— 在重新允许某个 handle 之前，确保该 handle 上
   所有在途 notify 的回调都已到达或已超时。`release_notify_generation()`
   （`:1090-1103`）已经在做部分排空（`while notify_tx_rx.try_recv().is_ok() {}`
   + 过滤 `notify_tx_pending`），但它排空的是**已经进入邮箱的**事件，
   **无法保证 NimBLE 内部没有还未回调的**。需要一个基于时间或基于
   `BLEDevice` 状态的确定性边界。
2. **换掉 correlation 载体** —— 不依赖 `on_notify_tx`，改用"发一条、等一条、带自己的
   超时"的严格串行模型（现有的 `inflight` + 2 s 超时已经接近这个形状），
   把位图降级为纯粹的"当前 handle 是否可用"单值状态。

**在解决归属问题之前，这条缺陷只能缓解不能根治**：例如把退休作用域限制在
`BleSession` 内并在 `Stop`→`Start` 时重建邮箱（现已如此），
或者在断开后**延迟一个已知上界**再解除退休。

**关于那 8 KiB**：位图的**机制**是有必要的，但它的**定量**不合理——
按 `u16` 全值域（65536 个 handle）开 1024 × u64 = 8,192 字节，
而 `CONFIG_BT_NIMBLE_MAX_CONNECTIONS=1` 且 `ble_control.rs:888-891` 会主动断开
第二个连接，实际需要追踪的 handle 是个位数。改成小型有序集合即可回收绝大部分空间，
**但这属于空间优化，不解决 P0-4**。

> ⚠️ 动这块会同时踩到 `logic/src/lib.rs:148` 的契约测试
> （`assert!(BLE_SOURCE.contains("retired_handles: [u64; 1024]"))`），需一并更新。

> 🔧 **第四轮未修改此处代码**。复核再次确认上述约束成立，因此**没有**做"清位"或
> "改 attempt_id 集合"这类看似顺手的修改。两条根治路线的取舍（以及"路线 2 会让
> 重连后偶发丢一条应答、靠幂等重放恢复"这一行为代价）写在 [`fixes.md`](fixes.md)。
> **立项前先做一件事**：真机抓断开-重连日志比对两次 `conn_handle`——
> 若 NimBLE 不复用 handle，本条不成立。

---

## P0-5 · HTTPS/TLS 在真机上必然失败 → 设备永远拿不到服务端内容 ✅ 真机实测

**位置**：`rust-firmware/src/sync.rs` 的 HTTPS 建连路径（`esp-idf-svc` 的
`EspHttpConnection` / `esp-tls`），触发点在 sync 任务与 urgent poll。

**真机证据**（第六轮，`esp32s3` rev v0.2 / 16 MB，MAC `20:6e:f1:b4:7d:e4`）：

```
I (16661) inkwash_note4::wifi: Wi-Fi connected to 'Ccloude_2.4G'
E (18733) esp-tls-mbedtls: mbedtls_ssl_setup returned -0x7F00
E (18734) esp-tls: create_ssl_handle failed
E (18734) esp-tls: Failed to open new connection
E (18736) HTTP_CLIENT: Connection failed, sock < 0
W (19311) inkwash_note4::ctx: Urgent poll failed: POST …/api/sync failed to start: ESP_ERR_HTTP_CONNECT
```

- `-0x7F00` = **`MBEDTLS_ERR_SSL_ALLOC_FAILED`**：不是证书、不是 DNS、不是凭据，
  而是 **TLS 上下文内存分配失败**。
- **100% 复现**：每次 Wi-Fi 连上后约 2 s 必失败；随后每约 8 s 重试一次，全部失败。
- `sync_now` 命令直接回 `error`：`failed to start: ESP_ERR_HTTP_CONNECT`。
- **A/B 已证实与本次修复无关**：在**基线 `0f493d7`**（未打任何本轮改动）上
  以完全相同的方式失败（同样的 `-0x7F00`）。

**为什么是 P0**：设备的产品前提是"只消费不生产、从服务端拉结构化 JSON"
（见 `hardware-assessment.md` §十）。TLS 建不起来意味着：

1. **alarms / todos / inbox 永远无法从服务端同步**——内容功能实质失效；
2. **NTP 校时永不发生**（`sync.rs` 的日常 RTC 对齐挂在同一个连接窗口里）；
3. 每 8 s 一次"连 Wi-Fi → TLS 失败"的空转，持续耗电；
4. 设备看起来"在工作"（时钟走、界面刷新、协议应答正常），
   **只有内容永远不更新**——正是本项目最典型的那种静默失效。

**与已有结论的关系**：`hardware-assessment.md` §八 的疑点 1 预测
"交棒释放的内存比代码认为的少"；本条是该方向的**实测落点**——
`mbedtls_ssl_setup` 需要一块较大的连续内 RAM，而当前内 RAM 预算（9 线程栈
>160 KiB + NimBLE/Wi-Fi 缓冲）已无余量。`review-findings.md` 的 P0-4、
`hardware-assessment.md` §一 的"24 KiB 可回收"建议，现在有了直接动机。

#### 堆取证结果（第六轮下半场，`verification.md` §7.6）

已按上述第 1、2 步取了证据（插桩 `heap_probe.rs`，原始日志与 ELF 在
`logs/hw-forensics/`）：

- **70 / 70 次同步尝试全部失败**（run1 15 + run2 27 + run3 28），`request-ok` 0 次；
  `EspHttpConnection::new` 从未失败 → 失败点在 TLS 会话建立。
- 失败时刻：`int_free` ≈ 24.6–28.7 KB，但 **`int_largest` 只有 7.7–12.3 KB**；
  `dma_largest` 同量级；**`psram_free` ≈ 8.37 MB 基本未用**。
- 生成的 sdkconfig：
  `ASYMMETRIC_CONTENT_LEN=y`、`SSL_IN_CONTENT_LEN=16384`、`SSL_OUT_CONTENT_LEN=4096`、
  `MBEDTLS_DYNAMIC_BUFFER` **未开**、`MBEDTLS_EXTERNAL_MEM_ALLOC` **未开**。

**失败分配的实际尺寸与 caps（第 ① 步，已从烧入 ELF 反汇编确认）**：

- 失败的是 `mbedtls_ssl_setup`（`ssl_tls.c:1386`）的**第 1 次**分配
  `ssl->in_buf = mbedtls_calloc(1, 16717)`：
  反汇编常量为 `movi.n a11,77` + `addmi a11,0x4100` = **`0x414D` = 16,717 B**
  （= `13 头部 + 320 载荷开销 + SSL_IN_CONTENT_LEN 16384`）。
  第 2 次 `ssl->out_buf = calloc(1, 4429)`（`0x114D`）**从未被尝试**——
  `in_buf` 失败即 `goto error`。
- **caps**：生效分支 `CONFIG_MBEDTLS_INTERNAL_MEM_ALLOC=y` ⇒
  `heap_caps_calloc(n, size, MALLOC_CAP_INTERNAL|MALLOC_CAP_8BIT)`；
  `MALLOC_CAP_INTERNAL` 要求内存"不得在 flash/spiram cache 关闭时消失"，
  **不允许 PSRAM**——8.37 MB 空闲 PSRAM 是**按 caps 不允许用**，不是"没用上"。
- **失败瞬间的池**（HEAPPROBE 用同一 caps 掩码）：`int_free` 24,575–28,731 B（够）、
  **`int_largest` 7,680–12,288 B（不够）**。

**机制层面已确认**：失败是"没有足够大的**连续**内部块"，而非"内存不足"。
这解释了"总空闲够却 100% 失败""PSRAM 始终充沛"两个观测。

> ⚠️ **边界**：确认的是失败的**直接机制**；**没有**验证任何修法，
> 也**没有**排除 `esp-tls` 此前对内部堆的占用/碎片化对"最大连续块仅 7.7–12.3 KB"
> 的贡献。**P0-5 的修复仍未确定**；A/B 依然只支持"基线也存在"。
> `CONFIG_MBEDTLS_DYNAMIC_BUFFER` / `CONFIG_MBEDTLS_EXTERNAL_MEM_ALLOC`
> （或下调 `SSL_IN_CONTENT_LEN`）是**待验证的候选修法**，不是结论。

#### 对照实验 ②：仅 `CONFIG_MBEDTLS_DYNAMIC_BUFFER=y`（`verification.md` §7.7）

单变量，生成的 sdkconfig 差异只有 3 行（唯一行为变更即该变量）。
二进制确认生效：`mbedtls_ssl_setup` 不再一次性分配 in/out 缓冲，
改为 I/O 时按实际记录长度增长。

| | 基线 | 实验 |
|---|---|---|
| `mbedtls_ssl_setup` 失败 | 70/70 | **0/45** |
| `request-ok` / `Sync fetched` | 0 / 0 | **24 / 17** |
| 复位 | 1 次 | 0 |

**但根因未消除**：失败点从 setup 移到 handshake，尺寸从 16,717 B 降到 ~4,770 B，
在 `int_largest` 掉到 4,608 时**仍然失败**（21/45）。阈值完全自洽
（`int_largest` 7680 → 全成功；4608 → 全失败）。
故该选项是**缓解而非修法**。

**仍待证明**：17 次 `Sync fetched` 全是 `0 alarms, 0 todos, 0 inbox`，
服务端返回空列表，因此"**非空数据被实际应用**"**未被证明**；
需先在服务端放入至少一条 alarm/todo 再复跑。

**实验 ①（非空数据应用）已完成**：放入 1 alarm + 1 todo 后，
`Sync fetched: 1 alarms, 1 todos` → 设备 NVS 与服务器 payload 逐字段一致 →
硬复位后字节相同。**应用与持久化成立**（`verification.md` §7.8）。

**实验 ③（`EXTERNAL_MEM_ALLOC`，单变量）**：`mbedtls_ssl_setup` 失败 70→**0**，
`request-ok` **40/40**，**分配失败（任何形式）0**，`Sync fetched` 29。
分配维度上优于实验②（22 次失败）。**但 P0-6 仍发生**：
一次 `Guru Meditation (LoadProhibited)`，落点
`ctx::DeviceContext::poll_alarm_snapshot`（`EXCVADDR=0x0c`）—— **第三个不同落点**，
且 `ctx.rs` 未被改动。**TLS 100% 成功时崩溃照样出现。**

**下一步**：

1. 若要继续收窄 P0-5：`EXTERNAL_MEM_ALLOC` 把全部 mbedTLS 分配搬到 PSRAM
   （非 DMA、更慢），牵动面大；可再试下调 `SSL_IN_CONTENT_LEN` 等更窄的变量；
2. P0-6 需独立立项：三个落点（缓存析构 / 异常保存 / RTC 快照轮询）
   指向"状态被破坏"而非某一处代码，建议按 `verification.md` §7.5 的顺序取证；
3. 查清碎片化趋势（12288 → 4608）。
   （原先记的"证书签名校验失败"已更正：`0x4290` = `RSA_PUBLIC_FAILED` 叠加
   `MPI_ALLOC_FAILED`，**同属分配失败**，见 `verification.md` §7.7。）

> ⚠️ 这条是**真机实测**结论，且**不由本轮修复引入**；本轮修复未触碰 TLS/sync 路径。

## P0-6 · 命令压力下间歇性崩溃（堆破坏迹象）⚠️ 真机复现，归因未定

**真机证据**（第六轮；`esp32s3` rev v0.2 / 16 MB，MAC `20:6e:f1:b4:7d:e4`）：
连续 `set_timezone` 压力 + soak 后出现两种硬崩溃，符号化后落点为：

```
RawVecInner::deallocate
  → <RawVec<u8> as Drop>::drop
    → drop_in_place::<[command_sessions::CachedReply]>
      → VecDeque<CachedReply>::truncate
        → CommandSessions::begin        ← logic/src/command_sessions.rs:58
          → inkwash_note4::main
```

即 `begin()` 的 `cached.clear()` 在析构缓存应答时读到**损坏的堆指针**
（`EXCVADDR = 0x3f`）。另一次是 `Double exception`、回溯 `CORRUPTED`、
CPU0/CPU1 同时 dump——典型的堆/栈被破坏后表象。

**复现率**（`40 次 set_timezone + 60 s soak`，同一脚本）：

| 构建 | 会话数 | Guru 事件 |
|---|---|---|
| 基线 `0f493d7` | 6 | **0** |
| 本轮逻辑改动 + 还原 `effect_task.rs` | 6 | **1** |
| 完整修复 | 3 | **1** |
| 完整修复（60 写 / 120 s） | 1 | **2** |

复现入口：`python3 scripts/smoke-note4.py --stress 40 --soak 100`。

**归因**：

- ✅ **与 P0-1 的 `effect_task` 看门狗改动无关** —— 把它还原到基线后崩溃照旧。
- ✅ 崩溃现场 `command_sessions.rs` **本轮未改动**；`logic` 内无 `unsafe`。
- ⚠️ **但基线 6 次全 0、含本轮改动的构建 5 个会话 4 次事件**，
  样本小且间歇，**不足以断定是既存缺陷**；也不能排除本轮改动影响了触发概率。
- 高度可疑的共同因子是**预存的 P0-5**（每 8 s 一次 `mbedtls_ssl_setup`
  `ALLOC_FAILED` + Wi-Fi 重连churn）。

**为什么值得单列 P0**：这是**内存安全**问题，且触发条件是普通命令流量，
不是极端输入。`CommandSessions` 是每条命令都会走的路径。

**取证进展（第七、八轮，细节见 `verification.md` §7.7–§7.11）**：

| 轮 | 变量 | 结果 |
|---|---|---|
| ⑦ | 堆取证（`HEAPPROBE`） | TLS 失败分配 = `ssl->in_buf` 16,717 B，caps 禁 PSRAM |
| ⑦ | `DYNAMIC_BUFFER=y` | setup 失败 70/70 → 0/45，但需求降到 ~4,770 B 仍会失败（缓解非修法） |
| ⑦ | `EXTERNAL_MEM_ALLOC=y` | `request-ok` 40/40、分配失败 0；**崩溃仍出现** |
| ⑧ | 栈高水位 | 8 任务余量 >56%、无下降趋势 → **未观察到常规栈耗尽** |
| ⑧ | `HEAP_POISONING_COMPREHENSIVE=y` | 1 次崩溃，`EXCVADDR = 0xcecece00` |

**目前能确认的**：崩溃现场出现了**填充模式特征**的异常地址
（`0x3f`、`0xa5a5a5b1`、`0xcecece00`），落点先后为
`command_sessions.rs` 缓存析构、`_xt_context_save` 双异常、
`ctx::DeviceContext::poll_alarm_snapshot`（`EXCVADDR=0x0c`）。

**仍不能确认的（边界）**：

- `0xcecece00` 与 `MALLOC_FILL_PATTERN`、`0xa5a5a5b1` 与 `tskSTACK_FILL_BYTE`
  只是**吻合**，**不能据此断定来源是未初始化的堆/栈对象**——
  错误复制、失效引用、指针破坏都能传播这类模式。
- **无毒化断言不能排除越界写或释放后使用**；且 canary 断言通常定位
  **检测现场与受损块**，不保证指出写坏指令。故**不以"多跑抓 canary"为依据**。
- **未初始化读取、破坏源、责任模块均未确定**；
  各项改动（含本轮修复）对**触发概率**的影响**未排除**。
- 崩溃落点**不等于**写坏位置（双异常还掩盖了原始帧）。

**建议**：定性之前**不要发布**。下一步改为**静态审查 FFI/C 边界**
（见 P0-6 审查清单），不再叠加设备侧诊断变量。

## P2-8 · 潜在死锁：BLE 生命周期邮箱的"无超时阻塞发送"（**未证明，未修复**）

**登记性质**：这是**单独登记的潜在死锁候选**，
**不是**已找到的 P0-6 内存破坏源，也**不作为缺陷**，在证明可达之前**不修**。

**位置**：`rust-firmware/src/ble_control.rs:205-213`（`send_lifecycle`）——
两跳投递都是"邮箱满则 `Condvar::wait` 且**无超时**"：

```
NimBLE host task --send_lifecycle--> session 邮箱(16) --worker 循环(:669)--> 转发
     --send_lifecycle--> BleControl 邮箱(16) --main(:1319)--> 状态机
```

**假设的死锁交错（尚待证明）**：

1. session 邮箱累计满 16 条生命周期事件，消费者（worker）尚未排空；
2. 此刻 `BleControl::stop` 到达 → worker 处理 `Stop` → `session.take()`
   → `Drop` → `shutdown_nimble()` → `nimble_port_stop()`
   **阻塞等待 host task 处理停止事件**；
3. host task 正阻塞在第 1 步的 `send_lifecycle` 里，而唯一消费者（worker）
   已进入 `shutdown_nimble` 不再排空 → **双方互等**，BLE worker 永久挂起。

**为什么现在不能判定为缺陷**：需要证明"第 1 步已满 16 且第 2 步紧随其后"
这一状态**可达**。当前只是"理论可能"：worker 每 ~20 ms 排空一次，
且 `main`(:1319) 是第二个独立消费者，单连接外设要在该窗口内堆积 16 次事件
难度很大——但**未做测量，也未构造**。

**若要推进**，需要：构造可复现的事件序列（或在真机上统计邮箱深度上界），
证明"满 16 且消费者停摆"可发生；在此之前既不算缺陷也不修。

**关联**：`docs/verification.md` §7.13 观察 C。

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
`rust-firmware/src/screens.rs:404-411` 是**另写的一份**，用 `DAYS[(month-1)]`，无任何校验。
同文件 `weekday_of`（`:413-419`）的 `T[(month - 1) as usize]` 同理。

**比首版描述的更宽**：不只 `month == 0` 会炸。入参来自 RTC 的
`bcd_to_bin(buf[5] & 0x1F)`（`rtc.rs:70`），BCD 解码的实际值域是 **0..=25**
（`((0x1>>4)&0xF)*10 + 0xF = 25`），因此：

| month | `DAYS[(month-1) as usize]` | 结果 |
|---|---|---|
| 0 | `DAYS[usize::MAX]`（下溢） | panic |
| 1..=12 | `DAYS[0..=11]` | 正常 |
| 13..=25 | `DAYS[12..=24]` | **越界 panic**（数组只有 12 项） |

**同一个仓库里存在两套标准**：`home.rs:159-161` 对同样的 RTC 字段做了钳制
（`(dt.month as usize).saturating_sub(1).min(11)`、`WEEKDAYS[(dt.weekday as usize).min(6)]`），
`screens.rs` 却是裸索引。logic 版另有 Zeller 交叉验证测试（`datetime.rs:128-141`），
固件版无任何测试。

**修法**（初版此处有误，已更正）：

- `days_in_month` **可以直接换**：`logic/src/datetime.rs:50` 是 `pub`，
  且 `lib.rs:10` 是 `pub mod datetime`，`screens.rs` 直接
  `use inkwash_logic::datetime::days_in_month` 即可。
- `weekday_of` **不能直接换**，需要先改 logic 的公开 API：

  | 事实 | 位置 |
  |---|---|
  | `weekday_from_days` 是 `pub(crate)`，不是 `pub` | `datetime.rs:94` |
  | `alarm_schedule.rs:7` 只重导出了 `date_from_days` / `days_since_epoch`，**没有** `weekday_from_days` | `alarm_schedule.rs:5,7` |
  | 它接收的是 **epoch 天数**（`days: i64`），不是 `(year, month, day)` | `datetime.rs:94` |

  所以替换要么写成
  `weekday_from_days(days_since_epoch(y, m, d))`（并把该函数改成 `pub`
  或由 `alarm_schedule` 重导出），要么在 `logic/src/datetime.rs` 里补一个
  `pub fn weekday_of(year: u16, month: u8, day: u8) -> u8` 包装再调用。
  **不存在"直接替换"的路径。**

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

### P1-10 · 渲染指纹漏采关键字段 → 列表显示陈旧内容 ✅ 实测

`render_plan.rs:61-88` 的 `data_fingerprint` 有两层缺口:**采集的屏幕不全**,
**已采集的屏幕字段也不全**。

**缺口一:只有 4 个 `Screen` 分支采数据,其余全部落进 `_ => {}`（`:86`）**,
指纹恒为 0 —— 但其中两个屏幕明确渲染了待办数据：

| 屏幕 | 指纹 | 实际绘制的数据 | 后果 |
|---|---|---|---|
| `Calendar` | **恒 0**（`_` 分支） | `draw_calendar_grid(.., todos)` `screens.rs:103-137`，画 due 圆点 | 待办变化不重绘 |
| `WeekView` | **恒 0**（`_` 分支） | `draw_week_view(.., todos, ..)` `screens.rs:292-402`，逐日列出待办 | **新增待办不出现** |
| **`Home`**（第四轮补漏） | **恒 0**（`_` 分支） | `draw_home_surface`（`app_runner.rs:512-533`）渲染下一闹钟、待办计数与今日到期数、未读角标、Wi-Fi 标志 | **闹钟/待办/未读变化时 Home 不重绘**；Home 是停留最久的页面，可见度高于上面两个 |

**缺口二:已采集的 4 个屏幕,字段比实际绘制的少**：

| 屏幕 | 指纹采集（`render_plan.rs`） | 实际绘制（`screens.rs`） | 漏掉 |
|---|---|---|---|
| `AlarmList` | `(id, hour, minute, enabled)` `:73` | `format_alarm_row` `:422-457` | **`label`、`repeat`** |
| `TodoList` | `(text, done)` `:78` | `format_todo_row` `:588-608` | **`importance`、`due_date`、`repeat`** |
| `Inbox` | `(id, read, title)` `:83` | `format_inbox_row` `:610-614` | — |
| `InboxItem` | 同 `Inbox`：`(id, read, title)` `:83` | `draw_inbox_item_detail` `:631-666`，**正文来自 `item.body`** | **`body`** |

后果举例：

- 服务端改一条通知的**正文**，设备停在通知详情页 → `Noop`，正文不更新；
- 服务端新增一条**本周的待办**，设备停在周视图 → `Noop`，待办不出现；
- 把待办重要度 Low→High（行首多出 `!! `）或改闹钟 label/repeat → `Noop`。

要等到切页或时钟分钟跳变才会更新。

**验证**：主机侧已复现——改通知正文、周视图新增待办，`plan_render()` 均返回 `Noop`；
`ViewModel::from_state` 两组指纹完全相同。

**修法**：指纹必须覆盖该屏幕实际渲染读到的**全部**状态，包括当前落进 `_` 分支的
`Calendar` / `WeekView` / `Home`。逐字段列举容易再次漏（本条已经漏了两轮），
更稳妥的做法是**直接 hash 渲染层的输出**——对列表页 hash `format_*_row`
的结果字符串，对 `Calendar` / `WeekView` hash 参与绘制的待办投影，
让"画什么"和"比什么"在结构上无法脱钩。

> ⚠️ **第五轮补充：补全指纹本身还不够。** `plan_render` 的 Home 分支**先**判断
> 分钟变化并返回 `PartialRegion::Clock`，于是"分钟 + 数据同时变化"时只刷时钟区域；
> 完成后注册表缓存了**新**指纹，下一次比较直接 `Noop`，图标/计数永久停在旧状态。
> **只改指纹会让这个缺口变得更隐蔽**：以前是"数据变化不重绘"，现在是"重绘了一部分
> 却记成全部完成"。时钟局刷必须附加"其他渲染数据未变"的前提。

### P1-11 · BLE 配对超时是死代码，配对页无超时

`app.rs:2662-2686` 的 tick 超时分支要求 `st.pairing_deadline_unix` 为 `Some`，但该字段
在生产路径**只被赋 `None`**：

| 位置 | 赋值 |
|---|---|
| `app.rs:207`（`Default`） | `None` |
| `app.rs:2088`（Settings 进入配对） | `None` |
| `app.rs:2671`（超时分支内部） | `None` |
| `app.rs:7189` | `Some(...)` —— **测试夹具，非生产路径** |

因此配对页没有任何超时保护，只能靠长按 ENTER（`app.rs:1459-1476`）或 BLE 侧报错退出。
实测 14 小时 tick 后界面仍在。

**修法**：进入配对时赋一个真实 deadline（例如 `now + 120`），否则应删掉整个超时分支
及该字段——留着会让维护者误以为有保护。

### P1-12 · 日历无法翻月

`app.rs:2559-2569` 的上下键被钳死在当月内（`saturating_sub(1).max(1)` /
`(selected_day + 1).min(dim)`），全屏没有任何翻月入口；`transition_tick`（`:2635-2642`）
还会在跨月时把 `cal.year/month` 强制拉回当前月。

结合 `Repeat::Monthly` 与待办 `due_date` 支持任意未来月份，**用户无法在设备上查看
下个月的待办分布**。不确定是刻意取舍还是遗漏，但 `docs/` 与代码里都没有依据。

### P1-13 · 安全模式里的 `split_at` 可 panic

`main.rs:1076-1081`：

```rust
let (r1, r2) = if reason.len() > 40 {
    let (head, rest) = reason.split_at(40);   // ← 非字符边界即 panic
```

与 P0-2 同类错误，但位置更敏感：这是"核心启动事实已不可用"的兜底路径，在这里 panic
会把一次可诊断的降级变成重启循环。`reason` 目前来自 `format!("... {err}")`，ESP-IDF
错误串基本是 ASCII，概率低但不为零。

**修法**：改用 `chars().take(40)`，与同函数 `:1078` 已有的 `rest.chars().take(40)` 一致。

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

> ⚠️ **不能直接删。** `logic/src/lib.rs:175` 的契约测试断言
> `MAIN_SOURCE.contains("pending_render_completion.is_some()")`，
> 删掉该字段会让 CI 变红。这正是"用源码文本断言守护约束"的代价：
> **契约测试把死代码也一起钉住了**。清理时必须同步改 `lib.rs`。

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

### P2-7 · 第二轮新增的死路径与边界失配

**a. Mergeable 档的背压路径不可达。** `event_queue.rs:114-119` 的
"非 Tick 的 Mergeable 事件满了就丢最旧"分支永远不会执行——`priority()`
（`:16-45`）里**只有 `Event::Tick` 映射到 `Priority::Mergeable`**，
而 Tick 走的是上面的 `retain` + `push_back` 分支。
`concurrency-and-resources.md` 的背压表把它列为一条真实的有损路径，属描述过强。

**b. Wi-Fi 密码缓冲差一字节。** `wifi.rs:72-73` 允许 64 字符密码
（`HeaplessString<64>`），但 `storage.rs:19` 的读回缓冲是
`WIFI_CRED_MAX_LEN = 64` 字节，`nvs.get_str` 需要容纳字符串 + NUL。
64 字符的密码**存得进、读不出**（`ESP_ERR_NVS_INVALID_LENGTH`），实际可用上限是 63。
`SERVER_CONFIG_MAX_LEN = 256` 对 `auth_token` 同理（实际 255）。

**c. `battery_percent_from_mv` 的 i32 溢出（理论）。** `board.rs:311` 的
`-mv * mv + 9016 * mv - 19189000`，`mv` 来自 `battery_millivolts()` 的
`u16`（上限 65535）。`mv ≥ 46341` 时 `mv * mv` 溢出 i32——release 下回绕、debug 下 panic。
DB_12 衰减下 ADC 实际读不到这个量级，属于**类型允许但物理不会发生**，
记录以备将来改 ADC 量程时回看。

**d. `InboxStore::mark_read` 的两个真实问题。**

> 初版在此处写的是"绕过 `save()` 的预算裁剪，items 可能超预算写失败"，
> **这条推论不成立,已删除**：`mark_read`（`inbox.rs:75-86`）先 `load()` 出
> 一份原本就存得下的 items，再把某条的 `"read":false` 改成 `"read":true`
> —— JSON **缩短 1 字节**，不可能因此超出原尺寸。

真正需要补查的是另外两点：

- **`pending` 列表无界增长。** 每次 `mark_read` 都 `pending.push(seq)`（`:81-83`），
  而收缩只发生在两处：`ack_read()`（服务端确认后，`:88-92`）和 `save()` 里的
  `merge_pending_read`（`:43-44`，只保留仍未读的条目）。
  **若设备长期离线或服务端一直不回 `inbox_read_acked`，`pending` 只增不减**，
  写入用的是 `BLOB_BUF_LEN = 4096` 的缓冲，每个 u64 的 JSON 表示最长 20 字符 + 分隔符，
  约 190 条后 `write_blob` 开始以 `blob too large` 失败 → 此后**所有标记已读都写不进去**。
- **部分提交：items 写成功、pending 写失败。** `:84-85` 是两次独立的 `write_blob`，
  中间用 `?` 短路：

  ```rust
  self.write_blob(KEY_ITEMS, &items)?;      // ← 成功
  self.write_blob(KEY_PENDING, &pending)    // ← 失败则整个函数返回 Err
  ```

  此时本地已读状态已落盘，但"待上传的已读"丢了。下一次同步时
  `save()` 会用服务端快照（该条仍是未读）覆盖本地
  —— **用户点开过的通知会变回未读，且服务端永远不知道它被读过**。
  `mark_read` 不是原子的，两个 key 也没有版本或校验来发现这种撕裂。

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
| 1 | **P0-3** 响铃期 `ClearAlarms` 死锁 + `transition_button` 兜底加 `AlarmRinging` | 上位机一条命令即可让设备卡死响铃到没电，只能断电；修法几行 |
| 2 | P0-1 `effect-task` 订阅看门狗 | 一行代码，消除"静默数据丢失"这一最坏失效模式 |
| 3 | P0-2 `screens.rs:381` 字节切片 | 远程可触发重启，修法在同文件已有正确范例 |
| 4 | `.gitattributes` 加 `*.rs text eol=lf` | 一行配置，让测试套件在 Windows 检出下能跑（见 `verification.md`） |
| 5 | **P0-4** BLE 回调归属 —— **需单独立项，不是顺手能改的 bug** | 断线重连即失去命令通道；但根治要先建立"旧回调排空边界"或换掉 correlation 载体（清位会引入误配，见该条修法）；需同步改 `lib.rs:148` 契约测试 |
| 6 | P1-1/P1-2 边界校验收口 | 让 NVS 数据与网络数据走同一套校验；顺手删掉 `screens.rs` 的重复日期实现 |
| 7 | 修 `rust-firmware` 那 4 个编译不过的测试 | 它们从未运行过（见 `verification.md`），要么修好要么删掉，不要留假覆盖 |
| 8 | P1-10 渲染指纹补齐字段 | 服务端改动在当前页不生效，是用户可感知的"卡住了"体验 |
| 9 | P2-5 重置 `.esp32-review.yml` + 补 README 链接 | 恢复工具与文档的可信度 |
| 10 | P2-1 / P2-2 删死代码 | `ble_radio.rs`、`harness.rs` 生产部分、3 个死字段、`footer`、P1-11 的死超时；**注意契约测试耦合** |
| 11 | P2-6 拆 `main.rs` 循环体为 reducer 函数 | 唯一能在不改语义前提下降复杂度的手段 |
| 12 | 审视 `logic/src/app.rs` 拆分 | 需先把 30+ 私有字段收敛成 3–4 个内聚子结构，风险较高 |

### 一条贯穿性建议

P0-2、P0-3、P1-1、P1-2、P1-13 看起来分散，**病根是同一个：状态机与渲染层没有为
"不该发生的输入"准备出路。** 它们分别是"服务端文本不该含 CJK 折行"、
"上位机不该在响铃时清闹钟"、"NVS 不该有越界值"、"RTC 不该返回 month=0"、
"错误串不该含多字节字符"。

与其逐条打补丁，更值得做两件收口：

1. **校验收口到"数据进入设备"的唯一入口** —— `AlarmStore::load()` /
   `TodoStore::load()` / `Pcf8563::read_time()` 走与 `sync_validate` 同一套校验，
   渲染层从此可以信任输入，裸索引才是安全的。
2. **阻断页必须有无条件出路** —— `AlarmRinging` / `Reminder` / `BlePairing`
   这类全屏页，任意按键都应能离开，而不是依赖某个 runtime 子状态恰好匹配。
