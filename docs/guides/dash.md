# DASH 直播输出

DASH 与 HLS 共用 `rtflow-hls` 的 fMP4 segmenter：HLS segmenter 把音视频写入同一个
init/media 分片，DASH 侧按 **单个 multiplexed Representation** 声明，而不是拆成两个
指向同一套分片的 AdaptationSet。

## 端点

```text
GET /dash/{mount}/manifest.mpd           # 动态 MPD
GET /dash/{mount}/init.mp4               # fMP4 init（兼容 init.m4s）
GET /dash/{mount}/segment_{N}.m4s        # 媒体分片
```

MPD 内的 `initialization` 与 `media` 是**相对 MPD URL** 的路径，因此部署在
`/dash/{mount}/...` 或反向代理子路径下都不需要额外配置。请求 MPD 时携带的
`?token=...` 会同样追加到 init 和分片 URL。

`rtflow-server` 依赖的 `rtflow` 开启了 `full`/`experimental-protocols`，因此官方应用默认
包含 DASH；库用户需要显式启用 `rtflow` 的 `dash` feature（它依赖 `hls`）：

```bash
cargo build -p rtflow --features dash
```

## 配置

```toml
[dash]
enabled = true
# 以下均可选：缺省继承 [hls] 的分片时长/窗口。
segment_duration = "1s"
window = 5
min_buffer_time = "2s"
time_shift_buffer_depth = "5s"        # 缺省 segment_duration × window
suggested_presentation_delay = "3s"   # 缺省 3 × segment_duration
```

- DASH 始终运行自己的 segmenter；缺省值继承 `[hls]`，避免两套参数不一致。
- `time_shift_buffer_depth` 控制 MPD 声明的时移窗口，应不小于实际保留窗口。
- 配置错误（0 分片、0 窗口、0 缓冲）在启动时拒绝。

## MPD 结构

- `type="dynamic"`，`availabilityStartTime` 在 presentation 内固定；
- `UTCTiming` 使用 `urn:mpeg:dash:utc:direct:2014`，值为生成 MPD 的 UTC 时间；
- 一个 AdaptationSet + 一个 Representation（id `muxed`），`codecs` 拼接视频与音频，
  例如 `codecs="avc1.640028,mp4a.40.2"`；
- 视频轨存在时带 `width/height/frameRate`，音频轨存在时带 `audioSamplingRate` 和
  `AudioChannelConfiguration`；
- `SegmentTimeline` 显式列出滑动窗口内的分片；
- `bandwidth` 由窗口内实际分片字节数/时长估算。

## 播放器验证

```bash
ffplay http://127.0.0.1:3000/dash/live/cam/manifest.mpd
ffprobe -v error -show_entries stream=codec_type -of csv=p=0 \
  http://127.0.0.1:3000/dash/live/cam/manifest.mpd
```

dash.js 也可直接加载该 MPD；`crates/rtflow/examples/protocols/dash_server.rs` 提供
一个带诊断按钮的示例播放器页面。

## 时间轴与重连

- 同一编码的发布者重连：`availabilityStartTime` 保持不变，DASH 以最后一段的
  `timestamp + duration` 为 offset 接续新分片，`S@t` 不倒退。
- init 段（编码配置）变化：清空当前滑动窗口并按新 presentation 重置 AST 与
  timeline；客户端重新加载 MPD/init。
- 输入 lag 后等待下一个关键帧；不把半帧写入分片。

## 已知限制

- 单 Representation，无多码率 ABR；不做 per-track fMP4 分片。
- 不支持 DASH VOD/静态 MPD（HLS VOD 使用 MPEG-TS；DASH VOD 需要单独的 fMP4 生成）。
- 不支持 LL-DASH/chunked CMAF、DRM/CENC、多 Period、广告插入。
- H.264 codec 从 avcC 提取；H.265 使用 `hvc1` 字符串，未做 hvcC 逐字节解析。

## 测试

```bash
cargo test -p rtflow-dash
cargo test -p rtflow --features "dash http server" --test protocols dash
cargo test -p rtflow-server --test dash
```

`rtflow-server/tests/dash.rs` 需要 `ffmpeg`；不可用时自动跳过。
