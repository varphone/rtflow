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

# SRT
ffmpeg -re -i input.mp4 -c copy -f mpegts 'srt://host:9000?streamid=live/cam'
```

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
token_secrets = ["${RTFLOW_TOKEN_SECRET}"]
```

- RTSP：Basic/Digest、`Authorization: Bearer` 或 URL `?token=`；
- RTMP：流密钥后接 `?key=<kid>.<secret>`，或 `user:password@cam`；
- HTTP HLS/FLV/DASH/VOD：Bearer、`?token=`/`?key=`；
- SRT：`[srt].allowed_stream_ids` 白名单。

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

`[hooks]` 支持 `on_publish_auth`、`on_play_auth`、`on_publish_done`、`on_record_done`。认证钩子失败时按 `fail_closed` 拒绝；请求带 `X-Rtflow-Timestamp` 与 `X-Rtflow-Signature`（HMAC-SHA256）。RTMP 认证钩子尚未接入（RTMP 使用流密钥/账号），`on_play_done` 为预留事件。

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
# 播放 /vod/<id>/index.m3u8
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

`/admin` 仍提供原有直播/点播/系统信息控制台；管理 API 适合脚本、CI 和外部业务系统。

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
- HLS 播放 token 需客户端在请求中携带，播放列表内的分片不会自动附加 token；
- DASH/HLS 在首个发布 keyframe 前返回 404/503 属于正常冷启动；
- `paths` API 注册的是逻辑路径，实际 mount 在推流上线时创建；离线路径会在发现中显示为 `online=false`。
