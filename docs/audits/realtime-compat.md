# rtflow 实时性与兼容性审计报告

- 日期：2026-07（基于当前工作区，含已提交的 15 个性能/可靠性修复 commit）
- 范围：`src/` 全部模块、`tests/`、`examples/`、`docs/`
- 方法：多路只读子代理分组审查 + 关键发现人工抽查复核 `file:line`；与《docs/audits/code.md》修复状态交叉，已修复问题（如 segmenter 时间回绕强制切段、纯音频切段）视为基线并评估其新副作用
- 参考基线：README 声明端到端延迟目标 1-3 秒（GOP ≤ 分段时长）；`dash/rtmp/srt/webrtc` 为实验性占位协议（README:23），本报告对它们的问题按"实验功能"标注但仍列出

## 严重性统计（去重后）

| 类别 | 高 | 中 | 低 | 合计 |
|---|---|---|---|---|
| 实时性（延迟/抖动/起播） | 6 | 10 | 9 | 25 |
| 兼容性（协议合规/互操作） | 6 | 19 | 14 | 39 |
| **合计** | **12** | **29** | **23** | **64** |

## 修复状态（2026-07 第二轮修复后）

本轮按审计结果实施修复（未新增 feature 门控，实验协议一并修）。64 条中 **修复 46 条**、**核对后无需改动 10 条**、**结构性/设计取舍跳过 6 条**、**降级声明处理 2 条（WebRTC 系列）**：

- 实时性 25 条：修复 19（startup 窗口 2s+pacing、广播队列 256、TSBPD 发送端时间戳调度、interleaved 写 10s 超时、keepalive 2s 上限、SETUP 预热代理、jitter 40ms、RTMP nodelay、重连抖动、周期 RR、biased select、SR 64 位 NTP 等），核对无需改 4（keepalive 抢占 ms 级、RTCP 5s 合规、RT-12/RT-18/RT-25 分析后保留），跳过 2（低价值：jitter 事件驱动、FLV flush 聚合）。
- 兼容性 39 条：修复 27（RTMP seq header 传递/播放先发、Playing 禁 idle、音频倍率、FLV dts、releaseStream 响应、fmt0 delta、onMetaData、SRT ACK/握手/NAK/ACK 字段、SDP c=/Content-Base/control 重写/会话级 control、HLS 空 playlist/MEDIA-SEQUENCE/DISCONTINUITY-SEQUENCE、DASH hvc1/AST、SSRC 随机、CNAME、BYE、通道号溢出、GStreamer fmtp、G.711 rate 位、AAC seq header 兜底等），核对无需改 8（CORS、相对 URI、窗口保留、GET_PARAMETER、SETUP 4xx、reason-phrase、重复头、RTMP ms 回绕），降级声明 2（WebRTC 媒体管线系列→README 明确不可用占位），跳过 2（SETUP 去重、u8 track 溢出等低风险细节）。
- 时间戳边界并入上两类的 8 条：修复 6（RtpNtpClock 64 位扩展、pts 回绕扩展、tfdt 舍入、jitter transit 参考系、跨轨回绕阈值、H.265 CTS），核对无需改 2（SR 未做 A/V 校准属设计取舍、RTMP u32 ms 回绕属 FLV 规范允许）。
- 附带发现并修复的产品 bug：**Dechunker 的 SetChunkSize 未在解析流内生效**（`src/rtmp/chunk.rs`）——解析结果依赖 TCP 分段，RTMP 服务器开启 TCP_NODELAY 后 `_result` 同批到达即截断（影响所有 RTMP 客户端互操作）；`poll_messages` 现按 RTMP 规范即时应用新 chunk size。

WebRTC 系列（CP-3/CP-28/CP-29 等 7 条）：按用户决策降级处理——README 已明确 **WHIP/WHEP 为不可用占位**（信令可完成但无媒体管线），不实施误导性的半吊子信令修复。

---

# 一、实时性问题

## 高

### RT-1. startup 快照无 pacing 回放：新订阅者从最近关键帧"倒放"（8.5s+ 旧数据 burst）
- **位置**：`src/mount.rs:237-275`（快照生成）、`src/proxy/continuity.rs:156-164,203-251`（缓存 4MB/256 帧）、`src/server/tasks.rs:86-88,267-271`（startup 背靠背发送）
- **问题**：`KeyframeCache` 默认缓存最近关键帧起全部帧（256 帧/4MB），新订阅者一次性取回后 sender 无 pacing 瞬间 burst 发出（30fps 下 256 帧 ≈ 8.5s 媒体）。播放器缓冲后从最旧帧渲染，后续实时帧按序到达无法追平——端到端显示延迟 = GOP 剩余 + 播放器缓冲，直接突破 1-3s 目标。
- **修复**：快照按媒体时间过滤（只保留最近 1-2s），发送时按帧间隔 pacing（`sleep_until(next_pts)`）。

### RT-2. broadcast 队列 1024/4096 帧无字节级背压：慢消费者恢复后回放数十秒旧帧
- **位置**：`src/mount.rs:234,594`（`channel(1024)`）、`src/server/mod.rs:71,546`（默认 `consumer_queue_capacity: 4096`）
- **问题**：消费端 TCP 写阻塞时 slot 累积最多 4096 帧 ≈ 136s（@30fps），无字节级限制；网络恢复后一次性 burst 排空，巨大延迟尖峰 + 时间戳跳变。
- **修复**：按字节预算的有界队列（超预算丢非关键帧/Lagged），或把容量降到与延迟预算匹配（64-128 帧）。

### RT-3. SRT TSBPD 用到达时刻而非 SRT 时间戳调度：网络抖动 1:1 透传，丢包固定等 240ms
- **位置**：`src/srt/tsbpd.rs:28,120,146`（`arrival_time: Instant::now()`、`age = now - arrival`、`delay = 120ms`）
- **问题**：TSBPD 应以数据包内 SRT 时间戳（发送端时钟）计算播放时刻；当前用本地到达时刻 + 固定 120ms 平移，公网抖动直接叠加到端到端延迟上。丢包后空洞要等 `age >= delay * 2`（240ms）才跳过，恢复时间固定 240ms。
- **修复**：解析数据包时间戳（64 位 µs），播放时刻 = 时间戳 + delay - 本地时钟偏移估计；gap 超时用时间戳差而非 2×delay。

### RT-4. HLS 段长 2-4s + 关键帧对齐：实际端到端延迟 2×GOP 量级
- **位置**：`src/hls/segmenter.rs:154-168`（关键帧对齐切段）、`segmenter.rs:12,20`（默认 2s）
- **问题**：段在"达到目标时长后的第一个关键帧"切出，实际段长 = 2s + GOP 剩余（1-2s 常见）≈ 3-4s；播放器还要缓冲 1-3 个段。README 自述 1-3s 目标在默认配置下不可达。
- **修复**：文档化配置指引（segment_duration ≈ GOP 或 1s），或提供"强制切段"模式（不等待关键帧、DISCONTINUITY 标记）。

### RT-5. interleaved(TCP) 发送路径每包互斥锁 + 无超时 write_all：慢客户端卡死整个连接发送链
- **位置**：`src/server/tasks.rs:110,154`（`writer.lock().await.write_all().await`）
- **问题**：音视频轨道 + RTCP 共用同一 `Arc<Mutex<ControlWriteHalf>>`；TCP 窗口满时 `write_all` 永久阻塞，持锁期间所有轨道与 RTCP 全停，广播 slot 累积（叠加 RT-2）。
- **修复**：单发送 task + mpsc 队列，或写操作加超时并在超时丢帧/断开。

### RT-6. SRT 单循环串行处理 + 10ms tick 量化：多连接 HOL 阻塞、数据包最多等 10ms
- **位置**：`src/srt/mod.rs:197-245`（`run()` 单循环 + `tick_interval 10ms`）、`mod.rs:290-330`（maintenance 逐连接串行）
- **问题**：所有连接的收包/投递在同一任务串行；maintenance（ACK/NAK/pop_ready/deliver）在一个连接上耗时（如 TS 解复用 + mount 投递）时，其他连接的数据包排队；数据包到达后要等下一个 10ms tick 才投递。
- **修复**：每连接独立任务 + 统一队列，或把投递移出 maintenance；tick 仅做超时维护，数据到达即处理。

## 中

- **RT-7. proxy keepalive 与帧消费共路串行**：`src/client/mod.rs:249-264`——20s 到期后 `keep_alive().await` 串行执行完整 RTSP 往返（最坏 `request_timeout` 5s），期间媒体帧停止消费、mpsc(64) 填满、上游积压；慢响应上游使订阅端周期性停顿。修复：keepalive 独立 task。（已列入 `docs/roadmap/client.md` M4）
- **RT-8. RTSP UDP 模式 keepalive 被媒体流抢占**：`src/client/mod.rs:252-258`——`sleep_until` 到期后与 33ms 一帧的媒体分支竞争（tokio 公平 select 下最多延迟数个帧周期），配合服务端 60s session 超时，半开控制连接可能到期才被发现。修复：控制面独立 task 或 `select!` 中 `biased` 媒体优先 + keepalive 专用间隔。（已列入 `docs/roadmap/client.md` M4）
- **RT-9. proxy 冷启动首帧 = 4+N 个 RTT + GOP 剩余**：`src/server/session.rs:656-694`（PLAY 先响应后连上游）、`src/server/proxy_runtime.rs:121-183`、`src/client/mod.rs:502-576`（无条件 OPTIONS+DESCRIBE，SDP 已缓存仍重取）——公网首帧延迟 2-4s。修复：SETUP 阶段预热、跳过冗余 DESCRIBE。（客户端侧回退复用连接已列入 `docs/roadmap/client.md` M2）
- **RT-10. 上游 jitter buffer 固定 max_delay=100ms**：`src/rtp/jitter.rs:19`、`src/client/setup.rs:58-66`——公网乱序/丢包时稳态增加 100ms 且突发。修复：直播场景 20-40ms 或事件驱动释放。
- **RT-11. 无订阅者 5s 后 proxy 停止，间歇观众每次冷启动**：`src/server/proxy_runtime.rs:100-119,209-234`、`src/server/mod.rs:72`。修复：常连配置或保留关键帧快速恢复。
- **RT-12. HLS/DASH Lagged 后丢弃全部帧等 GOP**：`src/hls/mod.rs:91-95`、`src/dash/mod.rs:138-145`——一次 lag 导致最长一个 GOP 的输出中断。修复：等待关键帧前先回放缓存关键帧。
- **RT-13. startup 缓存超限整段清空**：`src/proxy/continuity.rs:203-251`——字节/帧超限时 `clear()` 整段丢弃，起播回到"等下一个关键帧"。修复：按时间窗口滚动淘汰最旧帧。
- **RT-14. RTMP 无 TCP_NODELAY + 每帧 flush**：`src/rtmp/mod.rs`（accept 处无 `set_nodelay`）、`src/rtmp/session.rs:158-160`（writer 每消息 `flush().await`）——Nagle 聚合放大小包延迟（可达 40ms），flush 每帧一次 syscall。修复：连接建立时 `set_nodelay(true)`、批量 flush 或按时间聚合。
- **RT-15. UDP RTP 逐包 `send_to().await` 无批处理**：`src/server/tasks.rs:298`——每包一次 syscall 等待，高码率下 syscall 开销占延迟预算。修复：批量 send（`sendmmsg` 或小批量聚合）。
- **RT-16. 重连退避无抖动（thundering herd）**：`src/server/proxy_runtime.rs:60-100`——多客户端同时断线后同步重试。修复：指数退避加随机抖动。

## 低

- **RT-17. jitter drain 用 1-10ms 轮询 tick 且与收包同 task**：`src/client/setup.rs:121-145,343-405`——乱序恢复帧平均多等 5ms；`tx.send().await` 满时阻塞收包。修复：`next_deadline()` 事件驱动 + 满时丢弃。（已列入 `docs/roadmap/client.md` M4）
- **RT-18. bootstrap 单帧启动黑屏**：`src/mount.rs:255-265`——startup 缓存空时新订阅者仅得 1 个孤立关键帧，之后 `awaiting_key` 丢 P 帧等下一 GOP。修复：缓存完整 GOP。
- **RT-19. TSBPD 10ms tick 量化**：`src/srt/mod.rs:197`——投递时刻被量化到 10ms 网格。
- **RT-20. HTTP-FLV 每帧 flush**：`src/http_flv/handler.rs`（`write_all` + 每帧 flush）——小包 syscall 放大。修复：按 16-32KB 或 50ms 聚合。
- **RT-21. pipeline Block 队列超时 100ms**：`src/media/pipeline.rs`——`block_timeout` 默认 100ms 内下游拥塞即丢帧。修复：配置化。
- **RT-22. RTCP RR 仅回应式发送**：`src/server/tasks.rs`——接收端 RR 只在收到 SR 后发，空闲流无 RR，质量监控中断。修复：独立 RR 定时器。
- **RT-23. UDP sender select 中 RTCP 与帧抢分支**：`src/server/tasks.rs:224-336`——5s tick/RR 与帧撞车时当前帧延迟一轮。修复：`biased` 帧优先。
- **RT-24. proxy_runtime 每帧创建 10s timeout 与 idle tick 竞争**：`src/server/proxy_runtime.rs:210-241`。修复：idle 检查独立 task。
- **RT-25. 文件源起播无关键帧对齐**：`src/file/source.rs:217-221,243-255`——`start_position > 0` 时从非关键帧开始，订阅者黑屏一个 GOP。修复：seek 回退到前一个 IDR。

---

# 二、兼容性问题

## 高

### CP-1. RTMP play 从不发送 AVC/AAC sequence header（ffplay/OBS 拉流必受影响）
- **位置**：`src/rtmp/session.rs:732-734,770-772`（`build_video_tag(unit, false)` 恒 false）、`session.rs:296-305,364-367`（seq header 只存局部状态，不 push 进 mount）
- **问题**：play 客户端收到 AVCC 数据但无 AVCDecoderConfigurationRecord；AAC 完全无 AudioSpecificConfig。ffmpeg flvdec 依赖首帧含 SPS/PPS 才勉强可播；H.265（codec id 12）基本必失败；AAC 无法初始化。
- **修复**：publish 时把 seq header 写入 mount 启动快照；play 的 subscriber 先发 `build_video_tag(unit, true)`/`build_audio_tag(unit, true)`。

### CP-2. RTMP play 会话 30s idle 超时必断连（<667kbps 的流 30 秒即断）
- **位置**：`src/rtmp/session.rs:182,190`（`idle_timeout = 30s` 包住主循环 read）
- **问题**：RTMP 播放端不发送数据（ACK 仅 >2.5MB 触发），任何低码率流播放 30s 后被断开；发布端静音+静止画面同样被断。
- **修复**：Playing 状态禁用 idle timeout（仅握手/connect 阶段应用）；处理对端 UserControl PingRequest。

### CP-3. WHIP answer 只有 m=application 无媒体行（浏览器 100% 失败）
- **位置**：`src/webrtc/sdp_ext.rs:206-213`（固定 `m=application 9 UDP/DTLS/SCTP webrtc-datachannel`）
- **问题**：浏览器 WHIP 客户端期望 `m=video`/`m=audio` 媒体段；且信令后无任何媒体管线（无 ICE socket、STUN 绑定、DTLS 握手、SRTP 收发——`src/webrtc/ice.rs`/`dtls.rs` 为原语 no-op）。WHEP answer 固定 PT 96/111 不解析 offer。整条 WebRTC 路径不可用。
- **修复**：按 offer 生成媒体行 + PT 协商；实现 ICE/DTLS/SRTP 管线（或从 README 明确移除 WebRTC 能力声明）。

### CP-4. RTMP→RTP 音频时间戳 ×44 整数除法（44.1kHz 慢 0.23%，音画 8s/小时漂移）
- **位置**：`src/rtmp/session.rs:378`（`ts_ms.wrapping_mul(44100 / 1000)` = ×44 而非 ×44.1）
- **问题**：44.1kHz 与 22.05kHz 采样率倍率错误；下游 RTP/SR/HLS 按错误速率换算，长直播音画逐渐失步。
- **修复**：`ts_ms as u64 * 44_100 / 1_000`（64 位中间量）。

### CP-5. RTMP play 的 FLV tag 时间戳用 PTS 且叠加 CTS=PTS−DTS（B 帧双重偏移）
- **位置**：`src/rtmp/session.rs:765,767,780`（`ts = unit.pts`）+ `src/rtmp/flv.rs:370-375`（`cts = pts - dts`）
- **问题**：FLV 语义为 PTS = tag_ts + CTS；当前 tag_ts=PTS 又写 CTS=PTS−DTS，播放器解析出 2·PTS−DTS。无 B 帧时 CTS=0 恰好无影响；有 B 帧时播放端乱序/花屏。H.265 分支（flv.rs:388-391）则硬编码 cts=0，行为不一致。
- **修复**：tag 时间戳用 `unit.dts`（无 dts 回退 pts），CTS 保持 pts−dts。

### CP-6. SRT ACK 的 largest-acknowledged 序号偏 1（丢包后发送方无重传数据，永久丢包）
- **位置**：`src/srt/tsbpd.rs:209-211`（`ack_number()` 返回 `next_deliver_seq`）、`src/srt/mod.rs:296-301`
- **问题**：SRT ACK 第一字段应为"最后确认的序号"（next_expected−1）；发送方（ffmpeg/OBS libsrt）`ackData` 释放 ≤ack 的重传缓冲，多释放一个未确认包，该包丢失后 NAK 也救不回。
- **修复**：`ack_number()` 返回 `next_deliver_seq.wrapping_sub(1)`。

## 中

- **CP-7. SDP 单播缺 `c=` 行**：`src/protocol/sdp.rs:258-270`（仅 multicast feature 输出）——RFC 4566 §5.7 要求会话级或媒体级至少一个 `c=`；严格解析器（GStreamer gst_sdp）告警/拒绝。修复：单播默认 `c=IN IP4 0.0.0.0`。
- **CP-8. Content-Base 拼接原始请求 URI**：`src/server/session.rs:1120-1124`——含 userinfo/query 时泄露凭据且 SETUP 丢 query 参数。修复：用 `url::Url` 规范化后生成。
- **CP-9. 代理透传上游绝对 control URL → 下游 SETUP 必败**：`src/server/mod.rs:511-536` + `src/protocol/sdp.rs:310`——ffmpeg/VLC 对绝对 control 直接发起指向上游地址的 SETUP，本地匹配 404。修复：缓存描述时重写为相对 control。
- **CP-10. SDP 解析丢弃会话级 `a=control`**：`src/protocol/sdp.rs:151-154`——RFC 2326 C.1.1 允许会话级 control；部分摄像头只在会话级给 control，客户端 SETUP URL 错误。修复：记录会话级 control 为媒体级缺省。
- **CP-11. 空窗口 HLS playlist 返回 200 空 body**：`src/hls/playlist.rs:24-26`——无效 M3U8（无 #EXTM3U），hls.js/AVPlayer 直接报错。修复：返回有效空 playlist（`#EXTM3U` + `EXT-X-ENDLIST`）或 404。
- **CP-12. HLS VOD 的 MEDIA-SEQUENCE 硬编码 0**：`src/hls/playlist.rs:90`——多会话/段清除后应与实际首段一致。修复：取 ring 首段序列号。
- **CP-13. DISCONTINUITY-SEQUENCE 固定 0**：`src/hls/playlist.rs`——断点续播多会话后播放器缓存错乱。修复：会话内单调递增。
- **CP-14. 滑动窗口无 3×TARGETDURATION 保留**：`src/hls/segment_ring.rs`——RFC 8216 §6.3.3 要求旧段保留 ≥3 倍目标时长；并发拉取慢的播放器会 404。修复：窗口保留至少 3×targetduration。
- **CP-15. HLS playlist URI 为绝对路径**：`src/hls/playlist.rs`（`asset_uri` 拼 host）——mount 挂在子路径或反代后 URI 错误 404。修复：相对 URI。
- **CP-16. HLS 响应无 CORS 头**：`src/http/mod.rs`——hls.js 跨域部署失败。修复：加 `Access-Control-Allow-Origin`。
- **CP-17. DASH codecs=hev1 与 fMP4 hvc1 sample entry 矛盾**：`src/dash/mpd.rs:305` vs `src/hls/segmenter.rs:1149`——Safari/Chrome 对 hev1+hvc1 不一致拒绝。修复：统一（优先 hvc1）。
- **CP-18. Publisher 重连后 availabilityStartTime 回 1970**：`src/dash/mpd.rs`——AST 应为首个段时刻而非会话创建。修复：AST = 首段 pts 映射的墙钟。
- **CP-19. SSRC 恒定非随机、无冲突检测**：`src/server/tasks.rs:71,208`（`0x5254_5350 + track.id`）——RFC 3550 要求随机初始 + 冲突处理；多服务器同 SSRC 时接收端混流。修复：随机初始化。
- **CP-20. SDES CNAME 硬编码全局相同**：`src/server/tasks.rs:426`（`b"rtflow"`）——RFC 3550 要求每会话唯一；RTP 会话统计工具按 CNAME 聚合会错乱。修复：`user@host` 格式。
- **CP-21. RTCP 发送周期不符合 RFC 3550 §6.2/6.3**：`src/server/tasks.rs:81,219`——固定 5s 无随机化、无 5% 带宽计算、无 interval 抖动。修复：按 §6.3 计算。
- **CP-22. GStreamer 源无 fmtp → AAC config 缺失**：`src/gst/mod.rs:162`——下游 ffmpeg/GStreamer 无法初始化 AAC 解码。修复：从 caps 提取 config 生成 fmtp。
- **CP-23. 会话结束不发 RTCP BYE**：`src/server/tasks.rs`——接收端状态滞留。修复：TEARDOWN 时发 BYE。
- **CP-24. SRT induction 响应 version=5（规范要求 4）**：`src/srt/handshake.rs:192-208`——libsrt 严格校验时握手失败。修复：induction 用 version=4 + HSREQ 标志。
- **CP-25. SRT conclusion 扩展类型用 HSREQ(0x0001) 应为 HSRSP(0x0002)**：`src/srt/handshake.rs:180`——libsrt 按 HSRSP 查找协商结果，可能解析失败。修复：按方向区分扩展类型。
- **CP-26. HTTP-FLV G.711 sound rate 位全 0（按 5.5kHz 播放）**：`src/http_flv/mux.rs:552-564`——PCMA/PCMU 应为 rate=3（8kHz）。修复：rate/size/type 位按 codec 填写。
- **CP-27. HTTP-FLV AAC seq header 依赖 fmtp 无 fallback**：`src/http_flv/mux.rs`——fmtp 缺失时 AudioSpecificConfig 为空。修复：从流中提取 ASC。
- **CP-28. WHIP/WHEP 缺 `Access-Control-Expose-Headers: Location`**：`src/webrtc/whip.rs`——浏览器 fetch 读不到 Location 头，session 管理失败。修复：响应头补充。
- **CP-29. WHIP answer 缺 fmtp/rtcp-fb/extmap/ssrc 属性**：`src/webrtc/sdp_ext.rs`——浏览器协商失败或能力降级。修复：完整媒体属性。

## 低

- **CP-30. 重复 header 一律拒绝**：`src/protocol/rtsp.rs:101-105`——`WWW-Authenticate` 等可合法重复（Basic+Digest 双 challenge）；客户端遇此类服务器连接失败。修复：仅单值字段拒绝重复。
- **CP-31. GET_PARAMETER 强制 Session 头**：`src/server/session.rs:226-232`——RFC 允许无会话的服务器级 GET_PARAMETER。修复：无 Session 时按服务器级处理。
- **CP-32. SETUP track 匹配失败直接断连无响应**：`src/server/session.rs:446,496`——应回 404/461。修复：捕获错误回状态码。
- **CP-33. 状态行拒绝空 reason-phrase**：`src/protocol/rtsp.rs:124-129`。修复：`len >= 2`。
- **CP-34. 重复 SETUP 不按替换语义处理**：`src/server/session.rs:359-519`——双 sender 双倍带宽。修复：SETUP 前按 track 去重。
- **CP-35. interleaved 通道号 `(index*2) as u8` 溢出**：`src/client/setup.rs:51-52`——≥128 轨 panic。修复：`u8::try_from`。
- **CP-36. RTP sequence 初始 0 未随机**：`src/rtp/payload.rs:31`——RFC 3550 建议随机起点。修复：随机初始。
- **CP-37. PCMA/PCMU marker 恒 1**：`src/rtp/payload.rs`——应随说话突发置位。修复：帧边界置位。
- **CP-38. SR 的 NTP↔RTP 映射 i32 截断**：`src/server/tasks.rs:40-44`——6.6h 后翻转（同 RT 边界问题，见下）。修复：64 位扩展。
- **CP-39. RTMP releaseStream/FCPublish 无响应**：`src/rtmp/session.rs:491-494`——部分 SDK 严格等待事务卡住。修复：回 `_result`。
- **CP-40. RTMP fmt0+extended timestamp 不重置 timestamp_delta**：`src/rtmp/chunk.rs:226-234`——长时间直播（>4.66h）混用 fmt 时时间戳跳变。修复：fmt0 分支重置 delta。
- **CP-41. SRT ACK 载荷仅 4 字节（缺 RTT/RTTvar/缓冲/速率）**：`src/srt/packet.rs:186-195`——对端 RTT 估计无法更新，丢包恢复退化。修复：填 24 字节载荷。
- **CP-42. SRT NAK 无去重且流尾丢包永不报告**：`src/srt/mod.rs`——重复 NAK 风暴、尾包丢失无 TLPKTDROP。修复：NAK 去重 + 流尾超时报告。
- **CP-43. SRT 不主动发 keepalive**：`src/srt/mod.rs`——仅回应式，NAT 映射可能过期。修复：周期 keepalive。
- **CP-44. WebRTC PATCH trickle 未实现**：`src/webrtc/whip.rs`——多候选场景 ICE 失败。修复：实现 PATCH 处理。
- **CP-45. WebRTC 会话 60s 过期状态不推进**：`src/webrtc/session.rs`——`state` 恒 Connecting。修复：按 ICE/DTLS 阶段推进。
- **CP-46. FLV 无 onMetaData**：`src/http_flv/mux.rs`——老 Flash/部分 Android 播放器无法启动。修复：写 script tag。
- **CP-47. H.265 HTTP-FLV 依赖 FFmpeg ≥6.1（enhanced-rtmp）**：`src/http_flv/mux.rs:439-468`——老版本 ffmpeg 黑屏。修复：文档标注 + 降级路径。

---

# 三、时间戳/时钟边界（跨类别）

- **TB-1【高】RtpNtpClock i32 差值 6.6h 后翻转，SR NTP 倒退**：`src/server/tasks.rs:40-44`——`as i32` 截断 2³¹ ticks（90kHz ≈ 6.63h）；超限后 SR 的 NTP↔RTP 映射失效，ffplay/VLC 的 A/V 同步错位。修复：64 位扩展时间戳或发送 SR 前重锚定。
- **TB-2【高】pts_from_timestamp 无回绕扩展，13.1h 后 pts 归零**：`src/rtp/payload.rs:227-232`——32 位 RTP 时间戳线性映射 Duration，回绕瞬间跳回 0；RTMP/HLS/proxy 下游时间线重置（HLS 靠强制切段兜底，等于每 13.1h 断流一次）。修复：记录回绕计数叠加周期。
- **TB-3【中高】SR 的 NTP 只用于 RTT，未校准 A/V 时间线**：`src/client/setup.rs:454-486`、`src/rtcp/stats.rs:94-96`——多轨基准不同（RTSP 源常见）时音画起始偏移数秒；proxy 重连后 A/V 相对偏移沿用旧值。修复：用 SR (NTP, rtp_timestamp) 建墙钟↔媒体时钟映射统一时间轴。
- **TB-4【中】HLS tfdt 经 µs 两次截断，A/V 相对漂移 0.03-0.1%**：`src/hls/segmenter.rs:346-349,1362-1385`——90kHz/48kHz 与 µs 不可通约，每帧截断 1 tick。修复：段内保留原始 ticks（用 `rtp_timestamp` 字段）或 `div_round`。
- **TB-5【中】RTCP jitter transit 参考系错误**：`src/rtcp/stats.rs:83-90`——本地单调时钟直接减 RTP 时间戳，回绕时 jitter 爆表。修复：SR 校准映射 + 32 位有符号差。
- **TB-6【中低】HLS 跨轨 pts 基准差误判回绕**：`src/hls/segmenter.rs:146-151`——`current_segment_pts` 跨轨共享，A/V 基准不同的 RTSP 源触发虚假切段 + DISCONTINUITY + 段边界撕裂。修复：按轨维护时间线 + 大阈值回绕判断。
- **TB-7【低】RTMP 播放 32 位 ms 时间戳 49.7 天回绕**：`src/rtmp/session.rs:725-728,765-767`——与 TB-2 叠加实际更早触发。修复：wrapping 差分累加。
- **TB-8【低】H.265 FLV CTS 硬编码 0**：`src/rtmp/flv.rs:388-391`——B 帧流 PTS 丢失。修复：复用 H.264 的 cts 计算。

**已核查合规**：NTP 1900 epoch 换算与分数位（tasks.rs:434-445）、RTP/RTCP/SRT 大端编码、jitter sequence 回绕、文件源绝对 deadline 节流无漂移、chunk 头 AMF0 编码、AVCDecoderConfigurationRecord、enhanced-rtmp fourcc、FLV 头/tag 头、RTSP Transport/RTP-Info/错误码、HLS TARGETDURATION/EXTINF/DISCONTINUITY 放置、DASH startNumber 映射。

---

# 四、测试覆盖盲区

- 外部互操作测试仅覆盖：RTSP 播放（GStreamer rtspsrc）、RTSP 发布（ffmpeg）、五编码 RTP depay、代理四传输组合。
- **无覆盖**：RTMP 推/拉与 ffmpeg/OBS 互操作（CP-1/CP-2 直接由外部工具验证）、SRT 与 ffmpeg/OBS（CP-6/CP-24/CP-25）、WebRTC 浏览器端（CP-3）、HLS 与 hls.js/VLC/ExoPlayer（CP-11/12/13/14/15/16）、HTTP-FLV 与 ffplay/OBS（CP-26）、VOD Range 真实播放器 seek、Digest 鉴权互操作。
- **无长时测试**：无 >6.6h / >13.1h 时间戳回绕测试（TB-1/TB-2 需要）、无 B 帧流端到端测试（CP-5）、无 A/V 基准不同源测试（TB-3/TB-6）。

---

# 五、修复优先级建议（Top 10）

1. **CP-1 + CP-2**：RTMP play 的 sequence header 缺失与 30s idle 断连——RTMP 拉流完全不可用/必断，实验协议中最高优先。
2. **CP-3**：WebRTC 无媒体管线——若对外承诺 WHIP/WHEP 则整条不可用；否则按 README 明确降级为占位。
3. **TB-1 + TB-2**：时间戳回绕（6.6h/13.1h）——所有 RTSP 出流的 SR 与媒体时间线长时正确性。
4. **CP-4 + CP-5**：RTMP 音频倍率 ×44 与 FLV tag PTS/CTS 双重偏移——音画同步核心。
5. **RT-1 + RT-2**：startup 无 pacing 与 broadcast 无字节背压——起播延迟与慢客户端恢复延迟。
6. **CP-6**：SRT ACK 偏 1——丢包后永久丢帧。
7. **RT-3**：TSBPD 到达时刻调度——SRT 端到端延迟与抖动。
8. **CP-7/CP-8/CP-9/CP-10**：RTSP/SDP 合规四项——与严格客户端的互操作。
9. **CP-11 至 CP-16**：HLS playlist 合规六项——播放器兼容（空 playlist、MEDIA-SEQUENCE、CORS、绝对 URI）。
10. **RT-5**：interleaved 写锁无超时——慢客户端卡死整链。

---

# 六、与既有修复的关联

- 上一轮《docs/audits/code.md》修复的 segmenter 时间回绕强制切段（R10）在本轮被评估为 TB-2/TB-6 的"兜底但有害"方案：它把 13.1h 回绕变成每周期一次断流，且跨轨基准差会误触发——建议按 TB-2/TB-6 的根因方案替换。
- 上一轮新增的 proxy 媒体空闲超时（10s）与 RT-8 的 keepalive 抢占叠加时，可能把慢控制面误判为流失败——建议先修 RT-7/RT-8 再评估超时阈值。
- RTMP/SRT/WebRTC 的问题绝大多数位于上一轮已提交的"feat" commit 中，属实验功能开发质量，与本轮修复建议独立。
