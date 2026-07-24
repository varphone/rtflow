# 示例程序与联调指南

所有示例都是可运行 CLI。先用 `cargo run --example NAME -- --help` 查看完整参数；日志级别由 `RUST_LOG` 控制。服务端默认监听 `0.0.0.0:8554`，支持 `--listen 127.0.0.1:0` 使用系统分配端口。

## 本地编码源

```bash
cargo run --example local_source -- \
  --mount /camera --codecs h264,pcma --fps 25

cargo run --example play_client -- \
  rtsp://127.0.0.1:8554/camera --transport tcp --max-frames 100
```

`local_source` 模拟硬件编码器回调，把已编码的 `Bytes` 直接交给 `LocalPublisher`。示例载荷用于协议联调，不代替实际编码器。

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
./tools/regenerate-fixtures.sh
(cd tests/fixtures && sha256sum -c SHA256SUMS)
```

外部互操作测试显式标记为 ignored：

```bash
cargo test --test gstreamer_interop -- --ignored --nocapture
cargo test --test gstreamer_codec_matrix -- --ignored --nocapture
cargo test --test ffmpeg_publish_interop -- --ignored --nocapture
cargo test --test live_tool_stream -- --ignored --nocapture
cargo test --test file_stream -- --ignored --nocapture
```

CI 的 `media-interop` 任务安装 FFmpeg 和 GStreamer 后强制执行这些测试，不会把缺少工具误报为通过。
