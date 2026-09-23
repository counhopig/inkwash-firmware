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
