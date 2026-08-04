# rtflow 功能完整性审计报告

- 日期：2026-08（基于当前工作区，HEAD `8a6eef3`，前两轮性能/可靠性、实时性/兼容性修复已全部落地）
- 范围：`src/` 全部模块、`examples/`、`docs/`（README、architecture、examples、file-streaming）
- 方法：4 路只读子代理分组审查（文档承诺 vs 实现、服务端、输出协议、输入源与支撑设施）+ 高严重性条目逐条人工抽查复核 `file:line`
- 口径：功能完整性 = 承诺的功能是否真实可用、配置/指标/API 是否接线、常见播放器/工具链依赖的功能是否齐备；已修复问题视为基线不复检
- **修复状态（2026-08 第四轮）：40 条发现已修复 34 条；6 条经评估降级（原因见各条备注）**

## 严重性统计（去重后 40 条）

| 严重性 | 数量 | 类别 |
|---|---|---|
| 高（功能不可用/误导/恒 0） | 9 | GStreamer 错标、metrics 死指标、ffmpeg 空占位、srtp todo!、DASH 双轨、token 断播、RTMP play 无鉴权、HLS/DASH 无 VOD、webrtc 示例误导 |
| 中（可用但缺口明显） | 16 | RTSP 方法空洞、405 挂起、文件源解析缺口、VOD seek、安全限制、publish 无鉴权等 |
| 低（细节/文档口径） | 15 | 示例偏差、便利性缺口、边界行为 |

---

# 一、高严重性

**F1. GStreamer 源配置 H.265 时静默构建 H.264 编码链，SDP 却宣称 H.265（媒体与声明不符，下游无法解码）**
- `src/gst/pipeline.rs:96-110`：编码器分支全部为 `vpuenc_h264`/`v4l2h264enc`/`nvh264enc`/`vaapih264enc`/`x264enc`，从不检查 `enc.codec`；`src/gst/mod.rs:99-106` 的 `TrackDescription.codec = enc.codec`（用户配置 H.265 时 SDP 写 H265，实际流出 H.264 流）。修复：编码器按 `enc.codec` 选择（H.265 → x265enc/v4l2h265enc 等），或在构建时校验并报错。

**F2. metrics 大量指标为死代码、恒为 0（导出存在但无任何写入点）**
- `src/metrics/mod.rs:43-50`：`rtp_packets_sent`、`rtp_octets_sent`、`rtp_packets_received`、`rtcp_loss_ratio`、`rtcp_jitter_ms`、`rtcp_rtt_ms`——全仓库无 `fetch_add`/写入点（导出见 413-430、543-560）。
- `src/metrics/mod.rs:60-76`：`http_requests_total`、`hls_segments_served`、`dash_segments_served`、`flv_streams_active`、`srt_connections_total`、`srt_streams_active`、`webrtc_sessions_total`、`webrtc_sessions_active`、`rtmp_publishers_active`、`rtmp_players_active` 同样无写入点（唯一例外 `rtmp_connections_total`，`src/rtmp/mod.rs:168`）。
- 后果：Prometheus 导出中"每协议流量/质量"全部恒 0，运维监控失真。修复：在 `mount.stats`（帧计数已接线）、各协议 session 建立/销毁、HTTP handler 处补计数。

**F3. `ffmpeg` feature 为空占位（启用无任何功能）**
- `Cargo.toml:32`：`ffmpeg = []`，src 下无任何 ffmpeg 模块；`examples/ffmpeg_live_publish.rs:53-62` 只是 spawn 外部 ffmpeg 进程。README/feature profile 未明确承诺 crate 内 FFmpeg 集成，但该 feature 存在即误导。修复：删除 feature 或在文档标注"仅示例依赖"。

**F4. 公开 API `SrtpContext::srtcp_context()` 为 `todo!()` panic 占位**
- `src/srtp/mod.rs:261-266`：带完整 doc 注释的公开方法直接 `todo!("SRTCP context creation requires RTCP session keys")`，调用即 panic；该类型在默认 `full` profile 编译（Cargo.toml:17 `srtp`）并从 crate 根导出（lib.rs:102）。修复：改为返回 `Result` + `Error::Unsupported`，或实现 RTCP 密钥派生。

**F5. DASH 音视频双轨共用同一 init/segment 文件，MPD 声明与 fMP4 内容不匹配**
- `src/dash/mpd.rs:131-200`：video 与 audio 两个 AdaptationSet 的 `SegmentTemplate` 都指向 `{prefix}/init.mp4` 与 `{prefix}/segment_$Number$.m4s`——而 segmenter（`src/hls/segmenter.rs` 多轨 TrackState）把音视频写进**同一个** fMP4。播放器按 AdaptationSet 声明的单轨 codecs 解析含双轨的 init/segment，行为未定义（多数播放器只取第一轨或报错）。修复：为每轨生成独立 init/segment 文件，或 MPD 只声明合并轨（单 AdaptationSet `audio`+`video` mimeType 混合）。

**F6. HTTP token 鉴权只认 query 参数，HLS/DASH/FLV 播放必然断播**
- `src/http/auth_middleware.rs:15-65`：token 仅从 `req.query[param_name]` 读取并绑定请求路径。播放器加载 playlist/MPD（带 token）后，playlist 内嵌的段/init URI 是相对路径（无 token）→ 段请求 403。修复：playlist/MPD 生成时把 token 注入内嵌 URI，或中间件同时支持 `Authorization: Bearer`。

**F7. RTMP play 无任何鉴权（publish 有鉴权，拉流裸奔）**
- `src/rtmp/session.rs:667-696`：Play 分支仅 `mounts.get_handle` + StreamNotFound，无 `authorize_*` 调用；`StreamKeyAuthenticator`/`PublishAuthenticator` 只保护 publish（session.rs:550-557）。修复：play 路径按 app/stream 校验（可复用 `RtmpConfig.authenticator`）。

**F8. HLS/DASH 无 VOD/录制目录输出；`HlsConfig.live=false` 语义不成立**
- `src/hls/mod.rs:40-42`：`live=false` 只改 playlist 输出 `EXT-X-PLAYLIST-TYPE:VOD`+ENDLIST（playlist.rs:81-117），底层 `SegmentRing` 仍是 `segment_count` 滑动窗口（hls/mod.rs:40）——VOD playlist 声明了完整列表，但旧段被淘汰后 404（hls/handler.rs:40-47）。录制目录无 HLS/DASH 挂载路径。修复：VOD 模式段持久化或禁止 live=false 组合并报错。

**F9. webrtc_server 示例与 README「不可用占位」声明直接矛盾**
- README:23 明确 WHIP/WHEP 为不可用占位；`examples/webrtc_server.rs:226-260` 仍注册 `/whip*`、`/whep*` 路由并提供完整 player/publisher HTML 引导浏览器接入。且 `examples/webrtc_server.rs:230` 创建**独立** `MountRegistry::new(10)`，与 212 行 RTSP server 的 registry 不共享——即使未来补上媒体管线，WHIP/WHEP 与 RTSP mount 也不互通。修复：删除示例路由/HTML 或标注不可用；registry 与 RTSP 共享。

---

# 二、中严重性

**S1. GET_PARAMETER 两处空洞**（`src/server/session.rs:219-236`、1175-1186）
- 无 `Session:` 头的服务器级 GET_PARAMETER 返回 454（RFC 2326 允许无会话调用）；有会话时恒回 200 空 body，无任何参数查询能力（不返回服务器信息/会话状态）。OPTIONS 的 `Public` 头（1108-1113）声明了 GET_PARAMETER，功能空洞。

**S2. SETUP 到错误 control URI 直接断连而非回 4xx**
- `src/server/session.rs:446,505`：`track_for_uri(...)?` 错误沿 `serve_connection` 传播，连接关闭，客户端收不到 404/461。严格客户端会视为网络故障而非协议错误。

**S3. HTTP 405 响应缺 Content-Length 且无 Connection: close，客户端挂起**
- `src/http/router.rs:48-54`：`write_headers(405, &[("Allow", ...)])` 后 `write_raw` 写 body——响应无 Content-Length、无 Connection: close（`src/http/response.rs:24-43` 只写头，`connection_close` 仅当显式传入 close 头），HTTP/1.1 客户端将等待 body 结束直到超时。且 `Allow` 声明 `HEAD` 但 HEAD 未实现（任何方法→405 路径同样挂起）。

**S4. HTTP 连接/IP 限流超限时静默 drop，无 429 响应**
- `src/http/mod.rs:97-104`：Semaphore 获取失败直接 `continue`（关闭连接），客户端无法区分限流与网络故障。

**S5. MP4 edit list（elst）未处理**
- `src/file/mp4.rs` 全文无 elst 逻辑；mp4box 的样本提取（`extract_track_samples`）忽略 `edts/elst`。带初始 delay/empty edit 的 MOV/MP4（ffmpeg 常见输出）seek 位置与展示时长偏移。

**S6. MPEG-TS 与裸流（H.264/H.265/ADTS）无时长估计**
- `src/file/mpeg_ts.rs:310-312`、`src/file/raw.rs:227-229,395-397`：`duration()` 恒 `None`——无进度估计，播放器 seek 条不可用。

**S7. 裸流 H.264/H.265 无 SPS/PPS 解析、无参数集注入、fmtp 恒 None**
- `src/file/raw.rs:151`：`track_description(0, codec, 90_000, None, None)`。起播/seek 到流中部时播放器在收到含参数集的 AU 前无法解码；SDP 缺 profile-level-id。对比 MP4/MKV 的 AVCC/HVCC 解析齐全（`src/file/bitstream.rs:21-107`）。

**S8. 多音轨文件无选择 API**
- `FileSourceOptions`（`src/file/source.rs:50-63`）无音轨筛选；MP4/MKV/TS 解析出的**全部**音轨进入 MediaDescription，客户端被迫全部 SETUP。

**S9. MPEG-TS 与裸流 seek 为线性扫描，Matroska seek 每次重开文件**
- `src/file/mpeg_ts.rs:319-328`（reset 后逐样本扫到目标）、`src/file/raw.rs:238-247,404-413`（同）；`src/file/matroska.rs:206-212`（reopen）+228-245（Cue seek）。长文件 seek 慢。

**S10. RecordingDemuxer::seek 不定位关键帧**
- `src/storage/recording.rs:528-545`：线性推进到 `pts >= position` 的第一个样本，不检查 `is_keyframe`；发送端 awaiting_key 丢弃至下一关键帧，时间轴从请求位置起算（表现可接受，但非对齐 seek 且 O(段内样本数)）。

**S11. README「磁盘容量保留」名不副实：只有事后裁剪，无写前预留**
- `src/storage/recording.rs:640-669`：prune 为"滚动删除到 ≤ max_disk_bytes/max_segments"；全库无 reserve/prealloc/falloc。磁盘满时靠写失败丢弃段兜底（recording.rs:239-250）。

**S12. GStreamer `pull_mode` 为死标记**
- `src/gst/mod.rs:28-32`（`_mode` + `#[allow(dead_code)]`）、`src/gst/app_src.rs:10-87`：`PullController` 存在但无内置管线组装，无任何构建路径接入。

**S13. Digest 鉴权仅支持 MD5，无 RFC 7616 SHA-256**
- `src/protocol/auth.rs:291-296`（客户端遇非 MD5 直接报错）、351-362（服务端 verify_digest 硬编码 MD5）、server/session.rs:1041（挑战固定 algorithm=MD5）。SHA-256 是主流播放器/工具默认选项。

**S14. TLS 无 mTLS（服务端/客户端均固定 `with_no_client_auth`）**
- `src/security/tls.rs:29,68`。网关场景（设备证书认证）不可用。

**S15. SRT publish 无任何鉴权/ACL**
- `src/srt/mod.rs:570-586`：stream_id 直接映射 mount path 并注册 Publisher，任何人可推流（实验协议，但与 RTMP publish 的鉴权不一致）。

**S16. RTMP play 命令集缺口：deleteStream/closeStream 不停止播放、getStreamLength 返回 null、无 receiveVideo/receiveAudio 处理**
- `src/rtmp/session.rs:896-899`（deleteStream 仅空响应，播放持续到 TCP 断开）、903-913（getStreamLength 返回 null_result）。播放器停止后带宽持续占用。

**S17. 客户端 UDP 接收不校验来源地址/SSRC/PayloadType**
- `src/client/setup.rs:373-401`：`recv_from` 后直接 `RtpPacket::parse`，任意来源可注入伪造 RTP（服务端发布端有严格校验，客户端侧没有）。

**S18. 任务监督覆盖面与 README 表述不符**
- README:14「统一任务监督和有期限 shutdown」；FileSource（file/source.rs:183）、GStreamer（gst/mod.rs:258）、客户端（client/mod.rs:451）、RTMP（rtmp/mod.rs:163）、HTTP（http/mod.rs:91）、HLS（hls/mod.rs:59）、DASH（dash/mod.rs:94）、metrics exporter（metrics/mod.rs:321,331）等大量后台任务为裸 `tokio::spawn`，各自 cancel 但不进入 supervisor 的 shutdown 报告/有期限等待。

---

# 三、低严重性

- **L1** `examples/webrtc_server.rs` 用独立 MountRegistry（见 F9 佐证）。
- **L2** `examples/authenticated_server.rs:47` Digest nonce 硬编码 `"example-static-nonce"`（用 `verify_digest` 而非 `ManagedDigestAuthenticator`），README 声称的签名 nonce/防重放未在示例演示。
- **L3** `ServerConfigBuilder` 缺 `max_rtsp_header_bytes`/`max_rtsp_body_bytes` setter（字段公开可直赋，便利性缺口）。
- **L4** multicast feature 在默认 `full` 中启用且实现真实（src/multicast/mod.rs:54-70），但 docs/architecture.md:51 称组播"仍是独立扩展"，README 未提及——文档口径偏差。
- **L5** 实验协议 CLI 未暴露配置：`examples/hls_server.rs:78` 硬编码 `HlsConfig.live=true`；rtmp_server 无 `RtmpSessionConfig` CLI 开关。
- **L6** `Gateway::register_local_mount`（src/gateway.rs:148-195）手工构造 Mount 内部字段，与 `RtspServer::register_mount` 两套实现，字段演进存在漂移风险（audit-report.md R24 跳过项）。
- **L7** HTTP 请求行/头非法时直接断连无 400（src/http/mod.rs:184-190）。
- **L8** HLS playlist 响应无 `Access-Control-Allow-Origin`（hls/handler.rs:20-33），segment 响应有（55 行）——跨域部署时 playlist 请求失败。
- **L9** HLS 无 ETag（segment 有 Cache-Control: public, max-age=3600 但无校验头）；live playlist 无 `PROGRAM-DATE-TIME`。
- **L10** `MetricsExporter` 裸 HTTP 公开，无鉴权/TLS（src/metrics/mod.rs:300-358）。
- **L11** 客户端 interleaved 控制面写无超时包装（src/client/mod.rs:324）。
- **L12** WAV 仅接受 G.711 且 bits==8（src/file/raw.rs:656-669），PCM 16/24/32 位拒绝（README 承诺范围"G.711 WAV"，不算违约）。
- **L13** RTMP play 流程缺失事件：无 `NetStream.Play.Stop`/`UnpublishNotify`；onMetaData duration 恒 0（command.rs:282-302）。
- **L14** DASH MPD `availabilityStartTime` 在发布者重连（mount 重建）后重置为新时刻（AST 重置），时移窗口对播放器表现为时间轴跳变。
- **L15** HTTP/1.0 客户端被拒绝（仅支持 HTTP/1.1 格式解析）。

---

# 四、文档/承诺偏差汇总

| README/docs 表述 | 实际 | 严重性 |
|---|---|---|
| 「磁盘容量保留」 | 事后滚动删除，无写前预留 | 中 |
| 「统一任务监督和有期限 shutdown」 | 核心任务经 supervisor；大量后台任务裸 spawn | 中 |
| 「Basic/Digest、签名 nonce 和防重放」 | 真实实现（auth.rs:97-213），但仅 MD5；示例未演示 | 低-中 |
| 「文件源：MP4/MOV/M4A、Matroska、MPEG-TS、WAV、裸流」 | 全部可用，但 TS/裸流无时长、裸流无参数集、elst 未处理 | 中 |
| WHIP/WHEP「不可用占位」 | 代码与示例仍在引导接入（README 正确，示例违背） | 高 |

---

# 五、Top 修复建议（优先级）

1. **metrics 死指标接线**（F2）——最低成本、最高观测收益：RTP/RTCP 计数挂在 `mount.stats` 现有写入点，协议计数器挂在各 session 建立/销毁处。
2. **GStreamer H.265 错标**（F1）——构建时按 `enc.codec` 校验并报错，或补 H.265 编码器分支。
3. **token 鉴权断播**（F6）——playlist/MPD 内嵌 token 或支持 Authorization 头。
4. **RTMP play 鉴权 + deleteStream 停止播放**（F7/F16）。
5. **HTTP 405 挂起**（S3）——补 Content-Length/Connection: close。
6. **DASH 双轨**（F5）——每轨独立文件或 MPD 单 AdaptationSet 合并声明。
7. **srtp todo!**（F4）——返回 `Error::Unsupported` 而非 panic。
8. **HLS VOD 语义**（F8）——live=false 时段持久化或拒绝该组合。
