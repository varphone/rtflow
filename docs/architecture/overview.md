# 架构与扩展约定

## 分层

workspace 拆分为 29 个 crate，依赖只允许从上往下（`scripts/check-layering.py`
按各 `Cargo.toml` 的依赖图校验，当前 0 违规、0 环）：

```text
application
   │  rtflow（门面，仅 re-export）
   ▼
gateway ── admin ── protocol adapters
   │        ├── hls, dash
   │        ├── http-flv
   │        ├── rtmp, srt, webrtc, gst
   ▼        ▼
rtsp-server / rtsp-client / source
   │
http ── metrics ── mount ── pipeline ── storage ── file
   │
rtsp ── framing ── runtime
   │
rtp ── rtcp ── srtp ── security ── multicast
   │
media
   │
core
```

- `rtflow-core`：`error`、`identity`、`observe`、`metrics`（无状态计数器）。
- `rtflow-media`：Codec/AccessUnit/描述、位流与时钟工具、`Demuxer`/`VodSource`
  抽象、SDP 值类型（`SdpCrypto`/`SdpConnection`）。
- `rtflow-rtp`、`rtflow-rtcp`、`rtflow-srtp`：RTP 载荷编解包与 jitter、RTCP 包与
  统计、SRTP/SRTCP。
- `rtflow-rtsp`：RTSP 消息、SDP、NPT Range、鉴权、Transport；组播在
  `rtflow-multicast`，TLS 配置在 `rtflow-security`。
- `rtflow-runtime`（任务监督、控制连接、UDP 批量发送）与 `rtflow-framing`
  （RTSP 消息与 `$` interleaved frame 的帧化）。
- `rtflow-mount`（注册表、FrameBus、启动缓存、代理上游状态）、
  `rtflow-pipeline`（Element DAG）、`rtflow-http`、`rtflow-file`、
  `rtflow-storage`、`rtflow-metrics`（Prometheus 导出）。
- `rtflow-rtsp-client`、`rtflow-rtsp-server`、`rtflow-source`（合成测试源）。
- 协议适配器：`rtflow-hls`、`rtflow-dash`、`rtflow-http-flv`、`rtflow-rtmp`、
  `rtflow-srt`、`rtflow-webrtc`、`rtflow-gst`。
- `rtflow-admin`（可选控制台）、`rtflow-gateway`（多协议编排）。
- `rtflow`：唯一对外稳定入口，所有路径保持 `rtflow::*`；`rtflow-testkit` 为
  dev-only 线束。

RTSP 方法/传输/码流/feature 的完整矩阵见 [rtsp-matrix.md](rtsp-matrix.md)。

## 仓库结构

| 目录 | 内容 |
|---|---|
| `crates/<name>/src/` | 每个 crate 的源码；目录结构即模块结构（无 `#[path]`） |
| `crates/<name>/tests/` | 该 crate 的集成测试（目前集中在 `crates/rtflow/tests/`） |
| `crates/rtflow/examples/<area>/` | 示例：`server`/`client`/`protocols`/`gateway`/`embedded`，资源在 `examples/assets/` |
| `crates/rtflow-testkit/` | 不发布的 dev 线束（测试与示例共用） |
| `tests/fixtures/` | 全仓库共用的媒体夹具 |
| `scripts/` | 布局校验、示例冒烟、fixture 生成与发布检查 |
| `docs/` | `architecture`（含 ADR）、`guides`、`audits`、`roadmap`、`archive` |
| `benches/`、`fuzz/` | 性能基线与模糊测试（fuzz 为独立 workspace） |

布局规则由 `scripts/check-layering.py`、`check-tree.py`、`check-docs-paths.sh`
在 CI 中强制；单文件硬上限 800 行，软目标 600 行（当前最大 600 行）。

## 数据面与所有权

`AccessUnit.data`、`RtpPacket.payload` 和 `InterleavedFrame.payload` 使用 `Bytes`。RTP 解析和 interleaved 解帧通过 slice/split 转移所有权，不复制 payload；分片重组、RTP 头拼接和 socket syscall 边界仍会分配。所有队列都有显式容量，慢消费者通过 broadcast lag 隔离，不反压实时采集线程。

设备侧建议把硬件编码输出封装成 `Bytes` 后调用 `LocalPublisher::try_push`。文件后端在专用 blocking worker 中读取，异步侧只负责节流和投递。容器样本读取会产生一个拥有所有权的缓冲区；单 NAL 长度前缀转 Annex-B 可用 `Bytes::slice` 复用该存储，多 NAL/参数集注入只做一次合并分配。进入 RTP、广播和客户端队列后继续共享 `Bytes`。不要在 Tokio IO task 中执行耗时编码、磁盘操作或同步网络访问。

## 会话与任务生命周期

每条 RTSP TCP 连接拥有独立 Session ID 和 `Init → Ready → Playing/Recording/Paused` 状态。全局 shutdown、mount unregister、连接关闭均会终止其作用域内任务。mount 的取消 token 是 server token 的子节点，因此文件 demux worker、代理和会话不会在注销后悬挂。代理只在首个 PLAY 时启动；最后一个订阅者离开并超过 `no_subscriber_grace` 后回到 Idle。文件源默认在无订阅者时保留当前位置并暂停读取（有界预取最多提前若干样本）。

## 扩展新编码

1. 在 `Codec` 添加枚举值并扩展 SDP rtpmap 映射。
2. 在 `Packetizer`/`Depacketizer` 添加 RFC 对应载荷实现。
3. 增加单 NAL、聚合、分片、畸形输入、marker/timestamp 边界测试。
4. 增加 UDP 和 TCP interleaved 端到端用例。

不要在 RTP 层引入解码器；转码应作为独立 pipeline element，通过 `AccessUnit` 连接。

## 当前边界

共享文件 mount 仍适合循环宣传片；`MountConfig::Vod` 则为每个 RTSP session 创建独立 demux、NPT Range 和 PAUSE 游标。RTSPS 保护 RTSP 控制连接及 TCP interleaved 媒体；UDP RTP 若需加密应使用后续 SRTP transport。组播与 SRTP 已作为可选 feature（`multicast`、`srtp`）实现；ONVIF、转码和集群状态复制仍是独立扩展。

## 决策记录与兼容门面

- 架构决策见 [`docs/architecture/decisions/`](decisions/README.md)：
  单 crate（0001）、文件=模块（0002）、分层规则（0003）、兼容与弃用（0004）。
- 0.3 的弃用门面：`protocol`（→ `rtsp`）、`proxy`（→ `mount::{upstream,continuity}`）、
  `protocol_trait`（→ `gateway::protocol`）、`media_pipeline`（→ `pipeline`+`rtp`）、
  `server::{MountConfig,MountHandle,…}`（→ `mount`）。迁移表见
  [`docs/guides/migration/0.2-to-0.3.md`](../guides/migration/0.2-to-0.3.md)。
