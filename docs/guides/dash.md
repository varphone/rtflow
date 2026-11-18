# DASH 直播输出

DASH 复用 `rtflow-hls` 的 fMP4 封装实现，但像 SRS 一样**按轨独立打包**：
`video-init.mp4` + `video-N.m4s`、`audio-init.mp4` + `audio-N.m4s`，MPD 中每个
track 一个 AdaptationSet。

## 端点

```text
GET /dash/{mount}/manifest.mpd
GET /dash/{mount}/video-{track}-init.mp4
GET /dash/{mount}/audio-init.mp4          # 单音轨
GET /dash/{mount}/audio-{track}-init.mp4  # 多音轨
GET /dash/{mount}/video-{track}-{N}.m4s
GET /dash/{mount}/audio-{N}.m4s            # 单音轨
GET /dash/{mount}/audio-{track}-{N}.m4s    # 多音轨
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
- 音频轨在视频分片边界处 reap；没有视频时使用约 `segment_duration` 的音频分片。
- `time_shift_buffer_depth` 控制 MPD 声明的时移窗口。
- 配置错误（0 分片、0 窗口、0 缓冲）在启动时拒绝。

## MPD 结构

- `type="dynamic"`，`availabilityStartTime` 在当前 presentation 内固定；
- 一个视频 AdaptationSet + 每个音轨一个音频 AdaptationSet；
- 视频 AdaptationSet 可包含多个 Representation（每个源视频轨一个）：
  - `id="video-{track}"`、`codecs`（H.264 从 avcC 提取，H.265 用 hvc1）、
    `width/height/frameRate`、`video-{track}-init.mp4`、`video-{track}-$Number$.m4s`；
- 音频每个源音轨一个 AdaptationSet/Representation：
  - 单音轨：`id="audio"`、`audio-init.mp4`、`audio-$Number$.m4s`；
  - 多音轨：`id="audio-{track}"`、`audio-{track}-init.mp4`、`audio-{track}-$Number$.m4s`；
- 每个 Representation 自带 `SegmentTemplate` + `SegmentTimeline`，`startNumber`
  独立；
- `UTCTiming` 默认使用 `urn:mpeg:dash:utc:http-iso:2014` 指向公开的
  `/api/v1/time`；库用户未配置 `MpdConfig::utc_timing_url` 时回退到 `direct`；
- `bandwidth` 由各轨实际分片字节数/时长估算；
- MPD 在所有已声明视频轨和音频轨都出现首个分片后才返回 200，避免播放器先拿到
  不完整的 Representation 集合。

## LL-DASH（低延迟）

低延迟入口独立于普通 DASH，按 DASH-IF LL IOP 的 per-track CMAF chunk 方式实现：

```text
GET /dash-ll/{mount}/manifest.mpd
GET /dash-ll/{mount}/video-{track}-init.mp4
GET /dash-ll/{mount}/audio-{track}-init.mp4
GET /dash-ll/{mount}/video-{track}-{N}.m4s
GET /dash-ll/{mount}/audio-{track}-{N}.m4s
```

- 每个 track 使用 `transmux::LlSegmenter`，分片边界仍由视频关键帧对齐；
- 分片 URL 使用 HTTP chunked transfer，chunk 生成后立即写出；
- MPD 标 `availabilityTimeComplete="false"`，
  `availabilityTimeOffset = segment_duration - chunk_duration`；
- 顶层 `ServiceDescription/Latency` 给出目标延迟；
- 默认 segment 1s、chunk 200ms、target latency 2s；
- `UTCTiming` 指向 `/api/v1/time`。

```toml
[dash]
# LL-DASH 可选项（缺省 1s/200ms/2s）
ll_chunk_duration = "200ms"
ll_latency_target = "2s"
ll_time_shift = "6s"
```

dash.js 可直接播放该 MPD；控制台直播详情提供“预览 LL-DASH”。

## DASH VOD

录制目录会同时生成 DASH VOD：录制完成后在点播页可复制
`/dash-vod/{id}/manifest.mpd`。服务端按 `VodSource` 的 fingerprint 在
`state_dir/dash-vod-cache` 下生成一次 fMP4 缓存：

```text
GET /dash-vod/{id}/manifest.mpd       # static MPD
GET /dash-vod/{id}/video-{track}-init.mp4
GET /dash-vod/{id}/audio-init.mp4          # 单音轨
GET /dash-vod/{id}/audio-{track}-init.mp4  # 多音轨
GET /dash-vod/{id}/video-{track}-{N}.m4s
GET /dash-vod/{id}/audio-{N}.m4s            # 单音轨
GET /dash-vod/{id}/audio-{track}-{N}.m4s    # 多音轨
```

-静态 MPD 的 `mediaPresentationDuration` 来自录制时长，`Period@duration` 同步；
- 生成缓存按录制目录的 fingerprint 失效，录制删除时同步清理；
- 与 HLS VOD 相互独立：HLS 使用 MPEG-TS 以兼容 Firefox MSE，DASH 强制 fMP4；
- 请求 MPD 时携带的 token query 同样会传播到 init/分片。

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

- 直播多视频轨会生成多个视频 Representation，但需要源端本身提供多 rendition（当前不做转码
  ladder）；各 rendition 的分片边界独立，MPD 标 `segmentAlignment="false"`，只在源端 GOP
  对齐时具备完整 ABR 切换体验。直播和 VOD 都支持多条音轨（每轨一个 AdaptationSet）。
- DASH VOD 目前由服务端的录制目录生成；其他 `VodSource` 可复用同一 manager 接入。暂不支持多码率/多 Period VOD。
- LL-DASH 已在 `/dash-ll` 提供；暂不支持 DRM/CENC、多 Period、广告插入。
- H.264 codec 从 avcC 提取；H.265 从 hvcC 提取 `hvc1` codec（profile/tier/level/constraints），不支持的变体回退到静态字符串。

## 测试

```bash
cargo test -p rtflow-dash
cargo test -p rtflow --features "dash http server" --test protocols dash
cargo test -p rtflow-server --test dash
```

`rtflow-server/tests/dash.rs` 需要 `ffmpeg`；不可用时自动跳过。
