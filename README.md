# rtspflow

面向嵌入式实时音视频网关的异步 RTSP/RTP 框架。核心数据面只传递编码后的 `bytes::Bytes`，不做解码或转码，便于直接连接硬件编码器、DMA 输出缓冲区和自定义媒体管线。

## 已实现能力

- RTSP 客户端：OPTIONS、DESCRIBE、多轨 SETUP、PLAY、PAUSE、TEARDOWN、GET_PARAMETER 自动保活。
- RTSP 服务端：本地源、外部发布源、自动 SDP 探测的按需拉流代理；会话状态和超时、连接数上限、鉴权、UDP 端口范围。
- 传输：RTP/RTCP over UDP，以及 RTSP TCP interleaved；客户端默认仅在传输能力/网络失败时从 UDP 回退 TCP interleaved；服务端发布端严格校验来源地址、Payload Type 和 SSRC，并按时间调度 compound RTCP SR/SDES。
- 客户端重连：`RtspClient::connect_reconnecting` 配合 `ClientReconnectPolicy` 提供显式启用的断线退避重连；普通 `connect` 保持一次性连接和直接报错语义。
- 编码载荷：H.264 RFC 6184、H.265 RFC 7798、AAC MPEG4-GENERIC、G.711 PCMA/PCMU。
- 组播（`multicast` feature，默认启用）：向 `udp://` 地址发布 SRT/UDP 组播流。
- 文件源：MP4/MOV/M4A、Matroska/MKV、MPEG-TS/M2TS、G.711 WAV，以及裸 H.264/H.265/ADTS AAC/PCMA/PCMU；支持实时节流、起播位置、有限/无限循环、空闲暂停与有界预取。
- 实时性：有界广播队列和 Element DAG 管线、逐 Link 溢出策略、慢消费者隔离、视频丢包后等待关键帧、乱序/重复/序号回绕处理。
- 可靠性：完整 RTCP 质量统计、多上游故障转移与时间线连续、关键帧启动缓存、统一任务监督和有期限 shutdown。
- 安全：RTSPS/Rustls、IPv4/IPv6 ACL、有界连接限流、Basic/Digest、签名 nonce 和防重放。
- 录制/VOD：崩溃安全分段、容量保留和恢复索引；每客户端独立 `Range: npt=`、PAUSE/续播及 UDP/TCP 输出。
- HTTP 分发：HTTP-FLV（H.264/H.265 + AAC，含 onMetaData）和基于 fMP4 的 HLS live playlist；HLS 分段在随机访问点切换，并提供独立 init segment；HTTP 连接、请求头和请求体均有界，lag 后等待关键帧/生成 discontinuity。注意：HTTP-FLV 的 H.265 使用 enhanced-RTMP 封装（codec id 12），需要 FFmpeg ≥ 6.1 或支持 enhanced-RTMP 的播放器；老版本播放器无法解码。

## 分发能力边界

HTTP-FLV 与 HLS 是当前优先维护的 RTSP 输出协议。HLS 仍是 live 基线：实际分段长度取决于下一个视频随机访问点，因此要达到 1–3 秒端到端延迟，编码器 GOP/关键帧间隔必须不大于目标分段时长。

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
async fn main() -> rtsp::Result<()> {
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

完整 CLI 覆盖本地源、文件源、播放客户端、按需代理、外部发布、鉴权和 Element Pipeline。命令与联调组合见 `docs/examples.md`；文件格式矩阵和 VOD 边界见 `docs/file-streaming.md`，模块扩展约定见 `docs/architecture.md`。

## 动态实时串流

无需准备媒体文件即可运行完整的 `FFmpeg → Rtflow → GStreamer` 链路：

```text
cargo run --example live_stream_demo
cargo run --example live_stream_demo -- --headless --duration 10
```

也可以用 `ffmpeg_live_publish` 和 `gstreamer_live_play` 分别连接已有 RTSP 服务。详细参数、依赖和嵌入式 headless 用法见 `docs/examples.md`。

## 文件服务器

```rust,no_run
use rtflow::{FileSource, FileSourceOptions, LoopMode, MountConfig, RtspServer, ServerConfig};
use std::sync::Arc;

# async fn serve() -> rtsp::Result<()> {
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

## Feature profile

嵌入式网关可使用 `--no-default-features --features embedded-gateway`，只启用 RTSP client/server、TLS、metrics、HTTP-FLV 和 HLS 等生产模块。`full` 保留已实现的完整能力；实验协议必须显式启用 `experimental-protocols`。

## 验证

```text
cargo test --all-targets
cargo clippy --all-targets -- -D warnings
cargo build --examples
./tools/release-check.sh
```

默认测试使用仓库内的确定性媒体夹具，不依赖 FFmpeg/GStreamer。真实互操作测试显式标记为 ignored，由 CI 的媒体任务强制运行：

```text
cargo test --test gstreamer_interop -- --ignored --nocapture
cargo test --test gstreamer_codec_matrix -- --ignored --nocapture
cargo test --test ffmpeg_publish_interop -- --ignored --nocapture
cargo test --test live_tool_stream -- --ignored --nocapture
cargo test --test file_stream -- --ignored --nocapture
```
