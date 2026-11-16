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
- [ ] `restart_pending`（有订阅者时延迟切换）。
- [ ] 已发现描述缓存（重启后上游离线仍可注册）。

## M2 — 动态管理与可观测

- [x] admin CRUD、restart、probe。
- [x] 审计事件与代理指标（configured/streaming）。
- [x] 管理页代理只读表格（状态/上游/订阅/重试/错误）。
- [ ] 运行时配置持久化（可选，0600）。

## M3 — 运行时与时间轴

- [x] 媒体空闲看门狗可配置（`media_idle_timeout`），去掉逐帧 timeout。
- [x] `RtspClient::connect_with_description` 快速重连（失败自动回退 DESCRIBE）。
- [x] 周期性描述刷新（无订阅者时更新 mount 描述）。
- [x] RTCP SR NTP↔RTP 映射，统一多轨 A/V 起点（首次 SR 后重定基准）。
- [x] RTP 时间戳回绕扩展（depacketizer）。
- [ ] 重连后 A/V 相对偏移的长期漂移校准。
- [x] RTSPS/SRTP/跨源 REDIRECT/461 回退代理端到端测试（multicast 待补）。

## M4 — 安全与边界

- [x] 上游 host/CIDR 允许列表、私网策略、跨源重定向策略。
- [x] 代理数量、上游数与并发 DESCRIBE 上限。
- [x] DNS 解析后固定连接地址（`peer_addr` 固定，URL host 保留用于 SNI）。
- [ ] 下游路径鉴权与凭据泄漏测试。

## M5 — 互操作与收尾

- [x] GStreamer 拉取代理 mount 的互操作测试（`proxy_gstreamer.rs`）。
- [x] 10 代理 + 10 路 HLS 的自动化冒烟（`proxy_scale.rs`）。
- [x] 50 代理 + 200 路 HLS 短时冒烟（第二次遍历 200/200，RSS ~212MB）。
- [ ] 8h soak。
- [x] server 配置 → 代理 mount → HLS 端到端测试（`proxy_config.rs`）。
- [ ] 文档、迁移说明、CHANGELOG。

## 已知缺口

- 运行时 CRUD 只改内存，不写回配置文件。
- 代理媒体统计的 `bytes` 目前来自 mount `rtp_octets`，代理发布路径未累计字节（显示 0），M2 补 `published_bytes`。
- 代理 HLS 播放曾观察到重复 DTS 警告，列入 M3 时间轴专项。
