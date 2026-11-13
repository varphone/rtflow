# Changelog

本文件遵循 [Keep a Changelog](https://keepachangelog.com/zh-CN/1.1.0/)，
版本号遵循 [Semantic Versioning](https://semver.org/lang/zh-CN/)。

## [Unreleased] — 0.3.0（布局基线）

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
## [0.2.0] — 2026-08

WebRTC（WHIP/WHEP）、客户端事件与指标、传输自动回退、存储/VOD、HLS/DASH、
RTMP/SRT、示例与夹具矩阵。详见迁移指南 `docs/guides/migration/0.1-to-0.2.md`。
