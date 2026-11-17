# RTSP 拉流代理

`rtflow-server` 内置 RTSP 拉流代理：主动拉取第三方 RTSP/RTSPS 设备并注册为本地
mount，供直播（RTSP/HLS/DASH/HTTP-FLV/WebRTC）、录制与点播使用。一个拉流源对应
一路上游会话，多个下游共享同一路媒体；配置 `outputs` 后会自动开通对应路径策略。

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

# 注意：启用 SQLite 后，[[proxy.mounts]] 仅首次启动时 seed 到数据库；
# 之后的修改请使用 /admin/api/v1/proxies 或控制台。
[[proxy.mounts]]
path = "/cameras/front"
enabled = true
mode = "on-demand"                # on-demand | always
idle_grace = "5s"                 # 覆盖全局
description_refresh = "300s"
outputs = { read = true, hls = true, flv = true, dash = false,
            webrtc = false, record = true, max_readers = 0 }
# 可选：按服务器本地时间定时拉流（窗口外停止拉流并结束录制）。
schedule = { days = ["mon", "tue", "wed", "thu", "fri"],
             start = "08:00", end = "18:00" }

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

## 输出与路径策略

`outputs` 是拉流源对下游输出的声明：

| 字段 | 默认 | 说明 |
| --- | --- | --- |
| `read` | `true` | 允许 RTSP/读取访问 |
| `hls` | `true` | HLS 直播 |
| `flv` | `true` | HTTP-FLV / WS-FLV 直播 |
| `dash` | `false` | DASH 直播 |
| `webrtc` | `false` | WebRTC 直播 |
| `record` | `false` | 分段录制；要求 `[recording] enabled = true` 且 `auto_start = true` |
| `max_readers` | `0` | 最大观看数，0 表示不限制 |

创建或修改拉流源时，服务端会为该 mount 路径创建/更新一条 **受管 API PathRecord**：

- `publish = false`：只允许内部代理发布，阻止外部客户端向该路径推流；
- `read/hls/flv/dash/webrtc/record/max_readers` 来自 `outputs`；
- `metadata.managed_by = "rtsp-pull"`，路径页会显示“拉流”标记；
- 删除拉流源时只删除带该标记的策略，不碰人工创建的路径；
- 若同路径已有非受管 API 策略，创建/更新会返回 409，避免静默覆盖。

不配置 `outputs`（或旧版本创建的代理）时保持旧行为：路径策略仍由 `[paths]` 规则
或“路径”页单独管理。

**录制与常连**：`mode = "on-demand"` 时需要有观看者触发上游连接，录制才会启动；录制
自身的订阅会让该连接保持，因此首次观看后会持续录制。需要在无人观看时也立即录制，
使用 `mode = "always"`（控制台勾选“录制”会自动切换）。录制完成后，分段进入点播目录，
可在“点播”页按路径搜索和播放。

## 定时拉流

`schedule` 定义每周的拉流窗口，使用服务器本地时区，结束时间不包含在窗口内：

```toml
schedule = { days = ["mon", "tue", "wed", "thu", "fri"],
             start = "08:00", end = "18:00" }
```

- `days` 取 `mon`…`sun`，至少一天且不能重复；
- 结束早于开始表示跨天（例如 `22:00`–`06:00` 会延续到次日）；
- 窗口开始后服务端自动 `ensure` 拉流源并应用输出策略；窗口结束后自动停止拉流，
  mount 消失，录制器 finalize 当前分段；
- 调度器每 15 秒检查一次，边界最多有 15 秒延迟；
- 窗口外状态在控制台显示为 `scheduled`，配置仍保留，窗口开始时自动恢复；
- 未配置 `schedule` 的拉流源保持原来的按需/常连行为。

## ONVIF 设备发现（feature: onvif）

ONVIF 的底层实现位于独立 crate `rtflow-onvif`（WS-Discovery/SOAP 客户端，无内部依赖）；`rtflow` 门面在 `onvif` feature 下将它导出为 `rtflow::onvif`。ONVIF 是**编译期可选**能力，默认不编译。启用：

```bash
cargo build -p rtflow-server --features onvif
# 或
cargo run -p rtflow-server --features onvif -- --config rtflow.toml
```

启用后控制台“RTSP 拉流”页出现“发现设备”按钮；后端新增：

```text
GET  /admin/api/v1/onvif/status
POST /admin/api/v1/onvif/discover   { "timeout_ms": 3000 }
POST /admin/api/v1/onvif/inspect    { "xaddr": "http://.../onvif/device_service",
                                       "username": "...", "password": "..." }
```

`discover` 用 WS-Discovery 在 `239.255.255.250:3702` 搜索 NetworkVideoTransmitter；
`inspect` 读取设备信息、GetCapabilities(Media)、GetProfiles 和每个 profile 的
GetStreamUri。控制台选择 profile 后会把 RTSP 地址、账号密码预填到新建拉流抽屉。

实现边界：

- 仅支持 ONVIF ver10 media service；部分只实现 ver20 的设备可能需要在 UI 中手工填写 RTSP 地址；
- 支持 HTTP Basic / Digest；不支持 WS-Security UsernameToken；
- HTTPS 证书按系统信任链校验，自签名设备请使用 HTTP 或配置可信 CA；
- 发现依赖服务器所在网段的组播可达；跨 VLAN/禁组播时需手工填写 RTSP 地址。

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
- 成功 DESCRIBE 会把 SDP 缓存到 `state_dir/proxy-descriptions/`（0600）；服务重启时
  上游离线也会先用缓存 SDP 注册 mount，后台继续重试。

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
`retry_in_ms`、`last_error`，以及当前录制状态 `recording`（`active`、`segments`、
`bytes`、`directory`）。`/admin/api/v1/proxies/config` 同时返回 `outputs` 与
`managed`。`probe` 只做一次 DESCRIBE 并返回轨道摘要，不影响运行中的代理。

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
- 按需拉流 + 录制时，录制随观看者会话启停；无人观看的持续录制需要 `mode = "always"`。

## 规模/稳定性测试

```bash
PROXY_SOAK_SECS=28800 PROXY_SOAK_MOUNTS=50 \
  cargo test -p rtflow-server --test proxy_soak -- --ignored --nocapture
```

默认 10 个代理、60 秒；可通过环境变量调整。测试并发拉取所有代理的 HLS playlist，
统计成功/失败，并在失败超过 10% 时失败。
