# ADR-0001：保持单发布 crate

- 状态：已采纳（2026-11）
- 相关：布局重构 P1–P6

## 背景

`rtflow` 约 3.6 万行、115 个源文件，通过 19 个 feature 划分 RTSP/RTP 核心、
协议适配器（HLS/DASH/HTTP-FLV/RTMP/SRT/WebRTC/GStreamer）与嵌入式 profile。
曾评估拆成 `rtflow-core` + `rtflow-protocols` + `rtflow-server` 的 workspace。

## 决策

**保持单个发布 crate**，模块边界由目录、可见性与 `scripts/check-layering.py`
强制；不引入多个发布 crate。

理由：

1. 嵌入式用户需要一个依赖 + feature 选择（`embedded-gateway`），拆 crate 会让
   feature 统一（feature unification）与依赖声明变复杂。
2. 各子系统没有独立版本节奏或外部使用者；拆分的收益主要是并行编译，而当前
   全量 `cargo test` 的门槛仍在可接受范围。
3. 真正的耦合问题已通过消除循环依赖（SCC=0）解决，crate 拆分并非必需。

## 后果

- 允许新增一个**不发布**的 dev-only support crate（`rtflow-testkit`，P5），
  用于测试/示例共享线束；它不进入 crates.io 包。
- 重新评估的触发条件（任一满足）：
  - 某子系统（如 `rtsp`/`sdp`）出现独立第三方使用者或独立版本需求；
  - 增量编译或 CI 时间成为主要瓶颈（例如单次全量测试 > 15 分钟）；
  - 需要按子系统做独立的安全审计/发布节奏。
