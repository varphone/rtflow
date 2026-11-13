# 架构与扩展约定

## 分层

```text
application / device integration
        │ AccessUnit / MediaDescription
        ▼
file/storage ── pipeline ── server ── client ── proxy
  │            │             │
  └────── runtime/framing ───┘
               │
       protocol/{rtsp,sdp,range,auth,transport}
               │
        rtp + rtcp + media
```

- `file/`：统一 `Demuxer`、容器/裸流后端、时间戳节流、循环/seek、阻塞磁盘 worker 和有界预取；可直达本地 mount 或 Element DAG ingress。
- `media/`：稳定领域类型、有界 fan-out，以及可运行的 Element DAG；每个节点和 Link 都有独立有界队列。
- `protocol/`：RTSP、SDP、鉴权和 Transport 的纯协议解析/编码，输入全部设上限。
- `rtp/`、`rtcp/`：载荷编解包、jitter buffer 和控制报告。
- `runtime/`：连接 framing，把 RTSP 消息和 `$` interleaved frame 从同一字节流中分离。
- `security/`：CIDR ACL、有界连接限流以及 Rustls 客户端/服务端配置。
- `storage/`：崩溃安全分段录制、恢复扫描、保留策略和每会话 VOD source。
- `client/`：播放会话、Content-Base/control URI 解析、每轨接收任务、jitter 定时排空与自动保活。
- `server/`：mount registry、会话状态机及发送/接收任务。
- `proxy/`：上游配置、状态和重连策略；Proxy mount 可自动 DESCRIBE，上游 supervisor 由 mount 生命周期托管。

根模块继续导出 `rtflow::media`、`rtflow::rtp` 等稳定门面。物理目录可以继续拆分，不要求调用方同步修改 import。

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

共享文件 mount 仍适合循环宣传片；`MountConfig::Vod` 则为每个 RTSP session 创建独立 demux、NPT Range 和 PAUSE 游标。RTSPS 保护 RTSP 控制连接及 TCP interleaved 媒体；UDP RTP 若需加密应使用后续 SRTP transport。组播、SRTP、ONVIF、转码和集群状态复制仍是独立扩展。
