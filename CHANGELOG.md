# Changelog

本文件遵循 [Keep a Changelog](https://keepachangelog.com/zh-CN/1.1.0/)，
版本号遵循 [Semantic Versioning](https://semver.org/lang/zh-CN/)。

## [Unreleased]

### Fixed

- HLS 直播改为 MPEG-TS（`transmux::StreamingTsHlsSegmenter`）：按关键帧
  切段、每段自带 PAT/PMT、跨段 continuity counter，Firefox/Edge 的
  hls.js 均可持续播放；VOD/静态 HLS 仍使用 fMP4。
- HLS live 不再自动启动 segmenter；由首个 playlist 请求启动，修复长推流
  只产出首段、播放器缓冲停在 1.88s 的问题。
- TS 分段为每个 sample 写入稳定 duration：此前 duration 全为 0，所有 PES
  使用同一 PTS/DTS，播放器只显示首帧（1 fps）；现在视频按码流帧率
  （默认 30 fps）、AAC 按 1024 sample 计算 duration，不再依赖 RTMP 端
  被放大的时间戳。
- 管理台 HLS 播放移除会重置 currentTime 的 seek/nudge，交给 hls.js 正常
  live sync；RTMP fmt3 用显式 in_message 标记，避免续包重复累计时间戳。
- 参照 SRS/MediaMTX 审核 live/VOD：补 TS discontinuity sequence、清理
  VOD 临时目录、拒绝 `.`/`..` VOD id、处理 RTMP fmt3 extended timestamp；
  详细对照见 `docs/audits/2026-11-live-vod-srs-mediamtx.md`。

### Added

- RTMP：`onMetaData` 宽高/帧率进入动态 `MediaDescription`，AVC/HEVC/AAC
  sequence header 自动建立 track；`RtmpSessionConfig::token` 支持 OBS
  stream key `name?token=...`（HMAC，日志脱敏）。
- HLS：注册表驱动的 `/hls/{mount}/index.m3u8` 直播端点，动态 RTMP mount
  无需预注册 handler；`/vod/{id}` 从录制目录或 MP4/MKV/TS 文件生成磁盘
  缓存的 VOD playlist/segment。
- 管理前端：独立 `admin` feature（`rtflow::admin::{Admin,AdminConfig}`，
  自包含登录页/仪表盘/直播管理/点播管理/系统状态；HttpOnly 会话 Cookie、
  Argon2id 口令、CSRF 校验、写操作只读开关）。默认 `full` 不编译该模块。
- `FileVodSource`；`live_vod_gateway` 示例（RTMP + HLS + admin 控制台）。

### Fixed

- RTMP 视频帧统一转为 Annex-B 后进入数据面，HLS/RTSP 可正确消费；
  RTMP 拉流输出时再转回 AVCC。修复 HLS 直播出现
  `missing picture` / `data partitioning` / hls.js `internalException`。
- HLS 新 segmenter 首帧来自 startup cache 时不再被去重逻辑跳过，
  SPS/PPS + 首个 IDR 能进入 init/segment。
- HLS 视频不再在 2×target duration 处硬切；没有 IDR 时等待下一个
  关键帧，避免 segment 从 P 帧开始导致 `Missing reference picture`。
- RTMP sequence header 生成 H.264 `fmtp`（sprop-parameter-sets），HLS
  init segment 立即携带 avcC；修复 Edge 中 `resetInitSegment` 抛
  `Cannot read properties of undefined (reading 'toString')`。
- HLS init/segment 改为 `no-store`，live playlist 内嵌 URI 带每次
  启动唯一的 `?v=` 令牌，避免浏览器磁盘缓存复用旧 init。
- HLS segmenter 在首个视频 IDR 前丢弃音频，segment 不再从音频 PTS
  开始；60 fps 短分段起播不再等待/卡顿。示例 live 窗口增至 30 段，
  hls.js 使用 3 个目标时长同步和 30 秒缓冲，给多标签页/后台节流留出
  足够回看窗口。
- HLS live 每 mount 严格单 segmenter 所有权；worker 退出只清自己的
  ring，避免两个 sequence 计数器交叉写入产生 114/115/116/115。
- HLS ring 保留 3× playlist 窗口（默认 30 段 playlist / 90 段历史），
  多标签页或后台节流导致晚到的 segment 请求仍能命中，不再反复 404
  重试同一序号。
- 管理台 hls.js 升级到 1.6.15；Firefox 在 MSE 首帧到达前 play() 失败
  时于 MANIFEST_PARSED/LEVEL_LOADED/BUFFER_APPENDED 重试，并 seek 到
  首个 buffered 区间，避免长时间停在 0 秒。
- 回退“发布后主动启动 segmenter”的实验：它会在长推流时只产出一个
  segment 并让播放器缓冲停在首段。HLS live 改为首个 playlist 请求时
  启动，确保分段持续增长。
- segment sequence 跨 segmenter 重启保持单调；SegmentRing 拒绝
  非递增序列，playlist 不会再回退。
- HLS live playlist 使用相对 URI，并在首个 segment 就绪前最多等待
  3 秒；harness/hls.js 不再因空 playlist 判为 fatal。
- 管理台播放器在 Chromium/Edge 上优先使用 hls.js，而不是信任
  `canPlayType("application/vnd.apple.mpegurl")` 的原生 HLS 分支；
  Firefox 正常而 Edge 黑屏的问题由此修复。

### Changed

- `Mount` 的描述可动态更新（sequence headers/metadata）；`MountHandle::description()`
  返回当前快照，旧的 `description` 字段保留为初始快照。
- HLS VOD playlist 的 segment URI 与 VOD 目录一致，`EXT-X-ENDLIST` 语义
  由持久化段保证；不再用滑动窗口冒充 VOD。

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
- `MountRegistry::snapshots` 返回 `MountId`，metrics 只在展示边界转字符串。
- `ServerConfig::rtcp_report_interval` 可配置（默认 5s），UDP/interleaved/组播发送端统一使用；0 值在 validate 拒绝。
- interleaved 发送端加入 512 包 NACK 缓存；RTCP NACK 经 mount 反馈通道重传（含 SRTP），不再只请求关键帧。
- UDP/interleaved 共用 `RetransmitCache`，消除两份 512 包缓存实现；测试同步。
- `MountRegistry::paths()`/`snapshots()` 按路径稳定排序，metrics/挂载列表输出可复现。
- 新增 interleaved NACK 转发单测（PID+BLP 展开）。
- `RtspServer::bind_with` 为 Gateway 预注册的 proxy mount 启动订阅者 watcher，HLS/HTTP-FLV 在 server 绑定前注册也能按需拉流。
- 新增 RtcpSession 动态 interval 回归测试（首次固定间隔后按 RtcpInterval 计算）。
- `ClientOptions::rtcp_report_interval`（默认 5s）统一 interleaved/组播 RR tick 与 UDP 质量报告会话。
- 新增 ClientOptions::rtcp_report_interval 零值校验测试。
- ServerConfigBuilder 增加 rtcp_report_interval，保持 builder 与新增字段一致。
- `MountOptions::builder()` 提供与 ClientOptions/ServerConfig 一致的构造方式，并覆盖新增单测。
- `Gateway` Drop 取消 HTTP server，避免 drop 后 accept loop 继续持有端口。
- RTCP `nack_sequences()` 统一 PID+BLP 展开，UDP/interleaved 共用并补充单测。
- RTP 重传缓存增加 2 MiB 字节预算，避免大帧 × 512 包导致每 sender 内存膨胀。
- Gateway::http_addr 与 Drop 后 HTTP accept loop 停止的回归测试。
- RtmpServer/SrtServer 实现 Drop cancel，未进入 run 即被丢弃时不会残留 accept 任务。
- NACK 重传发送路径抽出 `retransmit_cached` 并用真实 UDP socket 回归测试。
- HLS/DASH/WebRTC 增加命名 handler struct（HlsHandlers/DashHandlers/WebRtcHandlers）与 create_*_router，避免位置元组。
- 新增 registry 路径边界、稳定排序与 cancel_all 单测。
- HLS SegmentRing 窗口硬上限 128，错误配置不会无限保留 segment/keyframe；补充 clamp 单测。
- HlsConfig/DashConfig/MpdConfig 增加 validate（0 段时长/timescale 拒绝）与单测。
- ServerConfigBuilder::rtcp_report_interval 的 0 值拒绝/正常值单测。
- 新增 LocalPublisher 超大帧直接 Dropped 且不进入广播的回归测试。
- prelude 补齐客户端：PlaySession/RecordSession/ReconnectingPlaySession、ClientQualitySnapshot、UpstreamTransport。
- `RtspClient` 入口统一接受 `IntoRtspUrl`（Url/&str/&String/String），删除 `options_str`/`describe_str`/`connect_str`/`publish_str` 重复 API。
- 新增 raw RTSP + 真实 UDP socket 的 NACK 端到端测试，验证重传缓存生效。
- broadcast ring 容量 clamp 到 4096，并在 MountSnapshot 暴露生效值；避免错误配置分配无界队列。
- 全 feature 测试 435 passed；默认 368、experimental 435。
- 新增 FrameBus 基础实现：帧数+字节双预算、Lagged/Closed、可等待接收；单测覆盖淘汰、唤醒与超大帧拒绝（尚未接入 Mount）。
- Mount 帧通道从 broadcast 切换为 FrameBus：帧数+字节双预算生效，VOD 输出、RTSP/HLS/FLV/RTMP/WebRTC 消费端适配；Mount Drop 关闭 receiver。
- Startup cache 与 FrameBus 共享 Arc<AccessUnit>：发布只分配一次，cache/broadcast/startup snapshot 复用；HLS/DASH 仅在需要修改时间戳时 clone。
- UDP 媒体帧与 NACK 重传使用 Linux `sendmmsg` 批量发送（nix 0.31，target linux）；非 Linux/部分发送/错误自动回退逐包。FrameBus/Startup cache 共享 Arc<AccessUnit>。
- RTX（RFC 4588）服务端：`ServerConfig.rtx` 开启后 SDP 广告独立 PT/SSRC，NACK 以 RTX 包（OSN+原 payload）重传；SRTP/非 Linux 保持原包重传。客户端 RTX 解包仍未实现。
- 客户端 RTX：SDP 解析保存 `TrackDescription.rtx`（PT/apt），UDP/interleaved/组播收到 RTX 包后按 OSN 还原原 sequence/PT/payload；新增解析与解包单测。
- 全 feature 445 passed；RTX 客户端/服务端闭环；all-features clippy 干净。
- SRTP 轨不再广告 RTX（无独立 SRTP context），NACK 保持加密原包重传；RTMP 播放 lag 后等待下一个视频关键帧；FrameBus 增加订阅起点/Lagged 计数/close 排空/receiver_count 单测。
- WHEP 消费端把 FrameBus `Lagged` 从“关闭流”改为等待下一个关键帧；RTMP/WHEP 的 lag 处理与 HLS/DASH/FLV 一致。
- SRTP RTX 独立流：RTX 包使用同一 SrtpContext 按 RTX SSRC 加密/解密（SRTP IV 含 SSRC），SDP SAVP 保留 `rtx`/`apt`；组播客户端为 RTX PT 建立独立接收 context。
## [0.2.0] — 2026-08

WebRTC（WHIP/WHEP）、客户端事件与指标、传输自动回退、存储/VOD、HLS/DASH、
RTMP/SRT、示例与夹具矩阵。详见迁移指南 `docs/guides/migration/0.1-to-0.2.md`。
