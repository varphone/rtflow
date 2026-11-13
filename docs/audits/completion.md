# 第一期完成性验收矩阵

| 原始要求 | 实现证据 | 验证证据 |
|---|---|---|
| Rust RTSP 客户端与服务端 | `client/`、`server/`、`protocol/rtsp.rs` | `tcp_play`、`udp_play`、`session_state` |
| 拉流转发代理 | `server/proxy_runtime.rs`、`proxy/`，自动 SDP 探测、退避和空闲停拉 | `proxy_play`、`proxy_reconnect` |
| 多轨及 UDP/TCP interleaved | 每轨 SETUP、独立 RTP/RTCP channel、UDP socket pair | `multitrack`、`session_reliability` |
| 外部发布服务器 | UDP/TCP RECORD、发布者原子 claim 与替换策略 | `publish_udp`、`publish_tcp` |
| H264/H265/AAC/G711 | `rtp/payload.rs`、`protocol/sdp.rs` | FU/AP/STAP、多 NAL、RFC 3640 AU-header、PCMA/PCMU 单元测试 |
| 主流文件流 | `file/` 的 Demuxer/FileSource、阻塞 worker、有界预取、节流/seek/循环；MP4/MOV、MKV、TS/M2TS、WAV 与裸流 | `file_stream` 真实 FFmpeg 格式矩阵、RTSP 客户端、GStreamer、Pipeline 与取消测试 |
| live555/GStreamer 式管线 | `media/pipeline.rs` 的 Element、状态迁移、DAG、独立 Link 队列、溢出策略 | `pipeline::tests` 的环检测、队列丢弃、状态和 Bytes 存储复用 |
| 实时性与零拷贝 | `Bytes` payload、RTP slice、interleaved `split_to`、有界队列 | pipeline 指针复用测试、transport/RTP 单元测试 |
| 长期可靠性 | GET_PARAMETER、任务 CancellationToken/RAII、jitter timer、受损 AU 丢弃、可等待 shutdown | `session_reliability`、`shutdown`、RTP loss 测试 |
| RTCP 闭环 | 正确 NTP epoch、UDP/TCP SR/RR 与统计 | `udp_play`、`tcp_play`、NTP 精确测试 |
| 鉴权 | 可插拔 Authenticator、Basic、Digest MD5/qop=auth，覆盖所有方法 | `authentication`、auth 单元测试 |
| 真实外部互操作 | 标准 SDP Content-Base/control URI、静态 G711 PT、标准 TCP framing | `gstreamer_interop` 实际运行 `rtspsrc ! rtph264depay ! fakesink` |
| 故障与资源边界 | header/body/frame/队列限制、Session 状态、代理重连、shutdown grace | `session_state`、`proxy_reconnect`、`resource_limits`、`resource_exhaustion`、shutdown 测试 |
| 混合传输压力 | 有界广播、每客户端输出队列、UDP/TCP 同时 fan-out | `stress`：16 客户端、1000 帧并验证全部收帧与资源归零 |
| 编码与传输矩阵 | H264/H265/AAC/PCMA/PCMU 经 UDP/TCP 完整服务链路 | `codec_matrix`、`fragmentation_integration`，并验证 PAUSE/PLAY 与大 AU 分片重组 |
| 代理传输矩阵 | 上游 UDP/TCP 与下游 UDP/TCP 四种组合、双轨转发 | `proxy_transport_matrix` |
| 确定性文件夹具 | 提交小型 MP4/MOV/fMP4/MKV/TS/M2TS/WAV/裸流及哈希清单 | `fixture_files` 默认必跑；再生成脚本可复现 |
| 外部发布互操作 | FFmpeg ANNOUNCE/SETUP/RECORD TCP/UDP 发布，GStreamer 五编码 TCP/UDP 拉流 | ignored 外部测试由 `media-interop` CI 强制执行 |
| 可运行示例 | 本地源、文件源、客户端、代理、发布、鉴权、Pipeline 七类 CLI | `cargo build --examples` 与 `docs/guides/examples.md` |


当前文件 mount 为多客户端共享时间线，不包含 RTSP Range/per-session 独立 seek。第一期明确不包含 TLS、SRTP、组播、转码、ONVIF、录像索引和完整 RTCP 拥塞控制；这些是独立后续功能。
