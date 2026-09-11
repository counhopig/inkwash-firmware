# 嵌入式工程师视角的总评

> 这是一份**主观工程评价**，不是缺陷清单（缺陷见 `review-findings.md`）。
> 结论基于源码阅读，未经真机功耗/时序测量。

## 总评

**设计纪律 A，资源经济性 B，失效可见性与运维 C。**

这是用 PC 软件工程方法做固件——好处和代价都很典型。

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

---

## 六、我不同意的三个决定

| | 决定 | 后果 |
|---|---|---|
| a | **删除全部 Rust 源码注释**（`ba5cd3d`） | 见下方修正说明 |
| b | **测试预算倒挂** | `logic` 11k 行测试 vs 固件 0 行，而固件才是碰硬件的那一半。386 个测试证明了 reducer 的价值，也说明测试投在了风险最低的地方 |
| c | **CI 不编译固件** | 项目最大的单点风险是"刷错变砖"，却零编译门禁。哪怕只加 `cargo check --target xtensa` + app size 检查 |

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
  这是对"CI 无法编译固件"的合理补偿。

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

## 九、结论

这是一份**想清楚了的设计**，不是随手堆的固件。它把工程预算几乎全投在
"策略正确性"上，回报是逻辑几乎不出错；代价是"失效可见性"和"设计意图留存"两项偏低，
所以一旦出问题线索很少。

**我会让它过设计评审，但在补上三件事之前不会批准量产：**

1. `effect-task` 订阅看门狗（+ 一条 `worker_batch_in_flight` 超时恢复路径）；
2. 边界校验收口——NVS 读出的数据与网络来的数据走同一套校验；
3. 重录 `.esp32-review.yml`（豁免行号已全过期）并补齐 README 的文档链接。

**第 4 件不紧急但收益最大**：把 `sync` 和 `ble` 合成一条 radio 线程，
用那 16 KiB 换掉整条 BLE 交棒链。
