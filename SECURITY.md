# 安全策略

## 支持版本

| 版本 | 支持 |
|---|---|
| 0.4.x | ✅ |
| 0.3.x | 仅关键安全修复 |
| 0.2.x | ❌ |
| < 0.2 | ❌ |

## 报告漏洞

请通过仓库的 **GitHub Security Advisory**（私有）报告，不要在公开 issue 中
披露细节。报告中请包含：受影响版本、最小复现、影响面（内存安全/拒绝服务/
鉴权绕过）以及是否已在真实部署中暴露。

我们会在 7 天内确认，并在修复发布后于 CHANGELOG 中记录（不披露利用细节）。

## 安全相关设计

- 解析路径全部设上限（RTSP/SDP/HTTP/RTMP/SRT/WebRTC 输入、RTP 重组上限
  16 MiB、每会话队列上限）。
- `#![forbid(unsafe_code)]`（除 `gstreamer` feature 绑定）。
- 鉴权：Basic/Digest、Bearer token、per-mount 角色 ACL。
- 拒绝服务：连接/会话/挂载/字节数配额，`ResourceLimits` 可配置。
