# 从 0.1.x 迁移到 0.2.0

## 新增模块

### SRTP 安全传输（`srtp`）

```rust
use rtflow::srtp::{SrtpContext, SrtpProfile, SessionKeys};

// 从 master key/salt 创建 SRTP 上下文
let mut ctx = SrtpContext::new(
    SrtpProfile::AES_128_CM_HMAC_SHA1_80,
    &master_key,
    &master_salt,
);

// 保护 RTP 包
let protected = ctx.protect_rtp(&rtp_packet)?;

// 解保护 SRTP 包
let plaintext = ctx.unprotect_rtp(&srtp_packet)?;
```

### 组播支持（`multicast`）

```rust
use rtflow::multicast::MulticastConfig;

let config = MulticastConfig::new("239.1.1.1".parse()?, 5004)
    .with_ttl(16);
let socket = config.bind().await?;
```

### 指标收集（`metrics`）

```rust
use rtflow::metrics::{MetricsRegistry, MetricsEventSink, MetricsExporter};
use std::sync::Arc;

let registry = Arc::new(MetricsRegistry::new());
let sink = MetricsEventSink::new(registry.clone());

// 在 ServerConfig 中使用
let config = ServerConfig::builder(addr)
    .event_sink(Arc::new(sink))
    .build()?;

// 启动指标导出
let exporter = MetricsExporter::new(registry, metrics_addr);
exporter.start().await?;
```

## 变更

### SDP 解析

- `TrackDescription` 新增 `crypto: Option<SdpCrypto>` 字段（SRTP crypto 属性）
- `MediaDescription` 新增 `connection: Option<SdpConnection>` 字段（c= 行）
- 新增 `SdpCrypto` 和 `SdpConnection` 类型

### 错误处理

- `Error` 新增 `Srtp(String)` 变体
- `ErrorCode` 新增 `SrtpAuthenticationFailed`、`SrtpReplayDetected`
- `Operation` 新增 `SrtpProtect`、`SrtpUnprotect`

### 已弃用

- `rtflow::recording` → 使用 `rtflow::storage`
- `rtflow::tls` → 使用 `rtflow::security`
- `rtflow::media_pipeline` → 使用 `rtflow::pipeline` + `rtflow::rtp`

### 依赖新增

```toml
aes = "0.9"
cipher = "0.5"
hmac = "0.13"
sha1 = "0.11"
```

---

# 预告：0.2.x → 0.3 的 client 破坏性变更（client-roadmap M3）

- `ClientOptions` 与 `ClientReconnectPolicy` 标注 `#[non_exhaustive]`：**crate 外的一切结构体字面量构造（含 `..Default::default()` 形式）都将编译失败**（E0639）。
  迁移方式：统一改用 builder：

  ```rust
  // 0.2（不再编译）
  let options = ClientOptions {
      transport: UpstreamTransport::TcpInterleaved,
      ..Default::default()
  };

  // 0.3
  let options = ClientOptions::builder()
      .transport(UpstreamTransport::TcpInterleaved)
      .build()?;

  let policy = ClientReconnectPolicy::builder()
      .initial_delay(Duration::from_secs(1))
      .max_attempts(5)
      .build()?;
  ```
  已构造实例的字段读写不受影响（`options.transport = ...` 仍可用）。
- `ClientOptions` 新增 `event_sink: Option<Arc<dyn EventSink>>` 字段（默认 `None`），
  配合新增的 `FrameworkEvent::Client(ClientEvent)` 事件流（连接/回退/重连退避/keep-alive 失败/关闭）。
- 错误类型化：非 2xx RTSP 响应改由 `Error::RtspStatus { method, status }` 承载；
  Display 文本与 0.2 的自由文本一致（`"SETUP returned 500"`），但 `matches!` 旧 `Error::Rtsp` 的代码需迁移。
- 新增便捷入口：`RtspClient::connect_str` / `describe_str`；`rtflow::client::UpstreamTransport` re-export。
