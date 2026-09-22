# P0-6 未归因故障 · 交接状态汇总

> **一句话状态**：**P0-6 未归因，发布继续阻断。** 设备当前处于 **halted** 且 **USB-Serial-JTAG 控制台失效**
> （需现场重插 USB / 断电重上电）。取证包已多处归档（`logs/` **被 gitignore**，另有**工作区外快照 1–20**）。
>
> 本文面向"换机继续"的接手人：先读本文，再读 `docs/verification.md`（§7.1–§7.50 为逐轮原始记录）。

## 1. 项目与不可动摇的事实
- Rust on ESP-IDF **v5.5.5**；**ESP32-S3-WROOM-1 N16R8**；4.2" 400×300 EPD（Zectrix Note 4）。
  两个 crate：`logic/`（纯逻辑、可主机测试、**零 `unsafe`**）、`rust-firmware/`（执行器）。
- **413 个主机测试通过**（`cd logic && cargo test --locked`）；clippy 0；两处 fmt 干净；`git diff --check` 干净。
  **这些绿灯不改变 P0-6 的归因状态，也不解除发布阻断。**
- 设备 MAC `20:6e:f1:b4:7d:e4`，`esp32s3` rev v0.2，端口 `/dev/cu.usbmodem1101`（USB-Serial-JTAG = 控制台）。
- 刷机红线：**仅 DIO / 16mb / 80mhz**，必须带 `--partition-table rust-firmware/partitions.csv`；
  不得混用 Note 4 / 4C 镜像；不得调用 `esp_wifi_stop()` / `esp_restart()`。
  分区：`nvs 0x9000 0x6000`、`phy_init 0xf000 0x1000`、`factory 0x10000 0x400000`、
  `storage 0x410000 0xBF0000`（**无剩余空间，无 coredump 分区**）。

## 2. P0-6 是什么（现象，而非结论）
间歇性硬崩溃，历史上出现过多处落点（`command_sessions.rs` 缓存析构 / `_xt_context_save` 双异常 /
`poll_alarm_snapshot`），**与配置、与本项目改动"看起来"无关**：即使 TLS 与分配 100% 成功也会出现。
**至今未归因。**

## 3. 已澄清的机制（可作为后续推理的基础）
1. **异常帧在原地且完整**：`g_exc_frames[0] = s_exc_frame`，帧内 `exit = 0xdeadbeef`
   （`COREDUMP_CURR_TASK_MARKER`）——但**假栈替换**会让转储丢失该帧。
2. **假栈替换机制**：`esp_core_dump_check_stack` 若判定 `stack_start >= stack_end`，
   会用 `0x20000000` 假栈替换 `stack_start/end` 且**仍返回 true** ⇒ 帧不进转储。
3. **`XT_STK_FRMSZ = 0xC0`**：由 `_xt_panic` 序言 `addmi 0xffffff00` + `addi 64` **指令级**给出。
4. **双异常会把 EPC1 覆盖**成 `_DoubleExceptionVector`（`0x403743C0`），且该向量执行
   `movi.n a0,2` + **`wsr.exccause`**（伪因 2；coredump 再加 `XCHAL_EXCCAUSE_NUM 0x40` ⇒ `0x42`）。
   ⇒ **首次异常的 EPC1 事后无法找回**（这是"更早的向量入口捕获"的动机）。
5. **转储完整性判据**：`24 (header) + ELF extent + 4 (CRC32) == raw_len`，尾 4 字节为 CRC32。
6. **历史压缩/元数据**：本 ELF 的 `.xt.prop`/`.xt.lit` 会让 GNU objdump 在部分区域只输出原始字；
   在**临时副本**上移除这两个节即可正常反汇编（**不要**原地 objcopy）。
7. **DTR/RTS 会复位芯片**（ESP32-S3 内置 USB-Serial-JTAG）：历史脚本都设 `dtr/rts=False`
   ⇒ **历史实验应统一理解为"冷启动后的观测窗口"**（启动横幅被 `reset_input_buffer` 冲掉）。

## 4. 已被推翻 / 已撤回的结论（避免重走弯路）
| 曾提出的说法 | 现状 |
|---|---|
| "缺少 S3 ISA 配置导致无法解码" | **错**。障碍是 `.xt.prop`/`.xt.lit` 元数据 |
| "`usleep` 组 timespec 后调 `nanosleep`" | **错**。本 ELF 是 `l32r`+`callx8` 调 **`vTaskDelay`** |
| "`0x600fe1e8` 是 PSRAM（lwIP 用 PSRAM 栈）" | **错**。是 **RTC fast RAM**（`SOC_RTC_IRAM/DRAM 0x600FE000..`） |
| "从已转储 TCB 走链 ⇒ 任务集合完整" | **不成立**（不能排除未访问的非空链表） |
| "EMPTY ⇒ handler 未进入" | **不成立**（CAS 缺陷会让已进入者判为 EMPTY；用户亦指出） |
| "`panic_abort` 绕过异常表" | **错**。它执行 `ill` ⇒ **IllegalInstruction(cause 0)**，**会**经过异常表 |
| "一致性检验不通过（a8 与帧不符）" | **撤回**：属**缺少有效性证明**，不是反证 |
| "实验中并未复位" | **未确定**（单个 reset-cause 读数不能覆盖整个历史） |
| "QEMU/主机模拟必然假阳性" | 不准确。准确边界：**其结果不能替代真机验收** |
| "三窗口是连续无复位实验" | **不成立**：win2 首次无应答已触发"停止并采集"，**win3 不应继续**（协议违背） |

## 5. 为 P0-6 建立的工具（现状与覆盖边界）
- **记录器组件** `rust-firmware/components/p06_recorder/`（C，`WHOLE_ARCHIVE`）：
  - 逐 cause 的 handler（0..31）→ 从**传入的 `XtExcFrame` 复制原始现场**（`f_*`），
    另列**记录时刻的特殊寄存器**（`live_*`，**不是**首次异常状态）；
  - 手写内联 CAS（`wsr.scompare1` + `s32c1i`，**成功判据 `old == expect`**，与 libatomic 一致）；
  - 状态机 `EMPTY → CLAIMED → memw → DONE`；记录区在 **RTC slow RAM**（`0x50000000`，复位后可回读），
    CAS 目标在 **DRAM**（避免对 RTC 慢速 RAM 施加原子语义）；
  - **链回真实旧 handler**（`xt_set_exception_handler` 的返回值）；`prev == 0` 时调
    **`xt_unhandled_exception`**（否则会"返回但不处理"⇒ 反复执行同一故障指令 ⇒ WDT）。
  - **构建门控**：`p06_diag`（只 `p06_arm()`，**无人工触发**）／`p06_validate`（core 0 V2/V3）／
    `p06_validate_core1`（core 1 V2）。三种诊断构建都会先在两核安装并复核 handler 表。
- **覆盖边界（务必先读）**：在 **core 0 和 core 1** 分别安装（`xt_set_exception_handler` 按
  `cause*portNUM_PROCESSORS + core_id` 索引），旧 handler 也按「核 × cause」保存；覆盖 **cause 0..31**；
  **不覆盖**：表分发**之前**分流的 1(syscall)/5(alloca)/4(level-1 中断)/≥32(coproc)、cause≥32、
  **双重异常**、**建帧/`_xt_context_save`/PS 改写阶段**的再异常。
- **主机侧验收解码器** `tools/p06_accept.py`（单文件、仅标准库）：布局由宿主 cc 编译真实结构体取得；
  固定样本自检 **7/7 通过**；`--elf` 每次都从**最终验证 ELF** 重新提取期望 PC。
- **采集脚本**：`logs/hw-forensics/p0-6-halt/capture_stress.py`（原压力序列 + 原始字节，Guru 即停）、
  `logs/hw-forensics/p0-6-diag/diag_window.py`（应答跟踪；停发条件：Guru/WDT/意外复位/无应答>15s/应答畸形）。

## 6. V2 / V3：工具验证结果（**范围必须按此命名**）
- **V2（通过）**：受控异常**到达 handler** 后，记录器**忠实复制异常帧**
  （整帧 **25/25** 逐字段一致；`f_pc` 等于**由最终 ELF 提取**的期望 PC）。
- **V3（通过）**：**第一次 handler 返回、调试器跳过故障指令之后，再发生一次独立异常**，
  **首条记录未被覆盖**（`f_pc`/`seq` 均未变）。
- **V3 不是嵌套异常测试** ⇒ **不能**证明"记录器或默认 handler **执行期间**再异常"的行为。

## 7. 自然故障观测（诊断构建，`p06_diag`）
- **win1**（25/25 正常跑完）：**本窗口未复现**。
- **win2**（9/25）与 **win3**（5/25）：均因 **`no-reply`（15 s 无应答）提前停发**。
  ⚠️ **协议违背**：win2 首次无应答已满足"停止并采集"，**win3 不应执行**。
- 应答并非 busy，而是 **`PCF8563 read regs 0x00 failed: ESP_FAIL`**（外部 RTC I²C 读失败，时好时坏）。
- **现场（复位前）**：已武装（`magic=0x50303631`、RTC `nonce` == DRAM `g_p06_nonce`、`boot_done=1`）；
  **无记录**；`g_exc_frames = {0, 0x3FCE7380}`、`s_exc_frame = 0x3FCE7380` ⇒ **panic 在 core 1**。
- **core 1 现场分析**：帧 `pc = 0x403834D8` = **`ill`**（`panic_abort` 内），`exccause = 0`
  （IllegalInstruction），`excvaddr = 0`；故障任务 = **core 1 的 `pthread`**
  （帧与帧内 a1 均在该任务栈内）；详情字符串
  **`abort() was called at PC 0x4209f8ce on core 1`**（`g_panic_abort_details = 0x3FCE7480`，
  指向任务栈内字符串）；`0x4209F8CE` = **`std::sys::pal::unix::abort_internal`**。
  ⇒ **已确认的调用链**：`abort_internal → IDF abort → panic_abort → ill`。
  ⇒ **未确认**：其上游是否为 Rust panic/unwind，**原始 panic 原因更未确认**（仅凭邻近符号与栈内候选不够）。
  ⇒ 与早前 P0-6（core0 / `_DoubleExceptionVector` / `exccause=2` / `excvaddr=0xCECECE00` / `main`）
    **签名不同**，但**原因未确认，不宣布与 P0-6 无关**。
- **"是否发生复位"= 未确定**：OpenOCD 只报 `Reset cause (1) Power on reset`（= reset button），
  **不能**覆盖整个历史期间；win3 小 uptime 更可能来自**未清缓冲的陈旧数据**，但**该解释仍缺证据**。

## 8. 设备与链路当前状态（接手人必读）
- 设备运行 `p06_diag`，记录器双核武装掩码为 `0x3`。
- USB-Serial-JTAG 在 JTAG `reset run` 后已恢复；`get_status` 应答正常。
- PCF8563 开机读取正常，`vl=false`。
- USB 节点只能用于定位设备，刷写前仍必须核对 USB 序列号/MAC。

## 9. core 1 受控捕获结果（2026-09-19）
- USB 序列号和芯片自报均确认目标 MAC 为 `20:6E:F1:B4:7D:E4`，ESP32-S3 rev v0.2，16 MB。
- `p06_validate_core1` 在 core 1 触发 cause 28；记录状态 `DONE`，`f_pc=0x421880D1`，
  与最终 ELF 的故障指令地址一致，`f_excvaddr=0`。
- PC、PS、A0–A15、SAR、EXCCAUSE、EXCVADDR、LBEG、LEND、LCOUNT，共 **24/24 个稳定字段**
  与 `frame_ptr=0x3FCAF610` 指向的异常帧逐字相等。
- `exit` 不能用 panic 停机后的帧做事后相等判据：记录器入口值为 `0x3FCAF6C0`，
  默认 panic/coredump 链路随后把帧内值改为 `0xDEADBEEF`。原“25/25 事后相等”判据修正为
  “24/24 稳定字段相等，`exit` 按时序单独判定”。
- 记录区 SHA-256：`62eb12a28a7015f7ae46e174c4228db4ae0caa28c45b908306620e0012346fc5`。
- 异常帧 SHA-256：`0c867546368b6810639d92e16200a4c5ba3167c01bef69f147c49e4cad7c8898`。

## 10. 下一步
1. 恢复压力测试。
2. 是否推进**更早的向量入口捕获**（以覆盖建帧阶段/双重异常）留待决定。

## 11. 归档位置与校验
- 仓库内：`logs/hw-forensics/**`（**被 `.gitignore` 忽略**，不会随提交保存）：
  `p0-6-halt/`（保留现场、栈/TCB/链表、解码器证据、OpenOCD 原始输出）、`p0-6-coredump/`、
  `p0-6-first-exception/`、`p0-6-v2/`、`p0-6-v3/`、`p0-6-diag/`、`p0-6-stacks/` 等。
- **工作区外快照**：`~/inkwash-forensics-backup/<时间戳>/`（**快照 1–20**），每个含
  `FILELIST.txt` + `MANIFEST.sha256`（逐文件 SHA256，`shasum -c MANIFEST.sha256` 可复核）。
  ⇒ **换机前请一并复制该目录**；`logs/` 不受提交保护。

## 12. 快速上手（命令）
```sh
# 主机测试
cd logic && cargo test --locked

# 构建三态（均需先 source IDF 环境：. $HOME/esp/esp-idf/export.sh）
cargo build --release                          # 默认（记录器组件不启用任何入口）
cargo build --release --features p06_diag      # 诊断：只装记录器，无人工触发
cargo build --release --features p06_validate  # 验证：含 V2/V3 人工触发
cargo build --release --features p06_validate_core1 # 验证：core 1 V2 人工触发

# 刷机（红线：DIO/16mb/80mhz + 指定分区表）
espflash flash --port /dev/cu.usbmodem1101 --chip esp32s3 --flash-size 16mb \
  --flash-mode dio --flash-freq 80mhz \
  --bootloader rust-firmware/target/xtensa-esp32s3-espidf/release/bootloader.bin \
  --partition-table rust-firmware/partitions.csv --partition-table-offset 0x10000 \
  --non-interactive \
  rust-firmware/target/xtensa-esp32s3-espidf/release/inkwash-note4

# 解码器
python3 tools/p06_accept.py --layout
python3 tools/p06_accept.py --selftest
python3 tools/p06_accept.py --dump <记录区原始字节> --vaddr 0 --cause 28 --nonce 0x...
```
**工具链陷阱**：① 组件改动后需**删除 `esp-idf-sys-*/` 构建目录 + `touch build.rs`** 才会真正重建
（否则 ninja 会跳过）；② 本机 `.cargo/config.toml` 设了 `rustflags` ⇒ **`RUSTFLAGS` 环境变量会被忽略**
（故用 Cargo **feature** 门控）；③ **禁止 GDB attach**（本工程 `MEMPROT_FEATURE=y` ⇒ `gdb-attach` 会
`reset halt` 毁掉现场）；只用 OpenOCD CLI；④ 串口脚本**不要触碰 DTR/RTS**（会复位）。

**P0-6 未归因，发布继续阻断。**
