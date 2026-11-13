# rtflow

面向嵌入式实时音视频网关的异步 RTSP/RTP 框架。核心数据面只传递编码后的 `bytes::Bytes`，不做解码或转码，便于直接连接硬件编码器、DMA 输出缓冲区和自定义媒体管线。

## 已实现能力

- RTSP 客户端：OPTIONS、DESCRIBE、多轨 SETUP、PLAY、PAUSE、TEARDOWN、GET_PARAMETER 自动保活。
- RTSP 服务端：本地源、外部发布源、自动 SDP 探测的按需拉流代理；会话状态和超时、连接数上限、鉴权、UDP 端口范围。
- 传输：RTP/RTCP over UDP、TCP interleaved 与 `rtcp-mux`（单端口/单 channel）；客户端默认仅在传输能力/网络失败时从 UDP 回退 TCP interleaved；服务端发布端严格校验来源地址、Payload Type 和 SSRC，并按时间调度 compound RTCP SR/SDES。
- 客户端重连：`RtspClient::connect_reconnecting` 配合 `ClientReconnectPolicy` 提供显式启用的断线退避重连；普通 `connect` 保持一次性连接和直接报错语义。
- 编码载荷：H.264 RFC 6184、H.265 RFC 7798、AAC MPEG4-GENERIC、G.711 PCMA/PCMU、Opus、VP8/VP9、MP3（MPA）与 G.726 16/24/32/40。
- 组播（`multicast` feature，需显式启用）：RTSP `RTP/AVP;multicast` 发布/接收；SDP `c=`/`m=` 组地址与端口。
- 安全传输（`srtp` feature，需显式启用）：RTSP `RTP/SAVP` + SDP `a=crypto`（AES_128_CM_HMAC_SHA1_80），RTP/SRTCP 加解密；临时/测试源由 `test-source` feature 提供。
- 文件源：MP4/MOV/M4A、Matroska/MKV、MPEG-TS/M2TS、G.711 WAV，以及裸 H.264/H.265/ADTS AAC/PCMA/PCMU；支持实时节流、起播位置、有限/无限循环、空闲暂停与有界预取。
- 实时性：有界广播队列和 Element DAG 管线、逐 Link 溢出策略、慢消费者隔离、视频丢包后等待关键帧、乱序/重复/序号回绕处理。
- 可靠性：完整 RTCP 质量统计、多上游故障转移与时间线连续、关键帧启动缓存、统一任务监督和有期限 shutdown。
- 安全：RTSPS/Rustls、IPv4/IPv6 ACL、有界连接限流、Basic/Digest、签名 nonce 和防重放。
- 录制/VOD：崩溃安全分段、容量保留和恢复索引；每客户端独立 `Range: npt=`、PAUSE/续播及 UDP/TCP 输出。
- HTTP 分发：HTTP-FLV（H.264/H.265 + AAC，含 onMetaData）、WS-FLV（RFC 6455 WebSocket 传输，mpegts.js/flv.js 浏览器低延迟播放）和基于 fMP4 的 HLS live playlist；HLS 分段在随机访问点切换，并提供独立 init segment；HTTP 连接、请求头和请求体均有界，lag 后等待关键帧/生成 discontinuity。注意：HTTP-FLV 的 H.265 使用 enhanced-RTMP 封装（codec id 12），需要 FFmpeg ≥ 6.1 或支持 enhanced-RTMP 的播放器；老版本播放器无法解码。
- 网页播放：`http_flv::web::create_web_player_handler` 提供自包含播放页面（mpegts.js + hls.js，CDN 加载），一个页面同时覆盖 WS-FLV / HTTP-FLV / HLS 三种浏览器直播协议，断流自动指数退避重连——`cargo run --example ipc_camera -- --web` 一键演示 IPC 摄像头网页实时预览。
- 商业级特性：FLV 打包零中间拷贝（Annex-B→AVCC 原地转换，单拷贝单分配，适合弱核 IPC）；WS-FLV 服务器主动心跳（30 s 间隔，~60 s 回收僵尸连接，防 fd 泄漏）；HLS live playlist ETag/304 缓存协商（hls.js 轮询期省带宽与生成开销）与 keep-alive 连接复用；HTTP/WS accept socket 显式 TCP_NODELAY；`MountSnapshot::broadcast_receivers` 让发布端功率门控覆盖网页订阅者（RTSP 会话 + HTTP-FLV/WS-FLV/HLS 任一订阅即推流）。
- 低延迟配置基线：HLS 默认 1 s 段（`HlsConfig::default`），配合编码器 GOP ≤1 s 时帧到段延迟下限约 GOP/2 = 0.5 s；IPC 示例 `ipc_camera` 默认 GOP=30（1 s @30 fps）与之一致。Lagged 恢复只等视频关键帧、音频立即恢复，断流重连不再中断声音。
- 编码支持矩阵：H.264/H.265（RTP↔FLV/WS-FLV↔HLS/DASH 全链路）、AAC（MPEG4-GENERIC）、G.711 A-law/μ-law（FLV/WS-FLV/HLS 输出，μ-law 正确标注 `ulaw`）、Opus（RTP 直通 + SDP/RTP 映射 + fmtp 注入 + HLS/DASH `dOps` sample entry；FLV 不支持，属容器限制）、VP8（RFC 7741 RTP 打包/解包，含分包重组与关键帧检测）、VP9（draft-ietf-payload-vp9 RTP 打包/解包：I/M 位 picture ID、B/E 分包重组、关键帧检测，与 WebRTC/ffmpeg 互通）；AV1 的 RTP 打包返回显式错误而非 panic（`Error::Media`）。RTP 重组与 PES 缓冲有 16 MB 硬上限，恶意/异常流无法撑爆单连接内存。
- 连接生命周期防护：HTTP 响应每写 60 s 超时（慢读/冻结客户端自动断开，`HttpConfig::response_timeout`）、RTMP 播放写路径 30 s 超时、WS-FLV 心跳回收——无界连接槽与僵尸写任务不会累积。帧指标 `frames_accepted/dropped` 对 RTSP/RTMP/SRT/gateway 全注册路径汇入全局 `/metrics`。
- 错误分类：`Error` 按域区分（`Rtsp`/`Media`/`Http`/`InvalidConfig`/`Unsupported`/…），`code()`/`retry_class()`/`rtsp_status()` 稳定映射（Http/InvalidConfig→500，Unsupported→501），供重试策略与遥测使用。

## 分发能力边界

HTTP-FLV、WS-FLV 与 HLS 是当前优先维护的 RTSP 输出协议。WS-FLV 走独立 TCP 端口（RFC 6455 服务端握手，路径与 HTTP-FLV 一致：mount `/live/stream` ↔ `/live/stream.flv`），延迟最低，配合 mpegts.js/flv.js 在浏览器播放；HLS 仍是兼容性基线：实际分段长度取决于下一个视频随机访问点，因此要达到 1–3 秒端到端延迟，编码器 GOP/关键帧间隔必须不大于目标分段时长。

RTSP 扩展的当前边界：`srtp` 仅支持 UDP unicast（`RTP/SAVP/TCP` 返回 461），
客户端 SRTP 会话暂不发送 SRTCP RR，SDP `inline:` 主密钥应配合 RTSPS 使用；
`multicast` 仅支持 IPv4，服务端每 mount 复用单个 RTP 端口按 PT 分流，客户端
组播接收不发送 RTCP；RTSP 2.0、AV1/JPEG/H.263/MPEG-4 Part 2 不在支持范围。

`dash`、`rtmp`、`srt`、`webrtc` feature 目前仅保留模块占位，不能视为可用协议实现；它们只通过 `experimental-protocols` 聚合 feature 开启，生产部署不应启用或对外承诺这些能力。其中 `webrtc`（WHIP/WHEP）的 ICE/DTLS/SRTP 媒体管线未实现：信令握手可完成，但浏览器协商后不会有媒体流——**WHIP/WHEP 对外声明为不可用占位**，不要用 `Location` 响应头引导客户端接入。嵌入式 Linux/Yocto 网关建议使用 `embedded-gateway` profile；默认 `full` 为兼容现有 0.2 使用方式，后续主版本再迁移默认 profile。

## 最小服务端示例

```rust,no_run
use bytes::Bytes;
use rtflow::{
    AccessUnit, Codec, MediaDescription, MediaKind, MountConfig, RtspServer,
    ServerConfig, TrackDescription,
};
use std::{sync::Arc, time::Duration};

#[tokio::main]
async fn main() -> rtflow::Result<()> {
    let server = RtspServer::bind(ServerConfig::new("0.0.0.0:8554".parse().unwrap())).await?;
    let mount = server.register_mount(
        Arc::from("/camera"),
        MountConfig::Local {
            description: MediaDescription {
                session_name: Arc::from("camera"),
                tracks: vec![TrackDescription {
                    id: 0,
                    kind: MediaKind::Video,
                    codec: Codec::H264,
                    clock_rate: 90_000,
                    channels: None,
                    payload_type: 96,
                    fmtp: None,
                    control: Arc::from("trackID=0"),
                }],
            },
        },
    ).await?;
    let publisher = mount.publisher()?;
    let _ = publisher.try_push(AccessUnit {
        track_id: 0,
        codec: Codec::H264,
        pts: Duration::ZERO,
        dts: None,
        rtp_timestamp: 0,
        is_keyframe: true,
        data: Bytes::from_static(&[0x65, 0x00]),
    })?;
    server.shutdown().await
}
```

完整 CLI 覆盖本地源、文件源、播放客户端、按需代理、外部发布、鉴权和 Element Pipeline。命令与联调组合见 `docs/guides/examples.md`；文件格式矩阵和 VOD 边界见 `docs/guides/file-streaming.md`，模块扩展约定见 `docs/architecture/overview.md`。

## 动态实时串流

无需准备媒体文件即可运行完整的 `FFmpeg → Rtflow → GStreamer` 链路：

```text
cargo run --example live_stream_demo
cargo run --example live_stream_demo -- --headless --duration 10
```

也可以用 `ffmpeg_live_publish` 和 `gstreamer_live_play` 分别连接已有 RTSP 服务。详细参数、依赖和嵌入式 headless 用法见 `docs/guides/examples.md`。

## 文件服务器

```rust,no_run
use rtflow::{FileSource, FileSourceOptions, LoopMode, MountConfig, RtspServer, ServerConfig};
use std::sync::Arc;

# async fn serve() -> rtflow::Result<()> {
let server = RtspServer::bind(ServerConfig::new("0.0.0.0:8554".parse().unwrap())).await?;
let source = FileSource::open_async("movie.mp4", FileSourceOptions {
    loop_mode: LoopMode::Forever,
    ..Default::default()
}).await?;
let mount = server.register_mount(
    Arc::from("/movie"),
    MountConfig::Local { description: source.description().clone() },
).await?;
source.play(mount.publisher()?).wait().await?;
server.shutdown().await
# }
```

普通文件 mount 是多客户端共享时间线；录制目录可通过 `RecordingVodSource` 注册为 `MountConfig::Vod`，为每个 RTSP session 提供独立 seek 和播放游标。

## 嵌入式接入（IPC / 海思 HiMPP）

把硬件编码器（HiMPP VENC / V4L2）输出的 H.264/H.265/AAC/G.711 帧直接
推入 RTSP 的最小路径见 `examples/embedded/ipc_camera.rs`；帧构造/时间戳/SPS/PPS
自动注入等细节见 `docs/guides/embedded.md`。浏览器实时预览（网页版 IPC
观看）无需额外代码：`cargo run --example ipc_camera -- --web` 后打开
`http://<ip>:8080/`，页面自动列出全部通道并以 WS-FLV / HTTP-FLV /
HLS 三种模式播放（`--no-web` 可只留 RTSP）。

## WebRTC（WHIP/WHEP）

基于 `webrtc` crate（0.20，纯 Rust ICE/DTLS/SRTP）的浏览器原生实时
协议，`webrtc` feature 启用：

- **WHEP 播放**（draft-ietf-wish-whep）：浏览器 `POST` SDP offer 到
  `/whep{path}`，服务器应答（含完整 ICE 候选）后把 mount 的 H.264/
  H.265/VP8/VP9 视频与 Opus/G.711/AAC-LC 音频打包为 RTP 推给浏览器
  ——`cargo run --example ipc_camera -- --whep-listen 0.0.0.0:8090` 后任意 WebRTC
  播放器（或原生 `<video>` + RTCPeerConnection）拉流。
- **WHIP 发布**（draft-ietf-wish-whip）：浏览器 `POST` offer 到
  `/whip{path}` 后推 H.264/Opus RTP，服务器解包并经公共
  depacketizer 进入 mount（与 RTSP 发布同路径）——
  `--whip-listen 0.0.0.0:8089` 即可让浏览器把摄像头推入
  `/cam` 等挂载点。
- 音频：G.711 PCMA/PCMU 是 RFC 7874 强制 codec（所有浏览器支持，
  直通打包，IPC 摄像头 G.711 音频可原样播放）；AAC-LC
  （`audio/mp4a.40.2`，MPEG4-GENERIC + AU-header）为 Chrome/Safari
  事实支持（Firefox 不支持），同样直通无转码；跨 NAT 需在
  `WebRtcConfig` 配置 STUN/TURN。

## Feature profile

嵌入式网关可使用 `--no-default-features --features embedded-gateway`，只启用 RTSP client/server、TLS、metrics、HTTP-FLV 和 HLS 等生产模块。`full` 保留已实现的完整能力；实验协议必须显式启用 `experimental-protocols`。

## 验证

```text
cargo test --all-targets
cargo clippy --all-targets -- -D warnings
cargo build --examples
./scripts/release-check.sh
```

默认测试使用仓库内的确定性媒体夹具，不依赖 FFmpeg/GStreamer。真实互操作测试显式标记为 ignored，由 CI 的媒体任务强制运行：

```text
cargo test --test interop -- --ignored --nocapture gstreamer_interop
cargo test --test interop -- --ignored --nocapture gstreamer_codec_matrix
cargo test --test interop -- --ignored --nocapture ffmpeg_publish_interop
cargo test --test interop -- --ignored --nocapture live_tool_stream
cargo test --test storage -- --ignored --nocapture file_stream
```
