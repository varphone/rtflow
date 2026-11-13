# Changelog

本文件遵循 [Keep a Changelog](https://keepachangelog.com/zh-CN/1.1.0/)，
版本号遵循 [Semantic Versioning](https://semver.org/lang/zh-CN/)。

## [Unreleased] — 0.3.0（布局基线）

### Changed

- 模块布局：`src/` 目录与模块路径一一对应，移除全部 `#[path]`；`protocols/`
  收纳协议适配器，`rtsp/` 成为真实模块，`pipeline/` 提升为顶层。
- 依赖分层：模块依赖图无环（SCC=0），`scripts/check-layering.py` 强制；
  低层不再反向依赖 `server`/`metrics`/`proxy`。
- 公共路径规范化并保留弃用门面：`protocol`→`rtsp`、`proxy`→
  `mount::{upstream,continuity}`、`protocol_trait`→`gateway::protocol`、
  `media_pipeline`→`pipeline`+`rtp`、`server::{Mount*}`→`mount`。
- `MetricsRegistry`/`Demuxer`/`VodSource`/SDP 值类型下沉到更底层模块，
  旧路径经再导出保持可用。
- feature 语义修正：`dash` 依赖 `hls`、`webrtc` 依赖 `http`、`hmac`
  改为非可选依赖；19 个 feature 均可独立构建。
- 大文件拆分：`mount`、`metrics`、`client`、`rtp::payload`、`hls::segmenter`、
  `http_flv::mux`、`rtmp::session` 均拆为子模块（单文件 ≤800 行）。
- 文档重组为 `docs/{architecture,guides,audits,roadmap,archive}`，新增 ADR
  0001–0004 与 0.2→0.3 迁移指南；`tools/` 合并进 `scripts/`。

### Added

- `rtflow::prelude`。
- `scripts/{check-layering.py,check-tree.py,check-docs-paths.sh}` 布局护栏与
  CI `layout` job。
- `LICENSE-MIT`、`LICENSE-APACHE`（此前只有 SPDX 声明）、CHANGELOG、
  CONTRIBUTING、SECURITY、`deny.toml`。

### Fixed

- RTSP client keep-alive 失败无退避（审计 R23）、控制面与媒体串行
  （RT-7/RT-8）、jitter 轮询（RT-17）。
- no-default 与单 feature 构建此前无法编译（`hmac` 可选、gateway/mount
  未按 feature 门控、`dash`/`webrtc` 缺依赖）。
- fuzz crate 依赖名与包名不一致导致目标无法编译。

## [0.2.0] — 2026-08

WebRTC（WHIP/WHEP）、客户端事件与指标、传输自动回退、存储/VOD、HLS/DASH、
RTMP/SRT、示例与夹具矩阵。详见迁移指南 `docs/guides/migration/0.1-to-0.2.md`。
