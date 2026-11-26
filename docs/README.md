# rtflow 文档索引

| 目录 | 内容 |
|---|---|
| [architecture/](architecture/overview.md) | 分层图、模块职责、数据面与所有权约定 |
| [architecture/rtsp-matrix.md](architecture/rtsp-matrix.md) | RTSP 方法/传输/码流/feature/内存矩阵 |
| [architecture/decisions/](architecture/decisions/README.md) | ADR：单 crate、文件=模块、分层规则、兼容策略 |
| [guides/embedded.md](guides/embedded.md) | 嵌入式交叉编译、调参与部署 |
| [guides/examples.md](guides/examples.md) | 示例导航（本地源、文件、客户端、代理、发布、鉴权、Pipeline） |
| [guides/file-streaming.md](guides/file-streaming.md) | 容器/格式矩阵、VOD 与循环播放边界 |
| [guides/migration/0.1-to-0.2.md](guides/migration/0.1-to-0.2.md) | 0.1 → 0.2 迁移 |
| [guides/migration/0.2-to-0.3.md](guides/migration/0.2-to-0.3.md) | 0.2 → 0.3 迁移（布局重构、弃用路径、feature 语义） |
| [guides/migration/0.3-to-0.4.md](guides/migration/0.3-to-0.4.md) | 0.3 → 0.4 迁移（MSRV 1.95、依赖升级、TLS provider、feature 门控） |
| [guides/release.md](guides/release.md) | 发布流程：门禁、打包、crates.io 拓扑发布、回滚 |
| [audits/](audits/README.md) | 代码/功能/可用性/嵌入式资源/实时兼容审计与状态 |
| [roadmap/client.md](roadmap/client.md) | client 模块 M0–M5 路线图 |
| [archive/](archive/) | 已完成阶段的历史计划（phase-2/3） |

## 阅读顺序

1. 新用户：根 [README](../README.md) → 本文档索引 → `guides/examples.md`。
2. 扩展协议/模块：`architecture/overview.md` → `architecture/decisions/0003-layering-rules.md`。
3. 升级：对应版本的 `guides/migration/*`。
4. 贡献：根 [CONTRIBUTING.md](../CONTRIBUTING.md) 的验证命令与 feature 矩阵。

## 维护约定

- 新增文档放在上表对应目录；跨文档引用使用相对链接。
- 审计条目修复后更新 `audits/README.md` 的状态表。
- 阶段计划完成后移入 `archive/`，不要删除历史。
