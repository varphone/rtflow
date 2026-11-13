# rtflow 易用性审计报告（嵌入式 IPC 接入视角）

- 日期：2026-08（基于当前工作区 HEAD `509e5bf`）
- 目标场景：**海思（HiSilicon）平台 IPC**——HiMPP 的 VENC 输出 H.264/H.265 编码帧，通过 rtflow 以 RTSP 发布，实现主/子多码流网络摄像机
- 方法：以"嵌入式客户从零接入"的最小路径逐环节走查（README 最小示例 → API 形状 → 帧格式契约 → 平台构建），对照现有 examples/docs 能力
- 口径：易用性 = 从拿到 SDK 到出流所需的代码量、需要理解的协议概念、需要自行实现的胶水、以及文档/示例覆盖
- **实施状态（2026-08）：H1-H4 全部落地**——`media::bitstream`/`media::clock` 公开工具、`AccessUnit::video/audio`、`TrackDescription::h264_video/h265_video/aac_audio/pcma_audio/pcmu_audio`、mount 首关键帧自动提取 `sprop-parameter-sets` 并合并进 DESCRIBE、`examples/embedded/ipc_camera.rs` 双码流参考、`docs/guides/embedded.md` 交叉编译/调参指南

## 结论摘要

**核心链路是通的**：`register_mount(Local) + publisher().try_push(AccessUnit)` 即可出流，`data` 直接接受 Annex-B（HiMPP VENC 原始输出格式），H.264/H.265 RTP 打包、RTSP 信令、多客户端、多码流（多 mount）均已就绪。

**但存在 4 个会卡住嵌入式客户的硬摩擦点**（缺 SPS/PPS 注入、无帧构造/时间戳 helper、无"本地推帧"生产示例、无交叉编译文档），以及一批中低优先级的文档/API 缺口。详见下文。

## 一、目标场景的最小接入路径（现状）

```text
HiMPP VENC 回调
  ├─ 编码帧（Annex-B H.264/H.265 es，含/不含 SPS/PPS）
  ├─ 帧类型（IDR / P）
  └─ 时间戳（64-bit us）

→ 用户代码需自行完成：
  1. 构造 MediaDescription / TrackDescription（手写 7+ 字段，需懂 SDP）
  2. 每帧构造 AccessUnit（6 字段：track_id/codec/pts/dts/rtp_timestamp/is_keyframe/data）
  3. rtp_timestamp 换算（us → 90kHz）
  4. 处理 SPS/PPS（若 VENC 分离输出——见下文硬摩擦点 1）
  5. try_push 循环 + 订阅状态管理

→ rtflow 完成：
  RTSP 信令、RTP/RTCP、多客户端、多码流（多 mount）、HLS/HTTP-FLV 分发
```

## 二、硬摩擦点（高优先级）

### H1. SPS/PPS 无注入机制 —— 分离参数集流会直接黑屏

- **现状**：`AccessUnit.data` 按 Annex-B 原样打包成 RTP（`src/rtp/payload.rs:67` 的 `h264()` 逐 NAL 打包）；`TrackDescription.fmtp` 仅是一个 `Option<Arc<str>>`（手填字符串）。**全库没有任何从关键帧提取 SPS/PPS 或生成 `sprop-parameter-sets=` 的工具**（`src/rtp/payload.rs`、`src/protocol/sdp.rs` 均无）。
- **影响**：海思 VENC 常见配置是 **SPS/PPS 分离输出**（独立 buffer，IDR 帧内不含）——按现有路径推流后，播放器拿到的 RTP 流**没有任何参数集**，VLC/ffmpeg 一律黑屏/无法解码。客户必须自己解析 SPS/PPS、base64 编码、拼 fmtp 字符串——这部分代码量不小且极易出错（还要处理 `packetization-mode`、`profile-level-id` 与 SPS 的一致性）。
- **建议**：
  1. 提供 `h264_parameter_sets(data: &[u8]) -> Option<(Bytes /*sps*/, Bytes /*pps*/)>` 公开工具（解析 Annex-B，提取 NAL type 7/8 与 H.265 33/34/32）。
  2. `register_mount` 时若 `fmtp: None` 且后续关键帧携带 SPS/PPS，自动提取并**延迟填充 fmtp**（首个关键帧到达后更新 SDP 的 `a=fmtp`，RTSP DESCRIBE 按需生成——现有 `announce_fmtp` 机制可复用，`src/mount.rs` 已有按 track 存 fmtp 的槽位）。
  3. 提供 `TrackDescription::h264_video(...)` 构造 helper，签名直接收 `(sps, pps, width, height, fps)`，自动生成完整 fmtp（`profile-level-id` + `sprop-parameter-sets` + `packetization-mode=1`）。
- **验收**：只喂"裸 IDR（不含 SPS/PPS）+ 手动提供 sps/pps buffer"，VLC 能解码。

### H2. 帧构造与时间戳换算无 helper

- **现状**：`AccessUnit` 6 个字段全公开、无构造函数（`src/media/types.rs:22`）；README 最小示例手写全部字段。`rtp_timestamp`（90kHz）与 `pts`（Duration）由调用方维护——嵌入式客户要从 HiMPP 的 64-bit us 时间戳换算，且要理解 `rtp_timestamp` 与 `pts` 的**双时钟**约定（RTP 线上时钟 vs 帧率时钟）。
- **影响**：每个接入方重复实现同一段胶水；时间戳换算/回绕错误是 RTSP 联调最常见的坑（本仓库前几轮审计也踩过）。
- **建议**：
  1. `AccessUnit::video(track_id, codec, pts, rtp_timestamp, is_keyframe, data)` 与 `AccessUnit::audio(...)` 构造 helper。
  2. 提供 `RtpClock` 辅助类型：`RtpClock::new(rate)` + `fn timestamp(&mut self, pts: Duration) -> u32`（内部做 `pts → rate 刻度`换算，自动处理 32 位回绕/跳变，参考 `src/server/tasks.rs` 的 `RtpNtpClock` 的扩展思路）。
  3. 文档明确"帧率时钟（pts）与 RTP 时钟（rtp_timestamp）可以不同，只要二者线性一致"。

### H3. 没有"本地程序内推帧"的生产级示例（HiMPP 参考）

- **现状**：examples 中与推流相关的：`publisher_server.rs`（接收**外部 RTSP 推流** ANNOUNCE，不是本地推帧）、`ffmpeg_live_publish.rs`（拉起 ffmpeg 子进程）、`live_stream_demo.rs`（同样依赖 ffmpeg + gstreamer 播放）。**没有任何示例展示"采集线程 → try_push 循环"的形态**——而这正是 HiMPP 接入的全部形态。
- **影响**：客户只能从 README 的 10 行最小示例自己扩展出"循环推帧 + 双码流 + 状态日志 + 优雅退出"，缺少可参照的完整骨架（含 PushResult 处理、帧率节流、订阅状态观测）。
- **建议**：新增 `examples/embedded_publish.rs`（或 `ipc_camera.rs`）：
  - 模拟采集循环（或对接 V4L2），`try_push` 双码流（主 1080p/子 720p 两个 mount，或同一 mount 双轨）
  - 演示 `PushResult`（DroppedNoSubscribers 正常、队列满丢帧策略）
  - 演示 SPS/PPS 分离注入（配合 H1 的工具）
  - 演示订阅状态日志（复用 `snapshot().subscriber_count`）
  - 注释中标明"HiMPP 接入点：把模拟 push 替换为 VENC 回调即可"

### H4. 无交叉编译/嵌入式构建文档

- **现状**：README 有 `embedded-gateway` feature profile 一行，但**没有任何交叉编译指南**：目标三元组（`aarch64-unknown-linux-gnu` / `-musl`）、`~/.cargo/config.toml` 的 linker 配置、静态链接与 musl 的取舍、`tokio` 的 io-uring/线程模型在嵌入式上的 footprint、`bytes`/`Arc` 共享避免拷贝的说明。
- **影响**：嵌入式客户第一关就是"编不出来"——即使代码写对了也跑不起来；没有 `cargo build --target aarch64-unknown-linux-gnu --no-default-features --features embedded-gateway` 的一键验证。
- **建议**：新增 `docs/guides/embedded.md`：目标三元组、交叉工具链示例（海思 SDK 的 aarch64-linux-gnu-gcc）、Cargo 配置片段、musl vs glibc 建议、二进制 footprint 数据（`cargo bloat`/`size` 实测表）、以及在目标板上运行的最小部署包。

## 三、中优先级

### M1. SPS/PPS 相关的基础工具未公开
- `src/file/raw.rs` 的 `AnnexBReader`（Annex-B 切 NAL）、`src/media/bitstream.rs` 的 NAL 类型判定都是**模块私有**。嵌入式用户解析 VENC 输出（判断 IDR、提取参数集、统计 GOP）要自己写。
- 建议：公开 `media::bitstream` 下的 `annex_b_nals`、`nal_type`（H.264/H.265）、`h264_sps_profile_level` 等只读工具（本就无状态、零依赖）。

### M2. `MountConfig::Local` vs `Publisher` 语义对推流方不直观
- README 最小示例用 `Local`；`Publisher` 带 `allow_replace_after_disconnect` 与"发布者断开后挂载可被接管"的语义。对 IPC 场景（设备长期在线、异常重启后重推），两者都能用但语义不同，文档未解释。
- 建议：文档明确：IPC 固定推流用 `Publisher + allow_replace_after_disconnect=true`（断线重推无需重建 mount）；纯本地注入用 `Local`。

### M3. `PushResult` 与订阅状态需要文档化
- `try_push` 返回 `DroppedNoSubscribers`/`DroppedQueueFull`（`src/mount.rs:55`）——首次联调的嵌入式用户会困惑"我推了怎么没有流"。且没有"无订阅者时自动停推"的推荐模式（IPC 场景通常希望有订阅者才编码，省 CPU）。
- 建议：文档化三个返回值语义 + 推荐模式：`snapshot().subscriber_count == 0` 时可停 VENC 或降帧率（省功耗），有订阅者再恢复。

### M4. 性能/内存调参无指南
- `consumer_queue_capacity`（广播队列）、`max_queued_bytes_per_session`、`STARTUP_WINDOW` 等参数对延迟/内存的影响无文档；嵌入式内存紧张，需要"256MB 内存能开几路"这类参考。
- 建议：`docs/guides/embedded.md` 或 README 增加调参表（参数 → 作用 → 建议值 → 对延迟/内存的影响）。

### M5. 多码流（主/子码流）无示例与约定
- 框架支持多 mount（每码流一个 URL：`/cam/main`、`/cam/sub`）或单 mount 多轨（一个 URL 两轨，需客户端同时 SETUP）。IPC 业界惯例是**独立 URL**（客户端按需拉主/子码流）。
- 建议：H3 的示例按"独立 URL 双码流"实现；文档说明两种模式各自的适用场景（独立 URL = 拉流方按需选择；多轨 = 一次协商全要，浪费带宽）。

## 四、低优先级

- L1. `AccessUnit`/`TrackDescription` 缺 `Debug` 友好打印（大 Bytes 截断）——排查现场问题时的日志噪音。
- L2. 无 `RUST_LOG` 快速上手说明（`RUST_LOG=rtflow=debug` 的模块层级）——联调第一件事。
- L3. `publisher()` 返回 `Result`，何时 `Err`（挂载已失效/类型不匹配）文档缺失。
- L4. examples 统一入口：现有 17 个示例各自 `--help` 风格略有差异；`docs/guides/examples.md` 已列，但缺"IPC 场景该看哪几个"的导航。
- L5. `TrackId`/`payload_type`/`clock_rate` 的默认约定（视频 90000、96）无显式文档——建议 helper 内聚（见 H2）。

## 五、建议的"IPC 快速接入包"（改进蓝图）

按依赖顺序：

1. **帧侧 helper**（H2 + H1.1/M1）：`AccessUnit::video/audio`、`RtpClock`、公开 `bitstream` 只读工具、`h264_parameter_sets`/`h265_parameter_sets`。
2. **描述自动生成**（H1.3）：`TrackDescription::h264_video(sps, pps, w, h, fps)`（自动 fmtp）与 `MediaDescription::camera(...)`（主码流/子码流两个 mount 的完整描述）。
3. **运行时注入**（H1.2）：首关键帧到达时若 fmtp 为空且帧含 SPS/PPS，自动提取并延迟生成 SDP fmtp（复用 `announce_fmtp` 槽位）。
4. **示例**（H3）：`examples/embedded/ipc_camera.rs`——双码流、SPS/PPS 分离注入、订阅感知省电、优雅退出。
5. **文档**（H4 + M4 + M2/M3/L2）：`docs/guides/embedded.md`（交叉编译 + 调参 + 部署）+ README 导航。

其中 1-4 的代码量估算：约 300-400 行 + 1 个示例 + 2-4 个测试；5 约 200 行文档。1 和 2 是纯增量 API（不破坏现有用法）；3 需要小心 DESCRIBE 时机的改动（fmtp 从"注册时固定"变为"可延迟确定"）。

## 六、验证建议

- 每个 helper 配单元测试（SPS/PPS 提取用真实 H.264 夹具；`RtpClock` 覆盖回绕/负跳变）。
- `ipc_camera` 示例配集成测试：注册双 mount → push 分离参数集帧 → 客户端 DESCRIBE 能看到 `sprop-parameter-sets` → 播放器可解码（用 ffmpeg 收流校验）。
- 交叉编译：CI 加 `cargo build --target aarch64-unknown-linux-gnu --no-default-features --features embedded-gateway`（若 CI 有交叉工具链）或文档化本地验证命令。
