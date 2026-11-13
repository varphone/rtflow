# 审计索引与状态

> 历史说明：以下审计文中的部分路径引用 0.3 布局重构前的 `src/*.rs`，状态以
> 当前 `docs/architecture/` 与测试为准；不要把旧文中的“未修”直接当成现状。

| 审计 | 范围 | 状态 |
|---|---|---|
| [code.md](code.md) | 代码级问题（R1–R25）与修复状态 | R23 等已修复，见文内标注 |
| [functionality.md](functionality.md) | 功能完整性/协议矩阵 | 已收敛，遗留项见文内 |
| [usability.md](usability.md) | 可用性 H/M/L 项 | H1–H4 已落地 |
| [embedded-resource.md](embedded-resource.md) | 嵌入式内存/CPU/二进制尺寸 | P0/P1 已落地 |
| [realtime-compat.md](realtime-compat.md) | 实时链路兼容性（RT-*） | 多数已修复/列入路线图 |
| [completion.md](completion.md) | 交付完整性核对 | 历史快照 |

## 布局重构相关（2026-11，0.3）

| 审计条目 | 处置 |
|---|---|
| R23 RTSP keep-alive 无退避 | 已修复：`client::control` 指数退避 + GET_PARAMETER→OPTIONS 降级；回归测试 `client_edge::keep_alive_failures_back_off` |
| RT-7/RT-8 keep-alive 与媒体串行 | 已修复：控制面独立任务（`client::control`） |
| RT-9 冷启动多 RTT | 客户端侧 461 回退复用连接；预热/跳过 DESCRIBE 见 roadmap |
| RT-17 jitter 轮询 | 已修复：事件驱动 `next_deadline()` + 满队列丢弃 |
| 布局类问题 | 见 [../architecture/overview.md](../architecture/overview.md) 与 ADR-0001–0004 |
