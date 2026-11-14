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
- UDP NACK 512 包缓存，SRTP 复用原索引重传；interleaved 仍走关键帧请求。

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
- `MediaDescription::new`；`Request/Response::header`；`PlaySession`/`RecordSession` 别名。
- `MountRegistry` 以 `MountId` 为键；`MountHandle::mount_id()`。
- README 示例独立 crate 编译通过；crate 包排除依赖 testkit 的 examples。
- CI 增加 experimental、SRTP/multicast、protocol clippy、srtp fuzz。

## 验证基线

```text
cargo test --no-fail-fast                         360 passed / 0 failed
cargo test --features experimental-protocols      427 passed / 0 failed
cargo test --features "srtp,multicast"            360 passed / 0 failed
cargo clippy --all-targets --features ... -D warnings  通过
cargo test --all-features --no-run                通过
cargo package --allow-dirty                       通过
```

GStreamer legacy-codec interop 在本机偶发一次失败，重跑通过；属于外部 GStreamer
时序抖动，不是库逻辑回归。

## 仍未完成

| 项 | 原因/建议 |
|---|---|
| UDP `sendmmsg` | 需要 Linux `libc`/unsafe，当前 crate 默认 `forbid(unsafe_code)` |
| startup cache Arc | 当前 Bytes 克隆已便宜；改 Arc 需改广播/快照类型 |
| mount broadcast ring 字节预算 | `tokio::broadcast` 无字节视图，需自研 bus |
| `ClientSession`/`PublisherSession` 实改名 | 已提供别名；实改名需全仓迁移 |
| `*_str` 去重、handler 命名 struct | 破坏面大，留待 0.4 |
| 完整审计文档重写 | 旧文已标历史；按本文与 architecture 矩阵为准 |
| CI semver 阻断 | 0.3.0 基线发布后启用 |

## 迁移提示

- `ServerConfig`/`MountOptions` 外部不能结构体字面量；用 builder/`Default`。
- `PushResult::DroppedQueueFull` → `Dropped`。
- `InterleavedFrame::encode` 返回 `Result`；`ReconnectingClientSession::teardown`
  返回 `Result<()>`；`SrtcpContext::unprotect` 需 `&mut self`。
- mount 路径非法现在返回 `Error::InvalidConfig`。
