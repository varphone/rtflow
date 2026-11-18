# WebRTC（WHIP/WHEP）

WebRTC 是当前延迟最低的输出/输入协议，适合局域网预览和嵌入式网关。
`webrtc` feature 默认不进入 `embedded-gateway`；需要时显式启用。

## 端点

```text
POST   /whip/{mount}         # WHIP 发布，body 为 SDP offer
PATCH  /whip/{session_id}    # trickle ICE 候选（application/trickle-ice-sdpfrag）
DELETE /whip/{session_id}
OPTIONS /whip/{mount}

POST   /whep/{mount}         # WHEP 播放，body 为 SDP offer
PATCH  /whep/{session_id}    # trickle ICE 候选
DELETE /whep/{session_id}
OPTIONS /whep/{mount}
```

WHEP 返回 `201`、`Location`、`ETag`、`Link: <stun/turn>; rel="ice-server"`、
`Access-Control-Expose-Headers`；
PATCH 使用 `If-Match`（可选）匹配 ETag。浏览器预检走 OPTIONS。

## Codec

默认只协商浏览器 offer 中出现的 codec：

| 类型 | 默认 | 说明 |
|---|---|---|
| H.264 | 是 | packetization-mode=1；fmtp 优先取源 SPS/fmtp |
| VP8/VP9 | 是 | 源和浏览器都支持时启用 |
| Opus | 是 | 48 kHz，`minptime=10;useinbandfec=1` |
| G.711 PCMA/PCMU | 是 | 8 kHz mono，直通 |
| H.265 | 否 | 非 WebRTC 标准，需显式实验开关 |
| AAC-LC | 否 | 浏览器支持不一致，需显式实验开关 |
| AV1 | 否 | 当前 RTP packetizer 不完整 |

不做转码；源 codec 不在浏览器 offer 中时 WHEP 返回 406，而不是添加一个
浏览器无法接收的 track。

## ICE / NAT

```toml
[webrtc]
enabled = true
ice_servers = ["stun:stun.example.org:3478", "turn:turn.example.org:3478?transport=udp"]
ice_username = "${TURN_USER}"
ice_credential = "${TURN_PASS}"
ice_transport_policy = "all"   # all | relay
```

- 空 `ice_servers` 只收集 host candidate，适合同机/局域网；
- 公网/跨 NAT 至少配置 STUN，对称 NAT 需要 TURN；
- `relay` 强制走 TURN（调试/合规场景）；
- `turns:` 需要 TURN TLS 服务。

当前 WHIP/WHEP 的 answer 会带 gathered candidate；PATCH 支持后续 trickle。
PATCH 也支持携带完整 SDP offer 进行 ICE restart/renegotiation；响应为
`200 application/sdp`。

## 延迟

端到端延迟由 VENC、服务端转发、网络、浏览器 jitter buffer 和解码/显示组成。
本机 x86 基准（640×360@30，H.264，视频-only，Live 源含软件 x264 编码）：

- 640×360：p50 ≈36 ms，p95 ≈52 ms；
- 1280×720：p50 ≈77 ms，p95 ≈145 ms；
- 1920×1080：软件 x264 单线程 p50 ≈73 ms / p95 ≈188 ms；4 线程一次 p95≈148 ms，复测又出现 228 ms，波动大，不能作为稳定达标；VAAPI p50≈126/p95≈178；
- 波动主要来自 Python 源生成/x264 队列和系统调度，不是 WebRTC 本身；
- 服务端 ingest→packetize→SRTP 通常 **<10 ms**；
- localhost 结果不含 HI3516 硬件 VENC 和真实网络，不能直接当作产品端到端承诺；
- 1080p 本机 p95 未达 <150 ms，需硬件 VENC 板上验证。

目标：

- LAN、Chrome、720p30：**p95 < 150 ms**，争取 100 ms；
- 需要 `zerolatency`、无 B 帧、GOP 0.5–1 s、host ICE、客户端低 jitter buffer；
- WHEP 启动只回放最近 150 ms、关键帧对齐的窗口，不在加入时回放整段缓存；
- 视频-only 比带音频更容易压低延迟；A/V 同步会增加视频侧等待。

## 已验证项

- Chromium 143 通过 WHEP 播放 H.264（含音轨时也正常）；控制台“预览 WebRTC”可播放；
- 带 `stun:stun.l.google.com:19302` 时 answer 同时包含 host 和 `typ srflx` candidate；
- PATCH 完整 SDP offer 可完成 ICE restart/renegotiation；
- WHEP 侧监听浏览器 PLI/FIR，并转发为源端 IDR 请求（丢包恢复）；
- WHEP 声明并处理 NACK/PLI/FIR，最近 256 个 RTP 包缓存用于重传；
- WHEP 每秒发送 RTCP Sender Report（NTP/RTP 映射、packet/octet 计数），用于 A/V 同步与浏览器统计；
- 本机基准方法：Python 实时生成 640×360 原始帧，帧顶栏编码毫秒值，x264
  `zerolatency` 编码后 RTMP 推流；Chromium WHEP 播放，canvas 解码每帧毫秒值并与
  本地时钟相减，取 120 帧分位。该结果包含源生成/x264/RTMP/服务端/WebRTC/浏览器，
  不含 HI3516 硬件 VENC 与真实网络。

## 指标

`/metrics` 暴露：

- `rtflow_webrtc_sessions_total`
- `rtflow_webrtc_sessions_active`

会话创建/回收时更新，可用于观察 WebRTC 泄漏和容量。

## 低内存/嵌入式

- 默认 `embedded-gateway` 不包含 WebRTC；启用后新增 `webrtc`/DTLS/SRTP 依赖；
- 每个会话有独立 PeerConnection/ICE socket，建议按内存设置 `max_sessions`；
- 可用上限：`max_sessions`、`max_sessions_per_mount`、`max_tracks`、`max_frame_bytes`、
  `max_ice_candidates`、`session_timeout`、`session_max_lifetime`；
- 本机实测（RTMP/HLS 基线约 46 MB）：第 1 路 640×360 WHEP +约 3 MB，
  4 路累计约 +3.5 MB（分配器复用，约 1 MB/路增量）；45 MB 高水位主要是
  RTMP/HLS 缓冲，不应全部算成 WebRTC 会话开销；
- 4 路 WHEP 服务端增量可控制在约 4 MB 内，满足 ≤3 MB/会话的初始目标；
  嵌入式预算仍应按目标板实测；
- 不要同时启用不用的 codec/平台；H.265/AAC 默认关闭；
- 具体 RSS 以目标板实测为准；本机不提供嵌入式内存承诺。

## 浏览器要求

- `getUserMedia`（WHIP 发布）要求 HTTPS 或 `localhost`；
- 浏览器必须支持源 codec；Chrome 对 H.264/Opus 最稳；
- 播放端可设置 `RTCRtpReceiver.playoutDelayHint` / `jitterBufferTarget`（控制台播放器已设置 0）降低缓冲，但浏览器可能忽略；
- 当前 WebRTC 仍标记为实验性，生产前按目标浏览器做互操作验证。

## 限制

- 不做 SFU/多人、simulcast/SVC、DataChannel、转码；
- 不做 TURN 服务器；
- 拥塞控制（TWCC/REMB 决策）、TURN relay 实测仍在完善中。
