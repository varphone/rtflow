# ADR-0002：文件路径与模块路径一一对应

- 状态：已采纳（2026-11）
- 相关：布局重构 P1

## 背景

历史上 `src/lib.rs` 用 6 处 `#[path]` 把 `protocol/auth.rs` 等声明为 crate 根模块，
`media/` 没有 `mod.rs`，`pipeline` 物理位于 `media/` 下却是顶层模块。后果：

- 新人/工具（rustdoc、IDE、`grep`）得到错误的模块心智模型；
- 每新增一个协议文件都要复制 `#[path]` 技巧；
- 物理目录无法作为"依赖层次"的可靠线索。

## 决策

1. **禁止 `#[path]`**（`scripts/check-tree.py` 强制）。
2. 目录结构即模块结构：`src/rtsp/mod.rs` 声明 `message/sdp/range/auth/...`，
   `src/protocols/` 收纳全部协议适配器，`src/mount/`、`src/client/` 等同理。
3. 公共路径通过**再导出**保持兼容，而不是反向的 `#[path]`：
   `lib.rs` 中 `pub use rtsp::{auth, transport, token_auth}` 等。

## 后果

- 迁移期间保持源码兼容：`rtflow::rtsp::Message`、`rtflow::hls::…` 均不变。
- 需要迁移的旧路径以 `#[deprecated]` 门面承接一个版本（见 ADR-0004）。
- 新增模块时先落目录、再写 `mod.rs`，不需要改 `lib.rs` 的声明方式。
