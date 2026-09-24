# ESP32-S3 固件代码审查报告

## 0. 审查范围与假设

### 审查基线

- 提交：`15246185820947bb4765bbafbbfd22fb4070142a`
- 审查日期：2026-09-23
- 固件定位：面向极客用户、通过 USB 本地刷写的 Zectrix Note 4 固件，不要求 OTA
- 审查过程未修改固件代码，未烧录设备，也未访问串口设备

### 已审查内容

- 构建与配置：
  - `rust-firmware/Cargo.toml`
  - `rust-firmware/Cargo.lock`
  - `rust-firmware/.cargo/config.toml`
  - `rust-firmware/build.rs`
  - `rust-firmware/sdkconfig.defaults`
  - `rust-firmware/sdkconfig.diagnostic.defaults`
  - 普通与 secure 构建生成的最终 `sdkconfig`
  - `rust-firmware/partitions.csv`
  - `rust-firmware/components/*/CMakeLists.txt`
  - `.github/workflows/ci.yml`
  - `scripts/build-rust.sh`
  - `scripts/build-secure.sh`
  - `scripts/release.sh`
- 启动、状态机与任务：
  - `rust-firmware/src/main.rs`
  - `rust-firmware/src/app_runner.rs`
  - `rust-firmware/src/ctx.rs`
  - `rust-firmware/src/tasks.rs`
  - `rust-firmware/src/watchdog.rs`
  - `logic/src/app.rs`
  - `logic/src/runtime.rs`
  - `logic/src/event_queue.rs`
- 外设与中断：
  - `rust-firmware/src/board.rs`
  - `rust-firmware/src/wake.rs`
  - `rust-firmware/src/rtc.rs`
  - `rust-firmware/src/rtc_executor.rs`
  - `rust-firmware/src/audio.rs`
  - `rust-firmware/src/audio_task.rs`
  - `rust-firmware/src/nfc.rs`
  - `rust-firmware/src/epd_task.rs`
  - `rust-firmware/components/zectrix_epd/zectrix_epd.cc`
- 网络、BLE、存储、安全与电源：
  - `rust-firmware/src/wifi.rs`
  - `rust-firmware/src/sync.rs`
  - `rust-firmware/src/sync_task.rs`
  - `rust-firmware/src/sync_apply.rs`
  - `rust-firmware/src/storage.rs`
  - `rust-firmware/src/nvs_blob.rs`
  - `rust-firmware/src/ble_control.rs`
  - `rust-firmware/src/power.rs`
  - `rust-firmware/src/boot_ledger.rs`
  - `rust-firmware/src/heap_probe.rs`

### 识别结果

| 项目 | 结论 |
|---|---|
| 固件形态 | Rust `std` + `esp-idf-svc`/`esp-idf-hal`，混合 C/C++ ESP-IDF 组件 |
| ESP-IDF | 明确固定为 `v5.5.5`，见 `rust-firmware/.cargo/config.toml:14` 和 `.github/workflows/ci.yml:70-73` |
| 目标芯片 | `xtensa-esp32s3-espidf`，最终配置为 `CONFIG_IDF_TARGET="esp32s3"` |
| Flash | 16 MB、DIO、80 MHz |
| PSRAM | Octal PSRAM、80 MHz；README 标明目标模组为 ESP32-S3-WROOM-1 N16R8 |
| CPU | 双核，默认 160 MHz；Wi-Fi 固定 Core 1，NimBLE 固定 Core 0 |
| 电源管理 | DFS、tickless idle、automatic light sleep 和 deep sleep |
| 分区 | NVS、PHY、NVS key、单一 factory app、coredump、reserved |
| 升级方式 | USB 本地刷写；不要求 OTA，单 factory 分区符合产品定位 |
| 安全构建 | 存在 Secure Boot V2、Flash 加密、NVS 加密构建脚本 |
| 常规发布 | `release.sh` 发布普通构建，不是 secure 构建 |
| Kconfig / idf_component.yml | 项目自身未提供；依赖由 Cargo、`esp-idf-sys` 和 extra components 管理 |
| 顶层 CMakeLists.txt | 源码树中没有手写版本，由 `esp-idf-sys` 构建过程生成 |

### 实际验证

- `cargo test --locked`：446 个测试全部通过
- `cargo clippy --all-targets --locked -- -D warnings`：通过
- `cargo +stable fmt --check`：通过
- `SOURCE_DATE_EPOCH=1700000000 ./scripts/build-rust.sh --release --locked`：通过
- Release ELF 约 3.7 MB

未进行板上功耗、BLE、Wi-Fi、RTC、EPD 或故障注入验证。涉及实际电气行为、功耗和射频稳定性的结论均需要实机数据确认。

## 1. 执行摘要

1. 整体架构质量明显高于一般 ESP32 应用固件。纯业务状态机与硬件执行层分离，并配有有界队列、异步完成事件和大量主机测试。
2. 睡眠路径设计尤其扎实：prepare/commit token、活动版本、防陈旧完成事件和最终条件复检共同降低了回复未发完、NVS 未落盘或 RTC 未确认就睡眠的风险。
3. EPD、RTC、网络、音频均已从主循环拆到专用任务，避免慢外设和 HTTPS 阻塞 UI/按键主循环；EPD 部分刷新失败还会自动回退全刷。
4. 最大安全风险不是缺少安全能力，而是常规发布流程没有使用已有的 secure profile。普通 release 中 Secure Boot、Flash 加密和 NVS 加密均关闭，而设备会保存 Wi-Fi 密码和服务器 bearer token。
5. 单一 factory app 分区符合极客固件通过 USB 本地刷写的定位。没有 OTA 双槽不是缺陷，保留大块 reserved 空间也有利于后续实验用途。
6. 看门狗策略总体合理，但存在几个边缘问题：主任务订阅失败后仍无条件 feed；RTC 同步请求无限等待；EPD 完成邮箱阻塞时可能触发 TWDT。
7. 中断实现短小、IRAM 化，并使用 task notification 延后处理，符合 ESP32-S3 ISR 约束。
8. PSRAM 使用有明确策略：大于 4 KiB 的一般分配倾向 PSRAM，HTTPS 16 KiB 响应缓存强制 PSRAM，线程栈则强制内部 RAM。
9. CI 已覆盖逻辑测试、格式、release、diagnostic 和 secure 构建，但还缺少硬件在环、静态内存预算和发布包安全配置门禁。
10. 未发现确定会导致普通启动失败、必然数据丢失或稳定死机的代码缺陷。P0 主要取决于公开 release 是否用于保存真实网络凭据和服务器 token。

## 2. 值得肯定的可行设计

### 2.1 纯逻辑状态机与硬件层分离

- 位置：`logic/src/app.rs`、`logic/src/runtime.rs`、`rust-firmware/src/app_runner.rs`
- 设计：业务行为被表达为 `Event -> EffectBatch`，硬件层执行 effect 后再把完成或失败事件送回状态机。
- 为什么好：业务状态不直接依赖 ESP-IDF 驱动，可以在主机上进行确定性测试。异步操作携带 operation ID、render generation 等关联信息，陈旧完成事件不会误推进当前状态。
- 建议：继续保持此边界。未来新增设备能力时，优先加入纯逻辑 effect/event，再接 ESP-IDF 执行器。

### 2.2 睡眠采用两阶段提交和最终复检

- 位置：`logic/src/power_state.rs:135-207`、`rust-firmware/src/main.rs:1075-1098`
- 设计：`prepare()` 产生带 activity version 的 token；commit 前重新检查输入、显示、持久化、网络、协议回复、RTC 唤醒计划、USB 状态和事件队列。新活动会使旧 token 失效。
- 为什么好：可避免刚收到按键、协议回复仍在队列、RTC 尚未完成编程时进入深睡，是可靠低功耗固件中很有价值的竞态防护。
- 建议：未来所有不可中断操作继续纳入 `SleepInputs`，不要绕过统一 admission path。

### 2.3 EPD 慢刷新从主线程隔离，并具备合并和恢复

- 位置：`rust-firmware/src/epd_task.rs:104-176`、`rust-firmware/src/epd_task.rs:303-367`
- 设计：独立 EPD 任务执行实际刷新；待处理刷新使用单一 slot，新请求可以替换或合并旧请求；被替换请求仍产生 terminal completion；部分刷新失败时回退完整刷新。
- 为什么好：墨水屏刷新不会冻结按键、USB、BLE 和状态机。每个 request ID 最终都有终态，部分刷新异常也不会继续依赖不可信的 shadow baseline。
- 建议：继续保持“失败后缓存失效”和“每请求恰好一次终态”的不变量。

### 2.4 RTC 单一所有者任务

- 位置：`rust-firmware/src/rtc_executor.rs:49-67`、`rust-firmware/src/rtc_executor.rs:131-203`
- 设计：PCF8563 仅由高优先级 RTC 任务访问，其他模块通过有界命令队列请求操作。
- 为什么好：避免不同线程交错修改 alarm flag、AIE 和时间寄存器。RTC 优先级为 8，高于显示、网络和持久化任务；alarm snapshot latch 还可防止边沿信息在多次读取间丢失。
- 建议：同类共享状态外设继续采用单一所有者模型，不要让新模块直接获取 RTC I2C 驱动。

### 2.5 ISR 短小并延后处理

- 位置：`rust-firmware/src/wake.rs:16-35`、`rust-firmware/src/board.rs:177-183`、`rust-firmware/sdkconfig.defaults:20`
- 设计：ISR 位于 IRAM，只禁用对应 GPIO 中断、发送 task notification，并按需触发调度。`CONFIG_GPIO_CTRL_FUNC_IN_IRAM=y` 保证 ISR 中使用的 GPIO 控制路径位于 IRAM。
- 为什么好：ISR 内没有日志、动态分配、I2C 操作或复杂状态机，符合 ESP32-S3 中断约束。
- 建议：新增 GPIO、定时器或外设 ISR 时继续保持“确认源 + 原子通知”的最小职责。

### 2.6 内存能力分区意识明确

- 位置：`rust-firmware/src/tasks.rs:18-28`、`rust-firmware/src/sync.rs:22-45`、`rust-firmware/sdkconfig.defaults:12-20`
- 设计：任务栈强制使用 `MALLOC_CAP_INTERNAL | MALLOC_CAP_8BIT`；16 KiB HTTPS 响应缓存明确使用 `MALLOC_CAP_SPIRAM`；大于 4 KiB 的一般堆分配优先 PSRAM，同时保留 32 KiB 内部内存。
- 为什么好：保证任务栈和实时路径不受 PSRAM 延迟影响，同时避免显示帧和网络缓冲快速耗尽内部 RAM。
- 建议：所有 DMA 缓冲继续显式使用 capability allocator，不要只依赖全局 malloc 阈值。

### 2.7 HTTPS 和服务端数据验证

- 位置：`rust-firmware/src/sync.rs:115-173`、`logic/src/sync_validate.rs:41-94`、`rust-firmware/src/storage.rs:62-83`
- 设计：HTTPS 使用 ESP-IDF CA bundle；请求设有 5 秒超时；响应限制为 16 KiB并做溢出探测；服务器 URL 强制 HTTPS 并拒绝嵌入式凭据；同步数据检查重复 ID、日期、闹钟时间和存储容量。
- 为什么好：限制错误或恶意服务器响应造成的无限内存增长，并避免明文 token 传输。
- 建议：继续扩充所有外部字符串字段的长度限制，特别是 inbox title/body 和未来新增字段。

### 2.8 BLE 控制特征要求认证加密

- 位置：`rust-firmware/src/ble_control.rs:889-985`
- 设计：使用 Secure Connections、MITM、动态 passkey；写特征要求 `WRITE_ENC | WRITE_AUTHEN`；读/通知特征要求 `READ_ENC | READ_AUTHEN`；第二个并发客户端会被拒绝。
- 为什么好：Wi-Fi 密码和服务器 token 不会通过未认证的 BLE GATT 链路直接写入设备。
- 建议：新增敏感控制特征时继续要求认证加密，不要只设置普通 `WRITE`。

### 2.9 同步持久化带恢复日志

- 位置：`rust-firmware/src/sync_apply.rs:9-47`
- 设计：应用同步结果前先写 journal，依次更新各存储，全部成功后删除 journal；启动时可以重放。
- 为什么好：设备在跨多个 NVS namespace 更新中途掉电，也能在下次启动恢复到完整结果。
- 建议：未来数据结构发生变化时，为 journal 增加 schema version 和校验摘要。

### 2.10 单 factory 分区符合本项目升级模型

- 位置：`rust-firmware/partitions.csv`
- 设计：使用 4 MiB factory app 分区，不引入 `otadata` 和双 OTA app 槽，剩余空间集中保留。
- 为什么好：本项目明确通过 USB 本地刷写，不需要 OTA。单 app 分区降低了分区、状态切换和回滚逻辑复杂度，也给实验数据或未来极客功能保留了较大空间。
- 建议：保持当前结构；只有在出现明确的本地资源或数据分区需求时再划分 `reserved`，不要预先引入未使用的分区。

### 2.11 构建资产和设备参数有自动门禁

- 位置：`rust-firmware/build.rs:45-95`、`scripts/release.sh:64-97`、`.github/workflows/ci.yml`
- 设计：构建期校验 CJK 字库尺寸、排序和索引唯一性；发布脚本检查 ESP32-S3、16 MB、DIO、80 MHz、coredump 配置和分区二进制一致性；CI 同时构建普通、诊断和 secure profile。
- 为什么好：把硬件参数和资源一致性从人工约定提升为构建失败条件。
- 建议：将发布包的安全配置、镜像大小和设备身份核验也纳入同类门禁。

## 3. 改进建议

### P0 必须修复

#### P0-1 常规发布产物未启用固件与密钥保护

- 位置：`scripts/release.sh:45-47`、`scripts/build-secure.sh:17-27`、普通构建生成的 `sdkconfig`
- 证据：`release.sh` 调用普通 `build-rust.sh --release`。普通构建未启用 `CONFIG_SECURE_BOOT`、`CONFIG_FLASH_ENCRYPTION_ENABLED` 和 `CONFIG_NVS_ENCRYPTION`；设备在 `rust-firmware/src/storage.rs:178-241` 保存 Wi-Fi 密码和 bearer token。
- 影响：能够直接读取 Flash 的人员可以提取网络凭据和服务器 token；未验证签名的固件也可以替换设备程序。
- 建议：明确区分 developer 和 production release。如果公开 release 会保存真实凭据，正式发布应使用 secure profile；如果普通 release 仅供开发实验，应在发布说明中明确其安全边界。
- 示例：

```bash
./scripts/build-secure.sh

require_config 'CONFIG_SECURE_BOOT=y'
require_config 'CONFIG_SECURE_BOOT_V2_ENABLED=y'
require_config 'CONFIG_FLASH_ENCRYPTION_ENABLED=y'
require_config 'CONFIG_NVS_ENCRYPTION=y'
require_config 'CONFIG_SECURE_FLASH_ENCRYPTION_MODE_RELEASE=y'
```

- 验证：检查 secure 最终 `sdkconfig`；在专用测试板验证 eFuse；确认签名错误镜像无法启动；确认原始 Flash dump 中无法搜索出 SSID、密码和 token。

#### P0-2 未加密普通构建中的 coredump 可能暴露敏感运行时数据

- 位置：`rust-firmware/sdkconfig.defaults:109-110`、`rust-firmware/src/sync.rs:142-146`、`rust-firmware/partitions.csv:6`
- 证据：coredump 写入 Flash；HTTPS 请求过程中内存中存在 bearer authorization 字符串；普通发布未启用 Flash 加密。
- 影响：崩溃转储可能包含 token、URL、请求内容或其他敏感运行时数据，物理读取 Flash 后可以离线分析。
- 建议：生产版本只在 Flash 加密开启时启用 coredump，或者将 coredump 限定在明确的 diagnostic profile。
- 验证：在同步过程中触发测试崩溃，导出 coredump，确认生产配置下无法未经设备密钥解析。

如果普通 release 明确只用于个人开发板且不保存真实凭据，上述两项可降为 P1；按当前公开发布形态和凭据存储能力，报告保留 P0 评级。

### P1 建议改进

#### P1-1 RTC 请求等待没有超时

- 位置：`rust-firmware/src/rtc_executor.rs:114-121`
- 证据：发送命令后使用 `replies.recv()` 无限等待。
- 影响：RTC 任务异常退出、回复通道逻辑错误或底层异常时，调用者会永久阻塞。当前主要依赖主任务 TWDT 在约 10 秒后整机重启，恢复粒度过粗。
- 建议：改为 `recv_timeout`，超时后返回明确错误，让状态机决定重试或进入安全模式。
- 示例：

```rust
let reply = replies
    .recv_timeout(Duration::from_millis(500))
    .map_err(|err| anyhow!("RTC request timed out: {err}"))?;
```

- 验证：故障注入让 RTC worker 丢弃一次回复，确认 UI 不会永久冻结，错误能进入已有重试或降级路径。

#### P1-2 主任务 TWDT 订阅失败后仍无条件 feed

- 位置：`rust-firmware/src/main.rs:164-167`、`rust-firmware/src/main.rs:525-527`、`rust-firmware/src/watchdog.rs:9-12`
- 证据：订阅失败只记录 warning；主循环仍每轮调用 `watchdog::feed()`，而 feed 失败也会记录 warning。
- 影响：异常配置下可能形成高频错误日志，额外占用 USB、日志锁和 CPU，并掩盖原始故障。
- 建议：保存 `watchdog_subscribed`，与其他 worker 一样只在订阅成功时 feed；也可在生产配置中把主任务订阅失败视为启动错误。
- 验证：注入 `esp_task_wdt_add` 失败，确认只报告一次，不产生持续日志风暴。

#### P1-3 EPD 完成邮箱的阻塞与 TWDT 策略存在冲突

- 位置：`rust-firmware/src/epd_task.rs:60-67`、`rust-firmware/src/epd_task.rs:261-295`
- 证据：`CompletionMailbox::send()` 在队列满时无限等待；EPD 任务已加入 10 秒 TWDT；等待期间没有 feed。
- 影响：主循环因其他故障暂时不消费完成事件时，EPD 任务可能因正常背压等待而触发系统重启。
- 建议：给条件变量等待增加短周期超时并在等待过程中 feed，或者为执行中的 command 预留 completion slot，使完成路径永不阻塞。
- 验证：停止主线程消费完成队列并连续提交替换刷新，确认不会误触发 TWDT，也不会丢失 terminal completion。

#### P1-4 NimBLE 默认服务未充分裁剪

- 位置：`rust-firmware/sdkconfig.defaults:76-96`、最终 `sdkconfig` 的 NimBLE service 配置段
- 证据：尽管关闭 central/observer，最终配置仍启用 Proximity、ANS、CTS、HTP、TPS、IAS、LLS、SPS、HR、BAS、DIS 等标准服务，而源码只创建自定义 control service。
- 影响：增加 Flash 占用、潜在内部 RAM 使用和协议攻击面，也与“最小 peripheral 配置”的目标不完全一致。
- 建议：逐项关闭未使用服务，只保留 GAP/GATT 和自定义服务必需能力。
- 验证：比较 map 文件中的 NimBLE text/data；对比启动 BLE 前后的内部 heap/largest block；扫描 GATT 服务表确认只暴露预期服务。

#### P1-5 secure 构建只验证可编译，未覆盖生产 eFuse/烧录闭环

- 位置：`scripts/build-secure.sh`、`.github/workflows/ci.yml:120-148`
- 证据：CI 使用临时签名密钥构建，但没有 secure image inspection、eFuse 计划文件、烧录顺序或首次启动验证。
- 影响：构建通过并不代表真实设备不会因密钥摘要、Flash encryption release mode 或烧录顺序错误而失去恢复能力。
- 建议：如果使用 secure profile，建立开发、测试和正式密钥流程；在 CI 检查签名信息；不可逆 eFuse 操作只能在专用流程中执行。
- 验证：在专用测试板完整演练首次烧录、加密重启和 USB 更新。

#### P1-6 烧录入口仍允许只凭串口选择目标

- 位置：`rust-firmware/.cargo/config.toml:6`、`README.md:81-87`
- 证据：Cargo runner 直接执行 `espflash flash --monitor`；README 示例通过串口名烧录，没有先核验 MAC。
- 影响：多块 ESP32 同时连接时，存在把 Note 4 固件烧到 Note 4C 或其他板的风险。
- 建议：默认 runner 调用带设备身份检查的包装脚本。每次烧录前必须确认目标为 ESP32-S3、MAC 为 `20:6E:F1:B4:7D:E4`，并使用 16 MB、DIO、80 MHz 和 `rust-firmware/partitions.csv`；不得只依据 `/dev/ttyACM0` 等名称。
- 验证：同时连接两块 ESP32，选择错误端口时必须拒绝烧录；正确设备仍需校验芯片、MAC 和 Flash 参数。

#### P1-7 动态电源管理缺少板上功耗与时延回归门槛

- 位置：`rust-firmware/sdkconfig.defaults:35-50`、`rust-firmware/src/power.rs:95-132`
- 证据：160→40 MHz DFS、200 ms tickless light sleep 已启用，但仓库没有完整的板上功耗和响应基线证据。
- 影响：USB Serial/JTAG、I2C、BLE/Wi-Fi 初始化和按键响应在 DFS/light sleep 下可能出现板级特有问题，逻辑测试无法发现这些问题。
- 建议：建立 Home idle、USB host、BLE pairing、Wi-Fi sync、EPD refresh 和 deep sleep 的功耗及唤醒时延基线。
- 验证：记录平均/峰值电流、按键到 UI 响应时间和 RTC alarm 唤醒成功率，并设定回归阈值。

### P2 优化项

#### P2-1 发布构建未使用 `--locked`

- 位置：`scripts/release.sh:46`
- 证据：调用 `./scripts/build-rust.sh --release`，而 CI 使用 `--locked`。
- 影响：本地正式发布可能隐式更新依赖解析结果，与 CI 审核过的依赖不完全一致。
- 建议：改为 `./scripts/build-rust.sh --release --locked`。
- 验证：在干净环境执行发布构建，确认不会修改 lockfile 或解析未经审核的新版本。

#### P2-2 默认构建不是完全可复现

- 位置：`rust-firmware/build.rs:17-28`
- 证据：未设置 `SOURCE_DATE_EPOCH` 时把当前时间写入固件。
- 影响：同一提交的两次构建产生不同二进制，不利于供应链审计和发布复核。
- 建议：release 脚本从提交时间设置 `SOURCE_DATE_EPOCH`。
- 示例：

```bash
export SOURCE_DATE_EPOCH="$(git show -s --format=%ct HEAD)"
```

- 验证：在相同工具链环境连续构建两次并比较生成二进制。

#### P2-3 栈水位有运行时日志，但没有自动预算门禁

- 位置：`rust-firmware/src/heap_probe.rs:47-110`
- 证据：主要任务都登记了 handle，低于 2048 bytes 时仅输出 warning。
- 影响：栈回归通常只能在现场日志中发现，CI 无法阻止风险版本。
- 建议：让 smoke test 自动收集 `STACKPROBE`，为每个任务设置最小 high-water mark；根据实测结果缩减过大的任务栈。
- 验证：覆盖 BLE、TLS、EPD、同步和闹钟同时活跃的压力场景。

#### P2-4 Wi-Fi 认证模式硬编码为 WPA2 Personal

- 位置：`rust-firmware/src/wifi.rs:96-105`
- 证据：非空密码固定使用 `AuthMethod::WPA2Personal`。
- 影响：可能降低 WPA3-only 网络兼容性；过度指定认证模式也可能影响 transition mode。
- 建议：按目标网络范围选择自动或 WPA2/WPA3 混合策略，并保留当前 PMF 配置。
- 验证：测试 open、WPA2、WPA2/WPA3 transition 和 WPA3-only AP。

#### P2-5 I2C 驱动有超时，但缺少统一总线恢复

- 位置：`rust-firmware/src/rtc.rs:10,54-75`、`rust-firmware/src/audio.rs:14,79-92`、`rust-firmware/src/nfc.rs:13,53-62`
- 证据：各操作均有 100 tick 超时和错误传播，但没有 SDA/SCL 卡死后的总线 reset/reinstall 策略。
- 影响：外设异常拉低 SDA 时，后续 RTC、音频和 NFC 操作可能持续失败，只能依赖整机恢复。
- 建议：先通过实机统计确认是否存在总线卡死；若可复现，再增加受控 bus recovery，不要对普通 NACK 盲目重置总线。
- 验证：注入 SDA stuck-low、外设断电和时钟拉伸超时。

#### P2-6 `Waker` 的 ISR 上下文依赖永久生命周期

- 位置：`rust-firmware/src/wake.rs:46-62`
- 证据：每个 GPIO 的 `WakeCtx` 通过 `Box::into_raw` 保存，成功注册后没有释放或 unsubscribe。
- 影响：当前板对象全生命周期存在，因此不是实际运行缺陷；但未来若重复构造、销毁或重注册，会产生泄漏或残留 task handle。
- 建议：在类型注释中明确 singleton/lifetime 不变量；只有未来需要动态卸载时再实现 handler remove 和上下文回收。
- 验证：增加防止同一 GPIO 重复订阅的断言或测试。

#### P2-7 主固件缺少目标侧静态分析与组件测试

- 位置：`.github/workflows/ci.yml`
- 证据：`logic` 执行完整 clippy；固件侧执行格式检查和交叉编译；C/C++ EPD 组件没有独立静态分析或组件测试。
- 影响：unsafe FFI、C++ 资源释放和错误路径主要依赖编译器与人工审查。
- 建议：为 EPD rectangle、packing、shadow 算法增加 host-side 测试；对 C/C++ 组件启用更严格警告；条件允许时增加目标工具链 clippy。
- 验证：在 CI 新增独立 job，并确保不会显著延长快速反馈路径。

## 4. 快速收益清单

以下改动通常可在 1–2 小时内完成，风险较低：

1. 将 `release.sh` 改为 `--release --locked`。
2. 为 release 增加最终 `sdkconfig` 安全配置检查或明确标注开发构建属性。
3. 修复主任务 TWDT 订阅状态，订阅失败后不再持续 feed。
4. 为 `RtcExecutor::request()` 增加有限超时。
5. 关闭未使用的 NimBLE 标准服务并比较 map/heap。
6. 用 Git commit timestamp 设置 `SOURCE_DATE_EPOCH`。
7. 在发布脚本中增加 app 分区和镜像大小检查。
8. 将默认 Cargo runner 改为带 MAC、芯片和 Flash 参数验证的安全包装脚本。
9. 在 CI 上传最终 `sdkconfig`，便于审核真正生效的配置。
10. 为栈水位日志增加统一可解析格式，供 smoke test 自动判定。

## 5. 中期与长期建议

### 中期

1. 建立硬件在环 smoke test：
   - RTC 读写和 alarm wake
   - 三键唤醒
   - EPD 全刷、部分刷和失败恢复
   - Wi-Fi TLS sync
   - BLE Secure Connections 配对
   - light/deep sleep 电流
2. 为普通、诊断和安全构建设立清晰配置层：
   - development：便于调试的日志和诊断能力
   - diagnostic：comprehensive heap poisoning 和故障注入
   - secure：Secure Boot、Flash/NVS encryption、受控日志
3. 将任务、优先级、栈和队列容量形成机器可检查的预算表。
4. 补充 NVS schema version、迁移测试和断电故障注入。
5. 为 EPD C++ 驱动增加 host-side 算法测试和 ESP-IDF 组件测试。
6. 将芯片、MAC、分区、签名、镜像大小和 Flash 参数检查整合成统一烧录门禁。

### 长期

1. 建立适合极客固件的可审计 USB 发布流程，重点防止错板、错 Flash 参数和错误安全配置。
2. 如果启用 secure profile，建立 Secure Boot 私钥离线保存、eFuse 操作审计和测试设备演练流程。
3. 对电源策略做基于场景的 PM lock 管理：Wi-Fi、BLE、TLS 和 EPD 活跃时持锁，静态 Home 页面才允许最低频率和自动睡眠。
4. 受控导出 reset reason、TWDT、EPD fallback、队列饱和、最低内部 heap/largest block 等诊断指标。
5. 长期记录 app image、IRAM/DRAM、PSRAM、内部 heap 峰值和任务 stack HWM 的变化趋势。
6. 保持当前状态机架构，避免未来实验功能直接侵入硬件主循环。

## 6. 需要确认的信息

1. 公开 GitHub Release 是否会用于保存真实家庭 Wi-Fi 和服务器 token，还是只用于无敏感数据的实验设备？这决定普通构建的安全问题是否必须维持 P0。
2. 已使用 secure profile 的设备是否有完整 eFuse 烧录和恢复记录？仓库只能确认 secure 构建可生成，不能确认设备实际安全状态。
3. `nvs_keys` 分区在已部署设备上的初始化流程是什么？启用 NVS encryption 后，旧明文 NVS 是否需要保留或迁移？
4. 是否有真实板上任务 stack high-water mark、内部 heap、light/deep sleep 电流、EPD 最长刷新时间和 BLE 初始化峰值数据？
5. PCF8563 INT GPIO5 是否有外部上拉，且在 deep sleep 电源域中持续有效？
6. 普通 release 是否必须保留 coredump？若需要，如何控制转储的提取权限和敏感信息？
7. Wi-Fi 目标环境是否要求 WPA3-only 支持？
8. `reserved` 分区是否有预定的实验用途？如果没有，保持当前整体保留即可，不需要提前切分。

## 7. 复核补充（2026-09-24）

复核基线：`9948383`（与上文基线相比仅新增本报告）。逐条对照源码复核了第 3 节结论，并补充了原报告遗漏的问题。本轮同样未烧录设备、未访问串口。

### 7.1 复核中的实际验证

- `logic/`：`cargo test --locked` 446 个测试全部通过；`cargo fmt --check` 通过；`rust-firmware/`：`cargo +stable fmt --check` 通过。
- `logic/`：本地 clippy 1.94.1 下 `cargo clippy --all-targets --locked -- -D warnings` **失败**：`logic/src/app.rs:1154` 触发 `clippy::nonminimal_bool`（建议写成 `is_none_or(|d| now_ticks < d)`）。GitHub CI 最近一次运行（run 27）为绿，说明结果取决于 `dtolnay/rust-toolchain@stable` 当时解析到的版本，见 7.3 N7。

### 7.2 对第 3 节已有结论的更正

#### 更正 P1-1（RTC 请求等待无超时）→ 建议降为 P2，且原示例修复会引入新缺陷

- 位置：`rust-firmware/src/rtc_executor.rs:114-121`、`:131-204`
- 复核：RTC 任务退出（探测失败 `return`、命令通道断开 `break`）时会 drop 回复端 `SyncSender`，调用方的 `recv()` 返回 `Err` 而不是永久阻塞。真正会“卡住”的只有 RTC 任务卡在 I2C 事务中，而 I2C 已有 100 tick 超时，RTC 任务本身也订阅了 TWDT。
- 风险：原报告建议直接改为 `recv_timeout(500ms)`。当前回复通道是所有调用方共享的 `sync_channel(1)`，回复不带请求序号。超时后迟到的回复会留在通道里，被**下一个**请求取走：类型不同时报 “mismatched reply”；类型相同时（`Acknowledge`/`Disable`/`Program`/`WriteTime` 都返回 `RtcReply::Unit`）会把上一条命令的结果当成本条命令的结果，静默出错。
- 建议：如果要加超时，必须同时给 `RtcCommand`/`RtcReply` 加单调序号并丢弃不匹配的回复，或者每个请求使用独立的 oneshot 回复通道。

#### 更正 P1-3（EPD 完成邮箱阻塞与 TWDT 冲突）→ 建议降为 P2

- 位置：`rust-firmware/src/epd_task.rs:42-67`、`:113-176`、`:272-286`
- 复核：邮箱已有预留机制（`reserve()`/`complete_reserved()`），被替换的请求在提交侧预留槽位，队列满时提交侧直接返回错误，不会阻塞。worker 的 `send()` 只有在 16 个完成事件都没被消费时才会阻塞，前提是主循环已经停止调用 `poll_completion()`。主任务本身订阅了 TWDT，这种情况下主任务的 TWDT 会先触发或同时触发，EPD 的 TWDT 只是重复报告同一个故障，不是误报重启。
- 建议：保留原建议（等待期间带超时并 feed），但优先级低于 7.3 中的新问题。

#### 补充 P1-2（主任务 TWDT 订阅失败后仍 feed）

- 结论成立（`rust-firmware/src/main.rs:164-166`、`:525`）。补充：主循环在非 light sleep 状态下按 `POLL_INTERVAL_MS` 运行，订阅失败后每一轮都会打印一条 `esp_task_wdt_reset failed` warning，日志风暴的频率高于原报告的估计。

### 7.3 新发现

#### N1（P1）多个 worker 的短周期轮询让自动 light sleep 基本无法进入

- 位置：
  - `rust-firmware/src/audio_task.rs:97`：`AudioMode::Idle => thread::sleep(5ms)`，任务常驻，空闲时每 5 ms 醒一次
  - `rust-firmware/src/ble_control.rs:579`：BLE worker `recv_timeout(20ms)`，没有 BLE 会话时也一直轮询
  - `rust-firmware/src/usb_console.rs:65,103`：stdin 无数据时 `sleep(10ms)` 轮询
  - 另有 `rtc_executor.rs:159`、`epd_task.rs:288`、`sync_task.rs:148`、`effect_task.rs:284` 各自 1 s 超时，相位互不对齐
- 证据：`sdkconfig.defaults` 设置 `CONFIG_FREERTOS_IDLE_TIME_BEFORE_SLEEP=200`，tickless idle 只在预计空闲时间 ≥ 200 ms 时才进入 light sleep。只要有任务每 5 ms 或 20 ms 醒一次，预计空闲时间就远低于阈值。`sdkconfig.defaults` 注释里“每秒约 0.8 s 处于 light sleep”的前提因此不成立。主循环的 `wake.wait(1000)`（`main.rs:1185-1187`）本身设计正确，但不能单独决定芯片是否睡眠。
- 影响：电池续航可能远低于设计预期，而逻辑测试和 CI 都发现不了。这也是原报告 P1-7（缺少功耗基线）背后一个具体的、可以直接修复的原因。
- 建议：
  - audio：Idle 时改为阻塞等待（`Condvar` 或 channel `recv()`），只在播放时进入 5 ms 节拍。
  - BLE worker：无会话时阻塞 `recv()`；会话期间再使用短超时。
  - USB 读线程：安装 `usb_serial_jtag` 驱动并让 VFS 使用阻塞读，或者只在 `usb_host_connected()` 为真时轮询，否则阻塞在一个事件上。
  - 1 s 心跳：TWDT 为 10 s，可以把 worker 空闲超时拉长到 3–4 s，减少唤醒次数。
- 验证：开启 `CONFIG_PM_PROFILING`，用 `esp_pm_dump_locks()` 或 sleep 统计比较修改前后的 light sleep 占比；在 Home 静止页面测平均电流。

#### N2（P1）同步进行中修改闹钟/待办，修改会被静默丢弃

- 位置：`logic/src/app.rs:2362-2386`（闹钟切换没有检查 `state.sync`）、`rust-firmware/src/sync.rs:191-203`（请求发出前快照 dirty 集合）、`logic/src/app.rs:2893`（应用时 `state.alarms.alarms = data.alarms`）、`rust-firmware/src/sync_apply.rs:41-46`
- 过程：
  1. 同步任务读取本地闹钟和 dirty 集合，发出 HTTPS 请求。
  2. 请求进行中，用户在闹钟列表切换闹钟 X。`PersistAlarmToggle` 保存新值并标记 X dirty。
  3. 服务器响应基于第 1 步上传的状态。`ApplySyncedData` 用服务器列表覆盖 NVS 和内存状态。
  4. 如果 X 已经在第 1 步的 dirty 集合里，`clear_dirty_ids` 会清掉 X，用户的第二次修改彻底丢失。如果 X 不在集合里，dirty 标记还在，但本地值已被服务器旧值覆盖，下一次同步上传的是旧值，修改同样丢失。
- 待办的 `PersistTodoEdit`（`app.rs:2560`）是同一条路径。
- 建议（二选一）：
  - 简单：`state.sync != SyncState::Idle` 时拒绝或排队本地编辑，并在 UI 上提示“同步中”。
  - 完整：记录每条记录的本地编辑代数。apply 时对快照之后又被编辑过的 ID 保留本地值、保留 dirty，只清除值与上传内容一致的 dirty ID。
- 验证：在 `logic/src/harness.rs` 中编写脚本：`SyncStarted` → 切换闹钟 → 注入 `SyncFetched` → 断言闹钟值和 dirty 状态都保留。

#### N3（P2）本地编辑先保存后标记 dirty，中途掉电会丢失上传意图

- 位置：`rust-firmware/src/effect_task.rs:187-217`
- 证据：先执行 `AlarmStore::save`/`TodoStore::save`，再执行 `mark_dirty`。两次 NVS 写入之间掉电时，新值已经落盘但没有标记 dirty，永远不会上传，下一次同步还会被服务器值覆盖。
- 建议：先 `mark_dirty` 再 `save`。反过来的失败情况只是多上传一次未变化的值，不会造成数据丢失。

#### N4（P2）light sleep 唤醒源不包含 RTC 中断脚，与配置注释不符

- 位置：`rust-firmware/src/power.rs:24,120-133`、`rust-firmware/src/board.rs:180`、`rust-firmware/sdkconfig.defaults` PM 段注释
- 证据：`WAKE_PINS = [GPIO0, GPIO18, GPIO39]`，light sleep GPIO 唤醒和 `Waker` ISR 都不包含 PCF8563 INT（GPIO5），但 sdkconfig 注释写的是“Keys and the RTC alarm line wake it”。当前闹钟靠主循环每秒轮询发现，延迟不超过约 1 s，功能上没问题。但如果按 N1 把空闲周期拉长，闹钟响铃延迟会随之变大。
- 建议：把 GPIO5 加入 light sleep 唤醒源和 `Waker`，或者修正注释并明确“闹钟延迟上限 = 空闲轮询周期”。

#### N5（P2）BLE passkey 在射频启用前生成

- 位置：`rust-firmware/src/ble_control.rs:594`（`esp_random()`）早于 `BLEDevice::init()`（`:869`）
- 证据：ESP32-S3 上 Wi-Fi/BT 都没有启用、也没有调用 `bootloader_random_enable()` 时，ESP-IDF 文档说明 `esp_random()` 的输出应视为伪随机。
- 影响：6 位 passkey 是 BLE 配对防 MITM 的唯一秘密，熵不足会削弱认证加密的前提。
- 建议：在 `BLEDevice::init()` 之后生成 passkey（`set_passkey` 本来就在 init 之后调用），或者在生成前后包裹 `bootloader_random_enable()`/`disable()`。

#### N6（P2）`.esp32-review.yml` 的例外清单已经过期

- 位置：`.esp32-review.yml`
- 证据：清单允许 `rust-firmware/src/board.rs:276` 和 `rust-firmware/src/wifi.rs:44`。当前 `board.rs` 已经没有 `Peripherals::steal()`（电池 ADC 改为持有句柄），`wifi.rs` 中唯一的调用点在第 52 行。
- 影响：按该文件的设计，审查工具会把 `wifi.rs:52` 报成新的违规，而已删除的 `board.rs` 例外还留着，削弱了例外清单的可审计性。
- 建议：删除 `board.rs:276`，把 `wifi.rs:44` 改为 `wifi.rs:52`，并同步修改文件头注释。

#### N7（P2）logic CI 工具链未固定，clippy 结果随 stable 变化

- 位置：`.github/workflows/ci.yml` 的 `logic` job
- 证据：使用 `dtolnay/rust-toolchain@stable` 且没有版本号，clippy 也没加 `--locked`。本地 clippy 1.94.1 已经在 `logic/src/app.rs:1154` 报错（见 7.1）。
- 建议：修复该处 lint（一行改动），并在 CI 中固定工具链版本（例如 `@1.94.1` 或 `logic/rust-toolchain.toml`），定期主动升级；clippy 同样加上 `--locked`。

### 7.4 更新后的优先级建议

1. N1（light sleep 被轮询打断）和 N2（同步期间编辑丢失）直接影响续航和用户数据，应排在原 P1 列表前面。
2. 原 P0-1、P0-2 的结论和前提（第 6 节第 1 问）保持不变。
3. N3、N5、N6、N7 都是小于 1 小时的改动，可以并入第 4 节的快速收益清单。

## 8. 修复状态（2026-09-24）

所有修复都在 `docs/esp32-s3-firmware-review` 分支上。在本地 ESP-IDF 5.5.5 + `esp` 1.98.1 工具链上完成了以下验证：release、diagnostic 和 secure 三种 profile 都构建通过，固件侧 `cargo clippy -D warnings` 通过，boot ledger 镜像检查通过，`logic/` 460 个测试和 clippy 都通过。**未烧录设备，未做实机验证。**

| 条目 | 状态 | 处理方式 |
|---|---|---|
| P0-1 发布产物无固件与密钥保护 | 已修复（边界明确化） | 公开 release 仍是 developer 构建：用 secure profile 发布会在每台用户设备首次启动时烧写不可逆 eFuse，并把设备绑定到维护者的签名密钥。`release.sh` 现在强制检查 `CONFIG_SECURE_BOOT` 和 Flash 加密均未开启，release notes 与 README 写明安全边界（每台设备使用独立 token，丢失后吊销）。需要锁定设备的用户可用自己的密钥构建 secure profile。 |
| P0-2 coredump 可能暴露敏感数据 | 已修复 | 显式设置 `CONFIG_ESP_COREDUMP_CAPTURE_DRAM=n`：转储只包含任务栈和 TCB，不含保存 Wi-Fi 密码和 token 的堆。`release.sh` 与 `logic` 契约测试都会拒绝开启该项的构建。 |
| P1-1 RTC 请求无超时 | 已修复 | 请求和回复带序号，超时 1 s；迟到的回复按序号丢弃，不会被下一个请求误收。 |
| P1-2 看门狗未订阅仍 feed | 已修复 | `watchdog::feed()` 遇到 `ESP_ERR_NOT_FOUND` 时不再告警。 |
| P1-3 EPD 完成邮箱阻塞 | 已修复 | 等待改为每 4 s 超时一次，等待期间喂狗。 |
| P1-4 NimBLE 标准服务未裁剪 | 已修复 | 关闭 12 个示例服务，并加了契约测试。实测镜像体积不变：这些服务从未被引用，链接器原本就会丢弃它们。 |
| P1-5 secure 构建只验证可编译 | 已修复 | secure profile 现在生成带 `--secure-pad-v2` 的应用镜像，用 `espsecure` 签名，再校验应用和 bootloader 的签名以及最终安全配置。原流程根本没有签名应用，开启 Secure Boot 的设备会拒绝启动。换用其他密钥或未签名镜像都会被校验拒绝。 |
| P1-6 烧录只凭串口名 | 已修复 | 新增 `scripts/flash-note4.sh`：先核对芯片为 ESP32-S3、Flash 为 16 MB、MAC 为授权 Note 4，再按 16 MB DIO 80 MHz 和 `partitions.csv` 烧录。cargo runner 与 README 改用该脚本，release notes 增加身份核验步骤。 |
| P1-7 功耗基线 | 部分完成 | diagnostic profile 开启 `CONFIG_PM_PROFILING`，主循环每 60 s 输出各电源模式和 PM lock 的驻留时间，可作为 light sleep 占比基线。电流与唤醒时延仍需实机测量。 |
| P2-1 / P2-2 release 可复现性 | 已修复 | release 构建加 `--locked`，并以提交时间设置 `SOURCE_DATE_EPOCH`。 |
| P2-3 栈水位门禁 | 已修复 | 每 10 s 输出一行 INFO 级 `STACKPROBE <stage> task=free ...`；`smoke-note4.py --min-stack-free` 统计各任务最低余量并据此判定。 |
| P2-4 Wi-Fi 认证 | 已修复（原结论需更正） | `auth_method` 是可接受的最低认证方式，WPA2 已兼容 WPA2/WPA3 过渡模式和 WPA3-only AP。真正的缺口是 `sae_pwe_h2e` 被零值初始化为 UNSPECIFIED，现已显式设为 `WPA3_SAE_PWE_BOTH`。 |
| P2-5 I2C 总线恢复 | 先统计 | 遵循原报告的建议，先不盲目复位总线。新增 `bus_timeout` / `bus_error` 两个诊断计数，用来区分总线被占住和设备 NACK；有实机数据后再决定是否加入恢复逻辑。 |
| P2-6 `Waker` 生命周期 | 已修复 | 同一 GPIO 重复订阅会被拒绝，并在代码中写明 `WakeCtx` 是有意泄漏的。 |
| P2-7 目标侧静态分析 | 已修复 | CI 增加固件 clippy（`-D warnings`）并上传最终 `sdkconfig`；EPD 与 recorder 两个组件启用 `-Wextra -Wsign-compare -Wunused-parameter -Wshadow -Werror`；矩形合并与打包算法移入 `inkwash_logic::epd_geometry` 并补了主机测试。 |
| N1–N7 | 已修复 | 见对应提交。N4 只修正注释：GPIO5 为电平唤醒，在闹钟被确认前会反复唤醒芯片。 |
| 其他 | 已修复 | `check-boot-ledger.sh` 同时兼容 esptool 4 和 5 的 `image_info` 输出（原先在 esptool 5 下会误报失败）；`release.sh` 增加应用镜像是否放得进 factory 分区的检查。 |
