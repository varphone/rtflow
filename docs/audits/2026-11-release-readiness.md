# 0.4.0 发布前全面审查

- 审查日期：2026-11-26
- 审查对象：`master`（依赖升级完成后），目标版本 **0.4.0**，首次 crates.io 发布
- 结论：**GO（有条件）**。构建门禁、测试、依赖审计、打包与发布演练全部通过；
  遗留项均为已书面的风险接受或发布后跟进（见 §7、§8）。打 tag 前需确认
  `cargo publish --workspace --dry-run` 结果与本文一致。

## 1. 环境与基线

| 项 | 值 |
|---|---|
| rustc / cargo | 1.100.0（MSRV 校验另用 1.95.0） |
| FFmpeg | 6.1.1（系统）+ N-128837-g986bab2b47-20261112（`~/.local/opt/ffmpeg`，含 libdav1d/libaom） |
| GStreamer | 1.24.2（含 `rtpjpegdepay`/`rtph263depay`/`rtpmp4vdepay`） |
| OS | Linux 7.0.0-43-generic x86_64 |
| workspace | 32 个 crate（30 个可发布；`rtflow-server`、`rtflow-testkit` 为 `publish = false`） |
| 代码规模 | 396 个 `.rs`；源码 76,692 行，测试 18,136 行 |
| 版本 | 全部可发布 crate 与内部依赖 0.4.0；`rtflow-testkit` 0.0.0 |

## 2. 门禁结果

| 门禁 | 命令 | 结果 |
|---|---|---|
| 发布总门禁 | `./scripts/release-check.sh` | ✅ 13m47s，170 个测试二进制/doctest 目标共 1160 项通过，0 失败；32 个 crate 打包并逐包验证 |
| 格式化 | `cargo fmt --all -- --check` | ✅ |
| Clippy | `cargo clippy --workspace --all-targets [-—all-features] -- -D warnings` | ✅ 两种配置 |
| 测试（默认） | `cargo test --workspace --all-targets --no-fail-fast` | ✅ 576 项 |
| 测试（全特性） | `cargo test --workspace --all-features --no-fail-fast` | ✅ |
| 无默认特性 | `cargo check/test -p rtflow --no-default-features --lib` | ✅（本轮修复） |
| Feature profile | `embedded-gateway`、`experimental-protocols`、`full`、`server,tls,srtp` | ✅ 4/4 |
| MSRV | `cargo +1.95.0 check --workspace --no-default-features --lib` | ✅ |
| 依赖审计 | `cargo deny check advisories licenses bans sources` | ✅（3 条已登记 ignore） |
| RustSec | `cargo audit` | ⚠️ 3 条 unmaintained 告警（见 §7） |
| 文档 | `RUSTDOCFLAGS=-D warnings cargo doc --workspace --no-deps --all-features` | ✅ |
| 布局 | `check-layering.py` / `check-tree.py` / `check-docs-paths.sh` | ✅ |
| 打包 | `cargo package --workspace --allow-dirty` | ✅ 32/32 |
| 发布演练 | `cargo publish --workspace --dry-run --allow-dirty` | ✅ 30 个可发布 crate 依次打包/校验，全部 "aborting upload due to dry run" |
| CI（GitHub Actions） | 首轮 run 45831090229 暴露 R-11..R-16；修复后 run 45836037716 全绿 | ✅ msrv/layout/dependency-audit/media-interop/feature-matrix/package/quality 全部通过；fuzz-smoke、soak 仅定时触发，semver 待发布后启用 |
| 外部互操作 | GStreamer interop / codec matrix / live tool / FFmpeg publish / 文件矩阵 | ✅ 1+1+1+2+2 全部通过 |
| 夹具完整性 | `sha256sum -c SHA256SUMS` | ✅ |
| 长跑 | `RTSP_SOAK_SECONDS=300 cargo test -p rtflow --test system -- --ignored soak` | ✅ 300s 通过 |
| 模糊测试 | `cargo fuzz run {control_framing,rtp_rtcp,srtp} -- -max_total_time=60` | ✅ 1,264,229 / 4,185,985 / 2,162,324 次执行，无崩溃 |

## 3. 发布前发现与修复

| ID | 级别 | 现象 | 根因 | 处置 |
|---|---|---|---|---|
| R-01 | P0 | `scripts/check-tree.py` 失败（6 个未登记的超 800 行文件 + hls.rs 基线条目过期） | 基线未随重构更新 | 重新生成 `scripts/tree-baseline.json`（7 条），登记为发布后拆分债 |
| R-02 | P0 | `cargo check -p rtflow --no-default-features --lib` 失败 | 门面 crate 无条件 `pub use` 了可选依赖 `rtflow_http`/`rtflow_rtsp_server`/`rtflow_hls` | 为 `http`/`server`/`hls` 三处 re-export 与 prelude 补 `#[cfg(feature = ...)]`；4 个 profile 复验 |
| R-03 | P0 | `cargo deny check advisories` 失败（chrono / humantime / rustls-pemfile unmaintained） | 三者均为最新版且无维护者；无升级路径 | 在 `deny.toml` 记录带理由的 ignore；`cargo audit` 结果同步登记；迁移列入发布后计划 |
| R-04 | P1 | CI `package` job 缺 GStreamer 开发包，`rtflow-gst` 打包验证预期失败 | job 未安装系统依赖 | job 增加 `pkg-config` + `libgstreamer1.0-dev` + `libgstreamer-plugins-base1.0-dev` |
| R-05 | P1 | 内置源 AV1 无法被 FFmpeg 8 解码（dav1d `Invalid data`） | `SyntheticSource` 在强制关键帧后跳到资产第 0 帧，下一帧又按 `units % keyframe_interval` 取帧，**跳过中间帧**，AV1 参考链断裂 | 改为强制关键帧/间隔到期后从资产头部**按序**推进（不跳帧）；`builtin_source_decode` 通过，TCP/UDP 实时播放无解码错误 |
| R-06 | P1 | `gstreamer_new_codecs` 偶发失败（JPEG 段 RTSP 拆除竞态） | ffmpeg 发布端固定 `-t 15`，慢解码器尚未取满缓冲时发布端已结束，服务端拆会话 | 发布端取消时限（测试结束即 kill）；连跑 3 次稳定通过 |
| R-07 | P1 | 版本/CHANGELOG/SECURITY 不一致（workspace 0.3.0、CHANGELOG 已有 0.3.0 段、282 条 Unreleased、SECURITY 只列 0.3.x） | 发布流程缺失 | 全量升到 0.4.0；Unreleased 整理为 `## [0.4.0]`（279→280 条，去除 2 条完全重复，无内容丢失）；SECURITY 支持矩阵更新 |
| R-08 | P2 | README 漂移：`webrtc` 写成 0.20；AV1 描述与实际 RTP 实现不符 | 文档未随实现更新 | 改为 0.21；AV1 改为"聚合/分片/重组 + 内置源可解码" |
| R-09 | P2 | `rtflow-testkit` 缺 repository，`cargo package` 告警 | manifest 缺字段 | 补 `repository.workspace` |
| R-10 | P2 | `release-check.sh` 不覆盖版本一致性、依赖审计与 no-default 构建 | 脚本早于这些门禁 | 增加版本/CHANGELOG 一致性、`cargo deny`、no-default check/test |
| R-11 | P0 | CI `quality`（Clippy）与 `msrv` 失败：`gobject-sys`/`glib-sys` 构建脚本报错 | 工作区成员 `rtflow-gst` 无条件依赖 GStreamer，两个 job 未安装系统开发包 | 两个 job 增加 `pkg-config` + GStreamer/GLib 开发包安装 |
| R-12 | P0 | CI `package` 失败：`--registry is required because not all package.publish settings agree` | 成员 `publish` 取值混用 `true`/`["crates-io"]`/`false`，runner 无默认 registry | `package`/`publish` 命令显式 `--registry crates-io`（CI、`release-check.sh`、release workflow） |
| R-13 | P1 | CI `dependency-audit` 失败：rustsec/audit-check 无法创建 check run（`Resource not accessible by integration`） | 工作流只授予 `contents: read` | 工作流权限补 `checks: write` |
| R-14 | P1 | 默认测试仍依赖 FFmpeg/GStreamer（`ffmpeg_new_codecs`、`gstreamer_new_codecs` 未 ignored） | 违反 README "默认测试不依赖外部工具"的约定，CI 无媒体工具时失败 | 两处标记 `#[ignore]`，并在 `media-interop` job 显式运行（本机 3 项复验通过） |
| R-15 | P2 | `semver` job 在首个 crates.io 版本前必然失败 | 无已发布基线 | 用仓库变量 `CI_SEMVER_BASELINE` 门控；发布 0.4.0 后开启并改为阻断 |
| R-16 | P1 | CI `quality` 的 Tests 失败：两个 RTSPS 用例 `ConnectionRefused` | GitHub runner 上 `localhost` 优先解析到 `::1`，服务端只监听 `127.0.0.1` | 用例改连绑定地址，并用 `TlsClientConfig::with_server_name("localhost")` 保持证书校验 |

## 4. 安全与合规要点（人工审查）

- 内存安全：除 `gstreamer` feature 外 `#![forbid(unsafe_code)]`；全仓 3 处 `unsafe`
  均为 `rtflow-gst` 的 `ManuallyDrop::take`（任务句柄移交），无裸指针运算。
- 输入边界：RTSP/SDP/HTTP/RTMP/SRT/WebRTC 解析均有上限；HTTP 头 64 个、头/体字节上限、
  `Content-Length` 校验；RTP 重组与 PES 缓冲 16 MiB 硬上限。
- 鉴权：Basic/Digest（签名 nonce 含过期与 nonce-count 防重放，单测覆盖）、Bearer token、
  per-mount 角色 ACL；`auth.token_secrets` 少于 32 字节时拒绝启动。
- 传输：rustls + ring 单一后端；provider 在 server/security/onvif 创建客户端前显式安装；
  证书校验使用 rustls 默认验证器（可用 `TlsClientConfig` 指定根证书）。
- 静态数据：状态目录 0700、数据库/密钥文件 0600；可恢复密钥用 AES-256-GCM 且以
  mount/用途字符串作 AAD 绑定；VOD id 做 URL-safe 校验，防止路径穿越。
- 依赖来源：`deny.toml` 禁止未知 registry/git；许可证白名单包含本轮新增的
  CC0-1.0（iso-639）与 CDLA-Permissive-2.0（webpki-root-certs）。

## 5. 性能与资源

- 长跑 300s 会话 churn/fan-out 无失败、无资源泄漏断言。
- 模糊测试三目标共约 760 万次执行，无 panic/崩溃。
- 未在本轮建立 criterion 基线（`benches/hot_path.rs`、`benches/throughput.rs` 可用），
  建议发布后纳入定期任务。
- release 构建（LTO/opt-level=s/panic=abort/strip）：`rtflow-server` **12,016,248 字节（≈11.5 MiB）**，
  `size` text 11,709,968 / data 299,969 / bss 2,712；构建 1m17s（增量缓存）。
- 目标板（aarch64/armv7）体积与 RSS 未在本轮测量，需要交叉工具链与目标硬件。

## 6. 版本与文档产出

- `Cargo.toml`：workspace 与 29 条内部依赖版本统一到 0.4.0。
- `CHANGELOG.md`：`[0.4.0] — 2026-11`，Added 40 / Changed 36 / Fixed 204。
- `SECURITY.md`：0.4.x 支持、0.3.x 仅关键修复。
- `docs/guides/migration/0.3-to-0.4.md`：MSRV、依赖、TLS provider、feature 门控、测试源。
- `docs/guides/release.md`：门禁、打包、拓扑发布、docs.rs、回滚。
- `docs/README.md`、`docs/audits/README.md`：索引更新。

## 7. 风险接受（书面）

| 项 | 依据 | 处置 |
|---|---|---|
| chrono / humantime / rustls-pemfile unmaintained | 均已是 crates.io 最新版；chrono 迁移 jiff、rustls-pemfile 迁移 rustls-pki-types 属跨版本改造 | `deny.toml` ignore + 本报告记录；发布后立项迁移 |
| docs.rs 无法构建 `rtflow-gst` | 依赖系统 GStreamer，docs.rs 不提供 | 文档注明；后续把 gstreamer 依赖改为可选 |
| 6 个源文件 >800 行 | 历史结构债，拆分风险高于收益 | 基线登记；发布后按模块拆分 |
| semver job 为 advisory | 0.4.0 是首个 crates.io 版本，尚无基线 | 发布后以 0.4.0 为基线改为阻断 |
| Actions 按 tag 而非 SHA 固定 | 现有 CI 约定 | 记为 P2 加固项 |

## 8. 未覆盖与后续项

1. 目标硬件（HiSilicon/Ingenic）上的交叉编译与运行验证；本机仅验证 x86_64。
2. `cargo hack` 的 feature-powerset 深度 2 与 CI 的 semver-checks 未在本机全量复跑
   （逐 feature 的关键 profile 已覆盖，CI 会执行）。
3. docs.rs 实际构建结果需发布后观察。
4. 建议发布后补齐：criterion 基线、chrono/rustls-pemfile 迁移、超长文件拆分、
   Actions SHA pin。

## 9. 复现命令

```bash
./scripts/release-check.sh
cargo +1.95.0 check --workspace --no-default-features --lib
cargo publish --workspace --dry-run --allow-dirty
(cd tests/fixtures && sha256sum -c SHA256SUMS)
cargo test -p rtflow --test interop -- --ignored --nocapture gstreamer_interop
cargo test -p rtflow --test interop -- --ignored --nocapture gstreamer_codec_matrix
cargo test -p rtflow --test interop -- --ignored --nocapture live_tool_stream
cargo test -p rtflow --test interop -- --ignored --nocapture ffmpeg_publish_interop
cargo test -p rtflow --test storage -- --ignored --nocapture file_stream
RTSP_SOAK_SECONDS=300 cargo test -p rtflow --test system -- --ignored soak
(cd fuzz && cargo fuzz run rtp_rtcp -- -max_total_time=60)
cargo deny check advisories licenses bans sources
```

## 10. 发布后 24 小时验证清单

1. docs.rs 各 crate 构建状态（`rtflow-gst` 例外）。
2. 全新目录 `cargo add rtflow@0.4.0` 并跑最小示例。
3. GitHub Release 二进制 `rtflow-server --version` / `--init-config`。
4. 下载量、issue、RustSec 新告警；必要时 `cargo yank` + 0.4.1。
