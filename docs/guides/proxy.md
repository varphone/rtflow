# RTSP 拉流代理

`rtflow-server` 内置按需拉流代理：把上游 RTSP/RTSPS 源注册为本地 mount，供 RTSP、
HLS、DASH、HTTP-FLV、录制等下游使用。一个代理对应一路上游会话，多个下游共享同一
路媒体。

## 配置

```toml
[proxy]
enabled = true
idle_grace = "5s"                 # 无订阅者后多久断开上游
max_proxies = 128
max_upstreams_per_proxy = 4
describe_concurrency = 8
description_refresh = "300s"      # 空闲时周期性重新 DESCRIBE
persist_runtime = false           # API 变更写入 state_dir/proxies.toml（0600）
allow_private_networks = true
allowed_hosts = []                # host / host:port / *.domain / CIDR
redirect_policy = "same-origin"   # none | same-origin | any

[[proxy.mounts]]
path = "/cameras/front"
enabled = true
mode = "on-demand"                # on-demand | always
idle_grace = "5s"                 # 覆盖全局
description_refresh = "300s"

[[proxy.mounts.upstreams]]
url = "rtsp://192.0.2.10:554/live/main"
transport = "tcp"                 # auto | udp | tcp
username = "admin"
password = "${CAMERA_PASSWORD}"
request_timeout = "5s"
media_idle_timeout = "15s"
jitter = { max_delay = "40ms", max_packets = 128, max_reorder_window = 64 }
# tls = { ca = "/etc/ssl/camera-ca.pem", server_name = "camera.local" }
```

第一个 `upstreams` 为主源，其余为 fallback。代理启动时异步 DESCRIBE；失败进入
`backoff` 并自动重试，不阻塞其他代理/协议。`persist_runtime=true` 时，运行时 API
的变更会写入 `state_dir/proxies.toml`（0600），重启后覆盖配置文件的 mount 列表。

## 生命周期

```text
describing → idle → connecting → streaming
                ↘ backoff（上游不可达/描述失败，自动重试）
                ↘ stopped
```

- `on-demand`：首个订阅者（RTSP/HLS/FLV 任一）触发连接，最后一个订阅者离开超过
  `idle_grace` 后断开上游、保留 mount。
- `always`：描述成功后立即连接并保持，不依赖订阅者。
- 媒体空闲超过 `media_idle_timeout`（半开连接）触发失败并重连/切换 fallback。
- 重连优先使用缓存 SDP（`connect_with_description`），失败自动回退完整 DESCRIBE。
- 无订阅者时按 `description_refresh` 重新探测，轨道变化会更新 mount 描述。

## 状态与管理 API

```text
GET /api/v1/proxies
GET /admin/api/v1/proxies
POST /admin/api/v1/proxies                  # 新增，body 为 ProxyMount
PATCH /admin/api/v1/proxies                 # 修改，body 为完整 ProxyMount
DELETE /admin/api/v1/proxies?path=%2Fcameras%2Ffront
POST /admin/api/v1/proxies/restart?path=%2Fcameras%2Ffront
POST /admin/api/v1/proxies/probe?path=%2Fcameras%2Ffront
```

状态包含 `state`、`upstream`（脱敏主机）、`subscribers`、`tracks`、`attempts`、
`retry_in_ms`、`last_error`。`probe` 只做一次 DESCRIBE 并返回轨道摘要，不影响运行中
的代理。

`restart` 在仍有订阅者时返回 `pending: true`，等待订阅者离开后自动重建，避免
直接打断观众。

管理页“路径与密钥”页包含代理只读状态表。

## 指标

```text
rtflow_proxy_configured          # 已配置代理数
rtflow_proxy_streaming           # 正在 streaming 的代理数
rtflow_proxy_reconnects_total
rtflow_proxy_failovers_total
```

## 安全

- `allowed_hosts` 在 DESCRIBE 前校验；`allow_private_networks=false` 时拒绝
  loopback/私网/链路本地解析结果。
- 解析后的地址会被固定用于连接（`peer_addr`），避免 DNS 重绑定；URL host 保留
  用于 TLS SNI。
- `redirect_policy` 控制是否跟随 REDIRECT、是否允许跨源。
- 配置打印、日志、状态 API 均对密码/URL userinfo 脱敏。
- 代理 mount 与普通 mount 一样受 `[auth]`、`[paths]` 下游鉴权与 reader 限制。
- 代理由下游触发，属于受控回源；仍建议只把管理 API 暴露给可信网络。

## 故障排查

| 状态 | 排查 |
| --- | --- |
| `describing` 卡住 | 上游 DNS/端口/防火墙；查看 `last_error` |
| `backoff` | 401/403 凭据、404 path、网关不可达；`probe` 验证 SDP |
| `idle` 但无画面 | 下游未建立订阅；`on-demand` 需要 PLAY/HLS 请求 |
| `streaming` 但 HLS 黑屏 | 上游关键帧间隔、`idle_grace` 过短、下游鉴权 |
| 首帧慢 | 上游 GOP、`description_refresh`、上游 RTSP 握手耗时 |

## A/V 时间轴

代理会周期性读取上游 RTCP Sender Report 的 `(NTP, RTP timestamp)`，把各轨映射到同一
壁钟基准，统一多轨起点；首次收到 SR 时会重定一次时间轴基准。重连时使用一个全局
时间偏移接续旧时间轴，保留 SR 推导出的音视频相对偏移，不会让各轨各自续接而丢失
A/V 对齐。RTP 时间戳回绕由 depacketizer 扩展处理。

## 已知限制

- 重连后的长期时钟漂移尚未做渐进校准（SR 映射在每次连接内生效）。
- 透明 RTSP 控制面代理、RTSP-over-HTTP、RTSP 2.0 不在范围内。
- 代理只处理 RTSP/RTSPS 上游；RTMP/SRT 回源使用各自 `[[rtmp.pull]]`/`[[srt.pull]]`。

## 规模/稳定性测试

```bash
PROXY_SOAK_SECS=28800 PROXY_SOAK_MOUNTS=50 \
  cargo test -p rtflow-server --test proxy_soak -- --ignored --nocapture
```

默认 10 个代理、60 秒；可通过环境变量调整。测试并发拉取所有代理的 HLS playlist，
统计成功/失败，并在失败超过 10% 时失败。
