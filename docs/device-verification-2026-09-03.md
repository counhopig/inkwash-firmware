# 真机验证矩阵 — 迁移收尾架构（阶段 5/6/7/最终复审，2026-09-04 更新）

**固件 HEAD：** `dd64f6b`（架构迁移：SM 屏为唯一屏幕宿主、RenderPlan 唯一刷新来源、
最小安全模式、alarm/reminder 均非阻塞——ring/reminder 为 SM overlay 经
Effect::Render→ViewModel→RenderPlan→EPD，音频经独立 audio task）
**宿主测试：** `logic` 253 通过
**基线历史：** 本文件早先记录对应旧基线 `d8acc74`（252 测试）；以下 ✅ 项若注明了
旧版本号则只对该旧基线成立。新最终 HEAD（dd64f6b）必须在重刷后才可把「最终版本通过」
结论继承到它——见 §0 重刷 + §7 逐项确认。
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
./scripts/check-git-rev.sh            # 期望: OK - ELF embeds current revision v0.5.0-142-gdd64f6b
espflash flash --port /dev/tty.usbmodem1101 --chip esp32s3 --flash-size 16mb \
  --flash-mode dio --flash-freq 80mhz --partition-table rust-firmware/partitions.csv \
  rust-firmware/target/xtensa-esp32s3-espidf/release/inkwash-note4
python3 scripts/capture-serial.py --port /dev/tty.usbmodem1101 --duration 20 \
  --expect "bring-up starting" --expect "Initial display refresh queued" --output /tmp/boot.log
```

期望日志：`bring-up starting (git v0.5.0-142-gdd64f6b)`、`Wakeup cause raw = 0x0`、
`PCF8563: ... vl=false`、`Initial display refresh queued to EPD task`、
`EPD refresh completed: Full`、`Audio task running` + `Audio task spawned`、
约 1 s 周期 `Power state:` 心跳。
- ✅ 本会话已确认 dd64f6b 冷启动（v0.5.0-142）：无 panic、无 safe-mode、audio task 启动、
  两笔 Full EPD completion、12 s power loop。
- ⚠️ d8acc74 及其后版本在更早会话确认的启动行为仅对相应旧基线成立；本 HEAD 已重刷确认，
  但 §3/§4/§5/§6 的交互项仍需在本 HEAD 上人工逐项复验。

---

## 1. 渲染契约（ViewModel/RenderPlan）

**观察要点：分钟变化只刷 CLOCK_RECT 一个 Partial；无每分全刷。**

```bash
python3 scripts/capture-serial.py --port /dev/tty.usbmodem1101 --duration 75 --output /tmp/tick.log
grep "EPD refresh" /tmp/tick.log
```
- ✅（早期基线已确认，机制未变）分钟边界经 SM transition_tick→render_batch→plan diff 产生
  一笔 clock Partial；无每分全刷。
- 待真机（本 HEAD）：进入各页（见 §3）后停留跨分钟，确认整页只在该页数据变化时重绘；
  记录 `grep "EPD refresh"` 证据。

### 1b. alarm/reminder 进出各一次 Full、无手工 partial/full 尾随（最终复审新增，待真机人工）

观察：alarm 或 reminder overlay 进入与退出各恰好一笔 Full；dismiss 后无 CLOCK_RECT/手工
partial/full 尾随（现 main 已无 redraw_requested/dismiss_full_refresh/FULL_SCREEN_RECT 路径，
刷新只来自 SM render——用串口 `EPD refresh completed:` 行数核对）。
- [ ] alarm 进入 → 1 笔 Full；ENTER dismiss → 恢复页 1 笔 Full；其后分钟变化只刷 clock partial
- [ ] reminder（urgent/todo）进入 → 1 笔 Full；任意键 dismiss → 恢复页 1 笔 Full
- [ ] alarm 抢占 reminder → alarm Full；alarm dismiss 后回 reminder 的底层页（不恢复 reminder）
- [ ] 连续两 alarm：各自 enter/dismiss 一次 Full，无 refresh storm

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
- 待真机（最终复审新增）：alarm 非阻塞期间与 reminder 非阻塞期间 USB GetStatus / BLE 命令 /
  Tick / sync completion 均可服务——命令回复到达且 overlay 不被扰动。

---

## 6. 电源：light/deep sleep 门控（待真机人工）

- 长静默（>IDLE_ENTER_AFTER）进入 1 s idle；>DEEP_SLEEP_AFTER 无 USB 时深睡
  （串口枚举消失/维护唤醒重现）。
- 深睡 wake 只刷 clock（`Deep-sleep wake: clock region refreshed`）。
- alarm 唤醒先响铃（SM Boot 路径）。
- 观察：无 watchdog reset、无卡死、唤醒后按键正常。

---

## 8. 音频 Start/Stop（最终复审新增，待真机人工）

alarm 响铃与 reminder 提示音现由独立 audio task 播放（main/executor 不阻塞音频）。
- [ ] alarm 进入 → 铃响（880Hz 反复 burst）；ENTER dismiss → 停响
- [ ] urgent reminder → siren（1397/1046Hz 交替）；dismiss → 停
- [ ] todo reminder → 3 声 beep；dismiss → 停（或自然结束）
- [ ] StartTone 失败（audio 不可用）不阻塞 dismiss：ring 保持可视、ENTER 仍退出
- [ ] 无 codec 的 boot：`ES8311 not available; tones disabled`，设备其余功能正常

---

## 7. 判定清单汇总（最后逐项打勾）

- [ ] 冷启动/复位/深睡 wake/RTC alarm wake 各一次干净（本 HEAD 重刷后）
- [ ] 空 alarm 离线运行（无响铃/无持续 RTC 中断）
- [ ] 各页进入/编辑/返回无旧帧、数据持久
- [ ] alarm/reminder 进出各一次 Full（§1b）；dismiss 无 CLOCK_RECT/手工 partial/full 尾随
- [ ] alarm 非阻塞期间 USB/BLE/Tick/sync 可服务（§5）
- [ ] reminder 非阻塞期间命令与 Tick 可服务（§5）
- [ ] 音频 Start/Stop（§8）：alarm 铃响/静音、urgent siren、todo beep 各按预期
- [ ] 连续 alarm（§1b）
- [ ] BLE connect/disconnect/reconnect 完整生命周期（待设备）
- [ ] USB/BLE 命令 + Busy + 网络成功/失败/超时/恢复
- [ ] light/deep sleep 门控 + 唤醒路径（§6）
- [ ] safe-mode 注入验证（§2）——固定错误屏 + 按键不可进入页面
- [ ] 无 panic/watchdog/卡死/漏键/refresh storm

完成以上并保存串口证据后才可声明 P1#6 通过。本文件不构成完成声明。
