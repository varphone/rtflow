# client 模块持续迭代路线图（M0–M5）

本文件是 `src/client/` 持续迭代的落库看板：每个阶段是一个可独立 review 的批次，完成后勾选并记录偏差。破坏性变更集中在 M3 一次性完成。

当前短板依据：`docs/audits/code.md` R23、`docs/audits/realtime-compat.md` RT-7/RT-8/RT-9/RT-17、以及 M0 前的测试缺口（模块 0 单测、`connect_reconnecting` 零覆盖）。

## M0 — 测试地基

- [x] `src/client/mod.rs`、`setup.rs` 纯函数单元测试（`jittered_backoff`、`should_fallback_to_tcp`、`encode_request`、`response_session_id`、`status_error`、CSeq 配对、选项/策略校验、`client_ssrc`）
- [x] `tests/support/mod.rs`（现已迁至 `support/rtflow-testkit`）新增 `RawRtspServer` 脚本化"坏服务器"线束（可编程状态码/原始字节/提前关闭，记录请求供断言）
- [x] `tests/client_edge.rs`：UDP 461→TCP 回退、过期 CSeq 跳过、401 无凭据/有凭据 Digest 重试、非 200 SETUP/PLAY、`rtsps://` 缺 TLS 配置、`describe()` 成功路径、`connect_reconnecting` max_attempts 耗尽契约、keep-alive 失败基线（R23）
- [x] `tests/udp_play.rs` 手写 TrackDescription 改用 `support::description` helper

## M1 — 错误类型化 + fallback/重连决策

- [x] `src/error.rs`：新增 `Error::RtspStatus { method, status }` 变体（非破坏，Display 与旧字符串一致），补齐 `code()`/`retry_class()`/`rtsp_status()` 映射与单测
- [x] `src/client/mod.rs`：`status_error` 与各 `format!` 错误改用新变体（含 `setup.rs` 四处）
- [x] `should_fallback_to_tcp` 改按类型化 461 判定，删除 `"461"`/`"unsupported transport"` 字符串嗅探（固化测试已更新为新语义）
- [x] `connect_reconnecting` 按 `retry_class()` 决策：`Never`（Unauthorized、配置错误）立即终止（回归测试 `reconnecting_session_stops_on_permanent_error`）
- [x] `ReconnectingClientSession` 耗尽契约测试已在 M0 就位，验证仍绿
- [x] `src/server/proxy_runtime.rs` 留 TODO：待 M3 事件化后评估复用 client 重连 API

## M2 — 协议兼容性修补

- [x] keep-alive 协商：GET_PARAMETER 收 405/501 后降级 OPTIONS 并记忆能力（测试 `keep_alive_downgrades_to_options_when_get_parameter_unsupported`）
- [x] keep-alive 失败退避（R23）：连续失败按指数退避（1x→16x 封顶 + 抖动）推迟，成功重置（测试 `keep_alive_failures_back_off`、单测 `keep_alive_backoff_grows_and_stays_bounded`）
- [x] 解析 Session `timeout=` 参数：`effective_keep_alive` 取 `min(配置值, timeout/2)`，未配置时按 `timeout/2`（下限 1s）自适应（测试 `session_timeout_parameter_drives_keep_alive_interval`）
- [x] Auto 回退省 RTT（RT-9 客户端侧）：`setup::SetupFailure` 归还控制连接 + SDP，461 后同一连接直接重发 interleaved SETUP；对端若已关连接则全新连接兜底（461 测试断言 OPTIONS/DESCRIBE 仅 1 次）
- [x] User-Agent 改 `env!("CARGO_PKG_VERSION")`（单测期望值随版本自动跟随）
- [x] IPv6：`tests/client_edge.rs::describe_succeeds_over_ipv6_loopback`（`[::1]:0` 绑定，不支持时跳过）；顺带修复 `open_and_describe` 未剥离 IPv6 host 方括号导致解析失败的真实缺陷

## M3 — 可观测性 + 易用性【含破坏性变更】

- [x] `src/observe.rs` 新增 `FrameworkEvent::Client(ClientEvent)`：Connected/Fallback/ReconnectBackoff/KeepAliveFailed/Failed/Closed（形状照 `ProxyEvent`，接入 `identity::SessionId`，静态原子计数分配）
- [x] **破坏**：`ClientOptions` 加 `event_sink: Option<Arc<dyn EventSink>>`，`ClientOptions`/`ClientReconnectPolicy` 加 `#[non_exhaustive]`。实施中发现 E0639：crate 外**任何**结构体字面量（含 `..Default::default()`）都被禁止，因此仓库内 ~21 处 tests/examples 构造点全部迁移为 builder；`ClientReconnectPolicy` 同步新增 builder。迁移注记已入 `docs/guides/migration/0.1-to-0.2.md`（0.3 预告节）
- [x] connect/fallback/backoff/keep-alive 失败/终止点 `observe::emit`（含结构化 tracing 字段 session_id/attempt/status）
- [x] `src/metrics/mod.rs`：client_sessions_total/active、client_reconnect_attempts、client_keep_alive_failures、client_fallbacks + Prometheus 导出（测试 `client_events.rs::reconnect_lifecycle_emits_backoff_failed_closed_and_feeds_metrics`）
- [x] 易用性：`rtflow::client::UpstreamTransport` re-export；`RtspClient::connect_str/describe_str`；`ClientSession::new` 收敛为 `SessionParts`（删除两处 `too_many_arguments` allow）；`examples/client/play_client.rs` 加 `--reconnect` 与 5s 质量快照；`docs/guides/examples.md` 增"播放客户端"章节
- [x] 事件序列测试：`tests/client_events.rs`（Collector sink 断言 Connected/Fallback 顺序、KeepAliveFailed、重连终止事件 + metrics 计数）

## M4 — 性能

- [x] 接收路径基线：`tests/client/performance.rs`，UDP p50≈0.14ms/p99≈1.1ms，interleaved p50≈0.16ms/p99≈0.74ms（loopback）
- [x] jitter drain 事件驱动（RT-17）：`sleep_until(JitterBuffer::next_deadline())` 替换 1–10ms tick；满队列 `try_send` + 丢弃计数
- [x] keep-alive/控制面独立任务（RT-7/RT-8）：命令 mpsc + oneshot 应答；`next()` 不再内联 RTSP 往返
- [x] UDP 收缓冲可配（`UdpPair` 4MB 固定值 → `ClientOptions` 可选覆盖；`low_memory()` 预设 256KiB）
- [x] UDP 收包使用 `BytesMut::split_to().freeze()`
- [x] 输出队列字节上限：`ClientOptions::max_output_bytes`（默认 8MiB，低内存 2MiB），计数器随出队递减

## M5 — 功能扩展（各子阶段独立立项）

- [x] M5.1 发布客户端（ANNOUNCE/RECORD，UDP + TCP interleaved）：`src/client/publish.rs`，`PublisherSession::push(AccessUnit)`，多轨与并发测试
- [ ] M5.2 SRTP 接收：`RTP/SAVP` + SDP `a=crypto`，`src/client/srtp_rx.rs`，复用 `src/srtp`
- [ ] M5.3 WHEP 客户端：`src/client/whep.rs`（feature `webrtc`），与 `ClientSession::next()` 同形态出帧；扩展 `tests/webrtc_e2e.rs`

## M6 — RTSP 1.0 方法面

- [x] `RtspClient::options` + `ServerCapabilities`（Public/Supported/Server）
- [x] `ClientSession::play_with(PlayRequest)`（Range/Scale/Speed）
- [x] `ClientSession::pause_with(range)`
- [x] `ClientSession::set_parameter` / `get_parameter`（text/parameters body）
- [x] interleaved 控制通道 Digest 401 重试
- [x] `ClientOptions::low_memory()` 有界预设
- [x] 32 路并发客户端集成测试、控制面方法测试
- [x] 消费 PLAY `RTP-Info`：seq 预置 jitter、rtptime 锚定非零 NPT 范围 PTS
- [x] 发布客户端（ANNOUNCE/RECORD，UDP/interleaved）
- [x] server-initiated REDIRECT：回 200；连接建立阶段与 `connect_reconnecting` 自动跟随（同源默认）
- [x] `Require`/`Proxy-Require` 头；551 解析为 `Error::RtspUnsupported { method, unsupported }`
- [x] `Require`/`Proxy-Require` 头与 551 上抛
- [x] RTP-Info seq/rtptime 消费

### Backlog

multicast 接收、RTSP-over-HTTP tunneling、RTSP 2.0/pipelining、interleaved >127 track 通道分配、proxy_runtime 与 client 重连去重。

## 每阶段收尾

1. 勾选本文件对应项并记录偏差；
2. 注记对应审计条目（R23、RT-7/8/9/17）修复状态；
3. 全量验证：

```bash
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test --all-targets
RUSTDOCFLAGS="-D warnings" cargo doc --no-deps
cargo check --no-default-features --features client
cargo check --no-default-features --features "server,tls,srtp"
```
