# 第三期安全传输、可观测性、性能与 0.2 发布

第三期将框架从"可观测、可限制、可恢复、可发布"推进到 **"可加密传输、可监控、可度量、可稳定依赖"**。

## 已完成能力

### SRTP 安全传输

- RFC 3711 AES-128-CM + HMAC-SHA1-80 加密/认证
- 密钥派生（KDF）通过 RFC 3711 §4.3 参考向量验证
- 滑动窗口重放保护（128-bit 窗口）
- SRTCP 加密与认证
- SDP `a=crypto:` 属性解析与编码
- `SrtpContext` 统一保护/解保护接口

### 组播支持

- `MulticastConfig` 配置与 socket 绑定
- IGMP join/leave 管理
- SDP `c=` 行解析（`SdpConnection`）
- IPv4 组播 TTL 管理

### 可观测性

- `MetricsRegistry` 原子计数器注册表（零竞争）
- `Histogram` 有界直方图（延迟分位数）
- `MetricsEventSink` 事件 → 指标映射
- `MetricsExporter` HTTP 端点：
  - `GET /metrics` — Prometheus 文本格式
  - `GET /health` — JSON 健康状态
- 指标覆盖：连接、会话、Mount、RTP、RTCP 质量、错误、代理

### 性能基准

- Criterion 基准框架
- 吞吐量基准：RTP packetize、RTP parse、RTSP parse、SRTP protect/unprotect、SDP parse

### API 稳定化

- 版本升级至 0.2.0
- crates.io 元数据（license、categories、keywords）
- 迁移指南（`docs/migration-0.1-to-0.2.md`）
- 弃用注解（`recording`、`tls`、`media_pipeline` 门面）

## Feature Flags

所有模块均可通过 Cargo features 按需启用，减少编译时间和二进制体积。

```toml
[features]
default = ["full"]
full = ["srtp", "tls", "mp4", "mkv", "client", "server", "file-storage", "metrics", "multicast"]
srtp = ["dep:aes", "dep:cipher", "dep:hmac", "dep:sha1"]       # SRTP 加密
tls = ["dep:rustls", "dep:rustls-pemfile", "dep:tokio-rustls"]  # RTSPS
mp4 = ["dep:mp4box"]                                             # MP4 容器
mkv = ["dep:matroska-demuxer"]                                   # MKV 容器
client = []                                                       # RTSP 客户端
server = ["client", "file-storage"]                               # RTSP 服务端 + 代理
file-storage = []                                                 # 文件播放 + 录制
metrics = []                                                      # Prometheus 指标
multicast = []                                                    # 组播支持
```

下游用户示例：

```toml
# 最小依赖（仅协议解析 + RTP）
rtflow = { version = "0.2", default-features = false }

# 仅客户端
rtflow = { version = "0.2", default-features = false, features = ["client"] }

# 完整服务端
rtflow = { version = "0.2", default-features = false, features = ["server", "tls"] }
```

## 目录边界

```text
srtp/       SRTP/SRTCP 加密与认证         (feature: srtp)
multicast/  组播配置与 socket 管理         (feature: multicast)
metrics/    指标注册表、直方图、导出端点     (feature: metrics)
client/     RTSP 客户端                    (feature: client)
server/     RTSP 服务端 + 代理              (feature: server)
file/       文件解复用                      (feature: file-storage, mp4, mkv)
storage/    录制与 VOD                      (feature: file-storage)
security/   ACL + TLS                       (ACL: 始终编译, TLS: feature: tls)
```

## 运行基准

```bash
cargo bench --bench throughput
```

## 验证指标端点

```bash
# 启动示例后
curl http://localhost:9090/metrics
curl http://localhost:9090/health
```
