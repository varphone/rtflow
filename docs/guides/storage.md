# 业务数据与 SQLite

`rtflow-server` 把**服务自身配置**和**运营期业务数据**分开：

- 服务配置：`rtflow.toml`（监听、TLS、日志、鉴权、限额、HLS/DASH、全局录制策略）。
- 业务数据：`state/rtflow.db`（SQLite，WAL），由管理 API/控制台和运行时共同使用。

## 配置

```toml
[storage]
database = "./state/rtflow.db"   # 空则使用 state_dir/rtflow.db
busy_timeout = "5s"
master_key_env = "RTFLOW_MASTER_KEY"
master_key_file = ""             # 可空；默认 state_dir/master.key
require_master_key = false
```

- 首次启动自动建库、执行迁移；DB/主密钥文件权限 0600，目录 0700。
- 主密钥用于加密可恢复的秘密（代理上游密码、SRT passphrase）；推流密钥只存 Argon2id 哈希。
- 未配置密钥来源时自动生成 `state_dir/master.key`；生产建议用 `RTFLOW_MASTER_KEY` 或备份该文件。`require_master_key=true` 时缺失直接拒绝启动。
- `--check-config`/`--print-config` 不创建数据库。

## 数据

| 表 | 内容 |
| --- | --- |
| `paths` | API/动态路径记录；TOML `[paths]` 仍作为 glob 策略层 |
| `stream_keys` | 推流密钥（哈希、角色、路径/协议范围、过期） |
| `proxy_mounts` | RTSP 代理配置（上游 JSON 加密） |
| `srt_endpoints` | SRT pull/push 端点（passphrase 加密） |
| `idempotency_keys` | POST 幂等重放 |
| `meta` | schema/seed 状态 |

- `revision` 用于乐观并发；PATCH/DELETE 支持 `If-Match`（同时兼容旧 `updated_at`）。
- 列表支持 `limit`/`cursor`，默认 200、最大 1000。
- 所有 JSON 列写入前经 serde 类型校验。

## 迁移

首次打开空库时一次性导入：

1. `state/paths.json`
2. `state/keys.json`
3. `state/proxies.toml` 覆盖文件
4. TOML `[[proxy.mounts]]`、`[[srt.pull]]`、`[[srt.push]]`

导入后旧文件改名为 `*.migrated-<timestamp>` 保留备份；迁移前自动生成 DB `.bak`。之后 TOML 业务段只读/忽略并打印 warning，修改请走管理 API/控制台。`[proxy].persist_runtime` 兼容保留但不再写入。

## API/控制台

```text
GET/POST        /admin/api/v1/paths
GET/PATCH/DELETE /admin/api/v1/paths/{name}
GET/POST         /admin/api/v1/keys
DELETE           /admin/api/v1/keys/{kid}
GET/POST         /admin/api/v1/proxies
GET/PATCH/DELETE /admin/api/v1/proxies?path=…
POST             /admin/api/v1/proxies/restart|probe?path=…
GET/POST         /admin/api/v1/srt/pulls|pushes
PATCH/DELETE     /admin/api/v1/srt/pulls|pushes/{id}
GET              /admin/api/v1/storage
```

控制台“路径与密钥”页提供路径、密钥、代理、SRT 拉/推的查看与新增/删除操作；密钥 secret 只显示一次。

## SRT 变更

SRT pull/push 每条记录独立任务；新增、修改、删除、启停只重启该端点，不影响其他
SRT 端点。配置保存到 SQLite 后立即生效。

## 备份与恢复

```bash
sqlite3 state/rtflow.db ".backup '/backup/rtflow.db'"
# 或通过 API 的存储信息确认 schema；停机恢复时替换 rtflow.db + 删除 -wal/-shm
```

同时备份 `master.key`（或环境变量中的密钥）；丢失主密钥会导致代理/SRT 秘密无法解密。
