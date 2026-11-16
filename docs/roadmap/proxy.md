# RTSP 代理服务完善路线图

本文件对应“完善 RTSP 代理服务”开发计划的状态看板。

## 已完成（M0 / M1 第一批）

- [x] 抽取公开 `rtflow::server::ProxyService`：DESCRIBE、描述净化、mount 注册、订阅者 watcher、状态快照。
- [x] `RtspServer::bind_with_proxy_service` 允许与 `rtflow-server` 共享同一服务，避免重复 watcher。
- [x] `[proxy]` / `[[proxy.mounts]]` / `[[proxy.mounts.upstreams]]` 配置、校验与密码脱敏。
- [x] `rtflow-server` 启动时并发注册静态代理；上游离线不阻塞启动。
- [x] `GET /api/v1/proxies`、`GET /admin/api/v1/proxies` 只读状态。
- [x] 现有代理集成测试与全量测试保持通过。

## M1 剩余

- [x] 描述失败自动重试/退避（`ProxyService::ensure` 后台 worker）。
- [x] SIGHUP 代理配置 diff（新增/删除/修改）。
- [x] per-mount `idle_grace`、常连模式（always）。
- [x] `restart_pending`（有订阅者时延迟切换，空闲后重建）。
- [x] 描述缓存：成功 DESCRIBE 写入 `state_dir/proxy-descriptions/*.sdp`（0600），
      重启时上游离线也能先注册 mount。

## M2 — 动态管理与可观测

- [x] admin CRUD、restart、probe。
- [x] 审计事件与代理指标（configured/streaming）。
- [x] 管理页代理只读表格（状态/上游/订阅/重试/错误）。
- [x] 运行时配置持久化（`persist_runtime`，0600）。

## M3 — 运行时与时间轴

- [x] 媒体空闲看门狗可配置（`media_idle_timeout`），去掉逐帧 timeout。
- [x] `RtspClient::connect_with_description` 快速重连（失败自动回退 DESCRIBE）。
- [x] 周期性描述刷新（无订阅者时更新 mount 描述）。
- [x] RTCP SR NTP↔RTP 映射，统一多轨 A/V 起点（首次 SR 后重定基准）。
- [x] RTP 时间戳回绕扩展（depacketizer）。
- [x] 上游重启后重连恢复的端到端测试。
- [x] 重连后以全局时间偏移延续旧时间轴，保留 SR 音视频相对偏移；
      同连接内每秒 SR 刷新继续校准漂移。
- [x] RTSPS/SRTP/multicast/跨源 REDIRECT/461 回退代理端到端测试。

## M4 — 安全与边界

- [x] 上游 host/CIDR 允许列表、私网策略、跨源重定向策略。
- [x] 代理数量、上游数与并发 DESCRIBE 上限。
- [x] DNS 解析后固定连接地址（`peer_addr` 固定，URL host 保留用于 SNI）。
- [x] 下游 Basic 鉴权、未认证拒绝与凭据泄漏测试。

## M5 — 互操作与收尾

- [x] GStreamer 拉取代理 mount 的互操作测试（`proxy_gstreamer.rs`）。
- [x] 10 代理 + 10 路 HLS 的自动化冒烟（`proxy_scale.rs`）。
- [x] 50 代理 + 200 路 HLS 短时冒烟（第二次遍历 200/200，RSS ~212MB）。
- [x] 可配置 soak 测试工具（`proxy_soak.rs`，默认 60s，可跑 8h）。
- [ ] 8h soak 实际执行。
- [x] server 配置 → 代理 mount → HLS 端到端测试（`proxy_config.rs`）。
- [x] 文档、迁移说明、CHANGELOG。

## 已知缺口

- 8h soak 工具已就绪，实际 8h 执行待部署环境完成。
- 运行时 CRUD 通过 `persist_runtime` 写入覆盖文件，不回写原配置文件。
- `restart` 延迟切换依赖订阅者计数归零，极端卡死的会话可能延长等待。
