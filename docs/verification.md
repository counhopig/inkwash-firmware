# 验证状态：测试、CI、构建与刷写

> 本文件回答两个问题：**(1) 仓库自己验了什么、没验什么；(2) 本文档集的结论里
> 哪些已被证实、哪些仍待真机确认。**

## 0. 第二轮复核的三处更正

首版本文件的三条结论在第二轮逐行复核中被证伪，先列出结论，细节见对应小节：

| 首版结论 | 实际 | 位置 |
|---|---|---|
| "386 passed, 0 failed（实测）" | **385 passed, 1 failed** —— 在任何 CRLF 检出下必然失败 | §1 |
| `effect_task.rs:350-483` 覆盖了 `execute_batch` 的顺序/Continue/AbortBatch | 这 4 个测试**编译不过**，从未运行过 | §1 |
| 契约测试有 9 个 | **13 个** | §2 |

---

## 1. 测试分布

`cargo test -p inkwash-logic` → **385 passed, 1 failed**（实测，Windows + CRLF 检出）。

### ⚠️ 唯一的失败：契约测试在 CRLF 检出下必然红

```
---- ble_memory_contract::ble_lifecycle_callbacks_retain_full_channel_observations ----
panicked at src\lib.rs:132:
assertion failed: BLE_SOURCE.contains("send_lifecycle(\n                &lc_tx,\n                BleLifecycle::Connected")
```

`lib.rs:132-137` 的两条断言把**字面 `\n` 连同缩进一起**写进了搜索串。
`include_str!` 读到的是工作区里的真实字节：

| 事实 | 值 |
|---|---|
| `rust-firmware/src/ble_control.rs` 的行尾 | **CRLF ×1118，裸 LF ×0** |
| 仓库是否有 `.gitattributes` | **没有** |
| 本机 `core.autocrlf` | `true` |

于是文件里是 `\r\n`，断言找的是 `\n`，必然失配。CI 跑 `ubuntu-latest`（检出为 LF）
所以永远看不到这条失败——**在 CRLF 检出下，仓库自己的测试套件就是红的**，
而且失败信息指向一条与改动无关的 BLE 契约，极具误导性。

> **准确边界**：这取决于**检出配置**，不是操作系统。
> `core.autocrlf=false` / `input` 的 Windows 开发者不受影响；
> 反过来用 CRLF 检出的 Linux/macOS 用户同样会中。
> 但 Windows 上 `core.autocrlf=true` 是默认值，所以实际受影响的以 Windows 为主。

**修法**：仓库根目录加 `.gitattributes`：

```gitattributes
*.rs text eol=lf
sdkconfig.defaults text eol=lf
```

更根本的做法是让契约测试不依赖行尾——断言前先
`BLE_SOURCE.replace("\r\n", "\n")`，或干脆改成不含换行的多段 `contains`。

### 按模块的生产/测试行数

（首个 `#[cfg(test)]` 行号之前算生产）：

| 模块 | 总行 | 生产 | 测试 | 测试占比 |
|---|---|---|---|---|
| `app.rs` | 10420 | 3753 | **6667** | 64% |
| `harness.rs` | 1105 | 510 | 595 | 54% |
| `runtime.rs` | 752 | 66 | **686** | 91% |
| `render_plan.rs` | 691 | 224 | 467 | 68% |
| `alarm_schedule.rs` | 654 | 234 | 420 | 64% |
| `event_queue.rs` | 476 | 102 | 374 | 79% |
| `epd_registry.rs` | 472 | 179 | 293 | 62% |
| `command_sessions.rs` | 453 | 268 | 185 | 41% |
| `scheduler.rs` | 358 | 110 | 248 | 69% |
| `power_state.rs` | 354 | 203 | 151 | 43% |
| `sync_validate.rs` | 345 | 131 | 214 | 62% |
| `lib.rs`（契约测试） | 289 | 29 | **260** | 90% |
| `runner.rs` | 214 | 146 | 68 | 32% |
| `audio_command.rs` | 201 | 49 | 152 | 76% |
| `datetime.rs` | 196 | 97 | 99 | 51% |
| `reminder_dedup.rs` | 162 | 53 | 109 | 67% |
| `rtc_latch.rs` | 162 | 50 | 112 | 69% |
| `protocol.rs` | 155 | 87 | 68 | 44% |
| `ble_radio.rs` | 138 | 104 | 34 | 25% |
| `ble_memory.rs` | 37 | 8 | 29 | 78% |
| `list_window.rs` | 118 | 59 | 59 | 50% |
| `alarm_flow.rs` | 127 | 113 | 14 | 11% |
| `worker_heartbeat.rs` | 24 | 4 | 20 | 83% |
| 其余 5 个 DTO 模块 | 171 | 171 | 0 | 0% |
| **合计** | **18037** | **≈6713** | **≈11324** | **63%** |

**测试是生产代码的约 1.69 倍**，且高度集中在一个纯函数 `update` 上。

### 测试真正覆盖了哪些难路径

- **时钟回拨**：`scheduler.rs` 有 `clock_rollback_re_aligns_the_urgent_cursor…`、
  `rollback_restores_advanced_boundaries` 等
- **闰年边界**：`datetime.rs:128-141` 用 Zeller 公式交叉验证 60 年范围
- **AbortBatch 语义**：`runtime.rs:528-575`、`app.rs` 的 `abort_batch_stops_remaining…`
- **队列饱和与续传**：`runtime.rs:404-479`
- **闹钟提交链**：`runtime.rs:481-525` 断言 RTC 重编程只在 `AckDone` 之后
- **协议嵌套深度**：`protocol.rs:141-148` 的注释写明"the shape that overflows
  the 4096-byte worker stack"
- **BLE 堆准入**：`ble_memory.rs:34-36` 硬编码实测值 64_831 / 31_744

### 固件侧的测试

`rust-firmware/` 曾有 3 个模块写了测试。**第四轮复核发现：这 3 个模块没有一个是活的**
（原因见 §6），其中 `effect_task.rs` 的 4 个还额外编译不过。三者均已在修复中移除：

| 位置 | 内容 | 第四轮结论 | 现状 |
|---|---|---|---|
| `effect_task.rs:350-483` | `execute_batch` 的顺序 / Continue / AbortBatch | ❌ 编译失败 + 从不运行 | **已删除**；Continue / AbortBatch 语义已移入 `logic/src/runner.rs` 并真实运行 |
| `epd_task.rs:366-392` | 完成邮箱有界性与预留的无损性 | ❌ 从不运行 | **已删除**（意图待补主机侧测试） |
| `usb_console.rs:209-292` | 命令暂存有界性、应答队列背压 | ❌ 从不运行 | **已删除**（意图待补主机侧测试） |

**固件侧真实测试覆盖是 0 个模块**（首版称 3、第二轮称 2，都偏高）。而固件才是碰
I2C/EPD/射频/NVS 的那一半。

`logic/src/lib.rs` 的契约测试现在断言 `rust-firmware` 不再出现 `#[cfg(test)]`
（`firmware_keeps_no_unrunnable_test_modules`），防止这类"假覆盖"再次静默腐烂。

#### `effect_task.rs` 的 4 个测试为何失效

那 4 个测试构造的 `EffectBatch` 字面量与当前类型定义完全不匹配：

```rust
// effect_task.rs 测试里写的                 // logic/src/app.rs:323-329 的实际定义
EffectBatch {                                pub struct EffectBatch {
    id: 1,                                       pub id: EffectBatchId,            // 元组结构体
    operation_id: None,                          pub operation_id: OperationId,    // 非 Option
    render_generation: 0,                        pub render_generation: Option<RenderGeneration>,
```

把它们提取到主机侧编译得到 **12 个 `E0308`（每个测试 3 个）**：

```
expected `EffectBatchId`, found integer
expected `OperationId`, found `Option<_>`
expected `Option<RenderGeneration>`, found integer
```

自 `EffectBatch` 引入 newtype 之后它们就没有运行过一次——而且**连编译都轮不到它们**，
这才是没人发现的原因（详见 §6）。

**修法（已实施）**：把有语义价值的断言（`Continue` 逐条上报、`AbortBatch` 首败即停）
搬进主机可跑的 `logic/src/runner.rs`，删掉固件侧那份不可运行的副本。

## 2. 契约测试：机制与脆弱性

`logic/src/lib.rs:30-289` 的 `mod ble_memory_contract` 是唯一守护固件约束的机制：

```rust
const BLE_SOURCE: &str = include_str!("../../rust-firmware/src/ble_control.rs");
const TASKS_SOURCE: &str = include_str!("../../rust-firmware/src/tasks.rs");
const SDKCONFIG: &str = include_str!("../../rust-firmware/sdkconfig.defaults");

#[test]
fn ble_worker_uses_internal_stack_and_checks_internal_heap_before_init() {
    assert!(BLE_SOURCE.contains("const BLE_TASK_STACK: usize = 16 * 1024"));
    assert!(TASKS_SOURCE.contains("MALLOC_CAP_INTERNAL | esp_idf_svc::sys::MALLOC_CAP_8BIT"));
    assert!(!TASKS_SOURCE.contains("MALLOC_CAP_SPIRAM"));
    // ...
}
```

**它守护的约束**（**13 个**测试，首版误记为 9）：
BLE 内 RAM 栈 + 初始化前堆检查、effect 任务栈预算、BLE worker 按需创建、
重复 Start 拒绝、生命周期邮箱完整性、notify 完成 attempt-bound、
main 先归约生命周期再处理结果、dispatch 饱和用来源闩锁、
传输应答保留 owned frame、BLE 控制器内存预算仅外设角色、
固件拥有射频仲裁并恢复 Wi-Fi、BLE 清理先停广播再 deinit。

**它是脆的**：断言的是**源码文本**。改写变量名、换行、调整表达式顺序都会让它失败，
而语义回归仍可能通过。但它把无法在主机运行验证的约束变成了 CI 红灯——
在这个"CI 装不了 ESP-IDF"的项目里，这是合理的工程补偿。

**未验证的**：它只断言形状，不断言行为。例如
`lib.rs:265-269` 检查 `WIFI.contains("wifi: Option<EspWifi<'static>>")`，
但**不能**证明 `EspWifi` drop 真的释放了内存（见 `review-findings.md` 疑点 1）。

### 它还有两个第二轮才发现的副作用

**a. 它把死代码也一起钉住了。** `lib.rs:175` 断言
`MAIN_SOURCE.contains("pending_render_completion.is_some()")`，而
`pending_render_completion` 是一个恒为 `None` 的死字段（`review-findings.md` P2-2）。
`lib.rs:148` 同理钉住了 `retired_handles: [u64; 1024]`，而该位图正是 P0-4 的成因。
**清理死代码和修 P0-4 都必须同步改契约测试**，否则 CI 变红。

这是"用源码文本断言守护约束"的固有代价：它无法区分"这个形状是必须的约束"
和"这个形状只是当时恰好长这样"。

**b. 它对行尾敏感。** 见 §1 的 CRLF 失败。断言里嵌字面 `\n` 让契约测试
额外耦合了检出方式，这一点在设计时显然没有考虑。

## 3. CI 覆盖与缺口

`.github/workflows/ci.yml` 原有 1 个 job，修复后新增 1 个（`firmware-format`）：

```yaml
logic:
  runs-on: ubuntu-latest
  defaults: { run: { working-directory: logic } }
  steps:
    - cargo test --locked
    - cargo fmt --check
    - cargo clippy --all-targets -- -D warnings

firmware-format:              # 第四轮新增
  defaults: { run: { working-directory: rust-firmware } }
  steps:
    - cargo +stable fmt --check      # 不需要 ESP-IDF；+stable 绕开 rust-toolchain.toml 的 esp
```

| 项 | 状态 |
|---|---|
| `logic` 测试 | ✅ 每次 push/PR |
| `logic` fmt / clippy | ✅ `-D warnings` |
| `rust-firmware` fmt | ✅ **第四轮新增**（不需 ESP-IDF） |
| `rust-firmware` 编译 / clippy | ❌ 仍需 ESP-IDF；注意 `--all-targets` 也抓不到 `#[test]` 函数体（§6） |
| app 体积检查 | ❌ 无 |
| `check-git-rev.sh` | ❌ 未接入 CI（脚本存在，只能手动跑） |
| 跨平台检出 | ❌ 只跑 `ubuntu-latest`，掩盖了 CRLF 下必然失败的契约测试（§1） |

`scripts/release.sh:2-3` 给出了不加固件门禁的理由：
"this repo builds with a real ESP-IDF toolchain, which is impractical on CI"。
理由本身成立，但结果是 **format/lint/size 回归会静默落地**，直到有人在本地构建才发现。

## 4. 构建与刷写

### 工具链

| 项 | 值 | 来源 |
|---|---|---|
| target | `xtensa-esp32s3-espidf` | `rust-firmware/.cargo/config.toml:2` |
| toolchain | `esp`（espup 安装） | `rust-firmware/rust-toolchain.toml:2` |
| ESP-IDF | `v5.5.5` | `.cargo/config.toml:13` |
| MCU | `esp32s3` | `.cargo/config.toml:12` |
| linker | `ldproxy` | `.cargo/config.toml:5` |
| build-std | `["std", "panic_abort"]` | `.cargo/config.toml:9` |
| 额外 cfg | `espidf_time64` | `.cargo/config.toml:6` |

`scripts/build-rust.sh` 做了两件必要的动态探测（而非硬编码）：

1. 若无 `$IDF_PATH`，依次探测 `~/esp/esp-idf`、`~/esp/esp-idf-*`、
   `~/.espressif/frameworks/esp-idf-*`，找到含 `export.sh` 的目录；
2. 若无 `$LIBCLANG_PATH`，从 `rustup toolchain list -v` 里取 `esp` 工具链根，
   再找 `xtensa-esp32-elf-clang/*/esp-clang/lib`。

> `scripts/build-rust.ps1:14` 注释自述 "Windows side remains unverified on a real
> toolchain - see docs." —— 该 docs 已不存在。

### 刷写红线

`README.md:85-88` 与 `scripts/release.sh:43-45` 一致给出：

```bash
espflash flash --chip esp32s3 --flash-size 16mb --flash-mode dio --flash-freq 80mhz \
  --bootloader rust-firmware/target/xtensa-esp32s3-espidf/release/bootloader.bin \
  --partition-table rust-firmware/partitions.csv --non-interactive inkwash-note4
```

- **DIO 模式，永远不要 QIO**（`sdkconfig.defaults:8` + `# CONFIG_ESPTOOLPY_OCT_FLASH is not set`）
- **不要把 Note 4 的镜像刷到 Note 4C**（两者屏不同）
- **不要 `esp_wifi_stop()` 或 `esp_restart()`**；要重启走深睡路径
  （`power.rs:66-76` 的 `restart_via_deep_sleep`）

### 分区表（`rust-firmware/partitions.csv`）

```
nvs,      data, nvs,     0x9000,  0x6000
phy_init, data, phy,     0xf000,  0x1000
factory,  app,  factory, 0x10000, 0x400000
storage,  data, 0x83,    0x410000,0xBF0000
```

- **无 OTA 槽**：单个 `factory` 4 MB。刷坏只能靠串口救，
  这使"刷错变砖"成为**不可回滚**风险——而 CI 恰恰不编译固件。
- `storage` 4 MB 已分配但**当前未被代码使用**（全部持久化走 NVS）。
- **无 app 体积门禁**，应用可增长到超过 4 MB 才在刷写期暴露。

### sdkconfig 关键项

| 配置 | 值 | 含义 |
|---|---|---|
| `CONFIG_ESP_MAIN_TASK_STACK_SIZE` | 32768 | 主线程 32 KiB |
| `CONFIG_ESPTOOLPY_FLASHMODE_DIO` | y | 红线 |
| `CONFIG_SPIRAM` / `_MODE_OCT` / `_SPEED_80M` / `_USE_MALLOC` | y | 8 MB OPI PSRAM，供 sync 响应缓冲 |
| `CONFIG_ESP_CONSOLE_USB_SERIAL_JTAG` | y | 控制台走 USJ（USB 命令通道的基础） |
| `CONFIG_USJ_NO_AUTO_LS_ON_CONNECTION` | y | 见 `:24-26` 注释：USJ 在自动浅睡下不响应 |
| `CONFIG_FREERTOS_HZ` | 1000 | tick = 1 ms |
| `CONFIG_PM_ENABLE` + `USE_TICKLESS_IDLE` | y | IDF 自动浅睡 |
| `CONFIG_FREERTOS_IDLE_TIME_BEFORE_SLEEP` | 200 | 200 ms 阈值（IDF 5.5 改名） |
| `CONFIG_ESP_TASK_WDT_TIMEOUT_S` | 10 | 见 `:52-58` 的由来说明 |
| `CONFIG_ESP_TASK_WDT_PANIC` | y | 挂起即重启 |
| `CONFIG_ESP_WIFI_TASK_PINNED_TO_CORE_1` | y | Wi-Fi 任务钉核 1 |
| `CONFIG_BT_NIMBLE_ROLE_CENTRAL` / `_OBSERVER` / `_50_FEATURE_SUPPORT` | n | 只做 GATT peripheral |
| `CONFIG_BT_NIMBLE_MAX_CONNECTIONS` | 1 | 单连接 |
| `CONFIG_BT_CTRL_RUN_IN_FLASH_ONLY` | y | 见 `:63-66`：归还 >20 KiB 内 RAM |

**`sdkconfig.defaults` 是全仓库注释质量最高的文件**，记录了 TWDT 超时的真实故障史、
USJ 浅睡问题、BLE/Wi-Fi 硬件互斥。查"为什么这么配"应先读它。

### 其它脚本

| 脚本 | 作用 |
|---|---|
| `build-rust.sh` / `.ps1` | 探测 IDF + LIBCLANG_PATH 后构建 |
| `release.sh` | 构建 release ELF → 打 tag → 推双 remote → `gh release create` 上传 |
| `check-git-rev.sh` | **守护 ELF 内嵌的 GIT_REV 与当前 `git describe` 一致** |
| `backup-flash.ps1` | `esptool read_flash 0x0 0x1000000` 备份全片 16 MiB |
| `capture-serial.py` | 非交互式串口日志采集（自动探测 `/dev/cu.usbmodem*`、`/dev/ttyACM*`） |

`check-git-rev.sh:6-13` 记录了一个真实的 P1 缺陷与修复：
`build.rs` 原先只声明 `rerun-if-changed` 于 `../.git/HEAD` 和 refs 目录，
而 Cargo 不递归监视目录 → 在当前分支提交（改写 `.git/refs/heads/<branch>`、
HEAD 不变）**不会**重跑 `build.rs` → ELF 内嵌的 `GIT_REV` 陈旧。
`build.rs:30-57` 的 `emit_git_ref_rerun_if_changed()` 是修复，
`check-git-rev.sh` 是把它变成硬失败的闸门。

## 5. 本文档集的验证状态

| 结论 | 状态 |
|---|---|
| 线程清单、栈大小、看门狗订阅点 | ✅ 逐处 grep 核对 |
| 共享资源矩阵、仲裁标志 | ✅ 静态分析 + 人工复核 |
| 死字段 / 死模块 | ✅ 全仓库 grep 确认零引用 |
| 分区表、sdkconfig、CI 内容 | ✅ 直接读取 |
| 测试数量与分布 | ✅ 实测 `cargo test` + 逐文件 `#[cfg(test)]` 定位 |
| **`screens.rs:381` 字节切片 panic** | ✅ **算术复算确认**（含 2 汉字即 panic），未在真机复现 |
| **P0-3 响铃期 `ClearAlarms` 死锁** | ✅ **写临时集成测试实跑确认**（ENTER 无效 + 30 min tick 无 `StopTone`） |
| **P1-10 渲染指纹漏字段** | ✅ **实测两组 `from_state` 指纹相同** |
| **P1-11 配对超时不可达** | ✅ **实测 14 h tick 后仍在配对页**；全仓库 grep 确认生产路径只赋 `None` |
| **`effect_task.rs` 测试编译失败** | ✅ **编译器复现**（同一字面量在 `logic` 中得到 3 个 `E0308`） |
| **`cargo check --all-targets` 能否抓到那 4 个测试** | ❌ **已证伪（第四轮）**：`rust-firmware/Cargo.toml:10` 是 `harness = false`，cargo 只传 `--cfg test` 而不传 `--test`，**`#[test]` 函数体既不运行也不做类型检查**（模块内普通函数仍被检查，见 §6）。 |
| **CRLF 导致契约测试失败** | ✅ **实测 385/1** + 二进制层面清点行尾（1118 CRLF / 0 LF） |
| P0-4 BLE conn_handle 永久退休 | ⚠️ 代码路径已确认（位图只置位不清位）；**"NimBLE 会复用 handle"未在真机验证** |
| **P0-4 的修法约束** | ✅ 主机侧用邮箱实现复现：清位后旧连接的迟到回调会取走新请求的 attempt（回调只带 `conn_handle`，`ble_control.rs:972-976`） |
| **P1-10 指纹缺口的完整范围** | ✅ 主机复现：改通知正文、周视图新增待办，`plan_render()` 均返回 `Noop` |
| `cargo check`（裸）能否抓到那 4 个测试 | ✅ 否——默认不检查 `#[test]` 函数体（原载"需 `--all-targets`"，该结论已在第四轮被证伪） |
| **`cargo check --all-targets` 能否抓到那 4 个测试** | ❌ **已证伪**：见 §6，`harness = false` 使 `#[test]` 函数体完全不被类型检查（`#[cfg(test)]` 模块内的普通函数仍被检查） |
| 固件侧"还有 2 个模块有测试" | ❌ **已修正**：`epd_task.rs` / `usb_console.rs` 的测试同样从不运行，固件侧真实覆盖为 **0** |
| `mark_read` 会因 items 超预算写失败 | ❌ **已证伪**：`false`→`true` 使 JSON 缩短 1 字节，不可能超出原尺寸 |
| 充电图标三张位图相同 | ✅ 逐行比对确认 |
| `.esp32-review.yml` 行号过期 | ✅ 核对实际行号 |
| `EspWifi::drop` 行为 | ✅ 读取 `esp-idf-svc-0.52.1` 源码；**对内存的实际影响未验证** |
| NimBLE 回调上下文 | ❌ **未验证**，需确认框架派发位置 |
| 功耗/时序数值（1.2 s 轮询、0.8 s 浅睡占比等） | ⚠️ 来自代码与 `sdkconfig.defaults` 注释，**未经真机测量** |
| `docs/` 原有内容（开发指南、控制协议、截图） | ❌ 已被 `b30c3af` 删除，只能重建可由源码确证的部分 |

---

## 6. 第四轮新增：`harness = false` 使固件侧 `#[test]` 函数体不被检查

修复过程中对"加固件编译门禁"这条建议做了实证，结果**推翻了第二轮给出的门禁选型**。

> **范围限定（第五轮更正）**：被跳过的是**带 `#[test]` 属性的函数体**，
> **不是**整个 `#[cfg(test)]` 模块。模块里的普通函数、方法、类型、`use` 语句
> **仍然会被正常类型检查**——所以模块内会出现 `unused import` / `dead_code` 警告，
> 这正是它看起来"检查过了"的原因。初版 §6 把结论写成了"`cfg(test)` 函数体不被检查"，
> 范围过大，已更正。

### 结论

`rust-firmware/Cargo.toml:10` 写着：

```toml
[[bin]]
name = "inkwash-note4"
harness = false
```

后果是 **`cargo check --all-targets --target xtensa-esp32s3-espidf` 不会检查
`#[test]` 函数体**——既不会发现其中的类型错误，也不会真正运行它们。
所以"用 `--all-targets` 就能抓到那 4 个测试"是**错的**。

### 证据（可复现）

用一个最小文件即可复现（`rustc --cfg test` 模拟 cargo 的调用，不带 `--test`）：

```rust
#[cfg(test)]
mod tests {
    pub fn helper_bad() -> u32 { "helper is not u32" }        // ← 报 E0308
    pub struct Helper;
    impl Helper { pub fn bad(&self) -> u32 { "method is not u32" } }  // ← 报 E0308
    #[test]
    fn test_bad() { let _: u32 = "test body is not u32"; }    // ← 不报
}
```

```
$ rustc --edition 2021 --crate-type bin --emit=metadata --cfg test probe.rs
error[E0308]: mismatched types   →  helper_bad
error[E0308]: mismatched types   →  Helper::bad
（test_bad 没有任何诊断）

$ rustc --edition 2021 --test --emit=metadata --cfg test probe2.rs   # 只有 test_bad 一个函数
error[E0308]: mismatched types   ×2
```

**只有 `#[test]` 函数体被跳过。**

同一结论在真机 crate 上复现过：在 `rust-firmware/src/effect_task.rs` 的
`#[cfg(test)] mod tests` 里、以及一个新增的 `#[cfg(test)] mod probe_tests` 里各插入
`let _: u32 = "definitely not a u32";`，然后
`CARGO_INCREMENTAL=0 cargo check --all-targets --target xtensa-esp32s3-espidf`
→ **0 个错误**，且确实重新 `Compiling inkwash-note4`（不是缓存命中）。
同一份探针放进 `logic/src/datetime.rs` 的 `#[cfg(test)]` 模块（主机目标）
→ **立即 E0308**。

从 `cargo check -v` 的 rustc 命令行可以看清差别：

| unit | `--cfg test` | `--test` |
|---|---|---|
| 普通 bin | ❌ | ❌ |
| "bin … test" | ✅ | ❌ |

### 由此确认的两件事

- 那 4 个测试**不是"运行失败"，而是"从未进入编译器"**：这就是它们能在
  `EffectBatch` 换成 newtype 之后一直腐烂而无人发现的原因。
- `epd_task.rs:366-392` 与 `usb_console.rs:209-292` 的测试同样从未运行
  （它们的 `#[test]` 函数体同样不被检查），**固件侧真实测试覆盖是 0**。

### 修复（已实施）

- 删除固件侧全部 `#[cfg(test)]` 模块；把其中**有语义价值**的断言
  （`Continue` 逐条上报 / `AbortBatch` 首败即停）搬进 `logic/src/runner.rs`，真实运行。
- 在 `logic/src/lib.rs` 的契约测试里加
  `firmware_keeps_no_unrunnable_test_modules`：断言 `Cargo.toml` 仍是
  `harness = false`，且固件源码里不再出现 `#[cfg(test)]`——因为该 crate 里
  **任何 `#[test]` 函数都不会执行**，测试模块只会在文档与直觉上制造假覆盖。
- CI 增加 `firmware-format` job（`cargo +stable fmt --check`），
  这是**不需要 ESP-IDF** 的固件侧门禁。

### 仍然成立的老建议

真想要"固件生产代码"的编译门禁，仍需 ESP-IDF 工具链（`cargo check --target …`），
那是独立的基础设施决策；但**不要再把 `--all-targets` 当作测试代码的保险**。

---

## 7. 真机验证（第六轮）

**被测硬件**：Zectrix Note 4，`esp32s3` rev v0.2，16 MB flash，MAC `20:6e:f1:b4:7d:e4`。
**方法**：按 `README.md` 的红线用 `espflash` 烧写（DIO / 16mb / 80mhz / 本文档的
`partitions.csv`）；交互用新增的 `scripts/smoke-note4.py`。
**对照**：同一台机器上分别烧入**修复版**与**基线 `0f493d7`**（git stash 切换），
以区分"本次改动引入"与"既存"。

### 7.1 已验证通过

| 项 | 结果 |
|---|---|
| 烧写 | ✅ bootloader + 分区表 + app；app **2,641,680 / 4,194,304 字节 = 62.98%**（基线 2,637,232 = 62.88%，本次改动 +4.4 KB flash，RAM 无实质变化） |
| 启动 | ✅ 无 panic / `Guru Meditation` / `assert` / 栈溢出；`Light sleep armed` |
| 控制协议（10 项） | ✅ `get_status`、`set_timezone` 接受→生效、同 `id` 幂等重放、越界时区拒绝、越界 `set_rtc` 拒绝、恢复原值 |
| **NVS 持久化跨硬复位** | ✅ 时区 480→60，`espflash reset`（等同按复位键）后仍为 **60**，随后恢复 480。证明 effect-task 的写路径在命令应答前已完成落盘（P0-1 关注的那条链） |
| 压力 | ✅ 连续 8/40 次 `set_timezone` 全部落盘；配合 60–90 s soak 无 TWDT 触发、无 panic、uptime 单调递增（无隐式复位） |
| 空闲刷新 | ✅ 90 s 内 1–2 次，**全部为 `Partial`（Clock 区域 `Rect{16,36,368,92}`）**，与基线一致 |

`smoke-note4.py` 的结果：**10/10 passed**。

### 7.2 真机新发现：P0-5（HTTPS/TLS 必然失败）

```
I (16661) inkwash_note4::wifi: Wi-Fi connected to 'Ccloude_2.4G'
E (18733) esp-tls-mbedtls: mbedtls_ssl_setup returned -0x7F00
E (18734) esp-tls: create_ssl_handle failed
E (18736) HTTP_CLIENT: Connection failed, sock < 0
W (19311) inkwash_note4::ctx: Urgent poll failed: POST …/api/sync failed to start: ESP_ERR_HTTP_CONNECT
```

`-0x7F00` 即 **`MBEDTLS_ERR_SSL_ALLOC_FAILED`**。Wi-Fi 每次都能连上，
**TLS 上下文分配必然失败**，因此 `POST /api/sync` 从未成功过；该循环每约 8 s 重试一次。

**A/B 结论：既存缺陷，与本次改动无关** —— 基线 `0f493d7` 在同一台机器上
以完全相同的方式失败（同样的 `-0x7F00`）。

后果（与本轮修复无关，但决定了设备当前的实际能力）：

- 设备**永远拿不到服务端的 alarms / todos / inbox**，只能靠 NVS 里的既有数据与手工配置；
- NTP 校时（`sync.rs` 的日常对齐）永不发生；
- 每 8 s 一次 Wi-Fi 连接 + TLS 失败的空转，直接消耗电量；
- 进而导致 **P0-2 / P1-1 / P0-3 无法在真机上被触发**（见 7.4）。

这条推翻了"设备拉结构化 JSON"这一产品前提在当前固件上的成立性，
**建议按 P0 处理**（详见 `review-findings.md` P0-5）。

### 7.3 观测到的性能特征（A/B 一致，非本次引入）

| 指标 | 基线 `0f493d7` | 修复版 |
|---|---|---|
| 30 次 `set_timezone` 的 EPD 刷新 | 72 次，**全部 Full** | 79 次，全部 Full |
| 空闲 90 s 的 EPD 刷新 | 1 次，Partial(Clock) | 2 次，Partial(Clock) |
| 单条 `set_timezone` 延迟 | ~3.9 s | ~3.6–4.0 s |

- **命令驱动的刷新全是 Full 而非 Clock 局刷**，每次 Full 约耗时 1.3 s（e-paper 物理时间），
  是命令延迟的主要来源。基线同样如此，**不是本次指纹改动的回归**；
  空闲路径的局刷行为两版一致，说明指纹收口没有引入过度刷新。
  根因未定位（`plan_render` 的 Home 分支在"仅分钟变化"时应返回 Clock 局刷，
  空闲实测也确实如此），建议单独排查。
- 曾观察到 **1/30 条命令 20 s 内无应答**（随后 `busy`），未能在复跑中重现。
  由于每 8 s 有一次失败的 urgent poll 竞争，怀疑与该路径争用有关，需单独排查。

### 7.4 因 P0-5 而**未能**在真机验证的项

| 项 | 原因 |
|---|---|
| P0-3 响铃期 `clear_alarms` 出路 | 需要一条闹钟；闹钟来自服务端 sync |
| P0-2 / P1-1 CJK 折行与越界索引 | 需要服务端下发的 CJK 待办文本 |
| BLE 配对全流程 | 未测（`CHANGELOG.md` 也把它列为缺硬件证据） |
| 同步/NTP 相关路径 | 被 P0-5 完全阻断 |

因此 P0-2 / P0-3 的修复目前仍是**主机测试 + 代码审查**级别，
**没有真机行为证据**；在 P0-5 解决之前也无法补上。

### 7.5 真机新发现：命令压力下间歇性崩溃（P0-6，**归因未定**）

在压力测试（连续 `set_timezone` + soak）中复现了**硬崩溃**，两种形态：

```
Guru Meditation Error: Core  0 panic'ed (LoadProhibited). Exception was unhandled.
EXCVADDR: 0x0000003f
Backtrace: 0x420b3858 0x420b3b80 0x42070a23 0x4207132b 0x4207479e 0x42021646

Guru Meditation Error: Core  0 panic'ed (Double exception).
PC: 0x403743c0 (_DoubleExceptionVector)   EXCVADDR: 0x3fca5bbc   |<-CORRUPTED
rst:0xc (RTC_SW_CPU_RST)
```

用 `xtensa-esp32s3-elf-addr2line` 对烧入的 ELF 符号化，第一条的调用链是：

```
RawVecInner::deallocate
  → <RawVec<u8> as Drop>::drop
    → drop_in_place::<[command_sessions::CachedReply]>
      → VecDeque<CachedReply>::truncate
        → CommandSessions::begin
          → inkwash_note4::main
```

即 **`CommandSessions::begin` 的 `cached.clear()` 在析构缓存应答时读到了损坏的堆指针**
（`EXCVADDR = 0x3f`）。第二条是双异常、回溯已 `CORRUPTED`，CPU0/CPU1 同时 dump——
是典型的**堆/栈被破坏**后的表象，而不是某处干净的空指针解引用。

#### 复现率数据（同一脚本：40 次 `set_timezone` + 60 s soak）

| 构建 | 会话数 | Guru 事件 | `task_wdt` |
|---|---|---|---|
| **基线 `0f493d7`** | 6 | **0** | 0 |
| 本次逻辑改动 + **还原** `effect_task.rs` | 6 | **1** | 0 |
| 完整修复（逻辑改动 + `effect_task` 看门狗） | 3 | **1** | 0 |
| 完整修复，更长的 60 写 / 120 s 会话 | 1 | **2** | 1 |

复现入口：`python3 scripts/smoke-note4.py --stress 40 --soak 100`
（该脚本的 `no panic / watchdog / reset during soak` 检查会因此 FAIL）。

#### 归因状态：**未能归属，且不能排除是本轮引入**

已确定的：

- **与 `effect_task` 看门狗改动无关**：把 `effect_task.rs` 还原到基线、
  保留其余改动后，崩溃**照样出现**（上表第 2 行）。
- 崩溃现场（`logic/src/command_sessions.rs`）**本轮从未修改**；`logic` 内
  没有 `unsafe`。
- 崩溃在 soak 阶段出现，且伴随**预存的 P0-5**（每 8 s 一次 TLS 分配失败）
  与大量 Wi-Fi 重连——堆破坏与该路径高度可疑。

未能排除的：

- 基线 6 次全 0，而含本轮逻辑改动的构建在 5 个会话里出现 4 次事件，
  **样本量小且事件间歇，但差异不足以用"噪声"打发**；
- 因此**不能断言这是既存缺陷**。需要更多次数、或直接在
  `mbedtls_ssl_setup` 失败路径上取证，才能定性。

#### 结论与建议

1. **在定性之前不建议把本轮改动视为可发布**——尽管崩溃出现在未修改的代码里。
2. 该崩溃**独立于本轮修复的价值**：即使把本轮改动全部回退，P0-5（TLS 失败）
   仍使设备无法同步，而这条堆破坏路径依然存在（只是当前样本里没撞上）。
3. 建议的下一步取证顺序：
   - 在 `mbedtls_ssl_setup` 失败分支前后打印内部堆的空闲量/最大连续块，
     确认失败路径是否留下不一致状态；
   - 打开 `CONFIG_HEAP_POISONING_COMPREHENSIVE` 或
     `CONFIG_ESP_MAIN_TASK_STACK_SIZE` 的栈高水位日志，区分"堆破坏"与"栈溢出"
     （双异常 + `_xt_context_save` 也符合栈溢出特征）；
   - 用 `smoke-note4.py --stress N --soak M` 提高次数，建立基线/修复两版的
     崩溃率置信区间，再决定是否需要二分定位。

### 7.6 TLS 分配失败处的堆取证（第六轮下半场）

针对 P0-5 加了临时插桩：`rust-firmware/src/heap_probe.rs` 输出 `HEAPPROBE` 行，
在 `sync.rs` 的 `fetch_and_apply` / `https_post`（TLS 建连前、建连后、请求失败后）
/ `poll_urgent` 打点，字段含 `uptime_ms`、`cmd`（控制命令计数）、`sync`（同步尝试计数）、
内部/DMA/PSRAM 的空闲量与**最大连续块**。
原始串口日志、ELF 与源码快照保存在 `logs/hw-forensics/`（该目录被 `.gitignore` 忽略，
需归档请另行拷贝）；分析见其中的 `ANALYSIS.md` 与 `MANIFEST.md`。

**采样结果（run1 150 s / run2 200 s / run3 200 s）**：

| | 观测 |
|---|---|
| `mbedtls_ssl_setup` 失败 | **70 / 70，无一成功**（15 + 27 + 28） |
| `https_post:request-ok` | **0** |
| `https_post:conn-new-failed` | 0（HTTP 客户端构造从未失败） |
| `int_free` | 24,575 – 28,731 B |
| **`int_largest`** | **7,680 – 12,288 B**（run3 全部为 7680/8704） |
| `dma_largest` | 7,168 – 12,288 B |
| `psram_free` / `psram_largest` | ≈ 8.37 MB / 8.26 MB |

生成的 sdkconfig 里：

```
CONFIG_MBEDTLS_ASYMMETRIC_CONTENT_LEN=y
CONFIG_MBEDTLS_SSL_IN_CONTENT_LEN=16384
CONFIG_MBEDTLS_SSL_OUT_CONTENT_LEN=4096
# CONFIG_MBEDTLS_DYNAMIC_BUFFER is not set
# CONFIG_MBEDTLS_EXTERNAL_MEM_ALLOC is not set
```

**证据支持的机制假设（尚非确认的根因）**：`DYNAMIC_BUFFER` 关闭 ⇒ in/out 缓冲在
`mbedtls_ssl_setup` 时按完整长度一次性分配（≈16.4 KB + 4.4 KB）；
`EXTERNAL_MEM_ALLOC` 关闭 ⇒ 走内部 RAM。实测最大连续内部块只有 7.5–12 KB，
**装不下那个约 16 KB 的连续请求**，于是 `MBEDTLS_ERR_SSL_ALLOC_FAILED`；
同时 PSRAM 的 8 MB 因未开该选项而用不上。
这与"总空闲 ~25 KB 却失败""失败率恒 100%""PSRAM 始终充沛"三点同时吻合。

> ⚠️ **边界**：本轮**没有**确认 mbedtls 实际请求的字节数与 caps（需要 hook
> `esp_mbedtls_mem_calloc` 或打开 mbedtls 分配日志），也**没有**做配置变更实验。
> 因此 **P0-5 的根因仍未确定**；A/B 依然只支持"基线也存在"。
> `DYNAMIC_BUFFER` / `EXTERNAL_MEM_ALLOC` 只是**待验证的候选修法**。

#### 第 ① 步：失败分配的实际尺寸与 caps（已确认）

从**烧入的 ELF** 反汇编 `mbedtls_ssl_setup` 直接读编译器常量（命令见
`logs/hw-forensics/MANIFEST.md`），而不是从宏手工推导：

| 次序 | 对象 | 申请字节数 | 构成 |
|---|---|---|---|
| **第 1 次（失败点）** | `ssl->in_buf` | **16,717 B**（反汇编 `0x4D + 0x4100 = 0x414D`） | `13 + 320 + 16384` |
| 第 2 次（走不到） | `ssl->out_buf` | 4,429 B（`0x4D + 0x1100`） | `13 + 320 + 4096` |

`mbedtls_ssl_setup`（`ssl_tls.c:1386`）先分配 `in_buf`，失败即 `goto error` 返回
`ALLOC_FAILED`，**所以失败的就是第 1 次、16,717 字节那次**。

**caps**：生效分支是 `CONFIG_MBEDTLS_INTERNAL_MEM_ALLOC=y`，故
`esp_mem.c:17` 的 `heap_caps_calloc(n, size, MALLOC_CAP_INTERNAL|MALLOC_CAP_8BIT)`。
`MALLOC_CAP_INTERNAL` 的定义明确要求"不得在 flash/spiram cache 关闭时消失"，
**不允许 PSRAM** —— 那 8.37 MB 不是"没用上"，而是**按 caps 不允许用**。

**失败瞬间的池状态**（HEAPPROBE 用的正是同一 caps 掩码）：

| 量 | 观测（70 次失败） | 与 16,717 B |
|---|---|---|
| `int_free` | 24,575 – 28,731 B | 总量够 |
| **`int_largest`** | **7,680 – 12,288 B** | **不够** |
| `psram_free` | ≈ 8.37 MB | caps 不允许 |

**机制层面确认**：失败是"没有足够大的**连续**内部块"，不是"内存不足"。
这解释了此前三个观测（总空闲够却失败 / 失败率恒 100% / PSRAM 始终充沛）。

> ⚠️ 这确认的是失败的**直接机制**；**没有**验证任何修法，也**没有**排除
> `esp-tls` 此前对内部堆的占用/碎片化对"最大连续块仅 7.7–12.3 KB"的贡献。
> 本轮未改配置，基线未动；未启用堆毒化/栈检测。

**P0-6 在本轮的观察**：run1 出现 1 次 `rst:0x8 (TG1WDT_SYS_RST)`，紧接在一条
`set_timezone` 回 `ok` 之后，`Saved PC` 符号化为 `xthal_save_extra_nw`
（异常现场保存例程，与更早的 `_xt_context_save` 同类）——**只说明 CPU 当时在
异常/中断入口，不能指向破坏源**；run2、run3 均无复位无崩溃。本轮**未**启用堆毒化/栈检测。

### 7.7 对照实验 ②：仅启用 `CONFIG_MBEDTLS_DYNAMIC_BUFFER=y`

单变量实验，基线配置与生成的 sdkconfig 均已留档
（`logs/hw-forensics/exp-dynamic-buffer/`）。**生成的 sdkconfig 差异只有 3 行**：

```
< # CONFIG_MBEDTLS_DYNAMIC_BUFFER is not set
> CONFIG_MBEDTLS_DYNAMIC_BUFFER=y
> # CONFIG_MBEDTLS_DYNAMIC_FREE_CONFIG_DATA is not set   ← 该选项新暴露的依赖符号，保持默认
```

即唯一行为变更是这一个变量。二进制层面也确认生效：`mbedtls_ssl_setup` 不再含
`0x414D`(16717) 常量，改为 ESP-IDF 的 `__wrap_mbedtls_ssl_setup`
（`components/mbedtls/port/dynamic/esp_ssl_tls.c:312`），**在 setup 阶段不分配
in/out 缓冲**，改用 I/O 时按实际记录长度增长（日志 TAG 变为 `Dynamic Impl`）。

**待验证假设：消除启动期大块分配、按实际记录长度分配。**
→ **机制成立，但不构成修法**。

| 指标（3 轮 × 200 s × 25 命令） | 基线插桩版 | 实验版 |
|---|---|---|
| `mbedtls_ssl_setup` 失败 | **70 / 70（100%）** | **0 / 45** |
| `https_post:request-ok` | 0 | **24** |
| `Sync fetched`（HTTPS+解码+校验全通） | 0 | **17** |
| 复位 / 崩溃 | 1 次 `rst:0x8` | **0** |

**但出现了新的失败点**（正是"后续握手/读写是否有新分配失败"要查的）：

```
I esp-x509-crt-bundle: Certificate validated     <- 握手已深入
E Dynamic Impl: alloc(4770 bytes) failed         <- 新失败点，尺寸小得多
E esp-tls-mbedtls: mbedtls_ssl_handshake returned -0x7F00
```

`alloc(4770)` ×20、`alloc(4437)` ×1。**阈值行为完全自洽**：

| 轮次 | `before-conn-new` 的 `int_largest` | 结果 |
|---|---|---|
| run1 | 4608 / 5888 / 6144 | **21/21 全失败** |
| run2 | 7680 / 12288 | **15/15 全成功** |
| run3 | 7680 / 12288 | **9/9 全成功** |

失败瞬间 `int_largest` = 4,608 < 申请 4,770 → 必失败；成功时 7,680 ≥ 4,770。

**验收判定**：

1. `mbedtls_ssl_setup` 成功：✅ 45/45。
2. 后续握手/读写出现**新的分配失败**：⚠️ **有**（21 次，handshake 阶段，~4.8 KB）。
3. HTTPS 请求成功：✅ 24/45。
4. **同步数据实际应用：⚠️ 只能部分证明**。17 次
   `Sync fetched: 0 alarms, 0 todos, 0 inbox` —— 请求-响应-解码-校验链路全通，
   且 urgent-poll 空转随之停止（说明结果被状态机处理）；
   但**服务端返回空列表**，"非空数据被应用"**未被证明**——需先在服务端放入
   至少一条 alarm/todo 再复跑。
5. 采集点堆指标：已取（原始日志在 `exp-dynamic-buffer/raw/`）。
6. 复位/崩溃：实验版 **0 次**。**这不改变 P0-6 的定性**——P0-6 独立追踪。

**结论与边界**：

- 该选项**没有消除根因**，只是把需求从 16,717 B 降到 ~4,770 B；
  `int_largest` 掉到 4,608 时照样失败。**不构成修法**。
- 另有 1 次（45 次中）`PK verify failed with error 0x4290` → 随后 `-0x3000` fatal alert。
  **更正（第七轮）：这也是分配失败，不是证书问题。**
  `esp_crt_bundle.c:164` 打印的是 `-ret`；而 `rsa.c:1264` 返回
  `MBEDTLS_ERROR_ADD(MBEDTLS_ERR_RSA_PUBLIC_FAILED, ret)`，即
  `-0x4280 + (-0x0010)` = **`-0x4290`**：
  `MBEDTLS_ERR_RSA_PUBLIC_FAILED` 叠加 **`MBEDTLS_ERR_MPI_ALLOC_FAILED`**。
  RSA 公钥路径中的 `mbedtls_mpi_read_binary` / `mbedtls_mpi_exp_mod_unsafe`
  都可能返回 `MPI_ALLOC_FAILED`。故实验版 45 次连接里
  **分配相关失败是 22 次**（21 次 `Dynamic Impl` + 1 次 RSA/MPI），不是 21 次；
  分配短缺**同样会在证书签名校验内部显现**。
- run1 的 `int_largest` 随时间单调退化（12288 → 4608，21 s → 196 s）；
  是缓慢碎片化还是"失败→churn→更碎片"的自我强化，**本轮未判定**。
- `CONFIG_MBEDTLS_DYNAMIC_BUFFER=y` **仍留在工作区**，属**未验收的实验改动**。

### 7.8 非空同步数据的端到端应用验证（第 ① 步）

前置：`POST /api/devices/:id/{alarms,todos}` 需要管理凭据 ——
`require_device_access`（`inkwash-server/src/routes.rs:540`）**明确拒绝**
设备自带 sync token（401）；本机 `.env` 的 ADMIN_TOKEN 对部署服务器也无效（401）。
故本轮由项目提供部署服务器 ADMIN_TOKEN（仅经环境变量传入，未落盘）。

**放入的标记数据**（服务器上仅一台设备 `My_Device`）：
alarm `HWVERIFY-ALARM-20260915-145342`（23:58 / Daily / enabled）、
todo `HWVERIFY-TODO-20260915-145342`（high / due 2026-09-20）。

**证据链（每环独立可查）**：

| # | 环节 | 证据 |
|---|---|---|
| 1 | HTTPS 往返 | `HEAPPROBE https_post:request-ok` ×4 |
| 2 | **响应内容非空** | `Sync fetched: 1 alarms, 1 todos, 0 inbox` @10659 ms |
| 3 | **应用 → 落盘** | NVS `alarms`/`todos` blob 与服务器 payload **逐字段一致** |
| 4 | 刷新侧（辅助） | `EPD refresh completed: Full` @12017 ms（fetch 后 1.4 s）—— **仅证明一次整屏刷新完成，不能单独证明屏幕正确显示了测试内容**；应用与持久化由第 3/5 行独立支撑 |
| 5 | **跨复位持久化** | `espflash reset` 后 NVS dump **字节完全一致** |
| 6 | 无新增分配失败 | 窗口内 `setup`/`Dynamic Impl`/RSA 分配失败均为 0 |
| 7 | 无复位/崩溃 | 窗口内 0；复位后启动 0 个崩溃标记 |

**结论**：**非空同步数据被实际应用并持久化**。
`Sync fetched` 之后继续走通了状态机应用（`Effect::ApplySyncedData`，
`app_runner.rs:109`）与 NVS 落盘，并在硬复位后保持 —— 这正是
"仅有 `Sync fetched` 不足以证明应用成功"要补上的那一环。

> ⚠️ **方法学提醒**：NVS 是**日志式**存储，旧值在被压缩前仍留在分区里，
> 所以"在裸 dump 里搜到某个 blob"**不能**证明它是当前值（首次抽取即因此混入过
> 相邻条目的字节）。本验证改用**此前不存在的唯一标记串**，旧条目不可能含有它。

> ⚠️ **残留**：为验证放入的数据仍在服务器与设备上，其中 alarm 为
> **enabled + Daily 23:58，设备每天 23:58 会响**，需清理。

## 8. 第七轮补充：`0x4290` 的更正

上一轮把实验中的 `PK verify failed with error 0x4290` 记为"与分配无关"，**这是错的**：

- `esp_crt_bundle.c:164` 打印的是 `-ret`；
- `rsa.c:1264` 返回 `MBEDTLS_ERROR_ADD(MBEDTLS_ERR_RSA_PUBLIC_FAILED, ret)`，
  即 `-0x4280 + (-0x0010)` = **`-0x4290`**
  = `MBEDTLS_ERR_RSA_PUBLIC_FAILED` 叠加 **`MBEDTLS_ERR_MPI_ALLOC_FAILED`**；
- RSA 公钥路径的 `mbedtls_mpi_read_binary` / `mbedtls_mpi_exp_mod_unsafe`
  都可返回后者。

因此该次失败**同样是分配失败**（发生在证书签名校验内部），
实验版 45 次连接中分配相关失败是 **22 次**（21 + 1），不是 21 次。
`verification.md` §7.7 与 `review-findings.md` P0-5 已同步更正。

### 7.9 对照实验 ③：仅 `CONFIG_MBEDTLS_EXTERNAL_MEM_ALLOC=y`

从**原始基线**出发（实验②的 `DYNAMIC_BUFFER` 已移除），只加一行，
把 mbedTLS 分配从内部 RAM 切到外部 RAM（PSRAM）。生成的 sdkconfig 差异 4 行，
但两行是**同一个互斥选择**（`INTERNAL_MEM_ALLOC=y` → `EXTERNAL_MEM_ALLOC=y`），
`DYNAMIC_BUFFER` 确认回到未设置 —— **单变量成立**。

二进制确认：`esp_mbedtls_mem_calloc` 的 caps 由
`0x804`(INTERNAL|8BIT) 变为 **`0x404`(SPIRAM|8BIT)**。

| 指标（3 轮 × 200 s × 25 命令） | 基线（内部） | ② DYNAMIC_BUFFER | ③ EXTERNAL_MEM_ALLOC |
|---|---|---|---|
| `mbedtls_ssl_setup` 失败 | 70 | 0 | **0** |
| `request-ok` | 0 | 24 | **40** |
| `request-failed` | 70 | 21 | **0** |
| `Sync fetched` | 0 | 17 | **29** |
| **分配失败（任何形式）** | 70 | 22 | **0** |
| Guru / 复位 | 1 次 `rst:0x8` | 0 | **1 次 Guru** |

**分配维度上这是三个配置里最干净的**（40/40 成功，
`-0x7F00`/`Dynamic Impl`/`-0x4290` 全为 0）。

> ⚠️ **P0-6 仍然发生**：`ext-run1` 一次 `Guru Meditation (LoadProhibited)`，
> `EXCVADDR=0x0c`，符号化为
> `<ctx::DeviceContext>::poll_alarm_snapshot` ← `main` —— 这是**第三个不同的崩溃落点**
> （此前为 `command_sessions.rs` 缓存析构、`_xt_context_save` 双异常）。
> `ctx.rs` 至今未被我改动（`git diff` 为空）。
> **TLS/分配本轮已 100% 成功，崩溃照样出现** —— 正好印证"不能因 TLS 恢复或短期不崩
> 就关闭 P0-6 或解除发布阻断"。

**边界**：`EXTERNAL_MEM_ALLOC` 把**所有** mbedTLS 分配搬到 PSRAM（非 DMA、更慢），
牵动面大于只调缓冲策略；本窗口仅 40 次连接，**不构成修法结论**，
也未改变内部堆的碎片化（`int_largest` 曾降到 4,608），只是 mbedTLS 不再需要它。

### 7.10 P0-6 取证第一轮：栈高水位（实验③ 固定为基线）

取证基线固定为实验③（`EXTERNAL_MEM_ALLOC=y`），ELF/sdkconfig/压力序列已冻结于
`logs/hw-forensics/p0-6-stacks/`；**本轮未改任何配置**。

插桩：各线程入口 `register_current_task(slot)` 记录自身 `TaskHandle_t`，
主循环每 10 s 采样全部已注册任务（`STACKPROBE ... hwm_free=N bytes`），
无动态分配、低频。
（不用 `uxTaskGetSystemState`：`CONFIG_FREERTOS_USE_TRACE_FACILITY` 未开，
启用它会新增配置变量；`xTaskGetHandle` 也不可行——pthread 任务默认名全是
`"pthread"`，`pthread_setname_np` 连绑定都没有。）

**结果（4 轮 × 19 采样 × 8 任务）**：

| 任务 | 栈 | 最小余量 | 占比 |
|---|---|---|---|
| main | 32 KiB | **20,184 B** | ~62% |
| sync | 16 KiB | **9,168 B** | ~56% |
| effect-task | 16 KiB | 9,256–9,320 B | ~57% |
| epd | 12 KiB | 10,548 B | ~86% |
| usb-console-rx | 12 KiB | 9,912–10,104 B | ~81% |
| audio / rtc / usb-writer | 8 KiB | 6,236–6,484 B | ~76–79% |

两次崩溃前最近一次采样的值也全部正常（main 20,184；sync 9,168；…），
且采样值不随时间下降 → **未观察到常规栈耗尽**。主任务 32 KiB 实际只用掉约 12.5 KiB。

> ⚠️ **结论收窄（第八轮更正）**：这**不能完全排除栈破坏**。
> `uxTaskGetStackHighWaterMark2` 是扫描残留的 0xa5 填充值
> （`FreeRTOS-Kernel/tasks.c:4807`），只反映"栈被写过的最深处"，
> **不检测所有越界写、错误指针或栈指针异常**。

**新证据：崩溃现场出现"栈填充字节"指针**

```
Guru Meditation (LoadProhibited)   EXCVADDR: 0xa5a5a5b1
  drop_in_place::<Repeat>
  <Vec<StoredAlarm> as Drop>::drop
  drop_in_place::<BootSnapshot>
  main
```

`0xa5` 已核实为 FreeRTOS 的 `tskSTACK_FILL_BYTE`（任务创建时填满整段栈），
而本配置**堆毒化是关闭的**，所以它与**任务栈填充图案吻合**。
（另一次 `Double exception` 的 `A0 = 0` 同属"用到坏值"。）

> ⚠️ **结论收窄（第八轮更正）**：图案吻合**不等于**证明读取了从未初始化的栈对象。
> 填充值可能经**错误复制、指针破坏或失效引用**传播；`BootSnapshot::drop`
> **仍可能只是受害位置**，不是源头。

> ⚠️ 这钉住了"坏值的特征"，但**仍未定位写入方**：还没指出是谁、在哪一步
> 把这段内存弄成这样。本轮**不排除**任何改动（含我的改动）对触发概率的影响；
> 崩溃在 4 轮里出现 1 轮，样本太小，不能比较概率。
>
> ⚠️ 本轮插桩自身出过一个缺陷（按名查找触发
> `assert failed: xTaskGetHandle tasks.c:2864 (strlen(...) < 16)`，
> 导致每 ~11.2 s 复位一次），改为自注册后消失；与 P0-6 无关。

### 7.11 P0-6 取证第二轮：全面堆毒化（唯一变量）

基线不变（实验③ + STACKPROBE2 + 同一压力序列），**唯一改动**
`CONFIG_HEAP_POISONING_COMPREHENSIVE=y`（生成的 sdkconfig 差异只有该互斥选择）。

毒化提供的判据：`MALLOC_FILL=0xce`、`FREE_FILL=0xfe`、
canary `0xABBA1234`/`0xBAAD5678` —— 与栈填充 `0xA5` 可区分。

| 轮 | min int_free | min int_largest | Guru | 毒化断言 |
|---|---|---|---|---|
| hp-run1 | 5,275 | 1,844 | 0 | 0 |
| hp-run2 | 5,199 | 2,036 | 0 | 0 |
| **hp-run3** | 5,759 | 2,036 | **1** | **0** |
| hp-run4 | 4,903 | 2,036 | 0 | 0 |
| 对照 未毒化 sp-run1–4 | 21,135–21,883 | 9,728–10,752 | 2 | — |

**hp-run3 的首个错误（@uptime 90.9 s）**：

```
Guru Meditation (Double exception)
A9 = 0xcecece01     EXCVADDR = 0xcecece00
PC = 0x403743c0 (_DoubleExceptionVector)   A0 = 0x00000000
Backtrace: 0x403743bd |<-CORRUPTED   （Core 1 在 idle）
```

- **`0xcecece00` 与 `MALLOC_FILL_PATTERN` 吻合**（`0xa5a5a5b1` 与栈填充同理），
  **但尚不能证明来源就是"未初始化堆指针"** —— 错误复制、失效引用或指针破坏
  同样可以把该模式传播到现场。
- **没有任何毒化断言触发，但这不能排除越界写或释放后使用**；
  且 canary 断言通常只定位**检测现场与受损块**，**不保证指出写坏指令**。
- 双异常掩盖了原始故障帧 → **崩溃落点仍不等于写坏位置**。

> **当前可确认的只有**：异常地址**呈现填充模式特征**。
> 未初始化读取、破坏源与责任模块**均未确定**。

其它观察：毒化把内部堆压低很多（−16 KB，这是本轮引入的环境差异）；
**崩溃与"堆最低"不相关**（hp-run4 更低却没崩）；崩溃前最后一条日志无单一主导者。
零成本静态检查：`logic/src/` **完全无 `unsafe`**；固件侧 `unsafe` 集中在 FFI
（`wifi.rs` 的 `mem::zeroed`、`sync.rs:38` 的 PSRAM 切片、`wake.rs:53` 的 `Box::from_raw`、
`epd_task.rs:245` 的 `mem::zeroed::<zectrix_epd_config_t>()`）。

> ⚠️ **边界**：毒化改变了内存水位与分配布局，**本轮崩溃未必与未毒化时同一表现**；
> 另外 3 轮未复现**不能**作为排除依据；**写入方仍未定位**；
> **不排除**任何改动对触发概率的影响。**发布继续阻断。**

### 7.12 P0-6 取证第三轮：C EPD 驱动 + Rust FFI 边界静态审查

本轮**不改设备、不改配置**（无新诊断变量）。审查
`rust-firmware/components/zectrix_epd/zectrix_epd.cc`（784 行）+ 公共头，
以及 Rust 侧 `epd_task.rs` / `canvas::pack_rect_from_frame`。详见
`logs/hw-forensics/p0-6-epd-review/REVIEW.md`。

按用户指定的六个轴逐项检查，**均未发现缺陷**：

| 轴向 | 结论 |
|---|---|
| 结构体布局 vs 函数签名 | ✅ `config_t`/`rect_t` 的 Rust 绑定与 C 定义逐字段同序一致 |
| 零初始化后必需字段 | ✅ `get_default_config` 先 `*config = {}` 再赋满全部 11 个字段 |
| 缓冲区长度与所有权 | ✅ 15000/60000 精确校验；partial 的 `(w+7)/8*h` 与 Rust `div_ceil(8)` 完全一致；`shadow` 恰好 15000；`line`≤100；`dma_buffer` 分块 ≤1024 |
| 异步期间生命周期 | ✅ 全部同步；句柄单所有者（主线程创建→移动进 epd 线程→退出时 `del`） |
| 错误路径清理 | ✅ 分配失败先释放已成功项再 `delete`；部分刷新失败置 `shadow_valid=false` 拒绝后续差分刷新 |
| 回调上下文 | ✅ 全文件无 ISR/回调，只有 `WaitBusy()` 轮询 |

> ⚠️ **覆盖边界**：只覆盖 EPD C 驱动与其 Rust 调用点。**未覆盖** vendored
> `esp-idf-hal`、esp-idf-svc/NimBLE 的 C 内部、`wifi.rs` 的 `mem::zeroed`、
> `sync.rs:38` 的 PSRAM 切片、`wake.rs:53` 的 ISR `Box::from_raw`、
> `ble_control.rs` 回调路径、`ssd2683_waveform.h`。
> **"未发现缺陷" ≠ "EPD 已排除"**。

### 7.13 P0-6 取证第四轮：`wake.rs` 与 `ble_control.rs` 静态审查

只做静态审查，设备/配置/插桩未动。详见
`logs/hw-forensics/p0-6-wake-ble-review/REVIEW.md`。

**`wake.rs`**：`into_raw`(44) 与 `from_raw`(53) 只在失败路径配对——
成功路径**故意不释放**，因为地址已注册进 GPIO ISR，且全仓库**无任何
`gpio_isr_handler_remove`**；泄漏有界（启动时 3 个唤醒脚各一次）。
注册（50）→ 失败即释放 → 成功后才 `gpio_intr_disable`(56)，顺序正确；
捕获的 `task` 是**不退出**的主任务（`main.rs:95` → `board.rs:172`）。
→ 未发现内存破坏路径。

**`ble_control.rs`**：闭包（`on_write`/`on_notify_tx`/`on_connect`/`on_disconnect`）
**只捕获 `Arc`/通道句柄/`Copy` 值，无借用或裸指针**；特征对象经
`BLE_SERVER`（静态）→ `services` → `characteristics` 持有，闭包随之存活。
`shutdown_nimble` → `deinit_full` 的顺序是**先 `nimble_port_stop()`
（读 IDF `nimble_port.c:379`：阻塞直到 host task 处理完停止事件），
之后才 `BLEServer::reset()` → `services.clear()` 释放特征与闭包**
（`ble_server.rs:232`）。

> ⚠️ **该"不存在回调访问已释放对象的窗口"的结论，明确以
> "GATT/GAP 回调全部由 NimBLE host task 派发"为前提**。该前提**未验证**
> （见下方未覆盖项）。若存在非 host task 派发路径（例如控制器侧或 ISR 上下文），
> 则 `nimble_port_stop()` 的"等 host task 停止"不足以保证释放后无回调。
notify 缓冲经 `ble_hs_mbuf_from_flat` → `os_mbuf_copyinto` **确认拷贝**；
`OsMBuf` 无 `Drop` impl → 无 double free。
→ 未发现内存破坏路径。

**与 P0-4 的边界**：`on_notify_tx` 只访问自持的邮箱与句柄，
不触碰已释放内存；其缺陷是**归属/可用性**（按 `conn_handle` 匹配导致误配、
`retired_handles` 只置位不清位）→ 属 **P0-4**，**不是** P0-6 类内存破坏。

**三条观察（均为可用性/延迟，非内存，未做真机验证）**：

| # | 位置 | 机制 | 失效对象 |
|---|---|---|---|
| A | `wake.rs:66-76` | `wait()` 的 `ulBitsToClearOnEntry=0xffffffff` 会清掉 `arm()` 与 `wait()` 之间到达的唤醒 → 最坏多睡一个空闲周期（≤1 s）；输入另有轮询路径不丢失 | 无 |
| B | `wake.rs:19-28` | 计算了 `higher_prio_woken` 但未 `portYIELD_FROM_ISR` → 切换推迟 ≤1 tick | 无 |
| C | `ble_control.rs:205-213` | 两跳生命周期投递均为**无超时阻塞**。**单独登记为"潜在死锁"，不是已找到的 P0-6 内存破坏源**：邮箱填满与停止流程能否按所述顺序交错**仍需进一步证明**（需证明"`stop` 时刻 session 邮箱恰好满 16 且消费者已进入 `shutdown_nimble`"可达） | 无 |

> ⚠️ **边界**：未覆盖 vendored `esp-idf-hal`、`sync.rs:38` PSRAM 切片、
> `wifi.rs` 的 `mem::zeroed`、`ssd2683_waveform.h`，> 以及"NimBLE 是否可能在**非 host task** 上下文派发 GATT/GAP 回调"。
> 注意：**非 host task 上下文本身并不必然使 Mutex 非法**——
> 需区分"另一个普通任务"（`std::sync::Mutex` 合法）、"ISR/中断上下文"
> （任何会阻塞的锁都不合法）与**具体锁依赖**
> （`notify_attempts`/`notify_tx_pending` 是短临界区普通锁；
> `send_lifecycle` 依赖 `Condvar` 的**无限等待**，在任何可能中断 host task 的
> 上下文里都成问题）。
> **"未发现缺陷" ≠ "已排除"**。P0-6 写入方仍未定位；发布继续阻断。

### 7.14 P0-6 取证第五轮：`sync.rs` PSRAM 缓冲生命周期审查

只做静态审查；限定为该缓冲及其调用链。详见
`logs/hw-forensics/p0-6-sync-psram/REVIEW.md`。

调用链：`sync_task::run`（sync 线程）→ `SyncCommand::SyncNow`(`sync_task.rs:157`)
→ `sync::sync_now`(`sync.rs:306`) → `fetch_and_apply`(`sync.rs:179`)：
`PsramBuffer::new(16384)`(:230) → `https_post(.., buf.as_mut_slice(), ..)`(:237)
→ `read_body_fully`(:174) → `body = &buf.as_mut_slice()[..bytes_read]`(:240)
→ `from_slice`(:243)。`SuspendForBle`/`ResumeAfterBle` 是同一单线程循环里的
独立命令，**无法中断**在途请求。

| 轴向 | 结论 |
|---|---|
| 分配尺寸与空指针检查 | ✅ `heap_caps_calloc(16384, 1, SPIRAM\|8BIT)`；`len` 为常量无乘法溢出；`NonNull::new` 失败即 `Err` 且**不构造对象**（不会 free 空指针） |
| 切片长度 | ✅ `from_raw_parts_mut(ptr, self.len)`，`len` 与 calloc 同源 |
| 初始化范围 | ✅ calloc **全量置零**；且每次调用**新分配**，无复用残留 |
| 读写边界 | ✅ `read(&mut buf[offset..])` ⇒ `offset ≤ len`；`offset == len` 时做**溢出探针**，有更多数据即报错（防静默截断）；`bytes_read` 即该返回值，故 `[..bytes_read]` 不 panic |
| 两次 `as_mut_slice()` | ✅ `https_post` 的 `&mut [u8]` **不与返回值关联**，借用于语句末终止；第二个切片**顺序**创建，非同时存活 → 非同时别名 |
| 跨线程共享 | ✅ `NonNull` 是 `!Send + !Sync`，且无手动 `impl Send/Sync` ⇒ `PsramBuffer` **自动 `!Send`**，编译期禁止跨任务移动 |
| 释放（正常/错误/取消） | ✅ RAII 恰好一次；`heap_caps_calloc` ↔ `heap_caps_free` 配对；无 `mem::forget`/`from_raw`/手动 free；**不存在取消路径**（同步命令循环 + 5 s 超时只产生 `Err`） |

**结论：该缓冲在上述八个轴向上未发现内存安全缺陷。**
一个值得记录的**正面**性质：`as_mut_slice(&mut self)` 要求 `&mut self`，
使"同时持有两个可变切片"在安全 API 层被借用检查器禁止，尽管实现用了 `unsafe`。

> ⚠️ 未覆盖（按要求不扩大）：`poll_urgent` 的栈上 `[0u8; 256]` 路径、
> `esp-tls` 内部 `read` 实现、`SyncOutcome` 后续消费方。
> **"未发现缺陷" ≠ "已排除"**；P0-6 写入方仍未定位，发布继续阻断。

### 7.15 P0-6 取证第六轮：GATT/GAP 回调派发上下文与排空保证

只做静态核对。详见 `logs/hw-forensics/p0-6-ble-dispatch/FINDINGS.md`。
目的：**验证** §7.13 停止顺序安全所依赖的前提。

**结论：前提为假 —— 存在同步、在 API 调用线程上执行的回调，且该路径在本固件可达。**

> ⚠️ **勘误**：本节曾一度改为"该路径在本固件不可达"，理由是
> `BLEServer::start()` 未被调用。**该理由错误**：`BLEAdvertising::start()`
> → `start_with_duration()`（`ble_advertising.rs:174-179`）在
> `BLE_SERVER` 已存在且 `!started` 时会**间接调用 `server.start()`**；
> 本固件 `start_initialized` 先 `get_server()`(:854) 再 `advertising.start()`(:1008)，
> 因此 `server.start()` **确实被执行**，GATT 注册与 `notify_characteristic` 填充均可达。
> 原结论（前提为假）**恢复成立**；也不应额外添加 `server.start()`。

| 回调 | 调用者链 | 上下文 |
|---|---|---|
| `on_connect` | `ble_gap_call_conn_event_cb`(:1139) ← HCI 连接完成 | host task |
| `on_disconnect` | ← `ble_gap_rx_disconn_complete`(:1936) → `ble_gap_conn_broken`(:1820) | host task |
| `on_write` | ATT 写 → `ble_gatts_chr_val_access` → `ble_gatts_val_access`(:571) → `chr_def->access_cb`(:607) | host task |
| `on_notify_tx` | **(a)** `ble_gatts_indicate_err/_tmo/_rx_rsp` → `ble_gap_notify_tx_event`(:9788)；**(b)** ⚠️ **`ble_gatts_notify_custom` 的 `done:` 无条件调用 `ble_gap_notify_tx_event`（`ble_gattc.c:5390-5393`）** | (a) host task；**(b) 同步，在调用者线程 = 我们的 BLE worker 线程** |

路径 (b) 的完整链：`BleSession::notify()`(worker 线程) →
`notify_char.lock().notify_with()` → `send_value` → `sys::ble_gatts_notify_custom`
→ C `ble_gatts_notify_custom` → `done:` → `ble_gap_notify_tx_event` → 我们的闭包。

**`nimble_port_stop()` 的保证**（`nimble_port.c:379` + `ble_hs_stop.c`）：
`ble_hs_stop` 逐连接 `ble_gap_terminate_with_conn`，由内部 GAP 监听器等待
`DISCONNECT`/`TERM_FAILURE` 归零；**并带 2 s 优雅断开超时**
（`CONFIG_BT_NIMBLE_HS_STOP_TIMEOUT_MS=2000`，超时回调走默认事件队列 = host task），
超时后**仍放行**（日志 "N connection(s) still up"）。随后 `nimble_port_stop`
再等待 host task 处理完 `ble_hs_ev_stop`。
⇒ 返回时 host task 已处理停止事件并退出循环；但**不保证**"其他线程不在回调中"。

**`BLEServer::reset()` 之后谁还持有引用**：
- `BLE_SERVER` 本身**被清空而非 drop** → `get_server()` 仍是存活空 server ✅
- `ble_gatts_svc_defs[]` **不残留**：`ble_gatts_start()` 在注册循环后**无条件**
  调用 `ble_gatts_free_svc_defs()`（`ble_gatts.c:1964`）释放数组并归零计数 ✅
- **ATT 属性表的 `arg`（= `chr->arg`，指向已释放的 Rust `Vec<ble_gatt_chr_def>`）
  在停止期间是悬垂的**（`ble_gatts.c:458-467` `chr = arg`）；host 已停止故正常不遍历，
  下次 `ble_gatts_start()` 会先重置 entry 表 → **若有非 host task 遍历路径则为 UAF，保留为未验证**
- `BleSession.notify_char` 这个 `Arc` 使该特征对象**活过** `services.clear()` ✅

> **假警报（已排除，如实记录）**：曾怀疑 `ble_gatts_add_svcs` 只存指针
> （`ble_gatts_svc_defs[n] = svcs`）、而 `ble_gatts_reset()` 在生产路径从不调用，
> 会导致第二次 BLE 会话在 `ble_gatts_start()` 的 `for (i < num_svc_defs)` 中
> 解引用上次已释放的 `svc_def`（跨会话 UAF）。
> **核对后确认不成立**：`ble_gatts_start()` 自己就调用
> `ble_gatts_free_svc_defs()`（:1964）释放数组并归零计数。**不登记为缺陷。**

**对结论的影响**：原前提作废；停止顺序安全**仍成立但换理由**——
(a) host task 侧回调由 `nimble_port_stop()` 保证结束（含 2 s 超时例外）；
(b) 唯一非 host task 的 `on_notify_tx` 同步路径与拆解**同处 BLE worker 线程且严格串行**，
不可能重叠；(c) `notify_char` 的 Arc 保证目标对象活过 `services.clear()`。

**附带观察（P0-4 家族，非内存破坏）**：同步路径使
`NotifyTxEvent` 在 `notify()` **返回前**就进入通道，而
`active.inflight = Some(..)` 在 `notify()` **返回后**才设置
（`ble_control.rs:627-638`）⇒ worker 下一轮取事件时 `inflight` 可能尚未落位；
**需确认该分支对"无 inflight 事件"的容忍度**，本轮未展开。

### 7.16 P0-4 核对：inflight 时序（已关闭）+ 意外发现"通知链不可达"

只做静态核对（范围仅"通知发送 → 事件入队 → 消费"）。详见
`logs/hw-forensics/p0-4-inflight/FINDINGS.md`。

**先接受纠正**：我上轮"事件在 `notify()` 返回前入队 ⇒ 消费时 `inflight` 未设置"的推理**有误**
——入队时刻 ≠ 消费时刻；若发送与消费同在一个 worker，且 `notify()` 返回后
**先赋值再进入下一轮**，同步回调本身不产生该窗口。

逐项核对用户列出的四种可能，**均不存在**：

| 项 | 结论 | 依据 |
|---|---|---|
| (a) 回调内消费事件/重入 worker | ❌ 无 | `on_notify_tx` 只 `take_for_callback` + `send_notify_tx`（`:174-198` **只入队**），不碰 `inflight`、不调 `poll_notify_tx` |
| (b) 第二个消费者 | ❌ 无 | `poll_notify_tx` 全文件**只有一个调用点 `:726`**；`release_notify_generation`(`:1091-1104`) 只是按 session/gen/handle **丢弃**，不是匹配消费者 |
| (c) `notify()` 返回后、赋值前的提前返回 | ❌ 无 | `:604-639` 两语句间无 `?`/`return`/`continue`/`break`/阻塞；`session` 只被同一 worker take(`:589`/`:786`)，不可能在中途变 `None`；赋值位于 `:726` 消费循环**之前** |
| (d) 失败仍产生完成事件且误配后续请求 | ❌ 否 | ①`notify()` 失败即 `quarantine`（`:1063-1069`）→ 清 armed + retire handle，不留陈旧 attempt；②`take_for_callback`(`:158-164`) 要求句柄匹配；③消费端(`:726-738`) 无 `inflight` 即丢弃、否则要求 `session_id/generation/conn_handle/event.conn_handle/attempt_id` **五字段全等**，不匹配则忽略且**不** take `inflight` |

**结论：关闭该时序疑点，不作为缺陷登记。**（仍属 P0-4 核对，**不能据此归因 P0-6**。）

> ⚠️ **勘误**：本节曾登记"通知链不可达 / `BLEServer::start()` 从未被调用"，
> **该结论错误并已撤销**（见 §7.15 勘误框）。`advertising.start()` 会间接调用
> `server.start()`，因此 GATT 注册与 `notify_characteristic` 填充均可达，
> `on_write`/`on_notify_tx` 也可达。

### 7.17 P1-14 撤销 + `Arc::get_mut_unchecked` 安全条件核对

只读源码。详见 `logs/hw-forensics/p0-4-inflight/ARC_GET_MUT_UNCHECKED.md`。

**P1-14 已撤销（我的错误）**：我漏掉了间接调用——
`BLEAdvertising::start()` → `start_with_duration()`（`ble_advertising.rs:176-179`）
在 `BLE_SERVER` 已存在且 `!started` 时会调用 `server.start()`。
本固件先 `get_server()`(`ble_control.rs:854`) 再 `advertising.start()`(`:1008`)
⇒ **`server.start()` 确实被执行**，GATT 注册与 `notify_characteristic` 填充均可达。
因此**不得**用"通知闭包不可达"修正 §7.15 —— **§7.15 原结论（前提为假：
存在同步、在 API 调用线程上执行且可达的回调）恢复成立**；也**不要**额外添加 `server.start()`。

**`Arc::get_mut_unchecked` 核对结论**（路径可达）：
- **契约原文（已核对 rust 源码 alloc/src/sync.rs）**：
  "If any other `Arc` or `Weak` pointers to the same allocation exist, then they must not be
  **dereferenced or have active borrows for the duration of the returned borrow** …"
  ⇒ **"引用计数 ≥2"本身不违反契约**（我上一版说法错误，已更正）。
- 该调用点：返回的 `&mut Mutex<..>` **当场 `as *mut`**，作为借用即刻结束；
  紧随的 `chr.lock()` 经 `Deref` 属共享借用 ⇒ **该调用本身合规**。
- **真正的风险在别处**（`ble_server.rs:142-143` 把取自 `MutexGuard` 的 `&mut`
  经 `extend_lifetime_mut` 延长为 `'static`，而 guard 在本轮迭代末即释放，
  该 `&'static mut` 又在运行期 `:335` 被解引用）。两条路径**分开论证**：
  - (a) 该 `&mut` 在 `start()` 内**立刻 `as *mut`**，未再作为引用使用；
    **但运行期每次 GATT 访问都由 `voidp_to_ref`（`utilities/mod.rs:36-38`，返回 `&mut T`）
    从该指针重新造出 `&mut Mutex<Self>`**（`ble_characteristic.rs:362`）。
  - (b) 两条路径**必须分开**：
    **路径 A（同步 `on_notify_tx`，worker 线程）——已证明**：`BleSession::notify()` 的
    `match self.notify_char.lock().notify_with(..)` 临时 `MutexGuard` **存活到整个 `match`**，
    同步回调在其内部发生 ⇒ guard 在回调期间仍被持有；而回调经 `:335` 从
    `notify_characteristic` 的 `&'static mut` 取 `&mut chr.on_notify_tx`。二者指向**同一对象**
    （同一 `Arc` 分配）⇒ **同线程上 `&mut` 与活跃 `&`/guard 并存 = 别名规则违反**。
    此结论**只依赖同线程嵌套**，无需跨线程论证。
    **路径 B（host task 的 GATT 访问）——未证明**：host task 经 `voidp_to_ref`
    造 `&mut Mutex` 再 `.lock()`，要与 worker 的 `lock()` 构成**跨线程**重叠需另行取证，
    本轮**未取得**，**保留为疑点**。
  - (c) 锁/生命周期：`arg` 路径**功能访问全部经 `Mutex::lock()`**（互斥量串行化 host task 与 worker）；
    `uuid`/`val_handle` 超出 guard 的使用只在**单线程注册期**（`notify_characteristic` 此时尚空）；
    `notify_characteristic` 绕过锁但只触碰 `on_notify_tx`，与持锁方访问的
    `value`/`handle`/`subscribed_list` **是不同字段**。
- ⇒ **路径 A 的别名规则违反已证明 = soundness 缺陷**（按用户指示：已证明别名违反即为
  soundness 缺陷，**无须**再证明数据竞争或具体误编译）；路径 B 保留为疑点。
  `val_handle`/`uuid`（`ble_service.rs:52/58`）超出 guard 的使用只在单线程注册期，
  **未见重叠**，仅列为次要项。
- **与 P0-6 的因果关系未建立**，不应据此归因。
- **可修复性**：可用 `[patch.crates-io]` 指向 vendored `esp32-nimble` 并改掉
  `extend_lifetime_mut` 用法——**是可行路径**（更正上一版"无法在本仓库修复"的过强说法），
  **本轮不实施**。**不要**为此添加 `server.start()`。

### 7.18 `wifi.rs` 静态审查：配置初始化 / 错误清理 / 重连与停止

只读源码。详见 `logs/hw-forensics/wifi-review/REVIEW.md`。
方法：**以实际 API 契约为准**（零值是否有效、必需字段是否设置、错误后能否安全停止/重试/释放）。

**两处 `mem::zeroed` —— 均非缺陷**：`wifi.rs:96` 的 `sta_config` 由
`esp_wifi_get_config` **填充**后才使用（读-改-写 PMF 补丁），失败即 `?` 返回；
`wifi.rs:119` 的 `ap_info` 仅作输出缓冲，**代码只查返回码、从不读取**。

**配置初始化**：`esp-idf-svc-0.52.1/src/wifi.rs:157-205` 的
`TryFrom<&ClientConfiguration> for wifi_sta_config_t` **逐字段显式构造** C 结构
（`scan_method`/`sort_method`/`threshold.rssi=-127`/`pmf_cfg` 等均给有效值），
必需字段 `ssid`/`password`/`threshold.authmode`（= 我们设的 `auth_method`）均已设置
⇒ 未显式设置的字段不是"裸零值"。这也解释了 `wifi.rs:95-109` 的 PMF 补丁：
库把 `pmf_cfg` 写死 `capable=false`，固件需 `capable=true`。

**错误路径**（逐失败点核对 `started`/`wifi`/`used`）：包括 `start()` 失败时
`started` 保持 false（赋值在其后）、PMF 补丁失败时 `started=true` 且驱动仍在
—— 所有分支都满足 `disconnect()` 的守卫条件（`started && wifi.is_some()`），
`self.wifi.take()` 交由 `EspWifi::drop` 反初始化
⇒ **任一错误返回后对象仍可安全停止、重试与释放**。

**重连/停止**：`disconnect()` 只断开**不 stop**（符合红线）；`suspend_for_ble` 断开后
`take()` 移出驱动以回收内 RAM 供 BLE；`resume_after_ble` 重建并 `start()`
（`start()` 失败时状态自洽）；`abort_ble_resume` 清标志；无自定义 `Drop`。

> ⚠️ **边界收窄**：以上只说 `wifi.rs` 的**本地状态**在各失败点自洽，
> **停止/释放动作委托给 `EspWifi` 的 Drop**。由于**未审 `esp-idf-svc` 的
> `EspWifi`/`Wifi` Drop 内部**（`esp_wifi_stop()`/`esp_wifi_deinit()` 的顺序与失败行为），
> **完整释放链未经核实** —— 不能宣称"释放链已验证"。

**观察（非内存缺陷）**：
- **O1** `used` 仅在完全成功时置位 ⇒ `sync.rs:295` 的"本 boot 已用过 Wi-Fi"警告可能漏报；
  `restart_for_fresh_wifi_session()` 是**死代码**。
- **O2** `set_storage(RAM)` 只在 `connect()` 调用，而 `resume_after_ble` 直接 `start()`
  ⇒ 重建的驱动在下次 `connect()` 前处于**默认存储模式**（本工程
  `CONFIG_ESP_WIFI_NVS_ENABLED=y` ⇒ 默认 FLASH）。当前窗口内不 `set_config`、不落盘，
  **良性**，但属**潜在**的配置落 NVS 路径；更稳可把 `set_storage(RAM)` 移入 `ensure_driver()`。
- **O3** `ensure_driver()` 在长度校验之前执行 ⇒ 失败连接会保留驱动及其内 RAM，
  直到 suspend/drop。属**资源占用**（与 P0-5 内 RAM 压力相关），**非泄漏**。
- **O4** `Peripherals::steal()` 两处（`wifi.rs:44` modem / `board.rs:276` gpio4）
  取用资源**互不相交** ⇒ 未构成同一外设的重复所有权。

> ⚠️ **未覆盖**：`esp-idf-svc` 的 `EspWifi`/`Wifi` Drop 内部（`esp_wifi_stop`/`deinit` 顺序）、
> `esp_netif`/DHCP 事件处理、`EspSntp` 创建/销毁语义、`esp-idf-hal` 补丁。
> **"未发现缺陷" ≠ "已排除"**；P0-6 写入方仍未定位，发布继续阻断。

### 7.19 vendored `esp-idf-hal` 补丁审查（静态路线收尾轮）

只读源码；范围限定为"相对上游的差异及其直接调用链"。详见
`logs/hw-forensics/hal-patch-review/REVIEW.md`。

**补丁形态**：`[patch.crates-io] esp-idf-hal = { path = "../vendor/esp-idf-hal" }`，
vendored 版本 0.46.2。与同版本上游比对：**仅 `src/sd.rs` 有差异，共 6 行**，
`build.rs`/`Cargo.toml` 完全一致。差异是在两处 `sdmmc_host_t` 结构体字面量中，
为 IDF ≥5.5.5 新增字段补初值：
```rust
#[cfg(esp_idf_version_at_least_5_5_5)]
unaligned_multi_block_rw_max_chunk_size: 0,
```
（`esp_idf_version_at_least_5_5_5` 为 `esp-idf-sys` 构建脚本真实发出的 cfg，已确认。）

**六项优先项**：指针/缓冲区生命周期、ISR 注册与注销、跨线程访问、外设所有权、
错误清理 —— **均未改变**（差异只是一个 `size_t` 整型字段的初值；不含任何
`isr`/锁/`Send`/`Sync`/take-drop/错误分支改动）。

**可达性：不可达**。`grep` 显示本固件**完全不使用 SD/SDMMC**；
`esp-idf-svc` 仅启用 `critical-section` feature（**未启 `sd`**）
⇒ 该结构体构造没有可达路径，`0` 不会进入 C 驱动。

**取值 `0` 本身合法且保守**（记录，非缺陷）：IDF 文档明确
"Set to 0 to use the default value of 1 (single-block transfers)"，
且 `sdmmc_cmd.c:20-24` 的 `get_chunk_size()` 对 0 有**显式守卫**
⇒ 不会除零/越界。但它与 `SDMMC_HOST_DEFAULT()` 的 **16** 不同
⇒ **若将来启用 SD**，非对齐多块传输块数由 16 降为 1（吞吐降、DMA 临时缓冲更小）。
**当前无影响**。

⇒ **未发现改动；按约定结束本阶段静态审查。**

### 7.20 BLE 别名缺陷与 P0-6 的经验解耦

按用户要求核对"崩溃轮是否实际进入过 BLE 会话、执行过通知路径"。
对**全部 5 个崩溃轮**（`sp-run4`、`hp-run3`、`ext-run1`、`stack-run1/2`）：

| 检查 | 结果 |
|---|---|
| `BLE preflight` / `BLE advertising started` / `BLE worker thread starting` / `BLE worker: Start session` / `BLE client connected` | **均 0 次** |
| 词边界 `\bBLE\b\|NimBLE` 行数 | **0** |
| STACKPROBE 中 `task=ble hwm_free=`（自注册版**真实注册**判据） | **从未出现** |

⇒ **在"日志完整、且相关日志必定输出"这一前提下，未观察到 BLE 执行**；
因此**即使 §7.17 的 BLE 别名 soundness 缺陷独立成立，也不能解释已观察到的 P0-6 崩溃**。
（注意：这是"未观察到"，不是"路径绝未执行"——后者仍以日志完整性与
相关日志必然输出为前提；`BleSession::start` 的 `BLE preflight` 等入口日志
若因任何原因未输出，该结论即受影响。）
该缺陷作为独立的依赖内部问题另行登记。

> ⚠️ 本轮我自己的两个量测错误（已更正）：`grep -c "BLE"` 会命中
> `ENABLED`/`DISABLED`（假阳性 35–70 行）；`grep "task=ble"` 会命中按名查找版的
> `task=ble absent`（32 次）而误示"已注册"。正确判据是词边界匹配与
> `task=ble hwm_free=`。

### 7.21 静态审查阶段收尾
`wifi.rs`（§7.18）、`esp-idf-hal` 补丁（§7.19）均未发现新的内存安全缺陷；
BLE 别名问题与 P0-6 经验解耦（§7.20）。**P0-6 写入方仍未定位。**
按用户指示：**暂不延长无定点目标的压力测试，也不混入依赖修复。发布继续阻断。**

### 7.22 P0-6 原始现场捕获：准备核查（未刷机）

只读源码与 ELF，**未刷机、未改配置、未跑压力**。详见
`logs/hw-forensics/p0-6-capture-prep/PREP.md`。

**当前配置**：`PANIC_PRINT_REBOOT`；**无** GDB stub；**core dump = NONE**；
分区表**无 `coredump` 分区**（`storage` 恰好到 `0x1000000`，无空余）。

**当前双异常路径保存/丢失（读汇编确认）**：
`xtensa_vectors.S:586` 的双异常向量把 `a0` 写入 `EXCCAUSE` 并 `call0 _xt_panic`；
`panic_handler_asm.S` 的 `_xt_panic` 构造 frame 时
**`frame.PC ← EPC_1`（现场读）**、`frame.A0 ← EXCSAVE_1`，
再由 `_xt_context_save` 存 `a2..a15`/`SAR`/`LBEG/LEND/LCOUNT`，最后存 `EXCCAUSE`/`EXCVADDR`。
⇒ **打印已含 frame 的全部内容**；我们日志里 `PC = 0x403743c0 = _DoubleExceptionVector`
说明 **`EPC_1` 已被硬件覆盖**，原始 PC 只可能还在**未被打印的特殊寄存器**
（`EPC_2..EPC_7`/`EPS_*`/`EXCSAVE_*`）里。`EXCCAUSE` 被覆盖为伪因；
`EXCVADDR` 可能保留原始数据故障地址（日志 `0xcecece00`、`A9=0xcecece01`）。

**候选方式 × 双异常覆盖性**：`panic.c` 主流程对任何 panic 都调用
`esp_core_dump_write(info)` 与（若启用）`esp_gdbstub_panic_handler(info->frame)`，
`esp_core_dump_write` 仅在 `TO_UART && SILENT_REBOOT` 时提前返回（我们不满足）
⇒ **两种方式都覆盖双异常路径**。FLASH 转储需新增分区（**触碰刷写红线，非单项变更**）。

**建议（单项）**：`CONFIG_ESP_COREDUMP_ENABLE_TO_UART=y`。理由：不动分区表、
双异常路径无守卫必执行、额外捕获 **`EPC_1..EPC_7`/`EPS_2..EPS_7`（现场读，
每条自带 `reg_index`，可按索引精确定位）**与**全部任务栈**、且全自动。
UART 模式下 `ESP_COREDUMP_PRINT = esp_rom_printf` → 走 USJ 控制台，默认格式为 ELF。

> ⚠️ **不确定性与风险（刷机前记录）**：原始 PC 是否落在 `EPC_2..7`/`EPS_*` 中，
> **本轮无法静态断定**；core dump 的价值正是把"原始 PC 是否还在"变成可观测。
> 若仍无所获，则须转 gdbstub（**本机未装 xtensa gdb**；但其传输层支持 USJ，
> 见 `gdbstub_transport.c`）或停机 + openocd（**openocd 已装**）。
> 另：panic 路径把 RTC WDT 设为 10 s，而本工程任务栈合计约 120 KiB（base64 ≈170 KB），
> **若 UART 输出慢于 ~17 KB/s，转储会被 WDT 截断**——这是唯一需先观测的量。
> 解码需要 `xtensa-esp-elf-gdb` 或自写解析器，两者**本轮均未实施**。

### 7.23 P0-6 原始现场捕获：core dump(UART) 第一份完整转储

**离线解码链路已建并自测通过**（`selftest_coredump.py` 18/18 PASS），覆盖：
固件的**分块 base64**（每 48 B 独立编码 ⇒ 中途出现 `=` 填充，**不能整体拼接解码**；
按上游 `loader.py:693` 的逐行解码修正）、`core_dump_header_t` 前导（本机 24 B，
用 `\x7fELF` 定位而非依赖版本号）、**完整 vs 截断**判定、`extra_info` 的
`(reg_index, reg_val)` 对与 `EPC1..7/EPS2..7` 定位、以及从 TCB 取任务名
（ELF 模式无 `TASK_INFO` note）。

**单变量**：`CONFIG_ESP_COREDUMP_ENABLE_TO_UART=y`。生成配置连带 35 行，
**两处非显然**：`CONFIG_FREERTOS_TASK_FUNCTION_WRAPPER=y`（新启用）、
`CONFIG_FREERTOS_ISR_STACKSIZE` 1536→2096（**+560 B 内 RAM**）。
基线 ELF `5b7cf81f`、实验 ELF `89e04bd5`（+20 KB）均已留档。

**复现**（原压力序列、串口原样采集、未插诊断命令）：`cd-run2` 得到
**完整转储**（START=1/END=1；34,460 B = 24 B 前导 + 34,436 B ELF + CRC32；
`ET_CORE`/`EM_XTENSA`/34 个 program header；16×`CORE` + `ESP_EXTRA_INFO`）。

**取回的**：**崩溃任务 = `main`**（`crashed_task_tcb=0x3fcb5be0` 处的 TCB
其 `pcTaskName` = `main`）；完整任务表（`main`/`IDLE0`/`IDLE1`/`ipc0`/`ipc1`/
`sys_evt`/`esp_timer`/`wifi` + **7×`pthread`**，与"pthread 名字不外传"一致）。

**❌ 未取回（关键负面结论）**：`extra_regs[16]` **全 0**（EPC1..7/EPS 一个都没填）、
`exccause=0xFFFF`（= `COREDUMP_INVALID_CAUSE_VALUE`）、`excvaddr=0`、
崩溃任务的 `CORE` note 为**假帧**（`pc=0x20000000` = `COREDUMP_FAKE_STACK_START`）、
段中**找不到** `0xdeadbeef`（`COREDUMP_CURR_TASK_MARKER`）且故障时刻的栈/寄存器
（`sp=0x3fcbb380`、`A0=0x3fcb3960`）**无任何段覆盖**。
⇒ **本轮拿不到原始故障 PC 与故障指令**；**用户预设的两点注意事项被实测证实**：
完整写出转储 ≠ 保存原始现场，额外 EPC 寄存器并未被填充。

本轮 panic 报 `Unhandled debug exception / BREAK instr`、`PC=0x403743c0`；
该地址是 `_DoubleExceptionVector`，其首条指令即 `break 1,4` ⇒ **底下仍是双异常**。

**下一步（定点）**：先查清"为何崩溃任务的帧/栈未被转储"——这正是 `extra_regs` 为空的
同一原因（`core_dump_port.c:206` 的 marker 分支未命中，未调用
`esp_core_dump_get_epc_regs/eps_regs`）。**先定位此因，再决定是否换捕获方式**
（`PRINT_HALT`+openocd 读 EPCn，或 gdbstub——后两者同样在 panic 阶段读寄存器，
**未必能找回已被硬件覆盖的状态**）。**P0-6 仍未归因；发布继续阻断。**

### 7.24 崩溃任务为何被替换为假栈/假帧（离线追踪，已闭合）

**未刷机、未换捕获方式**，由现有转储 + 匹配 ELF + IDF 源码推出。详见
`logs/hw-forensics/p0-6-coredump/FAKE_STACK_TRACE.md`。

**尺寸账目（消除歧义）**：`34,460 = 24 (core_dump_header_t) + 34,432 (ELF 本体) + 4 (CRC32)`。
依据：program header 覆盖的最大 `offset+filesz` = 34,432；尾部 4 字节 = `0xaa74b994`
= 日志 `Coredump checksum`。⇒ **CRC32 在 34,460 之内**；无歧义判据为
`preamble + elf_extent + 4 == raw_len` 且尾部 u32 == 日志 checksum。

**关键地址**：`s_exc_frame = frame.a1 − XT_STK_FRMSZ`
= `0x3fcbb380 − 0x90` = **`0x3fcbb2f0`**（`XtExcFrameSize=0x70` 取自 ELF 符号）；
main 的 `pxEndOfStack` 从转储 TCB 偏移 +72 读出 = **`0x3fcb55d0`**
（偏移正确性独立验证：16 个 TCB 均满足 `pxTopOfStack ∈ [pxStack(+48), pxEndOfStack(+72)]`
且区间长度与配置栈大小吻合）。

**失败条件（逐条代入 `esp_core_dump_check_stack`）**：
①`esp_stack_ptr_is_sane(0x3fcbb2f0)` 通过（在 DRAM）；
②`stack_end` 合法性通过；
**③`stack_start >= stack_end` 失败**（`0x3fcbb2f0 ≥ 0x3fcb55d0`，高出 **+0x5D20 = 23,840 B**）；
④无符号差 `0xFFFFA2E0` > 64 KiB（③已决定）。
⇒ **失败在"地址范围/栈边界"，不是对齐、不是 TCB 合法性**。
稳健性：要让③通过需 `XT_STK_FRMSZ > 23,984 B`，实际仅 `0x90` ⇒ 与常数精度无关。

**顺序（已闭合）**：`port_init` 先把 `frame->exit` 写成 marker `0xdeadbeef`
并把 `exccause` 初始化为 `0xFFFF` → 逐任务时当前任务的 `stack_start` 被设为
`s_exc_frame` → `check_task` 因③失败而**把 stack_start/end 换成假栈 `0x20000000`
并 return true** → 因此 `crashed_task_tcb` 仍被设置（TCB 正确）
→ 但 `get_regs_from_stack` 读到的是**假帧**的 `exit` ≠ marker
⇒ **marker 分支从不执行 ⇒ `extra_regs` 全 0、`exccause=0xFFFF`、`excvaddr=0`**。

> ✅ **"假帧与 extra_regs 为空是同一原因"已由实际分支证明**，根因唯一且明确：
> `esp_core_dump_check_stack()` 在 **`stack_start >= stack_end`** 上失败 → 假栈替换。

**附带发现（实测，不构成归因）**：故障时刻 `sp = 0x3fcbb380`
**不在 main 栈内**（高出栈顶 23,840 B）、**不在任何任务栈内**（16 个 TCB 逐一比对）、
**不在中断栈上**（`port_IntStack @0x3fca1810`，2096 B），而位于 `_heap_start` 之上的**堆区**。

**缺失项（明确列出）**：①首个异常状态（本转储按构造无法提供）；
②栈指针为何在栈外（因/果未定）；③`0x3fcbb2f0` 归属
（转储只含 16 TCB + 16 栈段，`CAPTURE_DRAM` 未启用，无法排除被跳过的 broken 任务）。

**最小插桩建议（未实施）**：`ESP_COREDUMP_LOG_PROCESS ≡ ESP_COREDUMP_LOGD`（DEBUG），
而 `CONFIG_LOG_MAXIMUM_LEVEL=3`(INFO) 使其被编译掉——故日志无
`stack is corrupted (...)`. 把日志级别提到 DEBUG 即可用**运行时值**直接确认上述推导，
属单项改动、不动捕获方式与压力序列。

### 7.25 缺失项 3 闭合：转储任务集合的完整性（离线）

详见 `logs/hw-forensics/p0-6-coredump/TASK_SET_COMPLETE.md`。

**结论：16 个任务是完整的，没有任务被跳过。**

**证明（状态链表闭合）**：FreeRTOS 中每个任务恒处于且仅处于一个状态链表。从已转储 TCB
沿 `xStateListItem.pxNext`（TCB 偏移 **+8**）走链，得到 **5 条链覆盖 16/16 个 TCB**，
且**全部终止于 `0x3fca5xxx` 静态链表区的链表头，无悬挂链接**。
（`xEventListItem`(+28) 链**不能**用作判据：任务被 `uxListRemove` 摘除后
`pxNext/pxPrevious` 不清零，仍指向队列内部的等待链头——观察到的 9 条"event 悬挂"
全是队列对象地址。）

**独立证据**：对每个 TCB 计算 `[pxTopOfStack(+0), pxEndOfStack(+72))` 与转储段匹配，
**15/16 逐字节吻合**（672×5、736、752、768、1104×2、1552、1600、1776、1808、3168），
**唯一不匹配的是 `main`**（期望 8032 B），其栈段被 **112 B 假栈 `0x20000000`** 取代
⇒ 与 §7.24 的假栈替换机制完全一致，并再次独立验证了 TCB 偏移假定。

**"被跳过任务的栈范围"不可恢复**：转储只含 TCB 段与栈段；本构建中
`_coredump_dram_start == _coredump_dram_end`（`0x3fca2d28`）、
`_coredump_iram_start == _coredump_iram_end`（`0x4038ec00`）**均为空**
⇒ UART/ELF 转储按设计不含 DRAM/IRAM 段，被跳过的 TCB 内容不在其中（本次无此情形）。

**表述精确化（采纳）**：
- 已闭合的是"**现场被替换为假栈的机制**"，**不是 P0-6 根因**。
- `s_exc_frame=0x3fcbb2f0` 高出 `stack_end` **23,840 B (0x5D20)**；`sp=0x3fcbb380`
  高出 **23,984 B (0x5DB0)**；差 144 = `XT_STK_FRMSZ` ✓ 自洽。
  **只确认它不在任何任务栈内**；**"高于 `_heap_start`"不能证明它是有效堆分配**。

**归属问题仍缺**：该地址的**内容与堆元数据**（不在转储内，且 `CAPTURE_DRAM` 亦无济于事）；
以及**首个异常的状态**（决定 sp 在栈外是因还是果）。**不换捕获方式则无法完成归属判定。**

**附带观察**：名为 `tiT` 的任务 = lwIP TCP/IP 任务（`lwipopts.h:849`
`TCPIP_THREAD_NAME "tiT"`），其 `pxStack = 0x600fe1e8`（**PSRAM**，~3.5 KB），
与 `CONFIG_SPIRAM_ALLOW_STACK_EXTERNAL_MEMORY=y` +
`CONFIG_FREERTOS_TASK_CREATE_ALLOW_EXT_MEM=y` 一致 ⇒ **HTTPS/lwIP 协议栈运行在 PSRAM 栈上**。

### 7.26 两处更正 + PRINT_HALT/OpenOCD 路线准备（未刷机、未 attach）

**更正 1（采纳）**：`0x600fe1e8` 是 **RTC fast RAM**，不是 PSRAM。
`soc.h`：`SOC_RTC_IRAM/DRAM_LOW=0x600FE000`、`HIGH=0x60100000`；PSRAM 映射为
`0x3C000000–0x3E000000`（转储中**没有**任何段落在该区间，只有 `0x600fecf0+752`）。
故 §7.25 末"lwIP 使用 PSRAM 栈"**不成立**，应为：**lwIP TCP/IP 任务（`tiT`）的栈位于
RTC fast RAM**，与 `CONFIG_ESP_SYSTEM_ALLOW_RTC_FAST_MEM_AS_HEAP=y` +
`CONFIG_FREERTOS_TASK_CREATE_ALLOW_EXT_MEM=y` 一致。

**更正 2（采纳）**：从 16 个已转储 TCB 走到链表头**不能**证明任务全集完整——
无法排除另一条未被访问的非空状态链表；15 个栈段吻合只验证了**已收录**对象。
要证明完整，必须独立读取**所有状态链表头**及其 `uxNumberOfItems` 与
**`uxCurrentNumberOfTasks`**（均在 `.bss`，不在转储内）⇒ 归入下述路线。

**路线准备（详见 `logs/hw-forensics/p0-6-coredump/PREP_HALT_OPENOCD.md`）**：
- OpenOCD `v0.12.0-esp32-20260424` 已装；`board/esp32s3-builtin.cfg`，`_ONLYCPU 0x03` **双核**；
  目标脚本暴露 **`epc1..7`/`eps2..7`/`exccause`/`excvaddr`/`excsave1..7`** ⇒ 双核特殊寄存器可读。
- **不复位 attach 可行，但必须走 OpenOCD CLI、禁用 GDB**：唯一 `reset halt` 分支在
  `gdb-attach` 事件（`esp_common.cfg:321`），条件 `_FLASH_SIZE!=0 && _ESP_MEMPROT_IS_ENABLED`，
  而本工程 `CONFIG_ESP_SYSTEM_MEMPROT_FEATURE=y`（+`_LOCK`）⇒ **GDB attach 会复位毁现场**；
  `init` 路径不触发；`reset init` 只属 `program_esp` 刷写流程。
- **看门狗允许保留现场**：`PANIC_PRINT_HALT` 分支停机前 `disable_all_wdts()`，
  **显式关闭 RTC WDT**（`panic.c:254-262`）⇒ panic 入口的 10 s RTC WDT 不会复位。
- 任务表/现场全局符号齐备：`pxReadyTasksLists @0x3fca5ee8`、`pxDelayedTaskList @0x3fca5ebc`、
  `pxOverflowDelayedTaskList @0x3fca5eb8`、`xSuspendedTaskList @0x3fca5e64`、
  `xPendingReadyList @0x3fca5e90`、`uxCurrentNumberOfTasks @0x3fca5e60`、
  `g_exc_frames @0x3fca5b34`、`s_exc_frame @0x3fca77c8` ⇒ **无需 gdb**，`mdw`+`reg` 即可。
- 副产物：转储的 5 个容器对上名字 = `xSuspendedTaskList`(7)/`pxReadyTasksLists[0]`(IDLE0,1)/
  `pxReadyTasksLists[1]`(main)/两个 delayed 链表 ⇒ 解释了"走链看似闭合"。
- **风险/未落实**：①OpenOCD 经 libusb 独占 `0x303a:0x1001`（与控制台同一设备），
  与 `/dev/cu.usbmodem1101` 采集**争用未验证**，方案是停机后先关采集再 attach；
  ②`halt` 停在 panic 停机循环 ⇒ 读到的是 panic 期寄存器，**只能补内存/链表/堆，补不了 EPC1**。
- 需一次单项改动 `CONFIG_ESP_SYSTEM_PANIC_PRINT_HALT=y`（**本轮未实施**）。

### 7.27 OpenOCD 连接可用性验证（未改配置、未跑压力）

详见 `logs/hw-forensics/p0-6-coredump/OPENOCD_CONNECTIVITY.md`。

| 验收项 | 结果 |
|---|---|
| 能连接 | ✅ 适配器 `20:6E:F1:B4:7D:E4`（本机 MAC）；两 TAP `0x120034e5`；**两核 Examination succeed** |
| 串口/JTAG 互斥？ | ✅ **实测可共存**——串口句柄保持打开期间 OpenOCD 全程可 `init`/`halt`/`reg`/`mdw`/`resume`/`shutdown` |
| 读两核状态 | ✅ cpu0 `pc=0x4037CA1A ps=0x00060120 a1=0x3FCB6B60`；cpu1 `pc=0x4037CA1A ps=0x00060320 a1=0x3FCB7DC0`（idle） |
| 读已知静态符号 | ✅ `uxCurrentNumberOfTasks@0x3FCA5E60=0x10`（**16**）；`esp_app_desc@0x3C1A0020` magic **`0xABCD5432`** ✓（证明是本 ELF 的运行镜像） |
| 特殊寄存器 | ✅ `epc1=0x4218EEDA`/`exccause=0x4`/`excvaddr=0x0` —— **正是 core dump 未提供的那组** |
| 正常退出 | ✅ `RESUME ok`+`shutdown`，rc=0 |
| **无复位** | ❌ **未通过** |
| 退出后设备应答 | ✅ `get_status` 正常（timezone=480） |

**"无复位"未通过（2/2 复现）**：两次"运行中 attach + halt"都干扰固件——第一次在
**OpenOCD 启动后约 5 s（halt 期间）**出现 `panic'ed (Cache error)`（文本被随后复位截断，
应为 "Cache disabled but cached memory region accessed"）；两次串口 CDC 均重新枚举
（`Errno 6`）。当时设备**空闲且健康**（uptime 53,422→64,542、power state 正常、
STACKPROBE 余量正常、刚 `Light sleep armed`），且**该签名在约 17 轮压力测试中从未出现**
⇒ 按现有证据应视为**调试器 attach 引入的干扰**，不是 P0-6 观测。
**对计划的含义**："先 panic 停机、再 attach"的顺序**不只是为保留现场，而是必需**。

**复位原因读数**：`(21) USB UART reset`（我自己的 DTR/RTS 复位残留）→ `(12) Software CPU reset`
（halt 自带 `Core was reset`）→ `(16) RTC WDT`（首次会话末尾 Cache error panic 后由 RTC WDT 复位）。

**⚠️ 方法论更正**：实测同一设备连续两次采样——**不触碰 DTR/RTS**：0 启动横幅、uptime 36,210→43,259；
**设 `p.dtr=False; p.rts=False`**：立刻出现启动横幅、uptime 从 **84 ms** 起。
⇒ 在 ESP32-S3 内置 USB-Serial-JTAG 上设置 DTR/RTS 会**复位芯片**，而**我归档的全部脚本都这么做**。
**影响**：脚本在 `open()` 后调用 `reset_input_buffer()` 冲掉了启动横幅，故**捕获窗口内看不到**该复位；
每轮实为"全新启动+压力"，**各配置条件一致 ⇒ 相对比较仍成立**，`boots=N` 计数亦仍为窗口内真实事件；
**但绝对"连续运行时长"说法不成立**，今后证明"未复位"必须不触碰 DTR/RTS。

**实操要点**：TCL `catch {reg}`/`catch {mdw}` 会把输出**作为结果捕获而不打印**（须 `echo $r`）；
`echo {…$var…}` 的花括号**阻止变量替换**；`mdw`/`reg` **必须 halt**；拒绝 gdb attach（`MEMPROT_FEATURE=y` ⇒ `reset halt`）。

### 7.28 第一份有效保留现场（`PANIC_PRINT_HALT` + OpenOCD 只读）

详见 `logs/hw-forensics/p0-6-halt/SCENE.md`。

**单项改动**：`CONFIG_ESP_SYSTEM_PANIC_PRINT_HALT=y`（生成配置差异 5 行）。
基线 ELF `89e04bd5` → 实验 ELF **`8b555b1f`** 已归档；coredump 保持开启。
采集脚本 `capture_stress.py`：**不触碰 DTR/RTS、不清空输入缓冲、从 open 起存原始字节**，
命令序列与冻结版逐条相同。**已验证打开串口不复位**（0 横幅，uptime 32,024→46,686 连续）。

> ⚠️ **方法论更正（历史实验表述）**：归档的全部串口脚本都设 `p.dtr/p.rts=False`，
> 实测这会**触发芯片复位**；脚本随后 `reset_input_buffer()` 把横幅冲掉，故窗口内看不到。
> **历史实验统一标为"冷启动后的观测窗口"**：条件一致支持继续参考配置对照，
> 但**不能自动保证**相对比较不受复位、启动日志丢失、重连差异影响。

**故障位置**：`run1` 干净跑完（uptime 到 209,375）后结束，`run2` 一开端口只读到 27 字节标记、
0 字节设备输出 ⇒ **崩溃发生在两轮之间的空档**，panic 文本与 coredump 输出**均未采到**；
`PRINT_HALT` 把现场留在芯片内，这是本轮能取证的前提。

**读取结果（OpenOCD CLI；禁 reset/GDB/写内存；读需 `halt`）**：
- 两核 PC = `0x4218F330`(**`esp_panic_handler`**) / `0x420BF6F0`(**`panic_handler`**) ⇒ 确认 panic 现场
- core0：`exccause=0x2`（**双异常伪因**）、**`excvaddr=0xCECECE00`**、`epc1=0x42109B46`、`excsave1=0`；
  `g_panic_entry_count=1`
- **异常帧在原地且完整**：`g_exc_frames[0]=s_exc_frame=0x3FCB3740`，`exit=0xDEADBEEF`（marker ✓）、
  `pc=0x403743C0`（向量）、**`a1=0x3FCB3800`**、`a9=0xCECECE01`
- **任务集合权威判据**：`uxCurrentNumberOfTasks=16`；`xSuspendedTaskList=7`、
  `pxDelayedTaskList=5`、`ready[0]=2`、`ready[1]=1`、`ready[5]=1`，`xPendingReadyList`/溢出延时链表
  **为空**、`xTasksWaitingTermination` 空、`uxDeletedTasksWaitingCleanUp=0`
  ⇒ **7+5+2+1+1 = 16 ✓ 任务集合完整、无跳过**（不再依赖"从已转储 TCB 走链"）
- **归属（本轮目标）**：按**现场指针**读 16 个 TCB，**`sp=0x3FCB3800` 落在 `main` 的栈内**
  （`0x3FCAD3DC..0x3FCB55D0`）且 **main 是唯一所有者**；不在中断栈内。
  **与上轮对比**：上轮帧在**所有任务栈之外**（→ 假栈替换 → 转储丢现场），本轮帧在 main 栈内
  ⇒ **跨启动布局变化，必须用当轮现场指针**（用户此前的坚持得到验证）；"假栈"问题**与具体崩溃相关**。
- **调用链**（main 栈内 app 返回地址）：**`main_task` → `inkwash_note4::main` → `usleep`**，
  被解引用值为 **`0xCECECE00`（MALLOC 填充）**；帧内 `a8=0x3FCB38C0`，其 +4 正是 `0x420C4E04`(usleep)。

**现场稳定性**：无 `Core was reset`；帧内容在会话 A/E **完全一致**、两核 PC 一致、
`g_panic_entry_count` 恒为 1 ⇒ **本次 attach 未改变现场，可作为完整保留现场证据** ✓
⚠️ 但 **OpenOCD 退出时会自动 resume**（下次 `init` 报 `running`），"禁止 resume"在工具层无法完全遵守；
因固件仅在 panic 停机循环空转，现场经 A/E 对比未被改变。

**仍未取得（限制）**：①**原始故障指令**——`EPC1` 仍是向量，说明"先停机再 attach"**同样找不回
被硬件覆盖的 EPC1**（与用户判断一致）；②**堆块归属**——`excvaddr=0xCECECE00` 是**值**而非有效堆地址，
缺"包含该值的堆块地址"就无法用分配器元数据定位，本项**未完成**；③`usleep` 归属来自**栈内返回地址**，
不等于故障指令在其中。

**设备当前处于 panic 停机，需复位才能恢复服务（本轮未复位）。P0-6 归因未定；发布继续阻断。**

### 7.29 `main → usleep` 调用链核对（离线；设备保持停机，未 attach/未复位）

详见 `logs/hw-forensics/p0-6-halt/CALLSITE_REVIEW.md`。

**⚠️ 本节 v1 的两处判断已被用户实测推翻并更正（v2）**：
① **不是"缺少 S3 ISA 配置"**——把 ELF **临时副本**的 `.xt.prop`/`.xt.lit` 去掉后，
**原有 S3 objdump 即可正常解码 `usleep`**，`.flash.text` 字节与原件一致；障碍与这两个元数据节有关。
② **`usleep` 并非"组 timespec 后调 `nanosleep`"**——本 ELF 经 `l32r`+`callx8` 调 **`vTaskDelay`**。
另：用户披露 `objcopy` 曾意外重写归档 ELF，已从构建目录恢复；我核对**归档副本与构建原件 SHA256
均为 `8b555b1f73873e40955dd162f1b8da900ee10dc7c2e1f9eba63e55c0e60e3b3a`** ✓，
且本轮只在 `/tmp` 临时副本上剥离节（归档未动）。

**指令级确认**（剥离元数据节后反汇编）：
```asm
420c4dfe: l32r   a8, …(40382920 <vTaskDelay>)
420c4e01: callx8 a8
420c4e04: call8  420c4fe8 <esp_time_impl_get_time>
```
⇒ **`0x420C4E04` 确为"紧随 `vTaskDelay` 调用"的指令地址**（用户结论成立）。

**栈槽核对：地址吻合，但未确认为该次调用的 ABI 保存槽。** `call8` 的返回地址带调用增量位，
`vTaskDelay` 的 `entry a1,32` 保存形式应为 **`0x820C4E04`**；实测在 main 栈窗口（2,560 字）中
**该形式一次都没出现**，只有 **plain `0x420C4E04` 一处**（0x3FCB38C4 = a8+4）；且邻域呈
**异常/上下文保存区**特征（word@a8 = `0x4037A1C4` = **`_xt_user_exit`**，邻近有 PS 形态值 `0x00060030`）。
⇒ ✅已确认"指令地址"；❌未确认"它是那次调用的返回地址保存槽"，只能说与"usleep 在调用链中"**相容但不构成证明**。

**先澄清矛盾（EPC1 vs 帧 PC）**——同一寄存器的两个时点，本轮 ELF `8b555b1f` 符号化：
| 值 | 时点 | 符号 |
|---|---|---|
| `0x403743C0`（`frame->pc`） | `_xt_panic` 在 **panic 入口**读 EPC1（**故障时刻**） | **`_DoubleExceptionVector`** |
| `0x42109B46`（live `epc1`） | OpenOCD 停机时 `reg epc1`（**panic 处理程序已运行之后**） | **`esp_core_dump_write`** |

⇒ "原始 PC 不可得"指**故障时刻**的 EPC1；停机时的 live EPC1 已被后续异常改写。此前未区分时点，已更正。

**`usleep` 返回地址的结构证据**（支持为真实返回地址）：`usleep` = 91 B 的 newlib FUNC
（`0x420c4db8..0x420c4e13`），候选返回地址 `0x420C4E04` = **`usleep+76`，距函数末 15 B**
（正是"最后一个 call 之后"的位置，newlib `usleep` 组 `timespec` 后调 `nanosleep`）；
存放在 **`frame.a8+4`**；同一栈内另有自洽的 `0x4201E960`(`inkwash_note4::main`×2) 与
`0x4218F9C8`(`main_task`)，与 `main_task → main → usleep` 嵌套一致；且均为不带调用增量高位的普通形式，
与本项目既有崩溃回溯同形。

> ⚠️ **未能完成的部分**：**无法用反汇编确认"该地址紧跟一条 call"**。该 ELF 中
> **newlib 时间函数整片无法解码**（`settimeofday$part$0`/`adjtime`/`_times_r`/`_gettimeofday_r`/
> `settimeofday`/`usleep` 反汇编可解行数均为 0，objdump 与 xtensa-gdb 都只输出原始 4 字节字），
> 而**同一 ELF 内** app/mbedTLS 函数可正常解码（含密度指令与 `callx8`）。
> 已排除 `e_flags=0`/缺 `.xtensa.info`（该段存在，0x38 B，新旧 ELF 相同）；本机无 `llvm-objdump`
> （esp-clang 为**仅 lib** 分发）。⇒ **工具/配置层面限制**；故调用点目前**只是结构推断**。

**源码层候选**（未与调用点绑定）：`main.rs:1064` `thread::sleep(POLL_INTERVAL_MS)`
（`light_sleep_committed == false` 分支）、`main.rs:1150`、`main.rs:1060`（light sleep 分支，
`wake.wait`，不经 usleep）、`power.rs:95/102/106`（light sleep 配置/撤销）。
现场崩溃前日志常为 `pm: Frequency switching config ... Light sleep: ENABLED/DISABLED` 与
`sleep_cpu_configure(236): Failed to enable CPU power down during light sleep.`，与"故障落在睡眠上下文"相符。
**按用户要求，未确认调用点前不预设"存在未初始化堆对象"**，故本节不做定性、不设计定点采集。

**归档确认（复位前必须齐备）**：异常帧（`oc_A/B2/E.out`）、相关栈内存（`oc_E.out` 0x3fcb3000..0x3fcb3800、
`oc_F.out` 0x3fcb3800..0x3fcb5800）、寄存器原始输出（`oc_A.out`）、任务链表/TCB（`oc_B2/C/D.out`）、
匹配 ELF（`8b555b1f`）、脚本（`oc_*.tcl`、`capture_stress.py`）**均已归档**；
**设备保持 panic 停机，本轮未复位、未再 attach。**

**下一步（待定）**：①先解锁可解码反汇编（带 S3 ISA 配置的 binutils/LLVM，或保留可解码构建产物），
再做指令级确认；②或沿上述候选点做**纯静态**指针来源审查，并标注"未与已确认调用点绑定"。
**P0-6 归因未定；发布继续阻断。**

**§7.29 续（vTaskDelay → 调度/上下文切换，反汇编已可读）**：
`vTaskDelay(40382920)`: `entry a1,32` → `beqz.n a2` 直接返回 → `call8 xTaskGetSchedulerState`；
非 RUNNING 时 `call8 __assert_func`（行 0x632=1586）；延时路径为
`xPortEnterCriticalTimeout(40380ba0)` → **`prvAddCurrentTaskToDelayedList(40381720)`** →
`vPortExitCritical(40380ce8)` → **`esp_crosscore_int_send_yield(40376224)`** → `retw.n`。
即睡眠路径深入**延时链表增删 + 临界区 + 跨核 yield**。
**边界**：这**不能**定位故障指令，也**不能**证明某个堆对象未初始化；现场与"坏值"有关的量仍只是
`excvaddr=0xCECECE00` 与 `a9=0xCECECE01`（**值**，不知其持有者则无法归属）。
**本轮未复位、未 attach；归档齐备。P0-6 归因未定；发布继续阻断。**

### 7.30 `0x3FCB38C0` 帧性质核对：**是有效异常/中断帧**（三者联系已建立）

离线完成；**未 attach、未复位**。详见 `logs/hw-forensics/p0-6-halt/FRAME_VERIFY.md`。

**本轮构建的 `XtExcFrame` 偏移**（窗口 ABI，`XCHAL_HAVE_LOOPS` 开，SWPRI/OVLY 关）：
`exit@0 pc@4 ps@8 a0@12 a1@16 a2..a15@20..72 sar@76 exccause@80 excvaddr@84
lbeg@88 lend@92 lcount@96 tmp0@100 tmp1@104 tmp2@108` ⇒ **112 = 0x70** ✓ 与 `XtExcFrameSize` 一致。
（参照帧自洽：`exit=0xDEADBEEF`、`pc=vector`、`a1=0x3FCB3800`、`excvaddr=0xCECECE00`、
`exccause=0x42` **= 2 + XCHAL_EXCCAUSE_NUM(0x40)**，正是 coredump 对伪因的 `+=` 结果 ✓。）

**① 布局吻合**：`0x3FCB38C0` 的 `exit=0x4037A1C4`(**`_xt_user_exit`**)、`pc=0x420C4E04`(plain)、
`ps=0x00060030`、`a0=0x8209B0E0`(**带 call8 增量位**)、`a1=0x3FCB3980`，
且 **`a1 − 帧基址 = 0xC0`，与真 panic 帧的 `0x3FCB3800−0x3FCB3740 = 0xC0` 完全相同** ✓
（两个独立帧互证该构建的 `XT_STK_FRMSZ = 0xC0`；该常量未能从 config 头解析，以实测为准）。

**② 有效指针引用**：本轮异常帧的 **`a8`（+44）= 0x3FCB38C0** ⇒ 故障上下文有活跃寄存器指向该帧基址 ✓
（a8 在窗口 ABI 中即"调用者帧基址"）。逐一核对 **16 个 TCB 的 `pxTopOfStack`：无一等于它**
（main = `0x3FCB37D0`）——语义不同，不矛盾。

**③ 保存路径**：`xtensa_vectors.S` 的 **`_xt_lowint1`(1218)** 与 `_xt_coproc_exc` 建帧时
`rsr a0,EPC_1; s32i a0,sp,XT_STK_PC` → `rsr a0,EXCSAVE_1; s32i a0,sp,XT_STK_A0` →
**`movi a0,_xt_user_exit; s32i a0,sp,XT_STK_EXIT`** ✓；`_xt_user_exit` 本体 `l32i PS/PC/A0 … rfe` ✓。

**⇒ 结论**：`0x3FCB38C0` 是**当前有效**的异常/中断帧（非陈旧残留，也非普通函数栈帧）。
**用户预判成立**：缺 `0x820C4E04` 只否定"按普通返回地址槽解释"，不否定上下文帧解释——
plain 形式正是 **`pc` 字段**（`EPC_1` 恒 plain），**带增量位**的形式在同一帧的 **`a0` 槽**
（`0x8209B0E0`，掩位后 = **`std::sys::thread::unix::sleep`**）。

**细化的调用链**：`main_task → inkwash_note4::main → std::sys::thread::unix::sleep → usleep
→ vTaskDelay`；即**中断/异常发生在 `usleep` 处于其 `vTaskDelay` 调用处**，
被中断上下文属 **main 的睡眠路径**。

**边界不变**：故障指令仍不可得（`pc`=双异常向量，EPC1 被硬件覆盖）；
`0xCECECE00` 仍是**值**，本帧不能指出其持有者；本轮**未**逐条审读延时链表、
**未**扩大到调度器全路径。**P0-6 归因未定；发布继续阻断。**

### 7.31 更正 §7.30：`0x3FCB38C0` 登记为**候选**上下文帧（撤回"当前有效帧"）

**用户更正（采纳）**：① `XT_STK_FRMSZ=0xC0` **有直接指令证据**（`_xt_panic` 序言
`addmi a1,a1,0xffffff00` + `addi a1,a1,64` ⇒ 净 −0xC0；并顺带独立验证偏移表：
旧 sp→+16=XT_STK_A1、PS→+8、EPC1→+4）；不需两帧互证。② **a8 不是窗口 ABI 中固定的
"调用者帧基址寄存器"** ⇒ §7.30 的"三者联系已建立／当前有效帧"**属过度结论，撤回**。
③ `0x420C4E04` 位于 `vTaskDelay` 调用**之后**，不得表述为"故障发生在 `vTaskDelay` 内"。

**保存路径已核实**：`_xt_context_save` 中 `a8` 全函数仅出现一次（`s32i a8, sp, XT_STK_A8`），
此前未被当暂存改写 ⇒ panic 帧 `+44` 是**真实的被中断上下文 a8** ✓。

**a8 来源无法唯一确定（本轮结论）**：
- 精确关系：`panic.a1(0x3FCB3800) + 0xC0 = 0x3FCB38C0 = panic.a8`；
  候选帧自身亦满足 `a1 = 基址 + 0xC0`。⇒ 提示"刚建好 0xC0 字节帧并把 sp 放进 a8"是**候选机制**，
  但无法刻定具体指令。
- **一致性检验不通过**：若故障上下文是 `usleep` 在 `pc=0x420C4E04`，则其 a8 应为
  `l32r a8,<vTaskDelay@0x40382920>` 载入的 **0x40382920**（调用返回后调用者窗口恢复，a8 不变）；
  实测 a8 = 0x3FCB38C0，且 **0x40382920 在(异常帧+栈+16×TCB)全部 dump 中一次未出现**
  ⇒ **故障上下文不是 usleep 在该 pc 处**，a8 与候选帧自身 `pc` **不能互相印证**。
- 该值另出现在 `0x3fcb3720`、`0x3fcb3894`（均在各自帧之外/之下），与"多层嵌套帧都把该基址当帧指针"
  相容，但同样不构成证明；候选帧内部无自引用。

**⇒ 登记为：结构吻合、且被故障时刻寄存器集合中一个字段（panic 帧 a8）引用的"候选上下文帧"**；
**不是**"当前有效帧"，也**未经**指令级证据证明执行过哪条把该地址写入 a8 的保存路径。
**停止沿该地址继续推导**（未做延时链表逐条审读、未扩大到调度器全路径）。

**不变**：`0xCECECE00`/`0xCECECE01` 仍是**值**，归属未知；故障指令仍不可得
（`pc`=双异常向量，EPC1 被硬件覆盖）；关于 `usleep` 只能说到"`pc` 指向其 `vTaskDelay` 调用**之后**的指令"。
**P0-6 归因未定；发布继续阻断。**

**§7.31 附：a8 来源的指令级枚举（唯一追加项）** —— 全镜像扫描**不可靠已弃用**
（剥离 `.xt.lit` 后数据集被误解码，`a8<-a1/sp` 家族出现 2311 条含不合理形态），仅采符号级可靠解码：
① **不存在** `addi a8,(a1|sp),0xc0/192` 形态 ⇒ `panic.a1+0xC0 = panic.a8` 非由立即数加法产生；
② **`_xt_lowint1` 完全不写 a8** ⇒ "中断入口把帧基址留在 a8"在指令级被排除；
③ 全镜像 `mov.n a8,a1` 命中点符号化为 `spi_flash_disable_interrupts_caches_and_other_cpu`、
`heap_caps_malloc_prefer`、`esp_cache_freeze_caches_disable_interrupts`、`ram_wifi_tx_dig_gain`
——均属**普通代码把栈地址当通用寄存器**，**无一属于帧管理/保存-恢复协议**（仅记录观察，不作推断）。
⇒ **无法唯一确定**该地址如何进入 a8，且三个候选机制被逐条排除 ⇒ **维持"候选上下文帧"登记**，
"当前有效"未获证明，**停止沿该地址推导**。设备保持现状；**P0-6 归因未定；发布继续阻断。**

### 7.32 更正 §7.31：撤回"一致性检验不通过"；本线收束

**撤回（用户指正，采纳）**：§7.31 中"一致性检验不通过 ⇒ 故障上下文不是 `usleep` 在该 pc 处"
**无效，予撤回**，理由：
1. **panic 帧与候选帧可能来自不同上下文** ⇒ 不能要求两者 a8 语义相同，也不能用候选帧的 `pc`
   推断 panic 帧 a8 的期望值；
2. **跨过 `vTaskDelay` 调用后**，未经 ABI 与被调函数逐条核对，**不能假定 a8 仍保存调用前的目标地址**
   （窗口轮转/被调者使用 a8 均改变该槽语义）。
⇒ 正确状态是 **缺少有效性证明，而非已有反证**。（"`0x40382920` 未出现在全部 dump 中"仍是事实，
但不构成推断。）

**收束决定**：**保留**"结构吻合、被现场寄存器引用的**候选上下文帧**"登记；**不再**由该地址推出
任何睡眠路径/故障位置结论。真机取证的**恢复条件**：提出能**捕获首次异常**或**直接定位破坏位置**
的具体方案后再恢复；在此之前不做无目标 attach、不复位。现有归档全部保留。

**P0-6 仍未归因，发布继续阻断；413 个主机测试通过不改变这一状态。**

### 7.33 首次异常捕获方案（离线设计+核查）与取证包独立备份

**边界**：不操作设备、不刷机、**未修改现有取证基线与冻结配置**。详见
`logs/hw-forensics/p0-6-first-exception/PLAN.md`。

**❌ 否定结论（含证据）：不能在异常入口"用调试器停住"。**
`_UserExceptionVector @0x40374340` = `wsr.excsave1 a0` → `call0 _xt_user_exc`，是软件可见最早点；
但硬件取异常时**已置 PS.EXCM=1**，而 Xtensa 规定 **EXCM=1 时调试异常（break/断点/观察点）转为双重异常**；
本 ELF `_DoubleExceptionVector @0x403743C0` 正是 `break 1,4` → `movi.n a0,2` → **`wsr.exccause a0`** → `call0 _xt_panic`
—— 这段**同时解释**了现场为何 `frame.pc=0x403743C0`、`exccause=2`。故入口断点会**再次覆盖 EPC1**，
**任何事后 attach 也不可能找回首次 EPC1**。

**✅ 可行方案（推荐 A′：异常表记录器，最省事）**：已核实
`xt_set_exception_handler()`（`xtensa_api.h:67`，底层 ROM `_xtos_set_exception_handler @0x40001c14`）、
`_xt_exception_table @0x3fca0068`（64 项，**默认全为 `xt_unhandled_exception @0x4037650c`** ⇒ 链回即可不改行为）；
分发路径 `_UserExceptionVector → _xt_user_exc(0x4037a128) → _xt_handle_exc(0x4037a131，在 0x4037a13a 建 0xC0 帧)
→ _xt_exception_table[cause] → 记录器 → xt_unhandled_exception → esp_panic_handler`
⇒ **记录器运行在 panic handler 之前**，EPC1/EXCVADDR/EXCCAUSE 仍为**首次异常**值。
记录器要求：`IRAM_ATTR`、**不调用函数/不打日志**、只 `rsr` 只读特殊寄存器 + 定址 `s32i`、
结构放 `RTC_NOINIT_ATTR`（S3 生效，复位后可回读）、记录后链回原 handler、多份自增 seq。
备选 A（向量入口记录，窗口最小但需构建期改向量）；可选第二阶段记录双重异常。

**验证步骤**：V1 离线反汇编核查（IRAM/无 call/无 flash 取址）→ **V2 可控已知故障**
（已知符号内 `*(volatile uint32_t*)0xDEADBEE0=1`，逐字段比对 `exccause/excvaddr/epc1`，必须完全一致）
→ **V3 双重异常下首次异常仍完整** → V4 才用于压力场景。
**停止条件**：改变故障率/时序、V2 字段不符、记录器可能二次异常、V3 首次异常被覆盖、
或需改动冻结基线中"新增记录器"以外的配置。

**取证包独立备份（工作区外，因 `logs/` 被 gitignore）**：
`~/inkwash-forensics-backup/20260916-170122/`（`hw-forensics/`、`FILELIST.txt` 116 项、
`MANIFEST.sha256` 逐文件 SHA256、`README.txt`）；**复核 116/116 OK**。

**P0-6 未归因，发布继续阻断。**

### 7.34 首次异常捕获：A′ 降级为条件性方案 + V1 离线产物与检查

**用户核对分发源码后指出，我逐条复核确认**：`_xt_user_exc` 通用路径在调用异常表 handler **之前**
已 ① 建帧 `addi sp,sp,-XT_STK_FRMSZ`、② `call0 _xt_context_save`、③ **`movi PS_INTLEVEL(..)|PS_UM`
+ `wsr a0,PS` 改写 PS 并清 EXCM**（`xtensa_vectors.S`）。⇒ 双异常若发生在 ①②③，**记录器到不了**；
handler 内读 PS **不是原始 PS**。故 **A′ 只能作为条件性方案，不能认定可覆盖 P0-6**。

**已落实的四项修正**（产物 `logs/hw-forensics/p0-6-first-exception/recorder.c`，见 `V1_CHECK.md`）：
① 原始现场**只从传入帧复制**（`f_pc/f_ps/f_a0/f_a1/a2..a15/sar/exccause/excvaddr/lbeg/lend/lcount`）；
现场特殊寄存器 **另列** `live_*` 并标注"非首次异常状态"；② 首条记录：`s32c1i` 原子抢占 +
`EMPTY→CLAIMED→memw→DONE` 状态机（**CLAIMED 无 DONE = 记录器自身出错**，可检出）+ `rsr.prid` 分核 +
NOINIT 生命周期（`p06_boot_init` 保留上一周期到 `prev[]`、写 `nonce`、最后写 `magic`；
记录器仅两字校验，失败只计数不写槽）；③ **链回 `xt_set_exception_handler` 的实际返回值**，不硬编码；
④ 覆盖范围写明**仅"成功到达异常表分发"的异常**。

**V1 反汇编检查**：32/32 个 handler 记录路径**恰好 1 次**调用（链回 `callx8`）、含 `s32c1i`/`memw`/`rsr.prid`、
`nm -u` **为空**（无外部依赖）、段归属 `.iram1`/`.rtc_noinit`/`.dram0.data` ✓。
**V1 查出并已修两处缺陷**：记录路径原本含 `call8 p06_fill`（我此前"不调用函数"的声明不成立）；
`p06_boot_init` 引入 `memcpy` 未定义符号。
**残留风险（登记）**：handler 栈占用 **128 B** ⇒ 栈耗尽类故障下记录器可能再次出错（可由 CLAIMED-无-DONE 检出）；
**列为进入真机验证前的待决项**，且 `IRAM_ATTR`/无日志/无锁**不保证**记录器绝不再异常。

**V2/V3 边界**：V2 只验证普通故障捕获；V3 只验证"记录后再异常仍保留"，
**不能**证明覆盖建帧阶段的双异常。**P0-6 未归因，发布继续阻断。**

### 7.35 首次异常捕获：验证固件**链接后** ELF 检查（未刷机；设备冻结）

构建：`INKWASH_P06_VALIDATE=1 cargo build --release`（新增组件 `components/p06_recorder`）。
详见 `logs/hw-forensics/p0-6-first-exception/V2_READY.md`。

**链接后符号与区域**：`g_p06` = **0x50000000（RTC slow RAM，复位后可回读）**；
`g_p06_prev`/`g_p06_nonce`/计数 = DRAM；**`p06_h0` = 0x40379CE4、`p06_arm` = 0x4037D800（IRAM）**；
`p06_v2_trigger` = 0x4210BEA4（flash，正常代码）。**记录路径的数据访问全部落在 DRAM/RTC** ✓。

**链接后与 `.o` 阶段不一致的两处（正是"不能只看 .o"的理由）**：
① 原子抢占在链接后成为 **libcall** `call8 <__atomic_compare_exchange_4>`（IRAM，内部再到
`__atomic_s32c1i_compare_exchange_4`）⇒ 原子性 ✓、全程 IRAM ✓，**但"记录路径只有链回一次调用"
在链接后不成立**；② 栈帧由 128 B 变为 **64 B**（链接选项不同）。链回旧 handler 为 `callx8 a6`（经
`g_p06_prev[cause]` 中保存的真实旧 handler）。

**初始化顺序已确认**：`p06_arm` → `call8 p06_boot_init` → `call8 p06_install`；由 Rust `main()`
在 bring-up 日志之后、`heap_probe` 注册之前门控调用，随后 `p06_v2_trigger()`。

**V2 验收基准（链接后反汇编）**：触发函数 `4210bea7: l32r a8,(0xDEADBEE0)` → `memw` →
**`4210beaf: s32i.n a9, a8, 0`** ⇒ 期望 `f_epc1 = 0x4210BEAF`、`f_excvaddr = 0xDEADBEE0`、
`f_exccause = 3（LoadStoreError→StoreProhibited）`、`state = DONE`、`prev_handler` = 原处理函数；
`live_*` 组必须与 `f_*` **分开验收**（分发器已改写 PS、清 EXCM）。

**边界与收窄（按用户）**：覆盖范围**仅"成功到达异常表分发"的异常**；建帧/`_xt_context_save`/PS 改写
阶段的双异常**不进记录器** ⇒ 对 P0-6 是**条件性**覆盖，**不构成可依赖的 P0-6 捕获工具**；
"CLAIMED 无 DONE"**只证明未完成**、**不能唯一归因**于记录器故障，且序言/占槽前出错可能**连 CLAIMED 都没有**
⇒ **不能保证检出所有失败**。128→64 B 栈占用**仅在 V2/V3 受控验证范围内接受**。

**为准备验证固件所做的源码改动（可回退）**：新增组件 2 文件；`Cargo.toml` 增一项 component_dir；
`build.rs` 增 `INKWASH_P06_VALIDATE` 门控与 `rerun-if-changed=Cargo.toml`；`main.rs` 增门控调用。
**`sdkconfig.defaults` 与已归档取证包未改；未刷机。P0-6 未归因，发布继续阻断。**

### 7.36 V2 基准三项更正（原因码 / 计数竞态 / 原子库锁依赖）——已修并重新链接

**① 原因码基准更正**：`panic_arch.c:228` 的 `reason[]` **按下标直取、无重映射**：
**`3 = LoadStoreError`（PIF 类）**、**`28 = LoadProhibited`**、**`29 = StoreProhibited`**。
`0xDEADBEE0` **不在任何区域**（DRAM/IRAM/RTC/EXTRAM/flash/外设映射之外）⇒ 区域保护违规、非 PIF
⇒ 期望 **`f_exccause = 29`**，**不是 3**；旁证：本项目早前 `EXCVADDR=0x0c` 的**读**被报为 **LoadProhibited(28)**。
V2 首轮实测确认；若实测为 3 则须**重新核定**（记为发现，不改写基准）。

**② 共享计数竞态（已修）**：删除 `++g_p06.written`、`g_p06_recorder_faults++`、`g_p06_missed_uninit++`
（跨核非原子读改写，槽 CAS 不保护之）；改为**按核索引** `g_p06_seq[2]` / `g_p06_missed[2]`。

**③ 原子库调用（已消除）**：链接后 `__atomic_compare_exchange_4` 内含
`xPortInIsrContext` / **`xPortEnterCriticalTimeout`** 回退 ⇒ 异常上下文存在**锁依赖**，不可接受；
且 64 B 只是 handler 自身栈帧。改为**手写内联 CAS**（`wsr.scompare1`+`s32c1i`，`always_inline`），
并**把 CAS 目标移到 DRAM**（`g_p06_state`）⇒ **不对 RTC 慢速 RAM 施加原子语义**（该支持问题由构造消除）。

**修正后的链接后证据**：`p06_h0`=0x40379CE8(IRAM)、`p06_arm`=0x4037D800(IRAM)、
`g_p06`=**0x50000000(RTC slow)**、`g_p06_state`=0x3FCAB264(**DRAM**)、`g_p06_seq`/`g_p06_missed`=DRAM(按核)、
`g_p06_prev`=0x3FCAB6DC。记录路径反汇编：`entry a1, 32`（由 64 B 降至 32 B）、`rsr.prid`、
`wsr.scompare1`+`s32c1i`、`memw`、**唯一调用 `callx8 a13`（链回真实旧 handler）**；
**无 libatomic、无临界区、无 flash 数据访问** ✓。

**V2 验收表**（逐字段）：`f_epc1 = 0x4210BEAF`、`f_excvaddr = 0xDEADBEE0`、**`f_exccause = 29`**、
`state = DONE`、`core`/槽位 = 实际故障核、`prev_handler` = 原处理函数；**`live_*` 与 `f_*` 分开验收**。
**边界不变**：仅"到达异常表分发"的异常；对 P0-6 属**条件性覆盖**；"CLAIMED 无 DONE"不能唯一归因、
序言前出错可能连 CLAIMED 都无 ⇒ 不保证检出所有失败。
源码改动可回退；**未刷机；设备冻结；P0-6 未归因，发布继续阻断。**

### 7.37 主机侧验收解码器（已完成）+ V2 前两点确认

**产物**：`tools/p06_accept.py`（单文件、仅标准库；不含模拟器、不引入测试框架）；
布局标记为**纯注释**（不改变代码生成，不影响已链接 ELF）。
**布局实测（宿主 cc 编译真实结构体取得，非手抄）**：`p06_frame_t=112`、`p06_rec_t=140`、
`p06_region_t=2256`、`slots@1136`。
**自检 7/7 通过**：完整记录→OK、magic 错→BAD_MAGIC、nonce 错→STALE_NONCE、
CLAIMED 未完成→INCOMPLETE、核号错→WRONG_CORE、字段不符→FIELD_MISMATCH、空区→BAD_MAGIC。
> **边界**：只证明布局与判定逻辑；**不证明**目标侧 CAS/重入/异常路径安全，**不能替代**真机 V2。

**V2 前两点确认**：① **期望 PC 每次从最终验证 ELF 重新提取**（`--elf` 机制强制；本次 **0x4210BEAF**），
不沿用上轮地址；② **原因码 29 仅是"保留预期"**——"地址不在已列出的内存区"**不足以单独证明**必为 29；
若实测为 **3**，须**分别**核查①芯片异常分类、②记录器是否忠实复制现场（`f_*` 对照帧），
**不得直接认定记录器失败**。

**表述更正**：此前"QEMU 必然假阳性"不准确；准确边界是**其结果不能替代本次真机验收**
（原因码/RTC 慢速 RAM/`s32c1i`/双核/IRAM 与锁行为与真机不一致）。
**解码器就绪 ⇒ 可安排受控 V2，通过后再做 V3。设备继续冻结；P0-6 未归因，发布继续阻断。**

### 7.38 受控 V2：**未通过**（触发不成立，记录器未被行使）

**烧入件核对**：ELF SHA256 `4ae9b30a…`；由该 ELF 提取期望 PC **0x4210BEAF**；ELF 含门控日志串
（1 次）⇒ 门控生效。（先前两次"未接线"判断是我**校验方法有误**：本工具链外部调用为 `lrr`+`callx8`
而非 `call8 <addr>`，且该区反汇编失步。）

**实测**：串口（原样保留）打印 `p06 validation: recorder armed; triggering V2 fault now` 后
**无 Guru/panic**；`E (11334) task_wdt` 报 `IDLE0 (CPU 0)` 未喂狗、`CPU 0: main`，`Aborting.`，
输出止于 `Print CPU 0 backtrace`。JTAG（读取在复位前完成）：core0 **`pc = 0x4210BEAF`**
（正是 `p06_v2_trigger` 的 `s32i.n` 地址）、`ps` 的 EXCM 位为 0 ⇒ **CPU 停在故障指令本身**，
即**对 `0xDEADBEE0` 的写入未产生异常**（总线挂起），并非走进向量；
`g_p06.magic/boot_done` ✓、RTC `nonce` 与 DRAM `g_p06_nonce` **一致**（启动期装配已执行 ✓），
但 **`g_p06_state[0..1]` 全为 EMPTY、槽 `state`=0** ⇒ **CAS 从未发生 ⇒ 记录器 handler 从未执行**；
`s_exc_frame = 0`、`g_exc_frames[0] = 0x3FCB70C0`（panic 路径已开始建帧）。

**判定**：**V2 不通过**，原因是**触发器不成立**（该地址不产生异常），异常表分发路径未被走到；
**原因码 29 vs 3 本轮未被检验**——实测既非 29 也非 3，而是**无异常**，此前"区域保护违规 ⇒ 必然 29"
的推理**未被证实**。按要求：**记录缺失 ⇒ 停在 V2 分析，不进入 V3、不跑 P0-6 压力**。
**最小改动（待批准）**：触发地址改为确实产生异常的写（候选 `*(volatile u32*)0x0 = 1`），
仅改常量 + 重新链接 + 重新提取期望 PC 后再跑 V2；记录器本体不动。
**设备已在读取时 halt，未复位；P0-6 未归因，发布继续阻断。**

### 7.39 受控 V2（第二次，触发器改为内联汇编 store to 0）：**仍未通过**

仅改触发器、记录器本体未动：`p06_v2_trigger` = `movi.n a8,0` → `movi.n a9,1` → **`s32i.n a9, a8, 0`**
（向地址 0 存储，不用 C 层解引用）；烧入 ELF SHA256 `f56612f0…`；期望 PC（由该 ELF 提取）**0x42187EB7**。

**实测**：串口原样保留；门控行 1 次后**无 Guru / 无 CORE DUMP / 无 CPU halted**，`E (11334) task_wdt`
→ `CPU 0: main` → `Aborting.` → 止于 `Print CPU 0 backtrace`。JTAG（读取在复位前）：core0
**`pc = 0x42187EB7`**（正是该 store）、`ps` EXCM=0、`excvaddr = 0`；`g_p06_nonce = 0x0cbd9bc7`（新一轮启动）；
**`g_p06_state` 全 EMPTY、`g_p06_seq`/`g_p06_missed` 均为 0**；`s_exc_frame = 0`；
`g_exc_frames[0] = 0x3FCB70C0`。**解码器机械判定：`NO_RECORD`**。

**收窄结论（按用户）**：**未取得预期异常记录**；**停机时 PC 位于触发指令**；**随后观察到 WDT**。
**单次停机快照不足以证明"总线永久挂起"**；**EMPTY 也不能单独证明 handler 完全未进入**
（旁证：`seq/missed` 为 0，但无法排除分发器在调用表 handler 前就卡住）。两次触发（`0xDEADBEE0`
与地址 `0`）同形态停滞、均未产生异常 ⇒ 该类触发器无法行使记录器；原因码 29/28 均**未被检验**。

**建议最小下一步（待批准）**：改用本项目**已实测会产生异常**的形态——**从无效地址读**
（内联汇编 `l32i`，a8=0）。**V2 通过前不进入 V3、不跑 P0-6 压力；设备已在读取时 halt，未复位。
P0-6 未归因，发布继续阻断。**

### 7.40 第三次 V2（内联汇编 l32i from 0）：**NO_RECORD ⇒ 停止更换触发器**

触发器改为 `movi.n a8,0` → **`l32i.n a9, a8, 0`**（从地址 0 读）；ELF SHA256 `b39970a0…`；
期望 PC（由该 ELF 提取）**0x42187EB5**。实测：串口原样保留，门控行后**无 Guru/无 CORE DUMP/无 CPU halted**，
`E (11334) task_wdt` → `Aborting.` → 止于 `Print CPU 0 backtrace`；JTAG（复位前）core0
**`pc = 0x42187EB5`**（正是该 `l32i`）、EXCM=0、`excvaddr=0`；`g_p06_nonce=0x0cbd5d53`（新启动）；
**`state` 全 EMPTY、`seq`/`missed` 全 0**；`s_exc_frame=0`。**解码器机械判定 `NO_RECORD`**。
按既定规则**停止更换触发器**，转入 ELF 与异常表注册/分发链排查。

**登记链现场核对（本轮 ELF 符号；无需刷机）**：`_xt_exception_table=0x3fca3768`（**偶数下标为本记录器
`p06_hN`，奇数下标仍为 `xt_unhandled_exception`**）、**`g_p06_prev` 全 0**（"保存并链回旧 handler"
的簿记从未发生）——**这是一处必须解释的不一致**：设计上应对 0..31 全部安装并把返回值写入 `g_p06_prev`。
在解释清楚前**不再做任何 V2 尝试**。
> 更正：先前一次用**上一版 ELF** 地址读 `_xt_exception_table`/`g_p06_prev` 得到乱码，**该读数无效**；
> 以上为用本轮 ELF 符号重读。

**待查方向**：① `xt_set_exception_handler` 在本 IDF 版本的真实语义（是否只接受部分 cause／是否有影子表／
表元素步长）；② `p06_arm→p06_install` 是否确实执行到写入分支（`boot_done=1` 却 `g_p06_prev` 全 0）；
③ 触发后是否真的从未进入向量/分发（三次 PC 均停在触发指令且 EXCM=0）。

**表述更正**：此前"两次均未产生异常"改为 **"两次（现为三次）均未取得目标异常记录"**——现有证据
尚不能区分触发、分发或记录环节的问题。**设备已 halt，未复位；不进入 V3、不跑 P0-6 压力；P0-6 未归因，发布继续阻断。**

### 7.41 记录器四项语义缺陷修正（用户源码审查；本轮**未操作设备**）

全部已修并重新链接（ELF SHA256 `14267ddf…`；期望 PC 仍为地址 0 `l32i` 的 **0x42187EB5**）。
详见 `logs/hw-forensics/p0-6-first-exception/RECORDER_FIXES.md`。

1. **「偶数项安装、奇数项默认」是正常的**（先前判断作废）：`xt_set_exception_handler` 按
   `n = cause * portNUM_PROCESSORS + xPortGetCoreID()` 索引，core0 只更新偶数项 ✓。
2. **链回写错（已修）**：旧 handler 返回 0 属正常语义（旧值为默认项时返回 0）；原
   `if (prev) …; return;` 会**跳过默认处理**，若故障 PC 不变即**反复执行同一故障指令**——
   这是"三次停在触发指令、最终 WDT"的**候选机制（尚需验证）**。改为 else 调 `xt_unhandled_exception`。
3. **CAS 两处错误（已修）**：`"=a"` 未把 `want` 作为输入（初始化可能被丢弃）；成功判据应比较
   **`expect`** 而非 `want`（以 libatomic `__atomic_s32c1i_compare_exchange_4` 的
   `sub/nsau/srli` 为准）。链接后验证：`scompare1`+`s32c1i`+**单条零值测试分支** ✓
   ⇒ 原缺陷足以使记录判定失效，**EMPTY 不能证明 handler 未进入**。
4. **核号提取错误（已修）**：应为 **PRID bit 13**（`xt_utils.h`），原 `& 3` 错误。
   （另修一处我引入的声明顺序问题，`-Werror` 曾导致组件未重建。）

**链接后**：记录路径 `entry a1, 32`、`rsr.prid`、`wsr.scompare1`+`s32c1i`、成功/失败分支 ✓。
⚠️ **待确认**：`p06_h0` 中按符号名未找到 `xt_unhandled_exception` 引用（可能经字面量/跳板），
**下次 V2 前需再确认 else 分支确实调用默认处理**。

**边界**：这些是**诊断工具缺陷**，**不是 P0-6 的归因**。**未操作设备；不进入 V3、不跑 P0-6 压力；发布继续阻断。**

### 7.42 受控 V2：**通过**（修正记录器后，同一"读地址 0"触发器）

**离线确认**：`p06_h28` 的 `beqz a13, 4037ce04` → `call8 40376540 <xt_unhandled_exception>`
（本 ELF：`_xt_exception_table=0x3fca3868`、`g_p06_prev=0x3fcab2d4`、默认处理 `0x40376540`）✓

**烧入/现场**：ELF `14267ddf…`；期望 PC（由该 ELF 提取）**0x42187EB5**。串口（原样保留）
出现 **`Guru Meditation Error: Core 0 panic'ed (LoadProhibited)`**，随后 `Setting breakpoint at
0x42187eb5 and returning...`（`ESP_DEBUG_OCDAWARE` 路径 ⇒ 无重启、无 coredump）。JTAG（复位前）：
`pc=0x42187EB5`、`exccause=0x1C=28`、`excvaddr=0`、`a1=0x3FCB7280`；`g_p06_state[0]=CLAIMED`、
`seq[0]=1`、`missed=0`；`frame_ptr=g_exc_frames[0]=0x3FCB71C0`。

**逐字段验收（槽 0）**：`state=DONE` ✓、`core=0` ✓、`cause=28` ✓、`seq=1` ✓、
**`f_pc=0x42187EB5` = 由最终 ELF 提取的期望 PC ✓（核心判据）**、`f_ps=0x00060630` ✓、
`f_exccause=28` ✓、`f_excvaddr=0` ✓、**`f_a1=0x3FCB7280` = 现场 reg a1 ✓**、`prev_handler=0` ✓、
`frame_ptr=0x3FCB71C0 = g_exc_frames[0]` ✓、`live_epc1=0x42187EB5` ✓；串口独立报 `LoadProhibited`
与 `cause=28` 一致 ✓。

**遗留（需补，不改变通过判定）**：完整异常帧 dump 因脚本笔误未取到 ⇒ "`f_*` 与整帧逐字段比对"
目前只有 `frame_ptr`/`f_a1`/`f_pc` 三项交叉吻合。另：前三次"停在触发指令 + WDT"的机制已被本次
结果支持——**修好链回前 handler 返回即重执行同一故障指令**（用户假设得到印证），并再次印证
**EMPTY 不能证明 handler 未进入**。

**下一步**：补整帧比对后再进入 V3。**V2 通过前不进入 V3、不跑 P0-6 压力；诊断工具修复不改变
发布阻断状态。设备已 halt，未复位。P0-6 未归因，发布继续阻断。**

### 7.43 V2 完整验收（只读补采）：**通过**（整帧 25/25 一致）

**一致性**：两次独立读取一致（`pc=0x42187EB5`、`ps=0x00060620`、`a1=0x3FCB7280`、
`exccause=28`、`excvaddr=0`；magic/nonce/boot_done、`state[0]=CLAIMED`、`seq[0]=1`、
`g_exc_frames[0]=0x3FCB71C0` 均不变）✓

**整帧比对（记录槽 0 ↔ 帧）**：**25/25 全一致**（`f_exit/f_pc/f_ps/f_a0/f_a1/f_sar/f_exccause/
f_excvaddr/f_lbeg/f_lend/f_lcount` + `f_a2..f_a15`）。**无需解释差异**：`exit=0x420B82B8`
（非 coredump 的 `0xdeadbeef`，因本路径 coredump 未执行），`f_exccause=28`（未加 0x40 伪因），
与串口 `LoadProhibited` 一致 ✓。**⇒ V2 完整验收通过**（判据：`f_pc` = 由最终 ELF 提取的期望 PC，
且 `f_*` 与帧逐字段一致）。

**`ESP_DEBUG_OCDAWARE` 行为（记为 V3 实验条件）**：串口出现
`Setting breakpoint at 0x42187eb5 and returning...`，说明 `esp_cpu_dbgr_is_attached()` 为真；
本轮**运行期间未启动 OpenOCD**（仅事后 attach 读取）⇒ 最可能是内置 USB-Serial-JTAG 被判为已连接
（**确切原因未验证**）。后果：panic 设 BP0 后返回 ⇒ **无重启/无 coredump/无 CPU halted**。
**V3 必须记录触发时的调试器/OCD 连接状态**并明确期望的 panic 出口。

**归因边界**：链回与 CAS 是**同时**修复的 ⇒ **不能据此证明前三次运行每一次都完整经历了所推测路径**。

**V2 已完整通过 ⇒ 待安排进入 V3。设备仍由 OpenOCD 挂住（halted，未恢复执行、未复位）。
P0-6 未归因，发布继续阻断。**

### 7.44 V3：首录完成后再次异常不覆盖首录 —— **通过**

V3 固件（ELF `452404a4…`）新增第二个触发器：第 1 次 `p06_v2_trigger` = `l32i` 读地址 **0** @
**0x42187EC1**；第 2 次 `p06_v3_trigger2` = `l32i` 读地址 **0x10** @ **0x42187ECD**（两地址均从最终 ELF 定位）。
**单次触发/无递归**：第二次排在第一次之后；第一次故障经 panic(OCDAware) 停住后，由调试器跳过该指令
（`pc ← 0x42187EC3`）并 `resume` 才到达第二次。

**Phase 1**（触发时**未运行 OpenOCD**）：串口 `Guru ... (LoadProhibited)` + `Setting breakpoint at
0x42187ec1`；现场 `pc=0x42187EC1`、`exccause=28`、**`excvaddr=0`**；槽 0 `state=DONE`、`seq=1`、
**`f_pc=0x42187EC1`**、`f_excvaddr=0`、`frame_ptr=0x3FCB71C0`。

**Phase 2**（OpenOCD 已连接并驱动）：**独立证据** ① 串口出现**第二次** `Guru ... (LoadProhibited)` +
`Setting breakpoint at 0x42187ecd`（=PC_B）；② halt 时 `pc=0x42187ECD`、**`excvaddr=0x10`**。

**保留判定**：Phase1 与 Phase2 的槽 0 **逐字段完全一致**（`state/core/cause/seq/f_pc/f_a1/f_exccause/
f_excvaddr/frame_ptr/prev_handler/live_epc1`）⇒ **`f_pc` 仍为第一次的 0x42187EC1、`seq` 仍为 1**
⇒ **首录未被第二次异常覆盖** ✓

**OCD 状态/panic 出口**：第一次故障时**未运行 OpenOCD** 却仍走 OCDAware 出口 ⇒ **不得把
USB-Serial-JTAG 的存在等同于"调试器已连接"**（确切原因未验证）；第二次时 OpenOCD 已连接（条件已分别记录）；
两次 panic 出口均为 **OCDAware 返回**（无重启/coredump/halt）。

**边界**：V3 仅证明该**受控路径**下的记录保留能力，**不证明**覆盖建帧阶段的双异常。
**本轮不跑 P0-6 压力；P0-6 未归因，发布继续阻断。**

### 7.45 V2/V3 收束 + P0-6 诊断构建（仅安装记录器，无人工触发）离线准备

**V2/V3 覆盖范围命名（收束）**：V2 = 受控异常**到达 handler** 后记录器**忠实复制异常帧**（整帧 25/25）；
V3 = **第一次 handler 返回、调试器跳过故障指令后，再发生一次独立异常**，首录未被覆盖。
**V3 不是嵌套异常测试**，**不能**证明"记录器或默认 handler **执行期间**再异常"的行为——本轮结果不受影响，
只是覆盖范围须如此命名。

**诊断构建**（`--features p06_diag`，`main` 只调 `p06_arm()`）：ELF `250b9c98…`。
**离线验证**：`p06_v2_trigger`/`p06_v3_trigger2` 的符号在 ELF 中**完全不存在**（无调用点 ⇒ gc-sections 回收）
⇒ 结构上无人工触发 ✓；`p06_arm` 被引用 ✓；诊断日志串在、`p06 validation` 串不在 ✓。

**安装覆盖范围**：只在 **core 0** 安装（`xt_set_exception_handler` 按 `cause*portNUM_PROCESSORS+core_id` 索引）
⇒ **core 1 的异常不会记录**；覆盖 **cause 0..31**；**不覆盖**表分发前分流的原因
（1 syscall / 5 alloca / 4 level-1 中断 / ≥32 coproc）、cause≥32、**双重异常**、
以及**建帧/`_xt_context_save`/PS 改写阶段**的再异常。

**预先规定的判读规则**：留 DONE ⇒ 分析首录（`f_pc`、与异常帧逐字段比对、cause 交叉核对）；
**没有完整记录 ⇒ 不得直接归为"建帧阶段双异常"**，须按序先核对①故障核是否 core 1（覆盖缺口）、
②是否已武装（magic/nonce/boot_done）、③`missed` 是否>0、④`state/seq`（EMPTY 且 seq==0 也**不能**断言"未进入"；
CLAIMED 无 DONE ⇒ 记录器自身未写完，属 V3 **未**覆盖的嵌套情形）；只有以上全部排除后才可**假设**
未到达表分发，且需更早的向量入口捕获确认。

**本轮未刷机、未恢复压力。P0-6 未归因，发布继续阻断。**

### 7.46 诊断构建自然故障观测 第 1 窗口：**本窗口未复现**

刷入诊断 ELF `250b9c98…`（仅安装记录器、无人工触发），跑一轮原压力序列（25 × `set_timezone` @6 s，
首次异常即停）：**25 条全部跑完，`Guru=0`、`rst=0`、无 coredump、无 CPU halted**，正常至 uptime 232,873 ms
⇒ **本窗口未复现**（按约定登记，**不自动延长**）。

**现场采集（读取在复位前完成）**：已武装（`magic=0x50303631`、RTC `nonce=0x0cbdafed` == DRAM `g_p06_nonce`、
`boot_done=1`）；**无记录**（`state=EMPTY`、`seq=0`、`missed=0`）；**安装表经验性证实"只装 core 0"**
（偶数项=本记录器 `p06_hN`，**奇数项=默认 `xt_unhandled_exception`**）；`g_p06_prev` 全 0（旧值为默认 ⇒ 返回 0 ⇒
链回走默认 ✓）；两核均正常运行（`pc=0x40380156`、`excvaddr=0`）；`s_exc_frame=0`、`g_exc_frames=0,0`。
采集流程端到端可用 ✓。

**判读规则（含两点修正）**：**DONE 保存首个被记录异常**——后续嵌套异常时 `g_exc_frames[0]`/串口原因码
**可能对应后一次**，**不要求与首录相等**，须分别确认时点；**CLAIMED 无 DONE 只说明记录未完成**，
**不能**直接判定为嵌套异常或记录器自身故障。本轮属"无记录且初始化/覆盖正常"，但**没有故障发生**，
故**不产生任何归因**。

**下一步（不自动推进）**：本窗口未复现 ⇒ 尚无数据判断现有记录器对 P0-6 的价值；是否推进
**向量入口捕获**留待决定。设备读取时已 halt，**未复位**。**P0-6 未归因，发布继续阻断。**

### 7.47 连续运行观测（最多 3 窗口）：**未能开始 —— 控制台链路失效**

**0/3 窗口完成**；**不是"3 个窗口均未复现"**，而是**设备控制台不可用**。
经过：上轮读取后设备被留在 halt；按授权恢复时 OpenOCD 报两核 `running` 但串口 `get_status` 无应答，
`resume` 报 `not halted`；随后 `reset run`（实验尚未开始，不属"窗口间复位"）后**仍无应答**；
三次尝试窗口 1 均被停发条件捕获：`sent=3 ok=0 stop=no-reply:dw1`（**无应答即停按设计生效** ✓）。

**主机侧诊断**：端口无进程占用、无残留进程；端口节点存在但**静听 8 s 收 0 字节**；
`system_profiler` **未列出该 USB 设备** ⇒ **USB-Serial-JTAG 控制台已从总线脱落**，节点为残留。
诱因可能为此前反复的 **attach/halt/reset/BP0** 操作。

**恢复动作（需现场操作）**：**重新插拔 USB 或断电重上电**，使 USJ 重新枚举；恢复后先确认
`get_status` 有应答（`tz=480`）再开始实验。

**边界**：无任何 P0-6 故障数据；本版记录器在 P0-6 现场的捕获能力**仍未验证**；按约定**不自动延长**。
连续运行脚本已就绪且在本次被验证有效（`diag_window.py`：25 × `set_timezone` @6 s；停发条件
Guru/WDT/意外复位/无应答>15 s/应答畸形；窗口间不复位；记录起止 uptime）。
**P0-6 未归因，发布继续阻断。**

### 7.48 连续运行观测（3 窗口）：win1 未复现；**现场显示 core 1 曾 panic（覆盖缺口）**

连续运行（窗口间不复位）：**win1 25/25 正常跑完**（uptime 27,661→229,979 ms，无 Guru/WDT/rst）⇒ 未复现；
**win2**（9/25）与 **win3**（5/25）均因 **`no-reply`（15 s 无应答）** 提前停发，win3 窗口内出现
20,595 ms 小 uptime ⇒ 疑似复位。应答并非 busy 而是 `PCF8563 read regs 0x00 failed: ESP_FAIL`
（外部 RTC 读失败，命令时好时坏；记为设备态观察）。

**现场（复位前，JTAG）**：已武装（`magic`/RTC `nonce=0x0cc16383` == DRAM `g_p06_nonce`/`boot_done=1`，
nonce 变化 ⇒ 期间有重启）；**无记录**（`state=EMPTY`、`seq=0`、`missed=0`、槽为未初始化内容）；
**`g_exc_frames = {0, 0x3FCE7380}`、`s_exc_frame = 0x3FCE7380`** ⇒ 非零项在下标 1
⇒ **panic 发生在 core 1**；安装表首项复核"只装 core 0"。

**判读（适用预登记规则第 1 条）**：故障核为 **core 1 ⇒ 覆盖缺口**，本构建本就记录不到，
**与记录器正确性无关**，**也不能**归为建帧阶段双异常；同时**经验性证实**了离线预登记的覆盖声明。

**设备态观察**：① USB-Serial-JTAG 控制台再次失效（win2/3 无应答，实验后亦无应答；JTAG 可用）
⇒ 本轮采集完全依赖 JTAG；② 外部 RTC 读失败影响压力序列到达率，分析需计入。

**结论**：本轮自然故障**落在覆盖缺口内** ⇒ 仍无法判断记录器对 **core 0** P0-6 故障的捕获能力。
**建议的最小改动（待决定，本轮未实施）**：在 **core 1 上**执行一次 `p06_install()`（绑定 core 1 的短任务），
使两核都被覆盖。需现场恢复控制台（重插 USB/断电重上电）才能继续串口驱动的窗口。
**P0-6 未归因，发布继续阻断。**

### 7.49 core 1 现场归档与分析 + 实验记录更正

**复位原因**：OpenOCD 报两核 **`Reset cause (1) - Power on reset`** ⇒ 最近复位是 reset button，
**其后无再次复位** ⇒ §7.48 的"win3 疑似复位"**更正**（该小 uptime 更可能是**未清缓冲的陈旧数据**）。

**帧 `0x3FCE7380`**（`g_exc_frames[1]=s_exc_frame`）：`exit=0xdeadbeef`(coredump 标记)、
**`pc=0x403834D8` = `panic_abort`**、`ps=0x00060e30`、`a1=0x3FCE7440`、
**`exccause=0`、`excvaddr=0`**、`lbeg/lend=0x40056f5c/0x40056f72`、`tmp2=0x4037D8A0`(`_xt_handle_exc`)
⇒ **软件 abort 路径**（abort/esp_system_abort，与 TWDT/assert 一类一致），**非 CPU 异常原因码**。

**故障任务与栈**：`pxCurrentTCBs={core0:0x3FCD9B5C, core1:0x3FCE85BC}`；core1 当前任务 `pthread`
（`pxStack=0x3FCE41B8..pxEndOfStack=0x3FCE81B0`，16,376 B）**包含**帧与该帧 a1 ⇒ **故障任务 = core1 的 pthread**；
core0 的 `pthread`(8,184 B) 不含该帧。

**是否同类**：与早前 P0-6（core0 / `_DoubleExceptionVector` / `exccause=2` / `excvaddr=0xCECECE00` / `main`）
**按现有证据属不同类**。**但** core1 未装记录器**只解释"为何无记录"**，**不能**认定这就是 P0-6，
**也不能**证明记录器在本次故障中正确运行（本轮记录器**未参与**）。

**对双核扩展的含义**：本次是 **`panic_abort` 软件 abort**，**不经过异常表分发** ⇒
**即使装到 core 1 也仍不会记录到此次这类事件**；双核扩展对 **core1 的 CPU 异常**有效，对 **abort/assert/TWDT** 无效。

**实验记录更正**：① **win2 首次无应答已触发"停止并采集"，win3 不应继续**（我的执行错误，已记录）；
② **三窗口不能整体称为"连续、无复位实验"**，应记为"win1 完成未复现 → win2 首次无应答即止 → win3 属越界补跑"；
且据 §1 实验中并未发生复位。**P0-6 未归因，发布继续阻断。**

### 7.50 core 1 abort 现场限定补采 + 关键更正（`ill` ⇒ IllegalInstruction）

**更正（用户）**：本机 IDF `panic_abort()` 执行 **`ill`** ⇒ **IllegalInstruction（EXCCAUSE 0）**，
**会经过异常表**；我此前"软件 abort 绕过异常表"**撤回**。ELF 证据：`403834d3 l32r a8,<g_panic_abort_details>` →
`403834d6 s32i.n a2,a8,0` → **`403834d8 ill`** → `j`；与帧内 `pc=0x403834D8`、`exccause=0` **完全吻合** ✓

**详情字符串**：`g_panic_abort_details=0x3FCE7480`（core1 任务栈内，等于帧内 `a2`）指向
**`abort() was called at PC 0x4209f8ce on core 1`** ✓；`g_panic_abort` 已置位。
调用者 PC `0x4209F8CE` 已符号化；栈内候选主要为 Rust 格式化/panic 帧
（`Location::fmt`、`<&str as Display>::fmt`、`<&dyn Debug>::fmt`）⇒ 与"Rust panic → 格式化 → abort"一致。

**对双核记录器的含义（修正后）**：该路径**会**以 cause 0 进入异常表 ⇒ 双核安装后 core1 的该路径
**可能被记录**（pc/exccause/excvaddr），但记录器**不会**指出**谁/为什么** abort ⇒ 诊断构建应把
`g_panic_abort_details` 指针与字符串**纳入采集清单**；**assert/TWDT 是否同样经 `ill` 需分别核对**。

**表述更正**：① 撤回"绕过异常表"；② **"实验中并未复位"改为"未确定"**（单个 reset-cause 读数不能证明
整个历史，"小 uptime 来自陈旧缓冲"亦仍缺证据）；③ 本次签名（`ill`/cause 0）与此前 P0-6 不同，
但**原因未确认**，**不据此宣布与 P0-6 无关**。**暂不复位；P0-6 未归因，发布继续阻断。**

**§7.50 补充（调用者定位）**：详情字符串给出的 abort 调用 PC **`0x4209F8CE` = `std::sys::pal::unix::abort_internal`**
（紧邻 `std::panicking::resume_unwind::…::take_box`）⇒ 调用链为
**Rust panic/unwind → `abort_internal` → IDF `abort()` → `panic_abort` → `ill` → IllegalInstruction(cause 0)**。
即本次是**经 Rust panic 路径触发的 abort**（记录器即使装上 core 1 也只能给出 `ill` 现场，
"是谁/为什么"仍须靠 `g_panic_abort_details` 与栈回溯）。**暂不复位；P0-6 未归因，发布继续阻断。**
