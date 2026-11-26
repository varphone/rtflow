# 贡献指南

## 开发环境

- Rust 1.95+（MSRV，CI 固定 1.95 验证 `cargo check --lib`）。
- `cargo fmt`、`cargo clippy`、`cargo test` 必须全绿；布局脚本见下。

## 提交前检查

```bash
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test --no-fail-fast
cargo test --features experimental-protocols   # RTMP/SRT/WebRTC/DASH 集成测试
cargo test --features test-source            # 内置 SyntheticSource 端到端测试
cargo check --no-default-features --lib   # dev harness enables srtp/multicast (see below)
RUSTDOCFLAGS="-D warnings" cargo doc --no-deps
python3 scripts/check-layering.py --baseline scripts/layering-baseline.json
python3 scripts/check-tree.py
./scripts/check-docs-paths.sh
```

快速一键：`./scripts/release-check.sh`。

> 说明：`rtflow-testkit` 是 dev-dependency，且 `MediaDescription` 等类型含
> feature 门控字段，因此它的 `rtflow` 依赖固定启用 `srtp`/`multicast`（两者是
> 显式 opt-in，不在默认 `full` 中）。真正的 no-default 验证用 `cargo check --no-default-features --lib`
> （该目标不构建 dev-dependencies）。

## 模块布局规则

- 目录结构 = 模块结构，禁止 `#[path]`（ADR-0002）。
- 依赖只允许从上层指向下层，依赖图 SCC 必须为空（ADR-0003，
  `scripts/check-layering.py` 强制）。
- 单文件软上限 600 行、硬上限 800 行；超限文件登记在
  `scripts/tree-baseline.json`，只允许减少。
- 新增公共类型时提供唯一规范路径；旧路径只能以 `#[deprecated]` 门面保留
  一个版本（ADR-0004），并更新迁移指南。

## 新增协议适配器

1. 在 `src/protocols/<name>/` 建模块，`protocols/mod.rs` 按 feature 声明。
2. `lib.rs` 增加 `pub use protocols::<name>;` 与对应 feature。
3. 只依赖 `mount`/`media`/`http` 等低层模块，不得依赖 `server` 或 `client`。
4. 集成测试放 `tests/`，共享线束复用 `tests/support`。

## 提交信息

沿用中文 Conventional Commits：`feat(scope): …`、`fix(scope): …`、
`refactor(scope): …`、`test(scope): …`、`docs: …`、`chore: …`；
破坏性变更在标题加 `!` 并在正文说明迁移方式。
