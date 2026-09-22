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
| 分区 | NVS 64 KiB、OTA metadata 8 KiB、双 4 MiB OTA 槽、core dump 512 KiB、LittleFS 7.375 MiB |
| OTA | 双槽与 bootloader 回滚已启用；远程下载协议尚未实现 |
| Secure Boot | 未启用 |
| Flash/NVS 加密 | 未启用 |

### 实机验证条件

当前可用条件仅为一台电脑、一台 Zectrix Note 4 和一根 USB 连接线。本报告中的“当前可验证”项目限定为 USB 身份核对、串口日志、单元测试、静态分析、构建、单设备功能与重启恢复。需要 JTAG 调试器、功耗分析仪、可控电源、第二台客户端或第二台设备的项目均标记为外部验证，不作为当前修复的完成前提。

### 验证结果

- `cargo test --locked`：420 项测试通过
- `cargo fmt --check`：通过
- `cargo clippy --all-targets -- -D warnings`：通过
- 固件 `cargo +stable fmt --check`：通过
- `scripts/build-rust.sh --release`：ESP32-S3 release 交叉构建成功
- 当前 release 应用镜像为 2,668,368 字节，占 4 MiB OTA 槽的 63.62%
- USB 只读身份核验确认当前 `/dev/ttyACM0` 的设备序列号/MAC 为 `20:6E:F1:B4:7D:E4`，与授权 Zectrix Note 4 一致
- 设备曾出现 USB CDC/JTAG 在线但应用无响应；当次没有保存 panic PC，因此根因仍未确认
- 重置后按授权配置烧录当前 release 固件：ESP32-S3、16 MB、DIO、80 MHz、`rust-firmware/partitions.csv`；烧录前再次核对 MAC，未擦除 NVS
- 烧录后日志确认主循环、RTC、EPD、同步、音频、Effect 和 USB 任务均能启动，各任务观测到的剩余栈约 6.2–19.8 KiB
- 高频 `set_timezone` 压力曾稳定复现 Double exception。RTC executor 改为启动时创建的复用回复通道后，最终连续 1,000/1,000 次 RTC/NVS 提交及 60 秒 soak 通过，12/12 检查无 panic、WDT、reset 或串口断线；同值时区确认不再触发无意义的 EPD 全刷
- 显式使用构建产物 `bootloader.bin` 刷写后，实机二级 bootloader 与应用均报告 ESP-IDF v5.5.5；启动日志同时确认 DIO 和 16 MB Flash
- 最终构建刷写后的基础 smoke 与 400 秒 soak 均为 11/11：状态查询、校时、重复命令缓存、非法参数拒绝、配置恢复、串口连续性及无 panic/WDT/reset 均通过
- NVS 扩容和同步 journal 版本刷写后保留了原 Wi-Fi、服务端及时区配置；10/10 持久化压力与 11/11 smoke 通过，期间两次 HTTPS 同步均成功应用包含 20 条 inbox 的快照
- 显式任务优先级版本再次通过 10/10 持久化压力和 30 秒 soak，11/11 smoke 全通过；期间无 panic、WDT、reset 或串口断线
- Wi-Fi 连接超时后固件恢复自动 Light Sleep；新配置下 CPU retention 申请错误不再出现，日志准确表述为“已请求自动 Light Sleep，唤醒源已配置”
- 未进行实机功耗、BLE 安全交互、掉电或故障注入测试；当前 Linux 环境没有 PowerShell，`backup-flash.ps1` 仅完成静态审查，尚未在 Windows 上执行

## 1. 执行摘要

1. 整体架构质量较高。业务状态机、Effect 执行层和硬件任务边界清晰，复杂异步操作有显式完成反馈和代际检查。
2. 并发设计经过了较多压力场景考虑。RTC、EPD、同步、BLE、音频和 USB 均采用独立执行上下文，多数队列具有固定容量和背压处理。
3. 纯逻辑层具备 420 项主机测试，是本项目最值得保留的工程资产之一。
4. BLE 控制通道已要求 LE Secure Connections、MITM、动态六位 passkey，以及加密并认证的读写权限；该边界应通过真实客户端继续做负向互操作验证。
5. Wi-Fi 密码和服务端 Bearer Token 存储于普通 NVS，而 Secure Boot、Flash Encryption 和 NVS Encryption 均未启用。
6. 服务端 URL 现已在状态机入口强制 HTTPS、长度上限和无 userinfo；这项安全边界有单元测试覆盖。
7. panic 策略已设为打印后自动重启，可避免同类故障永久停机；尚需补充重启循环识别。
8. 当前分区布局具备双 OTA 槽和回滚元数据，应用会在核心外设、NVS、任务及状态机启动成功后确认待验证镜像；仓库与服务端尚无远程 OTA 下载协议。
9. Light Sleep、tickless idle 与 40–160 MHz 动态调频已启用；ESP-IDF 5.5.5 会忽略 CPU retention 内存申请失败，当前配置已显式关闭未实际生效的 CPU-domain power-down，保留自动 Light Sleep 和唤醒源。
10. 同步应用先持久化完整 journal，再更新各 NVS namespace，全部成功后清除 journal；启动在读取业务状态前强制重放未完成事务，避免掉电后暴露跨数据集混合状态。
11. RTC 高频分配器崩溃路径已经移除，并通过 1,000 次连续持久化门槛；测试器现会检测任意相邻 uptime 回退、应用重启、部分完成和 USB 写超时。
12. 现有刷写文档和发布脚本已显式携带同一次构建生成的 v5.5.5 bootloader，消除了 `espflash` 内置 v6.1 beta bootloader 与应用版本混用。
13. 建议优先完成 BLE 安全互操作测试和敏感数据保护，再推进 OTA 传输协议与功耗优化。
14. 当前 `POST /api/sync` 是带本地变更的合并操作，服务端不会按 ETag 返回 304；固件已移除无效的 ETag 请求状态和 NVS 写入，避免伪缓存机制与额外 Flash 磨损。
15. 启动阶段对 Todo、Inbox、设备配置、Wi-Fi 和时区的部分 NVS 读取错误会静默降级为空值，存在把“数据损坏”误判成“尚未配置”的风险。
16. 完整 core dump 已从 UART 移至 512 KiB Flash 分区，避免 panic 时直接向 USB 主机输出凭据；综合堆毒化仍适合当前 P0 定位，不宜直接作为最终量产配置。

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

- `logic/src/app.rs:3424`、`:3571`：`SetServer` 在进入持久化前拒绝非 HTTPS、超过 240 字节、空 authority、带 userinfo 或 authority 含空白的 URL；对应的拒绝且不写入测试已纳入 420 项主机测试。
- `rust-firmware/src/wifi.rs:60`：`connect()` 的所有失败出口都会执行 `disconnect()`，避免连接或 DHCP 超时后遗留驱动状态。
- `rust-firmware/src/ble_control.rs:961`：BLE 命令解析失败只记录长度和错误，不再输出可能含 Wi-Fi 密码或 Token 的原始 payload。
- `rust-firmware/src/ble_control.rs:857`、`:944`：BLE 使用动态六位 passkey、LE Secure Connections、MITM 和 bonding；控制特征要求加密且认证后才能读写。
- `rust-firmware/sdkconfig.defaults:97`：prod panic 策略为 `CONFIG_ESP_SYSTEM_PANIC_PRINT_REBOOT=y`；生成的 sdkconfig 已确认 halt 关闭、reboot 开启。
- `rust-firmware/sdkconfig.defaults:39`：显式关闭 IDF 5.5.5 不能可靠初始化的 CPU retention，保留自动 Light Sleep；真机连接超时后成功恢复 Light Sleep，不再出现 retention 内存错误。
- `rust-firmware/components/zectrix_epd/zectrix_epd.cc`：OTP 刷新不再在运行期重新配置并释放共享 SPI bus，消除了已稳定复现的 `spi_bus_deinit_lock` 断言路径。
- `rust-firmware/src/wake.rs`、`board.rs`、`sdkconfig.defaults`：GPIO 唤醒 ISR、ISR 服务和 GPIO 控制函数均配置为 IRAM-safe；ELF 已确认 `wake_isr` 与 `gpio_intr_disable` 位于 `0x4037xxxx` IRAM 区间。
- `rust-firmware/src/rtc_executor.rs`、`ctx.rs`：RTC executor 使用单个容量为 1 的可复用回复通道，所有克隆句柄通过互斥接收端串行完成请求；250 ms 告警轮询不再反复创建和销毁一次性通道，瞬时 I²C 错误记录后留待下次轮询重试。
- `rust-firmware/src/rtc.rs`、`logic/src/app.rs`：RTC 读写均校验 2000–2099 年、真实月日、weekday 和时分秒范围；`SetRtc` 在时区换算后再次检查 PCF8563 可表达年份。
- `rust-firmware/src/audio.rs`：播放和初始化失败都会进入统一清理路径，按顺序关闭 I²S 与功放，并组合报告主错误和清理错误。
- `rust-firmware/src/main.rs:1296`：启动快照对 Todo、Inbox、设备配置、Wi-Fi 和时区读取错误统一返回失败，仅在 key 确实不存在时采用默认值。
- `rust-firmware/src/ble_control.rs:204`：BLE 生命周期回调使用非阻塞、定长并保留最新状态的邮箱；队列饱和时压缩状态，不阻塞 NimBLE host 回调线程。
- `logic/src/app.rs`、`rust-firmware/src/storage.rs`：服务端 Token 在状态机入口和持久化层均限制为 255 字节，与 NVS 读缓冲一致。
- `rust-firmware/src/storage.rs`：Wi-Fi 凭据和服务端配置分别保存为单个带版本的 NVS blob，写入不会再产生 SSID/密码或 URL/Token 新旧混合；读取端保留旧键兼容迁移，并对持久化内容重新校验。
- `rust-firmware/src/inbox.rs`：Inbox 条目与待确认已读 ID 合并为单个版本化 NVS blob；pending 集合只保留当前最多 32 个条目中的 ID，条目截断和序列化容量使用同一预算，已读标记与待上传确认原子落盘。
- `logic/src/app.rs`：命令确认只按唯一 operation ID 关联，不再被并发同步的 metadata/RTC 收尾全局屏蔽；实机复现的永久 `busy` 已由回归测试覆盖，修复后 10/10 压力写入和 11/11 smoke 通过。
- `rust-firmware/src/power.rs`：DFS 运行频率范围为 40–160 MHz；Wi-Fi、SPI 和 I2S 驱动仍由 ESP-IDF 的 PM lock 保证外设活跃期时钟，授权真机 smoke 未出现时序、WDT 或复位异常。
- `rust-firmware/src/epd_task.rs`：EPD worker 订阅 Task Watchdog，空闲等待每秒喂狗，刷新前后也喂狗；底层 BUSY 超时仍为 2 秒，非预期的 SPI/锁永久阻塞可由 10 秒 WDT 捕获。
- `rust-firmware/src/tasks.rs`：pthread 默认配置的读取、修改、线程创建与恢复由进程内全局互斥串行化，避免并发创建 worker 时互相覆盖栈能力策略。
- `rust-firmware/src/wake.rs`：GPIO ISR 在通知唤醒更高优先级任务后调用 Xtensa `_frxt_setup_switch()` 请求立即调度，保持 ISR 路径位于 IRAM。
- `.github/workflows/ci.yml`：CI 安装固定 ESP-IDF 5.5.5 和 ESP32-S3 Xtensa Rust 工具链，执行 `--release --locked` 完整交叉构建并上传 ELF、bootloader、分区表和 linker map。
- `scripts/release.sh`：发布前要求干净工作区，校验 GitHub 登录、仓库、标签和 Release 冲突；构建验证完成后先创建带完整附件的 draft Release，最后发布并同步次要远端标签，中途失败不会产生公开但附件不完整的 Release。
- `rust-firmware/src/board.rs`：GPIO4 与 ADC1 channel 3 在 `Note4Board::take()` 中一次取得，channel 连同 ADC driver 由 Board 全生命周期持有；周期采样不再通过 `Peripherals::steal()` 绕过 HAL 所有权。
- `rust-firmware/src/main.rs`、`heap_probe.rs`：每秒电源采样和正常栈水位降为 DEBUG；任一任务剩余栈低于 2 KiB 时仍以 WARN 输出，默认 INFO 量产运行不再持续打印正常周期状态。
- `rust-firmware/src/ble_control.rs`：notify 完成关联继续使用 session、generation、conn_handle 与 attempt_id 四元组；断连 handle 改为容量 4、3 秒安全窗口的退休队列，迟到回调可主动排空退休项，窗口到期后允许 NimBLE 复用 handle，不再永久拒绝新连接回复，同时省去原 8 KiB 位图。
- `rust-firmware/src/tasks.rs`：pthread 默认配置在修改前保存，创建 internal-stack worker 后恢复，避免全局线程栈策略泄漏到后续线程。
- `rust-firmware/sdkconfig.defaults`：15,000 字节显示帧优先进入 PSRAM；Wi-Fi RX/TX 缓冲数量按本设备短连接负载下调，减轻 DMA/internal heap 压力。
- `README.md`、`scripts/release.sh`、`docs/verification.md`：刷写流程显式指定同次构建的 bootloader；冷启动已确认 bootloader 和应用均为 ESP-IDF v5.5.5。发布附件包含 ELF、bootloader 和分区表。
- `rust-firmware/build.rs`：支持 `SOURCE_DATE_EPOCH`，相同源码和指定时间戳可生成稳定的构建时间元数据；非法值会直接终止构建。
- `rust-firmware/build.rs`：递归声明两个本地 ESP-IDF 组件的源码、头文件和 CMake 文件为构建依赖，驱动修改会触发 Cargo/embuild 重新配置。
- `rust-firmware/build.rs`：构建期校验 CJK 索引长度、严格排序、cell 唯一性与范围，并确认 12px/16px 字模具有完整的 94×94 网格长度。
- `scripts/release.sh`：发布前核验最终生成的 ESP32-S3、16 MB、DIO、80 MHz 配置，重新编译并逐字节比对分区表，同时用 `ota_0` 生成应用镜像以执行 4 MiB 容量门禁。
- `rust-firmware/partitions.csv`、`rust-firmware/src/main.rs:1099`：双槽、回滚 metadata 和 512 KiB core dump 分区已经落地；待验证镜像只有在 RTC、NVS、主要工作任务和状态机启动成功后才确认有效。
- `rust-firmware/src/sync_apply.rs`、`storage.rs`：同步快照采用持久化 redo journal；启动先重放再构造 `BootSnapshot`，写入失败则进入安全模式，不会把部分提交的数据交给业务状态机。NVS 扩至 64 KiB，为最大 12 KiB journal 和现有对象保留整理空间。
- `rust-firmware/src/tasks.rs`：所有应用 pthread 统一使用 internal RAM 栈、显式优先级和 `CONFIG_FREERTOS_NO_AFFINITY`；RTC 8、音频 7、USB 6、Effect 5、EPD/BLE 4、同步 3，实时告警路径明确高于后台 TLS，同一全局配置锁防止创建线程时策略串扰。
- `scripts/backup-flash.ps1`：读取 Flash 前强制核验 ESP32-S3、授权 MAC `20:6E:F1:B4:7D:E4` 和 16 MB 容量；备份后校验长度并生成包含设备身份和 SHA-256 的 JSON 清单。
- 以上状态通过 fmt、clippy、420 项单元测试、release 交叉构建和授权真机启动日志验证；RTC/NVS 路径另通过 1,000 次连续提交与 60 秒 soak。

## 3. 改进建议

### P0 必须修复

#### P0-1 敏感凭据存入未加密 NVS

- 位置：`rust-firmware/src/storage.rs:55`、`:70`；`rust-firmware/sdkconfig.defaults`
- 证据：Wi-Fi 密码和 Bearer Token 分别写入 `wifi_pass`、`auth_token`；实际生成配置未启用 Secure Boot、Flash Encryption 或 NVS Encryption。
- 影响：通过物理读取 Flash、恶意固件或未限制的调试接口可恢复网络凭据和服务端 Token，固件也没有可信启动链。
- 建议：生产配置启用 Secure Boot V2、Flash Encryption 和加密 NVS；开发与生产配置分离，量产流程单独管理 eFuse。
- 验证：离线读取 Flash 不应出现密码或 Token 明文；未签名固件应无法启动；验证量产、升级和恢复流程。

### P2 优化项

#### P2-6 7.375 MiB storage 分区当前未被使用

- 位置：`rust-firmware/partitions.csv:5`；全仓库文件系统挂载路径
- 证据：`storage` 数据分区占 `0x760000`，但固件没有 LittleFS/SPIFFS/FAT 挂载或读写代码；持久化全部位于 64 KiB NVS。
- 影响：约 46% Flash 空间预留但尚未承担运行功能；这不影响正确性，但分区用途尚未形成产品约定。
- 建议：确认未来是否存放用户文档或离线资源；不要仅为“利用空间”引入文件系统。
- 验证：用生成的 partition table 二进制核对偏移、大小和无重叠，并对刷写、升级和数据保留策略做回归。

#### P2-8 量产配置与诊断配置尚未分离

- 位置：`rust-firmware/sdkconfig.defaults:110-116`
- 证据：默认配置同时启用综合堆毒化、INFO 日志和 Flash core dump。这些设置适合当前 P0 定位，但会增加运行开销和 panic 后恢复时间。
- 影响：单一 profile 难以同时满足故障定位和量产的性能、安全、恢复时延要求。
- 建议：保留 diagnostic profile；另建 production defaults，明确日志级别、core dump 去向、堆检查级别和安全启动参数。两种 profile 都应纳入 CI 构建。
- 验证：比较两种镜像的体积、内部堆、性能和故障输出，并确保生产 profile 不泄露敏感内存。

## 4. 快速收益清单

以下改动通常可在 1–2 小时内完成，且风险较低：

1. 为队列 Full、BLE reply retry 和 EPD fallback 增加累计计数。

## 5. 中期与长期建议

### 中期

- 建立 RTC I2C、EPD BUSY、BLE 未授权访问和 Wi-Fi 失败清理的组件测试。
- 对已明确的任务优先级与不绑核策略做 Wi-Fi TLS、EPD 全刷和音频并发延迟测量。
- 复用现有 smoke 和串口采集脚本建立硬件在环测试。
- 对 NVS 提交执行掉电注入测试。
- 对同步 redo journal 执行逐写入点掉电注入，验证每次重启都能完成一致重放。
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
