# rtsp-rust

面向嵌入式实时音视频网关的异步 RTSP/RTP 框架。核心数据面只传递编码后的 `bytes::Bytes`，不做解码或转码，便于直接连接硬件编码器、DMA 输出缓冲区和自定义媒体管线。

## 已实现能力

- RTSP 客户端：OPTIONS、DESCRIBE、多轨 SETUP、PLAY、PAUSE、TEARDOWN、GET_PARAMETER 自动保活。
- RTSP 服务端：本地源、外部发布源、自动 SDP 探测的按需拉流代理；会话状态和超时、连接数上限、鉴权、UDP 端口范围。
- 传输：RTP/RTCP over UDP，以及 RTSP TCP interleaved；客户端和服务端均支持多轨播放，服务端支持 TCP/UDP RECORD 接收。
- 编码载荷：H.264 RFC 6184、H.265 RFC 7798、AAC MPEG4-GENERIC、G.711 PCMA/PCMU。
- 文件源：MP4/MOV/M4A、Matroska/MKV、MPEG-TS/M2TS、G.711 WAV，以及裸 H.264/H.265/ADTS AAC/PCMA/PCMU；支持实时节流、起播位置、有限/无限循环、空闲暂停与有界预取。
- 实时性：有界广播队列和 Element DAG 管线、逐 Link 溢出策略、慢消费者隔离、视频丢包后等待关键帧、乱序/重复/序号回绕处理。
- 可靠性：UDP/TCP RTCP SR/RR、代理指数退避与抖动、无订阅者自动停拉、客户端/mount/服务端分层取消及可等待 shutdown。
- 鉴权：服务端可插拔 `Authenticator`；客户端支持 Basic 及 Digest MD5/qop=auth。

## 最小服务端示例

```rust,no_run
use bytes::Bytes;
use rtsp::{
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

无需准备媒体文件即可运行完整的 `FFmpeg → Rust RTSP → GStreamer` 链路：

```text
cargo run --example live_stream_demo
cargo run --example live_stream_demo -- --headless --duration 10
```

也可以用 `ffmpeg_live_publish` 和 `gstreamer_live_play` 分别连接已有 RTSP 服务。详细参数、依赖和嵌入式 headless 用法见 `docs/examples.md`。

## 文件服务器

```rust,no_run
use rtsp::{FileSource, FileSourceOptions, LoopMode, MountConfig, RtspServer, ServerConfig};
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

文件 mount 是多客户端共享同一播放时间线的广播源。第一位订阅者到来时启动，默认在没有订阅者时暂停；它不是每个 RTSP session 各自 seek 的完整 VOD 服务。

## 验证

```text
cargo test --all-targets
cargo clippy --all-targets -- -D warnings
cargo build --examples
```

默认测试使用仓库内的确定性媒体夹具，不依赖 FFmpeg/GStreamer。真实互操作测试显式标记为 ignored，由 CI 的媒体任务强制运行：

```text
cargo test --test gstreamer_interop -- --ignored --nocapture
cargo test --test gstreamer_codec_matrix -- --ignored --nocapture
cargo test --test ffmpeg_publish_interop -- --ignored --nocapture
cargo test --test live_tool_stream -- --ignored --nocapture
cargo test --test file_stream -- --ignored --nocapture
```
