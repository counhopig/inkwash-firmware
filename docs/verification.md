# 验证状态：测试、CI、构建与刷写

> 本文件回答两个问题：**(1) 仓库自己验了什么、没验什么；(2) 本文档集的结论里
> 哪些已被证实、哪些仍待真机确认。**

## 1. 测试分布

`cargo test -p inkwash-logic` → **386 passed, 0 failed**（实测）。

按模块的生产/测试行数划分（首个 `#[cfg(test)]` 行号之前算生产）：

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

**`rust-firmware/` 只有 3 个模块内测试**：

| 位置 | 内容 |
|---|---|
| `effect_task.rs:350-483` | `execute_batch` 的顺序 / Continue / AbortBatch / 单条失败通知 |
| `epd_task.rs:366-392` | 完成邮箱有界性与预留的无损性 |
| `usb_console.rs:209-292` | 命令暂存有界性、应答队列背压与 owned 重试 |

**这三个文件之外，固件侧零测试。** 而固件才是碰 I2C/EPD/射频/NVS 的那一半。

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

**它守护的约束**（9 个测试）：
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

## 3. CI 覆盖与缺口

`.github/workflows/ci.yml` 只有一个 job：

```yaml
logic:
  runs-on: ubuntu-latest
  defaults: { run: { working-directory: logic } }
  steps:
    - cargo test --locked
    - cargo fmt --check
    - cargo clippy --all-targets -- -D warnings
```

| 项 | 状态 |
|---|---|
| `logic` 测试 | ✅ 每次 push/PR |
| `logic` fmt / clippy | ✅ `-D warnings` |
| `rust-firmware` 编译 | ❌ **无门禁** |
| `rust-firmware` fmt / clippy | ❌ |
| app 体积检查 | ❌ 无 |
| `check-git-rev.sh` | ❌ 未接入 CI（脚本存在，只能手动跑） |

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
| 充电图标三张位图相同 | ✅ 逐行比对确认 |
| `.esp32-review.yml` 行号过期 | ✅ 核对实际行号 |
| `EspWifi::drop` 行为 | ✅ 读取 `esp-idf-svc-0.52.1` 源码；**对内存的实际影响未验证** |
| NimBLE 回调上下文 | ❌ **未验证**，需确认框架派发位置 |
| 功耗/时序数值（1.2 s 轮询、0.8 s 浅睡占比等） | ⚠️ 来自代码与 `sdkconfig.defaults` 注释，**未经真机测量** |
| `docs/` 原有内容（开发指南、控制协议、截图） | ❌ 已被 `b30c3af` 删除，只能重建可由源码确证的部分 |
