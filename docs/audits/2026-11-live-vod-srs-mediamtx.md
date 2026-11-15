# 推流 / 直播 / 点播审核：SRS 与 MediaMTX 对照

日期：2026-11-15

## 范围与方法

- 本仓库的 RTMP 推流、HLS live/VOD、RTSP/FLV 分发、录制与 VOD 缓存。
- 对照：
  - SRS：`trunk/src/app/srs_app_hls.cpp`、`trunk/src/protocol/srs_protocol_rtmp_stack.cpp`、`trunk/src/kernel/srs_kernel_ts.cpp`。
  - MediaMTX v1.21.3：`internal/protocols/hls/from_stream.go`、`internal/protocols/rtmp/to_stream.go`，以及依赖 `gortmplib/pkg/rawmessage/reader.go`、`gohlslib/muxer_segmenter.go`。
- 实测：同一条 ffmpeg 命令分别推送到本仓库和控制组 MediaMTX，抓取 HLS playlist/segment 并用 ffprobe 比较。

## 实测差异

推流：

```bash
ffmpeg -re \
  -f lavfi -i testsrc2=size=640x360:rate=30 \
  -f lavfi -i sine=frequency=440:sample_rate=48000 \
  -c:v libx264 -preset veryfast -tune zerolatency -profile:v baseline \
  -pix_fmt yuv420p -g 30 -keyint_min 30 -sc_threshold 0 \
  -c:a aac -b:a 128k -ar 48000 \
  -f flv rtmp://127.0.0.1:<port>/live/cam
```

MediaMTX 输出：

```text
#EXT-X-VERSION:3
#EXT-X-ALLOW-CACHE:NO
#EXT-X-TARGETDURATION:1
#EXT-X-MEDIA-SEQUENCE:0
#EXT-X-PROGRAM-DATE-TIME:...
#EXTINF:1.00000,
<id>_main_seg0.ts
```

```text
ffprobe: 30 frames, duration 1.000333
PTS: 5.021, 5.054, 5.088, 5.121, ...
```

本仓库修复前：

```text
#EXT-X-TARGETDURATION:2
#EXTINF:1.699,
0.ts
```

```text
ffprobe: 30 frames, duration 1.699333
PTS: 0.1, 0.166, 0.234, 0.3, ...
```

按码流帧率修正 duration 后：

```text
#EXT-X-TARGETDURATION:1
#EXTINF:1.000,
0.ts

ffprobe: 30 frames, duration 1.000000
PTS: 0.100000, 0.133333, 0.166667, 0.200000, ...
```

播放实测：

- Firefox：约 30 fps，225 帧 / 7.5 s，buffer 每秒增加 1 s。
- Edge：约 31 fps，261 帧 / 8.4 s。

## 发现

| ID | 级别 | 区域 | 结论 | 状态 |
|---|---|---|---|---|
| L-01 | P0 | RTMP 推流 | 已用 tcpdump/代理抓取客户端→服务端原始 RTMP 字节，并用 `Dechunker` 重放：69 个视频消息 / 约 2.3 s，时间戳为 21/54/88/121/154/188/221/254…，与 ffmpeg 源一致；RTMP 时间戳本身没有放大。此前的 1.7 s/30 帧来自 TS mux 的 `Sample.duration` 处理，已用码流帧率修正。 | 已排除 |
| L-02 | P1 | HLS live | TS 路径已接入 H.265（hvcC + PMT stream_type 0x24），但 RTMP H.265 目前没有生成 sprop-vps/sps/pps fmtp，实际仍会回退 fMP4。 | 部分完成 |
| L-03 | P1 | HLS live | TS segment 的 `discontinuity_sequence` 之前恒为 `None`，`EXT-X-DISCONTINUITY-SEQUENCE` 永远为 0。 | 已修 |
| L-04 | P1 | HLS VOD | 生成失败时 `.tmp-<id>` 不清理；下次生成可能复用残留目录。 | 已修 |
| L-05 | P1 | HLS VOD | VOD id 允许 `.`/`..`（虽然只能来自本地配置），存在路径穿越风险。 | 已修 |
| L-06 | P1 | RTMP 收流 | fmt3 续包未读取 extended timestamp，长会话/使用 extended timestamp 的发布端会错位。SRS `srs_protocol_rtmp_stack.cpp` 明确处理了该分支。 | 已修 |
| L-07 | P1 | RTMP 收流 | `publisher.push` 的返回值被忽略；FrameBus 满时丢帧只增加统计，不反馈给发布端，也不在日志/指标中突出。SRS/MediaMTX 有明确的队列/丢帧策略。 | **未修** |
| L-08 | P1 | RTMP 编码 | 仅处理 legacy FLV H.264/H.265/AAC；enhanced-RTMP AV1/VP9、增强 H.264 未覆盖。MediaMTX 的 gortmplib 已覆盖。 | 已知限制 |
| L-09 | P2 | HLS live | 未输出 `EXT-X-PROGRAM-DATE-TIME`；MediaMTX 输出，便于绝对时间对齐、广告/DRM/时移。 | 未做 |
| L-10 | P2 | HLS live | playlist 窗口 30 段、保留 90 段；SRS/MediaMTX 默认 3–7 段。内存和切台延迟更高，但可用。 | 可配置 |
| L-11 | P2 | HLS live | 无低延迟 HLS（LL-HLS part/blocking reload）；MediaMTX 默认 lowLatency。当前 1 s 分段属于经典 HLS。 | 已知限制 |
| L-12 | P2 | HLS VOD | 仅对 segment 支持 Range；playlist/init 没有 HEAD/Range。不是协议错误，缓存代理场景可改进。 | 未做 |
| L-13 | P2 | HLS VOD | 仅按显式 `invalidate` 失效，不检测源文件 mtime/大小变化。 | 未做 |
| L-14 | P2 | HLS live | `/init.m4s` 对 TS 版本不再返回空 200（已改 404），但路由仍保留；不影响播放器。 | 已修 |
| L-15 | P2 | HLS live | 无 `EXT-X-INDEPENDENT-SEGMENTS`；不影响当前 hls.js，但部分播放器/下载器会利用。 | 未做 |

## 重点问题说明

### L-01 RTMP 时间戳：已排除

之前用会话日志的过滤输出（只打印关键帧和部分时间戳）误判为时间戳放大。后来通过本地 TCP 代理抓取客户端→服务端原始 RTMP 字节，把 3073 字节握手之后的数据直接喂给 `Dechunker`：

```text
69 个视频消息 / 约 2.3 s
21, 54, 88, 121, 154, 188, 221, 254, 288, 321, ...
```

与 ffmpeg 写入 FLV 的时间戳一致，说明 RTMP 收流时间戳是正确的。

HLS 之前出现 1.7 s/30 帧的真正原因在 TS mux：`Sample.duration` 为空/处理不当，导致 PES 时间轴不按 33 ms 推进；现在用码流帧率（30 fps）写 duration，MediaMTX 对照为 1.000 s/30 帧。

结论：RTSP/FLV/录制使用的 `ts_ms*90` 也来自正确时间戳；不需要重写 RTMP chunk 状态机。仍建议保留“30 fps 源 → 30 帧/1 s”的集成测试，防止回归。

### L-03 discontinuity sequence

TS 路径的 `Segment` 由 `transmux` 返回，原先只带 `discontinuous`，没有递增序号；现在 `TsPipeline` 自己维护 `discontinuity_sequence`，每个 discontinuity segment 写 `Some(n)`，playlist 的 `EXT-X-DISCONTINUITY-SEQUENCE` 才能随窗口滚动正确递增。

### L-04/L-05 VOD 缓存

`ensure()` 现在：

- 生成前删除残留 `.tmp-<id>`；
- 生成失败/rename 失败都删除临时目录；
- 拒绝 `.`、`..` 和以 `.` 开头的 VOD id。

### L-06 RTMP fmt3 extended timestamp

按 RTMP 2012 §5.3.1.3，type 3 chunk 在前一个 type 0/1/2 头声明 extended timestamp 时也要带 4 字节 extended timestamp。此前只对 fmt 0/1/2 读取，fmt3 会错位；现已读取。

## 推荐后续

1. **已完成：RTMP 时间戳验证**（原始字节回放，时间戳 21/54/88…；无需重写状态机）。
2. **部分完成：H.265 live TS**（hvcC/PMT 已接入；RTMP H.265 尚未生成 sprop fmtp，实际仍回退 fMP4）。
3. **已完成：发布端丢帧日志 + 独立分段标记 + VOD 临时目录/路径校验**。
4. **待做：HLS live `EXT-X-PROGRAM-DATE-TIME`、可配置窗口、LL-HLS**。
5. **待做：VOD 源变更检测（mtime/size）、HEAD/Range；enhanced-RTMP AV1/VP9。**

## 验证命令

```bash
cargo test --all-features
cargo clippy --all-features --all-targets -- -D warnings

# 30fps 源
ffprobe -v error -select_streams v:0 \
  -show_entries packet=pts_time,dts_time \
  -of csv=p=0 segment.ts | head
```

预期：每段 30 帧、`EXTINF:1.000`、PTS 步进约 `0.033333`。
