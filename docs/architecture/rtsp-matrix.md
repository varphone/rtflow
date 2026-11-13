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
| `RTP/SAVP/TCP` | 461 | TCP interleaved 上的 SRTP 未实现 |
| 组播 + SRTP | 不支持 | — |
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

明确不支持：AV1、JPEG、H.263、MPEG-4 Part 2、MPEG-2 Audio（MP3 除外）。

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
UDP 接收缓冲 4 MiB/socket。

`ServerConfig::embedded`：UDP 256 KiB、32 会话、8 track/会话、64 帧队列、
启动缓存 512 KiB/mount。

实测（Linux x86_64，release，UDP，4 会话 × 2 track，30fps 16 KiB 视频）：

| 配置 | 基线 VmRSS | 稳态 VmRSS | Δ（4 会话） |
|---|---|---|---|
| `embedded`（queue 64） | 4.3 MiB | 7.9–8.2 MiB | 3.5–3.8 MiB |
| 默认（queue 128） | 4.3 MiB | 8.2–8.4 MiB | 3.9–4.1 MiB |

均低于 24 MiB 目标（不含内核 socket 缓冲）。包化路径实测 **1.35 次分配/RTP 包**
（20 KB H.264 分片 + 复用 `encode_into` 缓冲），预算 ≤2。

## 6. 验证入口

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
