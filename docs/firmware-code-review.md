# ESP32-S3 固件代码审查报告

## 0. 审查范围与假设

### 审查范围

本报告基于仓库中的实际代码、配置及构建产物，重点审查了以下内容：

- 构建与目标配置：`rust-firmware/Cargo.toml`、`Cargo.lock`、`.cargo/config.toml`、`rust-toolchain.toml`、`build.rs`、`sdkconfig.defaults`、`partitions.csv`
- ESP-IDF 组件：`components/zectrix_epd/`、`components/p06_recorder/`
- 启动与主循环：`src/main.rs`、`app_runner.rs`、`ctx.rs`
- RTOS 与任务：`tasks.rs`、`effect_task.rs`、`epd_task.rs`、`rtc_executor.rs`、`sync_task.rs`、`audio_task.rs`、`ble_control.rs`、`usb_console.rs`
- 外设与电源：`board.rs`、`rtc.rs`、`power.rs`、`wake.rs`、`display.rs`、`audio.rs`、`nfc.rs`
- 网络与协议：`wifi.rs`、`sync.rs`、`control.rs`、`logic/src/protocol.rs`
- 持久化与安全：`storage.rs`、`nvs_blob.rs`、分区表及实际生成的 `sdkconfig`
- 纯逻辑层：`logic/src/` 中的状态机、事件队列、同步校验、闹钟、电源状态、渲染计划和测试
- CI 与发布：`.github/workflows/ci.yml`、`scripts/build-rust.sh`、`scripts/release.sh`

仓库不是传统的顶层 ESP-IDF C 工程，而是 Rust `cargo` + `esp-idf-sys/embuild` 工程，因此没有项目级 `CMakeLists.txt`、`idf_component.yml`、`sdkconfig` 或 `Kconfig`。最终 `sdkconfig` 由 `esp-idf-sys` 根据 `rust-firmware/sdkconfig.defaults` 生成。

### 已确认的目标和构建参数

| 项目 | 结果 |
|---|---|
| ESP-IDF | v5.5.5 |
| 目标 | ESP32-S3，Xtensa，双核 |
| 构建方式 | Rust Cargo + ESP-IDF CMake 子构建 |
| Flash | 16 MB，DIO，80 MHz |
| PSRAM | Octal PSRAM，80 MHz，通用堆模式 |
| CPU 默认频率 | 160 MHz |
| BLE | NimBLE Peripheral，最多一个连接 |
| TLS | mbedTLS 完整证书包 |
| 分区 | NVS 24 KiB、PHY 4 KiB、factory 4 MiB、LittleFS 约 11.9 MiB |
| OTA | 无 OTA 数据分区和双应用槽 |
| Secure Boot | 未启用 |
| Flash/NVS 加密 | 未启用 |

### 实机验证条件

当前可用条件仅为一台电脑、一台 Zectrix Note 4 和一根 USB 连接线。本报告中的“当前可验证”项目限定为 USB 身份核对、串口日志、单元测试、静态分析、构建、单设备功能与重启恢复。需要 JTAG 调试器、功耗分析仪、可控电源、第二台客户端或第二台设备的项目均标记为外部验证，不作为当前修复的完成前提。

### 验证结果

- `cargo test --locked`：414 项测试通过
- `cargo fmt --check`：通过
- `cargo clippy --all-targets -- -D warnings`：通过
- 固件 `cargo +stable fmt --check`：通过
- `scripts/build-rust.sh --release`：ESP32-S3 release 交叉构建成功
- 当前 release 应用镜像为 2,659,232 字节，占 4 MiB factory 分区的 63.40%
- USB 只读身份核验确认当前 `/dev/ttyACM0` 的设备序列号/MAC 为 `20:6E:F1:B4:7D:E4`，与授权 Zectrix Note 4 一致
- 设备曾出现 USB CDC/JTAG 在线但应用无响应；当次没有保存 panic PC，因此根因仍未确认
- 重置后按授权配置烧录当前 release 固件：ESP32-S3、16 MB、DIO、80 MHz、`rust-firmware/partitions.csv`；烧录前再次核对 MAC，未擦除 NVS
- 烧录后日志确认主循环、RTC、EPD、同步、音频、Effect 和 USB 任务均能启动，各任务观测到的剩余栈约 6.2–19.8 KiB
- 高频 `set_timezone` 压力仍可复现 Double exception：最新 200 次运行完成 178 次，首次异常约在第 133 次；完整日志为 `/tmp/inkwash-smoke-iram-200.log`。因此当前镜像不能判定为稳定发布版
- 显式使用构建产物 `bootloader.bin` 刷写后，实机二级 bootloader 与应用均报告 ESP-IDF v5.5.5；启动日志同时确认 DIO 和 16 MB Flash
- 最终构建刷写后的基础 smoke 为 10/10：状态查询、校时、重复命令缓存、非法参数拒绝、配置恢复及短时无 panic/WDT/reset均通过
- Wi-Fi 连接超时后固件恢复自动 Light Sleep；新配置下 CPU retention 申请错误不再出现，日志准确表述为“已请求自动 Light Sleep，唤醒源已配置”
- 未进行实机功耗、BLE 安全交互、掉电或故障注入测试

## 1. 执行摘要

1. 整体架构质量较高。业务状态机、Effect 执行层和硬件任务边界清晰，复杂异步操作有显式完成反馈和代际检查。
2. 并发设计经过了较多压力场景考虑。RTC、EPD、同步、BLE、音频和 USB 均采用独立执行上下文，多数队列具有固定容量和背压处理。
3. 纯逻辑层具备 414 项主机测试，是本项目最值得保留的工程资产之一。
4. 最大安全风险是 BLE 控制通道没有认证、绑定、MITM 或加密访问要求，却能执行网络配置、服务端配置、清除闹钟和修改时钟等命令。
5. Wi-Fi 密码和服务端 Bearer Token 存储于普通 NVS，而 Secure Boot、Flash Encryption 和 NVS Encryption 均未启用。
6. 服务端 URL 现已在状态机入口强制 HTTPS、长度上限和无 userinfo；这项安全边界有单元测试覆盖。
7. panic 策略已设为打印后自动重启，可避免同类故障永久停机；尚需补充重启循环识别。
8. 当前分区布局不支持 OTA、升级失败回滚或远程安全维护。
9. Light Sleep 与 tickless idle 已启用；ESP-IDF 5.5.5 会忽略 CPU retention 内存申请失败，当前配置已显式关闭未实际生效的 CPU-domain power-down，保留自动 Light Sleep 和唤醒源。运行态最低频率仍为 160 MHz。
10. 同步应用会连续更新多个 NVS 对象而没有事务或 generation 标记；掉电或写入失败可能留下跨数据集的部分提交状态。
11. 真机已稳定复现命令压力下的上下文/栈破坏：异常返回地址出现 `0xA5A5A5A5`，最新一次另一核处于 idle，说明问题尚未关闭，必须作为发布阻断项。
12. 现有刷写文档和发布脚本已显式携带同一次构建生成的 v5.5.5 bootloader，消除了 `espflash` 内置 v6.1 beta bootloader 与应用版本混用。
13. 建议优先完成内存破坏定位、BLE 授权和敏感数据保护，再推进 OTA、原子持久化与功耗优化。
14. 同步层虽然保存了 ETag，却没有发送 `If-None-Match`，且只接受 HTTP 200；当前 ETag 机制实际上没有形成条件请求闭环。
15. 启动阶段对 Todo、Inbox、设备配置、Wi-Fi 和时区的部分 NVS 读取错误会静默降级为空值，存在把“数据损坏”误判成“尚未配置”的风险。
16. 生产配置仍启用 UART core dump 和综合堆毒化：前者可能经 USB 暴露内存中的密码与 Token，后者适合定位当前 P0，但不宜直接作为最终量产配置。

## 2. 值得肯定的可行设计

### 2.1 状态机与硬件副作用分离

- 位置：`logic/src/app.rs`、`logic/src/runtime.rs`、`rust-firmware/src/app_runner.rs`
- 设计：应用状态通过事件更新，硬件动作建模为 `Effect`；异步结果以携带 `operation_id`、`effect_id` 和 `render_generation` 的完成事件回灌。
- 为什么好：减少 UI、网络、RTC 和显示之间的隐式耦合，能识别陈旧完成事件，并允许绝大部分业务逻辑脱离硬件测试。
- 建议：继续保持“状态只能经事件修改”的约束；OTA、电池低压和安全状态也应通过同一模型接入。

### 2.2 睡眠采用 prepare/commit 两阶段准入

- 位置：`logic/src/power_state.rs`、`rust-firmware/src/main.rs:2111`
- 设计：进入睡眠前检查输入、显示刷新、持久化、网络、协议回复、RTC 计划和事件队列，并使用带代际的 `SleepToken` 防止陈旧提交。
- 为什么好：避免回复或 NVS 写入尚未完成时睡眠；新输入会使旧 token 失效，降低睡眠竞态。
- 建议：引入 OTA 后，将镜像写入和首次启动确认状态纳入相同准入机制。

### 2.3 RTC 单一所有者模型

- 位置：`rust-firmware/src/rtc_executor.rs:63`、`rtc_executor.rs:151`
- 设计：PCF8563 仅由 RTC executor 操作，其他任务通过容量为 8 的队列访问；I2C 操作有 100 tick 超时。
- 为什么好：避免并发寄存器读改写，告警标志还通过 latch 防止异步处理中丢失。
- 建议：共享 I2C 上的其他外设应继续统一超时、仲裁和恢复策略。

### 2.4 EPD 刷新异步化并具备失败恢复

- 位置：`rust-firmware/src/epd_task.rs:108`、`:217`、`:279`
- 设计：显示刷新在独立任务执行；待处理刷新可以合并或替换；局刷失败后使用同一帧回退全刷。
- 为什么好：数秒级墨水屏操作不阻塞主循环；被替换请求仍收到 terminal completion；失败后不会继续信任错误的旧图基线。
- 建议：保留 completion reservation 和 render generation 设计，并补充 EPD 故障注入测试。

### 2.5 EPD 底层驱动正确约束 DMA 和超时

- 位置：`rust-firmware/components/zectrix_epd/zectrix_epd.cc:136`、`:210`、`:539`
- 设计：BUSY 等待有超时；帧大小和矩形边界经过验证；DMA 缓冲区使用 `MALLOC_CAP_DMA | MALLOC_CAP_INTERNAL`；公共入口有互斥保护。
- 为什么好：避免把 PSRAM 或不具备 DMA 能力的地址交给 SPI DMA，控制器无响应也不会永久等待。
- 建议：新增 SPI/I2S DMA 路径时继续使用 capability-aware 分配。

### 2.6 有界队列和背压处理完整

- 位置：`logic/src/event_queue.rs`、`rust-firmware/src/epd_task.rs`、`sync_task.rs`、`usb_console.rs`
- 设计：关键通道采用固定容量队列，区分 Full 与 Disconnected；关键事件饱和时由生产者保留并重试。
- 为什么好：避免慢网络或慢显示导致无界堆增长，也减少静默丢失关键完成事件的风险。
- 建议：统一增加队列高水位、拒绝和重试计数。

### 2.7 同步响应有明确的内存和语义边界

- 位置：`rust-firmware/src/sync.rs:20`、`:89`、`logic/src/sync_validate.rs:41`
- 设计：响应限定为 16 KiB，显式检测溢出；大缓冲区从 PSRAM 分配；写入前验证重复 ID、日期、闹钟时间、重复规则和 NVS 容量。
- 为什么好：限制异常服务器响应造成的内存压力，防止业务非法数据进入持久化层。
- 建议：继续增加文本长度、显示行数和 UTF-8 渲染预算校验。

### 2.8 TLS 使用系统证书包

- 位置：`rust-firmware/src/sync.rs:125`
- 设计：`EspHttpConnection` 配置 `esp_crt_bundle_attach`，生成配置确认启用了完整证书包。
- 为什么好：HTTPS 请求能够验证公共 CA 签发的服务端证书，而不是跳过验证。
- 建议：强制 HTTPS 后保留此实现；更高安全等级可增加证书或公钥固定。

### 2.9 已有堆和任务栈运行期观测

- 位置：`rust-firmware/src/heap_probe.rs:28`、`:78`
- 设计：记录 internal、DMA、PSRAM 空闲量和最大连续块，并输出任务 stack high-water mark。
- 为什么好：能区分总堆不足和连续内存碎片问题，对 BLE 内部堆门槛尤其有价值。
- 建议：发布配置改为阈值告警或按需诊断，减少长期日志和唤醒开销。

### 2.10 主机测试覆盖复杂时序

- 位置：`logic/src/`、`.github/workflows/ci.yml`
- 设计：测试覆盖告警重入、渲染代际、事件饱和、同步确认、BLE 会话和睡眠竞态。
- 为什么好：这些问题难以仅靠实机手工操作稳定复现；主机测试显著降低状态机回归风险。
- 建议：保留纯逻辑层无硬件依赖的边界，并补充少量 ESP-IDF 组件测试和硬件在环测试。

### 2.11 已落实的可靠性与安全边界

- `logic/src/app.rs:3424`、`:3571`：`SetServer` 在进入持久化前拒绝非 HTTPS、超过 240 字节、空 authority、带 userinfo 或 authority 含空白的 URL；对应的拒绝且不写入测试已纳入 414 项主机测试。
- `rust-firmware/src/wifi.rs:60`：`connect()` 的所有失败出口都会执行 `disconnect()`，避免连接或 DHCP 超时后遗留驱动状态。
- `rust-firmware/src/ble_control.rs:961`：BLE 命令解析失败只记录长度和错误，不再输出可能含 Wi-Fi 密码或 Token 的原始 payload。
- `rust-firmware/sdkconfig.defaults:97`：prod panic 策略为 `CONFIG_ESP_SYSTEM_PANIC_PRINT_REBOOT=y`；生成的 sdkconfig 已确认 halt 关闭、reboot 开启。
- `rust-firmware/sdkconfig.defaults:39`：显式关闭 IDF 5.5.5 不能可靠初始化的 CPU retention，保留自动 Light Sleep；真机连接超时后成功恢复 Light Sleep，不再出现 retention 内存错误。
- `rust-firmware/components/zectrix_epd/zectrix_epd.cc`：OTP 刷新不再在运行期重新配置并释放共享 SPI bus，消除了已稳定复现的 `spi_bus_deinit_lock` 断言路径。
- `rust-firmware/src/wake.rs`、`board.rs`、`sdkconfig.defaults`：GPIO 唤醒 ISR、ISR 服务和 GPIO 控制函数均配置为 IRAM-safe；ELF 已确认 `wake_isr` 与 `gpio_intr_disable` 位于 `0x4037xxxx` IRAM 区间。
- `rust-firmware/src/rtc_executor.rs`、`ctx.rs`：RTC 一次性回复改为有界通道；校时后不再立即析构仍在飞行的接收端，而是排空后释放。
- `rust-firmware/src/tasks.rs`：pthread 默认配置在修改前保存，创建 internal-stack worker 后恢复，避免全局线程栈策略泄漏到后续线程。
- `rust-firmware/sdkconfig.defaults`：15,000 字节显示帧优先进入 PSRAM；Wi-Fi RX/TX 缓冲数量按本设备短连接负载下调，减轻 DMA/internal heap 压力。
- `README.md`、`scripts/release.sh`、`docs/verification.md`：刷写流程显式指定同次构建的 bootloader；冷启动已确认 bootloader 和应用均为 ESP-IDF v5.5.5。发布附件包含 ELF、bootloader 和分区表。
- 以上状态通过 fmt、clippy、414 项单元测试、release 交叉构建和授权真机启动日志验证。

## 3. 改进建议

### P0 必须修复

#### P0-1 BLE 控制接口缺少认证和加密要求

- 位置：`rust-firmware/src/ble_control.rs:931`、`:943`；`logic/src/protocol.rs:9`
- 证据：写特征只声明 `NimbleProperties::WRITE`；未设置 passkey、bonding、MITM、Security Manager 或 encrypted-write 权限；实际配置为 `CONFIG_BT_NIMBLE_SM_LVL=0`。BLE 命令包含 `SetWifi`、`SetServer`、`SetRtc` 和 `ClearAlarms`。
- 影响：配对页面开启期间，附近任意 BLE 客户端可能修改敏感配置或设备状态。限制为单连接不能替代身份授权。
- 建议：至少启用 LE Secure Connections + MITM，写特征要求加密和认证；增加短时有效、屏幕显示的应用层配对码或 challenge。
- 示例：

```rust
// 伪代码，具体 API 名称需按 esp32-nimble 0.12 核对。
BLEDevice::set_security_auth(true, true, true); // bonding, MITM, SC
BLEDevice::set_security_passkey(displayed_passkey);

create_characteristic(
    WRITE_CHAR_UUID,
    NimbleProperties::WRITE | NimbleProperties::WRITE_ENC,
);
```

- 验证：未配对客户端写入必须失败；测试错误 passkey、重连、删除 bond、第二客户端和 ATT 抓包。

#### P0-2 敏感凭据存入未加密 NVS

- 位置：`rust-firmware/src/storage.rs:55`、`:70`；`rust-firmware/sdkconfig.defaults`
- 证据：Wi-Fi 密码和 Bearer Token 分别写入 `wifi_pass`、`auth_token`；实际生成配置未启用 Secure Boot、Flash Encryption 或 NVS Encryption。
- 影响：通过物理读取 Flash、恶意固件或未限制的调试接口可恢复网络凭据和服务端 Token，固件也没有可信启动链。
- 建议：生产配置启用 Secure Boot V2、Flash Encryption 和加密 NVS；开发与生产配置分离，量产流程单独管理 eFuse。
- 验证：离线读取 Flash 不应出现密码或 Token 明文；未签名固件应无法启动；验证量产、升级和恢复流程。

#### P0-3 真机压力测试仍存在上下文/栈破坏

- 位置：`rust-firmware/src/main.rs`、`ctx.rs`、`rtc_executor.rs`、`effect_task.rs`、`epd_task.rs` 及 Rust/C/FFI 边界。
- 证据：在授权实机上连续执行 `set_timezone` 已多次复现 `LoadProhibited` 或 Double exception。修复 SPI bus 释放、Wi-Fi 失败重试、RTC 回复析构竞态、PSRAM 阈值和 GPIO ISR IRAM 后，最新 200 次测试仍只完成 178 次；异常前第 133 次命令已回复，Core 0 回溯损坏且 A0 为 `0xA5A5A5A5`，Core 1 明确在 idle。各已注册任务 high-water mark 仍有至少约 6.2 KiB。生成配置启用了综合堆毒化，但未在异常前报告普通 heap corruption。
- 事实边界：崩溃落点不等于越界写入点；填充模式吻合也不能单独证明是未初始化读取、栈溢出或释放后使用。现有证据只能确认发生过内存安全故障，不能把责任归于 `CommandSessions`、TLS 或某一轮修改。
- 影响：普通控制命令压力即可触发设备崩溃；在启用自动重启后可能演变成重启循环或在写 NVS 时复位。
- 建议：下一步给每个任务记录 TCB、栈起止地址和当前 core，把异常 SP `0x3fcb37xx` 映射到具体任务；随后对该任务的所有 FFI 写入和通道生命周期做二分。保留固定 ELF、完整 UART core dump和综合堆毒化，不再用崩溃最终 PC 直接推断写坏点。
- 验证：至少连续 1,000 次同一压力命令零 panic/WDT/reset，并在混合 EPD、NVS、Wi-Fi 和 USB 场景重复；保存 ELF SHA、map、sdkconfig、原始日志和 core dump。未达到前不得关闭此 P0。

### P1 建议改进

#### P1-1 分区布局不支持 OTA 和回滚

- 位置：`rust-firmware/partitions.csv`
- 证据：只有单一 `factory` 应用分区，没有 `otadata`、`ota_0`、`ota_1`，仓库中也未发现 OTA 实现。
- 影响：发布后不能安全远程升级，现场缺陷依赖有线刷写，升级失败时也没有自动回滚路径。
- 建议：若产品需要远程维护，改为 `otadata + ota_0 + ota_1`，实现 HTTPS OTA、镜像签名、首次启动自检和 `esp_ota_mark_app_valid_cancel_rollback`。
- 风险：双 4 MiB 应用槽约占 8 MiB，会显著压缩当前 LittleFS 空间。
- 验证：执行断电注入、损坏镜像、首次启动崩溃、版本降级和空间不足测试。

#### P1-2 Light Sleep 的 CPU power-down 与动态内存策略不兼容

- 位置：`rust-firmware/src/main.rs` 电源管理和唤醒源配置路径；`rust-firmware/sdkconfig.defaults` 的 PM/tickless 配置
- 证据：授权真机每次启用 Light Sleep 时都输出 `Failed to enable CPU power down during light sleep`。IDF 源码 `components/esp_hw_support/lowpower/port/esp32s3/sleep_cpu.c:166` 显示 CPU retention 需要 `MALLOC_CAP_RETENTION` 连续内存，失败返回 `ESP_ERR_NO_MEM`；`components/esp_pm/pm_impl.c:504` 调用 `esp_pm_sleep_configure(config)` 却没有检查返回值，最终 `esp_pm_configure()` 仍返回 `ESP_OK`。
- 影响：固件会把部分失败当作成功，CPU 电源域实际没有关闭；反复启停还会在内部堆上分配和释放 retention 内存。
- 建议：`sdkconfig.defaults` 已显式设置 `CONFIG_PM_POWER_DOWN_CPU_IN_LIGHT_SLEEP=n`，保留自动 Light Sleep，并将应用日志改为只陈述请求状态和唤醒源配置。若未来要恢复 CPU-domain power-down，应在启动早期一次性预留 retention 内存，并先修补或升级 IDF 的错误传播。
- 验证：串口已确认 CPU retention 错误消失，Wi-Fi 超时后自动 Light Sleep 恢复。当前还可手动验证 GPIO0/18/39 唤醒、RTC 闹钟和 USB 重连。实际 idle/Light Sleep 电流属于需要功耗分析仪的外部验证，不作为当前完成前提。

#### P1-3 Wi-Fi 和服务端配置不是原子提交

- 位置：`rust-firmware/src/storage.rs:65`、`:84`
- 证据：SSID 与密码、URL 与 Token 分别写入两个 NVS key；中途掉电可只更新其中一项。
- 影响：设备可能启动到新旧配置混合状态，导致认证失败或把旧 Token 发送到新地址。
- 建议：将成组配置序列化为单个带版本的 blob；需要更强掉电一致性时使用 staging key 和 active generation。
- 验证：在每个 NVS 写入点注入复位，重启后应只看到完整旧配置或完整新配置。

#### P1-4 DFS 实际未生效

- 位置：`rust-firmware/src/power.rs:106`
- 证据：`max_freq_mhz` 和 `min_freq_mhz` 都等于默认 CPU 频率，实际为 160 MHz。
- 影响：自动 Light Sleep 有效，但运行态或短空闲期不能降频。
- 建议：实测后将最低频率设为 40 或 80 MHz；EPD、I2S、Wi-Fi 活跃阶段使用 PM lock 保证时钟要求。
- 示例：

```rust
let config = esp_pm_config_t {
    max_freq_mhz: 160,
    min_freq_mhz: 40,
    light_sleep_enable: enabled,
};
```

- 验证：分别测量 Home 空闲、按键、音频、Wi-Fi 和 EPD 刷新场景的电流及响应时间。

#### P1-5 RTC 读取值缺少合法性校验

- 位置：`rust-firmware/src/rtc.rs:60`
- 证据：BCD 转换后直接构造 `DateTime`，没有验证秒、分、时、月、日和 weekday 范围。
- 影响：I2C 干扰、寄存器损坏或未初始化值可能进入日期、调度和告警算法。
- 建议：读取后执行统一的 `DateTime::validate()`；非法值进入重新校时或安全降级流程。
- 验证：mock 非法 BCD、月 0、日 0、2 月 31 日和 weekday 7。

#### P1-6 任务优先级和核心亲和性没有明确策略

- 位置：`rust-firmware/src/tasks.rs` 及各任务的 `thread::Builder` 调用
- 证据：任务只设置名称和栈大小，没有显式 priority 或 affinity；仅 Wi-Fi 系统任务在配置中固定到 Core 1。
- 影响：任务调度依赖 pthread 默认值，无法明确保证 RTC、告警音频、UI 和后台同步的响应顺序。
- 建议：先记录实际 task priority/core，再以最小调整保证 RTC 和告警控制高于后台同步；不要在缺少测量时大范围提高优先级。
- 验证：Wi-Fi TLS、EPD 全刷和音频同时运行时测量按键和告警响应延迟。

#### P1-7 EPD 任务未加入 Task Watchdog

- 位置：`rust-firmware/src/epd_task.rs:255`
- 证据：主任务、RTC、同步、音频和 Effect worker 会订阅或喂 WDT，EPD worker 没有订阅。
- 影响：虽然 BUSY 等待有超时，但 SPI 驱动、互斥锁或其他未预见阻塞仍可能让显示任务永久失效而不触发恢复。
- 建议：为 EPD 任务加入 WDT，并在长刷新过程的受控边界喂狗；避免在不可控无限等待中简单订阅。
- 验证：模拟 BUSY 常高、SPI 错误和队列堵塞。

#### P1-8 BLE 断开重连后可能永久拒绝回复

- 位置：`rust-firmware/src/ble_control.rs:104`、`:126`、`:147`
- 证据：`NotifyAttemptMailbox` 使用 8 KiB 的 `retired_handles: [u64; 1024]`；`quarantine()` 和 `release_generation()` 会置位，但当前实现没有任何清位路径。`arm()` 会永久拒绝退休 handle。
- 影响：若 NimBLE 在同一 `BleSession` 内重连时复用 `conn_handle`，新连接的所有 notify 回复都会被拒绝。简单清位又可能让旧连接的迟到回调错误消费新 attempt，因此不能直接删除退休机制。
- 建议：先建立可靠的旧 notify 回调排空边界，或更换不依赖仅有 `conn_handle` 的完成关联模型；在确认回调归属前不要采用“连接时清位”的表面修复。
- 验证：在同一配对会话中反复断开/重连，记录每代 `conn_handle`、generation、attempt_id 和 notify 回调。如果 handle 被复用，应确认新请求仍能收到回复且旧回调不能完成新请求。

#### P1-9 同步数据应用存在跨 key 部分提交

- 位置：`rust-firmware/src/effect_task.rs:104`
- 证据：`ApplySyncedData` 依次写 alarms、todos、inbox、pending-read ack、alarm dirty set、todo dirty set 和 ETag。任一步失败只会返回错误，不会回滚前面已成功的 NVS 写入。
- 影响：掉电、NVS 空间不足或单次写失败可能形成“新 alarms + 旧 todos”“内容已覆盖但 dirty 标记未清”或“数据已写但 ETag 未推进”等跨集合不一致。状态机当次不会确认成功，但重启后会读到部分新数据。
- 建议：为同步快照增加 generation/commit marker。先写带同一 generation 的 staging 数据，全部成功后原子切换 active generation；最小方案至少应最后写 commit marker，并在启动时只接纳完整 generation。
- 验证：在每个写入步骤后注入复位和错误，重启后必须得到完整旧快照或完整新快照，不能出现混合状态。

#### P1-10 Inbox pending-read 集合无条目上限且双写不原子

- 位置：`rust-firmware/src/inbox.rs:42`、`:75`
- 证据：`mark_read()` 对 `pending` 只做去重后 `push`，没有数量或序列化大小上限；随后先写 `items` 再写 `pending`。两者共享 4096 字节上限，但只对 inbox items 做截断和预算控制。
- 影响：长时间离线或服务端不确认 read ack 时，pending ID 会持续增长，最终使 `KEY_PENDING` 写入失败；写入失败前 `KEY_ITEMS` 可能已标记为已读，形成 UI 状态与待上传确认不一致。
- 建议：为 pending 集合设置由序列化预算推导的硬上限；将 items 和 pending 合并为一个版本化 blob，或使用 generation/commit marker。达到上限时应保留最旧未确认项并显式告警，不能静默丢失。
- 验证：生成超过容量的唯一 ID、在两次写入之间注入复位，并测试服务端部分 ack、重复 ack 和长期离线场景。

#### P1-11 启动时将持久化读取失败误当作空配置

- 位置：`rust-firmware/src/main.rs:1313-1333`
- 证据：Todo、Inbox 读取失败后直接使用空列表；`device_config()`、`wifi_creds()` 的错误经 `.ok().flatten()` 丢弃；时区读取失败回退为 UTC。相比之下，Alarm 读取失败会阻止构造正常启动快照。
- 影响：NVS 损坏、字符串超出读缓冲或格式迁移失败时，设备可能表现为数据被清空或配置丢失；后续操作还可能把默认值写回，降低数据恢复机会。
- 建议：区分 `NotFound` 与 `Corrupt/TooLong/DecodeError`。仅缺少 key 时使用默认值；其余错误进入明确的降级模式并保留原始数据，至少在 UI/USB 状态中暴露持久化故障。
- 验证：分别注入 key 不存在、JSON 损坏、超长字符串和 NVS 读错误，确认只有第一种使用默认值，其余情况不会被报告为“未配置”。

#### P1-12 ETag 被保存但没有用于条件请求

- 位置：`rust-firmware/src/sync.rs:179-239`、`:294`
- 证据：`fetch_and_apply()` 的参数名为 `_etag` 且未使用；调用 `https_post()` 时额外 header 为空。HTTP 层只接受状态码 200，标准条件请求的 304 会被当作错误。
- 影响：每次同步都会下载完整数据，浪费流量、功耗和 Flash 写入次数；代码和持久化状态会让维护者误以为已经支持增量缓存。
- 建议：发送 `If-None-Match`，显式把 304 映射为 `NotModified`，且 304 时不重写业务数据；若服务端协议并不支持 ETag，则删除该状态，避免伪机制。
- 验证：服务端依次返回 200+ETag、304、更新后的 200，确认请求头、状态机、NVS 写入次数和同步时间符合预期。

#### P1-13 UART core dump 会暴露运行内存中的凭据

- 位置：`rust-firmware/sdkconfig.defaults:112`；`rust-firmware/src/storage.rs:55-86`；`rust-firmware/src/sync.rs:144-147`
- 证据：配置启用 `CONFIG_ESP_COREDUMP_ENABLE_TO_UART=y`；运行期内存会包含 Wi-Fi 密码、Bearer Token 和 HTTP Authorization header，USB Serial/JTAG 控制台无需身份认证即可读取 panic 输出。
- 影响：获得设备短时物理访问或串口日志的人可能从 core dump 提取敏感信息。当前 core dump 对定位 P0-3 很有价值，但不适合作为默认量产策略。
- 建议：保留专用诊断 profile；量产 profile 改为加密 Flash 中的 core dump 分区，或关闭完整 core dump、仅保留脱敏后的复位原因和故障计数。
- 验证：对诊断转储执行字符串扫描，确认风险范围；量产镜像触发受控 panic 后不得在 USB 输出 RAM 内容。

#### P1-14 RTC 允许范围在时区换算后越出芯片年份范围

- 位置：`logic/src/app.rs:3339-3353`；`rust-firmware/src/rtc.rs:60-86`
- 证据：`SetRtc` 先验证 UTC epoch 位于 2000-01-01 至 2100-01-01，再叠加 `[-12h,+14h]` 时区。边界值可变为 1999 或 2100；PCF8563 写入使用 `dt.year % 100`，读取固定加 2000。
- 影响：例如 2000-01-01 UTC 配负时区会写成 `99` 并读回 2099；2100 边界会读回 2000，造成日期、闹钟和同步调度跨世纪错误。
- 建议：对换算后的本地时间再次验证为 2000-01-01 00:00:00 至 2099-12-31 23:59:59，并让 `rtc.write_time()` 自身也拒绝越界或非法字段。
- 验证：覆盖最小/最大 UTC、-720/+840 分钟、闰日及 2099 年末的单元测试和 RTC 写读回测试。

#### P1-15 音频失败路径可能让功放保持开启

- 位置：`rust-firmware/src/audio.rs:153-195`
- 证据：代码先拉高 `pa_enable`，之后 `i2s.tx_enable()?` 可提前返回而不执行清理；正常清理中的 `tx_disable()` 和 `pa_enable.set_low()` 错误被直接忽略。
- 影响：I2S 或 GPIO 异常时功放可能持续上电，带来静态功耗、噪声和热风险，且上层只看到原始错误而不知道清理是否失败。
- 建议：用作用域 guard 保证所有出口关闭功放；合并并记录主错误与清理错误。关闭功放应优先于等待 drain。
- 验证：注入 `tx_enable`、写入、`tx_disable` 和 GPIO 失败，逐项确认最终 PA 电平和错误日志。

#### P1-16 BLE 生命周期回调可被有界邮箱无限阻塞

- 位置：`rust-firmware/src/ble_control.rs:205-213`
- 证据：`send_lifecycle()` 在队列达到 16 项时通过条件变量无限等待；该函数由 NimBLE 连接/断开等回调调用。只有主循环消费后才会唤醒。
- 影响：主循环停顿或饱和时会阻塞 NimBLE host 回调线程，进而拖住断连、协议栈清理和反初始化；这与回调应短小且不可无限等待的约束冲突。
- 建议：回调只写入非阻塞、预分配邮箱；用序号/状态压缩保证最终连接状态不丢，并把可靠重放放到 worker 线程，禁止在 NimBLE 回调等待主循环。
- 验证：暂停主循环并制造超过 16 次生命周期事件，确认 NimBLE host 仍可运行、停止和重新启动，且最终状态一致。

#### P1-17 服务端 Token 缺少与 NVS 缓冲一致的长度校验

- 位置：`logic/src/app.rs:3424-3462`；`rust-firmware/src/storage.rs:21`、`:70-86`；`rust-firmware/src/usb_console.rs` 的 512 字节命令行上限
- 证据：URL 限制为 240 字节，但 Token 没有独立上限；写入使用不定长 `set_str`，读回固定 256 字节缓冲。协议允许构造超过读缓冲能力但仍小于整行上限的 Token。
- 影响：配置命令可能当次持久化成功，重启后却因缓冲不足无法读取；结合 P1-11 又会被静默表现为服务端未配置。
- 建议：在协议入口按 UTF-8 字节数限制 Token，并使限制小于 NVS 读缓冲扣除终止符后的容量；写入层也应重复校验。
- 验证：测试 254、255、256 字节及多字节 UTF-8 Token 的写入、重启读回和错误回复。

### P2 优化项

#### P2-1 CI 不执行固件交叉构建

- 位置：`.github/workflows/ci.yml`
- 证据：CI 执行逻辑测试、Clippy 和格式检查；固件只检查格式。
- 影响：ESP-IDF API、C/C++ 组件、链接和分区问题可能直到人工发布时才暴露。
- 建议：增加带缓存的 ESP-IDF/Rust-ESP release build job，至少在 nightly 或合并前执行。
- 验证：CI 上传 ELF、map、partition table，并检查应用镜像不超过分区容量。

#### P2-2 发布脚本缺少最终目标参数校验

- 位置：`scripts/release.sh:27`
- 证据：发布前只检查 ELF 存在，没有解析生成配置和分区表确认 ESP32-S3、16 MB、DIO、80 MHz 及指定布局。
- 影响：环境或默认值漂移时可能发布错误目标产物。
- 建议：发布脚本自动校验最终 sdkconfig 和 partition-table.bin，并生成 SHA-256 与构建元数据。
- 验证：人为改变一个 Flash 参数，发布脚本应立即失败。

#### P2-3 构建不可复现

- 位置：`rust-firmware/build.rs:14`
- 证据：每次构建都会把当前 UNIX 时间写入 `BUILD_EPOCH_SECS`。
- 影响：同一提交和工具链重复构建会产生不同二进制，不利于供应链审计。
- 建议：优先读取 `SOURCE_DATE_EPOCH`，仅在开发构建中回退到当前时间。
- 验证：指定相同 `SOURCE_DATE_EPOCH` 连续构建并比较镜像哈希。

#### P2-4 ISR 没有请求立即任务切换

- 位置：`rust-firmware/src/wake.rs:15`
- 证据：`xTaskGenericNotifyFromISR` 返回 `higher_prio_woken`，但 ISR 没有执行 `portYIELD_FROM_ISR`。
- 影响：通知可能等到下一调度 tick 才被处理；当前 1 kHz tick 下通常为毫秒级影响。
- 建议：若实测需要降低唤醒延迟，在 `higher_prio_woken != 0` 时使用 ESP-IDF Xtensa 对应的 ISR yield API。
- 验证：使用逻辑分析仪测量 GPIO 边沿到任务响应的最坏延迟。

#### P2-5 pthread 栈策略恢复与并发保护

- 位置：`rust-firmware/src/tasks.rs:7`
- 证据：原实现曾在修改后才读取“恢复值”，导致 internal-stack 策略泄漏；现已改为修改前保存并在 spawn 后恢复，但修改/创建/恢复仍没有全局串行保护。
- 影响：当前启动顺序下已消除确定性泄漏；若未来从多个任务并发创建线程，仍可能互相覆盖全局 pthread 配置。
- 建议：用全局互斥封装修改、spawn、恢复三步，或统一所有 Rust 任务创建入口。
- 验证：记录每个任务的 stack capabilities，并做并发创建压力测试。

#### P2-6 周期诊断日志偏频繁

- 位置：`rust-firmware/src/main.rs:463`、`:467`
- 证据：每秒采集电源状态，每 10 秒逐任务输出 stack high-water mark。
- 影响：长期运行时会增加串口输出、格式化和唤醒开销。
- 建议：发布配置降低频率，或只在低栈、低内存和状态变化时输出。
- 验证：比较诊断开启和关闭时的平均电流及自动 Light Sleep 占比。

#### P2-7 缺少应用镜像体积门禁

- 位置：`.github/workflows/ci.yml`、`scripts/release.sh`、`rust-firmware/partitions.csv`
- 证据：当前 release 应用镜像为 2,659,232 字节，占 4,194,304 字节 factory 槽的 63.40%；当前 CI 和发布脚本均未检查镜像占用比例。
- 影响：目前仍有约 1.47 MiB 余量，但新增 TLS、安全启动、OTA 或字体资源后可能持续增长；超限会在发布或刷写阶段才暴露。
- 建议：CI 生成应用镜像并设置绝对上限和预警阈值，例如超过分区 85% 时失败、75% 时告警；采用 OTA 双槽后按 OTA 槽的新尺寸重新设定门禁。
- 验证：CI 输出镜像字节数、分区字节数和百分比，并用人为缩小分区的测试确认门禁生效。

#### P2-8 本地 C/C++ 组件变更可能未触发 Cargo 增量重建

- 位置：`rust-firmware/build.rs`；`rust-firmware/components/zectrix_epd/`、`components/p06_recorder/`
- 证据：项目 `build.rs` 只声明跟踪 Cargo 配置、自身和 Git ref；本次审查中修改外部组件源后，增量构建没有重新编译对应 C++，必须清理 ESP32-S3 target 后才得到新产物。
- 影响：开发者可能测试或发布旧的 native object，而误以为源码改动已经进入 ELF。这是构建正确性风险，不只是构建速度问题。
- 建议：为两个组件源、头文件和 `CMakeLists.txt` 建立明确的 rerun/reconfigure 依赖；发布构建至少使用隔离的新 target 目录，并验证 ELF 中的版本标记或对象时间戳。
- 验证：只改一个 C/C++ 可观测常量，执行普通增量构建，确认对应对象被重编译且 ELF 行为变化。

#### P2-9 发布流程可能留下已推送但未完成的版本标签

- 位置：`scripts/release.sh:35-56`
- 证据：脚本在检查 `gh` 登录状态、目标 release 是否可创建以及附件上传是否成功前，先创建并向 `origin` 推送 tag；向 `github` 推送的失败还被 `|| true` 忽略。
- 影响：发布失败会留下远端正式标签但没有完整 Release/附件，重试时语义不清晰；标签也未检查是否指向当前 HEAD、工作区是否干净。
- 建议：先完成所有本地校验和 `gh auth status`，确认 tag 不冲突且提交正确；最后阶段再创建/推送不可变标签和 Release。失败时明确报告每个远端状态，不吞错。
- 验证：在未登录、同名 tag 指向其他提交、附件上传失败和第二远端不存在的情形运行 dry-run。

#### P2-10 Flash 备份脚本没有核验授权设备身份

- 位置：`scripts/backup-flash.ps1:1-16`
- 证据：脚本仅接收串口名并直接读取完整 16 MiB Flash，没有读取和比对 MAC、芯片型号或 Flash 容量。
- 影响：多设备环境中可能对错误 ESP32 执行操作；虽然读取本身不改写 Flash，但生成的文件会被错误标记为 Note 4 工厂备份，并破坏恢复链可信度。
- 建议：执行任何读写前解析 `esptool chip_id/flash_id`，强制匹配授权 Note 4 MAC `20:6E:F1:B4:7D:E4`、ESP32-S3 和 16 MiB，并把身份元数据与哈希写入备份清单。
- 验证：授权设备可备份；错误 MAC、非 S3、容量不符和无法识别身份时必须在读取前退出。

#### P2-11 约 11.9 MiB storage 分区当前未被使用

- 位置：`rust-firmware/partitions.csv:5`；全仓库文件系统挂载路径
- 证据：`storage` 数据分区占 `0xBF0000`，但固件没有 LittleFS/SPIFFS/FAT 挂载或读写代码；持久化全部位于 24 KiB NVS。
- 影响：当前约 74.6% Flash 空间没有承担运行功能，同时 NVS、OTA 和 core dump 空间仍紧张。
- 建议：先确认产品路线；若无需用户文件，重新分配给双 OTA、加密 core dump 或更宽裕的 NVS。不要仅为“利用空间”引入文件系统。
- 验证：用生成的 partition table 二进制核对偏移、大小和无重叠，并对刷写、升级和数据保留策略做回归。

#### P2-12 字体索引与字模资源缺少构建期一致性校验

- 位置：`rust-firmware/src/font_cjk.rs:14-58`；`tools/generate_cjk_font.py`
- 证据：索引读取本身按 4 字节条目遍历，但 `glyph16()`/`glyph12()` 使用索引中的 cell 直接切片，没有检查字模长度；资源错配会在渲染时 panic。
- 影响：受控内置资源目前风险较低，但生成脚本、合并或打包错误会变成设备运行期崩溃。
- 建议：生成或构建阶段检查 index 长度为 4 的倍数、排序唯一、最大 cell 同时落入两套字模范围，并生成校验摘要。
- 验证：对截断字模、越界 cell、乱序和重复 code point 建立生成器失败测试。

#### P2-13 ADC 通道通过 `Peripherals::steal()` 重建所有权

- 位置：`rust-firmware/src/board.rs:278-284`
- 证据：每次电池采样都以 `unsafe { Peripherals::steal() }` 重新取得 GPIO4 token，而不是在 `Board` 初始化时建立并持有 ADC channel。
- 影响：当前 GPIO4 未见其他使用，未发现实际冲突；但该模式绕过 HAL 单例所有权，使未来引脚复用或并发访问无法由类型系统阻止。
- 建议：初始化时创建并保存 ADC channel，采样时只借用；把 `steal()` 限制在确有底层所有权证明的集中边界。
- 验证：编译期确认 GPIO4 不能被第二个驱动取得，并连续执行 ADC、Wi-Fi、EPD 并发压力测试。

#### P2-14 量产配置与诊断配置尚未分离

- 位置：`rust-firmware/sdkconfig.defaults:110-116`
- 证据：默认配置同时启用综合堆毒化、INFO 日志和 UART core dump。这些设置适合当前 P0 定位，但会增加运行开销、日志暴露和 panic 后恢复时间。
- 影响：单一 profile 难以同时满足故障定位和量产的性能、安全、恢复时延要求。
- 建议：保留当前 diagnostic profile 直至 P0-3 关闭；另建 production defaults，明确日志级别、core dump 去向、堆检查级别和安全启动参数。两种 profile 都应纳入 CI 构建。
- 验证：比较两种镜像的体积、内部堆、性能和故障输出，并确保生产 profile 不泄露敏感内存。

## 4. 快速收益清单

以下改动通常可在 1–2 小时内完成，且风险较低：

1. RTC 读取结果增加日期和时间范围校验。
2. 确认生成的 sdkconfig 中 CPU-domain power-down 已关闭，并用串口确认原 retention 申请错误不再出现。
3. 发布脚本检查芯片、Flash 容量、模式、频率和分区表。
4. CI 保存 release 产物大小、map 文件和分区表。
5. STACKPROBE 改为诊断开关或低栈阈值告警。
6. 为队列 Full、BLE reply retry 和 EPD fallback 增加累计计数。
7. `build.rs` 支持 `SOURCE_DATE_EPOCH`。
8. 为 Inbox pending-read 增加序列化容量检查和明确错误日志。
9. 在 CI 中加入应用镜像占用率门禁；当前基线为 2,659,232 / 4,194,304 字节（63.40%）。
10. 让 smoke 脚本轮询设备 ready，而不是依赖固定 0.3 秒串口等待；USB 关闭会触发延迟复位，现有脚本可能误报首次 `get_status` 超时。

## 5. 中期与长期建议

### 中期

- 将 Wi-Fi 和服务端配置改为单 blob、带版本、可原子切换的持久化格式。
- 在 CI 中加入完整固件交叉构建、map 检查和分区容量检查。
- 建立 RTC I2C、EPD BUSY、BLE 未授权访问和 Wi-Fi 失败清理的组件测试。
- 明确任务优先级、核心亲和性和 internal/PSRAM 栈策略。
- 复用现有 smoke 和串口采集脚本建立硬件在环测试。
- 对 NVS 提交执行掉电注入测试。
- 为同步快照和 Inbox 状态设计 generation/commit marker，消除多 key 部分提交。
- 建立 BLE 断开重连测试，验证 `conn_handle` 复用和迟到 notify 回调归属。
- 对历史内存破坏建立固定版本、固定脚本、固定证据格式的发布阻断回归门禁。

### 长期

- 实现双槽 OTA、签名验证、首启确认和自动回滚。
- 建立 Secure Boot V2、Flash Encryption、NVS Encryption 和 eFuse 量产流程。
- 建立启动失败计数和安全模式，避免 panic 重启循环。
- 将功耗策略演进为 DFS、Light Sleep、Deep Sleep 和 PM lock 的场景化模型。
- 为服务端通信增加设备身份、Token 轮换、重放保护和可选证书固定。
- 建立 SBOM、依赖审计、可复现构建和签名发布流程。

## 6. 需要确认的信息

1. BLE 配对页面是否被视为“物理在场即授权”，还是必须抵御附近陌生客户端。
2. 产品是否计划支持远程 OTA；如不支持，现场升级和故障恢复流程是什么。
3. 量产流程是否已规划 Secure Boot、Flash Encryption、NVS Encryption 和 eFuse 烧写。
4. LittleFS 的实际用途及最低容量要求。
5. 服务端是否只允许 HTTPS，以及使用公共 CA、私有 CA 还是证书固定。
6. USB 控制接口是否只在受控维修环境开放。
7. 实机长期运行的 stack high-water mark、内部堆最低值和最大连续块数据。
8. EPD、音频和 Wi-Fi 同时工作的峰值电流及电源设计余量。
9. 量产是否需要独立 sdkconfig profile，以分离诊断、日志、堆毒化和量产恢复策略。
10. 是否存在未纳入仓库的硬件在环、OTA、安全配置或量产脚本。
