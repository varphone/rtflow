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
  `http`、`http-flv`、`hls`。IPC 只需 RTSP 时可再裁剪（如
  `--no-default-features --features client,server`）。
- 二进制 footprint 实测（本仓库 HEAD，release + strip，见
  `docs/embedded-resource-audit.md`）：

  | 特性组合 | ROM |
  |---|---|
  | `embedded-gateway`（默认 release profile） | **2.2 MB** |
  | `embedded-gateway`（lto+abort+strip 前） | 4.8 MB |
  | 全特性（含 webrtc/srt/dash/rtmp/mkv） | ≈ 7 MB |

  release profile 已固化 `lto`/`panic=abort`/`strip=symbols`/
  `opt-level=s`（见 Cargo.toml `[profile.release]`）；`tracing` 已开
  `release_max_level_info`（release 下 info 级封顶，debug 构建不受影响）。
  CPU 吃紧时把 `opt-level` 改回 `3` 再对比。

## 6. 调参速查

| 参数 | 作用 | IPC 建议 |
|---|---|---|
| `ServerConfig::consumer_queue_capacity` | 每会话广播队列容量（帧） | **按码率预算**（见下方公式）：1080p@4Mbps 主码流 128（≈2MB），子码流 64 |
| `STARTUP_WINDOW`（`src/mount.rs`） | 新客户端起播回放窗口 | 500ms 默认；GOP 大于窗口时等下一个 IDR |
| `ServerConfig::session_timeout` | RTSP 会话超时 | 设备侧建议 86400s（IPC 长连接） |
| GOP / 关键帧间隔 | 起播延迟上限 | 建议 1s（如 30fps 下 `-g 30`）；越大起播越慢 |
| B 帧 | 解码缓冲延迟 | 建议关闭（VENC 开 low-delay / zerolatency） |

### 内存预算公式（P0-2）

```
每会话内存 ≈ consumer_queue_capacity × 平均帧大小
平均帧大小 ≈ 码率 ÷ fps ÷ 8
例：1080p 4Mbps@30fps → 16.7KB/帧 → 128 帧 ≈ 2.1MB/会话
例：子码流 512Kbps@15fps → 4.3KB/帧 → 64 帧 ≈ 0.27MB/会话
```

- 总预算 = 各码流每会话 × 并发客户端数 + startup 快照（0.5s×码率）+ HLS
  段缓冲（若开：2s 段 × 码率 × lookback）。
- `examples/ipc_camera --queue 128`（默认）即按此预算；主/子码流若需
  不同容量，可起两个 `RtspServer`（不同端口）分别配置。
- 队列满时丢最旧帧（`PushResult::DroppedQueueFull` / 客户端 lagged），
  不会无限增长——网络抖动与内存预算之间按产品取舍。

## 7. 部署检查清单

1. `cargo build --target aarch64-... --features embedded-gateway` 通过
2. 板子上跑 `examples/ipc_camera`（或你的接入代码），`RUST_LOG=rtflow=info`
3. PC 上 `ffprobe -v trace rtsp://<ip>:8554/cam/main`：能看到
   `a=fmtp:96 profile-level-id=...;sprop-parameter-sets=...` 且能出帧
4. VLC 播放主/子码流均正常；断网重连后设备自动恢复推流
   （`allow_replace_after_disconnect=true`）

## 8. 弱核 runtime 指引（P0-3）

框架不创建 runtime（由你的 `main` 决定）。弱核 IPC（如单核 A53）推荐：

```rust
#[tokio::main(flavor = "current_thread")]
async fn main() -> rtflow::Result<()> {
    // 纯 RTSP 预览（≤ 4 路客户端）单线程足够；
    // 并发 HLS/HTTP-FLV 消费或大客户端数时回到多线程。
    ...
}
```

- 多核平台：**每核一个 `current_thread` runtime、各管一路码流**，比
  多线程共享 runtime 的锁竞争更低（发布路径每帧一次 broadcast 锁）。
- `RtspServer` 与 `publisher()` 均可跨 task 使用；把一路码流的
  VENC 回调绑定到一个 runtime 的 task 即可。
- 线程数 = `rt-multi-thread` 默认按 CPU 核数；64MB RAM 设备建议
  `current_thread` 或 `worker_threads(2)`。

## 9. 多码流与节能策略（P1-7）

- 主/子码流 = 独立 `Mount`（`/cam/main`、`/cam/sub`）+ 独立 publisher；
  HiMPP 双 VENC 线程各推一路（见 `examples/ipc_camera`）。
- 按订阅者数节能：`mount.subscriber_count()` 已暴露唯一会话数——
  子码流无订阅者时可降低 VENC 帧率/码率或暂停推流：

```rust
if mount.subscriber_count() == 0 {
    // 通知 VENC 降帧率/暂停子码流（HiMPP 侧），或直接停推
}
```

- 断流恢复：`allow_replace_after_disconnect=true` 下，VENC 重启后新的
  publisher 自动替换旧会话，播放器无需重连（现有行为）。

## 10. 录制与掉电恢复（P1-8）

- `file-storage` 录制按 `pts` 切段；建议每段 ≤ 1s 并定期
  `fsync`（掉电最多丢 1 段），SD 卡/NAND 上避免高频小写。
- 断流（编码器重启）会写 `DISCONTINUITY` 标记，播放器侧按标记重置
  解码器；录制文件按段自包含（关键帧开头），可独立回放。
- 异常退出前调用 `server.shutdown()`（有 `shutdown_grace` 宽限），
  进程被杀时至少保证已写段完整。

## 11. HiMPP VB 零拷贝接入（P2-9）

默认路径：VENC 输出 VB 块 → memcpy 到 Rust `Vec` → `AccessUnit`。
省掉这次拷贝的方法——把 VB 块包装成 `Bytes`（引用计数释放时归还 VB）：

```rust
use std::os::raw::c_void;

// VB 块包装：drop 时调 HI_MPI_VB_ReleaseBlock
struct VbBlock { vb: u32 } // HI_VB_BLOCK
impl Drop for VbBlock {
    fn drop(&mut self) {
        unsafe { hisi::HI_MPI_VB_ReleaseBlock(self.vb) }; // 伪代码
    }
}
fn wrap_vb(block: VbBlock, ptr: *mut c_void, len: usize) -> Bytes {
    let slice = unsafe { std::slice::from_raw_parts(ptr as *const u8, len) };
    // Bytes::from_owner 将 Vec<u8> 的所有权语义迁移：
    let mut v = Vec::with_capacity(len);
    unsafe { v.set_len(len); }
    v.copy_from_slice(slice); // ← 若想真零拷贝，需自定义 allocator 让 Vec 直接
                              //   指向 VB 块并借 Drop 归还——Rust 侧可用
                              //   bytes::Bytes::from_owner(Box::new(VbBlock)) 的
                              //   扩展 allocator 方案，或直接每帧一次 memcpy
                              //   （4Mbps@30fps ≈ 0.5MB/s，CPU 可忽略）
    Bytes::from(v)
}
```

工程结论：4Mbps 主码流每帧 ~16KB，memcpy 一次的开销（≈2µs/帧）远小于
VENC 编码本身，**IPC 上建议先用 memcpy 路径**；真零拷贝（自定义
allocator / `Bytes::from_owner`）留到 CPU 余量 < 20% 时再优化。

## 12. 分配器与 IO 层评估（P2-10/11）

- 默认 glibc malloc 在多线程 runtime 下 arena 膨胀：64MB 设备建议
  `MALLOC_ARENA_MAX=2`（或 `MALLOC_ARENA_MAX=1`）环境变量，或链接
  jemalloc（`tikv-jemallocator`）——实测 RSS 通常可降 10–30%。
- `tokio-uring`（io_uring）对本框架的收益：RTSP 小包多发场景 syscall
  减少有限；**弱核 + 大客户端数（>16）时再评估**，当前 TCP socket 路径
  已用 tokio 批量写。
- RTP 打包（`src/rtp/payload.rs`）每帧新建 `BytesMut`（H.264 分片
  2–5 次小分配/帧，30fps ≈ 150 次小分配/秒，CPU 占比 <0.1%）——
  核查结论：**不值得引入池化**（复杂度/内存碎片风险大于收益）；
  如弱核上仍见分配热点，先 `perf` 确认热路径再优化。
