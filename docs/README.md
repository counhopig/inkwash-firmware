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

## 结论速览

**这是一个"单事件循环 + 纯函数状态机 + 声明式副作用"的固件。** `logic` crate 没有硬件、没有
线程、没有 IO，只有一个 `update(&mut AppState, Event) -> Vec<EffectBatch>`；`rust-firmware`
是它的执行器，独占全部真实资源。

- ✅ **优点突出**：逻辑与硬件彻底解耦（主机可测）；并发权责干净；副作用有界可退还可重试。
- ⚠️ **代价明确**：9 个线程吃满 128 KiB 内 RAM；24 个 `pending_*` 仲裁标志 + 660 行主循环；
  Rust 源码注释为零。
- 🔴 **必须修**（P0）：
  1. 响铃期收到 `clear_alarms` → **闹钟界面永久无法退出、铃声不停，只能断电**；
  2. `effect-task` 未订阅看门狗 → 持久化会静默停摆；
  3. `screens.rs:381` 字节切片 panic，**服务端待办文本即可远程触发重启**；
  4. BLE `retired_handles` 只置位不清位 → **断线重连后应答通道永久失效**
     （⚠️ 根治需先解决 NimBLE 回调归属，**要单独立项**，不能直接清位）。
- 🟡 **工程基建失灵**：仓库自己的测试套件在 Windows 检出下是红的（CRLF）；
  `rust-firmware` 有 4 个测试**编译不过**、从未运行过。

详见 `review-findings.md` 与 `verification.md`。

## 源码事实基线

```
commit      b30c3af (chore: remove the documentation set)
logic/src         18037 行  ├─ app.rs 10420 (prod 3753 / test 6667)
                            └─ 其他 27 模块 7617
rust-firmware/src 10514 行  ├─ main.rs 2166 / ble_control 1118 / ctx.rs 1060
                            └─ 其余 33 模块 6170
版本         v0.6.0 (rust-firmware/Cargo.toml:3)
目标         ESP32-S3-WROOM-1 N16R8，4.2" 400×300 SSD2683 EPD
IDF          v5.5.5 (.cargo/config.toml:13)
主机测试      386 个，实跑 385 passed / 1 failed（CRLF 检出必失败，见 verification.md §1）
固件侧测试    3 个模块，其中 effect_task.rs 的 4 个编译不过
```

> **文档修订**
> - **首版**：由源码通读得出。
> - **第二轮**：对全部 66 个模块逐行重读 + 可执行验证，新增 P0-3 / P0-4、P1-10~13、P2-7，
>   并更正了 `verification.md` 的三处结论。
> - **第三轮**：缺陷判定全部成立，但更正了四处**修复建议错误/覆盖遗漏**
>   （P0-4 修法、P1-2 修法、P1-10 范围、P2-7d 已证伪）与两处表述过强，
>   见 `review-findings.md` 的"第三轮更正"。
>
> 标注 ✅ 实测 的条目表示写过可运行的测试或让编译器复现过。

## 一条重要的取证提醒

`rust-firmware/src/*.rs` 与 `logic/src/*.rs` 的注释计数均为 **0**（`logic` 里的 13 处 `//`
是测试夹具中的 `https://` 字符串）。但**配置与脚本保留了注释**：`sdkconfig.defaults`（36 行）、
`.esp32-review.yml`（25 行）、`.cargo/config.toml`（4 行）、`scripts/*`。

**物理约束（DIO 模式、TWDT 超时由来、USJ 浅睡、BLE/Wi-Fi 硬件互斥）主要记录在
`sdkconfig.defaults` 和脚本里，不在 Rust 源码里。** 查"为什么这么设"时先看这两处。
