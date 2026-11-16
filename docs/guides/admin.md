# 管理控制台

控制台路径为 `/admin`（默认 HTTP `3000`），使用 `[http] admin_username` 与
`admin_password`/`RTFLOW_ADMIN_PASSWORD` 登录。

## 页面

- **运行**：概览（KPI 与异常聚合）、直播流、点播。
- **配置**：路径、推流密钥、RTSP 代理、SRT 拉/推、录制任务。
- **系统**：脱敏 TOML、数据库 schema/大小、构建特性。

业务配置写入 SQLite；服务配置只读，修改 `rtflow.toml` 后 SIGHUP 或重启。

## 操作

- 列表支持搜索/状态筛选；行操作按实体显示编辑、断开、重启、探测、删除。
- 新建/编辑使用右侧抽屉；密钥 secret 只显示一次。
- 所有写操作经过会话 Cookie + CSRF；也支持 Basic/Bearer API。
- `#/config/paths` 等 hash 可直接刷新/分享。

## 开发

前端资源位于 `crates/rtflow-admin/src/assets/`，由 Rust `include_bytes!` 嵌入：

```text
app.js        启动、会话、路由、shell
js/api.js     fetch/CSRF/错误
js/pages/*    页面
js/forms/*    抽屉表单
```

无 npm/Vite 构建；修改后重新编译 `rtflow-server`。新增静态文件需加入
`crates/rtflow-admin/src/api/assets.rs` 的 `ASSETS` 清单。
