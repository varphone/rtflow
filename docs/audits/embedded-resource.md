# 嵌入式资源受限 IPC 适用性深度审核

> 场景：小 RAM / 小 ROM 的 IPC（如海思 HiMPP、君正、联咏等平台；典型 64–256MB RAM、8–32MB NOR/NAND Flash）。
> 目标：把 HiMPP VENC 输出的 H.264/H.265 + AAC/PCM 码流通过本框架发布为 RTSP 多码流 IPC。
> 日期：2026-08（基于当前 git HEAD 实测）。

## 0. 结论

框架的**架构层面对嵌入式友好**（零拷贝帧传递、异步 IO、特性裁剪、`embedded-gateway` 特性集、单二进制），但**工程化层尚未完成 IPC 落地所需的"资源预算与调优"闭环**：

- ROM：`embedded-gateway` 特性 release+strip 实测 **4.8MB**（全特性 7.1MB）——8MB Flash 紧张、16MB 可行；通过构建优化可再降 ~30%。
- RAM：**最大风险是每消费会话 256 帧 broadcast 队列**（1080p@4Mbps ≈ 峰值 4MB/会话）与 **HLS 段内存**（2s 段 × 码率，lookback 5 段 ≈ 峰值 5MB）——必须按码率预算配置。
- CPU：帧路径零深拷贝（`Bytes` 引用计数），单核可处理 1080p 预览；但多线程 tokio runtime 默认开启，弱核平台需 `current_thread`。
- 协议栈取舍：WebRTC/SRT/DASH/RTMP 特性全开 +2.3MB ROM，IPC 预览场景建议只用 `embedded-gateway`。

## 1. 实测基线（本仓库 HEAD）

| 项目 | 数值 | 说明 |
|---|---|---|
| release 二进制（embedded-gateway, 未 strip） | 6.5MB | `examples/ipc_camera` |
| release 二进制（embedded-gateway, strip） | **4.8MB** | `strip` 后 |
| release 二进制（all-features, 未 strip） | 7.1MB | 含 webrtc/srtp/dash/rtmp/mkv |
| WebRTC+SRTP+DASH+RTMP+MKV 增量 | ≈ +2.3MB | 按需裁剪 |
| 依赖树 | full 315 节点 / embedded-gateway 23 节点 | webrtc-ice/dtls 是主要膨胀源 |
| tokio 特性 | `rt-multi-thread` 等 9 项 | `process/signal/fs` 对纯流媒体无用 |
| startup 快照窗口 | 500ms（`STARTUP_WINDOW`） | 已收敛，0.5s×码率 |
| 每会话 broadcast 队列 | `consumer_queue_capacity=256` 帧（默认） | 1080p@4Mbps ≈ 4MB 峰值 |
| HLS 段 | 2s/段；内存构建完整段；`lookback_count=5` | 1080p@4Mbps ≈ 峰值 5MB+ |
| SRT TSBPD | reorder 窗口 + 重传缓冲（240ms×码率） | 大数据量，IPC 慎用 |
| 帧拷贝 | 发布→broadcast→RTP 打包全程 `Bytes` 浅拷贝 | 每帧 0 次深拷贝 ✓ |

## 2. RAM 预算（每客户端，1080p 4Mbps 30fps 参考）

| 组件 | 峰值估算 | 可控项 |
|---|---|---|
| broadcast 队列（256 帧 × ~16KB） | ≤ 4MB | `consumer_queue_capacity`（建议 64–128） |
| RTP 打包缓冲 | ~1 帧 + 分片复用 | 已复用 `BytesMut` ✓ |
| startup 快照 | ~0.25MB（500ms） | `STARTUP_WINDOW` |
| TCP/内核缓冲 | ~0.3MB（收发各 64–128KB ×2） | socket 选项 |
| TLS（若开） | ~50–100KB/连接 | 可选特性 |
| task 栈 | 2MB 虚拟/个（guard 页，实占小） | 每连接 2–3 个 task |
| HLS writer（若开） | 段构建 1MB + lookback 5MB 峰值 | `lookback_count` 降到 2–3 |
| **合计（3 路预览 + 1 路 HLS）** | **≈ 12–18MB** | 64MB RAM 设备可行，需配低码率 |

关键结论：**内存大头随"并发客户端数 × 码率"线性增长**，多码流 IPC（主 2Mbps + 子 512Kbps + 第三码流 256Kbps）必须按此预算，而不是按帧数配容量。

## 3. 需要做的工作清单（按优先级）

### P0 — IPC 落地必需

1. **发布构建配置**（`.cargo/config.toml` + `docs/guides/embedded.md` 补全）
   - `profile.release`：`lto = true`、`panic = "abort"`、`codegen-units = 1`、`strip = "symbols"`、`opt-level = "z"`（实测 4.8MB → 预计 3.5MB 级）
   - 交叉编译 target 配置（aarch64-linux-gnu / armv7）与 strip 命令文档化
   - 特性矩阵固化：IPC 预览 = `embedded-gateway`；加 HLS = `+hls`；不要 `webrtc/srt/dash/rtmp/mkv`

2. **按码率的队列容量预算文档 + 示例**
   - `consumer_queue_capacity` 公式：`容量 × 平均帧大小 ≤ 目标内存/会话`（如 1080p 主码流 128 帧 ≈ 2MB）
   - `ipc_camera` 示例给出"主码流 128 / 子码流 64"的推荐配置注释

3. **弱核 runtime 指引**
   - 框架不建 runtime（用户 main 决定）✓；`docs/guides/embedded.md` 补 `#[tokio::main(flavor = "current_thread")]` 的适用条件（纯 RTSP 预览 ≤ 4 客户端）
   - 多核平台：每核一个 `current_thread` runtime 各管一路码流（避免多线程锁竞争）——文档级方案

4. **内存基线测试方法**（文档 + 可选示例）
   - 提供 `examples/mem_probe`（或文档脚本）：连 N 路客户端后读 `/proc/self/status` 的 `VmHWM`，给出 IPC 验收基线命令
   - 建立"每会话内存增量"回归基准（当前无此测试）

5. **日志开销裁剪**
   - 发布配置 `tracing` `release_max_level_info`（或 `warn`）；禁用 `debug`/`trace` 编译路径

### P1 — 常见 IPC 场景增强

6. **HLS 内存收敛**（若 IPC 要 HLS 回看/预览）
   - `lookback_count` 默认 5 → 建议 2–3（配文档）；段数据写盘后立即释放的路径已有（broadcast 满即弃）✓ 但要验证 writer 阻塞时的峰值
   - 或提供"段直写文件流"模式（当前整段内存构建，2s×码率 峰值 1MB 可接受，4s×高码率需注意）

7. **多码流共享发布**（已支持，补文档）
   - 主/子码流独立 `Mount` + 独立 `publisher()`，HiMPP 双 VENC 线程各推一路 ✓
   - 文档补"按订阅者数暂停低优先级码流"的节能策略（`subscriber_count` 已暴露 ✓）

8. **掉电/异常恢复**
   - 录制文件（若用 `file-storage`）的 fsync 策略、断流 `DISCONTINUITY` 语义文档化

### P2 — 长期优化（超出当前框架范围）

9. **HiMPP VB 零拷贝接入**：将 VB 块包装为 `Bytes`（自定义 `BytesMut` 底层或 `Bytes::from_owner`）避免 VENC 输出 → Rust 堆的拷贝；当前 `ipc_camera` 是 Vec 拷贝路径
10. **io_uring / 零拷贝 socket 发送**（`tokio-uring`）评估：弱核高并发下 syscall 开销
11. **内存分配器**：文档评估 jemalloc（碎片/性能）；或 glibc `MALLOC_ARENA_MAX=2` 等调优（多线程 runtime 下 arena 膨胀）
12. **池化 RTP 包头**：发布热路径的小对象池（当前每帧若干次小分配，弱核上有意义）
13. **SRT/WebRTC 缓冲上限**：IPC 上若启用，需按码率裁剪 TSBPD 窗口与 jitter buffer（当前无配置项）

## 4. 与现状的差距摘要

| 维度 | 现状 | IPC 落地需要 |
|---|---|---|
| ROM | 4.8MB（embedded-gateway） | 构建优化 + 交叉编译脚本（P0-1） |
| RAM | 每会话峰值 4MB（默认 256 帧） | 容量预算文档 + 示例配置（P0-2） |
| CPU | 多线程 runtime 默认 | current_thread 指引（P0-3） |
| 可观测 | tracing 全级别编译 | release 级别裁剪（P0-5） |
| 验证 | 无内存基线 | mem_probe + VmHWM 验收（P0-4） |

## 5. 建议的落地顺序（一次迭代）

1. P0-1 构建配置（30 分钟，收益最大：ROM −30%）
2. P0-2 队列预算文档（1 小时，防线上内存事故）
3. P0-3/P0-5 文档 + 日志裁剪（1 小时）
4. P0-4 mem_probe 示例 + 基线（半天）
5. P1 按产品需求取舍 HLS/录制

**总体判断**：架构无需改动即可支撑小内存 IPC（零拷贝 + 特性裁剪 + 配置项已具备）；缺的是"预算文档 + 构建固化 + 基线工具"这三件工程化工作，完成即达到可交付状态。
