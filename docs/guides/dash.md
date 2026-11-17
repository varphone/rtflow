# DASH 直播输出

DASH 复用 `rtflow-hls` 的 fMP4 封装实现，但像 SRS 一样**按轨独立打包**：
`video-init.mp4` + `video-N.m4s`、`audio-init.mp4` + `audio-N.m4s`，MPD 中每个
track 一个 AdaptationSet。

## 端点

```text
GET /dash/{mount}/manifest.mpd
GET /dash/{mount}/video-init.mp4
GET /dash/{mount}/audio-init.mp4
GET /dash/{mount}/video-{N}.m4s
GET /dash/{mount}/audio-{N}.m4s
```

MPD 内的 `initialization` 与 `media` 是**相对 MPD URL** 的路径，因此部署在
`/dash/{mount}/...` 或反向代理子路径下都不需要额外配置。请求 MPD 时携带的
`?token=...` 会同样追加到 init 和分片 URL。旧客户端请求 `init.mp4`/`init.m4s`
时按视频轨处理，`segment_N.m4s` 也按视频轨兼容。

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
- 音频轨使用独立的、目标长度约等于 `segment_duration` 的分片器；音视频分片边界
  可以不同，播放器按 `tfdt`/时间戳对齐。
- `time_shift_buffer_depth` 控制 MPD 声明的时移窗口。
- 配置错误（0 分片、0 窗口、0 缓冲）在启动时拒绝。

## MPD 结构

- `type="dynamic"`，`availabilityStartTime` 在当前 presentation 内固定；
- 每个 track 一个 AdaptationSet，`mimeType="video/mp4"` / `"audio/mp4"`；
- 每个 AdaptationSet 一个 Representation：
  - 视频：`id="video"`、`codecs`（H.264 从 avcC 提取，H.265 用 hvc1）、
    `width/height/frameRate`、`video-init.mp4`、`video-$Number$.m4s`；
  - 音频：`id="audio"`、`codecs`、`audioSamplingRate`、`AudioChannelConfiguration`、
    `audio-init.mp4`、`audio-$Number$.m4s`；
- 每个 Representation 自带 `SegmentTemplate` + `SegmentTimeline`，`startNumber`
  独立；
- `UTCTiming` 使用 `urn:mpeg:dash:utc:direct:2014`；
- `bandwidth` 由各轨实际分片字节数/时长估算；
- MPD 在两个已声明的 track 都出现首个分片后才返回 200，避免播放器先拿到单轨
  MPD 后不再添加另一轨。

## 播放器验证

```bash
ffplay http://127.0.0.1:3000/dash/live/cam/manifest.mpd
ffprobe -v error -show_entries stream=codec_type -of csv=p=0 \
  http://127.0.0.1:3000/dash/live/cam/manifest.mpd
```

dash.js 也可直接加载该 MPD；`crates/rtflow/examples/protocols/dash_server.rs` 提供
一个带诊断按钮的示例播放器页面。

## 时间轴与重连

- 同一编码的发布者重连：`availabilityStartTime` 保持不变，DASH 以两个轨最后一段的
  最大结束时间为 offset 接续新分片，`S@t` 不倒退。
- 输入 lag 后等待下一个视频关键帧；不把半帧写入分片。
- init 段由各轨独立生成；编码配置变化时播放器需要重新加载 MPD/init。

## 已知限制

- 每个 track 单 Representation，无多码率 ABR；音频/视频分片边界独立，未像 SRS
  那样逐段严格对齐。
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
