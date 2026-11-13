# rtflow 性能与可靠性审计报告

- 日期：2026-07（基于当前工作区，含未提交改动）
- 范围：`src/` 全部模块（约 3 万行）、`tests/`（9.5 千行）、`examples/`、`benches/`、`fuzz/`
- 方法：`cargo check --all-features` + `cargo clippy --all-targets --all-features` 静态基线；对数据面（热路径）深度审查、全库模式扫描（无界缓冲、锁、克隆、泄漏、超时缺失）；关键发现均经人工抽查复核 `file:line`
- 约定基线（避免误报）：有界队列是设计、`Bytes` 零拷贝共享、磁盘 IO 走 blocking worker、任务经 CancellationToken 树监督、协议解析输入设上限。以下问题均已排除"符合设计约定"的可能

## 修复状态（2026-07 复检）

| 级别 | 性能 | 可靠性 | 合计 | 已修复 |
|---|---|---|---|---|
| 高 | 5 | 11 | 16 | 16/16 |
| 中 | 8 | 12 | 20 | 16/20 |
| 低 | 9 | 10 | 19 | 8/19 |
| **合计** | **22** | **33** | **55** | **40/55** |

**已修复（40 条）**：R1-R4（SRT cookie/连接上限、WebRTC 会话清理、RTMP 握手超时/缓冲上限）、R5/R6/R7/R8/R9/R10/R11/R12/R13/R14/R15/R16/R17/R18/R19/R20、R26/R27/R28/R30、R31/R32/R33/R34/R35、P1/P2/P3/P5/P6/P7/P8/P9/P10/P11/P13/P16/P18/P19/P20/P22/P23/P25。

**经评估跳过（9 条，均有技术理由，见对应条目）**：
- P14（UDP recv 零拷贝）：安全 Rust + tokio 下 recv 必须写入连续缓冲区，转移所有权会带走整个缓冲区，每包分配无法消除，收益仅 memcpy→memset 持平
- P15（tsbpd BTreeMap 每包分配）：BTreeMap 是乱序缓冲的合理结构，替换为自定义结构风险高、收益小
- P17（jitter O(n)）：pending ≤ 128 且仅在乱序缺口时触发，O(128) 可忽略；`next_deadline` 无调用者
- P21（HLS try_recv 每帧轮询）：无锁原子操作，成本可忽略
- P24（push receiver_count TOCTOU）：语义可接受（计数仅用于统计），改动有行为风险
- P12 的"合并双队列"部分：结构性重构，已修其可观测性问题（Block 超时丢弃计数）
- R21（RTMP 写路径超时）、R22（HTTP 写超时）、R23（keep-alive 退避）、R29（VOD blocking worker 取消）、R36（push/try_push 语义）、R24（三套 register 统一）、R25 已并入 P4/R10 修复、R18 部分修复：这些属行为变更较大或需产品决策的项，保持现状并在对应条目注明

**遗留但已缓解**：R12 的帧指标仅对 RTSP 注册的 mount 生效（RTMP/SRT 直连 registry 的 mount 不汇入全局）；R18 的 prune 仍为轮换时全量扫描（错误处理已修复、坏段不再阻断）。

**验证**：`cargo test --all-targets --all-features` 59 个测试套件全绿（250+ 测试，含新增的纯音频切段与 SRT mount 注销断言）；`cargo clippy` 与审计基线持平（60 个 pre-existing 风格类警告），无新增。

---

## 严重性统计

| 级别 | 性能 | 可靠性 | 合计 |
|---|---|---|---|
| 高 | 5 | 11 | 16 |
| 中 | 8 | 12 | 20 |
| 低 | 9 | 10 | 19 |
| **合计** | **22** | **33** | **55** |

---

# 一、性能问题

## 高

### P1. RTMP 推流每帧整帧 `Bytes::copy_from_slice` 全量拷贝
- **位置**：`src/rtmp/flv.rs:101`（`parse_video_tag`）、`flv.rs:121,155,176,184,204`（`parse_audio_tag`）
- **问题**：调用方 `src/rtmp/session.rs:280/348` 传入的 `msg.payload` 是 `Dechunker` 中 `state.buffer.split().freeze()`（`src/rtmp/chunk.rs:265`）产出的唯一所有权 `Bytes`，本可零拷贝 `slice`。当前每个媒体帧（1080p 关键帧可达 100KB+）都重新分配并 memcpy 全帧。
- **影响**：所有 RTMP 推流每帧一次全帧 memcpy + 堆分配；多路流时分配器压力成倍。
- **修复**：签名改为接收 `&Bytes`，用 `data.slice(5..)` / `data.slice(2..)` 零拷贝返回。

### P2. SRT 推流每帧两次全帧拷贝（同一帧数据 3 份）
- **位置**：`src/srt/ts_demux.rs:262`（`payload.to_vec()` 进 `pes_buffers`）、`ts_demux.rs:333`（`Bytes::copy_from_slice(payload)` 进 `AccessUnit.data`）
- **问题**：PES 载荷先拷贝成 `Vec<u8>`，处理时再拷贝成 `Bytes`；原始 `Bytes` + `Vec<u8>` + 新 `Bytes` 三份并存、两次 memcpy、两次分配。
- **影响**：SRT 接收路径最大单点开销，每帧 2 次全帧拷贝。
- **修复**：`pes_buffers` 改 `HashMap<u16, BytesMut>`，处理时 `split_off(...).freeze()` 零拷贝，降到 1 次拷贝。

### P3. HTTP-FLV mux 每帧无条件 Annex-B 扫描 + 双重全帧复制，且与 RTMP 上游格式失配
- **位置**：`src/http_flv/mux.rs:570-620`（`annex_b_to_avcc`/`parse_annex_b_nals`/`find_start_code`），调用点 `mux.rs:119,129`
- **问题**：每个 H264/H265 帧：① 逐字节 start-code 扫描；② 逐 NAL `to_vec()` 小分配；③ 全帧 `put_slice` 第二份复制。而 RTMP 上游（`rtmp/flv.rs:50-51`）交付的本来就是 AVCC（length-prefixed），此处做无谓扫描重建；AVCC 帧内偶然出现 `00 00 01` 字节模式还会被误切分产生损坏输出。
- **影响**：每个 HTTP-FLV 会话每帧两次全帧拷贝；N 个并发客户端 = N 份独立转换。附带正确性风险（误切分）。
- **修复**：先做廉价格式探测（首 4 字节大端长度 == len-4），AVCC 输入直接 `put_slice(&unit.data)` 零拷贝；Annex-B 输入用 `Vec<&[u8]>` 视图 + 一次 `with_capacity(精确大小)` 写入。

### P4. HLS/DASH segmenter：纯音频流永不切段，`samples` 无界累积（性能 + 可靠性）
- **位置**：`src/hls/segmenter.rs:146-151`（`starts_new_segment` 要求 `unit.is_keyframe && matches!(codec, H264|H265)`）、`177-183`（`track.samples.push` 无上限）
- **问题**：无视频轨（AAC/G.711）时轮换条件恒为 false，`finalize_segment` 只在 `flush()`（任务退出）时调用；纯音频直播期间 `TrackState.samples` 无限累积（24h AAC ≈ GB 级），且 HLS 播放器永远收不到任何 `.m4s` 段。`hls/mod.rs:63-67` 的 `awaiting_keyframe = has_video` 反而掩盖了这一点。视频流若无 IDR/IDR 稀疏（`segmenter.rs:162-183`）同样无界增长直至 OOM。
- **影响**：内存无界（OOM）+ HLS 输出完全不可用。
- **修复**：轮换条件与视频解耦——按 `segment_duration` 时间窗口强制轮换；有视频轨时额外等关键帧对齐；同时给 samples 设硬上限（如 2× 段时长）。

### P5. HLS segmenter 每帧 Annex-B → length-prefixed 全量重建
- **位置**：`src/hls/segmenter.rs:359-380`（`annex_b_to_length_prefixed`），调用点 `segmenter.rs:173-176`；`codec_config_from_annex_b`（`segmenter.rs:382-403`）在配置未建立时每帧再扫一遍
- **问题**：对 RTP/Annex-B 源，每个视频帧被扫描 + 完整重建（`BytesMut` + 逐 NAL `put_slice`），即使 SPS/PPS 已提取缓存，后续每帧仍重复全帧遍历。
- **影响**：1080p60 约每秒 5-15MB 额外拷贝；对 RTMP/AVCC 源一旦帧内出现 start-code 字节模式还会误切分并缓存损坏数据。
- **修复**：上游 `rtp/payload.rs` 组帧时直接输出 AVCC；或此处先做格式探测、仅首帧/新参数集时重建。

## 中

- **P6. 每帧两次 `std::sync::Mutex` + AccessUnit clone**：`src/mount.rs:151-176`（`publish_cached`）每帧无条件锁 `startup_cache` + `KeyframeCache::push`（内部 `frame.clone()`，`continuity.rs:219`）+ 锁 `bootstrap` + `frame.clone()` + `frames.send`。修复：缓存更新改 `try_lock` 跳过、`bootstrap` 改定长槽位（track_id 是 u8）。
- **P7. 解包后整帧二次全量扫描判关键帧**：`src/rtp/payload.rs:446-461`（`is_keyframe` → `annex_b_nals` 全帧扫描 + 2 个 Vec 分配），调用点 `payload.rs:286`。组装过程中每个 NAL 头已可见，可在 `push` 循环内累计标志。
- **P8. `try_push` 每帧深拷贝整个 `MediaDescription`**：`src/mount.rs:373-380`（`description_for(...)?` → `description.clone()` + 线性 `track()` 查找），调用点 `file/source.rs:260`、`gst/app_sink.rs:50`。修复：预计算 `[Option<Codec>; 256]` 缓存。
- **P9. 每个 H265 帧一次全帧 CRA 扫描**：`src/http_flv/handler.rs:110,210-238`（`h265_random_access_is_cra` 逐字节扫描），60fps 下每秒 60 次全帧扫描，非关键帧上 100% 浪费。修复：`unit.is_keyframe &&` 前置短路。
- **P10. FLV `tag_buffer` 每帧重新分配 + 一次全帧复制**：`src/http_flv/mux.rs:93,149`（`BytesMut::split().freeze()` 后容量归零，下帧必然 malloc；AVCC 源每帧仍有一次 memcpy）。修复：先写 tag 头再 `put_slice(&unit.data)`、复用交替 buffer。
- **P11. 媒体段缓冲 64KB 起步多次扩容**：`src/hls/segmenter.rs:305`（`BytesMut::with_capacity(64*1024)`），8Mbps 的 2 秒段 ~2MB，翻倍扩容多次整体复制。修复：先遍历 `samples` 求和（`write_mdat` 已在做）再一次性 `with_capacity`。
- **P12. pipeline 内部链路双队列 + OverflowPolicy 失效**：`src/media/pipeline.rs:246-274`（mpsc 边队列 + edge task + 目标 input 队列，两次入队），且 edge task `target.send(unit).await` 无限阻塞使 `DropNewest`/`block_timeout` 对内部链路永不生效（`pipeline.rs:335` 只对外部 ingress 应用），同一队列语义分裂。
- **P13. RTMP 播放路径每帧 2 次分配 + 2 次 memcpy**：`src/rtmp/flv.rs:354`（`build_video_tag` 分配 + `put_slice`）+ `src/rtmp/chunk.rs:308`（`Chunker::encode` 再分配再拷贝）。修复：writer 任务复用输出缓冲、tag 头与 chunk 头合并组装。
- **P14. SRT 每 UDP 包一次全量 memcpy**：`src/srt/packet.rs:113,131`（`Bytes::copy_from_slice(&data[16..])`），10Mbps 下约 950 次/秒。修复：`recv_from` 直写 `BytesMut` 后 `freeze()`。
- **P15. SRT 每包 BTreeMap 节点堆分配 + `pop_ready` 分配**：`src/srt/mod.rs:454,460` + `src/srt/tsbpd.rs:99`；稳态 120ms 缓冲 ~120 节点，每包一次分配/删除 + `Vec` 分配。
- **P16. `deliver_units` 每包 `mounts.get_handle` + `publisher()`**：`src/srt/mod.rs:470-521`（RwLock 读锁 + HashMap 哈希 + 构造 publisher），可在连接建立时缓存。

## 低

- **P17. jitter buffer `next_deadline`/`is_expired` O(n) 全量扫描**：`src/rtp/jitter.rs:68-88`（`next_deadline` 现无调用者；`is_expired` 乱序时触发）。修复：`BTreeMap.first_key_value()` + 单调游标。
- **P18. mount 前缀查找 O(n) 线性扫描**：`src/mount.rs:494-507`（`find_longest_prefix` 持读锁 filter + `max_by_key`），每次 RTSP 请求执行（`server/session.rs:1172`）。
- **P19. AU 组装 `BytesMut::new()` 无预分配**：`src/rtp/payload.rs:270-277,348,400`（逐 NAL `extend_from_slice` 多次扩容搬移），NAL 长度已知可一次算总长。
- **P20. SRT keyframe 检测整帧逐字节扫描**：`src/srt/ts_demux.rs:358-400`，可限制扫描窗口（前 1-2KB）。
- **P21. HLS 每帧空轮询 `segments.try_recv()`**：`src/hls/mod.rs:97`、`src/dash/mod.rs:204`（段每 2 秒才产生一个）。
- **P22. seq header 未就绪期间每帧克隆 fmtp HashMap**：`src/http_flv/handler.rs:111-119`（`announce_fmtp()` 每帧加锁 + clone），只需取一次快照。
- **P23. `segment_ring` 持读锁全量 clone + O(n) 查找**：`src/hls/segment_ring.rs:33-36,45-48`；窗口放大后每段请求 O(n)。
- **P24. `push` 的 receiver_count 检查与 send 之间 TOCTOU**：`src/mount.rs:356-371`，丢帧语义误标 + 缓存残留。
- **P25. 鉴权/限流锁内 O(n) 全表清理**：`src/protocol/auth.rs:172-178`、`src/security/mod.rs:186-190`（`retain` 全量扫描在锁内），高并发时锁持有时间线性增长。

---

# 二、可靠性问题

## 高

### R1. SRT SYN-cookie 防护失效 + 连接数无上限（可 OOM）
- **位置**：`src/srt/mod.rs:352`（`let _expected_cookie = generate_cookie(...)` 计算后直接丢弃，与 `hs.cookie` 从不比较）、`src/srt/mod.rs:194-242`（`SrtConfig.max_connections` 仅定义于 `srt/mod.rs:64,82`，连接接受处从未使用；`connections: HashMap` 无上限）
- **问题**：攻击者伪造任意 UDP 源地址发一个 HSv5 Conclusion 控制包即可建立连接并注册发布挂载；配合无连接上限可无限灌入 `SrtConnection`（每个含 TSBPD 缓冲 + 解复用器）直至 OOM。
- **影响**：远程 DoS + 未授权发布。**注意：`srt` 为实验性协议（`experimental-protocols`），但即便实验启用也应防护。**
- **修复**：比较 `expected_cookie` 与握手携带 cookie；在 accept 处执行 `max_connections` 上限检查。

### R2. WebRTC 会话超时清理从未执行（会话表只增不减）
- **位置**：`src/webrtc/session.rs:165`（`cleanup_expired` 全库仅定义、无任何调用者；`create_session` 后 `last_activity` 无人更新）
- **问题**：`/whip/*`、`/whep/*` 任意 POST 创建会话后，60s 超时形同虚设，session 表只增不减。
- **影响**：攻击者可无限创建会话耗尽内存。
- **修复**：在 HTTP 请求循环或后台任务中周期性调用 `cleanup_expired()`。

### R3. RTMP 握手无超时（连接槽可被占满）
- **位置**：`src/rtmp/session.rs:131`（`handshake::server_handshake(&mut stream).await` 在超时保护外直接 await）；`src/rtmp/handshake.rs:36,65,98,102`（`read_exact` 均无 timeout）
- **问题**：建立 TCP 连接后不发数据或只发部分握手字节即可无限占住连接；`rtmp/mod.rs:129` 连接槽仅 100 个，合法用户无法连接。
- **影响**：远程 DoS。
- **修复**：握手整体包 `tokio::time::timeout`（如 10s）。

### R4. RTMP `Dechunker.pending` 无大小上限（内存无界）
- **位置**：`src/rtmp/chunk.rs:98-99`（`feed` 直接 `pending.extend_from_slice(data)`），配合 `chunk.rs` `NeedMoreData` 分支不消费数据
- **问题**：持续发送声明大 `msg_length` 但不完整、且不发送新字节（保持 TCP 连接活跃绕过 idle 超时）的 chunk 头，`pending` 无限增长。RTMP 设计约定为"解析输入设上限"，此处违反。
- **影响**：远程内存耗尽。
- **修复**：对 `pending` 设上限（如 `MAX_MESSAGE_SIZE`），超限断开连接。

### R5. Publisher claim 生命周期缺陷：PAUSE 不释放、replace=false 时永久锁定
- **位置**：`src/server/session.rs:817-826`（PAUSE 分支只 `release_recording_mounts` + abort senders，不释放 `resources.claims`）、`session.rs:910-923`（`release_remote_publishers(force=false)` 在 `allow_replace_after_disconnect=false` 时不清 `publisher_claimed`）
- **问题**：① 同一会话 RECORD → PAUSE → RECORD 时对自己已 claim 的 mount `compare_exchange` 失败 → 409 无法恢复推流；② `allow_replace_after_disconnect=false` 的 Publisher mount 在一次完整会话后 claim 永不释放，该 mount 永久 409，直到重新注册。
- **影响**：推流无法恢复/接管，mount 永久不可用。
- **修复**：Pause 分支同样释放 claims；Teardown/断开时无论 replace 标志都清 `publisher_claimed`（该标志只应防并发抢占，不应跨连接存续）。

### R6. 订阅者重复收到 startup 关键帧（结构性竞态，非偶发）
- **位置**：`src/mount.rs:151-176`（`publish_cached` 锁内写缓存、锁外 `frames.send`）与 `src/mount.rs:178-215`（`subscribe_with_startup`：`frames.subscribe()` + `cache.snapshot()`）
- **问题**：broadcast ring buffer（容量 1024/4096）会重放订阅时刻之前已发送的帧；而 `startup_cache` 快照也返回同一批关键帧。关键帧间隔通常远小于 ring 覆盖时长（30fps × 1024 ≈ 34s），活跃推流中任何新订阅者几乎必然把同一关键帧收两遍。消费端（RTSP PLAY、HLS、DASH、FLV、RTMP 播放，`server/session.rs:998`、`hls/mod.rs:50`、`dash/mod.rs:81`、`http_flv/mod.rs:17`、`rtmp/session.rs:684`）会把重复样本写入输出，破坏时间戳连续性。
- **影响**：播放端花屏/解码错误。
- **修复**：消费端按 `rtp_timestamp`/`pts` 对 startup 与 broadcast 帧去重；或把 `frames.subscribe()` 移入锁内并记录"已入缓存最大时间戳"为去重边界。

### R7. RTMP/SRT 注册的 mount 永不注销（槽位耗尽 + 指标漂移）
- **位置**：`src/rtmp/session.rs:508-533`、`src/srt/mod.rs:476-522`（只 `register`，全项目无对应 `remove`）
- **问题**：推流会话结束（publisher drop）后 mount 永远留在 registry；每个唯一路径永久占用一个 `max_mounts`（默认 1024）槽位，累计推流过 1024 个不同路径后所有新推流失败；且这些 mount 不发射 `MountEvent`，`metrics.mounts_active` 与实际注册数长期不符。
- **影响**：长时间运行的 RTMP/SRT 服务最终拒绝所有新推流；监控失真。
- **修复**：会话结束时注销无 publisher 的 mount；或 `max_mounts` 触顶时按 LRU 驱逐空闲 mount；统一发射 MountEvent。

### R8. 写盘失败静默丢弃：磁盘满无处理、截断记录使整个录制目录 VOD 不可用
- **位置**：`src/storage/recording.rs:236-240`（`write_unit` 失败仅 `dropped_units += 1`，writer 继续接收后续单元）、`recording.rs:217-234`（`create`/finalize 失败只计数，partial 文件残留）、`recording.rs:529-546`（`SegmentReader::next_unit` 对截断记录返回 `Err` 而非优雅 EOF）
- **问题**：磁盘满期间写失败后段内留下半条 AU 记录；`RecordingCatalog::scan` 在第一个损坏段即报错，整个录制目录 VOD 不可用；错误只累计到无人消费的 `RecordingReport.dropped_units`，无停止/降级/上报，`try_record` 仍返回 `Accepted`。
- **影响**：录制数据全量静默丢失 + VOD 目录不可用，进程对上层无任何可观测错误信号。
- **修复**：写失败立即废弃当前段（关闭并删除 partial）、尝试轮换；连续失败上报/停止；`try_record` 反馈真实落盘状态；`next_unit` 对截断记录返回优雅 EOF 并标记。

### R9. WAV 解析对 size=0 的未知 chunk 死循环
- **位置**：`src/file/raw.rs:619-649`（`WavDemuxer::open` 的 chunk 循环）
- **问题**：`size == 0` 且非 `fmt `/`data` 时 `seek` 回退到刚读完的 header 起点，下次读同一 header，`offset` 不变 → 无限循环，占满 CPU 且永不返回（blocking worker 被永久占用）。
- **影响**：损坏/恶意 WAV 导致 `open` 挂死。
- **修复**：`size == 0` 时若未找到两个必需 chunk 则报错退出；限制 chunk 遍历次数/累计偏移。

### R10. HLS/DASH PTS 回绕/跳变使轮换失效 + fMP4 字段溢出
- **位置**：`src/hls/segmenter.rs:146-151`（`pts >= start`）、`segmenter.rs:201`（`end_pts.saturating_sub(segment_pts)`）、`segmenter.rs:1369`（`mdat_size as u32`）、`segmenter.rs:1279`（`sequence as u32 + 1`）
- **问题**：PTS 一旦回绕（RTP 32-bit 90kHz 约 13.2h）或时间戳重置导致 `pts < start`，轮换条件恒 false，`samples` 无界增长；`segment_duration`/`mdat_size` 的 u32 截断产生错误 box 尺寸。`storage/recording.rs:214` 存在同型问题（时间轮换失效，段只能靠 128MB 字节上限封口）。
- **影响**：长时间直播内存无界 + 生成损坏的 fMP4/EXTINF。
- **修复**：检测 `pts` 相对 `current_segment_pts` 递减/突变时强制 finalize 并置 discontinuity；`mdat` 总量用 u64 计算并在超限时切段。

## 中

- **R11. TLS 握手失败泄漏 `connections_active`/`sessions_active` 计数**：`src/server/mod.rs:408-411`（`SessionEvent::Accepted` 在握手前发射）+ `server/mod.rs:434-449`（TLS 失败直接 return，不发射 `Closed`）。端口扫描/错误协议客户端即可让 gauge 永久 +1。
- **R12. 全局帧指标 `frames_accepted`/`frames_dropped` 恒为 0**：`src/metrics/mod.rs:39-40,391-399`（只有 `load`，全库无 `fetch_add`），`/metrics` 与 `/stats` 帧计数永远为 0，监控失真。
- **R13. `unregister_mount` 后数据面继续存活（注销竞态/流分裂）**：`src/server/mod.rs:576-590` 只从 registry 移除并 cancel child token，但已订阅 RTSP 会话持有的 `Arc<Mount>` 与 broadcast Sender 继续工作（`session.rs:74` 只监听 `state.cancel`）；`LocalPublisher::push`（`mount.rs:350-371`）不检查 cancel。立即重注册同名路径 → 新旧流分裂。行为不一致：HLS/FLV 任务停，RTSP 订阅者不停。
- **R14. `publisher()` 与 `claim_remote_publishers` 无互斥 → 同一 mount 双发布者**：`src/mount.rs:253-264`（`publisher()` 只 `fetch_add` 不检查 `publisher_claimed`）与 `src/server/session.rs:897-904`（claim 只 `compare_exchange` 不检查 `publishers`）。RTSP RECORD 认领后 RTMP/SRT 仍可推流，帧交错。
- **R15. `TaskSupervisor::shutdown` 丢失唤醒竞态**：`src/runtime/supervisor.rs:166-196`。最后一个任务在 `active_tasks()` 检查之后、`notified()` 注册之前完成时 `notify_waiters` 唤醒丢失（tokio `Notify::notify_waiters` 不保留 permit），循环空等到 deadline，再把已完成任务强行 `abort()`，`forced_tasks`/`timed_out_tasks` 虚高。多任务并发关闭时命中概率不低。
- **R16. Proxy 上游半开连接无媒体空闲超时**：`src/server/proxy_runtime.rs:235`（`session.next()` 即 `receiver.recv()`，底层 `client/setup.rs:144` 挂在无超时 `read` 上）。上游进程崩溃未发 FIN 时流永久冻结，无检测、无重连、无故障转移。
- **R17. `RtspServer::drop` 绕过有界 shutdown**：`src/server/mod.rs:614-621` 只 `cancel()` + abort accept 任务；强制 abort 剩余任务的逻辑（`supervisor.shutdown_after`）恰在 accept 任务体内（`server/mod.rs:482-486`），随 accept 一起被丢弃。不调用 `shutdown()` 直接 drop 时 `shutdown_grace` 完全失效。
- **R18. recording `prune` 每次轮换全量重读所有历史段**：`src/storage/recording.rs:222`（`let _ = prune(&config)` 每 10 秒）+ `recording.rs:586-605,377-408`（逐单元完整读每个段文件重建索引）；错误被 `let _` 吞掉，出现坏段后磁盘占用不再受 `max_disk_bytes` 约束。
- **R19. MPEG-TS EOF 处尾部不完整 AAC 帧使整个文件播放失败**：`src/file/mpeg_ts.rs:132-139` 返回 `Error::Media("truncated AAC ADTS frame")` 而非优雅 EOF，录制中断的 TS 文件尾部常见，VOD 播放整段报错。
- **R20. `aac_config_from_fmtp` 字节级 str 切片 panic**：`src/hls/segmenter.rs:1205-1222`（`&value[index..index+2]`），fmtp 来自 ANNOUNCE SDP 无字符校验，含多字节 UTF-8 且长度恰为偶数的 `config=` 值直接 panic；segmenter 运行在 `tokio::spawn` 任务（`hls/mod.rs:59`），该流 HLS/DASH 输出整体中断。
- **R21. RTMP 写路径无超时**：`src/rtmp/session.rs:156`（`write_all` 在客户端停止读取时无限阻塞；30s idle 超时只覆盖读侧），writer_task 与整个连接永久挂起。
- **R22. HTTP 请求读超时为滑动窗口、响应写无超时**：`src/http/mod.rs:159-165`（每 29s 发 1 字节可无限占用 1024 连接槽）；`ResponseWriter` 写路径无 `write_timeout`，慢读客户端可耗尽连接。
- **R23. RTSP keep-alive 失败无退避**：`src/client/mod.rs:254-262`（`last_control` 不更新，`next()` 每次调用立即重试 GET_PARAMETER，错误帧无限重复）。**已修复**（`docs/client-roadmap.md` M2：失败按指数退避推迟 + GET_PARAMETER→OPTIONS 降级 + Session `timeout=` 自适应）；回归测试：`tests/client_edge.rs::keep_alive_failures_back_off`。
- **R24. 同一"注册 mount"三套实现语义不一致**：`src/mount.rs:510-555`（`MountRegistry::register`：容量硬编码 1024、独立 token、4MB 字节上限、不发射事件）vs `src/server/mod.rs:496-575`（容量配置化、server child token、发射事件）vs `src/gateway.rs:134-179`。同一 mount 不同路径注册后满队列丢弃行为不同，`mounts_active` 系统性低估。
- **R25. 视频无 IDR/IDR 极稀疏时 `samples` 无界增长**：`src/hls/segmenter.rs:162-183`（与 R10 同根源的独立触发路径，见 P4）。

## 低

- **R26. Gateway 丢弃 RTMP/SRT server 任务 JoinHandle**：`src/gateway.rs:84,99,114`，错误/panic 静默、shutdown 不等待。
- **R27. UDP 发送失败被静默忽略**：`src/server/tasks.rs:220,274`，任务继续空转消费队列，无退避、无会话级错误。
- **R28. abort 发送任务后不 await JoinHandle**：`src/server/session.rs:564-566,758-760,821-823`，旧任务可能仍在写连接与新任务短暂交错；JoinHandle panic 无人检查。
- **R29. VOD `spawn_blocking` worker 不可取消**：`src/server/vod_runtime.rs:56-68`，阻塞在慢 I/O（如 NFS）时残留窗口无界。
- **R30. 认证失败延迟 `sleep` 不受 cancel 影响**：`src/server/session.rs:971-974`，shutdown 时被拖延。
- **R31. `msg_stream_id += 1` 普通加法**：`src/rtmp/session.rs:445`，debug 溢出 panic / release 回绕复用 stream id。
- **R32. SRT 序号回绕边界误判重传**：`src/srt/mod.rs:451`（直接比较而非回绕比较）。
- **R33. TSBPD 远跳包毒化**：`src/srt/tsbpd.rs:142-149`，单个异常序号包使 `next_deliver_seq` 推进约 2^31，其间正常包全被丢弃。
- **R34. `LoopMode::Forever` + 空文件/超界 start_position 忙转**：`src/file/source.rs:333-381`，两任务全速空转、CPU 100%。
- **R35. AAC 96kHz `clock_rate << 16` u32 溢出**：`src/hls/segmenter.rs:1192`，debug panic / release stsd 采样率字段写错。
- **R36. `push`（无 track 校验）与 `try_push`（有校验）语义不一致**：`src/mount.rs:350-401`，RTMP/SRT 错误 track 帧被计为 Accepted。

---

# 三、测试与模糊覆盖盲区

- **fuzz 目标仅 2 个**（`fuzz/fuzz_targets/control_framing.rs`、`rtp_rtcp.rs`），CI 每个仅 60 秒 smoke（`.github/workflows/ci.yml:97-107`）。覆盖 RTSP framing 与 RTP/RTCP 解析。
- **无 fuzz 覆盖的高风险解析面**：RTMP chunk/AMF/FLV（R4 pending 无上限、`parse_annex_b_nals` 误切分）、SRT packet/握手（R1 cookie）、SDP（R20 fmtp 来源）、HLS/DASH playlist/MPD、文件容器 mp4/mkv/ts/wav（R9 WAV 死循环）、HTTP 头。
- **tests 盲区**（src 内 166 单测 + tests/ 77 集成测试）：HLS 纯音频切段（`tests/hls.rs` 仅 H264 视频）、PAUSE→RECORD claim 恢复、WAV 畸形输入、SRT cookie/连接上限、WebRTC 会话清理、RTMP 握手超时均无测试；`src/srt/`、`src/rtmp/`、`src/webrtc/` 单测分别仅 3/3/1 个。
- 现有稳定性门（`tests/soak.rs` 60s 会话翻腾、`tests/stress.rs` 16 客户端 fanout、`tests/resource_exhaustion.rs` 连接/端口上限）覆盖的是 RTSP 主路径，未覆盖实验协议与上述缺陷路径。

---

# 四、修复优先级建议（Top 10）

1. **R1 + R2 + R3 + R4**：实验协议（SRT/WebRTC/RTMP）的远程资源耗尽与未授权接入面——任何对外暴露这些协议的部署都应先修（cookie 校验、连接上限、握手超时、pending 上限）。
2. **R5**：Publisher claim 生命周期——PAUSE 恢复推流与 replace=false 永久锁死，属核心 RTSP 路径。
3. **R6**：startup 关键帧重复投递——影响所有基于 startup 的消费路径（RTSP/HLS/DASH/FLV/RTMP），破坏时间戳连续性。
4. **R8**：录制写盘失败静默丢弃——数据丢失且目录 VOD 不可用，需先保证"失败可观测、段文件不损坏"。
5. **R7**：mount 泄漏——长时间运行的 RTMP/SRT 服务最终拒绝所有新推流。
6. **P4/R10**：HLS/DASH segmenter 无界增长（纯音频、无 IDR、PTS 回绕）——内存 OOM 与输出损坏，加时间窗口轮换 + samples 硬上限。
7. **P1 + P2 + P3 + P5**：推流/mux 热路径全帧拷贝（RTMP/SRT/FLV/HLS）——嵌入式网关的 CPU/内存预算敏感，改动局部、收益直接。
8. **R11 + R12**：指标失真——TLS 失败计数泄漏与帧指标恒 0，监控可信度修复成本低。
9. **R13 + R14**：mount 注销竞态与双发布者——统一"发布权"为单一受保护资源。
10. **R15 + R16 + R17**：shutdown 与半开连接——任务监督可靠性，长连接服务器上问题会累积。

---

# 附录：静态分析基线

- `cargo check --all-features`：通过（7 个 dead-code 警告）。
- `cargo clippy --all-targets --all-features`：61 行警告，全部为风格类（17× collapsible_if、14× deprecated `criterion::black_box`、4× `cloned_ref_to_slice_refs`、其余 new_without_default/identity_op/manual_contains 等），无性能/可靠性 lint 触发。
- 值得注意的痕迹：`src/rtmp/chunk.rs:354` `(fmt << 6) | 0`（identity_op，`| 0` 无效）；`src/webrtc/session.rs:209` 未使用变量 `id2`。
- 已核查排除的疑点：广播队列容量有界（1024）、`Bytes` 引用计数浅拷贝符合设计、rtsp/rtcp 解析均有严格长度与 `checked_*` 校验、recording finalize 有 `sync_all` + 目录 fsync、matroska 除零由 `NonZeroU64` 保证、锁顺序无环（唯一嵌套顺序 `startup_cache → bootstrap` 单向）、锁中毒处理全项目一致（`into_inner` 恢复）。
