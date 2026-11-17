# rtflow-server 配置与运维

`rtflow-server` 使用单个 TOML 文件启动；运营期数据（路径、密钥、代理、SRT 端点）
保存在 SQLite。完整字段见 [deploy/rtflow.toml](../../crates/rtflow-server/deploy/rtflow.toml)。

## 配置分层

| 类型 | 位置 | 修改方式 |
| --- | --- | --- |
| 服务配置 | `rtflow.toml` | 编辑后 SIGHUP 或重启 |
| 业务数据 | `state/rtflow.db` | 管理 API/控制台 |
| 录制文件/VOD | `recording.root` 等 | 文件系统/录制 API |

服务配置包括监听地址、TLS、日志、鉴权、限额、HLS/DASH（见 [dash.md](dash.md)）、
全局录制策略、`[storage]` 和 `[paths]` glob 策略。路径记录、推流密钥、RTSP 代理、SRT pull/push
属于业务数据，见 [storage.md](storage.md)。

## 最小示例

```toml
[server]
name = "rtflow"
state_dir = "./state"

[rtsp]
listen = "0.0.0.0:8554"

[rtmp]
listen = "0.0.0.0:1935"

[http]
listen = "0.0.0.0:3000"
admin_password = "${RTFLOW_ADMIN_PASSWORD}"

[storage]
database = "./state/rtflow.db"

[paths."live/*"]
publish = true
read = true
hls = true
```

## 启动与自检

```bash
rtflow-server --config rtflow.toml --check-config
rtflow-server --config rtflow.toml --print-config   # 脱敏
rtflow-server --config rtflow.toml --db-info
rtflow-server --config rtflow.toml --db-backup /backup/rtflow.db
rtflow-server --config rtflow.toml
```

- `--check-config`/`--print-config` 不创建数据库。
- 首次启动自动建库、迁移；旧 `paths.json`/`keys.json`/`proxies.toml` 导入一次。
- `SIGHUP` 重新读取服务配置；业务 TOML 段只作为首次 seed，之后修改会打印 ignored。

## 管理入口

```text
http://<host>:3000/admin
```

控制台提供直播/点播/路径/密钥/代理/SRT/录制/系统页面；业务 CRUD 走
`/admin/api/v1/*`。API 认证支持 Basic、Bearer 和浏览器会话 + CSRF。

## 相关文档

- [storage.md](storage.md)：SQLite、主密钥、迁移、备份。
- [proxy.md](proxy.md)：RTSP 拉流代理。
- [logging.md](logging.md)、[embedded.md](embedded.md)。
