# rtflow-server 使用指南

`rtflow-server` 是 `rtflow` workspace 的官方流媒体应用：RTSP、RTMP、SRT、WebRTC（WHIP/WHEP）、HLS/LL-HLS、DASH、HTTP-FLV、录制、VOD、路径注册/发现、流密钥和 HTTP 钩子。

## 快速开始

```bash
cargo run -p rtflow-server -- --init-config
# 编辑 rtflow.toml，至少设置 admin_password 或 RTFLOW_ADMIN_PASSWORD
cargo run -p rtflow-server -- --config rtflow.toml
```

校验配置而不启动网络：

```bash
cargo run -p rtflow-server -- --config rtflow.toml --check-config
```

## 推流

```bash
# RTMP（OBS：服务器 rtmp://host:1935/live，流密钥 cam）
ffmpeg -re -i input.mp4 -c copy -f flv rtmp://host:1935/live/cam

# RTSP
ffmpeg -re -i input.mp4 -c copy -f rtsp rtsp://host:8554/live/cam

# SRT（streamid 可带认证参数）
ffmpeg -re -i input.mp4 -c copy -f mpegts 'srt://host:9000?streamid=live/cam'
ffmpeg -re -i input.mp4 -c copy -f mpegts \
  'srt://host:9000?streamid=live/cam?key=<kid>.<secret>'
```

支持的 SRT 能力：listener / caller（拉流）/ sender（推流）、HSv5 握手、
HSREQ/HSRSP、Stream ID、ACK/ACKACK/NAK/keepalive/SHUTDOWN/DROPREQ、
TSBPD + TLPKTDROP、HaiCrypt AES-CTR（含发送侧密钥刷新）。

未实现（libsrt 可选扩展，默认部署不启用）：FEC 包过滤、rendezvous
（双端同时连接）、group/bonding。

SRT 与 libsrt 系工具（ffmpeg、OBS、srt-live-transmit）互通：HSv5 握手、
HSREQ/HSRSP 延迟协商、Stream ID、ACK/ACKACK/NAK、keepalive、TLPKTDROP。

### RTSP 拉流代理

```toml
[proxy]
enabled = true
idle_grace = "5s"

[[proxy.mounts]]
path = "/cameras/front"

[[proxy.mounts.upstreams]]
url = "rtsp://192.0.2.10:554/live/main"
transport = "tcp"          # auto | udp | tcp
username = "admin"
password = "${CAMERA_PASSWORD}"
request_timeout = "5s"
```

代理在首个订阅者（RTSP/HLS/FLV 任一协议）出现时连接上游，最后一个订阅者离开并超过
`idle_grace` 后断开；`mount` 与普通本地源一样参与 HLS/DASH/FLV/录制与路径鉴权。
启动时服务端先 DESCRIBE 上游以注册 mount；上游离线时该代理注册失败并记录日志，
当前需重启或重新加载配置后重试（动态 CRUD/自动重试见 `docs/roadmap/proxy.md`）。

```text
GET /api/v1/proxies          # 公开发现权限
GET /admin/api/v1/proxies    # admin
```

返回代理路径、状态（describing/idle/connecting/streaming/backoff）、订阅者、轨道数、
上游主机（已脱敏）、重试次数、下次重试时间与最后一次错误。

运行中管理（admin）：

```bash
# 新增
curl -u admin:pass -X POST http://127.0.0.1:8080/admin/api/v1/proxies \
  -H 'content-type: application/json' \
  -d '{"path":"/cameras/front","mode":"on-demand","upstreams":[
        {"url":"rtsp://192.0.2.10/live","transport":"tcp"}]}'

# 修改（body 为完整 ProxyMount，path 必须与 URL 一致）
curl -u admin:pass -X PATCH http://127.0.0.1:8080/admin/api/v1/proxies \
  -H 'content-type: application/json' -d '...'

# 重启 / 删除（path 使用 URL 编码）
curl -u admin:pass -X POST 'http://127.0.0.1:8080/admin/api/v1/proxies/restart?path=%2Fcameras%2Ffront'
curl -u admin:pass -X DELETE 'http://127.0.0.1:8080/admin/api/v1/proxies?path=%2Fcameras%2Ffront'
```

上游离线时状态为 `backoff`，按抖动退避自动重试；SIGHUP 会对比配置文件增删改代理。

### SRT 拉流（server 作为 caller）

```toml
[[srt.pull]]
remote = "192.0.2.10:9000"
stream_id = "live/camera1"
mount = "/live/camera1"          # 默认取 Stream ID 的 r= 资源
passphrase = "secretpass12345"   # 默认继承 [srt].passphrase
pbkeylen = 16
latency_ms = 120
```

服务端主动连接远端 SRT listener，按 TS demux 结果注册发布 mount，之后
HLS/DASH/FLV/RTSP 播放与录制流程与推流一致；断线按 1s→30s 指数退避重连，
连接成功后退避复位。pull 任务随服务端 SIGTERM 一起取消。

### SRT 输出（server 作为 caller/sender）

```toml
[[srt.push]]
mount = "/live/camera1"          # 已存在的发布 mount
remote = "192.0.2.20:9000"
stream_id = "live/camera1"       # 默认取 mount 去掉前导 "/"
passphrase = "secretpass12345"   # 默认继承 [srt].passphrase
pbkeylen = 16
latency_ms = 120
```

- 从 mount 订阅 AccessUnit，按 H.264/H.265/AAC 复用为 MPEG-TS
  （PAT/PMT 周期重复、连续计数器、统一时间基），经 `SrtSender` 推送；
- 等待源 mount 就绪，从首个视频关键帧开始发送；
- 连接失败/中断按 1s→30s 指数退避重连，随 SIGTERM 取消；
- 支持 passphrase 加密（KMREQ/KK/AES-CTR）。

### SRT 加密（HaiCrypt AES-CTR）

```toml
[srt]
passphrase = "secretpass12345"   # 10..80 字节
pbkeylen = 16                    # 16/24/32，0 表示默认 16
enforced_encryption = true       # 对端加密设置不一致时拒绝（libsrt 默认）
```

推流端：

```bash
ffmpeg -re -i input.mp4 -c copy -f mpegts \
  'srt://host:9000?streamid=live/cam&passphrase=secretpass12345&pbkeylen=16'
```

- 监听端作为响应方解包 KMREQ、回显 KMRSP，并用 AES-CTR 解密数据；
- 错误 passphrase 返回 `SRT_REJ_BADSECRET`（1010），明文对端在
  `enforced_encryption=true` 时返回 `SRT_REJ_UNSECURE`（1011）；
- 支持运行中的密钥刷新（`UMSG_EXT` KMREQ）；
- libsrt 不会对 `passphrase=` 做 URL 解码：含空格等字符时请直接使用可打印
  安全字符，避免 `%20` 被当作口令的一部分。

## 播放

| 协议 | 地址 |
|---|---|
| HLS | `http://host:8080/hls/live/cam/index.m3u8` |
| LL-HLS | `http://host:8080/llhls/live/cam/index.m3u8` |
| DASH | `http://host:8080/dash/live/cam/manifest.mpd` |
| HTTP-FLV | `http://host:8080/flv/live/cam.flv` |
| WS-FLV | `ws://host:8088/live/cam.flv` |
| RTSP | `rtsp://host:8554/live/cam` |
| WebRTC | `POST http://host:8080/whep/live/cam` |
| RTMP | `rtmp://host:1935/live/cam` |

## 路径注册

配置文件中的 `[paths]` 是静态规则；运行时可通过管理 API 注册具体路径：

```bash
curl -u admin:password -X POST http://127.0.0.1:8080/admin/api/v1/paths \
  -H 'content-type: application/json' \
  -d '{"name":"live/cam","publish":true,"read":true,"record":true}'
```

API 路径持久化在 `<state_dir>/paths.json`，重启后保留；配置中的路径规则只读，不能被 API 覆盖。动态推流路径会自动注册（内存中，默认不落盘）。

## 路径发现

```bash
# 需要 admin Basic，或开启 discovery.public / 配置 discovery.token
curl -u admin:password http://127.0.0.1:8080/api/v1/discovery
curl -u admin:password http://127.0.0.1:8080/api/v1/paths/live/cam
```

返回在线状态、发布者、读者数、轨道、录制状态以及所有已启用协议的播放 URL。

## 推流认证

### 模式

```toml
[auth]
mode = "token"       # none | basic | digest | token
token_secrets = ["${RTFLOW_TOKEN_SECRET}"]   # 每项至少 32 字节（用于 nonce/HMAC）
```

- RTSP：Basic/Digest、`Authorization: Bearer` 或 URL `?token=`；
- RTMP：流密钥后接 `?key=<kid>.<secret>`，或 `user:password@cam`；
- HTTP HLS/FLV/DASH/VOD：Bearer、`?token=`/`?key=`；
- SRT：`streamid=live/cam?token=<签名或流密钥>` 走与 RTSP 相同的 HMAC/密钥校验，另可用 `[srt].allowed_stream_ids` 白名单。

### 签发流密钥

```bash
curl -u admin:password -X POST http://127.0.0.1:8080/admin/api/v1/keys \
  -H 'content-type: application/json' \
  -d '{"name":"obs","roles":["publish"],"paths":["live/*"],"ttl":"24h"}'
```

响应中的 `secret` 只返回一次；服务端只保存 Argon2id 哈希。OBS 流密钥：

```text
cam?key=<kid>.<secret>
```

吊销：

```bash
curl -u admin:password -X DELETE http://127.0.0.1:8080/admin/api/v1/keys/<kid>
```

### HTTP 钩子

`[hooks]` 支持 `on_publish_auth`、`on_play_auth`、`on_publish_done`、`on_play_done`、`on_record_done`。RTSP/RTMP/SRT 的推流与播放认证都会先过本地凭据与路径规则，再调用钩子；钩子失败时按 `fail_closed` 拒绝。RTSP 会话结束后会为每个播放过的 mount 发送一次 `on_play_done`。请求带 `X-Rtflow-Timestamp` 与 `X-Rtflow-Signature`（HMAC-SHA256）。

### 配额

- `paths.<rule>.max_readers`：观看人数上限。鉴权时按 mount 当前订阅数检查，属于准入控制（并发鉴权可能短暂超出）。
- `rtsp.max_sessions`、`[security]` 的连接限流仍是硬上限。

## SRT 状态 API

```text
GET /api/v1/srt          # 公开发现权限（或 admin Basic）
GET /admin/api/v1/srt    # admin Basic / 会话 / token
```

返回监听连接与拉流源的实时状态：

```json
{
  "listener": [{"peer":"…","stream_id":"live/cam","mount":"/live/cam",
                "encrypted":true,"rtt_us":124,"packets":42,"delivered":42,
                "skipped":0,"losses":0,"retransmitted":3,"latency_ms":120}],
  "pulls":    [{"remote":"…","mount":"/live/pulled","connected":true,
                "bytes":67680,"rtt_us":120,"last_error":null}],
  "pushes":   [{"mount":"/live/camera1","remote":"…","connected":true,
                "bytes_sent":0,"last_error":null}]
}
```

## 指标

`/metrics` 输出 Prometheus 文本：框架连接/会话/协议计数之外，还有应用级指标：

| 指标 | 含义 |
|---|---|
| `rtflow_paths_total` | 已注册路径数 |
| `rtflow_paths_online` | 有活跃发布者的路径数 |
| `rtflow_recordings_active` | 活跃录制会话数 |
| `rtflow_recording_bytes_total` | 活跃录制已写字节 |
| `rtflow_auth_failures_total` | 被拒绝的认证决策 |
| `rtflow_hook_failures_total` | 认证钩子失败次数 |
| `rtflow_srt_connections_total` | SRT 连接总数 |
| `rtflow_srt_streams_active` | 活跃 SRT 流 |
| `rtflow_srt_rtt_microseconds` | SRT RTT 估计 |
| `rtflow_srt_loss_packets_total` | SRT 丢包数 |
| `rtflow_srt_skipped_packets_total` | TLPKTDROP 跳包数 |
| `rtflow_srt_handshake_failures_total` | 被拒绝的 SRT 握手 |

`/stats` 保留 JSON 形式，供管理台与脚本使用。

## 审计日志

```toml
[audit]
enabled = true
# 默认 <state_dir>/audit.log
# path = "/var/log/rtflow/audit.log"
max_bytes = 67108864
max_files = 5
```

JSON Lines，按大小轮转，记录认证允许/拒绝、路径增删改、kick、密钥签发/吊销、录像开始/停止、VOD 刷新。不记录密码、secret 或完整 token。

## 热加载

```bash
systemctl reload rtflow      # 发送 SIGHUP
```

SIGHUP 会重新读取配置文件并应用：`[paths]` 规则、`[recording]` 策略、发现接口使用的公开地址/能力。监听地址、启用的协议、认证模式/用户/密钥、钩子 URL 与管理台凭据变更会记录为 `restart_required`，需重启生效。

```text
INFO rtflow_server::server: configuration reloaded applied=["paths", "recording", "discovery"] restart_required=[]
```

## TLS

```toml
[rtsp.tls]
certificate = "/etc/rtflow/server.crt"
private_key = "/etc/rtflow/server.key"

[http.tls]
certificate = "/etc/rtflow/server.crt"
private_key = "/etc/rtflow/server.key"
```

两者都内置支持；配置后 RTSP 使用 `rtsps://`、HTTP 使用 `https://`。WS-FLV 仍为明文端口，可放在反向代理之后。

## 录制与点播

```toml
[recording]
enabled = true
root = "./recordings"
segment_duration = "10s"
retention = "7d"
max_disk_bytes = 53687091200
auto_start = true
```

路径规则 `record = true` 时，发布者上线自动录制；发布者断开后 finalize。录制目录按 `recordings/<path>/<timestamp>/` 组织。新录像会自动进入 VOD 目录：

```bash
curl -u admin:password http://127.0.0.1:8080/admin/api/v1/vods
# 播放 /vod/<id>/index.m3u8（按该录像来源路径的 read 权限鉴权）
```

手动开始/停止（需在线 mount）：

```bash
curl -u admin:password -X POST 'http://127.0.0.1:8080/admin/api/v1/recordings/start?path=live/cam'
curl -u admin:password -X POST 'http://127.0.0.1:8080/admin/api/v1/recordings/stop?path=live/cam' 
```

## 管理 API

除公开发现接口外，以下接口使用 admin Basic 认证：

```text
GET    /admin/api/v1/paths
POST   /admin/api/v1/paths
PATCH  /admin/api/v1/paths/{name}
DELETE /admin/api/v1/paths/{name}
GET    /admin/api/v1/keys
POST   /admin/api/v1/keys
DELETE /admin/api/v1/keys/{kid}
GET    /admin/api/v1/vods
POST   /admin/api/v1/vods/refresh
GET    /admin/api/v1/recordings
POST   /admin/api/v1/recordings/start?path=<path>
POST   /admin/api/v1/recordings/stop?path=<path>
POST   /admin/api/v1/kick?path=<path>
```

`/admin` 控制台除直播/点播/系统信息外，还提供“路径与密钥”页：路径注册/删除/kick、推流密钥签发/吊销、录像开始/停止。对应会话 API：

```text
GET/POST/PUT/DELETE /admin/api/paths
POST                /admin/api/paths/kick
GET/POST/DELETE     /admin/api/keys
GET                 /admin/api/recordings
POST                /admin/api/recordings/start|stop
```

这些接口使用管理台会话 + CSRF，与 `/admin/api/v1` 的 Basic 接口等价。管理 API 支持三种认证：

- 管理台会话 cookie（浏览器登录后自动携带）；
- HTTP Basic（`admin_username` / `admin_password`）；
- 静态 `Bearer` token（`[http].api_tokens`，也接受 `?token=`）。

管理 API 还支持：

- `If-Match: <updated_at>`：PATCH 的乐观并发控制，版本不匹配返回 412；
- `Idempotency-Key: <key>`：POST 重试在 10 分钟内返回首次结果，避免重复创建路径/密钥。

## 部署

- `crates/rtflow-server/deploy/rtflow.toml`：完整示例；
- `deploy/systemd/rtflow.service`：systemd；
- `deploy/docker/Dockerfile`、`docker-compose.yml`：容器部署。

生产建议：

- 不要使用空 admin 密码；用 `RTFLOW_ADMIN_PASSWORD`；
- 公网必须启用 `auth.mode` 和路径读/写规则；
- 反代 HTTP 时开启 `secure_cookies`（当前由 admin 配置项控制）；
- 录制目录和 `state_dir` 放在持久卷；
- 通过 `/metrics` 接入 Prometheus，`/healthz`、`/readyz` 做探针。

## 当前限制

- SRT 认证只支持 stream id 白名单，不支持按 key 的 HMAC 签名；
- 密钥的 `max_connections` 对 RTSP/RTMP 按 peer 租约（120s 刷新）强制执行；HTTP 不强制（可配合反代限流）；
- WebRTC 的认证钩子尚未接入；
- HLS 播放 token 需客户端在请求中携带，播放列表内的分片不会自动附加 token；
- DASH/HLS 在首个发布 keyframe 前返回 404/503 属于正常冷启动；
- `paths` API 注册的是逻辑路径，实际 mount 在推流上线时创建；离线路径会在发现中显示为 `online=false`。
