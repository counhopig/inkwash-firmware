# 嵌入式工程师视角的总评

> 这是一份**主观工程评价**，不是缺陷清单（缺陷见 `review-findings.md`）。
> 结论基于两轮源码通读（第二轮为逐行重读全部 66 个模块 + 可执行验证），
> **未经真机功耗/时序测量**。
>
> 评价范围：`inkwash-firmware` 的两个 crate 为逐行阅读；
> §十 对四仓库结构的判断只基于 README / 架构图，**未读 server / desktop / mcp 代码**，
> 置信度低于其余章节。

## 总评

| 维度 | 评分 | 依据 |
|---|---|---|
| 分层与解耦 | **A** | `logic` / `rust-firmware` 边界是全项目最有价值的资产（§二、§七） |
| 并发权责 | **A-** | 单所有者模型干净；扣分在线程数本身没被当成需要优化的量（§一、§二） |
| 实时性 | **B** | 交互够用，网络操作差，但对"离线优先 + RTC 硬件兜底"的产品可接受（§三） |
| 资源经济性 | **C+** | 内 RAM 是第一约束，却有 ~24 KiB 自造浪费（§一） |
| 输入健壮性 | **D** | 六条同源缺陷；校验按"数据来源"分两套标准（§五） |
| 失效可见性 | **D** | 最需要看门狗的线程没订阅；阻断页无兜底出路（§四、§五） |
| 验证基建 | **D-** | 固件零编译门禁 + 补偿机制已坏 + 不可回滚的刷写风险（§九） |

**一句话：这是用 PC 软件工程方法做固件——好处和代价都很典型。
它把预算几乎全投在"证明策略正确"上，回报真实可见；
但嵌入式的生死线在"意外发生时能不能活下来、活不下来时能不能看见"，
而这两项恰好是投入最少的。**

---

## 一、先算硬件账

固件第一件事我算 RAM。9 个线程栈合计**恰好 128 KiB**：

```
main 32K + sync 16K + ble 16K + effect 16K + epd 12K
     + usb-rx 12K + rtc 8K + audio 8K + usb-writer 8K  = 131,072 B
```

其中 `ble` 与 `effect` 经 `tasks.rs:9-10` 钉在**内 RAM**。再加 IDF 自身的
NimBLE host(5K)、WiFi、lwIP、esp_timer、双 IDLE(4K each)，
**内 RAM 栈占用 > 160 KiB**。

**这块板子的绑定约束不是 flash，不是算力，是内 RAM。**

由此得出本次评审最重要的一条因果判断——**架构的线程数是它最复杂特性的上游原因**：

```
128 KiB 栈 → 内 RAM 吃紧 → BLE init 前必须腾地方
   → ble_memory.rs 的 60K/24K 堆门
   → SuspendForBle / ResumeAfterBle 整条交棒链
   → ctx.rs 里 6 个 ble_* 字段 + main.rs:498-809 约 300 行会话管理
   → 3 个靠源码文本断言守护的契约测试
```

而更讽刺的是：`sync_task`(16K) 与 `ble_control`(16K) **永远不会同时工作**——
射频仲裁的存在本身就证明了这一点。合成一个 radio 线程直接省 16 KiB；
`usb-rx` + `usb-writer` 合成一个再省 8 KiB。

**24 KiB 回到内 RAM ≈ 给 NimBLE 的余量翻倍**，交棒逻辑可能根本不需要存在。
项目甚至写过那个状态机（`logic/src/ble_radio.rs` 的 `BleRadioCoordinator`），
然后弃用了它，改用跨线程消息——**弃用的那个设计恰好是省 RAM 的那个。**

### 第二轮补充：还有 8 KiB 花在了一个用不上的地方

`ble_control.rs:107` 为 `NotifyAttemptMailbox` 维护了 `retired_handles: [u64; 1024]`
—— **8,192 字节**的位图，用来记录"已退休的 conn_handle"。

而 `sdkconfig.defaults` 里写着 `CONFIG_BT_NIMBLE_MAX_CONNECTIONS=1`，
`ble_control.rs:888-891` 还会主动断开第二个连接。**一个单连接 GATT peripheral
需要追踪的 conn_handle 数量是个位数**，却按 u16 全值域（65536 个）开了位图。

这 8 KiB 同时也是 P0-4（断线重连后应答通道永久失效）的所在地——它只置位、从不清位。

> **但要区分两件事**（第三轮更正）：位图的**定量**不合理，它的**机制**却是必要的。
> NimBLE 的 `on_notify_tx` 回调只携带 `conn_handle`，不带 generation 或 attempt_id
> （`ble_control.rs:972-976`），请求身份完全由邮箱的 `armed` 槽位"猜"出来。
> 位图是"这个 handle 上还可能有迟到回调在路上"的唯一记账，**不是冗余的**。
> 初版曾建议"换成 `attempt_id` 集合"，那是错的——回调里没有 `attempt_id` 可比对。
> 详见 `review-findings.md` P0-4 的修法讨论。

**修正后的账**：

| 可回收项 | 内 RAM | 前置条件 |
|---|---|---|
| `sync` + `ble` 合成一条 radio 线程（硬件互斥，永不同时工作） | 16 KiB | 无，纯重构 |
| `usb-rx` + `usb-writer` 合成一个 | 8 KiB | 无，纯重构 |
| `retired_handles` 按 `u16` 全值域开位图 → 小型有序集合 | ~8 KiB | **需先解决回调归属**（见 P0-4），否则只能改定量不能改机制 |
| **无条件可回收** | **24 KiB** | |
| **解决归属后可回收** | **~32 KiB** | |

对一块内 RAM 栈占用刚过 160 KiB 的板子，**24 KiB 已是 15%，32 KiB 是 20%**。
这不是微优化，这是"要不要存在整条交棒子系统"级别的结构决策。

---

## 二、并发模型：全项目最好的部分

- **权责干净**：PCF8563 单所有者（`rtc_executor.rs:168` 日志自述）、
  EPD 单写者、Wi-Fi 驱动 `move` 进 sync 线程后主线程只能发命令。
- **用值传递换掉共享**：`display.rs:63` 把整帧 `to_vec()` 交给刷新线程，
  15 KB 拷贝换掉一个不可能的数据竞争。在 240 MHz 上这笔交易是对的。
- **BLE 防御到位**：`attempt_id` + `retired_handles` 位图过滤迟到的 notify 回调
  （`ble_control.rs:104-165`）——只有被 NimBLE 回调时序真坑过的人才会写这个。
  还有初始化前的**实测**堆门（`ble_memory.rs`，测试里写死 64831/31744 这种真实数字，
  不是拍的）、按需创建线程（`ble_control.rs:344-357`）、
  `stop advertising` 先于 `deinit_full`（`ble_control.rs:838-848`）。
- **全程有界 + 退还生产者**（`Err(DispatchSaturated{event})`），事件永不静默丢失。
  这在固件里很少见。

一个隐患：`tasks.rs:7-25` 改的是 **IDF 进程级 pthread 默认配置**，无锁。
当前只有主线程调用所以安全，但这是**隐式单线程契约且无注释**，
从别的线程加一个 spawn 点就是竞态。

---

## 三、实时性：诚实评估

主循环 20 ms、按键 4 次去抖（80 ms）看着慢，但浅睡时 ISR 用
`xTaskGenericNotifyFromISR`（`wake.rs:21-28`）立即唤醒，按键延迟是亚毫秒级。
**这是对的。**

但 `WifiManager::connect` 是**同步阻塞轮询**：500 ms sleep × 20 s 超时 + 10 s DHCP
（`wifi.rs:117-144`）。这不是设计选择，是被 `esp_wifi_connect` 无回调逼的。
后果是一次 sync 最坏占住射频 30 s，期间 `SyncNow` 返 Busy、BLE 配对被拒
（`ctx.rs:729-742`）。

EPD 阻塞刷新数秒，但被 supersede + 矩形并集（`epd_task.rs:109-149`）化解，
失败降级整屏。**这是 e-paper 的正确做法。**

**判断**：对用户交互够用，对网络操作差。但对这个"离线优先、闹钟由 RTC 硬件保证"
的产品，可以接受。

---

## 四、失效模式：最该打回的地方

**9 个线程里 5 个没订阅看门狗**，最严重的是 `effect-task`——所有 NVS 写入的
唯一执行者。它若挂起，整条持久化链静默停摆，而设备**看起来完全正常**
（时钟走、闹钟响、界面刷新，只是改动不落盘）。详见 `review-findings.md` P0-1。

作者知道该订阅（另外 4 个都订了），只是漏了。**这是最想让他补的一行。**

---

## 五、边缘校验不统一——真会杀死设备

规律很清楚：

- **信任"从网线来"的数据的地方都有防线**：`sync_validate.rs` 的去重/范围/repeat 校验很严。
- **信任"从 NVS 读出来"的数据的地方几乎都没有**：`alarms.rs:29`、`todos.rs:26`
  直接 `unwrap_or_default()`。

于是渲染层出现裸索引 `WEEKDAY_SHORT[*d as usize]`（`screens.rs:427/602`）、
`DAYS[(month-1)]`（`screens.rs:409`），加上 `screens.rs:381` 按字节递减的 `&line[..end]`
——含 2 个汉字的折行末行必 panic，而输入正是服务端同步来的待办文本。

**这两类数据的可信度其实一样。** 校验应收口到"数据进入设备"的那一个点，
而不是按来源分两套标准。

### 第二轮补充：这不是六个疏忽，是一个结构信号

逐行重读后，同源缺陷凑齐了六条。把它们按"被违反的隐含假设"排开：

| 隐含假设 | 违反后果 | 编号 |
|---|---|---|
| 服务端待办文本不会含 CJK 折行 | panic → 重启 | P0-2 |
| 上位机不会在响铃时清闹钟 | **永久卡死响铃、铃声不停，只能断电** | P0-3 |
| NimBLE 不会复用 conn_handle | 断线重连后命令通道永久失效 | P0-4 |
| NVS 里不会有越界 weekday | panic | P1-1 |
| RTC 不会返回 `month = 0`（或 >12） | panic | P1-2 |
| 错误串不会含多字节字符 | 在**安全模式**里 panic | P1-13 |

单独看每条都像小疏忽；**六条放一起就是一个结构信号：这个系统假设自己的输入是良性的。**
而嵌入式的输入从来不良性——RTC 电池会没电、NVS 会留着旧版本固件写的垃圾、
上位机会在最坏的时刻发命令、射频栈会复用句柄。

最能说明问题的是 P0-3。`transition_button`（`app.rs:1877-1891`）的兜底分支列了
12 个屏幕，**唯独漏了 `AlarmRinging`**。作者显然认为"响铃屏的退出由上面那个
`Firing` 分支负责，不需要兜底"——这个推理在状态机内部完全自洽，
它只默认了一件事：**没有任何外部事件能把 `alarm_runtime` 从 `Firing` 推走**。
而 `ClearAlarms`（`app.rs:3412`）恰好能。

**结构上缺的不是那一行代码，是"阻断页必须有无条件出路"这条不变式。**

同理，`home.rs:159-161` 对 RTC 月份/星期做了钳制、`screens.rs:409/413` 却是裸索引
——**同一个仓库、同一类输入、两套标准**。这说明防御不是从一条原则推导出来的，
而是写到哪里想起来了就加一下。

两条收口建议：

1. **校验收到"数据进入设备"的唯一入口** —— `AlarmStore::load()` /
   `TodoStore::load()` / `Pcf8563::read_time()` 走与 `sync_validate` 同一套校验。
   收口之后渲染层才**有资格**用裸索引。
2. **阻断页给无条件出路** —— `AlarmRinging` / `Reminder` / `BlePairing`
   任意按键都应能离开，而不是依赖某个 runtime 子状态恰好匹配。

---

## 六、我不同意的三个决定

| | 决定 | 后果 |
|---|---|---|
| a | **删除全部 Rust 源码注释**（`ba5cd3d`） | 见下方修正说明 |
| b | **测试预算倒挂** | `logic` 11k 行测试 vs 固件**实际 2 个模块**（`effect_task.rs` 的 4 个编译不过），而固件才是碰硬件的那一半。386 个测试证明了 reducer 的价值，也说明测试投在了风险最低的地方 |
| c | **CI 不编译固件** | 项目最大的单点风险是"刷错变砖"，却零编译门禁。哪怕只加 `cargo check --target xtensa` + app size 检查。详见 §九 |

### 修正：关于注释删除的准确判断

我在初评时说过"物理约束只存在于 log 字符串和 `.esp32-review.yml`"——**这个说法过强了**。
复核后：`sdkconfig.defaults` 有 36 行注释，`.esp32-review.yml` 25 行，
`.cargo/config.toml` 4 行，`scripts/*` 也有注释。而且 `sdkconfig.defaults` 的注释质量相当高：

- `:52-58` 解释了 TWDT 为何是 10 s（记录了"two live TWDT aborts died in that window"的
  真实故障史，以及 P2/P3 阶段把 sync 和 EPD 移出主线程后重新收紧到 10 s 的推理）；
- `:24-32` 解释了 USJ 在自动浅睡下不响应、可能无法重新枚举；
- `:39-43` 解释了 `IDLE_TIME_BEFORE_SLEEP` 在 IDF 5.5 的改名。

**所以准确的说法是**：*机器可执行的配置里保留了物理约束的推理*，
而 *Rust 源码里丢失的是"为什么是这个状态机顺序、为什么这个标志要这样仲裁"这类
设计意图*。后者恰恰是最难从代码反推、也最容易在重构中被改坏的部分——
三个死字段（`pending_ble_pairing_success`、`pending_render_completion`、
`pending_residue_time`）正是"没有注释说明意图，于是生产者被删掉而消费者留下来"的产物。

这是一个**比"完全没文档"轻、但比"注释无用"重**的问题：约束还在，
但只在"硬件配置"层面，不在"行为契约"层面。

---

## 七、我真心认可的部分

- **离线闹钟的两条独立提交轨**（ACK 寄存器 / 持久化 NVS）+ `WaitingForRearm`
  要求 `minute_advanced`（`app.rs:2714-2737`）——这是被"闹钟清了但寄存器没清、
  于是每分钟重复响"坑过之后才会写的状态机。
  **全项目工程质量最高的地方。**
- **RTC 快照锁存**（`rtc_latch.rs`）：AF 置位期间复用缓存快照，省掉重复 I2C 读。
- **充电状态去抖 + fault / no_battery 识别**（`board.rs:99-149`），
  50 行纯经验值，只有坐在台面上拿万用表量过才对。
- **PCF8563 VL 检测 → 用 build epoch 重新播种 + 强制一次 NTP**（`main.rs:208-223`）。
- **BLE 初始化前的实测堆门**（`ble_memory.rs` 60K/24K）。
- **契约测试**（`logic/src/lib.rs:30-289`）用 `include_str!` 对固件源码做断言。
  它很脆（改变量名即失效），但它把"栈必须内 RAM""advertising 先于 deinit"
  "BT 角色关闭"这类**无法在主机运行验证**的约束变成了 CI 红灯。
  这是对"CI 无法编译固件"的**思路正确**的补偿——可惜实现上已经坏了两处，见 §九。

---

## 八、需要上真机确认的疑点

**`wifi.rs:166` 的日志声称 "internal heap released"，但 `EspWifi::drop`
（`esp-idf-svc-0.52.1/src/wifi.rs:1853-1859`）只调 `detach_netif()`，
不调 `esp_wifi_stop`/`esp_wifi_deinit`。**

释放的是 netif 缓冲，Wi-Fi 驱动本体和它 30–40 KB 收发缓冲仍在。

- 推论一：交棒释放的内存比代码认为的少，"drop Wi-Fi 给 BLE 腾地方"的理由比
  `sdkconfig.defaults:74` 的注释更弱。
- 推论二：`resume_after_ble` 用 `EspWifi::new` 重建（`wifi.rs:178`），
  同进程内二次 init——而契约测试只断言了**代码形状**（`lib.rs:265-269`），
  没断言**行为**。

**这条必须抓真机串口日志对比交棒前后的
`heap_caps_get_free_size(MALLOC_CAP_INTERNAL)`，不能靠读源码下结论。**

---

## 九、验证基建：整个项目最薄的一环

这一层问题最严重，**因为它让上面所有问题都无法被发现**。

### a. CI 不编译固件 → 固件半边完全没有门禁

理由成立（`scripts/release.sh:2-3` 说得对：ESP-IDF 在 CI 上不现实），
但结果是 format / lint / 体积 / **甚至语法**回归都会静默落地。

第二轮拿到了实证：`rust-firmware/src/effect_task.rs:350-483` 的 4 个测试
自 `EffectBatch` 引入 newtype 之后就**编译不过**了——把这 4 个字面量提取到主机侧编译，
得到 **12 个类型错误（每个测试 3 个）**，它们从未运行过一次。
首版文档还把它们列为"覆盖了 `execute_batch` 的顺序 / Continue / AbortBatch"。
**没人知道，因为没人编译过。**

**但要挡住这一类回归，门禁得选对**（初版此处写得不准确，已更正）：

| 命令 | 能挡住什么 | 挡不住什么 |
|---|---|---|
| `cargo check --target xtensa-esp32s3-espidf` | 生产代码的类型/语法回归 | ❌ **默认不检查 `#[cfg(test)]`**，那 4 个测试照样漏过 |
| `cargo check --all-targets --target ...` | ✅ 含测试目标，能抓到这 4 个 | 不链接，抓不到体积 |
| `cargo build --release --target ...` + size 检查 | ✅ app 体积 | 需要完整 ESP-IDF 工具链，CI 成本最高 |

所以建议是分层的：

1. **最低成本、最高回报**：`cargo check --all-targets` —— 这一条就能抓到那 4 个测试，
   `espup` 在 CI 上装得动，不需要链接器。
2. **次优替代**：把这类纯逻辑测试（`execute_batch` 的顺序 / Continue / AbortBatch
   本来就不碰硬件）挪进**主机可编译的 harness**，跟着 `logic` 一起在 CI 跑。
   这比给固件加门禁更划算——它们本来就不该住在固件 crate 里。
3. **app 体积门禁必须实际构建**，`check` 拿不到产物。考虑到"刷错变砖不可回滚"，
   这一条值得单独付出工具链成本，哪怕只在 release tag 上跑。

**即便如此，加编译门禁仍是本次评审里杠杆最高的一个结构性修改**——
只是要写成 `--all-targets`，不是裸 `check`。

### b. 补偿机制本身已经坏了两处

契约测试的思路是对的，但实现有两个结构缺陷：

**① 它对行尾敏感。** `lib.rs:132-137` 的断言里嵌了字面 `\n` 和缩进。
仓库**没有 `.gitattributes`**，所以行尾取决于检出方式：在 **CRLF 检出**下
（Windows 上 `core.autocrlf=true` 是默认，本机实测 `ble_control.rs`
1118 个 CRLF / 0 个裸 LF），断言必然失配，**仓库自己的测试套件就是红的**
（385 passed / 1 failed），失败信息还指向一条与改动完全无关的 BLE 契约。

准确的说法是：**这取决于检出配置而非操作系统**——把 `core.autocrlf` 设为
`false`/`input` 的 Windows 开发者不受影响，用 CRLF 检出的 Linux/macOS 用户同样会中。
但因为 Windows 默认就是 CRLF，实际受影响的绝大多数是 Windows 开发者。
CI 只跑 `ubuntu-latest`（LF 检出），永远看不到这条。

**② 它分不清"约束"和"现状"。**

| 断言 | 钉住的东西 | 后果 |
|---|---|---|
| `lib.rs:175` | `pending_render_completion.is_some()` | 钉住一个恒为 `None` 的**死字段**，删了 CI 就红 |
| `lib.rs:148` | `retired_handles: [u64; 1024]` | 钉住 **P0-4 的成因**，修 bug 必须同时改测试 |

文本断言无法表达"这个形状是必须的约束"和"这个形状只是当时恰好长这样"的区别。
于是它在守护真约束的同时，也把死代码和 bug 一起钉住了。

### c. 单 factory 分区 + 无 OTA + 无体积门禁

`rust-firmware/partitions.csv` 只有一个 4 MB `factory`，刷坏只能串口救。
"刷错变砖"是**不可回滚**风险——而恰恰是这条路径上零编译门禁、零体积门禁。
另外 4 MB 的 `storage` 分区已分配但**代码完全没用**（全部持久化走 NVS）。

**把 a / b / c 放在一起看：项目最大的单点风险，受到的自动化保护最少。**
这是我对这个项目最担心的一条，超过任何单个 P0。

---

## 十、四仓库结构（置信度较低：未读 server / desktop / mcp 代码）

### 切分点是对的

设备只消费不生产、server 持有真理源、desktop 只推配置、设备拉结构化 JSON 本地渲染
——这是 e-paper 设备的**正确**架构。对比"服务端渲染图片的瘦客户端"，
后者一断网就是块砖。闹钟由 PCF8563 硬件寄存器兜底、完全不依赖网络，
这个决策尤其正确，也是整个产品定义里最硬的那块骨头。

### 但四个独立仓库对这个规模是重税

- **协议每改一次是五步舞**：改 logic → 发 rev → server 改 `Cargo.toml` pin →
  跑 `check-logic-pin.sh` → 同步更新 `sync-api.md`。
  这是 monorepo 级别的仪式感，**却没有 monorepo 的工具**（无 workspace、无共享 lockfile）。
- **端到端测试没有自然归属，目前也确实不存在**：伞仓库只跟踪
  `README` / `LICENSE` / `AGENTS.md`，不跟踪任何代码，因此没有任何一个现成的位置
  能同时构建固件和服务端、跑一次真实的 `POST /api/sync`。

  需要说明的是这**不是结构上不可能**——多检出一个仓库的 CI job、git submodule、
  docker-compose 编排都能做到，只是都需要额外搭建，四仓库让这件事从"顺手"
  变成"要立项"。而 `/api/sync` 恰恰是最需要端到端验证的契约，
  目前它只靠"两侧各自的单元测试 + 一份手写的 `sync-api.md`"维系。
- **失联更难被发现**：`README.md:9,68,72,88,123` 指向的
  `docs/development-guide.md`、`docs/control-protocol.md`、`docs/screenshots/*`
  已被 `b30c3af` 删除，首页链接全 404。分散在四个仓库让这类断裂更晚暴露。

### 建议

如果是团队项目，四仓库有组织上的理由（各自的 issue / release 节奏）。
如果是个人项目，我会合并成两个：

| 仓库 | 内容 | 收益 |
|---|---|---|
| `inkwash-device` | `logic` + `rust-firmware` + 一个 mock server | `sync-api` 契约测试终于有地方放；协议改动一次提交完成 |
| `inkwash-cloud` | `server` + `desktop` + `mcp` | 共享 ts-rs 绑定，DTO 改动一次提交完成 |

---

## 十一、结论

这是一份**想清楚了的设计**，不是随手堆的固件。它把工程预算几乎全投在
"策略正确性"上，回报是逻辑几乎不出错；代价是"失效可见性"和"设计意图留存"两项偏低，
所以一旦出问题线索很少。

第二轮逐行复核没有推翻这个判断，但把它**加重**了：新找到的两个 P0
（响铃期死锁、BLE 重连失效）与首版的两个 P0 是**同一个病根**——
状态机对"不该发生的输入"没有出路；而验证基建的三处失灵
（固件零门禁、契约测试在 CRLF 下必红、4 个测试编译不过）
意味着这类问题**在真机上撞见之前不会被发现**。

**我会让它过设计评审，但在补上三件事之前不会批准量产：**

1. **加固件编译门禁**：`cargo check --all-targets --target xtensa-esp32s3-espidf`
   （`--all-targets` 是关键，裸 `check` 不看 `#[cfg(test)]`，抓不到那 4 个测试），
   体积门禁另需实际构建。杠杆最高——它会立刻暴露那 4 个编译不过的测试，
   以及未来所有同类回归。**更划算的替代**：把这几个纯逻辑测试挪进主机 harness。
2. **两条不变式收口**：校验收到"数据进入设备"的唯一入口；
   阻断页（`AlarmRinging` / `Reminder` / `BlePairing`）给无条件出路。
   这一条同时消掉 P0-2、P0-3、P1-1、P1-2、P1-13 五条缺陷。
3. **`effect-task` 订阅看门狗** + 一条 `worker_batch_in_flight` 超时恢复路径。
   一行订阅 + 十几行恢复，换掉"设备看起来完全正常但数据永远不落盘"这个最坏失效模式。

**第 4 件不紧急但收益最大**：把 `sync` 和 `ble` 合成一条 radio 线程、
`usb-rx` 与 `usb-writer` 合成一个。这两项是纯重构、无前置条件，
**24 KiB 内 RAM 回流（板上占用的 15%）**；再把 `retired_handles` 的定量收敛
还能多回收 ~8 KiB。这不是优化，是**删掉一整个子系统**——
交棒链存在的唯一理由就是"BLE init 前腾不出内 RAM"，余量翻倍后它很可能不必存在。

> **P0-4 不在这四件里，因为它不是一次性修复。** NimBLE 的 notify-tx 回调
> 不携带请求身份，位图是当前唯一的归属记账；根治需要先建立"旧回调排空边界"
> 或换掉 correlation 载体（见 `review-findings.md` P0-4）。
> 在那之前只能缓解（限制退休作用域、断开后延迟解除），不能删位图。
> **这条要单独立项，不要当成顺手能改的 bug。**

**另外两件几乎零成本、但当下就在伤人的**：

- `.gitattributes` 加 `*.rs text eol=lf`（一行，让测试套件在 Windows 上能跑）；
- 重录 `.esp32-review.yml`（豁免行号已全过期）并补齐 README 的文档链接。
