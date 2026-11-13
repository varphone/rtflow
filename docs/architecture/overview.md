# 架构与扩展约定

## 分层

依赖只允许从上往下（脚本 `scripts/check-layering.py` 强制，当前依赖图
SCC = 0、上行依赖 = 0）：

```text
application / device integration
        │ AccessUnit / MediaDescription
        ▼
gateway ── protocols/{hls,dash,http_flv,rtmp,srt,webrtc,gst} ── server/client
   │                    │                                        │
   │                    └─────────────┬──────────────────────────┘
   │                                  ▼
   │            http / metrics(exporter) / mount / pipeline
   │                                  │
   │            segment / file / storage / runtime
   ▼                                  │
rtsp/{message,sdp,range,auth,token_auth,transport,multicast}
   │                                  │
   └──────── rtp / rtcp / srtp / security ──────────┘
                                      │
                        core/{error,identity,observe,metrics} + media
```

- `core/`：`error`、`identity`、`observe`（事件与 `EventSink`）、`metrics`
  （无状态的计数器/直方图注册表与事件汇）。整个 crate 的最底层。
- `media/`：稳定领域类型、位流/时钟/FLV 工具、容器无关的 `Demuxer`、
  `VodSource`、SDP 值类型（`SdpCrypto`/`SdpConnection`）与 fMP4 写出。
- `rtsp/`：RTSP 消息编解码、SDP、NPT Range、鉴权、Transport 与组播，输入
  全部设上限；根模块继续以 `rtflow::auth`、`rtflow::transport` 等路径再导出。
- `rtp/`、`rtcp/`、`srtp/`：载荷编解包、jitter buffer、控制报告与 SRTP。
- `security/`：CIDR ACL、有界连接限流以及 Rustls 客户端/服务端配置。
- `runtime/`：连接 framing，把 RTSP 消息和 `$` interleaved frame 从同一字节流中分离。
- `pipeline/`：可运行的 Element DAG，每个节点和 Link 都有独立有界队列。
- `mount/`：流注册表、发布者句柄、启动关键帧缓存，以及代理的上游配置/状态
  （`upstream`）与时间线连续性（`continuity`）。
- `http/`、`metrics/`：HTTP 服务与 Prometheus/`/stats` 导出（注册表在 `core`）。
- `client/`、`server/`：RTSP 播放客户端（控制面独立任务、重连、keep-alive）
  与服务端会话状态机、代理/VOD 运行时。
- `file/`、`storage/`：容器解复用与文件播放；崩溃安全录制、恢复与 VOD source。
- `protocols/`：HLS、DASH、HTTP-FLV/WS-FLV、RTMP、SRT、WebRTC、GStreamer
  适配器；根模块以 `rtflow::hls` 等路径再导出。
- `proxy/`：兼容门面，类型已迁入 `mount::{upstream,continuity}`（0.3 弃用）。
- `gateway/`：多协议编排入口。

## 仓库结构

| 目录 | 内容 |
|---|---|
| `src/` | 按上文分层组织的 crate 源码；目录结构即模块结构（无 `#[path]`） |
| `tests/<area>/` | 集成测试：`rtsp`/`client`/`proxy`/`protocols`/`security`/`storage`/`interop`/`system`，每个域一个测试二进制 |
| `examples/<area>/` | 示例：`server`/`client`/`protocols`/`gateway`/`embedded`，资源在 `examples/assets/` |
| `support/` | 不发布的 dev 线束 `rtflow-testkit`（测试与示例共用） |
| `scripts/` | 布局校验、示例冒烟、fixture 生成与发布检查 |
| `docs/` | `architecture`（含 ADR）、`guides`、`audits`、`roadmap`、`archive` |
| `benches/`、`fuzz/` | 性能基线与模糊测试（fuzz 为独立 workspace） |

布局规则由 `scripts/check-layering.py`、`check-tree.py`、`check-docs-paths.sh`
在 CI 中强制；单文件硬上限 800 行，软目标 600 行（当前最大 587 行）。

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
