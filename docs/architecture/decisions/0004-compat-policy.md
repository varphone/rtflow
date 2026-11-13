# ADR-0004：兼容与弃用策略

- 状态：已采纳（2026-11）
- 相关：布局重构 P4、`docs/guides/migration/0.2-to-0.3.md`

## 决策

1. **0.3 只弃用、不删除**：受影响的公共路径保留为 `#[deprecated]` 门面，
   0.4 再移除。门面实现方式：
   - 模块门面：`#[deprecated] pub mod protocol;`（内部只做 `pub use`）；
   - 别名门面：`#[deprecated] pub use gateway::protocol as protocol_trait;`；
   - 再导出：`pub use crate::mount::{…}`，类型身份不变。
2. **规范路径唯一**：每个类型有一个文档中的规范路径；旧路径只作为过渡。
3. **迁移文档必须同步**：新增弃用项时更新 `0.2-to-0.3` 迁移表，并在
   `cargo semver-checks` / `cargo package` 前检查。
4. **破坏性变更集中在大版本**：0.3 的破坏性变更包括 `ClientOptions`
   的 `#[non_exhaustive]`、feature 语义调整（`dash`→`hls`、`webrtc`→`http`）、
   GStreamer 注册方法签名；其余均为源码兼容的再导出/新增。

## 后果

- 升级到 0.3 的应用通常只需消除弃用警告，不需要改代码；需要改的地方在迁移表
  中逐项列出。
- 0.4 移除门面前，仓库内不得再使用旧路径（`tests/`、`examples/` 已迁移）。
