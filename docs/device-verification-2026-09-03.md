# 真机验证矩阵 — 迁移收尾架构（阶段 5/6/7，2026-09-03）

**固件 HEAD：** `d8acc74`（架构迁移：SM 屏为唯一屏幕宿主、RenderPlan 驱动刷新、最小安全模式、legacy blocking-page 链已删除）
**宿主测试：** `logic` 252 通过
**设备：** NOTE4 黑白版（MAC `20:6e:f1:b4:7d:e4`）
**端口：** `/dev/tty.usbmodem1101`（USB-Serial-JTAG；开/关端口会复位芯片，检查间隔请留足静默）

> 一份**真机验收矩阵**：每项给「刷写/串口命令 → 预期日志/观察 → 判定」。
> 已在本会话真机确认的项标注 ✅；标注「待真机」的项是代码已就绪、但需要
> 人工在设备上按本清单完成的验收步骤。诚实记录，不把推演当验证。

---

## 0. 刷写与基础冒烟

```bash
cd inkwash-firmware
./scripts/build-rust.sh --release
./scripts/check-git-rev.sh            # 期望: OK - ELF embeds current revision v0.5.0-127-gd8acc74
espflash flash --port /dev/tty.usbmodem1101 --chip esp32s3 --flash-size 16mb \
  --flash-mode dio --flash-freq 80mhz --partition-table rust-firmware/partitions.csv \
  rust-firmware/target/xtensa-esp32s3-espidf/release/inkwash-note4
python3 scripts/capture-serial.py --port /dev/tty.usbmodem1101 --duration 20 \
  --expect "bring-up starting" --expect "Initial display refresh queued" --output /tmp/boot.log
```

期望日志：`bring-up starting (git v0.5.0-127-gd8acc74)`、`Wakeup cause raw = 0x0`、
`PCF8563: ... vl=false`、`Initial display refresh queued to EPD task`、
`EPD refresh completed: Full`、约 1 s 周期 `Power state:` 心跳。✅（本会话已确认
d8acc74 冷启动：无 panic、无 safe-mode、两笔 Full EPD completion、15 s power loop。）

---

## 1. 渲染契约（ViewModel/RenderPlan）

**观察要点：分钟变化只刷 CLOCK_RECT 一个 Partial；无每分全刷。**

```bash
python3 scripts/capture-serial.py --port /dev/tty.usbmodem1101 --duration 75 --output /tmp/tick.log
grep "EPD refresh" /tmp/tick.log
```
- ✅ 分钟边界恰好一笔 `Partial(Rect { x: 16, y: 36, width: 368, height: 92 })`（clock rect）。
- ✅ 无其它全刷/双刷。
- 待真机：进入各页（见 §3）后停留跨分钟，确认整页只在该页数据变化时重绘。

---

## 2. 安全模式（注入钩子）

```bash
INKWASH_FORCE_SAFE_MODE=1 ./scripts/build-rust.sh --release   # 测试二进制
espflash flash ... <同 ELF 路径>
# 期望：boot 后 ~850ms 进入 safe mode（早于 Wi-Fi init / light-sleep arm）
```
- ✅（本会话已确认）：`Entering minimum safe mode (... forced ...)`；发
  `>>IW {"cmd":"get_status"}` 得到全 false/empty 的 `<<IW {...status...}` 回复；
  无 Wi-Fi 驱动日志、无 panic。
- 观察（待真机人工）：屏幕上固定 "SAFE MODE" 错误屏；按键不进入任何页面；
  断电/复位才退出。之后必须 `./scripts/build-rust.sh --release`（不带 env）重建
  正常二进制并重刷。

---

## 3. 页面矩阵（SM 屏为唯一宿主；待真机人工逐项）

导航：长按 UP/DOWN 开 drawer → 移动 → ENTER 进入各页；任意页长按 ENTER 回退。
| 页面 | 进入 | 移动/编辑 | 返回 |
|------|------|-----------|------|
| Home | 时钟/卡片渲染 | 分钟 tick 只刷 clock | — |
| Calendar | 本月网格 | UP/DOWN 移日 → ENTER 开周 | 任意键关 |
| Inbox | 未读 ○/已读 • | ENTER 开详情（自动标读） | 任意键关 |
| Alarms | 列表 + 开关行 | ENTER 翻转 enabled（持久化）；ADD 行 → 两段 picker | 长按 ENTER 回 |
| Todos | 列表 | ENTER 完成/长按 循环重要性（持久化） | 长按 ENTER 回 |
| Settings | SYNC NOW/INTERVAL/BLE/SLEEP | row1 → SM interval picker；row3 → 深睡 | 长按 ENTER 回 Home |
| BLE pairing | Settings row2 → 配对屏 |（见 §4）| 任意键退出 |

判定：每页渲染无旧帧残留、无部分刷新半帧；编辑后数据在重启后仍在（NVS）。

---

## 4. BLE pairing（待真机；SM 屏已接入，radio 接线在 executor）

- Settings → BLE PAIRING 行：期望进入配对屏 + advertising 启动
  （executor `StartBlePairing` 已同步完成，boot 不受影响 ✅）。
- 用 `inkwash-desktop` / 手机 nRF Connect 连接：**完整 connect → 发命令 → 回复 →
  disconnect → 重连** 生命周期为待真机项（NimBLE 事件闭环回调投递尚未在真机坐实）。

---

## 5. USB/BLE 命令 + Busy 隔离（待真机人工）

```bash
printf '>>IW {"cmd":"get_status"}\n' >/dev/tty.usbmodem1101   # 或 desktop --status
# 期望完整 <<IW status 帧（状态改变命令现由 SM 发 render：ClearAlarms 清空+重绘等）
```
- ✅ GetStatus 回复路径在 safe-mode responder 坐实；正常固件的 USB CDC host→device
  写在本会话 pyserial 环境不可靠 —— 用 `inkwash-desktop --status` 验收。
- 待真机：SyncNow/SetWifi/SetTimezone/ClearAlarms 各命令执行 + 状态改变后页面自动
  刷新（SM render）+ 双命令 Busy 隔离。

---

## 6. 电源：light/deep sleep 门控（待真机人工）

- 长静默（>IDLE_ENTER_AFTER）进入 1 s idle；>DEEP_SLEEP_AFTER 无 USB 时深睡
  （串口枚举消失/维护唤醒重现）。
- 深睡 wake 只刷 clock（`Deep-sleep wake: clock region refreshed`）。
- alarm 唤醒先响铃（SM Boot 路径）。
- 观察：无 watchdog reset、无卡死、唤醒后按键正常。

---

## 7. 判定清单汇总（最后逐项打勾）

- [ ] 冷启动/复位/深睡 wake/RTC alarm wake 各一次干净
- [ ] 空 alarm 离线运行（无响铃/无持续 RTC 中断）
- [ ] 各页进入/编辑/返回无旧帧、数据持久
- [ ] alarm dismiss 只一次 Full、无 CLOCK_RECT 尾随
- [ ] BLE 完整生命周期（待设备）
- [ ] USB/BLE 命令 + Busy + 网络成功/失败/超时/恢复
- [ ] light/deep sleep 门控 + 唤醒路径
- [ ] safe-mode 注入验证（§2）
- [ ] 无 panic/watchdog/卡死/漏键/refresh storm

完成以上并保存串口证据后才可声明 P1#6 通过。本文件不构成完成声明。
