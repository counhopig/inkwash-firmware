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
| [`hardware-assessment.md`](hardware-assessment.md) | 以嵌入式工程师视角的总评：RAM 经济性、实时性、失效模式、取舍 | 决策/评审 |
| [`verification.md`](verification.md) | 测试与 CI 的实际覆盖、哪些结论未经真机验证 | 想知道"哪些能信" |
| [`control-protocol.md`](control-protocol.md) | USB/BLE 的 7 条命令线上协议（由代码复原） | 写上位机/桌面工具 |

## 结论速览

**这是一个"单事件循环 + 纯函数状态机 + 声明式副作用"的固件。** `logic` crate 没有硬件、没有
线程、没有 IO，只有一个 `update(&mut AppState, Event) -> Vec<EffectBatch>`；`rust-firmware`
是它的执行器，独占全部真实资源。

- ✅ **优点突出**：逻辑与硬件彻底解耦（主机可测）；并发权责干净；副作用有界可退还可重试。
- ⚠️ **代价明确**：9 个线程吃满 128 KiB 内 RAM；24 个 `pending_*` 仲裁标志 + 660 行主循环；
  Rust 源码注释为零。
- 🔴 **必须修**：`effect-task` 未订阅看门狗（持久化会静默停摆）；渲染层对 NVS 数据无边界校验；
  `screens.rs:381` 的字节切片 panic 可由服务端数据触发。

详见 `review-findings.md`。

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
主机测试      386 passed (cargo test -p inkwash-logic)
```

## 一条重要的取证提醒

`rust-firmware/src/*.rs` 与 `logic/src/*.rs` 的注释计数均为 **0**（`logic` 里的 13 处 `//`
是测试夹具中的 `https://` 字符串）。但**配置与脚本保留了注释**：`sdkconfig.defaults`（36 行）、
`.esp32-review.yml`（25 行）、`.cargo/config.toml`（4 行）、`scripts/*`。

**物理约束（DIO 模式、TWDT 超时由来、USJ 浅睡、BLE/Wi-Fi 硬件互斥）主要记录在
`sdkconfig.defaults` 和脚本里，不在 Rust 源码里。** 查"为什么这么设"时先看这两处。
