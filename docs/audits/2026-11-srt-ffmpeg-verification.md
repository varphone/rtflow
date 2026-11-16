# SRT 与 ffmpeg 互操作验证（2026-11-16）

环境：ffmpeg/ffprobe 6.1.1（libsrt），`rtflow-server` 本地构建，loopback。
所有推流均为 `testsrc2` +（部分场景）`sine`，MPEG-TS 封装。

## 场景与结果

| 场景 | 配置 | 结果 |
| --- | --- | --- |
| 明文推流 → HLS | SRT :19100，HLS 开 | 连接建立、mount 注册；HLS 200，`ffmpeg -i` 解码 3s 成功；15s/3129 包/3.84MB |
| 推流 → 录制/VOD | 路径 `record = true` | 6 段录制 3.66MB 完成；VOD playlist 可播放，ffprobe 时长 14.96s |
| AES-128/192/256 | `passphrase=secretpass12345`，pbkeylen 16/24/32 | 三种均连接成功、HLS 200、`encrypted=true`、`undecryptable=0` |
| 错误口令 | `wrongpass12345` | 服务端 `reason=1010`（BADSECRET），ffmpeg 退出码 251 |
| 明文对端 + 强制加密 | 无 passphrase | 服务端 `reason=1011`（UNSECURE），ffmpeg 退出码 251 |
| SRT 流密钥 | `[auth] mode="token"`，签发 publish/read key | 带 key 推流成功，HLS `?key=` 200；无 key / 错 key 均 251 |
| Stream ID 白名单 | `allowed_stream_ids=["live/allowed"]` | `live/allowed` 成功且 HLS 200；`live/denied` 251 |
| SRT 回源（plain） | `[[srt.pull]]` → ffmpeg listener :19110 | mount 在线，HLS 200，解码 3s 成功，RTT 74µs |
| SRT 回源（加密） | pull passphrase + ffmpeg listener 加密 | mount 在线，HLS 200，`encrypted=true`，RTT 73µs |
| SRT 输出 | RTMP 源 → `[[srt.push]]` → ffmpeg listener :19111 | ffmpeg 写出 12.0s TS，H264+AAC，解码成功；`/api/v1/srt` 显示 sender、bytes_sent 实时增长 |
| 发送端 10% 丢包 | UDP 代理丢数据包 | ffmpeg 收到 12.01s 完整可解码文件 |
| 接收端 10% 丢包 | UDP 代理丢数据包，latency 300ms | 2507 包 / 2428 交付 / **0 skipped** / **0 未恢复** / 156 重传；HLS 解码成功 |
| 高码率 | 720p 4Mbps 20s | 8238 包全部交付，0 丢包/0 跳过，RTT ~107µs |

## 验证中发现并修复的问题

1. **TSBPD 时间基错误导致 ARQ 失效**（严重）
   - 现象：代理丢包后缺失包全部被 TLPKTDROP 跳过，`losses/skipped` 相等，重传数几乎为 0。
   - 根因：`set_time_base` 用 `now - handshake_ts`，首个数据包又用自身时间戳作外部基准，握手偏移被重复扣除；包一到就被判定过期。
   - 修复：改为 `now + (packet_ts - handshake_ts)` 语义；NAK 每 20ms 持续重发。
   - 复测：10% 丢包下 0 skipped、156 次重传恢复；HLS 正常解码。回归测试见 `crates/rtflow-srt/src/tsbpd.rs`。

2. **SRT RTT 指标恒为 0**
   - ACKACK 分支缺少 metrics 写入；补齐后 `/metrics` 显示 `rtflow_srt_rtt_microseconds ≈ 81`。

3. **push 状态 bytes_sent/RTT 只在断开时更新**
   - 改为每帧发送后更新，`/api/v1/srt` 实时可见。

4. **token 模式短密钥启动失败信息晦涩**
   - `auth.token_secrets` 少于 32 字节时启动报 `resource limit exceeded: Digest nonce secret`；
   - 配置校验改为明确错误：`auth.token_secrets entries must be at least 32 bytes (got N)`。

## 未修复的观察项（非 SRT）

- HLS 直播解码时 ffmpeg 偶发 “Application provided invalid, non monotonically increasing dts”，出现在 AAC 流；未确认是否既有 HLS 封装问题。
- 录制生成的 VOD 条目 `duration_seconds` 显示 0.6s，而其 playlist 实际约 15s、ffprobe 14.96s；疑似 VOD 目录元数据问题，与 SRT 无关。

## 复现方式

测试脚本与日志位于运行环境的 `/tmp/vrf/`（非仓库内容）；核心命令：

```bash
# 明文推流
ffmpeg -re -f lavfi -i testsrc2=size=640x360:rate=25 -f lavfi -i sine=frequency=440 \
  -t 15 -c:v libx264 -preset ultrafast -pix_fmt yuv420p -g 25 -c:a aac -shortest \
  -f mpegts 'srt://127.0.0.1:19100?streamid=live/plain&latency=120'

# 加密推流
ffmpeg ... -f mpegts 'srt://127.0.0.1:19101?streamid=live/enc&passphrase=secretpass12345&pbkeylen=32'

# 播放验证
ffmpeg -i http://127.0.0.1:18100/hls/live/plain/index.m3u8 -t 3 -f null -
```

## 自动化测试

```text
cargo test --workspace --all-features   522 passed / 0 failed
cargo clippy --workspace --all-targets --all-features -- -D warnings  clean
```
