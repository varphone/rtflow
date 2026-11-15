# 日志与调试

`rtflow` 使用 [`tracing`](https://docs.rs/tracing) 输出结构化日志。库本身不安装
subscriber；应用可以选择自己的 subscriber，或使用门面 crate 的可选初始化器。

## 快速开始

```rust
fn main() {
    rtflow::logging::init();
    // ...
}
```

等价于：

```bash
RUST_LOG=info cargo run --example live_vod_gateway
```

更详细的协议层调试：

```bash
RUST_LOG=info,rtflow_rtsp_server=debug,rtflow_rtsp_client=debug,rtflow_mount=debug,rtflow_hls=debug cargo run ...
```

`rtflow::logging::DEBUG_FILTER` 提供了同一组过滤器常量。

## 级别约定

| 级别 | 内容 |
|---|---|
| `error` | 不可恢复的任务 panic、数据面彻底失败 |
| `warn` | 可恢复失败、鉴权/限流拒绝、协议错误、丢帧/超时 |
| `info` | 服务启动/停止、mount 注册/注销、会话建立/关闭、推流/播放开始、HLS 分段器启动、VOD 生成 |
| `debug` | 每个 RTSP 请求、SETUP 传输细节、重连/回退、segment ready、协议协商 |
| `trace` | 逐请求/逐帧级别，仅用于深度排查（如 HTTP 请求） |

## 常用 target

默认 target 是模块路径，例如：

```text
rtflow_rtsp_server::session
rtflow_rtsp_client::rtsp_client
rtflow_mount::registry
rtflow_hls::live
rtflow_rtmp::session
rtflow_http
```

框架事件（`FrameworkEvent`）由 `TracingEventSink` 统一输出到
`rtflow::event` target，可单独打开：

```bash
RUST_LOG=info,rtflow::event=debug
```

## 常见排查

### 推流已连接但播放黑屏

```bash
RUST_LOG=info,rtflow_rtsp_server=debug,rtflow_mount=debug,rtflow_hls=debug
```

关注：

- `RTSP ANNOUNCE accepted` 的 `tracks`/`fmtp_tracks`；
- `RTSP RECORD started` 的 `udp_tracks`/`interleaved_tracks`；
- `mount registered` 的 `tracks`；
- `HLS live segmenter started` 与 `HLS segment ready`。

### 播放端反复重连

```bash
RUST_LOG=info,rtflow_rtsp_client=debug
```

关注 `RTSP client session established`、`RTSP client PLAY`、重连
`attempt`/`delay_ms`，以及 `background task failed`。

### 推流一段时间后断开

```bash
RUST_LOG=info,rtflow_rtsp_server=debug
```

服务端在会话超时/失败时输出 `RTSP session failed` 与会话耗时；UDP 发布
活动的超时判断见 `RTSP session` 相关代码。

### HLS/DASH 404 或空 playlist

```bash
RUST_LOG=info,rtflow_hls=debug,rtflow_dash=debug
```

关注 `HLS VOD generation started/finished`、`HLS segment ready`、
`HLS live segmenter started/stopped`。

## 接入自定义 subscriber

```rust
tracing_subscriber::fmt()
    .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
    .json()
    .init();
```

只要 subscriber 实现 `tracing::Subscriber`，框架日志会自动进入；无需修改
任何业务代码。

## feature

`rtflow::logging` 由门面 crate 的 `logging` feature 提供（`full` 默认包含）。
关闭它可避免把 `tracing-subscriber` 引入最终二进制；库代码仍然只依赖
`tracing`。
