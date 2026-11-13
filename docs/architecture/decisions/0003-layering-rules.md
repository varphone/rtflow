# ADR-0003：模块分层与单向依赖

- 状态：已采纳（2026-11）
- 相关：布局重构 P3、`scripts/check-layering.py`

## 决策

模块按层组织，依赖只允许从高编号指向低编号：

| 层 | 模块 | 说明 |
|---|---|---|
| L0 | `core::{error,identity,observe,metrics}` | 无业务依赖；metrics 仅含被动注册表 |
| L1 | `media` | 领域值类型、位流/时钟/容器抽象（`Demuxer`/`VodSource`/SDP 值类型） |
| L2 | `rtsp`、`rtp`、`rtcp`、`srtp`、`security` | 线格式与密码学 |
| L3 | `runtime`、`pipeline`、`mount`、`file` | 连接 framing、流编排、注册表、文件解复用 |
| L4 | `http`、`client`、`proxy`(门面) | 端点与客户端 |
| L5 | `metrics`（exporter）、`server`、`storage`、`protocols::*` | 导出、服务端、存储、协议适配器 |
| L6 | `gateway` | 组合根 |

规则：

1. **禁止上行依赖**（L3 → L4 等）与**同层环路**；依赖图 SCC 必须为空。
2. 同层跨组依赖允许但会被脚本提示，通常意味着应拆出共享模块（如
   `media::fmp4`、`media::demux`）。
3. `scripts/check-layering.py` 从非测试代码提取 `crate::` 依赖并强制以上规则；
   `docs/architecture/overview.md` 的图必须与脚本结果一致。

## 后果

- 组合根（`server`/`gateway`）显式装配依赖，低层不得反向读取上层配置。
- 新增模块前先在表中定位层次；跨层需求优先考虑下沉抽象。
