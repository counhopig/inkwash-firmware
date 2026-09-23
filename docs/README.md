# Inkwash 固件 — 架构与评审文档

本目录是对 `inkwash-firmware` 的**源码级架构梳理 + 静态评审**结果。全部结论来自逐文件阅读
`rust-firmware/src/`（36 个模块）与 `logic/src/`（28 个模块）以及构建配置、脚本、CI。

> **文档性质**：这些是**分析产物**，不是项目原有文档的重建。仓库原有的 `docs/`
> （`development-guide.md`、`control-protocol.md`、`screenshots/`）已被 commit `b30c3af`
> 删除；本目录只重建了其中**能从源码确证**的部分（见 `control-protocol.md`），
> 其余（开发环境、板级参考、冒烟测试清单、截图）需要原作者补齐。

## 怎么读

| 文档 | 内容 | 适合谁 |
|---|---|---|
| [`architecture.md`](architecture.md) | 分层、主循环、Event→Effect 控制回路、渲染/传输/同步/电源各机制 | 想理解"这个固件怎么运转" |
| [`concurrency-and-resources.md`](concurrency-and-resources.md) | 9 个线程/栈预算、共享资源矩阵、24 个仲裁标志、背压容量表 | 改并发、加功能、排查干扰 |
| [`review-findings.md`](review-findings.md) | 缺陷与风险清单（P0–P2），逐项 file:line + 触发条件 + 修法 | 要动手修 bug |
| [`hardware-assessment.md`](hardware-assessment.md) | 以嵌入式工程师视角的**结构评价**：七维打分、RAM 账、失效模式、验证基建、四仓库结构、量产前置条件 | 决策/评审 |
| [`verification.md`](verification.md) | 测试与 CI 的实际覆盖、哪些结论未经真机验证 | 想知道"哪些能信" |
| [`control-protocol.md`](control-protocol.md) | USB/BLE 的 7 条命令线上协议（由代码复原） | 写上位机/桌面工具 |
| [`fixes.md`](fixes.md) | **第四轮修复记录**：每条缺陷的处置状态、已修的改法与未修的理由 | 想知道"修了什么、还剩什么" |

## 结论速览

**这是一个"单事件循环 + 纯函数状态机 + 声明式副作用"的固件。** `logic` crate 没有硬件、没有
线程、没有 IO，只有一个 `update(&mut AppState, Event) -> Vec<EffectBatch>`；`rust-firmware`
是它的执行器，独占全部真实资源。

- ✅ **优点突出**：逻辑与硬件彻底解耦（主机可测）；并发权责干净；副作用有界可退还可重试。
- ⚠️ **代价明确**：9 个线程吃满 128 KiB 内 RAM；24 个 `pending_*` 仲裁标志 + 660 行主循环；
  Rust 源码注释为零。
- 🔴 **原 P0 四项**（详见 `fixes.md`）：
  1. 响铃期收到 `clear_alarms` → 闹钟界面永久无法退出、铃声不停，只能断电 —— **已修**
     （含"阻断页无条件出路"护栏）；
  2. `effect-task` 未订阅看门狗 → 持久化会静默停摆 —— **已修**；
  3. `screens.rs` 字节切片 panic，服务端待办文本即可远程触发重启 —— **已修**；
  4. BLE `retired_handles` 只置位不清位 → 断线重连后应答通道永久失效
     —— **未改代码**：根治必须先解决 NimBLE 回调归属，**要单独立项**（`fixes.md` 给了两条路线）。
- 🟡 **工程基建**：仓库自己的测试套件在 Windows 检出下是红的（CRLF）—— **已修**（`.gitattributes`）；
  `rust-firmware` 的测试**从未运行过**（不只是编译不过）—— **已修**（移入主机侧 + CI 门禁）。

详见 `review-findings.md` 与 `verification.md`。

## 源码事实基线

```
commit      b30c3af (chore: remove the documentation set)
logic/src         18681 行   ├─ app.rs 10576
                             └─ 其他 29 模块（新增 sanitize.rs）
rust-firmware/src 10299 行   ├─ main.rs 2168 / ble_control 1118 / ctx.rs 1060
                             └─ 其余模块
版本         v0.6.0 (rust-firmware/Cargo.toml:3)
目标         ESP32-S3-WROOM-1 N16R8，4.2" 400×300 SSD2683 EPD
IDF          v5.5.5 (.cargo/config.toml:13)
主机测试      413 个，实跑 413 passed / 0 failed（LF 检出；CRLF 问题已由 .gitattributes 修掉）
固件侧测试    0 个模块（原 3 个模块的 #[test] 函数全部从不运行，已删除，见 verification.md §6）
真机验证      第六轮已上机（Note 4 / esp32s3 v0.2 / 16MB / MAC 20:6e:f1:b4:7d:e4）：
              boot、控制协议、NVS 跨复位持久化、压力+soak 通过；app 占 62.98%
              真机新发现 P0-5：HTTPS/TLS 必然 ALLOC_FAILED（A/B 证实非本轮引入）
              真机新发现 P0-6：命令压力下间歇硬崩溃，落点在本轮未改动的
              command_sessions.rs；归因未定（基线 6 次全 0）→ 定性前不建议发布
              堆取证：70/70 次 TLS 失败，int_largest 仅 7.7–12.3KB，PSRAM 8.37MB 未用
              （第①步已确认：失败分配=ssl->in_buf 16,717B，caps 禁 PSRAM）
              实验②（DYNAMIC_BUFFER=y）：setup 70/70→0/45，但失败点移到 handshake
              的 ~4,770B，int_largest=4608 时仍失败 → 缓解非修法
              实验③（EXTERNAL_MEM_ALLOC=y，从原始基线单变量）：request-ok 40/40、
              分配失败 0（最干净），但 P0-6 仍崩溃（poll_alarm_snapshot，第三个落点）
              非空数据应用已验证：NVS 与服务器 payload 一致且跨复位保持
              证据包在 logs/hw-forensics/（gitignored）
```

> **文档修订**
> - **首版**：由源码通读得出。
> - **第二轮**：对全部 66 个模块逐行重读 + 可执行验证，新增 P0-3 / P0-4、P1-10~13、P2-7，
>   并更正了 `verification.md` 的三处结论。
> - **第三轮**：缺陷判定全部成立，但更正了四处**修复建议错误/覆盖遗漏**
>   （P0-4 修法、P1-2 修法、P1-10 范围、P2-7d 已证伪）与两处表述过强，
>   见 `review-findings.md` 的"第三轮更正"。
> - **第四轮（修复）**：实施了 P0-1/2/3、P1-1/2/5/8/9/10/11/13、验证基建与部分 P2-5；
>   新增 `fixes.md` 记录处置状态。**同时更正两处评审自身的错误结论**：
>   ① "`cargo check --all-targets` 能抓到固件的 4 个测试" **已证伪**
>      （`harness = false` 使 `#[test]` 函数体根本不被类型检查；模块内普通函数仍会被检查）；
>   ② 固件侧测试覆盖不是"3 个模块"也不是"2 个模块"，而是 **0**；
>   ③ P1-10 的漏采屏幕清单遗漏了 **`Screen::Home`**（用户停留最久的页面）。
> - **第五轮（评审独立复现后的三项调整）**：
>   ① **Home 仍会漏刷**：分钟与数据同时变化时 `plan_render` 优先返回 `Clock` 局刷，
>      刷完缓存新指纹，下一次 `Noop`，图标停在旧状态 —— 已改为仅当指纹也未变时才局刷；
>   ② **配对超时被时区调整提前触发**：deadline 原先锚在可修改的 RTC 墙钟上 ——
>      已改为锚定单调 `PowerPoll.now_ticks`；
>   ③ **§6 的表述范围过大**：被跳过的只有 `#[test]` 函数体，`#[cfg(test)]` 模块内的
>      普通函数/方法/类型**仍会被类型检查** —— 已收窄文档与契约测试的措辞。
> - **第六轮（遗留墙钟超时收口 + 真机验证）**：提醒（120 s）与响铃自动消音（300 s）
>   两处 deadline 也从墙钟改为单调 `PowerPoll.now_ticks`，`logic` 中**已不存在**墙钟
>   锚定的超时判定。修复固件随后**烧入真机验证**（`verification.md` §7）：
>   启动、控制协议、NVS 跨复位持久化、压力+soak 全部通过，新增 `scripts/smoke-note4.py`。
>   真机同时暴露 **P0-5：HTTPS/TLS 必然 `MBEDTLS_ERR_SSL_ALLOC_FAILED`**，
>   已用基线 A/B 证实与本次修复无关——但它使设备无法从服务端取任何内容，
>   也因此让 P0-2 / P0-3 目前**没有真机证据**。
>
> 标注 ✅ 实测 的条目表示写过可运行的测试或让编译器复现过。

## 一条重要的取证提醒

`rust-firmware/src/*.rs` 与 `logic/src/*.rs` 的注释计数均为 **0**（`logic` 里的 13 处 `//`
是测试夹具中的 `https://` 字符串）。但**配置与脚本保留了注释**：`sdkconfig.defaults`（36 行）、
`.esp32-review.yml`（25 行）、`.cargo/config.toml`（4 行）、`scripts/*`。

**物理约束（DIO 模式、TWDT 超时由来、USJ 浅睡、BLE/Wi-Fi 硬件互斥）主要记录在
`sdkconfig.defaults` 和脚本里，不在 Rust 源码里。** 查"为什么这么设"时先看这两处。

## 交接状态

- [P0-6 未归因故障 · 交接状态汇总](handover-p0-6.md) —— **换机/换人继续前先读**：
  已验证事实、已撤回结论、工具与覆盖边界、V2/V3 结果与范围命名、设备链路现状、下一步与归档位置。
