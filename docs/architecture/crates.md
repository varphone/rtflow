# Workspace crate 参考

仓库是 Cargo workspace，根 `Cargo.toml` 只包含 `[workspace]`、`[workspace.package]`、
`[workspace.dependencies]` 与 profile 配置。所有源码位于 `crates/<name>/src/`。

**对外只承诺 `rtflow` 门面 crate 的路径**；其余 crate 视为实现细节，可能在小版本中
调整（名称、模块、`#[doc(hidden)]` 项）。应用代码请依赖 `rtflow`。

## 层级（数字越小越底层，只能向下依赖）

| 层 | crate | 职责 | 主要 feature |
|---|---|---|---|
| 0 | `rtflow-core` | `Error`/`Result`/`ErrorCode`、`MountId`/`SessionId`、`FrameworkEvent`、`MetricsRegistry` | — |
| 1 | `rtflow-media` | `Codec`/`AccessUnit`/`MediaDescription`、位流与时钟、`Demuxer`/`VodSource` 抽象、SDP 值类型 | `mp4`, `mkv`, `multicast` |
| 2 | `rtflow-rtp` | RTP 打包/解包、jitter buffer | — |
| 3 | `rtflow-rtcp` | RTCP 包、接收统计、RTT | — |
| 3 | `rtflow-srtp` | SRTP/SRTCP 上下文与密钥派生 | — |
| 3 | `rtflow-multicast` | 组播 socket 配置 | — |
| 3 | `rtflow-security` | IP ACL、连接限流、TLS 配置 | `tls` |
| 4 | `rtflow-rtsp` | RTSP 消息、SDP、Range、鉴权、Transport | `srtp`, `multicast`, `tls` |
| 5 | `rtflow-runtime` | 任务监督、控制连接、UDP 批量发送 | — |
| 6 | `rtflow-framing` | RTSP/interleaved 帧化 | — |
| 6 | `rtflow-mount` | 流注册表、FrameBus、启动缓存、代理上游状态 | `tls`, `multicast` |
| 6 | `rtflow-pipeline` | Element DAG 管线 | — |
| 6 | `rtflow-http` | HTTP/1.1 服务、路由、鉴权中间件 | — |
| 6 | `rtflow-file` | 容器解复用与文件播放 | `mp4`, `mkv` |
| 7 | `rtflow-storage` | 崩溃安全录制、恢复索引、VOD source | — |
| 7 | `rtflow-metrics` | Prometheus/JSON 导出与 `/metrics`、`/stats` | — |
| 8 | `rtflow-rtsp-client` | RTSP 播放/发布客户端 | `tls`, `srtp`, `multicast` |
| 8 | `rtflow-rtsp-server` | RTSP 服务端会话、代理、VOD | `tls`, `srtp`, `multicast`, `file-storage`, `metrics` |
| 8 | `rtflow-source` | 内置合成测试源 | — |
| 9 | `rtflow-hls` | HLS live（MPEG-TS）、VOD（fMP4）、LL-HLS | `multicast` |
| 9 | `rtflow-http-flv` | HTTP-FLV / WS-FLV | `ws` |
| 9 | `rtflow-rtmp` | RTMP 推拉流 | — |
| 9 | `rtflow-srt` | SRT ingest | — |
| 9 | `rtflow-onvif` | ONVIF WS-Discovery 与 SOAP 客户端 | — |
| 9 | `rtflow-webrtc` | WHIP/WHEP | — |
| 9 | `rtflow-gst` | GStreamer 源/汇 | — |
| 10 | `rtflow-dash` | DASH MPD 与分片端点 | — |
| 10 | `rtflow-admin` | 管理控制台（API + SPA） | — |
| 10 | `rtflow-gateway` | 多协议编排 | — |
| 11 | `rtflow` | 门面：re-export 全部公开路径与 feature 开关 | 见下表 |
| 12 | `rtflow-server` | 官方应用：协议编排、录制/VOD、路径控制面、部署资产 | — |
| 13 | `rtflow-testkit` | dev-only 测试线束（不发布） | — |

## 门面 feature 与子 crate 的对应

| `rtflow` feature | 打开的子 crate / feature |
|---|---|
| `client` | `rtflow-rtsp-client` |
| `server` | `rtflow-rtsp-server` |
| `http` | `rtflow-http` |
| `metrics` | `rtflow-metrics` |
| `file-storage` | `rtflow-file` + `rtflow-storage` |
| `mp4` / `mkv` | `rtflow-file/{mp4,mkv}` |
| `hls` | `rtflow-hls`（并启用 `http`、`mp4`） |
| `dash` | `rtflow-dash`（并启用 `hls`） |
| `http-flv` | `rtflow-http-flv` |
| `ws-flv` | `rtflow-http-flv/ws` |
| `rtmp` / `srt` / `onvif` | `rtflow-rtmp` / `rtflow-srt` / `rtflow-onvif` |
| `webrtc` | `rtflow-webrtc` |
| `srtp` | `rtflow-srtp` + RTSP/客户端/服务端 `srtp` |
| `multicast` | media/rtsp/client/server/mount/hls `multicast` |
| `tls` | `rtflow-security/tls` + client/server/mount `tls` |
| `test-source` | `rtflow-source` |
| `gstreamer` | `rtflow-gst` |
| `admin` | `rtflow-admin`（并启用 `http`、`server`、`hls`） |
| `logging` | `tracing-subscriber` + `rtflow::logging`（`full` 默认包含） |
| `full`（默认） | 生产常用集合（`srtp`/`multicast` 仍为显式 opt-in） |
| `embedded-gateway` | RTSP + TLS + metrics + HTTP-FLV + HLS |
| `experimental-protocols` | DASH、RTMP、SRT、WebRTC |

## 发布顺序

`cargo publish --workspace` 会按内部依赖拓扑排序。`rtflow-testkit` 设置了
`publish = false`，不参与发布。

## 校验

```bash
python3 scripts/check-layering.py --matrix   # 分层与无环
python3 scripts/check-tree.py                # #[path]、文件行数、根目录
cargo test --workspace --all-features
cargo clippy --workspace --all-targets --all-features -- -D warnings
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --all-features
cargo package --workspace --allow-dirty
```
