# RTSP 能力与兼容矩阵

本文是 RTSP 子系统的实现口径清单：方法/状态码、传输、码流、feature 组合与
资源上限。基线版本：0.3.0（`src/rtsp` + `src/server` + `src/client`）。

## 1. 方法与状态码

| 方法 | 成功 | 主要错误 | 说明 |
|---|---|---|---|
| OPTIONS | 200 | 400/401 | 任意 URI（含 `*`）；返回 `Public` |
| DESCRIBE | 200 + SDP | 404/503 | 绝对 `Content-Base`；支持 IPv4/IPv6 `c=` 与 multicast |
| ANNOUNCE | 200 | 400/409/455 | 校验 SDP 与 mount 描述一致；支持 `a=crypto` |
| SETUP | 200 + Session/Transport | 404/453/454/455/459/461 | UDP、TCP interleaved、`rtcp-mux`、multicast、`RTP/SAVP` |
| PLAY | 200 + RTP-Info | 454/455/457/551 | `Range: npt=`；Scale/Speed 仅 1.0，其他 551 |
| PAUSE | 200 | 454/455 | 保留 NPT 游标 |
| RECORD | 200 | 409/454/455 | 发布者认领与接收任务 |
| TEARDOWN | 200 | 454 | 释放全部会话资源并关闭连接 |
| GET_PARAMETER | 200 | 454 | 保活；允许无 Session 的能力查询 |
| SET_PARAMETER | 200 | 400 | 接受并忽略参数（未知方法仍 501） |
| 未知方法 | 501 | — | `Method::Unsupported` |
| `Require`/`Proxy-Require` | — | 551 + `Unsupported` | RFC 2326 §12.32 |

通用错误：非法 CSeq/消息 400、鉴权失败 401/403、会话不匹配 454、状态不允许 455、
不支持的传输 461、非法 Range 457、聚合操作 459、带宽/轨道上限 453。

## 2. 传输与安全

| Transport | 状态 | 备注 |
|---|---|---|
| `RTP/AVP;unicast;client_port=a-b` | 支持 | UDP 独立 RTP/RTCP 端口 |
| `RTP/AVP;unicast;client_port=a;rtcp-mux` | 支持 | 单端口，RTP/RTCP 按 PT 192..=223 分流 |
| `RTP/AVP/TCP;interleaved=a-b` | 支持 | TCP interleaved |
| `RTP/AVP/TCP;interleaved=a;rtcp-mux` | 支持 | 单 channel |
| `RTP/AVP;multicast` | 支持（IPv4） | SDP `c=` + `m=` 端口；mount 级共享 sender |
| `RTP/SAVP;unicast` + `a=crypto` | 支持 | AES_128_CM_HMAC_SHA1_80；SRTP + SRTCP |
| `RTP/SAVP/TCP` | 支持 | TCP interleaved 上的 SRTP/SRTCP |
| 组播 + `RTP/SAVP` | 支持 | 组播 SRTP，客户端解密 |
| RTSPS | 支持 | TLS（`tls` feature） |

## 3. 码流矩阵

| Codec | RTP 打包 | SDP 名称 | 容器/输出 |
|---|---|---|---|
| H.264 | RFC 6184（FU-A/STAP-A） | `H264/90000` | RTSP/FLV/HLS/MP4/TS |
| H.265 | RFC 7798（FU/AP） | `H265/90000` | RTSP/FLV/HLS/MP4/TS |
| VP8 | RFC 7741 | `VP8/90000` | RTSP |
| VP9 | draft-ietf-payload-vp9 | `VP9/90000` | RTSP |
| AAC | RFC 3640（AU-header） | `MPEG4-GENERIC` | RTSP/FLV/HLS/MP4/TS |
| G.711 A/μ | RFC 3551 | `PCMA`/`PCMU` | RTSP/FLV/HLS/TS |
| Opus | RFC 7587 透传 | `opus/48000/2` | RTSP/HLS |
| MP3 | RFC 2250/5219（MPA） | `MPA/90000`、PT 14 | RTSP |
| G.726 16/24/32/40 | RFC 3551 | `G726-xx/8000` | RTSP |
| AV1 | draft-ietf-payload-av1 | `AV1/90000` | RTSP |
| JPEG | RFC 2435 | `JPEG/90000` | RTSP |
| H.263 | RFC 4629 | `H263-1998/90000`、PT 34 | RTSP |
| MPEG-4 Part 2 | RFC 3016 | `MP4V-ES/90000` | RTSP |

明确不支持：MPEG-2 Audio（MP3 除外）、RTSP 2.0（RFC 7826）。
AV1/JPEG/H.263/MPEG-4 Part 2 目前提供基础打包/解包与 SDP 映射，未做
profile/level 级 fmtp 校验与浏览器/ffmpeg 互操作矩阵。

## 4. Feature 组合

| Feature | 内容 | 是否在 `full` |
|---|---|---|
| `full` | TLS、MP4/MKV、client/server、file-storage、metrics、HTTP/HLS/WS-FLV | 是（默认） |
| `srtp` | RTSP `RTP/SAVP` + a=crypto | 否，显式启用 |
| `multicast` | RTSP 组播发布/接收 | 否，显式启用 |
| `test-source` | `rtflow::source::SyntheticSource` | 否，显式启用 |
| `experimental-protocols` | DASH/RTMP/SRT/WebRTC | 否 |
| `embedded-gateway` | RTSP + TLS + metrics + HTTP-FLV/HLS | 否（profile） |

## 5. 资源上限与实测

默认 `ServerConfig`：header 16 KiB、body 256 KiB、256 会话、16 track/会话、
interleaved 64 KiB、消费者队列 256 帧、启动缓存 4 MiB/track（mount 总计 16 MiB）、
UDP 接收缓冲 4 MiB/socket（rtcp-mux 与普通路径一致）。

`ServerConfig::embedded`：UDP 256 KiB、32 会话、8 track/会话、64 帧队列、
启动缓存 512 KiB/mount。

实测（Linux x86_64，release，UDP，4 会话 × 2 track，30fps 16 KiB 视频）：

| 配置 | 基线 VmRSS | 稳态 VmRSS | Δ（4 会话） |
|---|---|---|---|
| `embedded`（queue 64） | 4.3 MiB | 7.9–8.2 MiB | 3.5–3.8 MiB |
| 默认（queue 128） | 4.3 MiB | 8.2–8.4 MiB | 3.9–4.1 MiB |

均低于 24 MiB 目标（不含内核 socket 缓冲）。包化路径实测 **0.29 次分配/RTP 包**
（20 KB H.264 分片：整帧单缓冲切片 + 复用 `encode_into` 缓冲），预算 ≤1。

## 6. 真实工具互操作（2026-11 实测）

| codec | ffmpeg 推流 → rtflow → ffmpeg 拉流 | GStreamer 拉流 | 备注 |
|---|---|---|---|
| MPEG-4 Part 2 | **通过** | 通过（rtpmp4vdepay/avdec_mpeg4） | 首帧关键帧识别已修正 |
| JPEG | **通过**（RTP/JPEG Q=80、块尺寸与分片偏移修正后） | 通过（rtpjpegdepay/jpegdec） | ffmpeg/GStreamer 均通过 |
| H.263 | **通过**（H263-2000 推流，H263/PT34 拉流） | **通过**（rtph263depay/avdec_h263） | 出口按 RFC 2190 mode A 打包，入口兼容 RFC 4629 |
| AV1 | **通过**（FFmpeg master/8.x，`-strict -2`） | 不可测：GStreamer 1.24 无 `rtpav1pay/depay` | AV1 RTP v1.0 聚合/OBU 重组已实现 |
| H.264/AAC | 既有测试通过 | 既有测试通过 | — |

VLC 3.0（snap）可以连接并订阅 rtflow 的 RTSP mount，但在 `-I dummy`
+ 无显示环境下 H.264 解码出现 `buffer deadlock prevented`，未作为自动化断言。

AV1 真实互操作使用用户目录的 FFmpeg master（2026-11）验证：
`FFMPEG=~/.local/opt/ffmpeg/ffmpeg cargo test --test interop ffmpeg_round_trips_av1`。
测试会检测 FFmpeg 是否支持 AV1 RTP；系统 FFmpeg 6.1 无该能力时自动跳过。

## 7. 已确认的验证结果（2026-11）

- `cargo test --no-fail-fast`：360 passed / 0 failed。
- `cargo test --features experimental-protocols`：427 passed / 0 failed。
- fuzz（libFuzzer，600 s/目标，无崩溃）：
  - `sdp`：13,931,778 次执行
  - `transport`：53,815,772 次执行
  - `rtsp_message`：22,706,631 次执行
- 30 分钟会话翻腾浸泡（8 客户端/轮，UDP+TCP 交替）：通过，无 panic/forced task。
- 包化分配：0.29 次/RTP 包；嵌入式内存：4 会话 × 2 track 稳态 ≈8 MiB。

## 8. 已知未实现（明确边界）

| 项 | 现状 |
|---|---|
| RTSP 2.0（RFC 7826） | 不支持 |
| AV1/JPEG/H.263/MPEG-4 的 fmtp/profile 校验 | fmtp 透传，未做 profile/level 校验 |
| 组播 SRTP 的 SRTCP 报告 | 客户端发送 SRTCP RR（组 RTCP 端口） |

## 9. 验证入口

```bash
cargo test --no-fail-fast
cargo test --features experimental-protocols
cargo test --features test-source
cargo test --test alloc
cargo test --test system soak -- --ignored   # RTSP_SOAK_SECONDS=120
cargo +nightly fuzz run sdp -- -max_total_time=10
cargo +nightly fuzz run transport -- -max_total_time=10
cargo +nightly fuzz run rtsp_message -- -max_total_time=10
```
