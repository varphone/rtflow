# 2026-11 全面审计与重构状态

本文件记录本轮“可靠性、易用性、性能、协议完成度、接口/类型设计、命名”审计的
落地状态。代码按 P0→P1→P2→P3 分批提交，每批均在本地跑格式化、clippy、默认与
experimental 测试；不 push、不运行 CI。

## 结论

- P0（安全/正确性/内存边界）已完成关键项：SRTP/SRTCP、SAVP、Gateway registry、
  MountOptions、mux buffer、panic/DoS、客户端丢旧帧、RTMP/HLS 边界。
- P1（可靠性/低延迟）完成：SSRC/CNAME、静默失败、重复 SETUP、Proxy 跨协议触发、
  VOD EOS、NACK 重传、生命周期 cancel。
- P2（性能）完成：精确 UDP 收包、`try_send_to` 快路径、interleaved 批量写、
  quality 锁与 jitter deadline O(1)、统计按帧汇总。
- P3（API/文档）完成：0.3.0、non_exhaustive、脱敏 Debug、`MediaDescription::new`、
  header 访问器、`MountId` registry 键、README 可编译示例、CI 覆盖、报告。
- 未完成项见文末；不影响已列出的安全与正确性结论。

## P0 关键修复

| 问题 | 处理 |
|---|---|
| SRTP 发送 ROC/索引复用、接收回绕 | 每 SSRC 维护 ROC/last_seq/index；跨回绕推断；拒绝 index 回退 |
| SRTCP 重放 | replay window + `&mut unprotect`；各传输路径统一 |
| SAVP 互操作 | `a=crypto` 时 `m=` 输出 `RTP/SAVP`；第三方 SAVP SETUP 接受 |
| Gateway  mount 不可见 | `bind_with` 共享 `MountRegistry` |
| MountOptions 不生效 | 单一构造路径合并 server 默认；启动缓存/队列/timeline/cancel 生效 |
| mux 缓冲硬编码 | 使用 `udp_recv_buffer` |
| RTSP/WS/RTMP/HLS/HTTP 超时与 panic | 写超时、帧上限、writer 退出、accept 退避、flush 上限 |
| 客户端慢消费者内存 | FrameQueue 字节+条目双上限，满时丢最旧 |
| RTCP BYE | RR+BYE+SDES compound |
| 代理 RTCP 地址泄漏 | DESCRIBE 去掉上游 `a=rtcp` |

## P1 关键修复

- SSRC 按进程随机种子+SessionId+track 生成；CNAME 含唯一 SSRC。
- packetize/SRTP/录制/发布 keepalive 失败不再静默。
- 播放中可重新 PLAY；重复 SETUP 返回 455；组播轨计入上限；Transport 候选按
  mount/SRTP 能力过滤。
- proxy watcher 让 HLS/HTTP-FLV 订阅也触发按需上游。
- VOD 自然 EOS：排空广播后关闭会话（RTSP 无 EOS 方法）。
- UDP/interleaved NACK 512 包缓存（2 MiB 字节预算），SRTP 复用原索引重传；
  NACK PID+BLP 展开统一到 `Packet::nack_sequences()`；raw RTSP e2e 验证
  UDP NACK 触发缓存重传。

## P2 性能

- UDP 收包改为精确长度 `Bytes` 拷贝（不再每包保留 64 KiB）。
- UDP SNDBUF 1 MiB；发送 `try_send_to` 快路径；interleaved 每帧一次批量写。
- RTP 统计每帧汇总，减少全局原子 RMW。
- RTCP quality 改 std Mutex，移除每包 await。
- JitterBuffer 缓存 earliest deadline，`next_deadline`/`is_expired` O(1)。
- RTMP 重组总预算 32 MiB；HLS 段样本硬上限；mount 单帧字节上限。

## P3 API/文档

- 0.3.0；`ServerConfig`/`MountOptions` non_exhaustive；`PushResult::Dropped`。
- `ClientOptions`/`ServerConfig`/`MountOptions` 脱敏 Debug。
- `MediaDescription::new`；`Request/Response::header`；会话主名称改为
  `PlaySession`/`RecordSession`/`ReconnectingPlaySession`，旧名保留别名。
- `MountRegistry`/`MountHandle`/`snapshots` 使用 `MountId`；`MountOptions::builder()`。
- HLS/DASH/WebRTC 提供命名 handler 集合与 `create_*_router`（旧 tuple 保留）。
- README 示例独立 crate 编译通过；crate 包排除依赖 testkit 的 examples。
- CI 增加 experimental、SRTP/multicast、protocol clippy、srtp fuzz。

## 验证基线

```text
cargo test --no-fail-fast                         370 passed / 0 failed
cargo test --features experimental-protocols      437 passed / 0 failed
cargo test --features "srtp,multicast"            此前 351 passed / 0 failed
cargo clippy --all-targets --features ... -D warnings  通过
cargo test --all-features                         435 passed / 0 failed
cargo test --all-features --no-run                通过
cargo package --allow-dirty                       通过
```

GStreamer legacy-codec interop 在本机偶发一次失败，重跑通过；属于外部 GStreamer
时序抖动，不是库逻辑回归。

## 仍未完成

| 项 | 原因/建议 |
|---|---|
| UDP `sendmmsg` | 已完成：Linux nix 批量发送 + 逐包回退 |
| startup cache Arc | 已完成：cache/FrameBus/startup 共享 Arc<AccessUnit> |
| mount broadcast ring 字节预算 | 已完成：FrameBus 双预算替换 broadcast |
| `*_str` 去重 | 已完成：入口统一接受 `IntoRtspUrl` |
| NACK 线级 e2e | 已完成：`tests/rtsp/nack.rs`（UDP） |
| 完整审计文档重写 | 旧文已标历史；按本文与 architecture 矩阵为准 |
| CI semver 阻断 | 0.3.0 基线发布后启用 |

## 迁移提示

- `ServerConfig`/`MountOptions` 外部不能结构体字面量；用 builder/`Default`。
- `PushResult::DroppedQueueFull` → `Dropped`。
- `InterleavedFrame::encode` 返回 `Result`；`ReconnectingClientSession::teardown`
  返回 `Result<()>`；`SrtcpContext::unprotect` 需 `&mut self`。
- mount 路径非法现在返回 `Error::InvalidConfig`。
