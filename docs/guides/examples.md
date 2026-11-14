# 示例程序与联调指南

示例源码按域分组在 `examples/{server,client,protocols,gateway,embedded}/`，静态资源在 `examples/assets/`；命令中的 `--example <name>` 不变。所有示例都是可运行 CLI。先用 `cargo run --example NAME -- --help` 查看完整参数；日志级别由 `RUST_LOG` 控制。服务端默认监听 `0.0.0.0:8554`，支持 `--listen 127.0.0.1:0` 使用系统分配端口。

## 本地编码源

```bash
cargo run --example local_source -- \
  --mount /camera --codecs h264,pcma --fps 25

cargo run --example play_client -- \
  rtsp://127.0.0.1:8554/camera --transport tcp --max-frames 100
```

`local_source` 模拟硬件编码器回调，把已编码的 `Bytes` 直接交给 `LocalPublisher`。示例载荷用于协议联调，不代替实际编码器。

## 播放客户端（play_client）

`play_client` 是 `rtflow::client::RtspClient` 的独立演示：连接、拉流、可选落盘。

```bash
# 传输选择：udp / tcp / auto（默认 udp；auto 在 SETUP 被 461 拒绝时自动回退 TCP interleaved）
cargo run --example play_client -- rtsp://127.0.0.1:8554/camera --transport tcp

# 断线自动重连（connect_reconnecting，指数退避；永久性错误如 401 会立即终止）
cargo run --example play_client -- rtsp://127.0.0.1:8554/camera --reconnect --duration 60

# 认证 + 限时拉取 + 落盘
cargo run --example play_client -- rtsp://127.0.0.1:8554/secure \
  --username viewer --password viewer-secret --duration 10 --output-dir /tmp/out
```

非重连模式下每 5 秒打印一次 RTCP 质量快照（每轨道收包/丢包数）；日志加上 `RUST_LOG=rtflow=debug` 可看到会话 ID、传输协商与 keep-alive 细节。

## 文件转 RTSP

```bash
cargo run --example file_server -- tests/fixtures/h264-aac.mp4 \
  --mount /movie --plays 0 --rate 1.0 --pause-when-idle true

cargo run --example play_client -- \
  rtsp://127.0.0.1:8554/movie --transport udp \
  --duration 5 --output-dir /tmp/rtsp-output
```

输出目录按轨道保存裸编码数据。H.264/H.265 是 Annex-B，AAC 是不含 ADTS 头的访问单元拼接，G.711 是裸 A-law/μ-law。

## 按需拉流代理

```bash
cargo run --example proxy_server -- \
  rtsp://192.0.2.10/live/main \
  --mount /proxy --upstream-transport tcp \
  --username camera --password secret
```

代理在首个下游 PLAY 时连接上游，最后一个订阅者离开并超过 idle grace 后停止；日志周期输出 `ProxyState`、订阅者和 RTP 统计。

## 接收外部发布

```bash
cargo run --example publisher_server -- \
  --mount /ingest --codecs h264,aac

ffmpeg -re -i input.mp4 -c copy \
  -rtsp_transport tcp -f rtsp rtsp://127.0.0.1:8554/ingest
```

其他客户端可以同时拉取同一 `/ingest`。框架本期只提供 RECORD 接收服务，不提供 Rust ANNOUNCE/RECORD 客户端 API。

## RTMP 推流 + HLS 直播/点播 + 控制台

```bash
cargo run --example live_vod_gateway --features "server,rtmp,hls,http-flv,file-storage,mp4,mkv" \
  --vod-file demo=tests/fixtures/h264-aac.mp4 --vod-cache /tmp/rtflow-vod
```

- OBS：服务器 `rtmp://127.0.0.1:1935/live`，串流密钥 `cam`；凭据模式可用
  `cam?token=<HMAC>`（`--token secret`）。
- 浏览器：`http://127.0.0.1:8080/` 浏览直播/VOD，`/admin` 只读状态表。
- HLS 直播：`/hls/live/cam/index.m3u8`；VOD：`/vod/demo/index.m3u8`。

## IPC 摄像头 + 网页播放

`ipc_camera` 模拟 HiMPP 双码流（main/sub）+ 可选音频，并默认同时启动
浏览器播放服务：

```bash
cargo run --example ipc_camera -- --audio aac
# 浏览器打开 http://127.0.0.1:8080/ —— 页面自动列出 /cam/main、/cam/sub
# RTSP 播放: vlc rtsp://127.0.0.1:8554/cam/main
```

网页页面由 `http_flv::web::create_web_player_handler` 生成，三种浏览器
直播协议一键切换（断流自动重连）：

| 模式 | URL 形态 | 延迟 | 播放器 |
| --- | --- | --- | --- |
| WS-FLV | `ws://host:8088/cam/main.flv` | 最低 | mpegts.js |
| HTTP-FLV | `http://host:8080/cam/main.flv` | 低 | mpegts.js |
| HLS | `http://host:8080/cam/main.m3u8` | 1–3 s | hls.js / Safari 原生 |

默认端口：RTSP `8554`、页面/HTTP-FLV/HLS `8080`、WS-FLV `8088`；
`--no-web` 只保留 RTSP。mpegts.js/hls.js 从 CDN（jsdelivr，自动回退
unpkg）加载；离线内网环境请把 `src/http_flv/player.html` 中的两个
`<script>` 换成自托管文件。

## 动态实时串流

一键启动 Rust Publisher、FFmpeg 动态测试源和 GStreamer 播放器：

```bash
cargo run --example live_stream_demo

# 默认只播放视频，避免音频设备状态阻塞首帧；需要声音时显式开启
cargo run --example live_stream_demo -- --play-audio

# 无显示设备或 CI 环境
cargo run --example live_stream_demo -- --headless --duration 10
```

也可以分别运行三个组件，接入已有服务或播放器：

```bash
cargo run --example publisher_server -- --mount /ingest --codecs h264,aac

cargo run --example ffmpeg_live_publish -- \
  rtsp://127.0.0.1:8554/ingest \
  --transport tcp --size 1280x720 --fps 25 --audio true

cargo run --example gstreamer_live_play -- \
  rtsp://127.0.0.1:8554/ingest \
  --transport tcp --latency-ms 100

# 同时播放 AAC 音频
cargo run --example gstreamer_live_play -- \
  rtsp://127.0.0.1:8554/ingest --audio true
```

FFmpeg 使用 `testsrc2` 动态生成 H.264 测试图，使用 `sine` 生成 AAC 音频，不读取临时媒体文件。GStreamer 默认解码并播放视频；使用 `--audio true` 或一键演示的 `--play-audio` 开启音频。音频 sink 不参与异步状态门控，避免无音频设备或 AAC 首帧延迟阻塞视频；嵌入式无显示设备使用 `--headless`，完成解包、解码并把原始音视频送入 `fakesink` 验证。三个示例都支持 `--duration` 自动退出，并确保回收外部子进程。

依赖 Ubuntu/Debian 的 `ffmpeg`、`gstreamer1.0-tools`、`gstreamer1.0-plugins-base/good/bad/ugly` 和 `gstreamer1.0-libav`。FFmpeg 构建必须包含 `libx264` 和原生 AAC encoder。

## 鉴权与角色

```bash
cargo run --example authenticated_server -- --mode digest

cargo run --example play_client -- \
  rtsp://127.0.0.1:8554/secure \
  --username viewer --password viewer-secret

ffmpeg -re -i input.mp4 -c copy -f rtsp \
  rtsp://publisher:publish-secret@127.0.0.1:8554/secure-ingest
```

示例为读取和发布配置不同凭据。Publisher mount 的 SETUP、ANNOUNCE、RECORD 均按 `AccessRole::Publish` 鉴权。

## Element Pipeline

```bash
cargo run --example pipeline_server -- --mount /pipeline
cargo run --example play_client -- \
  rtsp://127.0.0.1:8554/pipeline --transport tcp --duration 5
```

该示例构建 `IdentityElement → PublisherSink` DAG，展示状态迁移、有界 ingress 和自定义 Element 如何连接 RTSP mount。

## 测试层级

默认测试不依赖系统媒体工具：

```bash
cargo test --all-targets
cargo build --examples
```

提交的 `tests/fixtures` 覆盖 MP4/MOV/fMP4、MKV、TS/M2TS、WAV 和裸流。需要重新生成时运行：

```bash
./scripts/regenerate-fixtures.sh
(cd tests/fixtures && sha256sum -c SHA256SUMS)
```

外部互操作测试显式标记为 ignored：

```bash
cargo test --test interop -- --ignored --nocapture gstreamer_interop
cargo test --test interop -- --ignored --nocapture gstreamer_codec_matrix
cargo test --test ffmpeg_publish_interop -- --ignored --nocapture
cargo test --test live_tool_stream -- --ignored --nocapture
cargo test --test storage -- --ignored --nocapture file_stream
```

CI 的 `media-interop` 任务安装 FFmpeg 和 GStreamer 后强制执行这些测试，不会把缺少工具误报为通过。

## 绝对时间进度

`live_stream_demo --timeline uptime|unix|absolute` 可选择 NPT 模式，
详见 [absolute-timeline.md](absolute-timeline.md)。
