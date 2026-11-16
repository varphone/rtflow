# Changelog

本文件遵循 [Keep a Changelog](https://keepachangelog.com/zh-CN/1.1.0/)，
版本号遵循 [Semantic Versioning](https://semver.org/lang/zh-CN/)。

## [Unreleased]

### Fixed

- 点播 HLS 默认生成 MPEG-TS（fMP4 不可用时回退），修复 Firefox MSE
  `bufferAppendError`；兼容 Edge/Chrome。

- HLS/VOD 转换跳过连续 Annex-B 起始码产生的空 NAL，避免 fMP4 样本写入 0 长度，
  修复 RTSP 录制点播 `bufferAppendError`/“不支持的视频格式”。

- 录制状态接口实时返回分段数与字节数（已落盘段 + 当前段），录制页面每 2 秒
  刷新，不再在录制期间始终显示 0。

- 断开直播流现在同时取消 mount 并关闭发布连接：RTMP/RTSP 会话监听 mount
  取消令牌，SRT 维护循环清理已取消 mount 的连接，不再只从列表移除。

- SRT MPEG-TS demux 按 ADTS 拆分音频帧、剥离 ADTS 头并从 ADTS 解析采样率/声道，
  修复 HLS 直播 AAC DTS 非单调与 48kHz 源被标成 44.1kHz。
- 录制从首个视频关键帧开始，HLS segmenter 在切段前提取 SPS/PPS，修复中途开始录制
  时 VOD init 的 avcC 为空、首段无法解码。
- VOD 目录在录制过程中持续刷新 `duration_seconds`，不再停留在首次扫描的短时长。
- RTMP chunk 解析器在 payload 未完整时提前提交 timestamp delta，TCP 分段会重复
  累加，导致音频时间戳/录像时长膨胀（10s 录成 14.5s）。改为整块到齐后再提交，
  并补充逐字节 feed 回归测试。

### Changed

- 命令面板接入在线流、路径与点播资源，支持直接跳转到对应筛选视图。

- 审计日志翻页浏览时自动刷新只累计“新事件”提示，避免列表跳动；点击后回到最新页。

- 登录页改为分栏品牌视图（移动端单卡），补充协议、可观测与安全说明。

- 概览趋势图支持“接收帧速率 / 订阅连接”切换，并为常量指标保留 Y 轴余量。

- 概览“需要关注”KPI 在存在异常时高亮，点播详情可直接跳到对应录制筛选。

- 点播详情抽屉展示时长/录制时间/格式/资源 ID，复制地址改为绝对 URL。

- 录制任务详情抽屉：目录、开始时间、分段、写入量、错误，并支持停止/开始与跳转点播。

- 顶栏用户菜单集中提供主题、表格密度、时间显示与退出登录，减少顶栏图标噪音。

- 直播流详情在打开期间每 3 秒刷新状态、订阅与帧/RTP 指标。

- 直播流页面支持 5 秒原地自动刷新（可关闭），保留筛选、多选与详情操作。

- 概览支持原地自动刷新（8 秒，可关闭），仅更新指标与面板，不整页重绘。

- 审计日志支持按当前筛选导出 CSV，便于离线排障与合规留档。

- 命令面板新增绝对/相对时间切换，所有表格与详情的时间显示统一生效。

- 审计日志支持事件详情抽屉，展示结构化字段与原始 JSON，并可跳转关联资源。

- 管理台支持舒适/紧凑表格密度切换，并持久化到 localStorage。

- 概览增加最近变更卡片，异常项可直接跳转到对应代理/SRT/录制筛选；活跃流可跳转直播详情。

- 路径与推流密钥页支持本地搜索、列排序；密钥新增复制 Key ID，避免输入时整页刷新。
- 路径详情抽屉展示策略/协议/地址，并可跳转到关联直播、点播与录制筛选。

- 点播页改为渐变封面卡片，增加时长/录制时间与 HLS 标识，搜索不再整页刷新。
- 点播支持多选、全选与批量刷新生成缓存。
- RTSP 代理与 SRT 页改为本地筛选/搜索，代理增加排序，SRT 切页不刷新。
- 代理详情抽屉与录制到点播的直达入口。
- 录制页增加搜索/状态筛选/排序，自动刷新保持筛选与滚动区域。

- 审计日志增加分页、自动刷新开关与固定高度表体，避免长列表把分页器推离视口。

- 直播流表格支持多选、全选与批量断开，并在筛选时保留选择。

- 直播流页面增加本地即时筛选、流详情抽屉（轨道、RTP、播放地址、操作），避免
  输入搜索时整页重绘。

- 管理台新增 `Ctrl+K` 命令面板、表单未保存确认与焦点约束；移动端顶栏/KPI/表格
  进一步适配。

- 管理控制台重构：分组侧栏、hash 路由、独立业务页面、抽屉表单、搜索/筛选、
  状态与异常聚合、只读服务配置页、深浅主题切换；前端拆分为原生 ES modules，
  无构建链依赖。

- 仓库重构为 Cargo workspace 多 crate 布局：根 `Cargo.toml` 只保留 workspace
  配置，源码迁入 `crates/<name>/src/`；`tests/`、`examples/`、`benches/` 随之
  迁入 `crates/rtflow/`，`support/` 变为 `crates/rtflow-testkit`。
- 对外 API 路径保持不变：`rtflow` 仍是唯一门面 crate，`rtflow::server`、
  `rtflow::client`、`rtflow::hls`、`rtflow::mount`、`rtflow::media` 等全部
  re-export；分层改由 Cargo 依赖图强制。
- 新增 `docs/architecture/crates.md`；分层/树形/示例/发布脚本与 CI 改为
  workspace 口径。

### Added

- 概览新增接收帧速率曲线（近 2 分钟、当前/峰值），随自动刷新更新。

- 管理台新增审计日志页（`/audit`），读取最近 300 条控制面/认证事件，支持搜索、
  变更/认证分类与结果标记。

- 业务数据改用 SQLite（`state/rtflow.db`，WAL）：路径、推流密钥、RTSP 代理、
  SRT pull/push、幂等键；TOML 服务配置保持不变，旧 JSON/TOML 业务数据首次启动
  一次性导入。秘密字段 AES-256-GCM 加密，DB/主密钥 0600。
- 管理台新增代理/SRT 列表与创建/删除/重启操作，业务 CRUD 走
  `/admin/api/v1`；代理/SRT 端点支持 revision + If-Match，SRT 单端点热更新。

- RTSP 拉流代理：`[proxy]`/`[[proxy.mounts]]` 配置、按需/常连、失败退避重试、
  per-mount `idle_grace`、SIGHUP 增删改、上游 host/CIDR/私网策略、DNS 固定、
  重定向策略、并发/数量上限、运行时持久化、缓存 SDP 快速重连、媒体空闲看门狗、
  周期描述刷新、RTCP SR 多轨 A/V 对齐。
- 代理管理 API：`GET/POST/PATCH/DELETE /admin/api/v1/proxies`、restart、probe，
  状态含 attempts/retry/last_error/bytes；代理 configured/streaming 指标与管理页
  只读表格。
- 代理传输矩阵测试：UDP/TCP/461 回退/multicast/SRTP/RTSPS/跨源 redirect，
  下游 Basic 鉴权、GStreamer/ffmpeg 互操作、10 代理自动化与 50 代理/200 路 HLS
  短时冒烟。

- SRT 新增连接统计（监听端、caller、sender），通过 `/api/v1/srt`、
  `/admin/api/v1/srt` 暴露 peer、mount、RTT、丢包/跳包、加解密与推拉流状态。
- SRT 新增 sender（推流）角色与 `SrtSender`：数据分包、重传窗口、ACK/NAK、
  ACKACK、keepalive、加密发送与密钥刷新（可配置包数间隔）；`rtflow-server` 支持 `[[srt.push]]`，将 mount
  复用为 MPEG-TS 后推送到远端 listener（指数退避重连）。
- SRT 新增 caller（拉流）角色与 `SrtCaller` API；`rtflow-server` 支持
  `[[srt.pull]]` 主动拉取 SRT 源并注册为可播放/可录制 mount（断线指数退避）。
- SRT 支持 HaiCrypt AES-CTR 加密（passphrase/pbkeylen/enforced_encryption），
  监听端解包 KMREQ、回显 KMRSP、解密数据并处理运行中密钥刷新；错误口令与
  明文对端按规范拒绝。
- SRT 协议与 libsrt 系工具（ffmpeg/OBS）互通：修正握手类型/字段、HSREQ/HSRSP、
  Stream ID（含字节序与 `#!::` 解析）、完整 ACK/ACKACK/RTT、NAK、keepalive、
  shutdown、DROPREQ；TSBPD 使用协商延迟并支持 TLPKTDROP 跳包与 fake ACK；
  被拒绝的连接返回 `1000+reason` 而非静默超时；新增 SRT RTT/丢包/跳包/
  握手失败指标；fuzz 修复并新增 SRT 目标。
- SRT 推流支持 HMAC token/流密钥校验；HTTP 内置 HTTPS；RTSP 会话结束触发
  `on_play_done` 钩子；RTMP 流密钥 `max_connections` 生效；控制 API 支持
  会话 cookie、Basic 与 Bearer token 三种认证。
- `rtflow-server` 增加审计日志（JSONL 轮转）、SIGHUP 热加载、RTSP TLS、
  路径观看配额、RTMP 异步认证钩子、应用级 Prometheus 指标，以及管理台
  “路径与密钥”控制页。
- 新增官方应用 `rtflow-server`：TOML 配置、RTSP/RTMP/SRT/WebRTC/HLS/LL-HLS/DASH/FLV、
  自动录制、VOD catalog、路径注册/发现、API 流密钥与 HTTP 钩子，以及
  systemd/Docker 部署资产。
- 统一结构化日志：新增 `rtflow::logging`（可选 `logging` feature，`full`
  默认包含）、`DEBUG_FILTER` 与 `docs/guides/logging.md`；覆盖 RTSP
  服务端/客户端、mount、HLS、HTTP、RTMP、WebRTC、admin 与会话 span。
- RTSP 发布增强：`ServerConfig::dynamic_publishers`（opt-in）允许 ANNOUNCE
  为未知路径创建 Publisher mount，断线且无订阅者时自动清理；
- UDP 发布端接收 RTCP SR 并回送 RR/SDES，支持独立端口与 `rtcp-mux`；
  发布端 SR 统计新增 `rtcp_publisher_sender_reports`；
- TCP interleaved 发布端同样回送 RR/SDES；
- SETUP 校验 transport mode 与 mount 角色；RECORD 必须先 ANNOUNCE；
- UDP 发布来源按 Peer IP 锁定、首个合法包学习源端口，容忍 NAT 端口变化；
- `live_vod_gateway` 开启动态 RTSP Publisher，ffmpeg 可直接推流。

### Fixed

- SRT TSBPD 时间基重复扣除握手时间戳，导致数据包到达即被判过期：抖动缓冲
  和 ARQ 实际失效。修正后丢包可经 NAK/重传恢复（10% 丢包下 0 跳过）。
- `auth.token_secrets` 少于 32 字节时给出明确配置错误，避免启动期
  "resource limit exceeded: Digest nonce secret"。

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
- H.265 live TS、enhanced-RTMP H.264/VP9/AV1、VOD fingerprint/HEAD/suffix Range。
- `live_vod_gateway` 新增 `--hls-segments`（默认 7）和
  `--hls-segment-seconds`（默认 1）。
- HLS live 输出 `EXT-X-PROGRAM-DATE-TIME`（UTC 毫秒，按 segmenter
  启动时间锚定）。
- RTMP 发布端连续丢帧 90 次后主动断开，避免慢订阅者拖垮 mount 队列。
- HLS live 支持 LL-HLS blocking playlist reload：`_HLS_msn` 等待目标
  media sequence，playlist 输出 `CAN-BLOCK-RELOAD=YES`（尚无 parts）。
- HLS live segment/init 支持 HEAD 与 Range（含 suffix `bytes=-N`）。
- 新增 `/llhls/{mount}/index.m3u8` LL-HLS 端点：CMAF parts、PART-INF、
  PRELOAD-HINT、200ms part target；与经典 TS HLS 并存。
- 管理台直播列表增加 “LL” 按钮，播放 LL-HLS；API `ll_hls_url`。

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

- SRT TSBPD 时间基重复扣除握手时间戳，导致数据包到达即被判过期：抖动缓冲
  和 ARQ 实际失效。修正后丢包可经 NAK/重传恢复（10% 丢包下 0 跳过）。
- `auth.token_secrets` 少于 32 字节时给出明确配置错误，避免启动期
  "resource limit exceeded: Digest nonce secret"。

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

- SRT TSBPD 时间基重复扣除握手时间戳，导致数据包到达即被判过期：抖动缓冲
  和 ARQ 实际失效。修正后丢包可经 NAK/重传恢复（10% 丢包下 0 跳过）。
- `auth.token_secrets` 少于 32 字节时给出明确配置错误，避免启动期
  "resource limit exceeded: Digest nonce secret"。

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
