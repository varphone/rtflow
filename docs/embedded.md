# 嵌入式集成指南（IPC / 海思 HiMPP）

本文面向把 rtflow 接入嵌入式平台的客户，重点是**把硬件编码器（HiSilicon
HiMPP VENC、V4L2 等）输出的帧通过 RTSP 发出去**，实现多码流 IPC。
`examples/ipc_camera.rs` 是完整的参考实现——把其中的合成帧生成替换为
VENC 回调即可。

## 1. 接入路径总览

```text
VENC 回调（每帧）                     rtflow
─────────────────────                ──────────────────────
编码帧（Annex-B H.264/H.265）  →  AccessUnit::video(...)   → try_push
帧类型（IDR / 非 IDR）        →  is_keyframe
时间戳（µs）                  →  RtpClock.timestamp(...)
SPS/PPS（分离输出）           →  首个关键帧自动提取 → SDP fmtp
                                        ↓
                          RTSP 信令 / RTP / 多客户端 / HLS / HTTP-FLV
```

支持的编码：**H.264 / H.265（视频）、AAC（MPEG4-GENERIC）、G.711
A-law / μ-law（PCMA / PCMU）**。RTP 打包、SDP（含 `sprop-parameter-sets`
等 fmtp）、RTCP、多客户端分发全部由框架完成，接入代码只做"帧搬运"。

## 2. 最小接入代码

```rust
use rtflow::media::bitstream::{h264_parameter_sets, is_idr};
use rtflow::media::clock::RtpClock;
use rtflow::media::{AccessUnit, Codec, MediaDescription, TrackDescription};
use rtflow::server::ServerConfig;
use rtflow::{MountConfig, RtspServer};

#[tokio::main]
async fn main() -> rtflow::Result<()> {
    let server = RtspServer::bind(
        ServerConfig::builder("0.0.0.0:8554".parse().unwrap()).build()?,
    )
    .await?;

    // 描述自动生成：只给分辨率/帧率；SPS/PPS 会从首帧自动注入 fmtp
    let track = TrackDescription::h264_video(0, &[], &[], 1920, 1080, 30);
    let mount = server
        .register_mount(
            "/cam/main",
            MountConfig::Publisher {
                description: MediaDescription::from_tracks("ipc", vec![track]),
                allow_replace_after_disconnect: true, // 断线重推不重建
            },
        )
        .await?;
    let publisher = mount.publisher()?;

    let mut clock = RtpClock::new(90_000); // RTP 时钟 90kHz
    // ... VENC 回调中：
    let pts = /* venc 时间戳（µs） */;
    let data = /* venc 编码帧（Annex-B，含 start code） */;
    let idr = is_idr(Codec::H264, &data); // 或直接用 venc 帧类型
    publisher.try_push(AccessUnit::video(
        0, Codec::H264,
        std::time::Duration::from_micros(pts),
        clock.timestamp(std::time::Duration::from_micros(pts)),
        idr,
        data.into(),
    ))?;
    Ok(())
}
```

要点：

- `data` 直接收 **Annex-B**（`00 00 00 01`/`00 00 01` 起始码），VENC 原始
  输出格式，无需转换。
- **SPS/PPS 分离输出**（HiMPP 常见配置）：把 SPS/PPS 打进**第一个 IDR
  帧**（NAL 7/8 或 H.265 33/34/32），框架自动提取并生成
  `a=fmtp:...sprop-parameter-sets=...`（见 `docs/bitstream 模块`）。
  之后 IDR 可不再携带参数集。
- 无订阅者时 `try_push` 返回 `DroppedNoSubscribers`（正常）：IPC 固件
  通常借此**门控 VENC**（无人观看时停编码省电），参考
  `examples/ipc_camera.rs` 的 `subscriber_count == 0` 检查。

## 3. 多码流（主/子码流）

IPC 惯例：**每个码流独立 URL**，客户端按需拉取。

```text
/cam/main  → 1080p 主码流
/cam/sub   → 720p  子码流（音频可只挂主码流）
```

每个码流独立 `register_mount` + `publisher`。也可以单 URL 多轨（视频+音频
同描述），此时客户端一次 SETUP 全部轨道。

## 4. 音频（AAC / G.711）

```rust
let aac = TrackDescription::aac_audio(1, 48_000, 2);   // AAC-LC 48k 双声道
let pcma = TrackDescription::pcma_audio(1, 8_000, 1);  // G.711 A-law
let pcmu = TrackDescription::pcmu_audio(1, 8_000, 1);  // G.711 μ-law
```

- AAC：`data` 为**裸 AAC 帧**（无 ADTS 头）；fmtp `config=` 由
  `aac_audio` 自动生成（RFC 3640），`sizelength=13;indexlength=3`。
- G.711：`data` 为 8kHz 采样块（如 20ms = 160 字节），RTP 时间戳按
  采样率走（`RtpClock::new(8000)`）。
- 音频时间戳可用独立的 `RtpClock`（如 48kHz）。

## 5. 交叉编译

目标三元组（海思平台常见）：

```bash
# aarch64（海思 Hi3559/Hi3516 等）
rustup target add aarch64-unknown-linux-gnu

# ~/.cargo/config.toml 或项目 .cargo/config.toml
# [target.aarch64-unknown-linux-gnu]
# linker = "aarch64-linux-gnu-gcc"   # 或海思 SDK 的交叉 gcc

cargo build --release --target aarch64-unknown-linux-gnu \
    --no-default-features --features embedded-gateway
```

- **musl vs glibc**：板子用 glibc（海思 SDK 通常如此）→
  `aarch64-unknown-linux-gnu` + SDK 的交叉 gcc；静态/极简部署 →
  `aarch64-unknown-linux-musl`（需 musl 交叉工具链）。
- `embedded-gateway` profile 已含：`client`、`server`、`tls`、`metrics`、
  `http`、`http-flv`、`hls`。IPC 只需 RTSP 时可再裁剪。
- 二进制 footprint 参考（release）：核心 RTSP 路径约 1-2 MB 量级
  （tokio + rustls 为主）；关闭 `tls`/`metrics` 可进一步减小。

## 6. 调参速查

| 参数 | 作用 | IPC 建议 |
|---|---|---|
| `ServerConfig::consumer_queue_capacity` | 广播队列容量（帧） | 默认 256 足够；内存紧张可降到 128 |
| `STARTUP_WINDOW`（`src/mount.rs`） | 新客户端起播回放窗口 | 500ms 默认；GOP 大于窗口时等下一个 IDR |
| `ServerConfig::session_timeout` | RTSP 会话超时 | 设备侧建议 86400s（IPC 长连接） |
| GOP / 关键帧间隔 | 起播延迟上限 | 建议 1s（如 30fps 下 `-g 30`）；越大起播越慢 |
| B 帧 | 解码缓冲延迟 | 建议关闭（VENC 开 low-delay / zerolatency） |

## 7. 部署检查清单

1. `cargo build --target aarch64-... --features embedded-gateway` 通过
2. 板子上跑 `examples/ipc_camera`（或你的接入代码），`RUST_LOG=rtflow=info`
3. PC 上 `ffprobe -v trace rtsp://<ip>:8554/cam/main`：能看到
   `a=fmtp:96 profile-level-id=...;sprop-parameter-sets=...` 且能出帧
4. VLC 播放主/子码流均正常；断网重连后设备自动恢复推流
   （`allow_replace_after_disconnect=true`）
