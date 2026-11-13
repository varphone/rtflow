# Changelog

本文件遵循 [Keep a Changelog](https://keepachangelog.com/zh-CN/1.1.0/)，
版本号遵循 [Semantic Versioning](https://semver.org/lang/zh-CN/)。

## [0.3.0] — 2026-11（布局与可靠性基线）

### Changed

- 模块布局：`src/` 目录与模块路径一一对应，移除全部 `#[path]`；`protocols/`
  收纳协议适配器，`rtsp/` 成为真实模块，`pipeline/` 提升为顶层。
- 依赖分层：模块依赖图无环（SCC=0），`scripts/check-layering.py` 强制；
  低层不再反向依赖 `server`/`metrics`/`proxy`。
- 公共路径规范化并保留弃用门面：`protocol`→`rtsp`、`proxy`→
  `mount::{upstream,continuity}`、`protocol_trait`→`gateway::protocol`、
  `media_pipeline`→`pipeline`+`rtp`、`server::{Mount*}`→`mount`。
- `MetricsRegistry`/`Demuxer`/`VodSource`/SDP 值类型下沉到更底层模块，
  旧路径经再导出保持可用。
- feature 语义修正：`dash` 依赖 `hls`、`webrtc` 依赖 `http`、`hmac`
  改为非可选依赖；19 个 feature 均可独立构建。
- 大文件拆分：`mount`、`metrics`、`client`、`rtp::payload`、`hls::segmenter`、
  `http_flv::mux`、`rtmp::session`、`server::{session,mod,tasks}`、`pipeline`、
  `srt`、`rtsp::auth`、`file::{mpeg_ts,raw}`、`storage::recording` 均拆为子模块；
  所有 `src/*.rs` 均 ≤600 行（最大 587），SETUP/PLAY/ANNOUNCE 已从连接循环抽取。
- 测试与示例分组：`tests/<area>/main.rs`（8 个测试二进制）、
  `examples/{server,client,protocols,gateway,embedded}/` + 显式 `[[example]]`；
  共享线束迁移到不发布的 `support/rtflow-testkit`。
- 文档重组为 `docs/{architecture,guides,audits,roadmap,archive}`，新增 ADR
  0001–0004 与 0.2→0.3 迁移指南；`tools/` 合并进 `scripts/`。

### Added

- RTSP：`rtcp-mux`（UDP 单端口/TCP 单 channel）、SDP 多 PT/`a=framerate`/
  IPv6 `c=`、绝对 `Content-Base`、`Proxy-Require` 551、随机 128-bit 会话 ID、
  `SET_PARAMETER`、PLAY `Scale/Speed` 551、客户端 3xx REDIRECT 跟随。
- 码流：VP8/VP9 SDP 往返、MP3（MPA）与 G.726 16/24/32/40 的 RTP/SDP 支持。
- RTSP 组播（`multicast`）与 SRTP（`srtp`，RTP/SAVP + a=crypto，UDP/TCP/
  组播）；两者不再属于默认 `full`，需显式启用。
- Transport `destination`/`source`/`mode` 解析与回显；SDP `a=sendonly/recvonly`、
  `a=ptime`、`a=rtcp:`、`b=AS`；AV1/JPEG/H.263/MPEG-4 Part 2 RTP/SDP。
- `test-source` feature 与 `rtflow::source::SyntheticSource`（库内实时测试源，
  覆盖全部 codec）。
- `ServerConfig::{udp_recv_buffer, embedded}` 与
  `StartupCacheConfig.max_bytes_total`（嵌入式 256 KiB UDP 缓冲、512 KiB
  启动缓存预设）；`RtpPacket::encode_into` 复用发送缓冲。
- `rtflow::prelude`。
- `scripts/{check-layering.py,check-tree.py,check-docs-paths.sh}` 布局护栏与
  CI `layout` job；CI 增加 cargo-hack 全 feature 矩阵、cargo-deny、
  cargo-semver-checks、release workflow 与 dependabot。
- `LICENSE-MIT`、`LICENSE-APACHE`（此前只有 SPDX 声明）、CHANGELOG、
  CONTRIBUTING、SECURITY、`deny.toml`。

### Fixed

- RTSP：`Content-Base` 非绝对 URI、`Proxy-Require` 未处理、SDP track 无上限、
  客户端 interleaved RTCP 写无超时、服务端 UDP 接收多一次拷贝。
- RTSP client keep-alive 失败无退避（审计 R23）、控制面与媒体串行
  （RT-7/RT-8）、jitter 轮询（RT-17）。
- no-default 与单 feature 构建此前无法编译（`hmac` 可选、gateway/mount
  未按 feature 门控、`dash`/`webrtc` 缺依赖）。
- fuzz crate 依赖名与包名不一致导致目标无法编译。

- RTSP 客户端补齐 OPTIONS 能力发现、PLAY `Range/Scale/Speed`、PAUSE `Range`、
  `SET_PARAMETER`/`GET_PARAMETER`、interleaved 控制面 Digest 重试与
  `ClientOptions::low_memory()` 预设；新增 32 路并发客户端测试。
- RTSP 客户端发布：`RtspClient::publish` / `PublisherSession::push`（ANNOUNCE/SETUP/RECORD、UDP、多轨、保活、TEARDOWN）。
- RTSP 客户端发布支持 TCP interleaved；输出队列增加字节上限
  (`ClientOptions::max_output_bytes`，低内存预设 2 MiB)。
- RTSP 客户端消费 PLAY `RTP-Info`：seq 预置 jitter buffer，rtptime 锚定非零 NPT 范围的 PTS；新增解析/单测。
- RTSP 客户端支持 `Require`/`Proxy-Require`，并处理 server-initiated `REDIRECT`（回 200、上抛 `Error::Redirect` / `ClientEvent::Redirect`）。
- RTSP 发布客户端支持 SRTP（UDP/interleaved）；新增接收路径 p50/p99 延迟冒烟测试。
- RTSP 发布端周期性 RTCP SR（UDP/interleaved，SRTP 时 SRTCP 保护）。
- 发布端 RTCP SR 增加线级测试（RTP/SR 分别到达协商的 server_port）。
- RTSP 客户端连接建立阶段自动跟随 server-initiated REDIRECT（同源；跨源需显式开启）。
- REDIRECT 在 `connect_reconnecting` 中自动切换目标；551 解析为带 token 的 `Error::RtspUnsupported`。
- RTSP 客户端完成 RTSP 1.0 方法面与连接行为审计（OPTIONS/DESCRIBE/ANNOUNCE/SETUP/PLAY/PAUSE/RECORD/TEARDOWN/GET_PARAMETER/SET_PARAMETER、REDIRECT、Require、RTP-Info、Range/Scale/Speed、认证/TLS/SRTP/组播、并发与低内存基线）。
- RTCP 完整包型：APP、RTPFB/PSFB（NACK/PLI/FIR/REMB）、XR；新增反馈计数 `FeedbackSnapshot`。
- RTCP `RtcpInterval`（带宽/成员数/随机化）与 `RtcpSession::bye` compound 构造。
- RTCP 反馈动作：PLI/FIR/NACK 触发本地源关键帧请求；SSRC collision 检测、reduced-size compound、XR RRTR/DLRR。
- RTSP 服务端新增 `TimelineMode`（Relative/ServerUptime/Unix/Absolute），可选择摄像机风格的绝对 NPT 进度。
- 绝对 NPT 模式的发送端 RTCP SR NTP 与 Range/RTP-Info 使用同一时间轴。
- 安全修复：SRTP 发送 ROC 滚动与序号回绕、SRTCP replay/index、SAVP SDP、interleaved/UDP/组播 SRTCP 路径、RTCP BYE；SRTP 扩展/填充边界校验。
- 架构修复：`RtspServer::bind_with` 共享 Gateway registry；server 统一走 `MountRegistry::register_with`，`MountOptions`（queue/cache/timeline/cancel）生效；rtcp-mux 使用配置的 UDP receive buffer。
- 稳定性：RTSP 写超时、WS 写超时/帧上限、RTMP writer 退出唤醒、HTTP accept 退避、HLS ring 无 panic/样本上限、`InterleavedFrame::encode` 返回 Result、`SdpCrypto` Debug 脱敏。
- 低延迟/内存：客户端媒体队列改为字节+条目双上限、满时丢最旧帧（覆盖重连会话）；SSRC/CNAME 按会话随机/唯一；静默失败（packetize/SRTP/录制/发布 keepalive）改为计数与日志；重复 SETUP 拒绝、PLAY 播放中允许重新 PLAY、组播轨计入上限、代理 SDP 去除上游 a=rtcp。
- 性能：UDP 收包改为精确长度拷贝（避免 64 KiB 残留缓冲）、UDP SNDBUF 1 MiB、interleaved 按帧批量写、RTP 统计按帧汇总；RTMP 重组总预算 32 MiB、HLS 段样本硬上限。
- API/文档：0.3.0 版本对齐；`MediaDescription::new`；`ServerConfig`/`MountOptions` non_exhaustive；`prelude` 补全；`PushResult::Dropped`；README 可编译示例；CI 增加 experimental 测试；矩阵计数更新。
- API 可诊断性：`ClientOptions`/`ServerConfig`/`MountOptions` 增加脱敏 Debug；`MediaDescription::new`；header 大小写不敏感访问。
- Proxy 订阅者 watcher：HLS/HTTP-FLV 等非 RTSP 订阅也能触发按需上游；删除未使用的 `TrackKey`；新增 SRTP fuzz target 与 CI protocol clippy。
- 性能：客户端 RTCP 质量统计改用 std Mutex（移除每包 await），jitter earliest deadline 缓存避免每包/每定时 O(n) 扫描。
- 生命周期：`MountRegistry::cancel_all` 在 server/gateway 关闭时取消并清空 mount；Gateway 实现 Drop 取消协议任务；mount 路径统一由 registry 校验并返回 `InvalidConfig`。
- UDP 发送端支持 RFC 4585 NACK 重传缓存（512 包，仅明文 RTP；SRTP 仍走关键帧请求），PLI/FIR/NACK 触发本地源关键帧。
- RTCP `RtcpSession::with_interval` 接入动态 bandwidth interval；mount 单帧字节上限生效（超限计 dropped，不进入缓存/广播）；新增 PlaySession/RecordSession 别名。
- 路线图/发布：SRTP 接收/发布与精确收包状态更新；crate 包排除依赖未发布 testkit 的 examples；CI semver 说明。
- NACK 缓存在 SRTP 会话同样启用：重传原始受保护字节/索引，不再限制为明文 RTP。
- VOD 自然播放结束：广播样本排空后通知连接并关闭 RTSP 会话（RTSP 无 EOS 方法）。
- UDP 发送快路径：socket 可写时直接用 `try_send_to`，仅 WouldBlock 回退异步 send，减少每包 task 唤醒；NACK 重传同样使用。
- MountRegistry 以 `MountId` 为键（`paths()` 返回 `Vec<MountId>`），路径字符串仅保留在 URI/展示边界。
- 会话类型主名称改为 `PlaySession`/`RecordSession`/`ReconnectingPlaySession`，旧名保留为类型别名。
- 客户端重连/keepalive 抖动改为进程内原子 PRNG 的独立采样，避免多个客户端按 attempt 同步重连。
- RTMP chunk stream 数上限 4096，避免仅发头部造成每连接状态膨胀；新增回归测试。
- Proxy 上游重试抖动改为每次独立随机采样，避免多个 proxy mount 同步重连。
- `MountHandle.path` 改为 `MountId`，`path()` 仍返回 `&str`；RTMP/HLS/HTTP 边界显式转换。
## [0.2.0] — 2026-08

WebRTC（WHIP/WHEP）、客户端事件与指标、传输自动回退、存储/VOD、HLS/DASH、
RTMP/SRT、示例与夹具矩阵。详见迁移指南 `docs/guides/migration/0.1-to-0.2.md`。
