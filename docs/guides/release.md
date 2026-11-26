# 发布流程（crates.io + GitHub Release）

面向 0.4.0 起的 workspace 发布。所有命令在仓库根目录执行，发布前必须全绿。

## 0. 前置条件

- rustc/cargo ≥ MSRV（当前 1.95）。
- 本机具备 `ffmpeg`（含 libaom/libdav1d）与 GStreamer 开发包，供
  `cargo package` 与互操作测试使用：`libgstreamer1.0-dev`、
  `libgstreamer-plugins-base1.0-dev`、`pkg-config`。
- `CRATES_IO_TOKEN` 已在 GitHub Secrets 中配置（仅发布 job 使用）。
- 磁盘余量 ≥ 20 GB（`cargo package --workspace` 会构建 30 个 crate）。

## 1. 冻结与版本

1. 确认 `master` 干净，CI 全绿。
2. 版本号：`Cargo.toml` 的 `[workspace.package] version` 与
   `[workspace.dependencies]` 中 29 条内部 crate 的 `version` 必须一致
   （`scripts/release-check.sh` 会校验）。
3. `CHANGELOG.md`：把 `[Unreleased]` 内容整理为新的
   `## [x.y.z] — YYYY-MM` 段落（Added/Changed/Fixed/Security），保留空的
   `[Unreleased]`。
4. `SECURITY.md` 的支持矩阵随版本更新。
5. 迁移指南 `docs/guides/migration/<old>-to-<new>.md` 与 `docs/README.md`
   索引同步。

## 2. 门禁

```bash
./scripts/release-check.sh          # fmt/clippy/tests/doc/layout/deny/package
cargo +1.95.0 check --workspace --no-default-features --lib
cargo hack check --each-feature --no-dev-deps -p rtflow --skip gstreamer
cargo hack check --feature-powerset --depth 2 --no-dev-deps -p rtflow --skip gstreamer
```

外部互操作与长跑（需要本机媒体工具，CI 的 media-interop/soak job 等价）：

```bash
(cd tests/fixtures && sha256sum -c SHA256SUMS)
cargo test -p rtflow --test interop -- --ignored --nocapture gstreamer_interop
cargo test -p rtflow --test interop -- --ignored --nocapture gstreamer_codec_matrix
cargo test -p rtflow --test interop -- --ignored --nocapture live_tool_stream
cargo test -p rtflow --test interop -- --ignored --nocapture ffmpeg_publish_interop
cargo test -p rtflow --test storage -- --ignored --nocapture file_stream
RTSP_SOAK_SECONDS=300 cargo test -p rtflow --test system -- --ignored --nocapture soak
```

## 3. 打包与发布预演（不触网）

```bash
cargo package --workspace --allow-dirty --list     # 内容审计
cargo package --workspace --allow-dirty            # 逐 crate 验证构建
cargo publish --workspace --dry-run --allow-dirty  # 拓扑顺序 + registry 校验
```

- `rtflow-server`（二进制）与 `rtflow-testkit` 为 `publish = false`，不会上传。
- `rtflow` 的 `readme = "../../README.md"` 会被打包进 crate。
- `rtflow-gst` 需要本机 GStreamer 开发包；docs.rs 无法构建它（见下）。

## 4. 打 tag 与发布

```bash
git tag -a v0.4.0 -m "rtflow 0.4.0"
git push origin v0.4.0
```

`.github/workflows/release.yml` 在 `v*` tag 上执行：
`release-check.sh` → `cargo publish --workspace --dry-run` → 正式
`cargo publish --workspace`（cargo 按依赖拓扑排序，先 core、后门面）。

首次发布注意：

- crates.io 的版本号**不可删除、不可重发**，只能 `yank`；同一版本号一旦上传
  就不能再改内容。发布前务必完成 dry-run。
- 名称已被占用时发布失败；本仓库 33 个包名在首次发布前应确认可用。
- 发布后 15–30 分钟内 docs.rs 开始构建；`rtflow-gst` 预期失败（缺系统库），
  其余 crate 应显示文档构建成功。

## 5. 发布后验证

1. docs.rs：`https://docs.rs/rtflow/0.4.0` 及子 crate 构建状态。
2. 全新目录验证：`cargo new demo && cd demo && cargo add rtflow@0.4.0`，
   跑一个最小示例。
3. 二进制：从 GitHub Release 下载 `rtflow-server`，执行 `--init-config` 与
   `--version`。
4. 监控 24 小时：crash/issue、下载量、RustSec 新告警。

## 6. 回滚

- 有安全/崩溃问题：`cargo yank --version 0.4.0 <crate>`（保留可下载性但阻止新
  解析），随后发布 0.4.1 修复。
- 不要删除 tag；在 CHANGELOG 中记录撤回原因。

## 7. 已知例外

| 项 | 状态 | 说明 |
|---|---|---|
| `rtflow-gst` docs.rs | 预期失败 | 依赖系统 GStreamer；后续可把 gstreamer 依赖改为可选 |
| advisories | 书面接受 | chrono/humantime/rustls-pemfile 为最新版且 unmaintained，见 `deny.toml` |
| semver-checks | advisory | 首个 0.4.0 发布后，CI 的 semver job 应以 0.4.0 为基线改为阻断 |
