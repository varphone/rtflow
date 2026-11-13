# 第二期稳定性与商业化基线

第二期把框架从“协议和媒体链路可用”推进到“可观测、可限制、可恢复、可发布”。核心数据面仍只传递编码后的 `Bytes`，TLS、鉴权、录制索引等控制面能力不改变 RTP payload 热路径。

## 已完成能力

- 稳定错误分类：`ErrorCode`、`RetryClass`、`Operation`，可映射重试、RTSP 状态和指标标签。
- 稳定身份：Session、Task、Mount、Track、Stream epoch 强类型 ID；服务、会话、挂载、代理、管线、运行时和安全事件统一进入 `EventSink`。
- 任务监督：任务完成、错误、panic、取消和强制终止均可计数；服务和管线支持有期限的协作式 shutdown。
- 资源治理：全局/单 IP 会话、mount、track、帧、排队字节、UDP 端口和连接速率均有显式上限；ACL 支持 IPv4/IPv6 CIDR。
- 安全控制：Basic/Digest 常量时间比较，HMAC-SHA256 签名 nonce、TTL 和 nonce-count 防重放，鉴权失败延迟与审计事件。
- RTSPS：客户端和服务端基于 Rustls，支持 PEM 配置、SNI/自定义根证书、握手超时和失败事件；代理可为每个上游独立配置 TLS 信任；TCP 与 TLS 共享 read-ahead 连接抽象。
- RTCP：复合包、SR/RR report block、SDES、BYE、未知包保留、loss/jitter/wrap/RTT 计算及客户端质量快照。
- 代理可靠性：多上游选择、熔断/退避、描述兼容检查、时间线连续、关键帧/GOP 启动缓存和异常状态恢复。
- 录制/VOD：有界异步入口、原子 `.partial → .rtspseg` 分段、可选 fsync、磁盘/段数保留、崩溃残留恢复、目录索引和 seek。
- 每会话 VOD：`Range: npt=`、独立 demux/游标、UDP/TCP 输出、PAUSE 后续播；各客户端互不共享播放时间线。
- 播放兼容：直播接受 GStreamer 等客户端使用的 `Range: npt=0-`/`now-`，VOD 保留精确 seek；真实 FFmpeg/GStreamer 音视频链路纳入 CI 门禁。
- 发布门禁：严格 Clippy、默认/外部互操作测试、RTSPS、资源耗尽、fuzz 目标、热路径基准、定时 soak 和 `cargo package`。

## 目录边界

```text
protocol/   RTSP、SDP、Range、鉴权和 Transport 纯协议
media/      稳定领域类型、FrameBus 和 Element DAG
runtime/    framing、TCP/TLS 控制连接和任务监督
security/   ACL、限流、TLS 配置
storage/    分段录制、目录索引、VOD source
client/     控制会话、UDP/TCP 接收、质量统计
server/     会话状态机、RTP sender/receiver、proxy/VOD runtime
proxy/      上游策略、连续性、启动缓存、故障转移
rtp/rtcp/   数据面与控制面包处理
```

`rtflow::recording`、`rtflow::tls` 等兼容门面继续保留，新的实现分别归入 `storage` 和 `security`，便于后续拆分 crate 而不迫使调用方同步迁移。

## 运行发布门禁

```text
./tools/release-check.sh
cargo bench --bench hot_path
cargo test --test soak -- --ignored --nocapture
cargo fuzz run control_framing
cargo fuzz run rtp_rtcp
```

商业部署仍需由产品侧提供证书轮换、密钥托管、账号/ACL 管理平面、指标导出后端和目标硬件容量基准。SRTP、组播、ONVIF、转码和集群级状态复制是独立扩展，不被当前基线隐式承诺。
