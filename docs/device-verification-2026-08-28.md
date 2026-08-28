# 真机验证记录 — 功耗与响应改造（2026-08-28）

**固件提交：** `8d2cec7`（`feat(power)` 系列三连的末次；烧录 payload 同 `0f8e568`）
**设备：** NOTE4 黑白屏版（MAC `20:6e:f1:b4:7d:e4`，esp32s3 rev v0.2）
**端口：** `/dev/cu.usbmodem1101`（USB-Serial-JTAG）

> 这是一份**真机验证的诚实记录**：明确区分「已在真机确认」和「代码路径
> 推演仍未在真机坐实」两类结论。它不是完成声明。

---

## 1. 已确认（真机证据）

### 1.1 烧录成功
- 首次 `espflash flash --before default-reset` **连不上 bootloader**。
- 根因：NOTE4 走 **USB-Serial-JTAG**，`default-reset` 的 DTR/RTS 复位时序
  不适用；改用 **`--before usb-reset`**（espflash 为 USB-JTAG-Serial 外设
  提供的专用复位时序）后立即连接并烧录成功。
- 结果日志：
  ```
  Chip type: esp32s3 (revision v0.2)
  Flash size: 16MB · mode DIO
  App/part: 2,334,496 / 4,194,304 bytes (55.66%)
  Flashing has completed!
  ```

### 1.2 启动完全正常（完整 boot 日志）
| 证据 | 日志 |
|------|------|
| 烧录版本正确 | `inkwash_note4: Inkwash NOTE4 Rust bring-up starting (git v0.5.0-4-g8d2cec7)` |
| DIO 模式（红线 #2） | `mode:DIO` / `SPI Mode: DIO` |
| 唤醒原因 | `Wakeup cause raw = 0x0`（冷启动） |
| RTC 正常 | `PCF8563: 2026-08-28 18:45:47 vl=false` |
| 闹钟路径 | `No enabled alarms; hardware alarm cleared` |
| EPD 任务 | `Initial display refresh queued to EPD task` |
| 电池 | `Power state: ... vbat_mV=4170 (100%)` |

### 1.3 协议栈活、主循环活（运行中）
- `>>IW {"cmd":"get_status"}` 正确回 `<<IW {status: status, ...}` 完整帧。
- `Power state` 心跳按约 1 s 周期输出（idle 态 `IDLE_POLL_INTERVAL_MS=1000`
  的表现，非 20 ms 活跃态）。

### 1.4 深睡分级在生效
- 烧录后观察到 `/dev/cu.usbmodem1101` **反复消失又重现**（USB 枚举断开/恢复），
  与「深睡中 USB-Serial-JTAG 挂起、维护唤醒时重新枚举」一致。
- 长时间静默（90 s 无任何日志）期间稳定无输出，符合深睡。

### 1.5 串口操作会复位芯片（复现了文档 §13 的既有结论）
- 抓到的复位日志含 `rst:0x15 (USB_UART_CHIP_RESET)` —— 打开/关闭 USB
  Serial-JTAG 端口确会触发芯片复位。这是**反复"检查"设备导致其感觉
  "卡住"**的直接原因之一：每次开串口都把它复位进 sleep 流程。

---

## 2. 发现的问题（需修）

### 问题 A — 屏幕时间冻结不刷新（**核心缺陷**）

**现象（真机已确认）：**
- 屏幕时间长期停在 **18:44**，而 RTC 实际已走到 18:49+。
- 期间设备 `get_status` 正常、主循环在跑、却不产生任何时钟刷新日志。

**影响面：** 用户看到的时钟陈旧，且不会自行恢复——只有当设备被重新
复位/重新插 USB（重新 boot）后才回到正常。

**候选根因（代码路径推演，尚未最终坐实，需加日志复现）：**

1. **深睡唤醒后的时钟区刷新可能未真正重绘**（`main.rs:244-254`）：
   - `woke_from_deep_sleep && !alarm_fired_at_boot` 分支只调用
     `refresh_partial_best_effort(CLOCK_RECT)`，但 EPD 局部刷新的执行在
     `epd_task` 异步进行，且依赖 `render_home_now` 在此前已把**新时间**
     画进 canvas。二者顺序与 `clock` 变量在深睡唤醒 boot 时的赋值时机
     需要逐帧核对。
2. **idle 态 RTC 读取门控可能让分钟变化漏检**（`main.rs:418-456`）：
   - idle 态 `clock_interval = IDLE_CLOCK_POLL_INTERVAL (10s)`，用
     `now.duration_since(clock_last)` 判断；`now` 是 `Instant::now()`
     在每次循环顶部采的，而 idle 态循环本体被 `wake.wait(1000ms)` 阻塞
     —— `clock_last` 的推进与 10 s 门控之间的边界需要核对是否在某条
     路径上永久失效。
3. **维护定时唤醒间隔**（`main.rs:660-669`）：`min(月界闹钟维护, 10 min
   fallback)` 的计算若在「无闹钟」场景下未正确落到 10 min fallback，
   会导致设备睡得过久、时钟长时间不更新。

**定位手段（下一步）：**
- 在 `render_home_now` / `refresh_partial(CLOCK_RECT)` / `woke_from_deep_sleep`
  三处各加一条 `log::info!`，烧录后捕获一次「深睡 → 维护唤醒」完整周期，
  confirm 到底哪一环没走。**不留日志直接改是猜测，不可取。**

### 问题 B — 交互体验：串口探测会"卡住"设备

**现象：** 反复 `espflash monitor` / `board-info` / python 开串口，会持续把
设备复位；配合深睡，表现为「屏幕冻结、按键不响应、要重插 USB 才恢复」。

**定性：** 这是**工具链行为 + 深睡机制叠加**，不是固件崩溃。但暴露了两个
值得改进的点：
1. 深睡唤醒后，用户**只有 ENTER / DOWN（GPIO18）能唤醒**（UP=GPIO39 不行，
   RTC alarm GPIO5 只在闹钟时有效）。若用户习惯按 UP，会误判"死机"。
2. 固件没有「复位后保留上次显示时间并在下一次交互/维护唤醒立即刷新」
   的兜底——深睡窗口内屏幕时间按设计就是陈旧的（这是 e-paper + 深睡的
   固有代价），但**陈旧时长**本该被封顶在 10 min fallback 内；当前观察
   到超过了该上界，属 bug（即问题 A）。

---

## 3. 与「未验证」清单的对应

本次真机验证把几项从「代码事实」提到了「真机证据」，也把一项坐实为缺陷：

| 之前状态 | 项 | 真机结论 |
|----------|----|---------|
| 已实现（待真机） | 轻睡眠/深睡分级生效 | ✅ 已确认（USB 周期性断开/静默） |
| 已实现（待真机） | EPD 刷新任务 | ✅ 已确认（boot 日志 `queued to EPD task`） |
| 待实施（真机） | §13 冒烟清单全项 | ⚠️ 部分（boot/协议/电源通过；按键/BLE/响铃未走） |
| 已实现（我此前重写时判"已完成"） | P4-10「屏幕时间最多滞后 10 min」 | ❌ **被真机否定**，见问题 A |

---

## 4. 复现需要的环境备注

- 烧录必须 `--before usb-reset`；`--before default-reset` 连不上。
- 观察运行日志用**单次长会话**（开一次串口、设备睡掉枚举消失后自动重连），
  **切勿反复开关端口**——每次开关都会复位芯片，污染观察并造成"卡住"错觉。
- `espflash monitor` 需要真实 TTY（后台沙箱 stdin 不可用，报
  `Failed to initialize input reader`）；无头捕获用 python `pyserial` 更稳。
